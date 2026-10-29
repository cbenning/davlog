mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use davgit::committer::Committer;
use davgit::config::Config;
use davgit::repo::Repo;
use davgit::server::Server;
use davgit::status::Status;
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
    let env = move |k: &str| (k == "DAVGIT_PASSWORD").then(|| pw.clone());
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
        challenge.starts_with("Basic realm=\"davgit\""),
        "{challenge}"
    );
}

#[tokio::test]
async fn wrong_password_is_401_and_delayed() {
    let s = start("").await;
    let started = Instant::now();
    let r = client()
        .get(s.url("/"))
        .basic_auth("notes", Some("wrong"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(started.elapsed() >= Duration::from_millis(200));
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
        .starts_with("davgit/"));
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
