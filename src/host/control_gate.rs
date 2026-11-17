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
//! - Ordinary requests yield to a queued exclusive request, so it is admitted
//!   as soon as the requests already inside finish.
//! - Terminal I/O, which launches nothing and holds the gate only for a
//!   HostState update and a send to a terminal worker, does not wait behind
//!   the requests a queued exclusive request is waiting for. It waits only
//!   while an exclusive holder is inside, and at the moment the gate empties
//!   with one queued, so the queued request takes the first empty gate
//!   instead of losing it to the next keystroke. No client request reaches a
//!   terminal worker while an exclusive request holds the gate.
//!
//! A connection's requests run one at a time, so a request that waits ahead of
//! a keystroke on the same connection delays it just as surely. The snapshot a
//! redraw asks for is therefore admitted as terminal I/O only while its read is
//! queued to the worker, and waits for the frame outside the gate.
//!
//! Exclusive requests choose how they wait:
//!
//! - Operations that must eventually win (shutdown, upgrade, chat release,
//!   return and account recovery, daemon exit) queue with
//!   [`ControlGate::exclusive_until`], holding back new ordinary requests
//!   until their deadline.
//! - Routine operations (closing a terminal, reserving a worktree removal)
//!   wait with [`ControlGate::exclusive_when_free_until`] without queuing, so
//!   they never hold anyone back; under steady traffic they may refuse at
//!   their deadline instead.
//!
//! Every wait but daemon exit's, which has no client to ask to retry, ends at
//! a deadline.
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

use crate::terminal_ipc::{DAEMON_BUSY, ProtocolError, ProtocolErrorCode};

#[derive(Debug, Default)]
struct Counts {
    shared: usize,
    exclusive: bool,
    /// Queued exclusive requests; ordinary requests yield to them.
    queued: usize,
    /// Exclusive requests waiting without queuing; nobody yields to them.
    unqueued: usize,
}

impl Counts {
    fn admits_exclusive(&self) -> bool {
        !self.exclusive && self.shared == 0
    }

    fn admits_ordinary(&self) -> bool {
        !self.exclusive && self.queued == 0
    }

    /// An empty gate with a request queued is that request's to take.
    fn admits_terminal_io(&self) -> bool {
        !self.exclusive && !(self.queued > 0 && self.shared == 0)
    }

    fn waiters(&mut self, queued: bool) -> &mut usize {
        if queued {
            &mut self.queued
        } else {
            &mut self.unqueued
        }
    }

    fn exclusive_waiters(&self) -> bool {
        self.queued > 0 || self.unqueued > 0
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

    fn enter_shared(&self, admits: fn(&Counts) -> bool) -> Shared<'_> {
        let mut counts = self.counts();
        while !admits(&counts) {
            counts = self.wait(counts);
        }
        counts.shared += 1;
        Shared(self)
    }

    /// Ordinary shared admission. Waits while an exclusive request holds the
    /// gate or is queued for it.
    pub(super) fn shared(&self) -> Shared<'_> {
        self.enter_shared(Counts::admits_ordinary)
    }

    /// Shared admission for terminal I/O. Never waits behind the requests a
    /// queued exclusive request is waiting for, only for that request itself.
    pub(super) fn terminal_io(&self) -> Shared<'_> {
        self.enter_shared(Counts::admits_terminal_io)
    }

    /// Ordinary shared admission only if it would not wait.
    pub(super) fn try_shared(&self) -> Option<Shared<'_>> {
        let mut counts = self.counts();
        if !counts.admits_ordinary() {
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

    /// Queued exclusive admission once the shared holders inside have left, or
    /// `None` at `deadline`. While it waits, new ordinary requests wait for it.
    pub(super) fn exclusive_until(&self, deadline: Instant) -> Option<Exclusive<'_>> {
        self.enter_exclusive(Some(deadline), true)
    }

    /// Exclusive admission whenever the gate is next free, or `None` at
    /// `deadline`. It holds back no other request while it waits.
    pub(super) fn exclusive_when_free_until(&self, deadline: Instant) -> Option<Exclusive<'_>> {
        self.enter_exclusive(Some(deadline), false)
    }

    /// Queued exclusive admission without a deadline. Only for daemon exit,
    /// which must win and has no client to ask to retry.
    pub(super) fn exclusive(&self) -> Exclusive<'_> {
        self.enter_exclusive(None, true)
            .expect("an exclusive wait without a deadline cannot expire")
    }

    fn enter_exclusive(&self, deadline: Option<Instant>, queue: bool) -> Option<Exclusive<'_>> {
        let mut counts = self.counts();
        *counts.waiters(queue) += 1;
        loop {
            if counts.admits_exclusive() {
                *counts.waiters(queue) -= 1;
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
        *counts.waiters(queue) -= 1;
        let released = queue && counts.queued == 0;
        drop(counts);
        if released {
            // Requests held back for this one may enter now.
            self.changed.notify_all();
        }
        None
    }
}

impl Drop for Shared<'_> {
    fn drop(&mut self) {
        let mut counts = self.0.counts();
        counts.shared -= 1;
        let empty = counts.shared == 0 && counts.exclusive_waiters();
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
    ProtocolError::new(ProtocolErrorCode::Internal, DAEMON_BUSY, true)
}

#[cfg(test)]
mod tests;
