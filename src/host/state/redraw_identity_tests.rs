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

fn stream(shared: &SharedHost, id: TerminalId) -> (RenderStreamFixture, Arc<TerminalRedraw>) {
    let redraw = Arc::new(TerminalRedraw {
        host: Arc::downgrade(shared),
        terminal_id: id,
        worker: OnceLock::new(),
    });
    let wake = Arc::clone(&redraw);
    (
        RenderStreamFixture::new(move |edge| wake.publish(edge.generation)),
        redraw,
    )
}

struct ReleaseOnDrop(crossbeam_channel::Sender<()>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

#[test]
fn held_old_completion_and_wake_cannot_advance_or_hide_a_same_id_successor() {
    for visible in [true, false] {
        let shared = host();
        let id = TerminalId::new();
        let (old, old_redraw) = stream(&shared, id);
        old.publish(100); // Pre-binding output is covered by the initial capture.
        assert!(old_redraw.worker.set(old.handle.downgrade()).is_ok());
        let delivery = {
            let mut state = shared.lock().unwrap();
            let delivery = state.register_client(1);
            state.reserve_spawn(id).unwrap();
            state
                .finish_spawn(1, session(id), old.handle.clone(), true)
                .unwrap();
            delivery
        };
        let (entered, at_cut) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        let (finished, done) = crossbeam_channel::bounded(1);
        let release = ReleaseOnDrop(release);
        let event_host = Arc::downgrade(&shared);
        old.handle.set_event_observer(move |handle, event| {
            let completed = matches!(event, TerminalEvent::Completed { .. });
            record_event_from(&event_host, id, event, Some(handle));
            if completed {
                let _ = entered.send(());
                // The real notifier has already accepted this exact old worker
                // under Host. Hold precisely the formerly unguarded interval.
                let resumed = released.recv_timeout(Duration::from_secs(5)).is_ok();
                if resumed {
                    broadcast_redraw(&event_host, id, handle.generation(), handle);
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

        let (successor, successor_redraw) = stream(&shared, id);
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
        let commands = successor.queued_commands();
        drop(release);
        assert!(
            done.recv_timeout(Duration::from_secs(2)).unwrap(),
            "cut timed out"
        );
        old.publish(101); // Exercise the initial wake path too, with a live old Handle.
        {
            let state = shared.lock().unwrap();
            assert!(!state.subscriptions.viewports.contains_key(&(1, id)));
            assert_eq!(state.terminals[&id].event_sequence, 0);
        }
        assert_eq!(
            successor.queued_commands(),
            commands,
            "old redraw hid successor"
        );
        if !visible {
            successor.publish(2);
            assert_eq!(
                successor.queued_commands(),
                commands + 1,
                "control: the current hidden worker must receive Hide"
            );
            shared
                .lock()
                .unwrap()
                .set_visibility(1, id, TerminalVisibility::Visible)
                .unwrap();
        }
        successor.publish(3);
        let first_revision = {
            let state = shared.lock().unwrap();
            let view = &state.subscriptions.viewports[&(1, id)];
            assert_eq!(view.source_generation, 3);
            view.revision
        };
        successor.publish(4);
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
    let (resident, resident_redraw) = stream(&shared, id);
    assert!(
        resident_redraw
            .worker
            .set(resident.handle.downgrade())
            .is_ok()
    );
    let delivery = {
        let mut state = shared.lock().unwrap();
        let delivery = state.register_client(1);
        state.reserve_spawn(id).unwrap();
        state
            .finish_spawn(1, session(id), resident.handle.clone(), true)
            .unwrap();
        delivery
    };
    let (unbound, _) = stream(&shared, id);
    unbound.publish(200);
    assert!(
        !shared
            .lock()
            .unwrap()
            .subscriptions
            .viewports
            .contains_key(&(1, id))
    );
    resident.publish(1);
    let weak = resident.handle.downgrade();
    drop(shared.lock().unwrap().retire_terminal(id).unwrap());
    drop(resident);
    assert!(
        weak.upgrade().is_none(),
        "wake binding formed a strong owner cycle"
    );
    resident_redraw.publish(201); // Expired callback is harmless, not an ID lookup.
    delivery.close_after_drain();
    let mut frames = Vec::new();
    while let Some(message) = delivery.pop() {
        if let ServerMessage::Snapshot { snapshot, .. } = message.message {
            frames.push(snapshot.generation);
        }
    }
    assert_eq!(frames, [1]);
}
