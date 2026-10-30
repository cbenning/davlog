//! The commit scheduler: one actor that owns the pending change set and decides
//! when to commit it, according to the configured policy. Changes that arrive while
//! a commit runs queue up and go into the next one (group commit).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use git2::Signature;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::change::{Change, ChangeSink, Op};
use crate::config::{CommitConfig, CommitMode};
use crate::notify::{AlertSink, Event, EventKind, Severity};
use crate::repo::{ForeignState, Repo};
use crate::status::Status;

const BODY_LINE_LIMIT: usize = 50;
/// How long to wait before retrying after a failed commit.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);
/// How long to wait before retrying while a person has rebase/merge state in `.git`.
const RETRY_AFTER_FOREIGN: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct CommitError(pub String);

type Waiter = oneshot::Sender<Result<(), CommitError>>;

enum Msg {
    /// The change and when it was recorded (not when the actor got to it).
    Change(Change, Instant),
    Flush(Waiter),
    Shutdown(Waiter),
}

/// Handle to the commit actor. Cheap to clone.
#[derive(Clone)]
pub struct Committer {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Committer {
    pub fn spawn(
        repo: Repo,
        cfg: CommitConfig,
        status: Arc<Status>,
        alerts: Arc<dyn AlertSink>,
    ) -> Committer {
        let (tx, rx) = mpsc::unbounded_channel();
        let now = Instant::now();
        let actor = Actor {
            next_tick: now + cfg.interval,
            repo,
            cfg,
            status,
            alerts,
            rx,
            pending: Vec::new(),
            paths: BTreeSet::new(),
            first_at: None,
            last_at: None,
            blocked_until: None,
            failing: false,
            waiters: Vec::new(),
        };
        tokio::spawn(actor.run());
        Committer { tx }
    }

    /// False once the actor has stopped, whether it was told to or died.
    pub fn is_running(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Report a mutation. Never blocks and never fails the caller.
    pub fn record(&self, change: Change) {
        if self.tx.send(Msg::Change(change, Instant::now())).is_err() {
            error!("committer has stopped; change was not recorded");
        }
    }

    /// Commit everything recorded so far now, and wait for the result.
    pub async fn flush(&self) -> Result<(), CommitError> {
        self.request(Msg::Flush).await
    }

    /// Flush, then stop the actor.
    pub async fn shutdown(&self) -> Result<(), CommitError> {
        self.request(Msg::Shutdown).await
    }

    async fn request(&self, make: fn(Waiter) -> Msg) -> Result<(), CommitError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .map_err(|_| CommitError("committer has stopped".into()))?;
        rx.await
            .map_err(|_| CommitError("committer has stopped".into()))?
    }
}

impl ChangeSink for Committer {
    fn record(&self, change: Change) {
        Committer::record(self, change);
    }
}

struct Actor {
    repo: Repo,
    cfg: CommitConfig,
    status: Arc<Status>,
    alerts: Arc<dyn AlertSink>,
    rx: mpsc::UnboundedReceiver<Msg>,
    pending: Vec<Change>,
    paths: BTreeSet<String>,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
    blocked_until: Option<Instant>,
    next_tick: Instant,
    failing: bool,
    waiters: Vec<Waiter>,
}

enum Wake {
    Msg(Msg),
    Closed,
    Timer,
}

impl Actor {
    async fn run(mut self) {
        let mut shutdown: Option<Waiter> = None;
        let mut closed = false;
        loop {
            let wake = match self.deadline() {
                Some(deadline) => tokio::select! {
                    m = self.rx.recv() => m.map_or(Wake::Closed, Wake::Msg),
                    _ = tokio::time::sleep_until(deadline) => Wake::Timer,
                },
                None => self.rx.recv().await.map_or(Wake::Closed, Wake::Msg),
            };
            match wake {
                Wake::Msg(m) => self.absorb(m, &mut shutdown),
                Wake::Closed => closed = true,
                Wake::Timer => {}
            }
            while let Ok(m) = self.rx.try_recv() {
                self.absorb(m, &mut shutdown);
            }

            let force = !self.waiters.is_empty() || shutdown.is_some() || closed;
            let due = self.deadline().is_some_and(|d| d <= Instant::now());
            if force || due {
                self.commit_now().await;
            }
            if self.cfg.mode == CommitMode::Interval {
                let now = Instant::now();
                while self.next_tick <= now {
                    self.next_tick += self.cfg.interval;
                }
            }

            if let Some(tx) = shutdown.take() {
                let _ = tx.send(self.final_result());
                break;
            }
            if closed {
                break;
            }
        }
    }

    fn final_result(&self) -> Result<(), CommitError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(CommitError(
                "changes could not be committed at shutdown".into(),
            ))
        }
    }

    fn absorb(&mut self, msg: Msg, shutdown: &mut Option<Waiter>) {
        match msg {
            Msg::Change(change, at) => {
                self.first_at.get_or_insert(at);
                self.last_at = Some(at);
                for path in change.staged_paths() {
                    self.paths.insert(path.to_string());
                }
                self.pending.push(change);
            }
            Msg::Flush(tx) => self.waiters.push(tx),
            Msg::Shutdown(tx) => *shutdown = Some(tx),
        }
    }

    /// When the next non-forced commit is due. `None` means nothing to wait for.
    fn deadline(&self) -> Option<Instant> {
        let policy = match self.cfg.mode {
            CommitMode::Interval => Some(self.next_tick),
            _ if self.pending.is_empty() => None,
            CommitMode::PerRequest => Some(Instant::now()),
            CommitMode::Debounce => {
                let quiet = self.last_at? + self.cfg.debounce;
                let cap = self.first_at? + self.cfg.max_wait;
                Some(quiet.min(cap))
            }
        };
        match (policy, self.blocked_until) {
            (Some(p), Some(b)) => Some(p.max(b)),
            (p, _) => p,
        }
    }

    fn reply_waiters(&mut self, result: Result<(), CommitError>) {
        for tx in self.waiters.drain(..) {
            let _ = tx.send(result.clone());
        }
    }

    async fn commit_now(&mut self) {
        if self.pending.is_empty() {
            self.reply_waiters(Ok(()));
            return;
        }

        let foreign = self.repo.foreign_state();
        self.status.set_foreign_state(foreign);
        if foreign != ForeignState::None {
            warn!(?foreign, "not committing: a person has git state in .git");
            self.blocked_until = Some(Instant::now() + RETRY_AFTER_FOREIGN);
            self.reply_waiters(Err(CommitError(
                "a rebase, merge or index.lock in .git blocks commits; they resume when it clears"
                    .into(),
            )));
            return;
        }

        let message = render_message(&self.cfg.message_template, &self.pending);
        let paths: Vec<String> = self.paths.iter().cloned().collect();
        let repo = self.repo.clone();
        let (name, email) = (self.cfg.author_name.clone(), self.cfg.author_email.clone());
        let outcome = tokio::task::spawn_blocking(move || {
            let sig = Signature::now(&name, &email)?;
            repo.commit_paths(&paths, &message, &sig)
        })
        .await;

        let result = match outcome {
            Ok(Ok(oid)) => Ok(oid),
            Ok(Err(e)) => Err(format!("{e:#}")),
            Err(e) => Err(format!("commit task failed: {e}")),
        };
        match result {
            Ok(oid) => {
                if let Some(oid) = oid {
                    info!(%oid, changes = self.pending.len(), "committed");
                    self.status.set_last_commit(SystemTime::now());
                }
                self.pending.clear();
                self.paths.clear();
                self.first_at = None;
                self.last_at = None;
                self.blocked_until = None;
                if self.failing {
                    self.failing = false;
                    self.alerts.alert(Event::new(
                        EventKind::Recovered(Box::new(EventKind::CommitFailed)),
                        Severity::Recovered,
                        "commits are working again",
                        "The pending changes were committed.",
                    ));
                }
                self.reply_waiters(Ok(()));
            }
            Err(message) => {
                error!(error = %message, "commit failed");
                self.blocked_until = Some(Instant::now() + RETRY_AFTER_FAILURE);
                if !self.failing {
                    self.failing = true;
                    self.alerts.alert(Event::new(
                        EventKind::CommitFailed,
                        Severity::Error,
                        "commit failed",
                        format!(
                            "Changes are on disk but not committed yet; the server retries every {} s.\n{message}",
                            RETRY_AFTER_FAILURE.as_secs()
                        ),
                    ));
                }
                self.reply_waiters(Err(CommitError(message)));
            }
        }
    }
}

fn describe(change: &Change) -> String {
    match (&change.to, change.op) {
        (Some(to), op @ (Op::Move | Op::Copy)) => {
            format!("{} {} -> {}", op.word(), change.path, to)
        }
        _ => format!("{} {}", change.op.word(), change.path),
    }
}

/// Commit message: the template with `{summary}` filled in, then a body listing the
/// changes when there is more than one (truncated after 50 lines).
pub fn render_message(template: &str, changes: &[Change]) -> String {
    let summary = match changes {
        [one] => match one.op {
            Op::Put => format!("update {}", one.path),
            Op::Mkcol => format!("mkdir {}", one.path),
            _ => describe(one),
        },
        many => format!("{} changes", many.len()),
    };
    let mut message = template.replace("{summary}", &summary);
    if changes.len() > 1 {
        message.push('\n');
        for change in changes.iter().take(BODY_LINE_LIMIT) {
            message.push('\n');
            message.push_str(&describe(change));
        }
        if changes.len() > BODY_LINE_LIMIT {
            message.push_str(&format!(
                "\n... and {} more",
                changes.len() - BODY_LINE_LIMIT
            ));
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{Change, Op};

    const T: &str = "davlog: {summary}";

    #[test]
    fn render_message_single_and_multi() {
        let single = render_message(T, &[Change::new(Op::Put, "notes/a.md")]);
        assert_eq!(single, "davlog: update notes/a.md");

        assert_eq!(
            render_message(T, &[Change::new(Op::Delete, "a")]),
            "davlog: delete a"
        );
        assert_eq!(
            render_message(T, &[Change::new(Op::Mkcol, "d")]),
            "davlog: mkdir d"
        );
        assert_eq!(
            render_message(T, &[Change::with_target(Op::Move, "a", "b")]),
            "davlog: move a -> b"
        );
        assert_eq!(
            render_message(T, &[Change::with_target(Op::Copy, "a", "b")]),
            "davlog: copy a -> b"
        );

        let multi = render_message(
            T,
            &[
                Change::new(Op::Put, "a"),
                Change::new(Op::Delete, "b"),
                Change::with_target(Op::Move, "c", "d"),
            ],
        );
        assert_eq!(multi, "davlog: 3 changes\n\nput a\ndelete b\nmove c -> d");
    }

    #[test]
    fn render_message_truncates_body_after_50_lines() {
        let changes: Vec<Change> = (0..60)
            .map(|i| Change::new(Op::Put, format!("f{i}")))
            .collect();
        let msg = render_message(T, &changes);
        let lines: Vec<&str> = msg.lines().collect();
        assert_eq!(lines[0], "davlog: 60 changes");
        assert_eq!(lines[1], "");
        assert_eq!(lines.len(), 2 + 50 + 1);
        assert_eq!(lines[2], "put f0");
        assert_eq!(*lines.last().unwrap(), "... and 10 more");
    }

    #[test]
    fn template_without_summary_variable_is_used_verbatim() {
        assert_eq!(render_message("sync", &[Change::new(Op::Put, "a")]), "sync");
    }
}
