//! Owner-thread idle scheduling for Ghostty's bounded compression steps.
use std::time::{Duration, Instant};

use libghostty_vt::terminal::{CompressionMode, CompressionResult};

const IDLE_DELAY: Duration = Duration::from_secs(1);
const STEP_DELAY: Duration = Duration::from_millis(10);

pub(super) struct IdleCompression<Activity> {
    activity: Option<Activity>,
    deadline: Option<Instant>,
    enabled: bool,
    pending_pass: bool,
    recheck_after_pass: bool,
}

impl<Activity: Copy + Eq> IdleCompression<Activity> {
    pub(super) fn new() -> Self {
        Self {
            activity: None,
            deadline: None,
            enabled: true,
            pending_pass: false,
            recheck_after_pass: false,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn observe(&mut self, activity: Activity, now: Instant) {
        // The token is opaque: wrapping or decreasing is still activity.
        if self.enabled && self.activity != Some(activity) {
            self.activity = Some(activity);
            self.deadline = Some(now + IDLE_DELAY);
        }
    }

    pub(super) fn due(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= now)
    }

    pub(super) fn after(&self, now: Instant) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub(super) fn finished(
        &mut self,
        result: CompressionResult,
        mode: CompressionMode,
        now: Instant,
    ) {
        if !self.enabled {
            return;
        }
        match result {
            // Yield to queued input/output between every bounded step.
            CompressionResult::Pending => {
                self.pending_pass = true;
                self.deadline = Some(now + STEP_DELAY);
            }
            CompressionResult::Complete => {
                self.pending_pass = false;
                // A bounded read can restore a page behind a verification
                // cursor. Complete resets that native cursor, so one fresh
                // incremental pass can reclaim it. A full scan already did so.
                let recheck = std::mem::take(&mut self.recheck_after_pass);
                self.deadline =
                    (mode == CompressionMode::Incremental && recheck).then_some(now + STEP_DELAY);
            }
            CompressionResult::Unsupported => self.disable(),
        }
    }

    pub(super) fn retry(&mut self, now: Instant) {
        // A transient allocation error must neither spin nor permanently
        // prevent reclamation while the terminal remains quiet.
        if self.enabled {
            self.deadline = Some(now + IDLE_DELAY);
        }
    }

    pub(super) fn history_read(&mut self, now: Instant) {
        // Ghostty restores cold pages for formatter/grid reads without
        // changing its activity token. These callers also restart the delay.
        if self.enabled {
            // Reads before a pass starts are covered by its oldest-page start.
            // A pending pass may already have verified the restored prefix.
            self.recheck_after_pass |= self.pending_pass;
            self.deadline = Some(now + IDLE_DELAY);
        }
    }

    pub(super) fn disable(&mut self) {
        self.enabled = false;
        self.deadline = None;
        self.pending_pass = false;
        self.recheck_after_pass = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_restarts_the_idle_delay_including_token_wrap() {
        let now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(u64::MAX, now);
        assert_eq!(schedule.after(now), Some(IDLE_DELAY));
        schedule.observe(u64::MAX, now + IDLE_DELAY / 2);
        assert_eq!(schedule.after(now), Some(IDLE_DELAY));
        schedule.observe(0, now + IDLE_DELAY / 2);
        assert!(!schedule.due(now + IDLE_DELAY));
        assert!(schedule.due(now + IDLE_DELAY + IDLE_DELAY / 2));
    }

    #[test]
    fn pending_steps_yield_and_new_activity_postpones_continuation() {
        let now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(1, now);
        let idle = now + IDLE_DELAY;
        assert!(schedule.due(idle));
        schedule.finished(
            CompressionResult::Pending,
            CompressionMode::Incremental,
            idle,
        );
        assert!(!schedule.due(idle));
        assert_eq!(schedule.after(idle), Some(STEP_DELAY));
        schedule.observe(2, idle);
        assert!(!schedule.due(idle + STEP_DELAY));
        assert!(schedule.due(idle + IDLE_DELAY));
    }

    #[test]
    fn complete_pass_sleeps_until_activity_changes() {
        let now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(1, now);
        schedule.finished(
            CompressionResult::Complete,
            CompressionMode::Incremental,
            now + IDLE_DELAY,
        );
        schedule.observe(1, now + IDLE_DELAY * 2);
        assert_eq!(schedule.after(now), None);
        schedule.observe(2, now + IDLE_DELAY * 2);
        assert_eq!(schedule.after(now + IDLE_DELAY * 2), Some(IDLE_DELAY));
    }

    #[test]
    fn unsupported_target_never_rearms_a_timer() {
        let now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(1, now);
        schedule.finished(
            CompressionResult::Unsupported,
            CompressionMode::Incremental,
            now,
        );
        schedule.retry(now);
        schedule.finished(
            CompressionResult::Pending,
            CompressionMode::Incremental,
            now,
        );
        schedule.observe(2, now + IDLE_DELAY);
        assert!(!schedule.enabled());
        assert_eq!(schedule.after(now), None);
    }

    #[test]
    fn compression_error_retries_with_a_full_idle_delay() {
        let now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(1, now);
        schedule.retry(now + IDLE_DELAY);
        assert!(!schedule.due(now + IDLE_DELAY));
        assert!(schedule.due(now + IDLE_DELAY * 2));
    }

    #[test]
    fn history_read_during_pending_pass_survives_retries_and_rechecks_once() {
        let now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(1, now);
        schedule.finished(
            CompressionResult::Pending,
            CompressionMode::Incremental,
            now,
        );
        schedule.history_read(now + STEP_DELAY);
        schedule.finished(
            CompressionResult::Pending,
            CompressionMode::Incremental,
            now + STEP_DELAY * 2,
        );
        schedule.retry(now + STEP_DELAY * 3);
        schedule.observe(2, now + STEP_DELAY * 4);
        let complete = now + IDLE_DELAY * 2;
        schedule.finished(
            CompressionResult::Complete,
            CompressionMode::Incremental,
            complete,
        );
        assert_eq!(schedule.after(complete), Some(STEP_DELAY));
        assert!(!schedule.due(complete));
        assert!(schedule.due(complete + STEP_DELAY));

        // Complete reset the native cursor. The sticky read is consumed, so
        // this fresh pass settles without scheduling another quiet pass.
        schedule.finished(
            CompressionResult::Pending,
            CompressionMode::Incremental,
            complete + STEP_DELAY,
        );
        schedule.finished(
            CompressionResult::Complete,
            CompressionMode::Incremental,
            complete + STEP_DELAY * 2,
        );
        assert_eq!(schedule.after(complete + STEP_DELAY * 2), None);
    }

    #[test]
    fn full_completion_and_disable_discard_pending_history_rechecks() {
        let now = Instant::now();
        for stop in 0..3 {
            let mut schedule = IdleCompression::new();
            schedule.observe(1, now);
            schedule.finished(
                CompressionResult::Pending,
                CompressionMode::Incremental,
                now,
            );
            schedule.history_read(now);
            match stop {
                0 => schedule.finished(CompressionResult::Complete, CompressionMode::Full, now),
                1 => schedule.finished(
                    CompressionResult::Unsupported,
                    CompressionMode::Incremental,
                    now,
                ),
                _ => schedule.disable(),
            }
            assert_eq!(schedule.after(now), None);
            // A read after Full completion starts an ordinary fresh pass;
            // disabled/unsupported schedules must remain asleep instead.
            schedule.history_read(now);
            assert_eq!(schedule.after(now), (stop == 0).then_some(IDLE_DELAY));
            schedule.finished(
                CompressionResult::Complete,
                CompressionMode::Incremental,
                now + IDLE_DELAY,
            );
            assert_eq!(schedule.after(now + IDLE_DELAY), None);
        }
    }

    #[test]
    fn history_reads_rearm_a_completed_pass_with_an_unchanged_token() {
        let now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(1, now);
        schedule.finished(
            CompressionResult::Complete,
            CompressionMode::Incremental,
            now,
        );
        schedule.history_read(now);
        schedule.observe(1, now + STEP_DELAY);
        assert_eq!(schedule.after(now), Some(IDLE_DELAY));
        schedule.history_read(now + IDLE_DELAY / 2);
        assert!(!schedule.due(now + IDLE_DELAY));
        assert!(schedule.due(now + IDLE_DELAY + IDLE_DELAY / 2));
        schedule.finished(
            CompressionResult::Pending,
            CompressionMode::Incremental,
            now + IDLE_DELAY * 2,
        );
        schedule.finished(
            CompressionResult::Complete,
            CompressionMode::Incremental,
            now + IDLE_DELAY * 2 + STEP_DELAY,
        );
        assert_eq!(schedule.after(now + IDLE_DELAY * 2 + STEP_DELAY), None);
        schedule.disable();
        schedule.history_read(now);
        assert_eq!(schedule.after(now), None);
    }

    #[test]
    fn incremental_steps_preserve_styled_history_and_resize_rearms_reclamation() {
        use libghostty_vt::{
            Terminal, TerminalOptions,
            fmt::{Format, Formatter, FormatterOptions},
            terminal::CompressionMode,
        };

        let mut terminal = Terminal::new(TerminalOptions {
            cols: 100,
            rows: 24,
            max_scrollback: 8 * 1024 * 1024,
        })
        .unwrap();
        for row in 0..2_000 {
            terminal.vt_write(
                format!(
                    "\x1b[38;5;{}mfile-{row:04}.rs: reading café ✓\x1b[0m\r\n",
                    row % 216 + 16
                )
                .as_bytes(),
            );
        }
        let text = |terminal: &Terminal<'_, '_>| {
            Formatter::new(
                terminal,
                FormatterOptions::new()
                    .with_format(Format::Vt)
                    .with_style(true),
            )
            .unwrap()
            .format_alloc(None)
            .unwrap()
            .to_vec()
        };
        let before = text(&terminal);
        let rows = terminal.scrollbar().unwrap().total;
        assert!(rows >= 2_000);
        let mut now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(terminal.compression_activity().unwrap(), now);
        now += IDLE_DELAY;
        let mut steps = 0;
        while schedule.due(now) {
            let result = terminal.compress(CompressionMode::Incremental).unwrap();
            schedule.observe(terminal.compression_activity().unwrap(), now);
            schedule.finished(result, CompressionMode::Incremental, now);
            steps += 1;
            assert!(steps <= 10_000, "compression did not settle");
            now += STEP_DELAY;
        }
        assert!(steps > 0);
        assert_eq!(schedule.after(now), None);
        if schedule.enabled() {
            let token = terminal.compression_activity().unwrap();
            // Calibrate the quiet native traversal without reading history.
            // The direct PageList regression checks its exact verifying flag
            // and storage state; this host path checks the bounded copy route.
            let mut quiet_steps = 0;
            loop {
                let result = terminal.compress(CompressionMode::Incremental).unwrap();
                quiet_steps += 1;
                assert!(quiet_steps <= 10_000);
                if result == CompressionResult::Complete {
                    break;
                }
                assert_eq!(result, CompressionResult::Pending);
            }
            assert!(quiet_steps >= 4, "history must span multiple skip budgets");
            schedule.history_read(now);
            now += IDLE_DELAY;
            for _ in 0..quiet_steps - 1 {
                assert!(schedule.due(now));
                let result = terminal.compress(CompressionMode::Incremental).unwrap();
                assert_eq!(result, CompressionResult::Pending);
                schedule.finished(result, CompressionMode::Incremental, now);
                now += STEP_DELAY;
            }
            let selected = crate::terminal::search::selected_text(
                &terminal,
                crate::terminal::search::SearchMatch {
                    start_row: 0,
                    start_column: 0,
                    end_row: 0,
                    end_column: 100,
                },
            )
            .unwrap();
            assert!(selected.contains("file-0000.rs: reading café ✓"));
            assert_eq!(terminal.compression_activity().unwrap(), token);
            schedule.history_read(now);
            now += IDLE_DELAY;
            let result = terminal.compress(CompressionMode::Incremental).unwrap();
            assert_eq!(result, CompressionResult::Complete);
            schedule.finished(result, CompressionMode::Incremental, now);
            assert_eq!(schedule.after(now), Some(STEP_DELAY));
            now += STEP_DELAY;
            let mut recheck_steps = 0;
            while schedule.due(now) {
                let result = terminal.compress(CompressionMode::Incremental).unwrap();
                schedule.observe(terminal.compression_activity().unwrap(), now);
                schedule.finished(result, CompressionMode::Incremental, now);
                recheck_steps += 1;
                assert!(recheck_steps <= 10_000);
                now += STEP_DELAY;
            }
            assert!(recheck_steps > 0);
            assert_eq!(schedule.after(now), None);
            assert_eq!(terminal.compression_activity().unwrap(), token);

            let (matches, truncated) =
                crate::terminal::search::search(&terminal, "reading café", true, || false)
                    .unwrap()
                    .unwrap();
            assert!(!truncated);
            assert_eq!(matches.len(), 2_000);
            // History reads restore compressed mappings without marking the
            // pinned Ghostty activity token. The worker must rearm explicitly.
            assert_eq!(terminal.compression_activity().unwrap(), token);
            schedule.history_read(now);
            schedule.observe(token, now);
            assert_eq!(schedule.after(now), Some(IDLE_DELAY));
            now += IDLE_DELAY;
            let mut second_steps = 0;
            while schedule.due(now) {
                let result = terminal.compress(CompressionMode::Incremental).unwrap();
                schedule.observe(terminal.compression_activity().unwrap(), now);
                schedule.finished(result, CompressionMode::Incremental, now);
                second_steps += 1;
                assert!(second_steps <= 10_000);
                now += STEP_DELAY;
            }
            assert!(second_steps > 0);
            assert_eq!(schedule.after(now), None);
        }
        // Formatting can itself restore history. Only read after the second
        // pass, so the assertion checks data preserved through recompression.
        assert_eq!(text(&terminal), before);
        assert_eq!(terminal.scrollbar().unwrap().total, rows);
        if schedule.enabled() {
            // A full pass has the same no-continuation contract. Resize then
            // decompresses history without any new PTY output.
            let result = terminal.compress(CompressionMode::Full).unwrap();
            schedule.observe(terminal.compression_activity().unwrap(), now);
            schedule.finished(result, CompressionMode::Full, now);
            assert_eq!(schedule.after(now), None);
            terminal.resize(110, 24, 8, 16).unwrap();
            schedule.observe(terminal.compression_activity().unwrap(), now);
            assert_eq!(schedule.after(now), Some(IDLE_DELAY));
        }
    }
}
