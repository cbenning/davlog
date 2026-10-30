#![allow(dead_code)]

use std::path::Path;

use davlog::repo::Repo;
use git2::Signature;
use tempfile::TempDir;

pub fn setup() {
    davlog::repo::isolate_from_user_git_config();
}

pub fn temp_repo(branch: &str) -> (TempDir, Repo) {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::open_or_init(dir.path(), branch).unwrap();
    (dir, repo)
}

pub fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

pub fn sig() -> Signature<'static> {
    Signature::now("tester", "tester@localhost").unwrap()
}

/// Number of commits reachable from HEAD (0 on an unborn branch).
pub fn commit_count(repo: &Repo) -> usize {
    repo.with_repository(|r| {
        let Ok(head) = r.head() else { return 0 };
        let mut walk = r.revwalk().unwrap();
        walk.push(head.target().unwrap()).unwrap();
        walk.count()
    })
}

/// Sorted file paths in HEAD's tree.
pub fn head_paths(repo: &Repo) -> Vec<String> {
    repo.with_repository(|r| {
        let Ok(head) = r.head() else { return vec![] };
        let tree = head.peel_to_tree().unwrap();
        let mut out = vec![];
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                out.push(format!("{dir}{}", entry.name().unwrap()));
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        out.sort();
        out
    })
}

pub fn head_blob(repo: &Repo, path: &str) -> Vec<u8> {
    repo.with_repository(|r| {
        let tree = r.head().unwrap().peel_to_tree().unwrap();
        let entry = tree.get_path(Path::new(path)).unwrap();
        r.find_blob(entry.id()).unwrap().content().to_vec()
    })
}

pub fn parent_count_of_head(repo: &Repo) -> usize {
    repo.with_repository(|r| r.head().unwrap().peel_to_commit().unwrap().parent_count())
}

use std::sync::{Arc, Mutex};

use davlog::notify::{AlertSink, Event};

#[derive(Default)]
pub struct RecordingSink {
    pub events: Mutex<Vec<Event>>,
}

impl RecordingSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn kinds(&self) -> Vec<davlog::notify::EventKind> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.kind.clone())
            .collect()
    }

    pub fn count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

impl AlertSink for RecordingSink {
    fn alert(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
}

/// Poll `cond` until true. Uses real sleeps so blocking git work can finish while
/// tokio's clock is paused.
pub async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..5000 {
        if cond() {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("condition not met in time");
}

/// Give the actor a chance to run without waiting for any condition.
pub async fn settle() {
    for _ in 0..30 {
        tokio::task::yield_now().await;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

pub fn set_objects_readonly(root: &Path, readonly: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if readonly { 0o555 } else { 0o755 };
    std::fs::set_permissions(
        root.join(".git/objects"),
        std::fs::Permissions::from_mode(mode),
    )
    .unwrap();
}

use davlog::change::{Change, ChangeSink};

#[derive(Default)]
pub struct RecordingChanges {
    pub changes: Mutex<Vec<Change>>,
}

impl RecordingChanges {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn all(&self) -> Vec<Change> {
        self.changes.lock().unwrap().clone()
    }
}

impl ChangeSink for RecordingChanges {
    fn record(&self, change: Change) {
        self.changes.lock().unwrap().push(change);
    }
}

// ---- remote fixtures ----

use std::time::Duration;

use davlog::config::{PushAuth, PushConfig};
use git2::{Repository, RepositoryInitOptions};

pub fn bare_remote() -> (tempfile::TempDir, String) {
    setup();
    let dir = tempfile::tempdir().unwrap();
    let mut opts = RepositoryInitOptions::new();
    opts.bare(true).initial_head("main");
    Repository::init_opts(dir.path(), &opts).unwrap();
    let url = dir.path().to_str().unwrap().to_string();
    (dir, url)
}

pub fn push_cfg(url: &str) -> PushConfig {
    PushConfig {
        remote: "origin".into(),
        url: Some(url.into()),
        branch: "main".into(),
        interval: Duration::from_secs(600),
        auth: PushAuth::None,
    }
}

pub fn remote_tip(url: &str) -> Option<git2::Oid> {
    let repo = Repository::open_bare(url).unwrap();
    repo.refname_to_id("refs/heads/main").ok()
}

pub fn local_tip(repo: &Repo) -> git2::Oid {
    repo.with_repository(|r| r.head().unwrap().target().unwrap())
}

pub fn commit(repo: &Repo, dir: &Path, file: &str, body: &str, msg: &str) {
    write(dir, file, body.as_bytes());
    repo.commit_paths(&[file.to_string()], msg, &sig())
        .unwrap()
        .unwrap();
}

/// Another writer: clone, commit and push straight to the remote.
pub fn other_writer_pushes(url: &str, file: &str, body: &str, msg: &str) -> git2::Oid {
    let dir = tempfile::tempdir().unwrap();
    let clone = Repository::clone(url, dir.path()).unwrap();
    std::fs::write(dir.path().join(file), body).unwrap();
    let mut index = clone.index().unwrap();
    index.add_path(Path::new(file)).unwrap();
    index.write().unwrap();
    let tree = clone.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = clone.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    let s = sig();
    let oid = clone
        .commit(Some("HEAD"), &s, &s, msg, &tree, &parents)
        .unwrap();
    let branch = clone.head().unwrap().shorthand().unwrap().to_string();
    clone
        .find_remote("origin")
        .unwrap()
        .push(&[format!("refs/heads/{branch}:refs/heads/main")], None)
        .unwrap();
    oid
}

// ---- notifier fixture ----

use std::future::Future;
use std::pin::Pin;

use davlog::notify::Notifier;

#[derive(Default)]
pub struct RecordingNotifier {
    pub events: Mutex<Vec<Event>>,
}

impl RecordingNotifier {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn kinds(&self) -> Vec<davlog::notify::EventKind> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.kind.clone())
            .collect()
    }
}

impl Notifier for RecordingNotifier {
    fn name(&self) -> &str {
        "recording"
    }

    fn send<'a>(
        &'a self,
        event: &'a Event,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        })
    }
}
