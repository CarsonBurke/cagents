//! The queue of runtime events between the [`Tracker`] and one
//! subscription's stream.
//!
//! The upstream tracker gave each subscription a channel of 100 events and
//! dropped events it could not queue. A listener's stream stops reading
//! while it waits for the runtime to accept its last message, so a busy
//! main thread made it lose key presses and window events (cagents #3644).
//!
//! This queue never drops an event. It only grows while a stream is stalled
//! that way, and it replaces a queued redraw or cursor move with a newer one
//! of the same kind, window and status when nothing came between them, since
//! a listener only needs the latest of each.
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
    pub fn send(&mut self, event: Event) {
        let mut shared = lock(&self.0);

        if !shared.receiving {
            return;
        }

        match shared.events.back_mut() {
            Some(last) if supersedes(&event, last) => *last = event,
            _ => shared.events.push_back(event),
        }

        let queued = shared.events.len();

        if queued >= shared.warn_at {
            shared.warn_at = queued * 2;

            log::warn!(
                "A subscription has {queued} runtime events waiting; \
                 its stream is waiting on the runtime"
            );
        } else if queued == 1 {
            shared.warn_at = BACKLOG_WARNING;
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

/// Whether `next` makes the `queued` event before it redundant: both are
/// redraws, or both cursor moves, in the same window with the same status.
fn supersedes(next: &Event, queued: &Event) -> bool {
    let (
        Event::Interaction {
            window: next_window,
            event: next_event,
            status: next_status,
        },
        Event::Interaction {
            window: queued_window,
            event: queued_event,
            status: queued_status,
        },
    ) = (next, queued)
    else {
        return false;
    };

    next_window == queued_window
        && next_status == queued_status
        && matches!(
            (next_event, queued_event),
            (
                event::Event::Window(window::Event::RedrawRequested(_)),
                event::Event::Window(window::Event::RedrawRequested(_)),
            ) | (
                event::Event::Mouse(mouse::Event::CursorMoved { .. }),
                event::Event::Mouse(mouse::Event::CursorMoved { .. }),
            )
        )
}
