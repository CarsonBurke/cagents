//! Owner-thread idle scheduling for Ghostty's bounded compression steps.
use std::time::{Duration, Instant};

use libghostty_vt::terminal::CompressionResult;

const IDLE_DELAY: Duration = Duration::from_secs(1);
const STEP_DELAY: Duration = Duration::from_millis(10);

pub(super) struct IdleCompression<Activity> {
    activity: Option<Activity>,
    deadline: Option<Instant>,
    enabled: bool,
}

impl<Activity: Copy + Eq> IdleCompression<Activity> {
    pub(super) fn new() -> Self {
        Self {
            activity: None,
            deadline: None,
            enabled: true,
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

    pub(super) fn finished(&mut self, result: CompressionResult, now: Instant) {
        if !self.enabled {
            return;
        }
        match result {
            // Yield to queued input/output between every bounded step.
            CompressionResult::Pending => self.deadline = Some(now + STEP_DELAY),
            CompressionResult::Complete => self.deadline = None,
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
            self.deadline = Some(now + IDLE_DELAY);
        }
    }

    pub(super) fn disable(&mut self) {
        self.enabled = false;
        self.deadline = None;
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
        schedule.finished(CompressionResult::Pending, idle);
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
        schedule.finished(CompressionResult::Complete, now + IDLE_DELAY);
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
        schedule.finished(CompressionResult::Unsupported, now);
        schedule.retry(now);
        schedule.finished(CompressionResult::Pending, now);
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
    fn history_reads_rearm_a_completed_pass_with_an_unchanged_token() {
        let now = Instant::now();
        let mut schedule = IdleCompression::new();
        schedule.observe(1, now);
        schedule.finished(CompressionResult::Complete, now);
        schedule.history_read(now);
        schedule.observe(1, now + STEP_DELAY);
        assert_eq!(schedule.after(now), Some(IDLE_DELAY));
        schedule.history_read(now + IDLE_DELAY / 2);
        assert!(!schedule.due(now + IDLE_DELAY));
        assert!(schedule.due(now + IDLE_DELAY + IDLE_DELAY / 2));
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
            schedule.finished(result, now);
            steps += 1;
            assert!(steps <= 10_000, "compression did not settle");
            now += STEP_DELAY;
        }
        assert!(steps > 0);
        assert_eq!(schedule.after(now), None);
        if schedule.enabled() {
            let token = terminal.compression_activity().unwrap();
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
                schedule.finished(result, now);
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
            schedule.finished(result, now);
            assert_eq!(schedule.after(now), None);
            terminal.resize(110, 24, 8, 16).unwrap();
            schedule.observe(terminal.compression_activity().unwrap(), now);
            assert_eq!(schedule.after(now), Some(IDLE_DELAY));
        }
    }
}
