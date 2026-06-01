//! Kairos — the opt-in, proactive/autonomous agent layer.
//!
//! This module owns the *clock-free decision cores* of the autonomous loop so
//! they can be tested deterministically (E2E_TEST_PLAN B2/B3, "virtual clock
//! only — no real `sleep`"). It deliberately does **not** rewrite the
//! synchronous REPL into an interruptible event pump — that is the highest
//! blast-radius change and is intentionally deferred (see the Kairos build
//! order). What lives here:
//!
//! - [`kairos_enabled`]: the single feature gate. Off by default, so every
//!   Kairos behaviour is dormant unless the operator opts in. With the gate off
//!   the live REPL path is byte-identical to baseline.
//! - [`startup_dream_action`]: the B3 dream-on-start decision, folded with the
//!   gate, returning a *pure intent* ([`StartupAction`]) so the startup wiring
//!   can be tested without spawning a thread or calling a model.
//! - [`IdleBriefPolicy`]: the B2 idle-loop core. Given a monotonically
//!   increasing virtual tick, it decides when exactly one proactive `Brief`
//!   should be emitted, and re-arms on user activity. It is **not** wired into
//!   the live REPL in this slice; only its semantics are delivered + tested.

use crate::dream::{dream_on_start_decision, DreamOnStartDecision};
use crate::journal::JournalDate;

/// Environment variable that opts the agent into the Kairos autonomous layer.
pub const KAIROS_ENV: &str = "CLAW_KAIROS";

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
}
