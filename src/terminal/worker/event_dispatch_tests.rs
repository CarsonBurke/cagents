//! The real semantic publication/observer code without subprocess owners.
//! These tests measure event delivery threads, not total terminal/PTy scaling.
use super::*;
use std::{collections::HashSet, sync::Barrier};

fn stream() -> (TerminalHandle, Arc<Sender<TerminalEvent>>) {
    let (commands, _) = command_channel(1);
    let (events, received) = crossbeam_channel::unbounded();
    let events = Arc::new(events);
    let published = Arc::new(PublishedState::new(false, Arc::new(|_| {})));
    published
        .semantic_events
        .set(Arc::downgrade(&events))
        .unwrap();
    let handle = TerminalHandle {
        inner: Arc::new(HandleInner {
            commands,
            events: received,
            worker: joins::JoinOwner::empty(published.clone()),
            published,
        }),
    };
    (handle, events)
}

#[test]
fn thirty_semantic_streams_share_one_callback_thread_and_preserve_startup_order() {
    let mut streams = Vec::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    for id in 0..30 {
        let (handle, events) = stream();
        handle
            .inner
            .published
            .emit_event(&events, TerminalEvent::TitleChanged("startup".into()));
        let received = received.clone();
        let expected = handle.clone();
        // Keep only a weak identity in the callback to mirror Host ownership.
        let expected = Arc::downgrade(&expected.inner);
        handle.set_event_observer(move |actual, event| {
            assert!(Arc::ptr_eq(&actual.inner, &expected.upgrade().unwrap()));
            received
                .lock()
                .unwrap()
                .push((id, event, thread::current().id()));
        });
        handle
            .inner
            .published
            .set_agent_activity(Some(crate::agent::Activity {
                provider: crate::agent::Provider::Claude,
                state: crate::agent::ActivityState::Working,
            }));
        handle
            .inner
            .published
            .emit_archived(&events, TerminalEvent::Bell);
        streams.push((handle, events));
    }
    flush_archived_notices();
    let received = received.lock().unwrap();
    assert_eq!(received.len(), 90);
    let workers: HashSet<_> = received.iter().map(|(_, _, worker)| *worker).collect();
    assert_eq!(
        workers.len(),
        1,
        "adding semantic streams must not add callback threads"
    );
    assert!(!workers.contains(&thread::current().id()));
    for id in 0..30 {
        let events: Vec<_> = received
            .iter()
            .filter(|(owner, _, _)| *owner == id)
            .map(|(_, event, _)| event)
            .collect();
        assert!(
            matches!(events.as_slice(), [TerminalEvent::TitleChanged(title),
            TerminalEvent::AgentActivityChanged(Some(_)), TerminalEvent::Bell] if title == "startup")
        );
    }
    assert!(streams.iter().all(|(handle, _)| handle.events().is_empty()));
}

#[test]
fn live_callbacks_can_take_the_senders_lock_and_another_terminals_stream() {
    let (handle, events) = stream();
    let (other, other_events) = stream();
    let subscriber_lock = Arc::new(Mutex::new(()));
    let callback_lock = subscriber_lock.clone();
    let (called, observed) = crossbeam_channel::bounded(1);
    handle.set_event_observer(move |_, _| {
        let _guard = callback_lock.lock().unwrap();
        // Callback code may publish another semantic event; it must hold no
        // stream publication lock and must not run inline under the sender.
        other
            .inner
            .published
            .emit_event(&other_events, TerminalEvent::Bell);
        called.send(()).unwrap();
    });
    let guard = subscriber_lock.lock().unwrap();
    handle
        .inner
        .published
        .emit_event(&events, TerminalEvent::Bell);
    assert!(observed.try_recv().is_err());
    drop(guard);
    observed.recv_timeout(Duration::from_secs(2)).unwrap();
    flush_archived_notices();
}

#[test]
fn observer_installation_racing_real_publication_neither_loses_nor_reorders_events() {
    const EVENTS: usize = 4096;
    let (handle, events) = stream();
    let publishing = handle.clone();
    let barrier = Arc::new(Barrier::new(2));
    let producer_barrier = barrier.clone();
    let producer = thread::spawn(move || {
        producer_barrier.wait();
        for sequence in 0..EVENTS {
            publishing
                .inner
                .published
                .emit_event(&events, TerminalEvent::TitleChanged(sequence.to_string()));
        }
    });
    let received = Arc::new(Mutex::new(Vec::with_capacity(EVENTS)));
    let saved = received.clone();
    barrier.wait();
    handle.set_event_observer(move |_, event| {
        if let TerminalEvent::TitleChanged(sequence) = event {
            saved
                .lock()
                .unwrap()
                .push(sequence.parse::<usize>().unwrap());
        } else {
            panic!("unexpected semantic event");
        }
    });
    producer.join().unwrap();
    flush_archived_notices();
    assert_eq!(*received.lock().unwrap(), (0..EVENTS).collect::<Vec<_>>());
    assert!(handle.events().is_empty());
}
