mod common;

use std::path::Path;
use std::sync::Arc;

use common::*;
use davlog::app::{self, AppError, Options};
use davlog::bootstrap::Choice;
use davlog::config::Config;
use davlog::notify::EventKind;
use davlog::repo::Repo;
use reqwest::Method;

fn cfg(repo_path: &Path, extra: &str) -> Config {
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nrepo_path = \"{}\"\n[auth]\nusername = \"notes\"\n{extra}",
        repo_path.display()
    );
    Config::parse(&toml, &|k: &str| {
        (k == "DAVLOG_PASSWORD").then(|| "pw".to_string())
    })
    .unwrap()
}

fn opts(n: &Arc<RecordingNotifier>) -> Options {
    Options {
        bootstrap: None,
        extra_notifiers: vec![n.clone()],
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn req(m: &str, url: String) -> reqwest::RequestBuilder {
    client()
        .request(Method::from_bytes(m.as_bytes()).unwrap(), url)
        .basic_auth("notes", Some("pw"))
}

fn url(app: &app::App, path: &str) -> String {
    format!("http://{}{}", app.local_addr(), path)
}

async fn started(dir: &Path, extra: &str, n: &Arc<RecordingNotifier>) -> app::App {
    setup();
    app::start(cfg(dir, extra), opts(n), None)
        .await
        .unwrap_or_else(|e| panic!("start failed: {e}"))
        .expect("not aborted")
}

#[tokio::test]
async fn local_only_server_commits_put_and_runs_no_pusher() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(dir.path(), "", &n).await;
    let r = req("PUT", url(&app, "/a.md"))
        .body("hello")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    assert_eq!(head_paths(&repo), vec!["a.md"]);

    let health: serde_json::Value = client()
        .get(url(&app, "/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(health["last_commit_unix"].is_u64());
    assert!(
        health["last_push_unix"].is_null(),
        "no pusher in local-only mode"
    );
    app.shutdown().await.unwrap();
    assert!(n.kinds().is_empty(), "{:?}", n.kinds());
}

#[tokio::test]
async fn startup_commits_a_dirty_tree_left_from_a_crash() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    write(dir.path(), "left-behind.md", b"x");
    assert!(repo.is_dirty().unwrap());
    let n = RecordingNotifier::new();
    let app = started(dir.path(), "", &n).await;
    assert_eq!(head_paths(&repo), vec!["left-behind.md"]);
    assert!(!repo.is_dirty().unwrap());
    let msg = repo.with_repository(|r| {
        r.head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string()
    });
    assert!(msg.contains("found at startup"), "{msg}");
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_on_the_wrong_branch_with_push_configured_alerts_and_errors() {
    setup();
    let (_r, remote_url) = bare_remote();
    let dir = tempfile::tempdir().unwrap();
    drop(Repo::open_or_init(dir.path(), "dev").unwrap());
    let n = RecordingNotifier::new();
    let push = format!("[push]\nurl = \"{remote_url}\"\nbranch = \"main\"\n");
    let result = app::start(cfg(dir.path(), &push), opts(&n), None).await;
    let Err(AppError::Other(e)) = result else {
        panic!("expected an error");
    };
    assert!(format!("{e:#}").contains("dev"), "{e:#}");
    assert_eq!(
        n.kinds(),
        vec![EventKind::RepoBadState],
        "alert delivered before exit"
    );
}

#[tokio::test]
async fn startup_with_rebase_state_serves_reads_blocks_writes_and_alerts() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    write(dir.path(), "a.md", b"x");
    repo.commit_all("seed", &sig()).unwrap();
    std::fs::create_dir(dir.path().join(".git/rebase-merge")).unwrap();
    write(dir.path(), "uncommitted.md", b"y");

    let n = RecordingNotifier::new();
    let app = started(dir.path(), "", &n).await;
    assert_eq!(
        req("GET", url(&app, "/a.md"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        req("PUT", url(&app, "/b.md"))
            .body("z")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        head_paths(&repo),
        vec!["a.md"],
        "the person's dirty tree was not committed"
    );
    wait_until(|| n.kinds().contains(&EventKind::ForeignGitState)).await;

    std::fs::remove_dir(dir.path().join(".git/rebase-merge")).unwrap();
    let r = req("PUT", url(&app, "/b.md"))
        .body("z")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn graceful_shutdown_flushes_pending_debounced_changes() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(
        dir.path(),
        "[commit]\nmode = \"debounce\"\ndebounce = \"1h\"\nmax_wait = \"2h\"\n",
        &n,
    )
    .await;
    let r = req("PUT", url(&app, "/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    assert!(head_paths(&repo).is_empty(), "debounce has not fired");
    app.shutdown().await.unwrap();
    assert_eq!(head_paths(&repo), vec!["a.md"], "final commit on shutdown");
}

#[tokio::test]
async fn end_to_end_push_to_bare_remote() {
    setup();
    let (_r, remote_url) = bare_remote();
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let push = format!("[push]\nurl = \"{remote_url}\"\nbranch = \"main\"\ninterval = \"100ms\"\n");
    let app = started(dir.path(), &push, &n).await;
    req("PUT", url(&app, "/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    let remote = git2::Repository::open_bare(&remote_url).unwrap();
    wait_until(|| remote.refname_to_id("refs/heads/main").is_ok()).await;
    // the remote gets the ref a moment before davlog records the push time
    let mut health = serde_json::Value::Null;
    for _ in 0..50 {
        health = client()
            .get(url(&app, "/healthz"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if health["last_push_unix"].is_u64() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(health["last_push_unix"].is_u64(), "{health}");
    app.shutdown().await.unwrap();
    assert!(n.kinds().is_empty(), "{:?}", n.kinds());
}

#[tokio::test]
async fn git_config_from_the_app_config_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(
        dir.path(),
        "[git.config]\n\"core.autocrlf\" = \"false\"\n\"user.name\" = \"from app config\"\n",
        &n,
    )
    .await;
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    let (a, u) = repo.with_repository(|r| {
        let c = r.config().unwrap().snapshot().unwrap();
        (
            c.get_string("core.autocrlf").unwrap(),
            c.get_string("user.name").unwrap(),
        )
    });
    assert_eq!((a.as_str(), u.as_str()), ("false", "from app config"));
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn bootstrap_without_an_answer_returns_the_exit_code_10_error() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "mine.md", b"mine");
    let n = RecordingNotifier::new();
    let err = match app::start(cfg(dir.path(), ""), opts(&n), None).await {
        Err(e) => e,
        Ok(_) => panic!("expected an error"),
    };
    assert_eq!(err.exit_code(), 10);
    assert!(err.to_string().contains("--bootstrap"));
}

#[tokio::test]
async fn bootstrap_flag_init_keeps_files_and_starts() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "mine.md", b"mine");
    let n = RecordingNotifier::new();
    let options = Options {
        bootstrap: Some(Choice::Init),
        extra_notifiers: vec![n.clone()],
    };
    let app = app::start(cfg(dir.path(), ""), options, None)
        .await
        .unwrap()
        .unwrap();
    // the existing file is picked up by the startup commit
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    assert_eq!(head_paths(&repo), vec!["mine.md"]);
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn bootstrap_flag_abort_starts_nothing() {
    setup();
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "mine.md", b"mine");
    let n = RecordingNotifier::new();
    let options = Options {
        bootstrap: Some(Choice::Abort),
        extra_notifiers: vec![n],
    };
    assert!(app::start(cfg(dir.path(), ""), options, None)
        .await
        .unwrap()
        .is_none());
    assert!(!dir.path().join(".git").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_burst_has_no_index_lock_errors_and_commits_everything() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(dir.path(), "", &n).await;
    let base = url(&app, "");
    let mut tasks = Vec::new();
    for i in 0..50 {
        let base = base.clone();
        tasks.push(tokio::spawn(async move {
            let c = reqwest::Client::new();
            let put = c
                .put(format!("{base}/f{i:02}.md"))
                .basic_auth("notes", Some("pw"))
                .body(format!("{i}"))
                .send()
                .await
                .unwrap();
            let get = c
                .get(format!("{base}/f{i:02}.md"))
                .basic_auth("notes", Some("pw"))
                .send()
                .await
                .unwrap();
            (put.status(), get.status())
        }));
    }
    for t in tasks {
        let (put, get) = t.await.unwrap();
        assert!(put.is_success(), "{put}");
        assert!(get.is_success(), "{get}");
    }
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    assert_eq!(head_paths(&repo).len(), 50);
    assert!(n.kinds().is_empty(), "no commit failures: {:?}", n.kinds());
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_full_disk_alerts_once_and_shows_in_healthz() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(dir.path(), "[limits]\nmin_free_space = \"999TiB\"\n", &n).await;
    wait_until(|| n.kinds().contains(&EventKind::DiskLow)).await;
    let health: serde_json::Value = client()
        .get(url(&app, "/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["disk_low"], true, "{health}");
    let r = req("PUT", url(&app, "/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 507);
    app.shutdown().await.unwrap();
    assert_eq!(
        n.kinds()
            .iter()
            .filter(|k| **k == EventKind::DiskLow)
            .count(),
        1
    );
}

#[tokio::test]
async fn a_healthy_disk_shows_disk_low_false() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(dir.path(), "[limits]\nmin_free_space = \"0\"\n", &n).await;
    let health: serde_json::Value = client()
        .get(url(&app, "/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["disk_low"], false, "{health}");
    app.shutdown().await.unwrap();
}

fn loose_objects(dir: &Path) -> usize {
    let mut n = 0;
    for entry in std::fs::read_dir(dir.join(".git/objects"))
        .unwrap()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            // the directory may be removed by a repack while we count
            n += std::fs::read_dir(entry.path())
                .map(|d| d.count())
                .unwrap_or(0);
        }
    }
    n
}

#[tokio::test]
async fn maintenance_packs_loose_objects_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(
        dir.path(),
        "[maintenance]\ninterval = \"200ms\"\nloose_object_limit = 20\npack_limit = 5\n",
        &n,
    )
    .await;
    for i in 0..30 {
        let r = req("PUT", url(&app, &format!("/n{}.md", i % 5)))
            .body(format!("edit {i} {}", "pad ".repeat(50)))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }
    wait_until(|| loose_objects(dir.path()) < 20).await;
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    assert_eq!(
        head_paths(&repo).len(),
        5,
        "the tree is intact after repacking"
    );
    assert!(
        dir.path()
            .join(".git/objects/pack")
            .read_dir()
            .unwrap()
            .count()
            >= 2,
        "a pack and its index"
    );
    app.shutdown().await.unwrap();
    assert!(n.kinds().is_empty(), "{:?}", n.kinds());
}

#[tokio::test]
async fn maintenance_can_be_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(
        dir.path(),
        "[maintenance]\nenabled = false\ninterval = \"100ms\"\nloose_object_limit = 1\npack_limit = 1\n",
        &n,
    )
    .await;
    for i in 0..5 {
        req("PUT", url(&app, &format!("/n{i}.md")))
            .body("x")
            .send()
            .await
            .unwrap();
    }
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(loose_objects(dir.path()) > 5, "nothing was packed");
    app.shutdown().await.unwrap();
}

/// A remote that accepts the connection and then never answers. libgit2 would wait for it
/// for minutes; shutdown must not.
#[tokio::test]
async fn shutdown_makes_the_final_commit_even_if_a_push_is_stuck() {
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let _ = held_tx.send(stream);
        }
    });

    setup();
    let dir = tempfile::tempdir().unwrap();
    // an existing repository, so startup does not need the remote to answer
    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    repo.ensure_remote("origin", &format!("git://127.0.0.1:{port}/r.git"))
        .unwrap();
    let n = RecordingNotifier::new();
    let extra = format!(
        "[commit]\nmode = \"debounce\"\ndebounce = \"1h\"\nmax_wait = \"2h\"\n[push]\nurl = \"git://127.0.0.1:{port}/r.git\"\nbranch = \"main\"\ninterval = \"100ms\"\n"
    );
    let app = started(dir.path(), &extra, &n).await;
    let r = req("PUT", url(&app, "/a.md"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    // wait until the pusher is stuck talking to the silent remote
    let _held = tokio::task::spawn_blocking(move || {
        held_rx.recv_timeout(std::time::Duration::from_secs(10))
    })
    .await
    .unwrap()
    .expect("the pusher connected");

    let started_at = std::time::Instant::now();
    tokio::time::timeout(std::time::Duration::from_secs(20), app.shutdown())
        .await
        .expect("shutdown must not wait for the stuck push")
        .unwrap();
    assert!(started_at.elapsed() < std::time::Duration::from_secs(15));
    assert_eq!(head_paths(&repo), vec!["a.md"], "the final commit happened");
    drop(_held); // close the silent connection so the stuck git call can end
}

#[tokio::test]
async fn a_healthy_server_reports_no_failure() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(dir.path(), "", &n).await;
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(2500), app.failure()).await;
    assert!(quiet.is_err(), "nothing has died: {quiet:?}");
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_dead_background_task_is_noticed_and_named() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(dir.path(), "", &n).await;
    app.abort_task_for_test("git state monitor");
    let who = tokio::time::timeout(std::time::Duration::from_secs(5), app.failure())
        .await
        .expect("the dead task is noticed");
    assert_eq!(who, "git state monitor");
    app.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_dead_committer_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let n = RecordingNotifier::new();
    let app = started(dir.path(), "", &n).await;
    // ending the committer's actor stands in for it panicking
    app.committer().shutdown().await.unwrap();
    let who = tokio::time::timeout(std::time::Duration::from_secs(5), app.failure())
        .await
        .expect("the dead committer is noticed");
    assert_eq!(who, "committer");
    let _ = app.shutdown().await;
}
