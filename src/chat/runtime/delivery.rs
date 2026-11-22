//! Sends a message once, however often its sender asks.
//!
//! A message whose acknowledgement does not come in time may still reach the
//! provider, so its sender cannot tell whether to send it again. Each send
//! instead carries a key its sender chose, and the actor remembers what became
//! of each recent key. Asking again with the same key waits on the original
//! send rather than sending another: it reports the original's outcome once it
//! is known, and sends the message again only when the original was refused,
//! which is the one outcome that proves it never arrived.
use super::Rejection;
use crate::chat::DeliveryKey;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

/// The acknowledgement of one admitted command, once it comes.
pub(crate) type Reply = crossbeam_channel::Receiver<Result<(), Rejection>>;

/// How many recent keys an actor remembers. A sender asks again about a
/// message it is still showing, which is among its last few.
const KEPT: usize = 64;

/// What became of the send under one key.
enum Delivery {
    /// A caller is sending it or waiting for it now.
    Sending,
    /// Admitted, and its acknowledgement has not come yet.
    Waiting(Reply),
    /// The provider took it.
    Delivered,
    /// Its outcome can no longer be learned.
    Unknown(String),
}

/// The keys an actor was asked to send under, and what became of each.
#[derive(Default)]
pub(crate) struct Deliveries {
    kept: Mutex<VecDeque<(DeliveryKey, Delivery)>>,
}

impl Deliveries {
    /// Sends what `admit` admits once under `key`, waiting `wait` for its
    /// acknowledgement, and answers with `unknown` when none comes in that
    /// time. A key asked about before waits on that send instead of
    /// admitting another, unless it was refused.
    pub(crate) fn deliver(
        &self,
        key: DeliveryKey,
        wait: Duration,
        unknown: impl Fn() -> Rejection,
        admit: impl FnOnce() -> Result<Reply, Rejection>,
    ) -> Result<(), Rejection> {
        let earlier = {
            let mut kept = self.kept.lock().unwrap_or_else(|p| p.into_inner());
            match kept.iter_mut().find(|(kept, _)| *kept == key) {
                Some((_, delivery)) => match std::mem::replace(delivery, Delivery::Sending) {
                    Delivery::Waiting(reply) => Some(reply),
                    known => {
                        let answer = match &known {
                            Delivery::Delivered => Ok(()),
                            Delivery::Unknown(error) => {
                                Err(Rejection::Unacknowledged(error.clone()))
                            }
                            // Another caller is sending it right now.
                            Delivery::Sending | Delivery::Waiting(_) => Err(unknown()),
                        };
                        *delivery = known;
                        return answer;
                    }
                },
                None => {
                    // The oldest settled send gives way first: forgetting
                    // one still unacknowledged would let it go twice.
                    if kept.len() >= KEPT
                        && let Some(oldest) = kept
                            .iter()
                            .position(|(_, delivery)| {
                                matches!(delivery, Delivery::Delivered | Delivery::Unknown(_))
                            })
                            .or_else(|| {
                                kept.iter().position(|(_, delivery)| {
                                    matches!(delivery, Delivery::Waiting(_))
                                })
                            })
                    {
                        kept.remove(oldest);
                    }
                    kept.push_back((key, Delivery::Sending));
                    None
                }
            }
        };
        let outcome = match earlier.map(|reply| Self::await_reply(reply, wait)) {
            // The earlier send never arrived, so this one is the first.
            Some(Ok(Err(Rejection::Refused(_)))) | None => {
                admit().map(|reply| Self::await_reply(reply, wait))
            }
            Some(outcome) => Ok(outcome),
        };
        let (delivery, result) = match outcome {
            Err(refusal) => (None, Err(refusal)),
            Ok(Ok(Ok(()))) => (Some(Delivery::Delivered), Ok(())),
            Ok(Ok(Err(Rejection::Refused(error)))) => (None, Err(Rejection::Refused(error))),
            Ok(Ok(Err(Rejection::Unacknowledged(error)))) => (
                Some(Delivery::Unknown(error.clone())),
                Err(Rejection::Unacknowledged(error)),
            ),
            Ok(Err(Some(reply))) => (Some(Delivery::Waiting(reply)), Err(unknown())),
            Ok(Err(None)) => {
                let (Rejection::Unacknowledged(error) | Rejection::Refused(error)) = unknown();
                (
                    Some(Delivery::Unknown(error.clone())),
                    Err(Rejection::Unacknowledged(error)),
                )
            }
        };
        let mut kept = self.kept.lock().unwrap_or_else(|p| p.into_inner());
        match delivery {
            Some(delivery) => {
                if let Some((_, kept)) = kept.iter_mut().find(|(kept, _)| *kept == key) {
                    *kept = delivery;
                }
            }
            // A refused send can be sent again as new.
            None => kept.retain(|(kept, _)| *kept != key),
        }
        result
    }

    /// The acknowledgement `reply` brings within `wait`, or else the reply
    /// still to be waited on, or none where its sender is gone.
    fn await_reply(reply: Reply, wait: Duration) -> Result<Result<(), Rejection>, Option<Reply>> {
        match reply.recv_timeout(wait) {
            Ok(result) => Ok(result),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Err(Some(reply)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const WAIT: Duration = Duration::from_millis(20);

    fn unknown() -> Rejection {
        Rejection::Unacknowledged("not confirmed".into())
    }

    /// A send that is admitted, and the sender of its acknowledgement.
    fn admitted() -> (
        crossbeam_channel::Sender<Result<(), Rejection>>,
        impl FnOnce() -> Result<Reply, Rejection>,
    ) {
        let (sender, reply) = crossbeam_channel::bounded(1);
        (sender, move || Ok(reply))
    }

    #[test]
    fn a_send_asked_for_again_waits_on_the_first_instead_of_sending_twice() {
        let deliveries = Deliveries::default();
        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        assert_eq!(
            deliveries.deliver(key, WAIT, unknown, admit),
            Err(unknown())
        );
        let sent = Cell::new(0);
        let again = || {
            sent.set(sent.get() + 1);
            Err(Rejection::Refused("sent twice".into()))
        };
        // Still unacknowledged: asking again reports it unknown again.
        assert_eq!(
            deliveries.deliver(key, WAIT, unknown, again),
            Err(unknown())
        );
        acknowledge.send(Ok(())).unwrap();
        let again = || {
            sent.set(sent.get() + 1);
            Err(Rejection::Refused("sent twice".into()))
        };
        assert_eq!(deliveries.deliver(key, WAIT, unknown, again), Ok(()));
        let again = || {
            sent.set(sent.get() + 1);
            Err(Rejection::Refused("sent twice".into()))
        };
        assert_eq!(deliveries.deliver(key, WAIT, unknown, again), Ok(()));
        assert_eq!(sent.get(), 0);
    }

    #[test]
    fn a_refused_send_is_sent_again_when_asked_for_again() {
        let deliveries = Deliveries::default();
        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        assert_eq!(
            deliveries.deliver(key, WAIT, unknown, admit),
            Err(unknown())
        );
        acknowledge
            .send(Err(Rejection::Refused("not sent".into())))
            .unwrap();
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(deliveries.deliver(key, WAIT, unknown, admit), Ok(()));

        let key = DeliveryKey::new_v4();
        let refuse = || Err(Rejection::Refused("busy".into()));
        assert_eq!(
            deliveries.deliver(key, WAIT, unknown, refuse),
            Err(Rejection::Refused("busy".into()))
        );
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(deliveries.deliver(key, WAIT, unknown, admit), Ok(()));
    }

    #[test]
    fn an_outcome_that_can_no_longer_be_learned_is_never_sent_again() {
        let deliveries = Deliveries::default();
        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        drop(acknowledge);
        assert_eq!(
            deliveries.deliver(key, WAIT, unknown, admit),
            Err(unknown())
        );
        let resend = || -> Result<Reply, Rejection> { panic!("sent twice") };
        assert_eq!(
            deliveries.deliver(key, WAIT, unknown, resend),
            Err(unknown())
        );

        let key = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        let stopped = Rejection::Unacknowledged("stopped".into());
        acknowledge.send(Err(stopped.clone())).unwrap();
        assert_eq!(
            deliveries.deliver(key, WAIT, unknown, admit),
            Err(stopped.clone())
        );
        let resend = || -> Result<Reply, Rejection> { panic!("sent twice") };
        assert_eq!(deliveries.deliver(key, WAIT, unknown, resend), Err(stopped));
    }

    #[test]
    fn only_recent_keys_are_remembered() {
        let deliveries = Deliveries::default();
        let first = DeliveryKey::new_v4();
        let (acknowledge, admit) = admitted();
        acknowledge.send(Ok(())).unwrap();
        assert_eq!(deliveries.deliver(first, WAIT, unknown, admit), Ok(()));
        for _ in 0..KEPT {
            let (acknowledge, admit) = admitted();
            acknowledge.send(Ok(())).unwrap();
            assert_eq!(
                deliveries.deliver(DeliveryKey::new_v4(), WAIT, unknown, admit),
                Ok(())
            );
        }
        assert_eq!(
            deliveries.kept.lock().unwrap().len(),
            KEPT,
            "the oldest key gives way"
        );
        assert!(
            !deliveries
                .kept
                .lock()
                .unwrap()
                .iter()
                .any(|(key, _)| *key == first)
        );
    }
}
