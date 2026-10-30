//! Alert events and the sink that carries them out of the server.
//! (Notifiers, the rate limiter and the dispatcher are added with the email channel.)

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, warn};

pub mod email;

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
    DiskLow,
    MaintenanceFailed,
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
            EventKind::DiskLow => "disk_low",
            EventKind::MaintenanceFailed => "maintenance_failed",
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

/// Longest a single notifier gets to deliver one event.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// A channel that can deliver an alert (email, webhook, ...).
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;

    fn send<'a>(
        &'a self,
        event: &'a Event,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

/// A problem that comes back within this long of its recovery is held back instead of being
/// reported again straight away, so a condition that flaps around a threshold sends at most
/// one alert and one recovery per hour instead of an email every few seconds.
pub const FLAP_GUARD: Duration = Duration::from_secs(3600);

struct KeyState {
    last_sent: Instant,
    /// True from an alert until its recovery.
    active: bool,
}

/// Lets the first event for a key through at once, then at most one reminder per period
/// while the problem lasts. A recovery is reported only for an alert that went out.
pub struct RateLimiter {
    period: Duration,
    guard: Duration,
    keys: HashMap<String, KeyState>,
}

impl RateLimiter {
    pub fn new(period: Duration) -> Self {
        RateLimiter {
            period,
            guard: period.min(FLAP_GUARD),
            keys: HashMap::new(),
        }
    }

    pub fn allow(&mut self, key: &str, now: Instant) -> bool {
        if let Some(state) = self.keys.get(key) {
            let wait = if state.active {
                self.period
            } else {
                self.guard
            };
            if now.duration_since(state.last_sent) < wait {
                return false;
            }
        }
        self.keys.insert(
            key.to_string(),
            KeyState {
                last_sent: now,
                active: true,
            },
        );
        true
    }

    /// True if a recovery for `key` should be sent now.
    pub fn recovered(&mut self, key: &str, _now: Instant) -> bool {
        match self.keys.get_mut(key) {
            Some(state) if state.active => {
                state.active = false;
                true
            }
            _ => false,
        }
    }
}

/// Fans events out to every notifier without ever blocking the caller. Each notifier
/// has its own queue and worker, so a slow or hanging one delays only itself.
pub struct Dispatcher {
    limiter: std::sync::Mutex<RateLimiter>,
    workers: Vec<mpsc::UnboundedSender<WorkerMsg>>,
}

enum WorkerMsg {
    Event(Event),
    /// Answered once everything queued before it has been handled.
    Sync(tokio::sync::oneshot::Sender<()>),
}

impl Dispatcher {
    pub fn spawn(notifiers: Vec<Arc<dyn Notifier>>, rate_limit: Duration) -> Dispatcher {
        let workers = notifiers
            .into_iter()
            .map(|notifier| {
                let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMsg>();
                tokio::spawn(async move {
                    while let Some(msg) = rx.recv().await {
                        let event = match msg {
                            WorkerMsg::Event(event) => event,
                            WorkerMsg::Sync(done) => {
                                let _ = done.send(());
                                continue;
                            }
                        };
                        match tokio::time::timeout(SEND_TIMEOUT, notifier.send(&event)).await {
                            Ok(Ok(())) => debug!(notifier = notifier.name(), "alert delivered"),
                            Ok(Err(e)) => {
                                warn!(notifier = notifier.name(), error = %e, "alert delivery failed")
                            }
                            Err(_) => warn!(notifier = notifier.name(), "alert delivery timed out"),
                        }
                    }
                });
                tx
            })
            .collect();
        Dispatcher {
            limiter: std::sync::Mutex::new(RateLimiter::new(rate_limit)),
            workers,
        }
    }
}

impl AlertSink for Dispatcher {
    fn alert(&self, event: Event) {
        let pass = {
            let mut limiter = self.limiter.lock().unwrap_or_else(|p| p.into_inner());
            match &event.kind {
                EventKind::Recovered(inner) => limiter.recovered(inner.key(), Instant::now()),
                kind => limiter.allow(kind.key(), Instant::now()),
            }
        };
        if !pass {
            debug!(kind = event.kind.key(), "alert suppressed by rate limit");
            return;
        }
        for worker in &self.workers {
            let _ = worker.send(WorkerMsg::Event(event.clone()));
        }
    }
}

impl Dispatcher {
    /// Wait until every event queued so far has been delivered, failed or timed out.
    /// Call before exiting so a last alert is not lost.
    pub async fn drain(&self) {
        let waits = self.workers.iter().filter_map(|worker| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            worker.send(WorkerMsg::Sync(tx)).ok().map(|()| rx)
        });
        for rx in waits.collect::<Vec<_>>() {
            let _ = tokio::time::timeout(SEND_TIMEOUT + Duration::from_secs(5), rx).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Mock {
        name: &'static str,
        behaviour: Behaviour,
        events: Mutex<Vec<Event>>,
    }

    enum Behaviour {
        Ok,
        Fail,
        Hang,
    }

    impl Mock {
        fn new(name: &'static str, behaviour: Behaviour) -> Arc<Self> {
            Arc::new(Mock {
                name,
                behaviour,
                events: Mutex::new(Vec::new()),
            })
        }

        fn count(&self) -> usize {
            self.events.lock().unwrap().len()
        }
    }

    impl Notifier for Mock {
        fn name(&self) -> &str {
            self.name
        }

        fn send<'a>(
            &'a self,
            event: &'a Event,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
            Box::pin(async move {
                match self.behaviour {
                    Behaviour::Ok => {
                        self.events.lock().unwrap().push(event.clone());
                        Ok(())
                    }
                    Behaviour::Fail => anyhow::bail!("boom"),
                    Behaviour::Hang => std::future::pending().await,
                }
            })
        }
    }

    fn event(kind: EventKind) -> Event {
        Event::new(kind, Severity::Error, "summary", "details")
    }

    async fn wait_for(mut cond: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if cond() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition not met");
    }

    #[test]
    fn rate_limiter_first_passes_then_blocks_until_period() {
        let mut rl = RateLimiter::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(rl.allow("k", t0));
        assert!(!rl.allow("k", t0 + Duration::from_secs(59)));
        assert!(rl.allow("k", t0 + Duration::from_secs(60)));
        assert!(!rl.allow("k", t0 + Duration::from_secs(61)));
    }

    #[test]
    fn rate_limiter_is_per_key() {
        let mut rl = RateLimiter::new(Duration::from_secs(24 * 3600));
        let t0 = Instant::now();
        assert!(rl.allow("a", t0));
        assert!(rl.allow("b", t0));
        assert!(!rl.allow("a", t0));
    }

    #[test]
    fn recovery_is_only_reported_for_an_alert_that_was_sent() {
        let mut rl = RateLimiter::new(Duration::from_secs(3600));
        let t0 = Instant::now();
        assert!(!rl.recovered("k", t0), "nothing was alerted");
        assert!(rl.allow("k", t0));
        assert!(rl.recovered("k", t0 + Duration::from_secs(5)));
        assert!(
            !rl.recovered("k", t0 + Duration::from_secs(6)),
            "already reported"
        );
    }

    #[test]
    fn a_new_problem_right_after_recovery_waits_out_the_flap_guard() {
        let mut rl = RateLimiter::new(Duration::from_secs(24 * 3600));
        let t0 = Instant::now();
        assert!(rl.allow("k", t0));
        assert!(rl.recovered("k", t0 + Duration::from_secs(30)));
        assert!(
            !rl.allow("k", t0 + Duration::from_secs(60)),
            "flapping: held back"
        );
        assert!(
            rl.allow("k", t0 + FLAP_GUARD + Duration::from_secs(1)),
            "a later problem is reported"
        );
    }

    #[test]
    fn the_guard_never_exceeds_the_configured_period() {
        let mut rl = RateLimiter::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(rl.allow("k", t0));
        assert!(rl.recovered("k", t0 + Duration::from_secs(1)));
        assert!(rl.allow("k", t0 + Duration::from_secs(61)));
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_follows_an_alert_and_a_later_problem_is_reported_again() {
        let mock = Mock::new("m", Behaviour::Ok);
        let d = Dispatcher::spawn(vec![mock.clone()], Duration::from_secs(24 * 3600));
        d.alert(event(EventKind::PushPaused));
        wait_for(|| mock.count() == 1).await;
        d.alert(event(EventKind::PushPaused)); // suppressed: same problem, inside the period
        d.alert(event(EventKind::Recovered(Box::new(EventKind::PushPaused))));
        wait_for(|| mock.count() == 2).await;
        tokio::time::advance(FLAP_GUARD + Duration::from_secs(1)).await;
        d.alert(event(EventKind::PushPaused)); // a real new problem, long after
        wait_for(|| mock.count() == 3).await;
        let kinds: Vec<_> = mock
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.kind.clone())
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::PushPaused,
                EventKind::Recovered(Box::new(EventKind::PushPaused)),
                EventKind::PushPaused
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_flapping_problem_does_not_flood_the_inbox() {
        let mock = Mock::new("m", Behaviour::Ok);
        let d = Dispatcher::spawn(vec![mock.clone()], Duration::from_secs(24 * 3600));
        for _ in 0..20 {
            d.alert(event(EventKind::DiskLow));
            d.alert(event(EventKind::Recovered(Box::new(EventKind::DiskLow))));
            tokio::time::advance(Duration::from_secs(30)).await;
        }
        d.drain().await;
        assert_eq!(
            mock.count(),
            2,
            "one alert and one recovery, not forty emails"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_recovery_for_an_alert_that_never_went_out_is_not_sent() {
        let mock = Mock::new("m", Behaviour::Ok);
        let d = Dispatcher::spawn(vec![mock.clone()], Duration::from_secs(3600));
        d.alert(event(EventKind::Recovered(Box::new(
            EventKind::CommitFailed,
        ))));
        d.drain().await;
        assert_eq!(mock.count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn reminder_passes_after_the_rate_limit_period() {
        let mock = Mock::new("m", Behaviour::Ok);
        let d = Dispatcher::spawn(vec![mock.clone()], Duration::from_secs(3600));
        d.alert(event(EventKind::PushPaused));
        wait_for(|| mock.count() == 1).await;
        tokio::time::advance(Duration::from_secs(3601)).await;
        d.alert(event(EventKind::PushPaused));
        wait_for(|| mock.count() == 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn dispatcher_never_blocks_caller_and_sends_to_all_notifiers() {
        let slow = Mock::new("slow", Behaviour::Hang);
        let fast = Mock::new("fast", Behaviour::Ok);
        let d = Dispatcher::spawn(vec![slow.clone(), fast.clone()], Duration::from_secs(1));
        // alert() is synchronous: it returns at once even though one notifier hangs
        d.alert(event(EventKind::CommitFailed));
        wait_for(|| fast.count() == 1).await;
    }

    #[tokio::test(start_paused = true)]
    async fn dispatcher_survives_failing_notifier() {
        let bad = Mock::new("bad", Behaviour::Fail);
        let good = Mock::new("good", Behaviour::Ok);
        let d = Dispatcher::spawn(vec![bad, good.clone()], Duration::from_secs(1));
        d.alert(event(EventKind::CommitFailed));
        d.alert(event(EventKind::RepoBadState));
        wait_for(|| good.count() == 2).await;
    }

    #[tokio::test(start_paused = true)]
    async fn drain_waits_until_queued_events_were_delivered() {
        let mock = Mock::new("m", Behaviour::Ok);
        let d = Dispatcher::spawn(vec![mock.clone()], Duration::from_secs(1));
        d.alert(event(EventKind::CommitFailed));
        d.alert(event(EventKind::RepoBadState));
        d.drain().await;
        assert_eq!(
            mock.count(),
            2,
            "everything queued before drain() was delivered"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dispatcher_send_times_out() {
        let hang = Mock::new("hang", Behaviour::Hang);
        let good = Mock::new("good", Behaviour::Ok);
        let d = Dispatcher::spawn(vec![hang, good.clone()], Duration::from_secs(1));
        d.alert(event(EventKind::CommitFailed));
        wait_for(|| good.count() == 1).await;
        // the hanging send is abandoned after the timeout, so later events still flow
        tokio::time::advance(SEND_TIMEOUT + Duration::from_secs(1)).await;
        d.alert(event(EventKind::RepoBadState));
        wait_for(|| good.count() == 2).await;
    }
}
