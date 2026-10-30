//! HTTP front end: health check, Basic auth, the write gate while a person has git
//! state in `.git`, early size rejection, the WebDAV handler, and waiting for the
//! commit before answering when the config says so.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::{Context, Result};
use dav_server::body::Body;
use dav_server::memls::MemLs;
use dav_server::DavHandler;
use http::{header, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tracing::{debug, error, warn};

use crate::auth;
use crate::brand;
use crate::committer::Committer;
use crate::config::{AuthConfig, CommitMode, Config};
use crate::davfs::GitFs;
use crate::disk::DiskGuard;
use crate::repo::{ForeignState, Repo};
use crate::status::Status;

const RETRY_AFTER_SECS: &str = "30";
/// Largest PROPFIND answer we will hold in memory to quote its ETags.
const MAX_PROPFIND_RESPONSE: usize = 128 * 1024 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

struct Ctx {
    handler: DavHandler,
    auth: AuthConfig,
    committer: Committer,
    repo: Repo,
    status: Arc<Status>,
    max_file_size: Option<u64>,
    body_idle_timeout: Duration,
    disk: Arc<DiskGuard>,
    /// Answer only after the commit finished (per_request mode with `wait_for_commit`).
    wait_for_commit: bool,
}

pub struct Server {
    listener: TcpListener,
    ctx: Arc<Ctx>,
    max_connections: usize,
    header_read_timeout: Duration,
}

impl Server {
    /// Bind `cfg.server.listen` and build the WebDAV handler over the repo's working tree.
    pub async fn bind(
        cfg: &Config,
        repo: Repo,
        committer: Committer,
        status: Arc<Status>,
    ) -> Result<Server> {
        let listener = TcpListener::bind(cfg.server.listen)
            .await
            .with_context(|| format!("bind {}", cfg.server.listen))?;
        let fs = GitFs::new(
            repo.workdir().to_path_buf(),
            cfg.limits.max_file_size,
            Arc::new(committer.clone()),
        );
        let handler = DavHandler::builder()
            .filesystem(Box::new(fs))
            .locksystem(MemLs::new())
            .build_handler();
        let disk = Arc::new(DiskGuard::new(
            repo.workdir().to_path_buf(),
            cfg.limits.min_free_space,
            cfg.limits.max_repo_size,
        ));
        let ctx = Arc::new(Ctx {
            handler,
            auth: cfg.auth.clone(),
            committer,
            repo,
            status,
            max_file_size: cfg.limits.max_file_size,
            body_idle_timeout: cfg.limits.body_idle_timeout,
            disk,
            wait_for_commit: cfg.commit.mode == CommitMode::PerRequest
                && cfg.commit.wait_for_commit,
        });
        Ok(Server {
            listener,
            ctx,
            max_connections: cfg.limits.max_connections,
            header_read_timeout: cfg.limits.header_read_timeout,
        })
    }

    /// The disk-space guard, so the caller can keep it fresh and watch it.
    pub fn disk(&self) -> Arc<DiskGuard> {
        self.ctx.disk.clone()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().expect("listener has an address")
    }

    /// Serve until `shutdown` resolves, then stop accepting and let in-flight requests finish.
    pub async fn serve(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let graceful = GracefulShutdown::new();
        let slots =
            (self.max_connections > 0).then(|| Arc::new(Semaphore::new(self.max_connections)));
        tokio::pin!(shutdown);
        loop {
            // Take a slot before accepting, so a flood waits in the kernel's queue instead.
            let permit = match &slots {
                Some(slots) => tokio::select! {
                    permit = slots.clone().acquire_owned() => permit.ok(),
                    _ = &mut shutdown => break,
                },
                None => None,
            };
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(pair) => pair,
                        Err(e) => {
                            warn!(error = %e, "accept failed");
                            continue;
                        }
                    };
                    let ctx = self.ctx.clone();
                    let service = service_fn(move |req| {
                        let ctx = ctx.clone();
                        async move { Ok::<_, std::convert::Infallible>(handle(ctx, peer, req).await) }
                    });
                    let mut http = hyper::server::conn::http1::Builder::new();
                    http.timer(TokioTimer::new());
                    http.header_read_timeout(
                        (!self.header_read_timeout.is_zero()).then_some(self.header_read_timeout),
                    );
                    let conn = http.serve_connection(TokioIo::new(stream), service);
                    let conn = graceful.watch(conn);
                    tokio::spawn(async move {
                        let _slot = permit;
                        if let Err(e) = conn.await {
                            debug!(%peer, error = %e, "connection ended with error");
                        }
                    });
                }
                _ = &mut shutdown => break,
            }
        }
        drop(self.listener);
        tokio::select! {
            _ = graceful.shutdown() => {}
            _ = tokio::time::sleep(SHUTDOWN_GRACE) => warn!("gave up waiting for in-flight requests"),
        }
        Ok(())
    }
}

/// A request body that fails if the client stops sending for `timeout` (zero disables).
/// The clock restarts every time data arrives, so a slow but steady upload is fine.
struct IdleTimeout<B> {
    inner: B,
    timeout: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
    started: bool,
}

impl<B> IdleTimeout<B> {
    fn new(inner: B, timeout: Duration) -> Self {
        IdleTimeout {
            inner,
            timeout,
            sleep: Box::pin(tokio::time::sleep(timeout)),
            started: false,
        }
    }

    fn restart_clock(&mut self) {
        if !self.timeout.is_zero() {
            self.sleep
                .as_mut()
                .reset(tokio::time::Instant::now() + self.timeout);
        }
    }
}

#[derive(Debug)]
enum BodyError {
    Read(hyper::Error),
    Stalled,
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::Read(e) => write!(f, "{e}"),
            BodyError::Stalled => f.write_str("the client stopped sending the request body"),
        }
    }
}

impl std::error::Error for BodyError {}

impl http_body::Body for IdleTimeout<Incoming> {
    type Data = bytes::Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        if !this.started {
            // time spent before the handler first reads the body is not the client's fault
            this.started = true;
            this.restart_clock();
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(frame) => {
                this.restart_clock();
                Poll::Ready(frame.map(|f| f.map_err(BodyError::Read)))
            }
            Poll::Pending => {
                if !this.timeout.is_zero() && this.sleep.as_mut().poll(cx).is_ready() {
                    Poll::Ready(Some(Err(BodyError::Stalled)))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn text_response(status: StatusCode, text: &'static str) -> Response<Body> {
    let mut res = Response::new(Body::from(text));
    *res.status_mut() = status;
    res
}

fn is_mutating(method: &Method) -> bool {
    matches!(
        method.as_str(),
        "PUT" | "DELETE" | "MKCOL" | "MOVE" | "COPY" | "PROPPATCH" | "LOCK" | "PATCH"
    )
}

/// Request path, percent-decoded, is valid UTF-8.
fn path_is_utf8(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
                decoded.push(v);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    std::str::from_utf8(&decoded).is_ok()
}

/// `dav-server` writes `<D:getetag>` without the quotes RFC 4918 requires (its `ETag`
/// header has them). Strict clients treat an unquoted value as weak and refuse to
/// overwrite or delete on it, so quote every plain value.
fn quote_getetag_values(xml: &str) -> String {
    const OPEN: &str = "<D:getetag>";
    const CLOSE: &str = "</D:getetag>";
    let mut out = String::with_capacity(xml.len() + 16);
    let mut rest = xml;
    while let Some(start) = rest.find(OPEN) {
        let value_start = start + OPEN.len();
        let Some(len) = rest[value_start..].find(CLOSE) else {
            break;
        };
        let value = &rest[value_start..value_start + len];
        out.push_str(&rest[..value_start]);
        if value.is_empty() || value.starts_with('"') || value.starts_with("W/") {
            out.push_str(value);
        } else {
            out.push('"');
            out.push_str(value);
            out.push('"');
        }
        rest = &rest[value_start + len..];
    }
    out.push_str(rest);
    out
}

async fn quote_getetags(res: Response<Body>, limit: usize) -> Response<Body> {
    let (mut parts, body) = res.into_parts();
    let bytes = match http_body_util::Limited::new(body, limit).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(error = %e, "could not read the PROPFIND response");
            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the listing is too large; list subfolders one at a time\n",
            );
        }
    };
    let fixed = match String::from_utf8(bytes.to_vec()) {
        Ok(text) => quote_getetag_values(&text).into_bytes(),
        Err(_) => bytes.to_vec(),
    };
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(fixed.len()));
    Response::from_parts(parts, Body::from(bytes::Bytes::from(fixed)))
}

/// Content types a browser can run script from. Served from our origin, a stored page
/// could otherwise read other files with the credentials the browser has cached.
fn is_active_content(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "text/html" | "application/xhtml+xml" | "image/svg+xml" | "text/xml" | "application/xml"
    )
}

async fn handle(ctx: Arc<Ctx>, peer: SocketAddr, req: Request<Incoming>) -> Response<Body> {
    let mut res = route(&ctx, peer, req).await;
    let headers = res.headers_mut();
    headers.insert(header::SERVER, HeaderValue::from_static(brand::USER_AGENT));
    // Never let a browser guess a more dangerous type than the one we declared.
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let active = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(is_active_content);
    if active {
        // Pages stay viewable but get a unique origin and no scripts. Not applied to PDFs
        // or images, where `sandbox` would switch off the browser's viewers.
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox"),
        );
    }
    res
}

async fn route(ctx: &Ctx, peer: SocketAddr, req: Request<Incoming>) -> Response<Body> {
    if req.uri().path() == "/healthz" && matches!(*req.method(), Method::GET | Method::HEAD) {
        let json = serde_json::to_string(&ctx.status.snapshot()).unwrap_or_else(|_| "{}".into());
        let mut res = Response::new(Body::from(json));
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return res;
    }

    let authorization = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !auth::check(&ctx.auth, authorization) {
        if authorization.is_some() {
            // Never log what was typed: a password may have landed in the username field.
            warn!(%peer, "failed login");
            if !ctx.auth.failed_login_delay.is_zero() {
                tokio::time::sleep(ctx.auth.failed_login_delay).await;
            }
        }
        let mut res = text_response(StatusCode::UNAUTHORIZED, "authentication required\n");
        res.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_str(&format!("Basic realm=\"{}\"", brand::NAME))
                .expect("realm is a valid header value"),
        );
        return res;
    }

    if !path_is_utf8(req.uri().path()) {
        return text_response(StatusCode::BAD_REQUEST, "path is not valid UTF-8\n");
    }

    let mutating = is_mutating(req.method());
    if mutating {
        let foreign = ctx.repo.foreign_state();
        ctx.status.set_foreign_state(foreign);
        if foreign != ForeignState::None {
            let mut res = text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "the repository is being worked on; try again shortly\n",
            );
            res.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_static(RETRY_AFTER_SECS),
            );
            return res;
        }
    }

    if matches!(req.method().as_str(), "PUT" | "COPY" | "MKCOL" | "PATCH") {
        if let Err(problem) = ctx.disk.check() {
            warn!(%problem, "refusing a write: the disk is too full");
            return text_response(
                StatusCode::INSUFFICIENT_STORAGE,
                "the server is out of space for new data; deletes still work\n",
            );
        }
    }

    if let Some(max) = ctx.max_file_size {
        let declared = req
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if matches!(req.method().as_str(), "PUT" | "PATCH") && declared.is_some_and(|n| n > max) {
            return text_response(StatusCode::PAYLOAD_TOO_LARGE, "file is too large\n");
        }
    }

    let propfind = req.method().as_str() == "PROPFIND";
    let (parts, body) = req.into_parts();
    let req = Request::from_parts(parts, IdleTimeout::new(body, ctx.body_idle_timeout));
    let mut res = ctx.handler.handle(req).await;
    if propfind && res.status().as_u16() == 207 {
        res = quote_getetags(res, MAX_PROPFIND_RESPONSE).await;
    }
    if mutating && res.status().is_success() && ctx.wait_for_commit {
        if let Err(e) = ctx.committer.flush().await {
            error!(error = %e, "commit failed after a successful request");
            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the change was saved but could not be committed\n",
            );
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getetag_values_get_quotes() {
        let xml = "<D:response><D:getetag>6-abc</D:getetag></D:response><D:getetag>def</D:getetag>";
        assert_eq!(
            quote_getetag_values(xml),
            "<D:response><D:getetag>\"6-abc\"</D:getetag></D:response><D:getetag>\"def\"</D:getetag>"
        );
    }

    #[test]
    fn already_quoted_weak_and_empty_values_are_left_alone() {
        let xml = "<D:getetag>\"q\"</D:getetag><D:getetag>W/\"w\"</D:getetag><D:getetag></D:getetag><D:getetag/>";
        assert_eq!(quote_getetag_values(xml), xml);
    }

    #[test]
    fn text_without_getetag_is_unchanged() {
        let xml = "<D:multistatus><D:href>/a</D:href></D:multistatus>";
        assert_eq!(quote_getetag_values(xml), xml);
    }
}

#[cfg(test)]
mod size_limit_tests {
    use super::*;

    #[tokio::test]
    async fn a_response_over_the_limit_becomes_a_500() {
        let big = Response::new(Body::from("x".repeat(1000)));
        let res = quote_getetags(big, 100).await;
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn a_response_under_the_limit_passes_through_quoted() {
        let small = Response::new(Body::from("<D:getetag>a</D:getetag>"));
        let res = quote_getetags(small, 1000).await;
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"<D:getetag>\"a\"</D:getetag>");
    }
}
