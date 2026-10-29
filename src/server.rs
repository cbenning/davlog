//! HTTP front end: health check, Basic auth, the write gate while a person has git
//! state in `.git`, early size rejection, the WebDAV handler, and waiting for the
//! commit before answering when the config says so.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use dav_server::body::Body;
use dav_server::memls::MemLs;
use dav_server::DavHandler;
use http::{header, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tracing::{debug, error, warn};

use crate::auth;
use crate::brand;
use crate::committer::Committer;
use crate::config::{AuthConfig, CommitMode, Config};
use crate::davfs::GitFs;
use crate::repo::{ForeignState, Repo};
use crate::status::Status;

const FAILED_LOGIN_DELAY: Duration = Duration::from_millis(250);
const RETRY_AFTER_SECS: &str = "30";
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

struct Ctx {
    handler: DavHandler,
    auth: AuthConfig,
    committer: Committer,
    repo: Repo,
    status: Arc<Status>,
    max_file_size: Option<u64>,
    /// Answer only after the commit finished (per_request mode with `wait_for_commit`).
    wait_for_commit: bool,
}

pub struct Server {
    listener: TcpListener,
    ctx: Arc<Ctx>,
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
        let ctx = Arc::new(Ctx {
            handler,
            auth: cfg.auth.clone(),
            committer,
            repo,
            status,
            max_file_size: cfg.limits.max_file_size,
            wait_for_commit: cfg.commit.mode == CommitMode::PerRequest
                && cfg.commit.wait_for_commit,
        });
        Ok(Server { listener, ctx })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().expect("listener has an address")
    }

    /// Serve until `shutdown` resolves, then stop accepting and let in-flight requests finish.
    pub async fn serve(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let graceful = GracefulShutdown::new();
        tokio::pin!(shutdown);
        loop {
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
                        async move { Ok::<_, std::convert::Infallible>(handle(ctx, req).await) }
                    });
                    let conn = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service);
                    let conn = graceful.watch(conn);
                    tokio::spawn(async move {
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

async fn quote_getetags(res: Response<Body>) -> Response<Body> {
    let (mut parts, body) = res.into_parts();
    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(error = %e, "could not read the PROPFIND response");
            return text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error\n");
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

async fn handle(ctx: Arc<Ctx>, req: Request<Incoming>) -> Response<Body> {
    let mut res = route(&ctx, req).await;
    res.headers_mut()
        .insert(header::SERVER, HeaderValue::from_static(brand::USER_AGENT));
    res
}

async fn route(ctx: &Ctx, req: Request<Incoming>) -> Response<Body> {
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
            tokio::time::sleep(FAILED_LOGIN_DELAY).await;
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
    let mut res = ctx.handler.handle(req).await;
    if propfind && res.status().as_u16() == 207 {
        res = quote_getetags(res).await;
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
