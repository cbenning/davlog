mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use davlog::committer::Committer;
use davlog::config::Config;
use davlog::repo::Repo;
use davlog::server::Server;
use davlog::status::Status;
use reqwest::{Method, StatusCode};

struct Running {
    addr: SocketAddr,
    dir: tempfile::TempDir,
    repo: Repo,
    status: Arc<Status>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Running {
    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    async fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.unwrap().unwrap();
        }
    }
}

async fn start(extra: &str) -> Running {
    start_with_password(extra, "pw").await
}

async fn start_with_password(extra: &str, password: &str) -> Running {
    let (dir, repo) = temp_repo("main");
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nrepo_path = \"{}\"\n[auth]\nusername = \"notes\"\n{extra}",
        dir.path().display()
    );
    let pw = password.to_string();
    let env = move |k: &str| (k == "DAVLOG_PASSWORD").then(|| pw.clone());
    let cfg = Config::parse(&toml, &env).unwrap();
    let status = Arc::new(Status::new());
    let sink = RecordingSink::new();
    let committer = Committer::spawn(repo.clone(), cfg.commit.clone(), status.clone(), sink);
    let server = Server::bind(&cfg, repo.clone(), committer, status.clone())
        .await
        .unwrap();
    let addr = server.local_addr();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.serve(async move {
        let _ = rx.await;
    }));
    Running {
        addr,
        dir,
        repo,
        status,
        stop: Some(stop),
        task: Some(task),
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn req(c: &reqwest::Client, m: &str, url: &str) -> reqwest::RequestBuilder {
    c.request(Method::from_bytes(m.as_bytes()).unwrap(), url)
        .basic_auth("notes", Some("pw"))
}

#[tokio::test]
async fn unauthenticated_is_401_with_challenge() {
    let s = start("").await;
    let r = client().get(s.url("/")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let challenge = r.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        challenge.starts_with("Basic realm=\"davlog\""),
        "{challenge}"
    );
}

async fn wrong_login_elapsed(extra: &str) -> (StatusCode, Duration) {
    let s = start(extra).await;
    let started = Instant::now();
    let r = client()
        .get(s.url("/"))
        .basic_auth("notes", Some("wrong"))
        .send()
        .await
        .unwrap();
    (r.status(), started.elapsed())
}

#[tokio::test]
async fn wrong_password_is_401_without_delay_by_default() {
    let (status, took) = wrong_login_elapsed("").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(took < Duration::from_millis(250), "{took:?}");
}

#[tokio::test]
async fn failed_login_delay_is_applied_when_configured() {
    // `failed_login_delay` belongs to [auth], which `start` already opened
    let (status, took) = wrong_login_elapsed("failed_login_delay = \"400ms\"\n").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(took >= Duration::from_millis(400), "{took:?}");
}

#[tokio::test]
async fn correct_password_put_get_roundtrip_and_server_header() {
    let s = start("").await;
    let c = client();
    let r = req(&c, "PUT", &s.url("/a.md"))
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert!(r.headers()["server"]
        .to_str()
        .unwrap()
        .starts_with("davlog/"));
    let r = req(&c, "GET", &s.url("/a.md")).send().await.unwrap();
    assert_eq!(r.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn argon2_password_hash_accepted() {
    use argon2::password_hash::PasswordHasher;
    let hash = argon2::Argon2::default()
        .hash_password(b"pw")
        .unwrap()
        .to_string();
    let (dir, repo) = temp_repo("main");
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nrepo_path = \"{}\"\n[auth]\nusername = \"notes\"\npassword_hash = \"{hash}\"\n",
        dir.path().display()
    );
    let cfg = Config::parse(&toml, &|_: &str| None).unwrap();
    let status = Arc::new(Status::new());
    let committer = Committer::spawn(
        repo.clone(),
        cfg.commit.clone(),
        status.clone(),
        RecordingSink::new(),
    );
    let server = Server::bind(&cfg, repo, committer, status).await.unwrap();
    let addr = server.local_addr();
    tokio::spawn(server.serve(std::future::pending()));
    let r = client()
        .request(
            Method::from_bytes(b"PROPFIND").unwrap(),
            format!("http://{addr}/"),
        )
        .header("depth", "0")
        .basic_auth("notes", Some("pw"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 207);
}

#[tokio::test]
async fn healthz_needs_no_auth_and_leaks_no_paths() {
    let s = start("").await;
    let c = client();
    req(&c, "PUT", &s.url("/very-secret-name.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    let r = c.get(s.url("/healthz")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let text = r.text().await.unwrap();
    assert!(!text.contains("very-secret-name"));
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(json["last_commit_unix"].is_u64(), "{text}");
    assert_eq!(json["foreign_git_state"], false);
    assert!(json["push_paused"].is_null());
}

#[tokio::test]
async fn put_response_waits_for_commit() {
    let s = start("").await;
    let r = req(&client(), "PUT", &s.url("/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert_eq!(
        head_paths(&s.repo),
        vec!["a.md"],
        "committed before the response"
    );
}

#[tokio::test]
async fn commit_failure_gives_500_when_waiting() {
    let s = start("").await;
    set_objects_readonly(s.dir.path(), true);
    let r = req(&client(), "PUT", &s.url("/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    set_objects_readonly(s.dir.path(), false);
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn wait_for_commit_false_does_not_wait_for_the_commit() {
    let s = start("[commit]\nwait_for_commit = false\n").await;
    set_objects_readonly(s.dir.path(), true);
    let r = req(&client(), "PUT", &s.url("/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    set_objects_readonly(s.dir.path(), false);
    assert_eq!(
        r.status(),
        StatusCode::CREATED,
        "commit failure is not reported to the client"
    );
}

#[tokio::test]
async fn foreign_rebase_state_blocks_writes_with_503_but_reads_work() {
    let s = start("").await;
    let c = client();
    req(&c, "PUT", &s.url("/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    let rebase = s.dir.path().join(".git/rebase-merge");
    std::fs::create_dir(&rebase).unwrap();

    const LOCK: &str = r#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockinfo>"#;
    const PATCH: &str = r#"<?xml version="1.0"?><D:propertyupdate xmlns:D="DAV:"><D:set><D:prop><D:displayname>x</D:displayname></D:prop></D:set></D:propertyupdate>"#;
    for (method, path, body) in [
        ("PUT", "/b.md", "x"),
        ("DELETE", "/a.md", ""),
        ("MKCOL", "/d/", ""),
        ("MOVE", "/a.md", ""),
        ("COPY", "/a.md", ""),
        ("PROPPATCH", "/a.md", PATCH),
        ("LOCK", "/a.md", LOCK),
    ] {
        let r = req(&c, method, &s.url(path))
            .header("destination", s.url("/z.md"))
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE, "{method}");
        assert!(r.headers().contains_key("retry-after"), "{method}");
    }
    assert!(s.dir.path().join("a.md").exists());
    for (method, path, depth) in [
        ("GET", "/a.md", "0"),
        ("HEAD", "/a.md", "0"),
        ("PROPFIND", "/", "1"),
        ("OPTIONS", "/", "0"),
    ] {
        let r = req(&c, method, &s.url(path))
            .header("depth", depth)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{method} gave {}", r.status());
    }
    assert!(s.status.snapshot().foreign_git_state);

    std::fs::remove_dir(&rebase).unwrap();
    let r = req(&c, "PUT", &s.url("/b.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn recursive_delete_response_waits_until_all_children_are_committed() {
    let s = start("").await;
    let c = client();
    req(&c, "MKCOL", &s.url("/d/")).send().await.unwrap();
    for n in ["x", "y", "z"] {
        req(&c, "PUT", &s.url(&format!("/d/{n}.md")))
            .body(n)
            .send()
            .await
            .unwrap();
    }
    assert_eq!(head_paths(&s.repo).len(), 3);
    let r = req(&c, "DELETE", &s.url("/d/")).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    assert!(
        head_paths(&s.repo).is_empty(),
        "deletes committed before the response"
    );
    assert!(!s.dir.path().join("d").exists());
}

#[tokio::test]
async fn lock_and_unlock_work() {
    let s = start("").await;
    let c = client();
    req(&c, "PUT", &s.url("/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    let r = req(&c, "OPTIONS", &s.url("/a.md")).send().await.unwrap();
    let dav = r.headers()["dav"].to_str().unwrap().to_string();
    assert!(dav.contains('2'), "class 2 locking advertised: {dav}");

    let lock = r#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>me</D:owner></D:lockinfo>"#;
    let r = req(&c, "LOCK", &s.url("/a.md"))
        .body(lock)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let token = r.headers()["lock-token"].to_str().unwrap().to_string();
    let r = req(&c, "UNLOCK", &s.url("/a.md"))
        .header("lock-token", token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn propfind_depth_1_lists_children_without_dot_git() {
    let s = start("").await;
    let c = client();
    req(&c, "PUT", &s.url("/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    let r = req(&c, "PROPFIND", &s.url("/"))
        .header("depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 207);
    let text = r.text().await.unwrap();
    assert!(text.contains("a.md") && !text.contains(".git"), "{text}");
}

#[tokio::test]
async fn invalid_utf8_path_is_400() {
    let s = start("").await;
    for m in ["GET", "PUT"] {
        let r = req(&client(), m, &s.url("/%ff%fe"))
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{m}");
    }
}

#[tokio::test]
async fn put_over_the_limit_is_413() {
    let s = start("[limits]\nmax_file_size = \"10\"\n").await;
    let r = req(&client(), "PUT", &s.url("/big.bin"))
        .body(vec![0u8; 11])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!s.dir.path().join("big.bin").exists());
}

#[tokio::test]
async fn graceful_shutdown_stops_the_server() {
    let mut s = start("").await;
    let c = client();
    req(&c, "PUT", &s.url("/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), s.stop())
        .await
        .expect("serve() returned");
    let late = reqwest::Client::new()
        .get(s.url("/healthz"))
        .timeout(Duration::from_secs(2))
        .send()
        .await;
    assert!(late.is_err(), "no longer accepting");
}

#[tokio::test]
async fn propfind_getetag_is_a_quoted_strong_etag_matching_the_header() {
    let s = start("").await;
    let c = client();
    req(&c, "PUT", &s.url("/a.md"))
        .body("hello")
        .send()
        .await
        .unwrap();
    let head = req(&c, "HEAD", &s.url("/a.md")).send().await.unwrap();
    let header_etag = head.headers()["etag"].to_str().unwrap().to_string();
    assert!(
        header_etag.starts_with('"') && header_etag.ends_with('"'),
        "{header_etag}"
    );

    let r = req(&c, "PROPFIND", &s.url("/"))
        .header("depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 207);
    let xml = r.text().await.unwrap();
    let values: Vec<&str> = xml
        .split("<D:getetag>")
        .skip(1)
        .map(|rest| rest.split("</D:getetag>").next().unwrap())
        .collect();
    assert_eq!(values.len(), 2, "root and a.md: {xml}");
    assert!(
        values
            .iter()
            .all(|v| v.starts_with('"') && v.ends_with('"')),
        "{values:?}"
    );
    assert!(
        values.contains(&header_etag.as_str()),
        "{values:?} vs {header_etag}"
    );
}

// ---- connection and request hardening ----

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn basic() -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("notes:pw")
    )
}

/// Wait for the server to close (or reset) the connection, or for `within` to pass.
/// Returns true if the connection was closed.
async fn closed_within(stream: &mut TcpStream, within: Duration) -> bool {
    let mut buf = [0u8; 1024];
    let result = tokio::time::timeout(within, async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => continue, // an error response, then the close
            }
        }
    })
    .await;
    result.is_ok()
}

#[tokio::test]
async fn propfind_depth_infinity_is_refused() {
    let s = start("").await;
    let r = req(&client(), "PROPFIND", &s.url("/"))
        .header("depth", "infinity")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 501);
    assert!(r.text().await.unwrap().contains("propfind-finite-depth"));
}

#[tokio::test]
async fn propfind_without_a_depth_header_does_not_recurse() {
    let s = start("").await;
    let c = client();
    req(&c, "MKCOL", &s.url("/a/")).send().await.unwrap();
    req(&c, "MKCOL", &s.url("/a/b/")).send().await.unwrap();
    req(&c, "PUT", &s.url("/a/b/deep.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    let r = req(&c, "PROPFIND", &s.url("/")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 207);
    let text = r.text().await.unwrap();
    assert!(text.contains("/a/"), "{text}");
    assert!(
        !text.contains("deep.md") && !text.contains("/a/b"),
        "must not recurse: {text}"
    );
}

#[tokio::test]
async fn an_oversized_propfind_body_is_rejected() {
    let s = start("").await;
    let big = format!(
        "<?xml version=\"1.0\"?><D:propfind xmlns:D=\"DAV:\"><D:prop>{}</D:prop></D:propfind>",
        "<D:x/>".repeat(40_000)
    );
    let r = req(&client(), "PROPFIND", &s.url("/"))
        .header("depth", "0")
        .body(big)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error(), "{}", r.status());
}

#[tokio::test]
async fn a_client_that_never_finishes_its_headers_is_dropped() {
    let s = start("[limits]\nheader_read_timeout = \"300ms\"\n").await;
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    stream.write_all(b"GET / HTT").await.unwrap();
    assert!(
        closed_within(&mut stream, Duration::from_secs(5)).await,
        "slow headers must time out"
    );
}

#[tokio::test]
async fn an_idle_keep_alive_connection_is_closed() {
    let s = start("[limits]\nheader_read_timeout = \"300ms\"\n").await;
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    assert!(
        closed_within(&mut stream, Duration::from_secs(5)).await,
        "an idle connection holds a slot only briefly"
    );
}

#[tokio::test]
async fn connections_beyond_the_limit_wait_until_a_slot_frees_up() {
    let s = start("[limits]\nmax_connections = 2\n").await;
    let first = TcpStream::connect(s.addr).await.unwrap();
    let _second = TcpStream::connect(s.addr).await.unwrap();
    // let the server accept both before the third arrives
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut third = TcpStream::connect(s.addr).await.unwrap();
    third
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 64];
    let early = tokio::time::timeout(Duration::from_millis(600), third.read(&mut buf)).await;
    assert!(
        early.is_err(),
        "the third connection is not served while two are open"
    );

    drop(first);
    let n = tokio::time::timeout(Duration::from_secs(5), third.read(&mut buf))
        .await
        .expect("served once a slot is free")
        .unwrap();
    assert!(n > 0 && buf.starts_with(b"HTTP/1.1 200"), "{:?}", &buf[..n]);
}

#[tokio::test]
async fn an_upload_that_stalls_is_cut_off_and_leaves_nothing() {
    let s = start("[limits]\nbody_idle_timeout = \"300ms\"\n").await;
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let head = format!(
        "PUT /stalled.md HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\nContent-Length: 10\r\n\r\nabc",
        basic()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    assert!(
        closed_within(&mut stream, Duration::from_secs(5)).await,
        "a stalled upload must be cut off"
    );
    assert!(!s.dir.path().join("stalled.md").exists());
    let tmp = s.dir.path().join(".git/davlog-tmp");
    assert_eq!(std::fs::read_dir(tmp).map(|d| d.count()).unwrap_or(0), 0);
    assert!(head_paths(&s.repo).is_empty());
}

#[tokio::test]
async fn a_slow_but_steady_upload_is_not_cut_off() {
    let s = start("[limits]\nbody_idle_timeout = \"600ms\"\n").await;
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let head = format!(
        "PUT /steady.md HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\nContent-Length: 6\r\nConnection: close\r\n\r\n",
        basic()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    for chunk in ["ab", "cd", "ef"] {
        tokio::time::sleep(Duration::from_millis(300)).await;
        stream.write_all(chunk.as_bytes()).await.unwrap();
    }
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out))
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.starts_with(b"HTTP/1.1 201"),
        "{}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(
        std::fs::read(s.dir.path().join("steady.md")).unwrap(),
        b"abcdef"
    );
}

// ---- disk guard ----

#[tokio::test]
async fn writes_that_add_data_get_507_when_free_space_is_below_the_floor() {
    let s = start("[limits]\nmin_free_space = \"999TiB\"\n").await;
    std::fs::write(s.dir.path().join("existing.md"), "x").unwrap();
    let c = client();
    for (method, path) in [("PUT", "/new.md"), ("MKCOL", "/dir/")] {
        let r = req(&c, method, &s.url(path))
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 507, "{method}");
    }
    let r = req(&c, "COPY", &s.url("/existing.md"))
        .header("destination", s.url("/copy.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 507, "COPY");
    assert!(!s.dir.path().join("new.md").exists());

    // reads and deletes still work, so a full disk can be dealt with
    assert_eq!(
        req(&c, "GET", &s.url("/existing.md"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let r = req(&c, "PROPFIND", &s.url("/"))
        .header("depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 207);
    let r = req(&c, "DELETE", &s.url("/existing.md"))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
}

#[tokio::test]
async fn writes_get_507_when_the_repo_is_over_its_size_cap() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.bin"), vec![0u8; 50_000]).unwrap();
    // start() makes its own temp repo, so build this one by hand
    setup();
    let rdir = dir;
    let r = davlog::repo::Repo::open_or_init(rdir.path(), "main").unwrap();
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nrepo_path = \"{}\"\n[auth]\nusername = \"notes\"\n[limits]\nmax_repo_size = \"10KiB\"\n",
        rdir.path().display()
    );
    let cfg = Config::parse(&toml, &|k: &str| {
        (k == "DAVLOG_PASSWORD").then(|| "pw".to_string())
    })
    .unwrap();
    let status = Arc::new(Status::new());
    let committer = Committer::spawn(
        r.clone(),
        cfg.commit.clone(),
        status.clone(),
        RecordingSink::new(),
    );
    let server = Server::bind(&cfg, r, committer, status).await.unwrap();
    let addr = server.local_addr();
    tokio::spawn(server.serve(std::future::pending()));
    let c = client();
    let put = c
        .put(format!("http://{addr}/new.md"))
        .basic_auth("notes", Some("pw"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(put.status().as_u16(), 507);
    let del = c
        .delete(format!("http://{addr}/big.bin"))
        .basic_auth("notes", Some("pw"))
        .send()
        .await
        .unwrap();
    assert!(del.status().is_success(), "{}", del.status());
}

// ---- response headers for stored content ----

async fn header_of(s: &Running, path: &str, name: &str) -> Option<String> {
    let r = req(&client(), "GET", &s.url(path)).send().await.unwrap();
    assert!(r.status().is_success(), "{path}: {}", r.status());
    r.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
}

#[tokio::test]
async fn stored_files_are_served_with_nosniff() {
    let s = start("").await;
    for (path, body) in [
        ("/a.md", "# hi"),
        ("/b.png", "not really a png"),
        ("/c.html", "<b>x</b>"),
    ] {
        req(&client(), "PUT", &s.url(path))
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            header_of(&s, path, "x-content-type-options")
                .await
                .as_deref(),
            Some("nosniff"),
            "{path}"
        );
    }
    let unauth = client().get(s.url("/a.md")).send().await.unwrap();
    assert_eq!(unauth.headers()["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn active_content_types_get_a_sandbox_csp() {
    let s = start("").await;
    for path in ["/page.html", "/page.xhtml", "/image.svg", "/data.xml"] {
        req(&client(), "PUT", &s.url(path))
            .body("<x/>")
            .send()
            .await
            .unwrap();
        assert_eq!(
            header_of(&s, path, "content-security-policy")
                .await
                .as_deref(),
            Some("sandbox"),
            "{path} must not run script on the server's origin"
        );
    }
}

#[tokio::test]
async fn ordinary_content_gets_no_csp_so_pdfs_and_images_keep_working_in_a_browser() {
    let s = start("").await;
    for path in ["/note.md", "/photo.png", "/paper.pdf", "/data.json"] {
        req(&client(), "PUT", &s.url(path))
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(
            header_of(&s, path, "content-security-policy").await,
            None,
            "{path}"
        );
    }
}

#[tokio::test]
async fn a_client_that_stops_reading_a_download_is_cut_off() {
    let s = start("[limits]\nbody_idle_timeout = \"400ms\"\n").await;
    let size = 96 * 1024 * 1024;
    std::fs::write(s.dir.path().join("big.bin"), vec![7u8; size]).unwrap();

    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let request = format!(
        "GET /big.bin HTTP/1.1\r\nHost: x\r\nAuthorization: {}\r\n\r\n",
        basic()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    // read nothing for a while: the server's send buffer fills and it stalls
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // the server must have given up: draining now ends early instead of delivering it all
    let mut total = 0usize;
    let mut buf = vec![0u8; 1 << 20];
    let ended = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n,
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the connection ended");
    assert!(
        total < size,
        "received {total} of {size}: the stalled download was cut off"
    );
}

#[tokio::test]
async fn a_slow_but_steady_download_is_not_cut_off() {
    let s = start("[limits]\nbody_idle_timeout = \"1s\"\n").await;
    let size = 8 * 1024 * 1024;
    std::fs::write(s.dir.path().join("mid.bin"), vec![1u8; size]).unwrap();
    let mut stream = TcpStream::connect(s.addr).await.unwrap();
    let request = format!(
        "GET /mid.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: {}\r\n\r\n",
        basic()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut total = 0usize;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                total += n;
                tokio::time::sleep(Duration::from_millis(20)).await; // reads slowly, never stalls
            }
            other => panic!("read failed: {other:?}"),
        }
    }
    assert!(total >= size, "received {total} of {size}");
}

// ---- /readyz ----

#[tokio::test]
async fn readyz_is_ok_for_a_healthy_server_and_needs_no_login() {
    let s = start("").await;
    let r = client().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let json: serde_json::Value = r.json().await.unwrap();
    assert_eq!(json["ok"], true, "{json}");
    assert_eq!(json["problems"].as_array().unwrap().len(), 0);
    let head = client().head(s.url("/readyz")).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);
}

#[tokio::test]
async fn readyz_is_503_and_lists_the_problem_when_pushing_is_paused() {
    let s = start("").await;
    s.status
        .set_paused(Some("the remote branch has diverged".into()));
    let r = client().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json: serde_json::Value = r.json().await.unwrap();
    assert_eq!(json["ok"], false);
    let text = json["problems"].to_string();
    assert!(
        text.contains("pushing is paused") && text.contains("diverged"),
        "{text}"
    );
    // healthz keeps meaning "alive"
    assert_eq!(
        client()
            .get(s.url("/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    s.status.set_paused(None);
    assert_eq!(
        client()
            .get(s.url("/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn readyz_sees_a_rebase_in_progress_right_away() {
    let s = start("").await;
    let rebase = s.dir.path().join(".git/rebase-merge");
    std::fs::create_dir(&rebase).unwrap();
    let r = client().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.text().await.unwrap().contains("rebase or merge"));
    std::fs::remove_dir(&rebase).unwrap();
    assert_eq!(
        client()
            .get(s.url("/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn readyz_sees_a_full_disk() {
    let s = start("[limits]\nmin_free_space = \"999TiB\"\n").await;
    let r = client().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.text().await.unwrap().contains("disk"));
}

#[tokio::test]
async fn readyz_reports_failing_commits_and_pushes() {
    let s = start("").await;
    s.status.set_commit_failing(true);
    s.status.set_push_failing(true);
    let r = client().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    let text = r.text().await.unwrap();
    assert!(
        text.contains("commits are failing") && text.contains("pushes are failing"),
        "{text}"
    );
}

#[tokio::test]
async fn healthz_now_also_shows_the_failure_flags() {
    let s = start("").await;
    let json: serde_json::Value = client()
        .get(s.url("/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(json["commit_failing"], false);
    assert_eq!(json["push_failing"], false);
}
