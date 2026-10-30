//! Many clients doing a random mix of operations at once. Afterwards, git's idea of the
//! tree must match the files on disk exactly, with nothing left uncommitted.

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::*;
use davlog::app::{self, Options};
use davlog::config::Config;
use davlog::repo::Repo;
use reqwest::Method;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn pick_path(rng: &mut Rng) -> String {
    match rng.below(3) {
        0 => format!("/f{}.md", rng.below(12)),
        1 => format!("/d{}/f{}.md", rng.below(4), rng.below(6)),
        _ => format!("/d{}/s{}/f{}.md", rng.below(4), rng.below(3), rng.below(4)),
    }
}

/// Every regular file under `root` (not `.git`), as relative path -> bytes.
fn disk_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" {
                continue;
            }
            let md = std::fs::symlink_metadata(&path).unwrap();
            if md.is_dir() {
                stack.push(path);
            } else if md.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// Every file in HEAD's tree, as relative path -> bytes.
fn head_files(repo: &Repo) -> BTreeMap<String, Vec<u8>> {
    repo.with_repository(|r| {
        let mut out = BTreeMap::new();
        let Ok(head) = r.head() else { return out };
        let tree = head.peel_to_tree().unwrap();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                let blob = r.find_blob(entry.id()).unwrap();
                out.insert(
                    format!("{dir}{}", entry.name().unwrap()),
                    blob.content().to_vec(),
                );
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        out
    })
}

async fn soak(mode: &str, clients: u64, ops: u64) {
    // DAVLOG_SOAK_SCALE=10 cargo test --test soak   runs ten times as many operations
    let scale: u64 = std::env::var("DAVLOG_SOAK_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let ops = ops * scale;
    let _ = tracing_subscriber::fmt()
        .with_env_filter("davlog=warn")
        .with_test_writer()
        .try_init();
    setup();
    let dir = tempfile::tempdir().unwrap();
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\nrepo_path = \"{}\"\n[auth]\nusername = \"u\"\n[commit]\nmode = \"{mode}\"\ndebounce = \"50ms\"\nmax_wait = \"200ms\"\n[maintenance]\ninterval = \"200ms\"\nloose_object_limit = 30\npack_limit = 3\n",
        dir.path().display()
    );
    let cfg = Config::parse(&toml, &|k: &str| {
        (k == "DAVLOG_PASSWORD").then(|| "p".to_string())
    })
    .unwrap();
    let notifier = RecordingNotifier::new();
    let app = app::start(
        cfg,
        Options {
            bootstrap: None,
            extra_notifiers: vec![notifier.clone()],
        },
        None,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"))
    .unwrap();
    let base = format!("http://{}", app.local_addr());

    let mut tasks = Vec::new();
    for client_id in 0..clients {
        let base = base.clone();
        tasks.push(tokio::spawn(async move {
            let c = reqwest::Client::new();
            let mut rng = Rng(0x9E3779B97F4A7C15 ^ ((client_id + 1) * 7919));
            for n in 0..ops {
                let path = pick_path(&mut rng);
                let url = format!("{base}{path}");
                let req = |m: &str, u: &str| {
                    c.request(Method::from_bytes(m.as_bytes()).unwrap(), u)
                        .basic_auth("u", Some("p"))
                };
                let label;
                let result = match rng.below(10) {
                    0..=4 => {
                        label = format!("PUT {path}");
                        // make parents first, as a real client would
                        let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                        let mut so_far = String::new();
                        for part in parent.split('/').filter(|p| !p.is_empty()) {
                            so_far.push('/');
                            so_far.push_str(part);
                            let _ = req("MKCOL", &format!("{base}{so_far}/")).send().await;
                        }
                        let body = format!(
                            "client {client_id} write {n} to {path}\n{}",
                            "x".repeat(rng.below(3000) as usize)
                        );
                        req("PUT", &url).body(body).send().await
                    }
                    5 | 6 => {
                        label = format!("DELETE {path}");
                        req("DELETE", &url).send().await
                    }
                    7 => {
                        let to = pick_path(&mut rng);
                        label = format!("MOVE {path} -> {to}");
                        req("MOVE", &url)
                            .header("destination", format!("{base}{to}"))
                            .send()
                            .await
                    }
                    8 => {
                        let to = pick_path(&mut rng);
                        label = format!("COPY {path} -> {to}");
                        req("COPY", &url)
                            .header("destination", format!("{base}{to}"))
                            .send()
                            .await
                    }
                    _ => {
                        let d = rng.below(4);
                        label = format!("DELETE /d{d}/");
                        req("DELETE", &format!("{base}/d{d}/")).send().await
                    }
                };
                let response = result.expect("the server stays up");
                let status = response.status();
                if status.is_server_error() && status.as_u16() != 507 {
                    let text = response.text().await.unwrap_or_default();
                    panic!("{label}: unexpected {status}: {text}");
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    app.shutdown().await.unwrap();

    let repo = Repo::open_or_init(dir.path(), "main").unwrap();
    assert!(!repo.is_dirty().unwrap(), "nothing may be left uncommitted");
    let on_disk = disk_files(dir.path());
    let in_git = head_files(&repo);
    let only_disk: Vec<_> = on_disk
        .keys()
        .filter(|k| !in_git.contains_key(*k))
        .collect();
    let only_git: Vec<_> = in_git
        .keys()
        .filter(|k| !on_disk.contains_key(*k))
        .collect();
    assert!(
        only_disk.is_empty(),
        "on disk but not in git: {only_disk:?}"
    );
    assert!(only_git.is_empty(), "in git but not on disk: {only_git:?}");
    for (path, bytes) in &on_disk {
        assert_eq!(
            &in_git[path], bytes,
            "content of {path} differs between disk and git"
        );
    }
    assert!(!on_disk.is_empty(), "the run did something");

    // every commit, tree and blob in the whole history can still be read, even though the
    // repository was repacked while it was being written to
    let fresh = git2::Repository::open(dir.path()).unwrap();
    let mut walk = fresh.revwalk().unwrap();
    walk.push_head().unwrap();
    let mut commits = 0;
    for id in walk {
        let commit = fresh.find_commit(id.unwrap()).unwrap();
        commit
            .tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |_, entry| {
                if entry.kind() == Some(git2::ObjectType::Blob) {
                    fresh.find_blob(entry.id()).unwrap();
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        commits += 1;
    }
    assert!(commits >= 1, "history was recorded");
    assert!(
        notifier.kinds().is_empty(),
        "no alerts expected during a healthy run: {:?}",
        notifier.kinds()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_request_mode_stays_consistent_under_a_mixed_load() {
    soak("per_request", 6, 120).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn debounce_mode_stays_consistent_under_a_mixed_load() {
    soak("debounce", 6, 120).await;
}
