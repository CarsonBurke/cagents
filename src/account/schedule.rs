//! When to ask a provider's CLI about an account again.
//!
//! A probe starts the provider's CLI, which costs a process launch, a network
//! round trip on the provider's side and a sliver of the provider's patience,
//! so it is spent only where a person can see the answer and the answer they
//! have is getting old:
//!
//! - never while the window is unfocused, since nobody is reading it;
//! - on regaining focus, once the figures are more than [`STALE_AFTER`] old;
//! - on the periodic tick, once they are more than [`REFRESH_INTERVAL`] old;
//! - on a manual refresh at most once per [`MANUAL_DEBOUNCE`], whatever the
//!   figures' age, because the reader asked;
//! - after a failure, not before an exponential backoff from
//!   [`BACKOFF_BASE`] to [`BACKOFF_MAX`] has passed;
//! - and never twice at once for the same account.
//!
//! Figures that arrive another way - another window's probe read back from
//! the cache, or a running conversation's live events - count exactly as a
//! probe would, which is what keeps several windows, and a busy conversation,
//! from probing at all.
//!
//! Every method takes the current time rather than reading a clock, so the
//! policy is tested by stating times instead of waiting for them.

/// Unix milliseconds.
pub type Millis = u64;

pub const STALE_AFTER: Millis = 5 * 60_000;
pub const REFRESH_INTERVAL: Millis = 10 * 60_000;
pub const MANUAL_DEBOUNCE: Millis = 60_000;
pub const BACKOFF_BASE: Millis = 30_000;
pub const BACKOFF_MAX: Millis = 15 * 60_000;
/// Older than this, figures are shown as of when they were observed rather
/// than as current: two missed refreshes, plus the slack of one probe.
pub const DISPLAY_STALE_AFTER: Millis = 2 * REFRESH_INTERVAL + 60_000;
/// How far past this machine's clock a stamp may be before it is read as a
/// clock that has since been set back rather than as a newer observation.
/// Trusting it would hold every refresh off until the clock caught up.
pub const CLOCK_SLACK: Millis = 60_000;
/// How long after a sign-in was started the account is watched for it to
/// land, and how often it is checked meanwhile. Signing in happens in a
/// browser, so the check that matters is the one when the reader comes back.
pub const LOGIN_WATCH: Millis = 5 * 60_000;
pub const LOGIN_POLL: Millis = 45_000;
pub const LOGIN_RETURN: Millis = 5_000;
/// Well past a probe's own timeout: a probe still unanswered by then was lost.
pub const IN_FLIGHT_LIMIT: Millis = 2 * 60_000;

/// What prompted a scheduling decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The periodic tick while the window is focused.
    Tick,
    /// The window just gained focus, including the first focus at startup.
    Focus,
    /// The reader asked.
    Manual,
}

/// One account's probe state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    /// When the newest figures, from any source, were observed.
    observed_at: Option<Millis>,
    /// When the probe in flight started.
    in_flight: Option<Millis>,
    failures: u32,
    retry_at: Option<Millis>,
    last_manual: Option<Millis>,
    awaiting_login_until: Option<Millis>,
}

impl Schedule {
    /// Whether `trigger` at `now` should start a probe.
    pub fn should_probe(&self, trigger: Trigger, now: Millis, focused: bool) -> bool {
        if self.in_flight(now) {
            return false;
        }
        // A stamp beyond the slack is from before the clock was set back,
        // and says nothing about how old the figures are.
        let age = self
            .observed_at
            .filter(|observed| *observed <= now.saturating_add(CLOCK_SLACK))
            .map_or(Millis::MAX, |observed| now.saturating_sub(observed));
        match trigger {
            // A manual refresh overrides staleness and backoff - the reader
            // is looking at a failure and asking for another attempt - but
            // not the debounce that stops a held key from becoming a storm.
            Trigger::Manual => self
                .last_manual
                .is_none_or(|last| now.saturating_sub(last) >= MANUAL_DEBOUNCE),
            Trigger::Tick | Trigger::Focus => {
                let awaiting = self.awaiting_login(now);
                let threshold = match (trigger, awaiting) {
                    (Trigger::Focus, true) => LOGIN_RETURN,
                    (_, true) => LOGIN_POLL,
                    (Trigger::Focus, false) => STALE_AFTER,
                    _ => REFRESH_INTERVAL,
                };
                focused && self.retry_at.is_none_or(|retry| now >= retry) && age >= threshold
            }
        }
    }

    /// Whether a manual refresh at `now` would be accepted, for a control
    /// that should look disabled while it would not.
    pub fn manual_ready(&self, now: Millis) -> bool {
        self.should_probe(Trigger::Manual, now, true)
    }

    /// When the debounce on the last manual refresh lifts, so a control shown
    /// disabled can be re-enabled at that moment rather than on a later tick.
    pub fn manual_ready_at(&self) -> Option<Millis> {
        self.last_manual
            .map(|last| last.saturating_add(MANUAL_DEBOUNCE))
    }

    /// Whether a probe is running. One that has not reported back within
    /// [`IN_FLIGHT_LIMIT`] never will - its task was lost - and no longer
    /// holds the account's next probe back.
    pub fn in_flight(&self, now: Millis) -> bool {
        self.in_flight
            .is_some_and(|since| now.saturating_sub(since) < IN_FLIGHT_LIMIT)
    }

    pub fn start(&mut self, trigger: Trigger, now: Millis) {
        self.in_flight = Some(now);
        if trigger == Trigger::Manual {
            self.last_manual = Some(now);
        }
    }

    /// A probe produced an answer, which includes "signed out" and "not
    /// installed": those are states to report, not failures to retry.
    ///
    /// The answer is the newest thing known about the account, so it replaces
    /// the previous stamp outright - which is also what recovers from a stamp
    /// left in the future by a clock that has since been set back.
    pub fn succeed(&mut self, observed_at: Millis) {
        self.in_flight = None;
        self.failures = 0;
        self.retry_at = None;
        self.observed_at = Some(observed_at);
    }

    /// A probe produced no answer - it timed out, the CLI crashed, or its
    /// output could not be read - and the next automatic one waits longer.
    pub fn fail(&mut self, now: Millis) {
        self.in_flight = None;
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now.saturating_add(backoff(self.failures)));
    }

    /// A probe ended without producing anything to count either way, such
    /// as another window holding the provider's probe lock. A manual refresh
    /// that checked nothing does not spend the reader's attempt.
    pub fn abandon(&mut self) {
        if self.in_flight.is_some() && self.last_manual == self.in_flight {
            self.last_manual = None;
        }
        self.in_flight = None;
    }

    /// Figures observed another way: a live event or the shared cache. An
    /// older observation never rewinds a newer one.
    pub fn observe(&mut self, observed_at: Millis) {
        self.observed_at = Some(
            self.observed_at
                .map_or(observed_at, |seen| seen.max(observed_at)),
        );
    }

    /// The login changed under the figures, so they no longer describe it:
    /// the next tick or focus checks again whatever their age, and backoff
    /// earned by the previous login no longer applies.
    pub fn invalidate(&mut self) {
        self.observed_at = None;
        self.failures = 0;
        self.retry_at = None;
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// A sign-in was started at `now`: until it lands, or for
    /// [`LOGIN_WATCH`], check far more eagerly than usual so the account
    /// comes back as soon as the reader does. Backoff from the failures that
    /// led here no longer applies.
    pub fn await_login(&mut self, now: Millis) {
        self.awaiting_login_until = Some(now.saturating_add(LOGIN_WATCH));
        self.retry_at = None;
    }

    /// Keep watching a running sign-in without bypassing backoff earned by
    /// a failed verification while its browser handoff is in progress.
    pub fn extend_login_watch(&mut self, now: Millis) {
        self.awaiting_login_until = Some(now.saturating_add(LOGIN_WATCH));
    }

    /// The account answered as signed in, so the watch is over.
    pub fn login_settled(&mut self) {
        self.awaiting_login_until = None;
    }

    pub fn awaiting_login(&self, now: Millis) -> bool {
        self.awaiting_login_until.is_some_and(|until| now < until)
    }
}

/// The wait after the `failures`th consecutive failure: 30 s, doubling, to
/// at most 15 minutes.
pub fn backoff(failures: u32) -> Millis {
    let doublings = failures.saturating_sub(1).min(16);
    BACKOFF_BASE.saturating_mul(1 << doublings).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: Millis = 60_000;
    const T0: Millis = 1_790_000_000_000;

    #[test]
    fn a_first_focus_probes_and_an_unfocused_window_never_does() {
        let schedule = Schedule::default();
        assert!(schedule.should_probe(Trigger::Focus, T0, true));
        assert!(schedule.should_probe(Trigger::Tick, T0, true));
        assert!(!schedule.should_probe(Trigger::Focus, T0, false));
        assert!(!schedule.should_probe(Trigger::Tick, T0, false));
    }

    #[test]
    fn fresh_figures_suppress_automatic_probes_until_they_age() {
        let mut schedule = Schedule::default();
        schedule.start(Trigger::Focus, T0);
        schedule.succeed(T0);
        assert!(!schedule.should_probe(Trigger::Focus, T0 + 4 * MINUTE, true));
        assert!(schedule.should_probe(Trigger::Focus, T0 + 5 * MINUTE, true));
        assert!(!schedule.should_probe(Trigger::Tick, T0 + 9 * MINUTE, true));
        assert!(schedule.should_probe(Trigger::Tick, T0 + 10 * MINUTE, true));
    }

    /// A conversation reporting its limits live, or another window's probe
    /// read from the cache, is as good as a probe of our own.
    #[test]
    fn figures_from_elsewhere_count_as_a_probe() {
        let mut schedule = Schedule::default();
        schedule.observe(T0);
        assert!(!schedule.should_probe(Trigger::Tick, T0 + 9 * MINUTE, true));
        // An older observation never rewinds a newer one.
        schedule.observe(T0 - 60 * MINUTE);
        assert!(!schedule.should_probe(Trigger::Tick, T0 + 9 * MINUTE, true));
    }

    /// A clock set back after a probe leaves its stamp in the future. That
    /// must neither hold refreshes off until the clock catches up nor, once
    /// a probe answers, keep asking every tick.
    #[test]
    fn a_clock_set_back_neither_freezes_nor_floods_probing() {
        let mut schedule = Schedule::default();
        schedule.start(Trigger::Tick, T0 + 60 * MINUTE);
        schedule.succeed(T0 + 60 * MINUTE);
        assert!(schedule.should_probe(Trigger::Tick, T0, true));
        schedule.start(Trigger::Tick, T0);
        schedule.succeed(T0);
        assert!(!schedule.should_probe(Trigger::Tick, T0 + MINUTE, true));
    }

    #[test]
    fn only_one_probe_runs_at_a_time() {
        let mut schedule = Schedule::default();
        schedule.start(Trigger::Tick, T0);
        assert!(schedule.in_flight(T0));
        assert!(!schedule.should_probe(Trigger::Focus, T0 + MINUTE, true));
        assert!(!schedule.should_probe(Trigger::Manual, T0 + MINUTE, true));
        // A probe that never reports back stops blocking the next.
        assert!(!schedule.in_flight(T0 + IN_FLIGHT_LIMIT));
        assert!(schedule.should_probe(Trigger::Tick, T0 + IN_FLIGHT_LIMIT, true));
        schedule.abandon();
        assert!(schedule.should_probe(Trigger::Tick, T0, true));
    }

    #[test]
    fn manual_refresh_is_debounced_but_ignores_age_and_backoff() {
        let mut schedule = Schedule::default();
        schedule.start(Trigger::Manual, T0);
        schedule.fail(T0);
        // Backing off from automatic probes...
        assert!(!schedule.should_probe(Trigger::Tick, T0 + 10_000, true));
        // ...but still debounced for the reader.
        assert!(!schedule.manual_ready(T0 + 59_000));
        assert!(schedule.manual_ready(T0 + 60_000));
        schedule.start(Trigger::Manual, T0 + 60_000);
        schedule.succeed(T0 + 61_000);
        // Fresh figures do not stop a manual refresh once the debounce ends.
        assert!(schedule.manual_ready(T0 + 120_000));
        // One that found another window already checking checked nothing,
        // and the reader may ask again at once.
        schedule.start(Trigger::Manual, T0 + 120_000);
        schedule.abandon();
        assert!(schedule.manual_ready(T0 + 121_000));
    }

    /// The moment a debounced refresh control comes back is known in
    /// advance, so the interface can wake for it instead of waiting a tick.
    #[test]
    fn the_end_of_the_manual_debounce_is_known_in_advance() {
        let mut schedule = Schedule::default();
        assert_eq!(schedule.manual_ready_at(), None);
        schedule.start(Trigger::Manual, T0);
        schedule.succeed(T0 + 1_000);
        let ready = schedule.manual_ready_at().unwrap();
        assert!(!schedule.manual_ready(ready - 1));
        assert!(schedule.manual_ready(ready));
    }

    /// A login that changed under fresh figures is checked at the next
    /// chance, without waiting out their age or the old login's backoff.
    #[test]
    fn a_changed_login_is_checked_at_the_next_chance() {
        let mut schedule = Schedule::default();
        schedule.start(Trigger::Tick, T0);
        schedule.fail(T0);
        schedule.observe(T0);
        assert!(!schedule.should_probe(Trigger::Tick, T0 + 1, true));
        schedule.invalidate();
        assert!(schedule.should_probe(Trigger::Tick, T0 + 1, true));
        assert!(!schedule.should_probe(Trigger::Tick, T0 + 1, false));
    }

    #[test]
    fn failures_back_off_exponentially_to_a_ceiling() {
        assert_eq!(backoff(1), 30_000);
        assert_eq!(backoff(2), 60_000);
        assert_eq!(backoff(3), 120_000);
        assert_eq!(backoff(6), BACKOFF_MAX);
        assert_eq!(backoff(u32::MAX), BACKOFF_MAX);

        let mut schedule = Schedule::default();
        let mut now = T0;
        for failures in 1..=3 {
            schedule.start(Trigger::Tick, now);
            schedule.fail(now);
            assert_eq!(schedule.failures(), failures);
            let wait = backoff(failures);
            assert!(!schedule.should_probe(Trigger::Tick, now + wait - 1, true));
            assert!(schedule.should_probe(Trigger::Tick, now + wait, true));
            now += wait;
        }
        schedule.start(Trigger::Tick, now);
        schedule.succeed(now);
        assert_eq!(schedule.failures(), 0);
        assert!(schedule.should_probe(Trigger::Tick, now + REFRESH_INTERVAL, true));
    }

    #[test]
    fn a_started_sign_in_is_checked_eagerly_until_it_lands_or_the_watch_ends() {
        let mut schedule = Schedule::default();
        schedule.start(Trigger::Focus, T0);
        schedule.fail(T0);
        schedule.await_login(T0);
        assert!(schedule.awaiting_login(T0));
        // The backoff from the stale login no longer holds the check back.
        assert!(schedule.should_probe(Trigger::Tick, T0, true));
        schedule.start(Trigger::Tick, T0);
        schedule.succeed(T0);
        // Returning from the browser checks almost at once...
        assert!(!schedule.should_probe(Trigger::Focus, T0 + 4_000, true));
        assert!(schedule.should_probe(Trigger::Focus, T0 + LOGIN_RETURN, true));
        // ...and the tick polls meanwhile, though never while unfocused.
        assert!(schedule.should_probe(Trigger::Tick, T0 + LOGIN_POLL, true));
        assert!(!schedule.should_probe(Trigger::Tick, T0 + LOGIN_POLL, false));
        // The watch ends on its own...
        assert!(!schedule.awaiting_login(T0 + LOGIN_WATCH));
        assert!(!schedule.should_probe(Trigger::Tick, T0 + LOGIN_WATCH, true));
        // ...or once the account answers signed in.
        schedule.await_login(T0);
        schedule.login_settled();
        assert!(!schedule.should_probe(Trigger::Focus, T0 + LOGIN_RETURN, true));
    }

    #[test]
    fn running_sign_in_extends_polling_without_retrying_failed_verification_early() {
        let mut schedule = Schedule::default();
        schedule.await_login(T0);
        schedule.start(Trigger::Tick, T0);
        schedule.fail(T0);
        schedule.extend_login_watch(T0 + 10_000);
        assert!(!schedule.should_probe(Trigger::Tick, T0 + backoff(1) - 1, true));
        assert!(schedule.should_probe(Trigger::Tick, T0 + backoff(1), true));
        assert!(schedule.awaiting_login(T0 + LOGIN_WATCH));
        assert!(!schedule.awaiting_login(T0 + LOGIN_WATCH + 10_000));
    }
}
