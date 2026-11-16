//! When a text cursor blinks, and when it stops.
use crate::time::Duration;

use std::sync::atomic::{AtomicU64, Ordering};

/// A text cursor's blink: on for one interval, off for the next, until it
/// has gone [`Blink::timeout`] without input, when it rests visible.
///
/// A cursor that blinks forever repaints its whole window twice a second for
/// as long as its field keeps focus, even while nobody is typing. Resting
/// after a timeout follows GTK's `gtk-cursor-blink-timeout`, whose default of
/// ten seconds this shares; any input restarts the blink.
///
/// The desktop's own setting, when the application has read one, replaces the
/// default through [`Blink::set_preferred`]; widgets draw with
/// [`Blink::current`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blink {
    /// How long the cursor stays in each phase.
    pub interval: Duration,
    /// How long after the last input the cursor stops blinking.
    pub timeout: Duration,
}

impl Blink {
    /// The blink of text fields and editors.
    pub const DEFAULT: Self = Self::new(Duration::from_millis(500));

    /// A cursor that never blinks: it is always drawn and needs no frames.
    pub const STEADY: Self = Self {
        interval: Self::DEFAULT.interval,
        timeout: Duration::ZERO,
    };

    /// A blink with the given phase length and the default timeout.
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            timeout: Duration::from_secs(10),
        }
    }

    /// Whether the cursor blinks at all after input.
    pub fn blinks(&self) -> bool {
        !self.timeout.is_zero()
    }

    /// The blink the desktop asks for, if the application has read one.
    pub fn preferred() -> Option<Self> {
        let interval = PREFERRED_INTERVAL.load(Ordering::Relaxed);
        let timeout = PREFERRED_TIMEOUT.load(Ordering::Relaxed);
        (interval != UNSET && timeout != UNSET).then(|| Self {
            interval: Duration::from_nanos(interval),
            timeout: Duration::from_nanos(timeout),
        })
    }

    /// Records the desktop's blink for every cursor in the process, or
    /// forgets it with `None`. Cursors pick it up on their next frame; the
    /// caller is expected to redraw, so that a cursor turned steady while in
    /// its hidden phase is drawn again. Call it from the thread that draws:
    /// the interval and timeout are stored separately, so a reader on another
    /// thread could see half of a change.
    pub fn set_preferred(blink: Option<Self>) {
        let nanos = |duration: Duration| {
            u64::try_from(duration.as_nanos()).map_or(UNSET - 1, |nanos| nanos.min(UNSET - 1))
        };
        let (interval, timeout) =
            blink.map_or((UNSET, UNSET), |blink| (nanos(blink.interval), nanos(blink.timeout)));
        PREFERRED_INTERVAL.store(interval, Ordering::Relaxed);
        PREFERRED_TIMEOUT.store(timeout, Ordering::Relaxed);
    }

    /// The desktop's blink when there is one, otherwise [`Blink::DEFAULT`].
    pub fn current() -> Self {
        Self::preferred().unwrap_or(Self::DEFAULT)
    }

    /// Whether the cursor is drawn `elapsed` after the last input.
    pub fn visible(&self, elapsed: Duration) -> bool {
        elapsed >= self.timeout
            || (elapsed.as_nanos() / self.interval.as_nanos().max(1)).is_multiple_of(2)
    }

    /// How long after `elapsed` the cursor next needs drawing, or `None`
    /// once it rests and needs no more frames.
    pub fn next_frame(&self, elapsed: Duration) -> Option<Duration> {
        let remaining = self.timeout.checked_sub(elapsed).filter(|left| !left.is_zero())?;
        let interval = self.interval.as_nanos().max(1);
        let phase = elapsed.as_nanos() % interval;
        let until_phase = Duration::from_nanos((interval - phase) as u64);
        Some(until_phase.min(remaining))
    }
}

/// Marks [`Blink::preferred`] unset; no duration reaches it.
const UNSET: u64 = u64::MAX;
static PREFERRED_INTERVAL: AtomicU64 = AtomicU64::new(UNSET);
static PREFERRED_TIMEOUT: AtomicU64 = AtomicU64::new(UNSET);

impl Default for Blink {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn a_cursor_alternates_until_the_timeout_then_rests_visible() {
        let blink = Blink::DEFAULT;
        assert!(blink.visible(MS(0)));
        assert!(blink.visible(MS(499)));
        assert!(!blink.visible(MS(500)));
        assert!(!blink.visible(MS(999)));
        assert!(blink.visible(MS(1000)));
        assert!(blink.visible(MS(10_000)));
        assert!(blink.visible(MS(10_700)));
        assert!(blink.visible(MS(3_600_000)));
    }

    #[test]
    fn frames_follow_each_phase_and_stop_at_the_timeout() {
        let blink = Blink::DEFAULT;
        assert_eq!(blink.next_frame(MS(0)), Some(MS(500)));
        assert_eq!(blink.next_frame(MS(120)), Some(MS(380)));
        assert_eq!(blink.next_frame(MS(9_800)), Some(MS(200)));
        assert_eq!(blink.next_frame(MS(10_000)), None);
        assert_eq!(blink.next_frame(MS(60_000)), None);
    }

    #[test]
    fn a_timeout_inside_a_hidden_phase_brings_the_cursor_back() {
        // 10 s falls 250 ms into the fourteenth 750 ms phase, which is hidden.
        let blink = Blink::new(MS(750));
        assert!(!blink.visible(MS(9_999)));
        assert_eq!(blink.next_frame(MS(9_900)), Some(MS(100)));
        assert!(blink.visible(MS(10_000)));
        assert_eq!(blink.next_frame(MS(10_000)), None);
    }

    #[test]
    fn a_steady_cursor_is_always_drawn_and_needs_no_frames() {
        let blink = Blink::STEADY;
        assert!(!blink.blinks());
        for elapsed in [0, 1, 499, 500, 750, 10_000] {
            assert!(blink.visible(MS(elapsed)));
            assert_eq!(blink.next_frame(MS(elapsed)), None);
        }
    }

    // The only test that touches the process-wide preference, so tests that
    // run alongside it never see it set.
    #[test]
    fn the_desktop_preference_replaces_the_default_until_forgotten() {
        assert_eq!(Blink::current(), Blink::DEFAULT);
        let desktop = Blink {
            interval: MS(600),
            timeout: Duration::from_secs(4),
        };
        Blink::set_preferred(Some(desktop));
        assert_eq!(Blink::preferred(), Some(desktop));
        assert_eq!(Blink::current(), desktop);
        Blink::set_preferred(Some(Blink::STEADY));
        assert!(!Blink::current().blinks());
        Blink::set_preferred(None);
        assert_eq!(Blink::preferred(), None);
        assert_eq!(Blink::current(), Blink::DEFAULT);
    }
}
