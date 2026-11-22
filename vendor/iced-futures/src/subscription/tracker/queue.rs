//! The queue of runtime events between the [`Tracker`] and one
//! subscription's stream.
//!
//! The upstream tracker gave each subscription a channel of 100 events and
//! dropped events it could not queue. A listener's stream stops reading
//! while it waits for the runtime to accept its last message, so a busy
//! main thread made it lose key presses and window events (cagents #3644).
//!
//! This queue never drops an event; it grows while its stream is not
//! reading. Redraws and cursor moves are coalesced: a new one replaces a
//! queued one of the same kind, window and status if only other redraws and
//! cursor moves were queued after it. Every listener in cagents and
//! libcosmic needs only the latest of each, and no other kind of event
//! passes one.
//!
//! [`Tracker`]: super::Tracker

use crate::core::event;
use crate::core::mouse;
use crate::core::window;
use crate::subscription::Event;

use futures::Stream;

use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

/// How many queued events make the queue log that its stream is far behind.
/// It logs again each time the backlog doubles past this.
const BACKLOG_WARNING: usize = 1024;

/// Creates a queue whose [`Receiver`] ends once its [`Sender`] is dropped.
pub fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Mutex::new(Shared {
        events: VecDeque::new(),
        waker: None,
        sending: true,
        receiving: true,
        warn_at: BACKLOG_WARNING,
    }));

    (Sender(shared.clone()), Receiver(shared))
}

struct Shared {
    events: VecDeque<Event>,
    waker: Option<Waker>,
    sending: bool,
    receiving: bool,
    warn_at: usize,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The tracker's end of a subscription's event queue.
pub struct Sender(Arc<Mutex<Shared>>);

impl Sender {
    /// Queues an event for the stream, unless the stream has dropped its
    /// end.
    pub fn send(&self, event: Event) {
        let mut shared = lock(&self.0);

        if !shared.receiving {
            return;
        }

        if let Some(key) = Coalesce::of(&event) {
            let superseded = shared
                .events
                .iter()
                .rev()
                .map_while(Coalesce::of)
                .position(|queued| queued == key);

            if let Some(from_back) = superseded {
                let index = shared.events.len() - 1 - from_back;
                let _ = shared.events.remove(index);
            }
        }

        shared.events.push_back(event);

        let queued = shared.events.len();

        if queued >= shared.warn_at {
            shared.warn_at = queued * 2;

            log::warn!(
                "A subscription has {queued} runtime events waiting; \
                 its stream has not read them"
            );
        }

        let waker = shared.waker.take();
        drop(shared);

        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Whether the stream has dropped its end, so events need not be sent.
    pub fn is_closed(&self) -> bool {
        !lock(&self.0).receiving
    }
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shared = lock(&self.0);

        f.debug_struct("Sender")
            .field("queued", &shared.events.len())
            .field("closed", &!shared.receiving)
            .finish()
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let mut shared = lock(&self.0);
        shared.sending = false;
        let waker = shared.waker.take();
        drop(shared);

        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// A subscription's stream of runtime events.
pub struct Receiver(Arc<Mutex<Shared>>);

impl Stream for Receiver {
    type Item = Event;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Event>> {
        let mut shared = lock(&self.0);

        if let Some(event) = shared.events.pop_front() {
            if shared.events.is_empty() {
                shared.warn_at = BACKLOG_WARNING;
            }

            return Poll::Ready(Some(event));
        }

        if !shared.sending {
            return Poll::Ready(None);
        }

        match &mut shared.waker {
            Some(waker) => waker.clone_from(cx.waker()),
            waker @ None => *waker = Some(cx.waker().clone()),
        }

        Poll::Pending
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let mut shared = lock(&self.0);
        shared.receiving = false;
        shared.events = VecDeque::new();
    }
}

/// What identifies a redraw or cursor move that a newer one replaces.
#[derive(Clone, Copy, PartialEq)]
struct Coalesce {
    window: window::Id,
    status: event::Status,
    redraw: bool,
}

impl Coalesce {
    fn of(event: &Event) -> Option<Self> {
        let Event::Interaction {
            window,
            event,
            status,
        } = event
        else {
            return None;
        };

        let redraw = match event {
            event::Event::Window(window::Event::RedrawRequested(_)) => true,
            event::Event::Mouse(mouse::Event::CursorMoved { .. }) => false,
            _ => return None,
        };

        Some(Self {
            window: *window,
            status: *status,
            redraw,
        })
    }
}
