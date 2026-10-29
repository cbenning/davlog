//! Timer-driven push of the configured branch to the backup remote.
//!
//! The remote is a write-only backup. If it has commits we do not have, that is
//! unsupported: the pusher fetches for diagnosis only, pauses, and alerts. It never
//! rebases, merges or force-pushes. Every tick while paused tries again, so pushing
//! resumes by itself once a push is a fast-forward again.

use std::future::Future;
use std::sync::Arc;
use std::time::SystemTime;

use tracing::{debug, info, warn};

use crate::committer::Committer;
use crate::config::PushConfig;
use crate::notify::{AlertSink, Event, EventKind, Severity};
use crate::repo::{ForeignState, PushError, PushResult, Repo};
use crate::status::Status;

/// Consecutive failed ticks before a `PushFailing` alert.
const FAILURES_BEFORE_ALERT: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// A person has git state in `.git`; nothing was done.
    Skipped,
    UpToDate,
    Pushed,
    /// Entered the paused state on this tick.
    Paused,
    StillPaused,
    Failed,
}

pub struct Pusher {
    repo: Repo,
    cfg: PushConfig,
    committer: Committer,
    alerts: Arc<dyn AlertSink>,
    status: Arc<Status>,
    paused: bool,
    consecutive_failures: u32,
}

impl Pusher {
    pub fn new(
        repo: Repo,
        cfg: PushConfig,
        committer: Committer,
        alerts: Arc<dyn AlertSink>,
        status: Arc<Status>,
    ) -> Pusher {
        Pusher {
            repo,
            cfg,
            committer,
            alerts,
            status,
            paused: false,
            consecutive_failures: 0,
        }
    }

    /// Push every `cfg.interval` until `shutdown` resolves.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) {
        tokio::pin!(shutdown);
        let mut interval = tokio::time::interval(self.cfg.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.tick().await;
                }
                _ = &mut shutdown => break,
            }
        }
    }

    /// One push attempt: all the decision logic, without the timer.
    pub async fn tick(&mut self) -> TickOutcome {
        let foreign = self.repo.foreign_state();
        self.status.set_foreign_state(foreign);
        if foreign != ForeignState::None {
            warn!(?foreign, "skipping push: a person has git state in .git");
            return TickOutcome::Skipped;
        }

        if let Err(e) = self.committer.flush().await {
            warn!(error = %e, "could not commit pending changes before pushing");
        }

        let repo = self.repo.clone();
        let cfg = self.cfg.clone();
        let pushed = tokio::task::spawn_blocking(move || repo.push_branch(&cfg))
            .await
            .unwrap_or_else(|e| Err(PushError::Other(format!("push task failed: {e}"))));

        match pushed {
            Ok(result @ (PushResult::UpToDate | PushResult::Pushed)) => {
                self.on_synced();
                if result == PushResult::Pushed {
                    info!(branch = %self.cfg.branch, "pushed");
                    TickOutcome::Pushed
                } else {
                    TickOutcome::UpToDate
                }
            }
            Ok(PushResult::NonFastForward) => self.on_diverged().await,
            Err(e) => self.on_failure(e),
        }
    }

    fn on_synced(&mut self) {
        self.status.set_last_push(SystemTime::now());
        if self.paused {
            self.paused = false;
            self.status.set_paused(None);
            info!("pushing resumed");
            self.alerts.alert(Event::new(
                EventKind::Recovered(Box::new(EventKind::PushPaused)),
                Severity::Recovered,
                "pushing has resumed",
                format!(
                    "The remote branch {}/{} accepts pushes again.",
                    self.cfg.remote, self.cfg.branch
                ),
            ));
        }
        if self.consecutive_failures >= FAILURES_BEFORE_ALERT {
            self.alerts.alert(Event::new(
                EventKind::Recovered(Box::new(EventKind::PushFailing)),
                Severity::Recovered,
                "pushes are working again",
                "The last push succeeded.",
            ));
        }
        self.consecutive_failures = 0;
    }

    async fn on_diverged(&mut self) -> TickOutcome {
        self.consecutive_failures = 0;
        let repo = self.repo.clone();
        let cfg = self.cfg.clone();
        let diagnosis = tokio::task::spawn_blocking(move || {
            repo.fetch_branch(&cfg)
                .map_err(|e| e.to_string())
                .and_then(|()| repo.divergence(&cfg).map_err(|e| format!("{e:#}")))
        })
        .await
        .unwrap_or_else(|e| Err(format!("diagnosis task failed: {e}")));

        let details = divergence_details(&self.cfg, self.repo.workdir(), diagnosis);
        let was_paused = self.paused;
        self.paused = true;
        self.status
            .set_paused(Some("the remote branch has diverged".to_string()));
        if !was_paused {
            warn!("push paused: the remote branch has diverged");
        } else {
            debug!("push still paused");
        }
        self.alerts.alert(Event::new(
            EventKind::PushPaused,
            Severity::Error,
            "push paused: the remote branch has diverged",
            details,
        ));
        if was_paused {
            TickOutcome::StillPaused
        } else {
            TickOutcome::Paused
        }
    }

    fn on_failure(&mut self, e: PushError) -> TickOutcome {
        self.consecutive_failures += 1;
        warn!(error = %e, failures = self.consecutive_failures, "push failed");
        if self.consecutive_failures >= FAILURES_BEFORE_ALERT {
            self.alerts.alert(Event::new(
                EventKind::PushFailing,
                Severity::Error,
                format!(
                    "push has failed {} times in a row",
                    self.consecutive_failures
                ),
                format!("{e}\n\n{}", failure_hint(&e)),
            ));
        }
        TickOutcome::Failed
    }
}

fn failure_hint(e: &PushError) -> &'static str {
    match e {
        PushError::Auth(_) => "Check the SSH key or token configured under [push].",
        PushError::HostKey(_) => "Check the known_hosts file configured under [push].",
        PushError::Network(_) => "Check that this server can reach the remote.",
        PushError::Other(_) => "Check the server log for details.",
    }
}

fn divergence_details(
    cfg: &PushConfig,
    workdir: &std::path::Path,
    diagnosis: Result<crate::repo::Divergence, String>,
) -> String {
    let dir = workdir.display();
    let (remote, branch) = (&cfg.remote, &cfg.branch);
    let facts = match diagnosis {
        Ok(d) => format!(
            "This server is ahead {}, behind {} relative to {remote}/{branch}.\nRemote tip: {}",
            d.ahead, d.behind, d.remote_tip_summary
        ),
        Err(e) => format!("The remote could not be inspected: {e}"),
    };
    format!(
        "The remote branch {remote}/{branch} has changed independently of this server.\n\
         {facts}\n\n\
         Concurrent writes to the remote are not supported. Pushing is paused; the server \
         keeps serving and committing locally, and it resumes pushing by itself once a \
         push is a fast-forward again.\n\n\
         To keep the remote's changes, merge or rebase by hand:\n  \
         git -C {dir} fetch {remote}\n  \
         git -C {dir} rebase {remote}/{branch}\n\
         To overwrite the remote with this server's history, after checking it:\n  \
         git -C {dir} push --force-with-lease {remote} {branch}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_kind_gets_its_own_hint() {
        assert!(failure_hint(&PushError::Auth("x".into())).contains("SSH key or token"));
        assert!(failure_hint(&PushError::HostKey("x".into())).contains("known_hosts"));
        assert!(failure_hint(&PushError::Network("x".into())).contains("reach the remote"));
        assert!(failure_hint(&PushError::Other("x".into())).contains("log"));
    }
}
