//! Timing of the frames a shell presents, for an application's own tracing.
//!
//! A shell that supports it reports each presented frame to the observer the
//! application installs, if any. Without one it reads no clock.
use std::cell::Cell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// One presented frame.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    /// Time the shell spent handling events since it presented the previous
    /// frame, up to presenting this one: updates, views, layout, drawing and
    /// presentation. Time spent waiting for events is excluded.
    pub work: Duration,
    /// Time since the shell presented the previous frame.
    pub interval: Option<Duration>,
}

static OBSERVER: OnceLock<fn(Frame)> = OnceLock::new();

/// Reports every frame presented from now on to `observer`, on the thread that
/// presents it. Only the first observer installed is kept.
pub fn observe(observer: fn(Frame)) {
    let _ = OBSERVER.set(observer);
}

/// Accounts a shell's event handling into frames, while an observer is set.
#[derive(Debug)]
pub struct Clock {
    observer: fn(Frame),
    busy: Cell<Duration>,
    since: Cell<Instant>,
    presented: Cell<Option<Instant>>,
}

impl Clock {
    /// A clock for the installed observer, if there is one.
    pub fn new() -> Option<Self> {
        OBSERVER.get().map(|&observer| Self::reporting_to(observer))
    }

    /// A clock that reports to `observer`, whichever is installed.
    pub fn reporting_to(observer: fn(Frame)) -> Self {
        Self {
            observer,
            busy: Cell::new(Duration::ZERO),
            since: Cell::new(Instant::now()),
            presented: Cell::new(None),
        }
    }

    /// Counts the time until the returned guard drops as event handling.
    pub fn handling(&self) -> Handling<'_> {
        self.since.set(Instant::now());
        Handling(self)
    }

    /// Reports the frame just presented. The rest of the current event's
    /// handling counts towards the next frame.
    pub fn presented(&self) {
        let now = Instant::now();
        let work =
            self.busy.take() + now.saturating_duration_since(self.since.get());
        self.since.set(now);
        let interval = self
            .presented
            .replace(Some(now))
            .map(|previous| now.saturating_duration_since(previous));
        (self.observer)(Frame { work, interval });
    }
}

/// Event handling in progress; see [`Clock::handling`].
#[derive(Debug)]
pub struct Handling<'a>(&'a Clock);

impl Drop for Handling<'_> {
    fn drop(&mut self) {
        let clock = self.0;
        clock
            .busy
            .set(clock.busy.get() + clock.since.get().elapsed());
    }
}
