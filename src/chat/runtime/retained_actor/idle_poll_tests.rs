//! Virtual-time policy and one private socket; no helper or provider process.
use super::*;

struct StatePeer {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<Result<Vec<Invocation>, String>>>,
}

impl StatePeer {
    fn start(listener: UnixListener, socket: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            loop {
                let (control, _) = listener.accept().map_err(|error| error.to_string())?;
                if stopped.load(Ordering::Acquire) {
                    return Ok(requests);
                }
                requests.push(receive::<Invocation>(&control, TIMEOUT)?);
                send(&control, &Ok::<Value, String>(Value::Null), TIMEOUT)?;
            }
        });
        Self {
            socket,
            stop,
            worker: Some(worker),
        }
    }

    fn finish(&mut self) -> Vec<Invocation> {
        self.stop.store(true, Ordering::Release);
        // Wake the owned blocking accept; the peer checks stop before reading.
        let _ = UnixStream::connect(&self.socket);
        self.worker.take().unwrap().join().unwrap().unwrap()
    }
}

impl Drop for StatePeer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.stop.store(true, Ordering::Release);
            let _ = UnixStream::connect(&self.socket);
            let _ = worker.join();
        }
    }
}

#[test]
fn hidden_virtual_second_sends_four_state_rpc_envelopes_visible_sends_twenty() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("state.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut peer = StatePeer::start(listener, socket.clone());
    let mut actor = RemoteActor::fixture();
    Arc::get_mut(&mut actor.0).unwrap().socket = socket;
    let state = actor.state();
    let now = Instant::now();
    let mut counts = Vec::new();
    for visible in [false, true] {
        let mut poll = StatePoll::new(now);
        let mut calls = 0;
        for tick in 0..20 {
            let force = fast_state_poll(
                &state,
                visible,
                false,
                actor.input_maintenance_pending(&state),
            );
            if let Some(response) =
                poll.call_if_due(now + Duration::from_millis(tick * 50), force, || {
                    calls += 1;
                    actor.call::<Option<Resident>>("state", json!([state.changed]))
                })
            {
                assert!(response.unwrap().is_none());
            }
        }
        counts.push(calls);
    }
    let requests = peer.finish();
    assert_eq!(counts, vec![4, 20], "half-open virtual [0,1000 ms)");
    assert_eq!(requests.len(), 24);
    for request in requests {
        assert_eq!(
            request.method, "state",
            "state cadence must issue metadata RPCs"
        );
        assert_eq!(request.args, json!([state.changed]));
        assert_eq!(request.epoch, Some(actor.0.epoch));
        assert_eq!(
            serde_json::to_value(request.proof).unwrap(),
            serde_json::to_value(&actor.0.proof).unwrap()
        );
    }
    // The visible control retains the old unconditional twenty-call cadence.
    assert_eq!(counts[1], (0..20).count());
}

#[test]
fn state_poll_honors_exact_boundary_forcing_and_next_tick_error_retry() {
    let now = Instant::now();
    let mut poll = StatePoll::new(now);
    let ok = || Ok::<_, &str>(());
    assert_eq!(poll.call_if_due(now, false, ok), Some(Ok(())));
    assert_eq!(
        poll.call_if_due(now + Duration::from_millis(249), false, ok),
        None
    );
    assert_eq!(
        poll.call_if_due(now + Duration::from_millis(250), false, ok),
        Some(Ok(()))
    );
    assert_eq!(
        poll.call_if_due(now + Duration::from_millis(300), true, ok),
        Some(Ok(()))
    );
    assert_eq!(
        poll.call_if_due(now + Duration::from_millis(549), false, ok),
        None
    );
    assert_eq!(
        poll.call_if_due(now + Duration::from_millis(550), false, ok),
        Some(Ok(()))
    );
    assert_eq!(
        poll.call_if_due(now + Duration::from_millis(600), true, || Err::<(), _>(
            "retry"
        )),
        Some(Err("retry"))
    );
    assert_eq!(
        poll.call_if_due(now + Duration::from_millis(650), false, ok),
        Some(Ok(())),
        "transient failure must not postpone the next maintenance tick"
    );
}

#[test]
fn active_background_semantic_and_startup_states_keep_the_fast_cadence() {
    let actor = RemoteActor::fixture();
    let quiet = actor.state();
    assert!(!fast_state_poll(&quiet, false, false, false));
    let cases: &[(&str, fn(&mut Resident))] = &[
        ("startup", |state| state.installed = false),
        ("not ready", |state| state.ready = false),
        ("not prompt idle", |state| state.idle = false),
        ("busy parent", |state| state.busy = true),
        ("waiting parent", |state| state.waiting = true),
        ("aggregate work while parent idle", |state| {
            state.working = true
        }),
        ("background child while parent idle", |state| {
            state.background_subagents.push("background".into());
        }),
        ("interruptible child", |state| {
            state.interrupt_subagents.push("child".into())
        }),
        ("unknown activity", |state| {
            state.activity = crate::agent::ActivityState::Unknown
        }),
        ("provider question", |state| {
            state.requests.push(chat::Request {
                kind: chat::RequestKind::Question,
                id: "question".into(),
                title: "Choose".into(),
                description: String::new(),
                fields: Vec::new(),
                actions: Vec::new(),
            });
        }),
        ("completed", |state| state.completed = true),
        ("closing", |state| state.close.closing = true),
        ("close completed", |state| state.close.completed = true),
        ("unavailable", |state| {
            state.failure = Some("fixture failure".into())
        }),
        ("uncommitted outcome", |state| state.outbox_latest = 1),
        ("inconsistent outcome clock", |state| state.outbox_ack = 1),
        ("continuity loss", |state| {
            state.continuity_loss = Some(ContinuityLoss {
                id: uuid::Uuid::new_v4(),
                affected_runs: Vec::new(),
                reason: "fixture loss".into(),
                acknowledged: false,
            });
        }),
    ];
    let now = Instant::now();
    for (label, change) in cases {
        let mut state = quiet.as_ref().clone();
        change(&mut state);
        let mut poll = StatePoll::new(now);
        let mut calls = 0;
        for tick in 0..20 {
            let force = fast_state_poll(&state, false, false, false);
            let _ = poll.call_if_due(now + Duration::from_millis(tick * 50), force, || {
                calls += 1;
                Ok::<_, ()>(())
            });
        }
        assert_eq!(calls, 20, "{label}");
    }
}

#[test]
fn newly_consumed_visibility_observe_and_input_intent_force_a_future_deadline() {
    let actor = RemoteActor::fixture();
    let state = actor.state();
    let now = Instant::now();
    for (label, visible, observe, input) in [
        ("hide", Some(false), false, false),
        ("show", Some(true), false, false),
        ("observe", None, true, false),
        ("input maintenance", None, false, true),
    ] {
        let mut poll = StatePoll::new(now);
        assert_eq!(
            poll.call_if_due(now, false, || Ok::<_, ()>(())),
            Some(Ok(()))
        );
        if let Some(visible) = visible {
            actor.set_visible(visible);
        }
        if observe {
            actor.observe_next_update();
        }
        // Consume exactly as the worker does: successful control calls may
        // clear both flags before the periodic state decision is made.
        let desired = actor.0.desired_visible.lock().unwrap().take();
        let observed = actor.0.desired_observe.swap(false, Ordering::AcqRel);
        assert!(actor.0.desired_visible.lock().unwrap().is_none());
        assert!(!actor.0.desired_observe.load(Ordering::Acquire));
        let force = fast_state_poll(
            &state,
            desired.unwrap_or(false),
            desired.is_some() || observed,
            input,
        );
        assert_eq!(
            poll.call_if_due(now + Duration::from_millis(50), force, || Ok::<_, ()>(())),
            Some(Ok(())),
            "{label} cannot wait behind the previous quiet deadline"
        );
    }
}

#[test]
fn live_receipts_force_polling_but_unknown_envelopes_remain_stable_without_fast_spinning() {
    let actor = RemoteActor::fixture();
    let state = actor.state();
    let payload = serde_json::to_value(InputPayload::Command {
        run: None,
        command: Command::Interrupt,
        decline_only: false,
    })
    .unwrap();
    let bytes = serde_json::to_vec(&payload).unwrap();
    let identity = InputIdentity {
        sequence: 1,
        id: uuid::Uuid::new_v4(),
        hash: payload_digest(&bytes),
    };
    let (reply, response) = crossbeam_channel::bounded(1);
    actor.0.pending.lock().unwrap().push(Pending {
        identity: identity.clone(),
        payload: payload.clone(),
        bytes: bytes.len(),
        reply: Some(reply),
    });
    *actor.0.input_sequence.lock().unwrap() = 2;
    *actor.0.uncertain_input.lock().unwrap() = Some(identity.clone());
    assert!(actor.input_maintenance_pending(&state));
    actor.apply_receipts(&[InputReceipt {
        identity: identity.clone(),
        state: InputState::Unknown("outcome is unconfirmed; do not resend".into()),
    }]);
    assert!(matches!(
        response.try_recv(),
        Ok(Err(Rejection::Unacknowledged(_)))
    ));
    assert!(!actor.input_maintenance_pending(&state));
    let now = Instant::now();
    let mut poll = StatePoll::new(now);
    let mut calls = 0;
    for tick in 0..20 {
        let force = fast_state_poll(
            &state,
            false,
            false,
            actor.input_maintenance_pending(&state),
        );
        let _ = poll.call_if_due(now + Duration::from_millis(tick * 50), force, || {
            calls += 1;
            Ok::<_, ()>(())
        });
    }
    assert_eq!(calls, 4);
    {
        let pending = actor.0.pending.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].identity, identity);
        assert_eq!(pending[0].payload, payload);
        assert!(pending[0].reply.is_none());
    }
    assert_eq!(*actor.0.input_sequence.lock().unwrap(), 2);
    // Unknown is terminal in the journal. A distinct accepted envelope may
    // settle synchronously while the original unknown envelope stays intact.
    let accepted = InputIdentity {
        sequence: 2,
        id: uuid::Uuid::new_v4(),
        hash: identity.hash,
    };
    let (reply, acknowledged_response) = crossbeam_channel::bounded(1);
    actor.0.pending.lock().unwrap().push(Pending {
        identity: accepted.clone(),
        payload: payload.clone(),
        bytes: bytes.len(),
        reply: Some(reply),
    });
    *actor.0.input_sequence.lock().unwrap() = 3;
    actor.apply_receipts(&[InputReceipt {
        identity: accepted.clone(),
        state: InputState::Accepted,
    }]);
    assert!(actor.input_maintenance_pending(&state));
    actor.apply_receipts(&[InputReceipt {
        identity: accepted.clone(),
        state: InputState::Acknowledged(Ok(())),
    }]);
    assert_eq!(acknowledged_response.try_recv(), Ok(Ok(())));
    assert_eq!(actor.pending_inputs(), vec![identity.clone()]);
    assert!(actor.input_maintenance_pending(&state));
    assert_eq!(*actor.0.retiring_inputs.lock().unwrap(), vec![accepted]);
    actor.0.retiring_inputs.lock().unwrap().clear();
    assert!(!actor.input_maintenance_pending(&state));
    let mut acknowledged = state.as_ref().clone();
    acknowledged.acknowledged_input_count = 1;
    assert!(actor.input_maintenance_pending(&acknowledged));
    actor.0.recovering_inputs.store(true, Ordering::Release);
    assert!(actor.input_maintenance_pending(&state));
    actor.0.recovering_inputs.store(false, Ordering::Release);
    assert!(!actor.input_maintenance_pending(&state));
    let pending = actor.0.pending.lock().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].identity, identity);
    assert_eq!(pending[0].payload, payload);
    assert!(pending[0].reply.is_none());
    assert_eq!(*actor.0.input_sequence.lock().unwrap(), 3);
}

#[test]
fn helper_wait_uses_remaining_orphan_budget_and_preserves_startup_input_ticks() {
    let now = Instant::now();
    let due = now + Duration::from_millis(250);
    assert_eq!(listener_wait_millis(true, false, due, now), 250);
    assert_eq!(listener_wait_millis(false, false, due, now), 20);
    assert_eq!(listener_wait_millis(true, true, due, now), 20);
    assert_eq!(listener_wait_millis(false, true, due, now), 20);
    assert_eq!(
        listener_wait_millis(true, false, due, now + Duration::from_millis(237)),
        13
    );
    assert_eq!(
        listener_wait_millis(true, true, due, now + Duration::from_millis(237)),
        13
    );
    assert_eq!(
        listener_wait_millis(true, false, due, due - Duration::from_nanos(1)),
        1
    );
    assert_eq!(listener_wait_millis(true, false, due, due), 0);
    assert_eq!(
        listener_wait_millis(false, true, due, due + Duration::from_millis(1)),
        0
    );
    // No positive fractional remainder becomes a zero-time busy loop.
    assert_eq!(
        listener_wait_millis(true, false, due, due - Duration::from_micros(1001)),
        2
    );
    let distant = now + Duration::from_millis(i32::MAX as u64 + 1);
    assert_eq!(listener_wait_millis(true, false, distant, now), i32::MAX);
}

#[test]
fn private_listener_wait_accepts_already_ready_connections_and_empty_zero_wait() {
    let root = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(root.path().join("readiness.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(!wait_for_listener(&listener, 0).unwrap());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let client = UnixStream::connect(root.path().join("readiness.sock")).unwrap();
    // The pending connection precedes this call. Check actual POLLIN on the
    // same owned listener, without a wake-latency or throughput claim.
    assert!(wait_for_listener(&listener, 250).unwrap());
    let (accepted, _) = listener.accept().unwrap();
    assert!(client.peer_addr().is_ok());
    assert!(accepted.peer_addr().is_ok());
    drop(accepted);
    drop(client);
    assert!(!wait_for_listener(&listener, 0).unwrap());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
