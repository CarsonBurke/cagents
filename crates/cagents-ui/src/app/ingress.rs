//! Takes what a background source has queued as one update.
//!
//! The runtime already folds every message that waits for it into one view
//! and layout, but it still updates once per message. For a source whose
//! updates each end with the same whole-window work, such as the shared
//! model's reconciliation, a burst that queued while the window was busy
//! should cost that work once.
//!
//! A subscription that forwards each item as it arrives cannot do that: it
//! takes items off the channel as soon as they are sent, so they queue as
//! separate messages instead. [`gated`] forwards one item and then takes no
//! more until the update that receives it has [drained](Arrival::drain) the
//! channel, so everything sent in between waits in the channel and arrives
//! with it, in the order it was sent. The first item after a quiet spell is
//! forwarded at once: nothing waits for a timer.
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use cosmic::iced::futures::Stream;
use tokio::sync::Semaphore;

/// Opens the gate again when the last copy of an [`Arrival`] goes.
struct Held(Arc<Semaphore>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

/// The first item of a run, holding its source until the run is drained.
///
/// Dropping it without draining also lets the source go on, so a message
/// that is never handled cannot stall its source.
#[derive(Clone)]
pub struct Arrival<T> {
    first: T,
    held: Arc<Held>,
}

impl<T: fmt::Debug> fmt::Debug for Arrival<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Arrival").field(&self.first).finish()
    }
}

impl<T> Arrival<T> {
    /// The first item and every item sent after it that is waiting now, in
    /// the order they were sent. The source forwards its next item only
    /// after this returns.
    pub(super) fn drain(self, receiver: &async_channel::Receiver<T>) -> Vec<T> {
        let mut run = vec![self.first];
        run.extend(std::iter::from_fn(|| receiver.try_recv().ok()));
        // An extra copy of the message would hold the gate until it went
        // too; the runtime keeps none.
        drop(self.held);
        run
    }

    /// An arrival with no source behind it, for tests that deliver items
    /// themselves.
    #[cfg(test)]
    pub(super) fn detached(first: T) -> Self {
        Self {
            first,
            held: Arc::new(Held(Arc::new(Semaphore::new(0)))),
        }
    }
}

/// Forwards `receiver`'s items one run at a time: each item it yields waits
/// in the update that drains its run, and the next is taken only after that.
pub(super) fn gated<T: Send + 'static>(
    receiver: async_channel::Receiver<T>,
) -> Pin<Box<dyn Stream<Item = Arrival<T>> + Send>> {
    let gate = Arc::new(Semaphore::new(1));
    Box::pin(cosmic::iced::futures::stream::unfold(
        (receiver, gate),
        |(receiver, gate)| async move {
            // The permit comes back when the arrival's run is drained.
            gate.acquire().await.ok()?.forget();
            let held = Arc::new(Held(gate.clone()));
            let first = receiver.recv().await.ok()?;
            Some((Arrival { first, held }, (receiver, gate)))
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;
    use cosmic::iced::futures::executor::block_on;
    use cosmic::iced::futures::future::FutureExt;

    #[test]
    fn a_run_takes_what_was_sent_while_its_first_item_waited_in_order() {
        let (sender, receiver) = async_channel::unbounded();
        let mut stream = gated(receiver.clone());
        sender.send_blocking(1).unwrap();
        let arrival = block_on(stream.next()).unwrap();
        // Sent while the first waits for its update: the stream takes none
        // of them, so they queue behind it rather than as messages of their
        // own.
        for item in 2..=4 {
            sender.send_blocking(item).unwrap();
        }
        assert!(
            stream.next().now_or_never().is_none(),
            "nothing more is forwarded before the run is drained"
        );
        assert_eq!(arrival.drain(&receiver), [1, 2, 3, 4]);
        sender.send_blocking(5).unwrap();
        assert_eq!(
            block_on(stream.next()).unwrap().drain(&receiver),
            [5],
            "after a drain, the next item goes at once"
        );
    }

    #[test]
    fn an_arrival_dropped_undrained_lets_the_source_go_on() {
        let (sender, receiver) = async_channel::unbounded();
        let mut stream = gated(receiver.clone());
        sender.send_blocking(1).unwrap();
        sender.send_blocking(2).unwrap();
        let first = block_on(stream.next()).unwrap();
        let copy = first.clone();
        drop(first);
        assert!(
            stream.next().now_or_never().is_none(),
            "a copy still holds the source"
        );
        drop(copy);
        assert_eq!(block_on(stream.next()).unwrap().drain(&receiver), [2]);
    }
}
