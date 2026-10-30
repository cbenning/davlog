//! Shared, cheap-to-read server status for `/healthz`.

use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::repo::ForeignState;

#[derive(Default)]
struct Inner {
    last_commit: Option<SystemTime>,
    last_push: Option<SystemTime>,
    paused: Option<String>,
    foreign: Option<ForeignState>,
    disk_low: bool,
    commit_failing: bool,
    push_failing: bool,
}

#[derive(Default)]
pub struct Status {
    inner: RwLock<Inner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusSnapshot {
    pub last_commit_unix: Option<u64>,
    pub last_push_unix: Option<u64>,
    /// Why pushing is paused, if it is.
    pub push_paused: Option<String>,
    /// True while a person has rebase/merge state or an index lock in `.git`.
    pub foreign_git_state: bool,
    /// True while writes that add data are refused for lack of space.
    pub disk_low: bool,
    /// The last commit attempt failed and is being retried.
    pub commit_failing: bool,
    /// Pushes have failed several times in a row.
    pub push_failing: bool,
}

impl StatusSnapshot {
    /// What needs attention, in plain words. Empty means everything is fine.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(why) = &self.push_paused {
            problems.push(format!("pushing is paused: {why}"));
        }
        if self.foreign_git_state {
            problems.push(
                "a person has a rebase or merge (or a stale lock) in the repository, so commits and writes are on hold"
                    .to_string(),
            );
        }
        if self.disk_low {
            problems.push("the disk is too full for new data".to_string());
        }
        if self.commit_failing {
            problems.push("commits are failing and being retried".to_string());
        }
        if self.push_failing {
            problems.push("pushes are failing".to_string());
        }
        problems
    }
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Status {
    pub fn new() -> Self {
        Self::default()
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set_last_commit(&self, at: SystemTime) {
        self.write().last_commit = Some(at);
    }

    pub fn set_last_push(&self, at: SystemTime) {
        self.write().last_push = Some(at);
    }

    pub fn set_paused(&self, reason: Option<String>) {
        self.write().paused = reason;
    }

    pub fn set_foreign_state(&self, state: ForeignState) {
        self.write().foreign = Some(state);
    }

    pub fn set_commit_failing(&self, failing: bool) {
        self.write().commit_failing = failing;
    }

    pub fn set_push_failing(&self, failing: bool) {
        self.write().push_failing = failing;
    }

    pub fn set_disk_low(&self, low: bool) {
        self.write().disk_low = low;
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let inner = self.inner.read().unwrap_or_else(|p| p.into_inner());
        StatusSnapshot {
            last_commit_unix: inner.last_commit.map(unix),
            last_push_unix: inner.last_push.map(unix),
            push_paused: inner.paused.clone(),
            foreign_git_state: matches!(inner.foreign, Some(s) if s != ForeignState::None),
            disk_low: inner.disk_low,
            commit_failing: inner.commit_failing,
            push_failing: inner.push_failing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_status_has_no_problems() {
        assert!(Status::new().snapshot().problems().is_empty());
    }

    #[test]
    fn each_problem_is_named_in_plain_words() {
        let s = Status::new();
        s.set_paused(Some("the remote branch has diverged".into()));
        s.set_foreign_state(ForeignState::RebaseOrMerge);
        s.set_disk_low(true);
        s.set_commit_failing(true);
        s.set_push_failing(true);
        let problems = s.snapshot().problems();
        assert_eq!(problems.len(), 5, "{problems:?}");
        let all = problems.join(" | ");
        for needle in [
            "pushing is paused",
            "diverged",
            "rebase or merge",
            "disk",
            "commits are failing",
            "pushes are failing",
        ] {
            assert!(all.contains(needle), "{needle}: {all}");
        }
        s.set_paused(None);
        s.set_foreign_state(ForeignState::None);
        s.set_disk_low(false);
        s.set_commit_failing(false);
        s.set_push_failing(false);
        assert!(s.snapshot().problems().is_empty());
    }

    #[test]
    fn snapshot_reflects_setters() {
        let s = Status::new();
        assert_eq!(
            s.snapshot(),
            StatusSnapshot {
                last_commit_unix: None,
                last_push_unix: None,
                push_paused: None,
                foreign_git_state: false,
                disk_low: false,
                commit_failing: false,
                push_failing: false
            }
        );
        s.set_paused(Some("diverged".into()));
        s.set_foreign_state(ForeignState::IndexLock);
        s.set_last_commit(UNIX_EPOCH + std::time::Duration::from_secs(42));
        let snap = s.snapshot();
        assert_eq!(snap.push_paused.as_deref(), Some("diverged"));
        assert!(snap.foreign_git_state);
        assert_eq!(snap.last_commit_unix, Some(42));
        s.set_foreign_state(ForeignState::None);
        assert!(!s.snapshot().foreign_git_state);
    }
}
