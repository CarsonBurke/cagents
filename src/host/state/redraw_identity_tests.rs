//! Real shared callback/publication machinery and Host replacement transitions,
//! without a PTY or child. These are identity races, not native lifecycle tests.
use super::*;
use crate::{
    model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
    terminal::RenderStreamFixture,
};

pub(super) fn session(id: TerminalId) -> TerminalSession {
    let recipe = LaunchRecipe {
        program: "/bin/true".into(),
        arguments: Vec::new(),
        working_directory: PathBuf::from("/"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id,
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        owned_codex_listener: None,
        transcript_path: None,
        resume_hint: None,
        recovery_origin: None,
        predecessor: None,
    }
}

fn host() -> SharedHost {
    Arc::new(HostLock::new(HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("redraw-test"),
        boot_id: BootId::new("redraw-boot"),
        host_incarnation: HostIncarnation::new("redraw-incarnation"),
    })))
}

type PendingRedraws = crossbeam_channel::Receiver<Weak<TerminalRedraw>>;

fn stream(
    shared: &SharedHost,
    id: TerminalId,
) -> (RenderStreamFixture, Arc<TerminalRedraw>, PendingRedraws) {
    let (notify, pending) = crossbeam_channel::bounded(1);
    let redraw = Arc::new(TerminalRedraw::new(
        Arc::downgrade(shared),
        id,
        Some(notify),
    ));
    let wake = Arc::clone(&redraw);
    (
        RenderStreamFixture::new(move |edge| wake.publish(edge.generation)),
        redraw,
        pending,
    )
}

fn deliver(pending: &PendingRedraws) {
    pending
        .try_recv()
        .unwrap()
        .upgrade()
        .unwrap()
        .deliver_pending();
    assert!(pending.is_empty());
}

fn install_visible(
    shared: &SharedHost,
    id: TerminalId,
    stream: &RenderStreamFixture,
    redraw: &TerminalRedraw,
) -> Arc<DeliveryQueue> {
    assert!(redraw.worker.set(stream.handle.downgrade()).is_ok());
    let mut state = shared.lock().unwrap();
    let delivery = state.register_client(1);
    state.reserve_spawn(id).unwrap();
    state
        .finish_spawn(1, session(id), stream.handle.clone(), true)
        .unwrap();
    delivery
}

struct ReleaseOnDrop(crossbeam_channel::Sender<()>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

#[test]
fn a_pulled_capture_from_a_replaced_worker_is_not_accepted() {
    let shared = host();
    let id = TerminalId::new();
    let (old, _, _) = stream(&shared, id);
    old.publish(100);
    let mut state = shared.lock().unwrap();
    state.register_client(1);
    state.reserve_spawn(id).unwrap();
    state
        .finish_spawn(1, session(id), old.handle.clone(), true)
        .unwrap();
    let handle_a = state.history_handle(1, id).unwrap();
    state.presentation_revision(1, id);
    let epoch = state.subscriptions.viewports[&(1, id)].epoch;
    assert_eq!(epoch, 0);
    let source = handle_a.generation();

    let (successor, _, _) = stream(&shared, id);
    successor.publish(1);
    let retired = state.retire_terminal(id).unwrap();
    assert!(retired.handle.same_worker(&handle_a));
    state.reserve_spawn(id).unwrap();
    state
        .finish_spawn(1, session(id), successor.handle.clone(), true)
        .unwrap();
    state.attach(1, id).unwrap();
    assert!(state.subscriptions.attached(1, id));
    assert!(!state.terminals[&id].handle.same_worker(&handle_a));
    assert!(!state.subscriptions.viewports.contains_key(&(1, id)));

    assert_eq!(
        presentation::accept_pulled_capture(&mut state, 1, id, &handle_a, epoch, source).unwrap(),
        None
    );
    assert!(!state.subscriptions.viewports.contains_key(&(1, id)));
}

#[test]
fn a_pulled_capture_from_the_current_worker_is_accepted() {
    let shared = host();
    let id = TerminalId::new();
    let (current, _, _) = stream(&shared, id);
    current.publish(100);
    let mut state = shared.lock().unwrap();
    state.register_client(1);
    state.reserve_spawn(id).unwrap();
    state
        .finish_spawn(1, session(id), current.handle.clone(), true)
        .unwrap();
    let handle = state.history_handle(1, id).unwrap();
    let revision = state.presentation_revision(1, id);
    let epoch = state.subscriptions.viewports[&(1, id)].epoch;
    assert_eq!(epoch, 0);
    let source = handle.generation();

    assert_eq!(
        presentation::accept_pulled_capture(&mut state, 1, id, &handle, epoch, source).unwrap(),
        Some(revision)
    );
}

#[test]
fn held_old_completion_and_wake_cannot_advance_or_hide_a_same_id_successor() {
    for visible in [true, false] {
        let shared = host();
        let id = TerminalId::new();
        let (old, old_redraw, old_pending) = stream(&shared, id);
        old.publish(100); // Pre-binding output is covered by the initial capture.
        let delivery = install_visible(&shared, id, &old, &old_redraw);
        let (entered, at_cut) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        let (finished, done) = crossbeam_channel::bounded(1);
        let release = ReleaseOnDrop(release);
        let event_host = Arc::downgrade(&shared);
        let completion_redraw = Arc::clone(&old_redraw);
        old.handle.set_event_observer(move |handle, event| {
            let completed = matches!(event, TerminalEvent::Completed { .. });
            record_event_from(&event_host, id, event, Some(handle));
            if completed {
                let _ = entered.send(());
                // The real notifier has already accepted this exact old worker
                // under Host. Hold precisely the formerly unguarded interval.
                let resumed = released.recv_timeout(Duration::from_secs(5)).is_ok();
                if resumed {
                    completion_redraw.publish(handle.generation());
                }
                let _ = finished.send(resumed);
            }
        });
        old.emit(TerminalEvent::Completed {
            exit_code: 0,
            signal: None,
            intentional: false,
        });
        at_cut.recv_timeout(Duration::from_secs(2)).unwrap();

        let (successor, successor_redraw, successor_pending) = stream(&shared, id);
        successor.publish(1);
        assert!(
            successor_redraw
                .worker
                .set(successor.handle.downgrade())
                .is_ok()
        );
        {
            let mut state = shared.lock().unwrap();
            let retired = state.retire_terminal(id).unwrap();
            assert!(retired.handle.same_worker(&old.handle));
            state.reserve_spawn(id).unwrap();
            state
                .finish_spawn(1, session(id), successor.handle.clone(), visible)
                .unwrap();
            assert!(!state.terminals[&id].handle.same_worker(&old.handle));
            assert!(!state.subscriptions.viewports.contains_key(&(1, id)));
        }
        // Whatever installing the successor asked of it.
        successor.drain_visibility();
        drop(release);
        assert!(
            done.recv_timeout(Duration::from_secs(2)).unwrap(),
            "cut timed out"
        );
        old.publish(101); // Exercise the live wake path too, with a live old Handle.
        deliver(&old_pending);
        {
            let state = shared.lock().unwrap();
            assert!(!state.subscriptions.viewports.contains_key(&(1, id)));
            assert_eq!(state.terminals[&id].event_sequence, 0);
        }
        assert_eq!(
            successor.drain_visibility(),
            None,
            "old redraw hid successor"
        );
        if !visible {
            successor.publish(2);
            deliver(&successor_pending);
            assert_eq!(
                successor.drain_visibility(),
                Some(false),
                "control: the current hidden worker must receive Hide"
            );
            shared
                .lock()
                .unwrap()
                .set_visibility(1, id, TerminalVisibility::Visible)
                .unwrap();
        }
        successor.publish(3);
        deliver(&successor_pending);
        let first_revision = {
            let state = shared.lock().unwrap();
            let view = &state.subscriptions.viewports[&(1, id)];
            assert_eq!(view.source_generation, 3);
            view.revision
        };
        successor.publish(4);
        deliver(&successor_pending);
        {
            let state = shared.lock().unwrap();
            let view = &state.subscriptions.viewports[&(1, id)];
            assert_eq!(view.source_generation, 4);
            assert!(view.revision > first_revision);
        }
        delivery.close_after_drain();
        let mut frames = Vec::new();
        while let Some(message) = delivery.pop() {
            if let ServerMessage::Snapshot { snapshot, .. } = message.message {
                frames.push(snapshot.generation);
            }
        }
        assert_eq!(
            frames,
            [first_revision, first_revision + 1],
            "old callback must not project successor output with its generation"
        );
    }
}

#[test]
fn unbound_wake_never_borrows_a_resident_worker_and_binding_does_not_retain_it() {
    let shared = host();
    let id = TerminalId::new();
    let (resident, resident_redraw, resident_pending) = stream(&shared, id);
    let delivery = install_visible(&shared, id, &resident, &resident_redraw);
    let (unbound, _, unbound_pending) = stream(&shared, id);
    unbound.publish(200);
    assert!(unbound_pending.is_empty());
    assert!(
        !shared
            .lock()
            .unwrap()
            .subscriptions
            .viewports
            .contains_key(&(1, id))
    );
    resident.publish(1);
    deliver(&resident_pending);
    let weak = resident.handle.downgrade();
    drop(shared.lock().unwrap().retire_terminal(id).unwrap());
    drop(resident);
    assert!(
        weak.upgrade().is_none(),
        "wake binding formed a strong owner cycle"
    );
    resident_redraw.publish(201); // An expired worker has no new frame to deliver.
    drop(resident_redraw);
    assert!(resident_pending.try_recv().unwrap().upgrade().is_none());
    delivery.close_after_drain();
    let mut frames = Vec::new();
    while let Some(message) = delivery.pop() {
        if let ServerMessage::Snapshot { snapshot, .. } = message.message {
            frames.push(snapshot.generation);
        }
    }
    assert_eq!(frames, [1]);
}

#[test]
fn a_busy_host_keeps_one_latest_redraw_without_granting_capture_credit() {
    let shared = host();
    let id = TerminalId::new();
    let (stream, redraw, pending) = stream(&shared, id);
    let delivery = install_visible(&shared, id, &stream, &redraw);
    {
        let _state = shared.lock().unwrap();
        // A synchronous callback would try to lock the host again here.
        stream.publish(1);
        stream.publish(2);
        stream.publish(3);
        assert_eq!(pending.len(), 1);
        assert_eq!(stream.acknowledged_generation(), 0);
    }
    deliver(&pending);
    let source = shared.lock().unwrap().redraw_source(1, id, 3);
    assert_eq!(source, 3);
    assert_eq!(stream.acknowledged_generation(), 0);

    delivery.close_after_drain();
    let mut generations = Vec::new();
    while let Some(message) = delivery.pop() {
        if let ServerMessage::Snapshot { snapshot, .. } = message.message {
            generations.push(snapshot.generation);
        }
    }
    assert_eq!(generations, [3]);
    stream.handle.acknowledge_redraw(source);
    assert_eq!(stream.acknowledged_generation(), 3);
}

#[test]
fn an_unavailable_redraw_slot_still_delivers_the_frame() {
    let shared = host();
    let id = TerminalId::new();
    let (stream, redraw, pending) = stream(&shared, id);
    let _delivery = install_visible(&shared, id, &stream, &redraw);
    // A retired ticket can outlive its worker and occupy the slot.
    redraw
        .notifier
        .as_ref()
        .unwrap()
        .try_send(Weak::new())
        .unwrap();
    stream.publish(1);
    assert_eq!(shared.lock().unwrap().redraw_source(1, id, 1), 1);
    assert_eq!(stream.acknowledged_generation(), 0);
    assert!(pending.try_recv().unwrap().upgrade().is_none());

    stream.publish(2);
    deliver(&pending);
    assert_eq!(shared.lock().unwrap().redraw_source(1, id, 2), 2);
}

#[test]
fn retiring_events_follow_the_published_grid_even_with_a_pending_wake() {
    for event in [
        TerminalEvent::Exited {
            exit_code: 7,
            signal: None,
            intentional: false,
        },
        TerminalEvent::Completed {
            exit_code: 0,
            signal: None,
            intentional: false,
        },
    ] {
        let shared = host();
        let id = TerminalId::new();
        let (stream, redraw, pending) = stream(&shared, id);
        let delivery = install_visible(&shared, id, &stream, &redraw);
        stream.publish(3);
        redraw.record_event(&stream.handle, event);
        deliver(&pending);
        delivery.close_after_drain();

        let mut order = Vec::new();
        while let Some(message) = delivery.pop() {
            match message.message {
                ServerMessage::Snapshot { snapshot, .. } => {
                    assert_eq!(snapshot.generation, 3);
                    order.push("grid");
                }
                ServerMessage::Event {
                    event: WireTerminalEvent::Exited { .. } | WireTerminalEvent::Completed { .. },
                    ..
                } => order.push("exit"),
                _ => {}
            }
        }
        assert_eq!(order, ["grid", "exit"]);
    }
}
