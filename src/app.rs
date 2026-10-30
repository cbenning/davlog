//! Wiring: bootstrap the repository, start the committer, the optional pusher, the
//! git-state monitor and the HTTP server, and shut them down in the right order.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use git2::Signature;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::bootstrap::{self, Action, Choice, ResolveError, Terminal};
use crate::brand;
use crate::committer::Committer;
use crate::config::Config;
use crate::disk::DiskGuard;
use crate::notify::email::EmailNotifier;
use crate::notify::{AlertSink, Dispatcher, Event, EventKind, Notifier, Severity};
use crate::pusher::Pusher;
use crate::repo::{self, ForeignState, MaintenanceLimits, Repo};
use crate::server::Server;
use crate::status::Status;

const MONITOR_INTERVAL: Duration = Duration::from_secs(2);
/// How long a background task gets to stop at shutdown, after the final commit.
const TASK_STOP_GRACE: Duration = Duration::from_secs(5);
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Options {
    /// The operator's answer to a bootstrap question (`--bootstrap` or the env var).
    pub bootstrap: Option<Choice>,
    /// Notifiers added to those built from the config (used by tests).
    pub extra_notifiers: Vec<Arc<dyn Notifier>>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The server needs a bootstrap answer and could not ask for one.
    #[error("{0}")]
    NeedsAnswer(String),
    #[error("{0:#}")]
    Other(anyhow::Error),
}

impl AppError {
    pub fn exit_code(&self) -> i32 {
        match self {
            AppError::NeedsAnswer(_) => bootstrap::EXIT_NEEDS_BOOTSTRAP_ANSWER,
            AppError::Other(_) => 1,
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Other(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppExit {
    Clean,
    /// The operator chose `abort` at the bootstrap question.
    Aborted,
}

/// A running server. Call `shutdown` to stop it cleanly.
pub struct App {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    server: JoinHandle<Result<()>>,
    tasks: Vec<(&'static str, JoinHandle<()>)>,
    committer: Committer,
    dispatcher: Arc<Dispatcher>,
}

impl App {
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn committer(&self) -> &Committer {
        &self.committer
    }

    /// Resolves with the name of a core part that has stopped without being asked to. If the
    /// committer or the pusher died quietly, the server would keep accepting uploads that
    /// never get committed, so the caller should exit and let the container restart.
    /// Never resolves once shutdown has begun.
    pub async fn failure(&self) -> String {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if *self.stop.borrow() {
                std::future::pending::<()>().await;
            }
            if !self.committer.is_running() {
                return "committer".to_string();
            }
            if self.server.is_finished() {
                return "http server".to_string();
            }
            if let Some((name, _)) = self.tasks.iter().find(|(_, t)| t.is_finished()) {
                return (*name).to_string();
            }
        }
    }

    /// Kill a background task, to test that its death is noticed.
    #[doc(hidden)]
    pub fn abort_task_for_test(&self, name: &str) {
        for (task_name, task) in &self.tasks {
            if *task_name == name {
                task.abort();
            }
        }
    }

    /// Stop accepting requests, let in-flight ones finish, make a final commit, and
    /// deliver any queued alerts.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.stop.send(true);
        self.server.await.context("server task")??;
        // Make the final commit before waiting on anything that can be slow: a push stuck on
        // an unresponsive remote must not delay it, or cost us the chance to make it.
        let committed = self.committer.shutdown().await;
        for (_, mut task) in self.tasks {
            if tokio::time::timeout(TASK_STOP_GRACE, &mut task)
                .await
                .is_err()
            {
                warn!("a background task did not stop in time and was abandoned");
                task.abort();
            }
        }
        self.dispatcher.drain().await;
        committed.map_err(|e| anyhow!("final commit failed: {e}"))
    }
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

/// Set everything up and start serving. `Ok(None)` means the operator chose `abort`.
pub async fn start(
    cfg: Config,
    options: Options,
    terminal: Option<&mut Terminal<'_>>,
) -> Result<Option<App>, AppError> {
    repo::isolate_from_user_git_config();

    let situation = bootstrap::inspect(&cfg)?;
    let action =
        bootstrap::resolve(situation, options.bootstrap, terminal).map_err(|e| match e {
            ResolveError::NeedsAnswer(message) => AppError::NeedsAnswer(message),
            ResolveError::Other(e) => AppError::Other(e),
        })?;
    if action == Action::Abort {
        return Ok(None);
    }
    let repo = bootstrap::apply(&cfg, action)?;
    repo.apply_config(&cfg.git)?;

    let mut notifiers = options.extra_notifiers;
    for email in &cfg.notify.email {
        notifiers.push(Arc::new(EmailNotifier::new(email)?));
    }
    let dispatcher = Arc::new(Dispatcher::spawn(notifiers, cfg.notify.rate_limit));
    let alerts: Arc<dyn AlertSink> = dispatcher.clone();
    let status = Arc::new(Status::new());

    if let Some(push) = &cfg.push {
        let head = repo.head_branch()?;
        if head.as_deref() != Some(push.branch.as_str()) {
            let message = format!(
                "HEAD is on {} but [push].branch is {}; refusing to start so commits do not land on the wrong branch",
                head.as_deref().unwrap_or("(detached)"),
                push.branch
            );
            alerts.alert(Event::new(
                EventKind::RepoBadState,
                Severity::Error,
                "repository is on the wrong branch",
                message.clone(),
            ));
            dispatcher.drain().await;
            return Err(AppError::Other(anyhow!(message)));
        }
    }

    commit_dirty_tree(&cfg, &repo, alerts.as_ref()).await;

    let committer = Committer::spawn(
        repo.clone(),
        cfg.commit.clone(),
        status.clone(),
        alerts.clone(),
    );
    let (stop, stop_rx) = watch::channel(false);
    let mut tasks = Vec::new();

    if let Some(push) = cfg.push.clone() {
        let pusher = Pusher::new(
            repo.clone(),
            push,
            committer.clone(),
            alerts.clone(),
            status.clone(),
        );
        tasks.push(("pusher", tokio::spawn(pusher.run(stopped(stop_rx.clone())))));
    }
    if cfg.maintenance.enabled {
        tasks.push((
            "maintenance",
            tokio::spawn(maintain_repository(
                repo.clone(),
                cfg.maintenance.clone(),
                alerts.clone(),
                stop_rx.clone(),
            )),
        ));
    }
    tasks.push((
        "git state monitor",
        tokio::spawn(monitor_git_state(
            repo.clone(),
            status.clone(),
            alerts.clone(),
            stop_rx.clone(),
        )),
    ));

    let server = Server::bind(&cfg, repo, committer.clone(), status.clone()).await?;
    let disk = server.disk();
    tasks.push((
        "disk size refresh",
        tokio::spawn(disk.clone().run_refresh(stopped(stop_rx.clone()))),
    ));
    tasks.push((
        "disk monitor",
        tokio::spawn(monitor_disk(disk, status, alerts.clone(), stop_rx.clone())),
    ));
    let addr = server.local_addr();
    info!(%addr, "{} is serving", brand::NAME);
    let server = tokio::spawn(server.serve(stopped(stop_rx)));

    Ok(Some(App {
        addr,
        stop,
        server,
        tasks,
        committer,
        dispatcher,
    }))
}

/// Start, wait for `shutdown`, then stop cleanly.
pub async fn run(
    cfg: Config,
    options: Options,
    terminal: Option<&mut Terminal<'_>>,
    shutdown: impl Future<Output = ()>,
) -> Result<AppExit, AppError> {
    let Some(app) = start(cfg, options, terminal).await? else {
        return Ok(AppExit::Aborted);
    };
    tokio::select! {
        _ = shutdown => {}
        who = app.failure() => {
            error!(component = %who, "a core component stopped unexpectedly; exiting so it can be restarted");
            let _ = app.shutdown().await;
            return Err(AppError::Other(anyhow!("{who} stopped unexpectedly")));
        }
    }
    info!("shutting down");
    app.shutdown().await?;
    Ok(AppExit::Clean)
}

/// Commit a dirty tree left by a crash, unless a person has git state in `.git`.
async fn commit_dirty_tree(cfg: &Config, repo: &Repo, alerts: &dyn AlertSink) {
    if repo.foreign_state() != ForeignState::None {
        return;
    }
    let dirty = repo.is_dirty().unwrap_or(false);
    if !dirty {
        return;
    }
    let (name, email) = (
        cfg.commit.author_name.clone(),
        cfg.commit.author_email.clone(),
    );
    let message = format!("{}: commit changes found at startup", brand::NAME);
    let repo = repo.clone();
    let result = tokio::task::spawn_blocking(move || {
        let sig = Signature::now(&name, &email)?;
        repo.commit_all(&message, &sig)
    })
    .await;
    match result {
        Ok(Ok(Some(oid))) => info!(%oid, "committed changes found at startup"),
        Ok(Ok(None)) => {}
        Ok(Err(e)) => {
            warn!(error = %format!("{e:#}"), "could not commit changes found at startup");
            alerts.alert(Event::new(
                EventKind::CommitFailed,
                Severity::Error,
                "could not commit changes found at startup",
                format!("{e:#}"),
            ));
        }
        Err(e) => warn!(error = %e, "startup commit task failed"),
    }
}

/// Watch for a person leaving rebase/merge state or an index.lock in `.git`.
async fn monitor_git_state(
    repo: Repo,
    status: Arc<Status>,
    alerts: Arc<dyn AlertSink>,
    stop: watch::Receiver<bool>,
) {
    let mut last = ForeignState::None;
    let mut ticker = tokio::time::interval(MONITOR_INTERVAL);
    let stopped = stopped(stop);
    tokio::pin!(stopped);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = repo.foreign_state();
                status.set_foreign_state(now);
                match (last, now) {
                    (ForeignState::None, ForeignState::None) => {}
                    (ForeignState::None, _) => alerts.alert(Event::new(
                        EventKind::ForeignGitState,
                        Severity::Warning,
                        "a person has rebase or merge state in the repository",
                        "While it lasts, commits and pushes are skipped and changes over WebDAV return 503. It clears when the rebase or merge is finished or aborted. If nothing is running, a crashed git command may have left a stale .git/index.lock or rebase directory; remove it.",
                    )),
                    (_, ForeignState::None) => alerts.alert(Event::new(
                        EventKind::Recovered(Box::new(EventKind::ForeignGitState)),
                        Severity::Recovered,
                        "the repository is free again",
                        "Commits, pushes and writes over WebDAV have resumed.",
                    )),
                    _ => {}
                }
                last = now;
            }
            _ = &mut stopped => break,
        }
    }
}

/// Alert when the disk gets too full for new data, and again when it recovers.
async fn monitor_disk(
    disk: Arc<DiskGuard>,
    status: Arc<Status>,
    alerts: Arc<dyn AlertSink>,
    stop: watch::Receiver<bool>,
) {
    let mut low = false;
    let mut ticker = tokio::time::interval(DISK_CHECK_INTERVAL);
    let stopped = stopped(stop);
    tokio::pin!(stopped);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let problem = disk.check().err();
                status.set_disk_low(problem.is_some());
                match (low, problem) {
                    (false, Some(problem)) => {
                        low = true;
                        warn!(%problem, "writes that add data are being refused");
                        alerts.alert(Event::new(
                            EventKind::DiskLow,
                            Severity::Error,
                            "the disk is too full for new data",
                            format!(
                                "{problem}.\nWrites that add data return 507 until this clears; reads and deletes still work. Free some space or raise the limit."
                            ),
                        ));
                    }
                    (true, None) => {
                        low = false;
                        alerts.alert(Event::new(
                            EventKind::Recovered(Box::new(EventKind::DiskLow)),
                            Severity::Recovered,
                            "there is room for new data again",
                            "Writes have resumed.",
                        ));
                    }
                    _ => {}
                }
            }
            _ = &mut stopped => break,
        }
    }
}

/// Pack loose objects now and then, since libgit2 cannot run `git gc`.
async fn maintain_repository(
    repo: Repo,
    cfg: crate::config::MaintenanceConfig,
    alerts: Arc<dyn AlertSink>,
    stop: watch::Receiver<bool>,
) {
    let limits = MaintenanceLimits {
        loose_objects: cfg.loose_object_limit,
        packs: cfg.pack_limit,
    };
    let stopped = stopped(stop);
    tokio::pin!(stopped);
    // first look soon after start, then every interval
    let mut wait = cfg.interval.min(Duration::from_secs(60));
    let mut failing = false;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = &mut stopped => break,
        }
        wait = cfg.interval;
        let target = repo.clone();
        let outcome = tokio::task::spawn_blocking(move || target.maintain(&limits)).await;
        match outcome {
            Ok(Ok(_)) => {
                if failing {
                    failing = false;
                    alerts.alert(Event::new(
                        EventKind::Recovered(Box::new(EventKind::MaintenanceFailed)),
                        Severity::Recovered,
                        "repository maintenance works again",
                        "The last repack succeeded.",
                    ));
                }
            }
            Ok(Err(e)) => {
                warn!(error = %format!("{e:#}"), "repository maintenance failed");
                failing = true;
                alerts.alert(Event::new(
                    EventKind::MaintenanceFailed,
                    Severity::Warning,
                    "repository maintenance failed",
                    format!(
                        "{e:#}\n\nNothing was deleted: a repack removes old files only after the new pack is verified. Commits and pushes are unaffected."
                    ),
                ));
            }
            Err(e) => warn!(error = %e, "maintenance task failed"),
        }
    }
}
