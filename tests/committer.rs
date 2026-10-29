mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use davgit::change::{Change, Op};
use davgit::committer::Committer;
use davgit::config::{CommitConfig, CommitMode};
use davgit::notify::EventKind;
use davgit::repo::Repo;
use davgit::status::Status;

struct Fixture {
    dir: tempfile::TempDir,
    repo: Repo,
    sink: Arc<RecordingSink>,
    status: Arc<Status>,
    committer: Committer,
}

fn fixture(mode: CommitMode) -> Fixture {
    let (dir, repo) = temp_repo("main");
    let sink = RecordingSink::new();
    let status = Arc::new(Status::new());
    let cfg = CommitConfig {
        mode,
        ..CommitConfig::default()
    };
    let committer = Committer::spawn(repo.clone(), cfg, status.clone(), sink.clone());
    Fixture {
        dir,
        repo,
        sink,
        status,
        committer,
    }
}

fn put(f: &Fixture, rel: &str) {
    write(f.dir.path(), rel, rel.as_bytes());
    f.committer.record(Change::new(Op::Put, rel));
}

#[tokio::test(start_paused = true)]
async fn per_request_commits_each_change_immediately() {
    let f = fixture(CommitMode::PerRequest);
    put(&f, "a.md");
    wait_until(|| commit_count(&f.repo) == 1).await;
    put(&f, "b.md");
    wait_until(|| commit_count(&f.repo) == 2).await;
    assert_eq!(head_paths(&f.repo), vec!["a.md", "b.md"]);
    assert!(f.status.snapshot().last_commit_unix.is_some());
}

#[tokio::test(start_paused = true)]
async fn per_request_group_commit_coalesces_burst() {
    let f = fixture(CommitMode::PerRequest);
    for i in 0..100 {
        put(&f, &format!("d/f{i:03}.md"));
    }
    wait_until(|| head_paths(&f.repo).len() == 100).await;
    let commits = commit_count(&f.repo);
    assert!(commits < 100, "expected coalescing, got {commits} commits");
}

#[tokio::test(start_paused = true)]
async fn flush_waits_for_commit_and_returns_ok() {
    let f = fixture(CommitMode::Debounce);
    put(&f, "a.md");
    f.committer.flush().await.unwrap();
    assert_eq!(commit_count(&f.repo), 1);
    // nothing pending: flush is a no-op that succeeds
    f.committer.flush().await.unwrap();
    assert_eq!(commit_count(&f.repo), 1);
}

#[tokio::test(start_paused = true)]
async fn flush_returns_err_when_commit_fails() {
    let f = fixture(CommitMode::Debounce);
    put(&f, "a.md");
    set_objects_readonly(f.dir.path(), true);
    let result = f.committer.flush().await;
    set_objects_readonly(f.dir.path(), false);
    assert!(result.is_err());
    assert_eq!(commit_count(&f.repo), 0);
}

#[tokio::test(start_paused = true)]
async fn debounce_waits_for_quiet() {
    let f = fixture(CommitMode::Debounce);
    put(&f, "a.md");
    tokio::time::advance(Duration::from_secs(4)).await;
    settle().await;
    assert_eq!(commit_count(&f.repo), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_until(|| commit_count(&f.repo) == 1).await;
}

#[tokio::test(start_paused = true)]
async fn debounce_max_wait_caps_continuous_stream() {
    let f = fixture(CommitMode::Debounce);
    for t in 1..=59 {
        put(&f, &format!("f{t}.md"));
        tokio::time::advance(Duration::from_secs(1)).await;
    }
    settle().await;
    assert_eq!(commit_count(&f.repo), 0, "quiet period never elapsed");
    put(&f, "f60.md");
    tokio::time::advance(Duration::from_secs(1)).await;
    wait_until(|| commit_count(&f.repo) >= 1).await;
}

#[tokio::test(start_paused = true)]
async fn interval_commits_only_when_dirty() {
    let f = fixture(CommitMode::Interval);
    tokio::time::advance(Duration::from_secs(300)).await;
    settle().await;
    assert_eq!(commit_count(&f.repo), 0);
    put(&f, "a.md");
    tokio::time::advance(Duration::from_secs(240)).await;
    settle().await;
    assert_eq!(commit_count(&f.repo), 0);
    tokio::time::advance(Duration::from_secs(60)).await;
    wait_until(|| commit_count(&f.repo) == 1).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_flushes_pending() {
    let f = fixture(CommitMode::Debounce);
    put(&f, "a.md");
    f.committer.shutdown().await.unwrap();
    assert_eq!(commit_count(&f.repo), 1);
}

#[tokio::test(start_paused = true)]
async fn commit_failure_sends_alert_and_keeps_running() {
    let f = fixture(CommitMode::PerRequest);
    set_objects_readonly(f.dir.path(), true);
    put(&f, "a.md");
    wait_until(|| f.sink.count() == 1).await;
    assert_eq!(f.sink.kinds(), vec![EventKind::CommitFailed]);
    set_objects_readonly(f.dir.path(), false);

    // the failed change stays pending and is retried
    tokio::time::advance(Duration::from_secs(6)).await;
    wait_until(|| commit_count(&f.repo) == 1).await;
    put(&f, "b.md");
    wait_until(|| commit_count(&f.repo) == 2).await;
    assert_eq!(head_paths(&f.repo), vec!["a.md", "b.md"]);
    assert_eq!(
        f.sink.kinds(),
        vec![
            EventKind::CommitFailed,
            EventKind::Recovered(Box::new(EventKind::CommitFailed))
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn foreign_state_defers_commit_until_it_clears() {
    let f = fixture(CommitMode::PerRequest);
    let rebase = f.dir.path().join(".git/rebase-merge");
    std::fs::create_dir(&rebase).unwrap();
    put(&f, "a.md");
    settle().await;
    assert_eq!(commit_count(&f.repo), 0);
    assert!(f.status.snapshot().foreign_git_state);
    std::fs::remove_dir(&rebase).unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    wait_until(|| commit_count(&f.repo) == 1).await;
    assert!(!f.status.snapshot().foreign_git_state);
}

#[tokio::test(start_paused = true)]
async fn move_stages_both_paths() {
    let f = fixture(CommitMode::PerRequest);
    put(&f, "old.md");
    wait_until(|| commit_count(&f.repo) == 1).await;
    std::fs::rename(f.dir.path().join("old.md"), f.dir.path().join("new.md")).unwrap();
    f.committer
        .record(Change::with_target(Op::Move, "old.md", "new.md"));
    wait_until(|| commit_count(&f.repo) == 2).await;
    assert_eq!(head_paths(&f.repo), vec!["new.md"]);
}
