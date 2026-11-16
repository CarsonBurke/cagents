//! The daemon-wide control admission gate.
//!
//! Requests and background effects hold it shared while they act; upgrade,
//! shutdown, close and inventory commits hold it exclusively so they see every
//! effect admitted before them and none while they run.
//!
//! Admission is not a plain reader-writer lock. With a writer-preferring lock,
//! an exclusive request waiting behind one slow shared request (a GitHub read,
//! a provider probe) also holds back every request that arrives after it,
//! including keystrokes to every terminal. A reader-preferring lock, or a
//! writer that only polls for a free gate, lets a steady stream of readers
//! starve shutdown instead. So the gate separates two kinds of shared holder:
//!
//! - Ordinary requests yield to a waiting exclusive request, so it is admitted
//!   as soon as the requests already inside finish.
//! - Terminal I/O, which is short and launches nothing, only waits while an
//!   exclusive holder is inside, never while one waits.
//!
//! A waiting exclusive request gives up at its deadline, so ordinary requests
//! wait for it at most that long. Only daemon exit, which has to win, waits
//! without one.
//!
//! The gate sits first in the daemon's lock order: control gate, agent gate,
//! registry, HostState. Its own mutex is held only to update the counts, never
//! while acquiring anything else. A thread already holding a shared admission
//! must not take another: an exclusive request waiting between the two would
//! wait for the first while the second waits for it.

use std::{
    sync::{Condvar, Mutex, MutexGuard},
    time::Instant,
};

use crate::terminal_ipc::{ProtocolError, ProtocolErrorCode};

#[derive(Debug, Default)]
struct Counts {
    shared: usize,
    exclusive: bool,
    /// Exclusive requests waiting for the gate; ordinary requests yield to them.
    waiting: usize,
}

impl Counts {
    fn admits_exclusive(&self) -> bool {
        !self.exclusive && self.shared == 0
    }
}

#[derive(Debug, Default)]
pub(super) struct ControlGate {
    counts: Mutex<Counts>,
    changed: Condvar,
}

/// Shared admission, released on drop.
#[derive(Debug)]
#[must_use = "admission ends when the guard is dropped"]
pub(super) struct Shared<'gate>(&'gate ControlGate);

/// Exclusive admission, released on drop.
#[derive(Debug)]
#[must_use = "admission ends when the guard is dropped"]
pub(super) struct Exclusive<'gate>(&'gate ControlGate);

impl ControlGate {
    fn counts(&self) -> MutexGuard<'_, Counts> {
        // Nothing panics while the counts are held, and every guard releases
        // its admission on unwind, so a poisoned mutex still has exact counts.
        self.counts.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn wait<'a>(&self, counts: MutexGuard<'a, Counts>) -> MutexGuard<'a, Counts> {
        self.changed.wait(counts).unwrap_or_else(|p| p.into_inner())
    }

    fn enter_shared(&self, yields: bool) -> Shared<'_> {
        let mut counts = self.counts();
        while counts.exclusive || (yields && counts.waiting > 0) {
            counts = self.wait(counts);
        }
        counts.shared += 1;
        Shared(self)
    }

    /// Ordinary shared admission. Waits while an exclusive request holds the
    /// gate or is waiting for it.
    pub(super) fn shared(&self) -> Shared<'_> {
        self.enter_shared(true)
    }

    /// Shared admission for terminal I/O. Waits only while an exclusive
    /// request holds the gate, so a waiting one never delays a keystroke.
    pub(super) fn terminal_io(&self) -> Shared<'_> {
        self.enter_shared(false)
    }

    /// Ordinary shared admission only if it would not wait.
    pub(super) fn try_shared(&self) -> Option<Shared<'_>> {
        let mut counts = self.counts();
        if counts.exclusive || counts.waiting > 0 {
            return None;
        }
        counts.shared += 1;
        Some(Shared(self))
    }

    /// Exclusive admission only if the gate is free now.
    pub(super) fn try_exclusive(&self) -> Option<Exclusive<'_>> {
        let mut counts = self.counts();
        if !counts.admits_exclusive() {
            return None;
        }
        counts.exclusive = true;
        Some(Exclusive(self))
    }

    /// Exclusive admission once the shared holders inside have left, or `None`
    /// at `deadline`. While it waits, new ordinary requests wait for it.
    pub(super) fn exclusive_until(&self, deadline: Instant) -> Option<Exclusive<'_>> {
        self.enter_exclusive(Some(deadline))
    }

    /// Exclusive admission without a deadline. Only for daemon exit, which
    /// must win and has no client to ask to retry.
    pub(super) fn exclusive(&self) -> Exclusive<'_> {
        self.enter_exclusive(None)
            .expect("an exclusive wait without a deadline cannot expire")
    }

    fn enter_exclusive(&self, deadline: Option<Instant>) -> Option<Exclusive<'_>> {
        let mut counts = self.counts();
        counts.waiting += 1;
        loop {
            if counts.admits_exclusive() {
                counts.waiting -= 1;
                counts.exclusive = true;
                return Some(Exclusive(self));
            }
            counts = match deadline {
                None => self.wait(counts),
                Some(deadline) => {
                    let Some(remaining) = deadline
                        .checked_duration_since(Instant::now())
                        .filter(|remaining| !remaining.is_zero())
                    else {
                        break;
                    };
                    self.changed
                        .wait_timeout(counts, remaining)
                        .unwrap_or_else(|p| p.into_inner())
                        .0
                }
            };
        }
        counts.waiting -= 1;
        let released = counts.waiting == 0;
        drop(counts);
        if released {
            // Ordinary requests held back for this one may enter now.
            self.changed.notify_all();
        }
        None
    }
}

impl Drop for Shared<'_> {
    fn drop(&mut self) {
        let mut counts = self.0.counts();
        counts.shared -= 1;
        let empty = counts.shared == 0 && counts.waiting > 0;
        drop(counts);
        if empty {
            self.0.changed.notify_all();
        }
    }
}

impl Drop for Exclusive<'_> {
    fn drop(&mut self) {
        self.0.counts().exclusive = false;
        self.0.changed.notify_all();
    }
}

/// The retryable refusal for an exclusive request the gate did not admit.
pub(super) fn busy() -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::Internal,
        "The daemon is finishing another operation. Wait for it and retry.",
        true,
    )
}

#[cfg(test)]
mod tests;
