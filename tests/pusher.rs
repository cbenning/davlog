mod common;

use std::sync::Arc;

use common::*;
use davlog::change::{Change, Op};
use davlog::committer::Committer;
use davlog::config::{CommitConfig, PushConfig};
use davlog::notify::EventKind;
use davlog::pusher::{Pusher, TickOutcome};
use davlog::repo::Repo;
use davlog::status::Status;
use git2::Repository;

struct F {
    _rdir: tempfile::TempDir,
    url: String,
    dir: tempfile::TempDir,
    repo: Repo,
    cfg: PushConfig,
    sink: Arc<RecordingSink>,
    status: Arc<Status>,
    committer: Committer,
    pusher: Pusher,
}

fn fixture() -> F {
    let (rdir, url) = bare_remote();
    let (dir, repo) = temp_repo("main");
    let cfg = push_cfg(&url);
    repo.ensure_remote("origin", &url).unwrap();
    let sink = RecordingSink::new();
    let status = Arc::new(Status::new());
    let committer = Committer::spawn(
        repo.clone(),
        CommitConfig::default(),
        status.clone(),
        sink.clone(),
    );
    let pusher = Pusher::new(
        repo.clone(),
        cfg.clone(),
        committer.clone(),
        sink.clone(),
        status.clone(),
    );
    F {
        _rdir: rdir,
        url,
        dir,
        repo,
        cfg,
        sink,
        status,
        committer,
        pusher,
    }
}

fn tip(url: &str) -> Option<git2::Oid> {
    Repository::open_bare(url)
        .unwrap()
        .refname_to_id("refs/heads/main")
        .ok()
}

fn head(repo: &Repo) -> git2::Oid {
    repo.with_repository(|r| r.head().unwrap().target().unwrap())
}

/// A pushed first commit, then a competing write on the remote, then our own commit.
async fn diverged() -> (F, git2::Oid) {
    let mut f = fixture();
    commit(&f.repo, f.dir.path(), "a.md", "one", "first");
    assert_eq!(f.pusher.tick().await, TickOutcome::Pushed);
    let theirs = other_writer_pushes(&f.url, "theirs.md", "t", "their change");
    commit(&f.repo, f.dir.path(), "b.md", "two", "our change");
    (f, theirs)
}

#[tokio::test]
async fn clean_push_updates_remote_and_status() {
    let mut f = fixture();
    commit(&f.repo, f.dir.path(), "a.md", "one", "first");
    assert_eq!(f.pusher.tick().await, TickOutcome::Pushed);
    assert_eq!(tip(&f.url), Some(head(&f.repo)));
    assert!(f.status.snapshot().last_push_unix.is_some());
    assert!(f.status.snapshot().push_paused.is_none());
    assert_eq!(f.sink.count(), 0);
    assert_eq!(f.pusher.tick().await, TickOutcome::UpToDate);
}

#[tokio::test]
async fn tick_commits_pending_changes_before_pushing() {
    let mut f = fixture();
    write(f.dir.path(), "a.md", b"one");
    f.committer.record(Change::new(Op::Put, "a.md"));
    assert_eq!(f.pusher.tick().await, TickOutcome::Pushed);
    assert_eq!(tip(&f.url), Some(head(&f.repo)));
    assert_eq!(head_paths(&f.repo), vec!["a.md"]);
}

#[tokio::test]
async fn divergence_pauses_alerts_once_and_leaves_repo_untouched() {
    let (mut f, theirs) = diverged().await;
    let before = head(&f.repo);
    assert_eq!(f.pusher.tick().await, TickOutcome::Paused);

    assert_eq!(head(&f.repo), before, "local branch untouched");
    assert!(
        !f.dir.path().join("theirs.md").exists(),
        "worktree untouched"
    );
    assert!(!f.repo.is_dirty().unwrap());
    assert_eq!(tip(&f.url), Some(theirs), "remote untouched, never forced");
    assert!(f.status.snapshot().push_paused.is_some());

    assert_eq!(f.sink.kinds(), vec![EventKind::PushPaused]);
    let event = f.sink.events.lock().unwrap()[0].clone();
    assert!(
        event.details.contains("ahead 1, behind 1"),
        "{}",
        event.details
    );
    assert!(event.details.contains("their change"), "{}", event.details);
    assert!(event.details.contains("not supported"), "{}", event.details);
    let workdir = f.repo.workdir().display().to_string();
    assert!(event
        .details
        .contains(&format!("git -C {workdir} fetch origin")));
    assert!(event.details.contains("rebase origin/main"));
    assert!(event.details.contains("--force-with-lease origin main"));
}

#[tokio::test]
async fn remote_ahead_only_is_treated_as_divergence_without_touching_worktree() {
    let mut f = fixture();
    commit(&f.repo, f.dir.path(), "a.md", "one", "first");
    f.pusher.tick().await;
    other_writer_pushes(&f.url, "t1.md", "1", "theirs 1");
    other_writer_pushes(&f.url, "t2.md", "2", "theirs 2");
    assert_eq!(f.pusher.tick().await, TickOutcome::Paused);
    assert!(!f.dir.path().join("t1.md").exists());
    let event = f.sink.events.lock().unwrap()[0].clone();
    assert!(
        event.details.contains("ahead 0, behind 2"),
        "{}",
        event.details
    );
}

#[tokio::test]
async fn while_paused_writes_and_local_commits_continue() {
    let (mut f, theirs) = diverged().await;
    assert_eq!(f.pusher.tick().await, TickOutcome::Paused);
    let commits_before = commit_count(&f.repo);

    write(f.dir.path(), "c.md", b"three");
    f.committer.record(Change::new(Op::Put, "c.md"));
    f.committer.flush().await.unwrap();
    assert_eq!(commit_count(&f.repo), commits_before + 1);

    assert_eq!(f.pusher.tick().await, TickOutcome::StillPaused);
    assert_eq!(tip(&f.url), Some(theirs));
    assert!(f.status.snapshot().push_paused.is_some());
}

#[tokio::test]
async fn recovery_after_remote_reset_pushes_and_sends_recovered() {
    let (mut f, _theirs) = diverged().await;
    assert_eq!(f.pusher.tick().await, TickOutcome::Paused);

    let parent = f.repo.with_repository(|r| {
        r.head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent_id(0)
            .unwrap()
    });
    Repository::open_bare(&f.url)
        .unwrap()
        .reference("refs/heads/main", parent, true, "operator reset")
        .unwrap();

    assert_eq!(f.pusher.tick().await, TickOutcome::Pushed);
    assert_eq!(tip(&f.url), Some(head(&f.repo)));
    assert!(f.status.snapshot().push_paused.is_none());
    assert_eq!(
        f.sink.kinds(),
        vec![
            EventKind::PushPaused,
            EventKind::Recovered(Box::new(EventKind::PushPaused))
        ]
    );
}

#[tokio::test]
async fn recovery_after_manual_merge_pushes() {
    let (mut f, theirs) = diverged().await;
    assert_eq!(f.pusher.tick().await, TickOutcome::Paused);

    // the operator merges by hand: a merge commit whose parents are ours and theirs
    f.repo.with_repository(|r| {
        let ours = r.head().unwrap().peel_to_commit().unwrap();
        let other = r.find_commit(theirs).unwrap();
        let s = sig();
        r.commit(
            Some("HEAD"),
            &s,
            &s,
            "merge",
            &ours.tree().unwrap(),
            &[&ours, &other],
        )
        .unwrap();
    });

    assert_eq!(f.pusher.tick().await, TickOutcome::Pushed);
    assert_eq!(tip(&f.url), Some(head(&f.repo)));
    assert_eq!(
        f.sink.kinds().last(),
        Some(&EventKind::Recovered(Box::new(EventKind::PushPaused)))
    );
}

#[tokio::test]
async fn three_consecutive_failures_alert_with_the_network_hint_then_recover() {
    let mut f = fixture();
    commit(&f.repo, f.dir.path(), "a.md", "one", "first");
    f.repo
        .with_repository(|r| r.remote_set_url("origin", "/definitely/not/a/repo"))
        .unwrap();

    assert_eq!(f.pusher.tick().await, TickOutcome::Failed);
    assert_eq!(f.pusher.tick().await, TickOutcome::Failed);
    assert_eq!(f.sink.count(), 0, "no alert before the third failure");
    assert_eq!(f.pusher.tick().await, TickOutcome::Failed);
    assert_eq!(f.sink.kinds(), vec![EventKind::PushFailing]);
    let details = f.sink.events.lock().unwrap()[0].details.clone();
    assert!(details.contains("reach the remote"), "{details}");

    f.repo
        .with_repository(|r| r.remote_set_url("origin", &f.cfg.url.clone().unwrap()))
        .unwrap();
    assert_eq!(f.pusher.tick().await, TickOutcome::Pushed);
    assert_eq!(
        f.sink.kinds().last(),
        Some(&EventKind::Recovered(Box::new(EventKind::PushFailing)))
    );
}

#[tokio::test]
async fn failures_below_the_threshold_that_recover_send_nothing() {
    let mut f = fixture();
    commit(&f.repo, f.dir.path(), "a.md", "one", "first");
    f.repo
        .with_repository(|r| r.remote_set_url("origin", "/definitely/not/a/repo"))
        .unwrap();
    f.pusher.tick().await;
    f.repo
        .with_repository(|r| r.remote_set_url("origin", &f.cfg.url.clone().unwrap()))
        .unwrap();
    assert_eq!(f.pusher.tick().await, TickOutcome::Pushed);
    assert_eq!(f.sink.count(), 0);
}

#[tokio::test]
async fn foreign_rebase_state_skips_tick_and_does_not_abort_it() {
    let mut f = fixture();
    commit(&f.repo, f.dir.path(), "a.md", "one", "first");
    let rebase = f.dir.path().join(".git/rebase-merge");
    std::fs::create_dir(&rebase).unwrap();
    std::fs::write(rebase.join("head-name"), "refs/heads/main").unwrap();

    assert_eq!(f.pusher.tick().await, TickOutcome::Skipped);
    assert_eq!(tip(&f.url), None, "nothing pushed");
    assert!(
        rebase.join("head-name").exists(),
        "the person's rebase state is untouched"
    );

    std::fs::remove_dir_all(&rebase).unwrap();
    assert_eq!(f.pusher.tick().await, TickOutcome::Pushed);
}
