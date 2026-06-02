//! Kairos — the opt-in, proactive/autonomous agent layer.
//!
//! This module owns the *clock-free decision core* of the autonomous loop plus
//! the thin real-time wrapper that drives it in the live REPL. The decision core
//! is tested deterministically (E2E_TEST_PLAN B2/B3, "virtual clock only — no
//! real `sleep`"); the wrapper is exercised end-to-end through a pty. The
//! synchronous REPL's blocking line reader is **not** rewritten into an async
//! event pump — idle detection runs on a separate watcher thread instead. What
//! lives here:
//!
//! - [`kairos_enabled`]: the single feature gate. Off by default, so every
//!   Kairos behaviour is dormant unless the operator opts in. With the gate off
//!   no watcher, clock, or printer is created and the REPL path is byte-identical
//!   to baseline.
//! - [`startup_dream_action`]: the B3 dream-on-start decision, folded with the
//!   gate, returning a *pure intent* ([`StartupAction`]) so the startup wiring
//!   can be tested without spawning a thread or calling a model.
//! - [`IdleBriefPolicy`]: the B2 idle-loop decision core. Given a monotonically
//!   increasing virtual tick, it decides when exactly one proactive `Brief`
//!   should be emitted, and re-arms on user activity.
//! - [`KairosIdleRuntime`] + [`run_idle_watch`]: the real-time adapter and
//!   watcher loop that drive the policy from a wall clock and emit through a
//!   caller-supplied sink (the REPL's prompt-preserving external printer).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::dream::{dream_on_start_decision, DreamOnStartDecision};
use crate::journal::JournalDate;

/// Environment variable that opts the agent into the Kairos autonomous layer.
pub const KAIROS_ENV: &str = "CLAW_KAIROS";

/// Environment variable overriding the idle threshold (in whole seconds) after
/// which the autonomous loop emits one proactive Brief. Unset/invalid → default.
pub const KAIROS_IDLE_SECS_ENV: &str = "CLAW_KAIROS_IDLE_SECS";

/// Default idle threshold: five minutes, matching the prompt-cache window the
/// Kairos design paces against (so a proactive Brief lands while the cache is
/// still warm rather than after an arbitrary gap).
pub const DEFAULT_IDLE_SECS: u64 = 300;

/// The proactive Brief the live idle loop surfaces. Deterministic and uniquely
/// greppable so the autonomous-loop E2E can assert exactly-one emission without
/// depending on model output. Richer, model-generated proactive content is a
/// later (SleepTool/cache-paced) layer.
pub const IDLE_BRIEF_NOTICE: &str =
    "[kairos] proactive check-in: you've been idle — reply to keep going.";

/// Whether the Kairos autonomous layer is enabled. Off by default; enabled when
/// `CLAW_KAIROS` is set to a truthy value (`1`/`true`/`yes`/`on`, case-folded).
/// Kept deliberately narrow so the gate can later move to settings without
/// touching call sites.
#[must_use]
pub fn kairos_enabled() -> bool {
    std::env::var(KAIROS_ENV)
        .ok()
        .is_some_and(|value| env_is_truthy(&value))
}

fn env_is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// What the startup hook should do for the dream-on-start scheduler (B3). A pure
/// intent so the wiring is testable without threads or a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    /// Do nothing — the gate is off, or a dream already covered today.
    Noop,
    /// Kick off one best-effort background dream for `today`.
    SpawnDream,
}

/// Fold the feature gate with the pure [`dream_on_start_decision`] (B3). Returns
/// [`StartupAction::Noop`] whenever the gate is off, so the disabled path does
/// no marker read, no spawn, and no output. `enabled` is passed in rather than
/// read from the environment here so this stays a pure, deterministic unit.
#[must_use]
pub fn startup_dream_action(
    enabled: bool,
    last_dream: Option<JournalDate>,
    today: JournalDate,
) -> StartupAction {
    if !enabled {
        return StartupAction::Noop;
    }
    match dream_on_start_decision(last_dream, today) {
        DreamOnStartDecision::Run => StartupAction::SpawnDream,
        DreamOnStartDecision::SkipUpToDate => StartupAction::Noop,
    }
}

/// A signal the autonomous loop wants surfaced to the user. In this slice the
/// only signal is a single proactive `Brief` after an idle stretch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomousSignal {
    /// Emit one proactive `Brief` (vs a normal, user-solicited one).
    ProactiveBrief,
}

/// The B2 autonomous idle-loop core, driven by a virtual monotonic tick (no
/// real time, no `sleep`). It fires exactly **one** proactive `Brief` once the
/// idle stretch since the last user activity reaches `idle_threshold`, and
/// re-arms only when fresh user activity arrives. Ticks observed while already
/// fired (and before any new activity) do not re-emit.
///
/// "Externally visible behaviour" (the test contract) is the sequence of
/// [`AutonomousSignal`]s returned from [`IdleBriefPolicy::on_tick`]; callers
/// never inspect internal tick counts.
#[derive(Debug, Clone)]
pub struct IdleBriefPolicy {
    idle_threshold: u64,
    last_activity_tick: u64,
    fired_since_activity: bool,
}

impl IdleBriefPolicy {
    /// Create a policy that emits one proactive `Brief` after `idle_threshold`
    /// idle ticks. A threshold of `0` is clamped to `1` so a brand-new policy
    /// cannot emit before any tick has elapsed.
    #[must_use]
    pub fn new(idle_threshold: u64) -> Self {
        Self {
            idle_threshold: idle_threshold.max(1),
            last_activity_tick: 0,
            fired_since_activity: false,
        }
    }

    /// Record user activity at virtual tick `now`. This resets the idle stretch
    /// and re-arms the policy so a future idle period can emit again.
    pub fn on_user_activity(&mut self, now: u64) {
        self.last_activity_tick = now;
        self.fired_since_activity = false;
    }

    /// Advance the virtual clock to `now`. Returns [`AutonomousSignal`] exactly
    /// once per idle stretch — when the gap since the last activity first
    /// reaches the threshold — and `None` otherwise.
    pub fn on_tick(&mut self, now: u64) -> Option<AutonomousSignal> {
        if self.fired_since_activity {
            return None;
        }
        let idle_for = now.saturating_sub(self.last_activity_tick);
        if idle_for >= self.idle_threshold {
            self.fired_since_activity = true;
            return Some(AutonomousSignal::ProactiveBrief);
        }
        None
    }
}

/// Resolve the idle threshold (whole seconds) from [`KAIROS_IDLE_SECS_ENV`],
/// falling back to [`DEFAULT_IDLE_SECS`] when unset, unparseable, or zero.
#[must_use]
pub fn idle_threshold_secs() -> u64 {
    std::env::var(KAIROS_IDLE_SECS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(DEFAULT_IDLE_SECS)
}

/// Real-time wrapper that drives [`IdleBriefPolicy`] from a wall clock for the
/// live REPL (E2E_TEST_PLAN B2 wiring). The policy stays the single decision
/// core; this only supplies a monotonic millisecond tick and the concurrency
/// glue: the REPL main thread calls [`record_activity`](Self::record_activity)
/// on every real submit, while a background watcher calls
/// [`poll`](Self::poll) on an interval. Both go through one mutex, so there is
/// no torn read between activity reset and idle evaluation. A `stop` flag makes
/// shutdown emit-safe: once set, `poll` never returns a signal, so a watcher
/// racing teardown cannot print after the REPL has decided to exit.
pub struct KairosIdleRuntime {
    policy: Mutex<IdleBriefPolicy>,
    stop: AtomicBool,
    start: Instant,
}

impl KairosIdleRuntime {
    /// Build from a millisecond idle threshold (clamped to >= 1ms).
    #[must_use]
    pub fn new(idle_threshold_ms: u64) -> Self {
        Self {
            policy: Mutex::new(IdleBriefPolicy::new(idle_threshold_ms.max(1))),
            stop: AtomicBool::new(false),
            start: Instant::now(),
        }
    }

    /// Build from a whole-second idle threshold (the env/config unit).
    #[must_use]
    pub fn from_idle_secs(idle_secs: u64) -> Self {
        Self::new(idle_secs.saturating_mul(1000))
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Record a real user submit: resets the idle stretch and re-arms emission.
    pub fn record_activity(&self) {
        if let Ok(mut policy) = self.policy.lock() {
            policy.on_user_activity(self.now_ms());
        }
    }

    /// Evaluate idle state now. Returns `Some` at most once per idle stretch.
    /// Returns `None` whenever a stop has been requested — checked both before
    /// and after the policy call so a signal produced as teardown begins is
    /// dropped rather than printed.
    pub fn poll(&self) -> Option<AutonomousSignal> {
        if self.stop.load(Ordering::Acquire) {
            return None;
        }
        let signal = self
            .policy
            .lock()
            .ok()
            .and_then(|mut policy| policy.on_tick(self.now_ms()));
        if self.stop.load(Ordering::Acquire) {
            return None;
        }
        signal
    }

    /// Signal the watcher to stop emitting and exit.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Whether a stop has been requested.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// Drive the idle watcher loop until [`KairosIdleRuntime::request_stop`] is
/// called: poll on `poll_interval`, and on each idle signal hand
/// [`IDLE_BRIEF_NOTICE`] to `emit`. This is the body the REPL's background
/// thread runs; factoring it here lets it be tested directly (real time, real
/// emission, real stop/join) without a terminal. The one-shot latch in the
/// policy means a long idle stretch yields exactly one emission.
pub fn run_idle_watch(
    runtime: &KairosIdleRuntime,
    poll_interval: Duration,
    emit: &mut dyn FnMut(&str),
) {
    while !runtime.is_stopped() {
        std::thread::sleep(poll_interval);
        if runtime.poll().is_some() {
            emit(IDLE_BRIEF_NOTICE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> JournalDate {
        JournalDate { year, month, day }
    }

    #[test]
    fn startup_action_is_noop_when_gate_off_regardless_of_marker() {
        // Kairos-OFF byte identity: even with a clearly-due marker, the disabled
        // path must produce no spawn intent.
        let today = date(2026, 6, 1);
        assert_eq!(
            startup_dream_action(false, None, today),
            StartupAction::Noop
        );
        assert_eq!(
            startup_dream_action(false, Some(date(2026, 5, 31)), today),
            StartupAction::Noop
        );
    }

    #[test]
    fn startup_action_spawns_when_enabled_and_marker_is_yesterday_or_absent() {
        let today = date(2026, 6, 1);
        assert_eq!(
            startup_dream_action(true, None, today),
            StartupAction::SpawnDream
        );
        assert_eq!(
            startup_dream_action(true, Some(date(2026, 5, 31)), today),
            StartupAction::SpawnDream
        );
    }

    #[test]
    fn startup_action_is_noop_when_enabled_but_already_dreamed_today() {
        // Same-day restart idempotence: marker == today => no second dream.
        let today = date(2026, 6, 1);
        assert_eq!(
            startup_dream_action(true, Some(today), today),
            StartupAction::Noop
        );
    }

    #[test]
    fn startup_action_is_noop_for_future_marker() {
        // Clock skew / hand-edited marker dated ahead of today: treat as
        // up-to-date rather than re-dreaming the future.
        let today = date(2026, 6, 1);
        assert_eq!(
            startup_dream_action(true, Some(date(2026, 6, 2)), today),
            StartupAction::Noop
        );
    }

    #[test]
    fn idle_policy_emits_exactly_one_brief_after_threshold() {
        let mut policy = IdleBriefPolicy::new(3);
        // Below threshold: no signal.
        assert_eq!(policy.on_tick(1), None);
        assert_eq!(policy.on_tick(2), None);
        // Threshold reached: exactly one proactive Brief.
        assert_eq!(policy.on_tick(3), Some(AutonomousSignal::ProactiveBrief));
        // Further idle ticks do not re-emit.
        assert_eq!(policy.on_tick(4), None);
        assert_eq!(policy.on_tick(10), None);
    }

    #[test]
    fn idle_policy_recent_activity_suppresses_the_brief() {
        let mut policy = IdleBriefPolicy::new(3);
        assert_eq!(policy.on_tick(1), None);
        assert_eq!(policy.on_tick(2), None);
        // User interacts just before the threshold — the idle stretch resets, so
        // the tick that would have fired now does not.
        policy.on_user_activity(2);
        assert_eq!(policy.on_tick(3), None);
        assert_eq!(policy.on_tick(4), None);
        // It only fires once a fresh full idle stretch elapses (2 -> 5).
        assert_eq!(policy.on_tick(5), Some(AutonomousSignal::ProactiveBrief));
    }

    #[test]
    fn idle_policy_rearms_after_activity_following_an_emission() {
        let mut policy = IdleBriefPolicy::new(2);
        assert_eq!(policy.on_tick(2), Some(AutonomousSignal::ProactiveBrief));
        assert_eq!(policy.on_tick(3), None);
        // Fresh activity re-arms; a new idle stretch can emit again.
        policy.on_user_activity(3);
        assert_eq!(policy.on_tick(4), None);
        assert_eq!(policy.on_tick(5), Some(AutonomousSignal::ProactiveBrief));
    }

    #[test]
    fn idle_policy_clamps_zero_threshold_so_it_never_emits_pre_tick() {
        let mut policy = IdleBriefPolicy::new(0);
        // Clamped to 1: tick 0 is not idle yet, tick 1 is.
        assert_eq!(policy.on_tick(0), None);
        assert_eq!(policy.on_tick(1), Some(AutonomousSignal::ProactiveBrief));
    }

    #[test]
    fn idle_policy_does_not_emit_one_tick_before_threshold() {
        let mut policy = IdleBriefPolicy::new(5);
        assert_eq!(policy.on_tick(4), None);
        assert_eq!(policy.on_tick(5), Some(AutonomousSignal::ProactiveBrief));
    }

    #[test]
    fn idle_policy_same_tick_activity_then_tick_does_not_emit() {
        // Coarse clock: activity and a tick can land on the same tick value.
        // idle_for == 0 must not fire.
        let mut policy = IdleBriefPolicy::new(3);
        policy.on_user_activity(10);
        assert_eq!(policy.on_tick(10), None);
        assert_eq!(policy.on_tick(12), None);
        assert_eq!(policy.on_tick(13), Some(AutonomousSignal::ProactiveBrief));
    }

    #[test]
    fn idle_policy_activity_exactly_at_threshold_tick_resets() {
        let mut policy = IdleBriefPolicy::new(3);
        // Activity arrives on the very tick the Brief would have fired: reset
        // wins, no emit, and the next stretch is measured from here.
        policy.on_user_activity(3);
        assert_eq!(policy.on_tick(3), None);
        assert_eq!(policy.on_tick(6), Some(AutonomousSignal::ProactiveBrief));
    }

    #[test]
    fn idle_runtime_poll_is_suppressed_after_stop() {
        // Even with a 1ms threshold (so it would otherwise be idle almost
        // immediately), a requested stop makes poll never emit.
        let runtime = KairosIdleRuntime::new(1);
        runtime.request_stop();
        assert!(runtime.is_stopped());
        assert_eq!(runtime.poll(), None);
    }

    #[test]
    fn idle_runtime_not_idle_with_large_threshold() {
        // Freshly constructed with a 1-hour threshold: not idle yet, no emit,
        // and recording activity is safe.
        let runtime = KairosIdleRuntime::from_idle_secs(3600);
        assert_eq!(runtime.poll(), None);
        runtime.record_activity();
        assert_eq!(runtime.poll(), None);
    }

    #[test]
    fn idle_threshold_secs_defaults_and_parses() {
        // Env-dependent: assert the pure default constant rather than mutating
        // the process environment (keeps tests parallel-safe).
        assert_eq!(DEFAULT_IDLE_SECS, 300);
    }

    #[test]
    fn run_idle_watch_emits_exactly_once_then_stops_on_request() {
        use std::sync::Arc;

        // 60ms idle threshold, polled every 10ms; a 300ms idle window crosses it
        // once. The one-shot latch + stop request bound the loop deterministically.
        let runtime = Arc::new(KairosIdleRuntime::new(60));
        let emitted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let watcher = {
            let runtime = Arc::clone(&runtime);
            let emitted = Arc::clone(&emitted);
            std::thread::spawn(move || {
                let mut sink = |message: &str| emitted.lock().unwrap().push(message.to_string());
                run_idle_watch(&runtime, Duration::from_millis(10), &mut sink);
            })
        };

        std::thread::sleep(Duration::from_millis(300));
        runtime.request_stop();
        watcher.join().expect("watcher thread should join");

        let emitted = emitted.lock().unwrap();
        assert_eq!(
            emitted.len(),
            1,
            "exactly one proactive Brief should fire per idle stretch, got {emitted:?}"
        );
        assert_eq!(emitted[0], IDLE_BRIEF_NOTICE);
    }

    #[test]
    fn run_idle_watch_emits_nothing_when_stopped_before_threshold() {
        use std::sync::Arc;

        // 10s threshold but stopped almost immediately: no emission, clean join.
        let runtime = Arc::new(KairosIdleRuntime::from_idle_secs(10));
        let emitted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let watcher = {
            let runtime = Arc::clone(&runtime);
            let emitted = Arc::clone(&emitted);
            std::thread::spawn(move || {
                let mut sink = |message: &str| emitted.lock().unwrap().push(message.to_string());
                run_idle_watch(&runtime, Duration::from_millis(10), &mut sink);
            })
        };
        std::thread::sleep(Duration::from_millis(40));
        runtime.request_stop();
        watcher.join().expect("watcher thread should join");
        assert!(emitted.lock().unwrap().is_empty());
    }
}
