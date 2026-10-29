//! Alert events and the sink that carries them out of the server.
//! (Notifiers, the rate limiter and the dispatcher are added with the email channel.)

use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    PushPaused,
    PushFailing,
    CommitFailed,
    RepoBadState,
    ForeignGitState,
    Recovered(Box<EventKind>),
}

impl EventKind {
    pub fn key(&self) -> &'static str {
        match self {
            EventKind::PushPaused => "push_paused",
            EventKind::PushFailing => "push_failing",
            EventKind::CommitFailed => "commit_failed",
            EventKind::RepoBadState => "repo_bad_state",
            EventKind::ForeignGitState => "foreign_git_state",
            EventKind::Recovered(_) => "recovered",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub severity: Severity,
    pub summary: String,
    pub details: String,
    pub at: SystemTime,
}

impl Event {
    pub fn new(
        kind: EventKind,
        severity: Severity,
        summary: impl Into<String>,
        details: impl Into<String>,
    ) -> Self {
        Event {
            kind,
            severity,
            summary: summary.into(),
            details: details.into(),
            at: SystemTime::now(),
        }
    }
}

/// Where the server sends alerts. Must never block the caller.
pub trait AlertSink: Send + Sync {
    fn alert(&self, event: Event);
}
