mod common;

use common::*;
use davlog::config::PushConfig;
use davlog::repo::{PushError, PushResult, Repo};
use git2::Repository;

fn synced_pair() -> (
    tempfile::TempDir,
    String,
    tempfile::TempDir,
    Repo,
    PushConfig,
) {
    let (rdir, url) = bare_remote();
    let (dir, repo) = temp_repo("main");
    let cfg = push_cfg(&url);
    repo.ensure_remote("origin", &url).unwrap();
    commit(&repo, dir.path(), "a.md", "one", "first");
    assert_eq!(repo.push_branch(&cfg).unwrap(), PushResult::Pushed);
    (rdir, url, dir, repo, cfg)
}

#[test]
fn push_first_commit_to_empty_remote() {
    let (_rdir, url) = bare_remote();
    let (dir, repo) = temp_repo("main");
    repo.ensure_remote("origin", &url).unwrap();
    commit(&repo, dir.path(), "a.md", "one", "first");
    assert_eq!(
        repo.push_branch(&push_cfg(&url)).unwrap(),
        PushResult::Pushed
    );
    assert_eq!(remote_tip(&url), Some(local_tip(&repo)));
}

#[test]
fn push_second_time_is_up_to_date() {
    let (_r, _url, _d, repo, cfg) = synced_pair();
    assert_eq!(repo.push_branch(&cfg).unwrap(), PushResult::UpToDate);
}

#[test]
fn push_new_commit_is_pushed() {
    let (_r, url, dir, repo, cfg) = synced_pair();
    commit(&repo, dir.path(), "b.md", "two", "second");
    assert_eq!(repo.push_branch(&cfg).unwrap(), PushResult::Pushed);
    assert_eq!(remote_tip(&url), Some(local_tip(&repo)));
}

#[test]
fn rejected_push_returns_non_fast_forward_and_does_not_force() {
    let (_r, url, dir, repo, cfg) = synced_pair();
    let theirs = other_writer_pushes(&url, "theirs.md", "t", "their change");
    commit(&repo, dir.path(), "b.md", "two", "our change");
    assert_eq!(repo.push_branch(&cfg).unwrap(), PushResult::NonFastForward);
    assert_eq!(remote_tip(&url), Some(theirs), "remote untouched");
}

#[test]
fn fetch_does_not_touch_local_branch_or_worktree() {
    let (_r, url, dir, repo, cfg) = synced_pair();
    other_writer_pushes(&url, "theirs.md", "t", "their change");
    commit(&repo, dir.path(), "b.md", "two", "our change");
    let before = local_tip(&repo);
    repo.fetch_branch(&cfg).unwrap();
    assert_eq!(local_tip(&repo), before);
    assert!(!dir.path().join("theirs.md").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.md")).unwrap(),
        "two"
    );
    assert!(!repo.is_dirty().unwrap());
}

#[test]
fn divergence_counts_ahead_and_behind() {
    let (_r, url, dir, repo, cfg) = synced_pair();
    let theirs = other_writer_pushes(&url, "theirs.md", "t", "their change\n\nbody");
    commit(&repo, dir.path(), "b.md", "two", "our change");
    repo.fetch_branch(&cfg).unwrap();
    let d = repo.divergence(&cfg).unwrap();
    assert_eq!((d.ahead, d.behind), (1, 1));
    assert_eq!(d.remote_tip, Some(theirs));
    assert!(
        d.remote_tip_summary.contains("their change"),
        "{}",
        d.remote_tip_summary
    );
    assert!(!d.remote_tip_summary.contains("body"));
}

#[test]
fn remote_strictly_ahead_reports_behind_only() {
    let (_r, url, _d, repo, cfg) = synced_pair();
    other_writer_pushes(&url, "t1.md", "1", "theirs 1");
    other_writer_pushes(&url, "t2.md", "2", "theirs 2");
    assert_eq!(repo.push_branch(&cfg).unwrap(), PushResult::NonFastForward);
    repo.fetch_branch(&cfg).unwrap();
    let d = repo.divergence(&cfg).unwrap();
    assert_eq!((d.ahead, d.behind), (0, 2));
}

#[test]
fn local_that_contains_remote_tip_is_a_fast_forward_again() {
    let (_r, url, dir, repo, cfg) = synced_pair();
    other_writer_pushes(&url, "t.md", "t", "theirs");
    commit(&repo, dir.path(), "b.md", "two", "ours");
    assert_eq!(repo.push_branch(&cfg).unwrap(), PushResult::NonFastForward);
    // the operator makes the remote an ancestor of local: simulate by resetting the remote
    let bare = Repository::open_bare(&url).unwrap();
    let ours_parent = repo.with_repository(|r| {
        r.head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent_id(0)
            .unwrap()
    });
    bare.reference("refs/heads/main", ours_parent, true, "operator reset")
        .unwrap();
    assert_eq!(repo.push_branch(&cfg).unwrap(), PushResult::Pushed);
}

#[test]
fn ensure_remote_is_idempotent_and_rejects_url_change() {
    let (_rdir, url) = bare_remote();
    let (_dir, repo) = temp_repo("main");
    repo.ensure_remote("origin", &url).unwrap();
    repo.ensure_remote("origin", &url).unwrap();
    assert!(repo.ensure_remote("origin", "/somewhere/else").is_err());
}

#[test]
fn unreachable_remote_is_a_network_error() {
    let (dir, repo) = temp_repo("main");
    let cfg = push_cfg("/definitely/not/a/repo");
    repo.ensure_remote("origin", "/definitely/not/a/repo")
        .unwrap();
    commit(&repo, dir.path(), "a.md", "one", "first");
    match repo.push_branch(&cfg) {
        Err(PushError::Network(_)) => {}
        other => panic!("expected a network error, got {other:?}"),
    }
}

#[test]
fn push_never_sends_a_force_refspec() {
    // Rejection above already proves the remote tip survives; this pins the refspec shape.
    let (_r, url, dir, repo, cfg) = synced_pair();
    let theirs = other_writer_pushes(&url, "t.md", "t", "theirs");
    commit(&repo, dir.path(), "b.md", "two", "ours");
    for _ in 0..3 {
        assert_eq!(repo.push_branch(&cfg).unwrap(), PushResult::NonFastForward);
    }
    assert_eq!(remote_tip(&url), Some(theirs));
}

#[test]
fn a_hanging_remote_does_not_block_local_commits() {
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    // A remote that accepts the connection and then says nothing.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "git://127.0.0.1:{}/repo.git",
        listener.local_addr().unwrap().port()
    );
    let (held_tx, held_rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = held_tx.send(stream);
        }
    });

    let (dir, repo) = temp_repo("main");
    let cfg = push_cfg(&url);
    repo.ensure_remote("origin", &url).unwrap();
    commit(&repo, dir.path(), "a.md", "one", "first");

    let pushing = repo.clone();
    let push_thread = thread::spawn(move || pushing.push_branch(&cfg));
    let held = held_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("push connected");

    write(dir.path(), "b.md", b"two");
    let committing = repo.clone();
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let r = committing.commit_paths(&["b.md".to_string()], "second", &sig());
        let _ = done_tx.send(r.is_ok());
    });
    let finished = done_rx.recv_timeout(Duration::from_secs(3));

    drop(held); // lets the stuck push fail and its thread end
    let _ = push_thread.join();
    assert_eq!(
        finished,
        Ok(true),
        "a commit must not wait for a push in flight"
    );
}
