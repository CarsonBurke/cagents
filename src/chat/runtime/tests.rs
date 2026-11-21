#[test]
fn notification_frame_fixture_preserves_busy_capture_and_acknowledges_teardown() {
    struct Waiting;
    impl Adapter for Waiting {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("fixture commands must not reach the decoder".into())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step {
                // TurnStarted establishes identity, not busy state. This
                // fixture must report work as well as its input wait.
                events: vec![
                    Event::Status {
                        busy: true,
                        detail: "Fixture work waiting for input".into(),
                    },
                    Event::Waiting(true),
                ],
                ..Step::default()
            })
        }
    }
    let (receipts, snapshot) = notification_frames_with_snapshot_for_test(
        Box::new(Waiting),
        vec![serde_json::Value::Null],
        false,
    );
    assert!(
        snapshot.busy && snapshot.waiting && !snapshot.exited,
        "captured busy={}, waiting={}, exited={}, error_present={}",
        snapshot.busy,
        snapshot.waiting,
        snapshot.exited,
        snapshot.error.is_some(),
    );
    assert!(snapshot.error.is_none());
    assert_eq!(
        receipts.len(),
        1,
        "teardown must not publish a turn outcome"
    );
    assert_eq!(receipts[0].0.kind, crate::notifications::Kind::Waiting);
    assert_eq!(receipts[0].1.request, None);
}

#[test]
fn typed_provider_error_items_keep_full_provenance_and_ids_through_store_hydration() {
    let mut store = Store::new().unwrap();
    let raw = "You've hit your limit · resets 1:30pm (America/Los_Angeles)\noriginal diagnostics";
    for (prompt, failure) in [("prompt-one", "failure-one"), ("prompt-two", "failure-two")] {
        store
            .apply(Event::Item(Item {
                id: prompt.into(),
                role: Role::User,
                content: ItemContent::Text {
                    text: "Explain the limit notice".into(),
                    thinking: false,
                    phase: Phase::Final,
                },
                at: None,
                started_at: None,
            }))
            .unwrap();
        let mut item = Item {
            id: failure.into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: raw.into(),
                thinking: false,
                phase: Phase::Final,
            },
            at: None,
            started_at: None,
        };
        assert!(super::super::failure::mark_provider_error(
            crate::agent::Provider::Claude,
            &mut item
        ));
        store.apply(Event::Item(item)).unwrap();
    }
    let window = store.window().unwrap();
    let failures: Vec<_> = window
        .items
        .iter()
        .filter_map(|item| {
            super::super::failure::provider_error_text(crate::agent::Provider::Claude, item)
                .map(|body| (item.id.as_str(), body))
        })
        .collect();
    assert_eq!(failures, [("failure-one", raw), ("failure-two", raw)]);
    assert_eq!(
        window
            .items
            .iter()
            .filter(|item| item.role == Role::User)
            .count(),
        2
    );
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let quote = Item {
        id: "child-user-quotation".into(),
        role: Role::System,
        content: ItemContent::Text {
            text: raw.into(),
            thinking: false,
            phase: Phase::Final,
        },
        at: None,
        started_at: None,
    };
    for event in store
        .apply(Event::SubagentItem {
            id: "child".into(),
            item: quote,
        })
        .unwrap()
    {
        apply_event(&mut state, &mut indices, event);
    }
    let quoted = &state.subagents[0].items[0];
    assert_eq!(quoted.role, Role::System);
    assert!(
        super::super::failure::provider_error_text(crate::agent::Provider::Claude, quoted)
            .is_none()
    );
    assert!(matches!(&quoted.content, ItemContent::Text { text, .. } if text==raw));
}

#[test]
fn mixed_child_failure_and_background_notice_preserve_real_activity_through_store() {
    let mut store = Store::new().unwrap();
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let mut apply = |state: &mut Snapshot, event| {
        for event in store.apply(event).unwrap() {
            apply_event(state, &mut indices, event);
        }
    };
    let child = |id: &str, status, waiting, detail: &str| Event::Subagent {
        id: id.into(),
        kind: "Review".into(),
        description: id.into(),
        status: Some(status),
        background: Some(true),
        model: None,
        waiting: Some(waiting),
        detail: Some(detail.into()),
    };
    apply(&mut state, Event::Ready(true));
    apply(
        &mut state,
        Event::Status {
            busy: false,
            detail: "Parent turn ended".into(),
        },
    );
    apply(
        &mut state,
        Event::Error {
            message: "You've hit your limit · resets 1:30pm".into(),
            fatal: false,
        },
    );
    apply(
        &mut state,
        child(
            "failed",
            ToolStatus::Failed,
            false,
            "connection reset by peer",
        ),
    );
    apply(&mut state, child("live", ToolStatus::Running, false, ""));
    apply(
        &mut state,
        child("blocked", ToolStatus::Running, true, "quota exceeded"),
    );
    assert_eq!(state.activity(), ActivityState::Waiting);
    assert!(state.is_working());
    assert!(
        !state.turn_accepted(),
        "children at work leave a refused parent's own turn unaccepted"
    );
    let error = state.error.clone();
    apply(
        &mut state,
        Event::Item(Item {
            id: "actual-background-completion".into(),
            role: Role::User,
            content: ItemContent::Delivery {
                kind: super::super::DeliveryKind::Notice,
                from: String::new(),
                sender: "bash-task".into(),
                summary: "Background command completed (exit code 0)".into(),
                body: String::new(),
            },
            at: None,
            started_at: None,
        }),
    );
    assert!(!state.busy);
    assert_eq!(state.error, error);
    assert_eq!(state.activity(), ActivityState::Waiting);
    assert_eq!(
        state
            .subagents
            .iter()
            .filter(|child| child.status == ToolStatus::Running)
            .count(),
        2
    );
    apply(
        &mut state,
        child("blocked", ToolStatus::Failed, false, "quota exceeded"),
    );
    assert_eq!(
        state.activity(),
        ActivityState::Working,
        "A distinct live child still owns work"
    );
    // A replayed failed child's diagnostic is not a new launch or a parent
    // status update. Its terminal status and the live sibling survive.
    apply(
        &mut state,
        Event::Subagent {
            id: "failed".into(),
            kind: String::new(),
            description: String::new(),
            status: None,
            background: None,
            model: None,
            waiting: Some(true),
            detail: Some("connection reset by peer".into()),
        },
    );
    assert_eq!(state.activity(), ActivityState::Working);
    assert_eq!(
        state
            .subagents
            .iter()
            .find(|child| child.id == "failed")
            .unwrap()
            .status,
        ToolStatus::Failed
    );
    apply(&mut state, child("live", ToolStatus::Succeeded, false, ""));
    assert_eq!(state.activity(), ActivityState::Idle);
    assert_eq!(
        state.error, error,
        "An unrelated completion cannot clear the parent's limit"
    );
}

#[test]
fn parent_prompt_availability_tracks_whole_steps_independently_of_children() {
    struct Frames(VecDeque<Vec<Event>>);
    impl Adapter for Frames {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![
                    Event::Ready(true),
                    Event::Status {
                        busy: true,
                        detail: String::new(),
                    },
                    Event::Subagent {
                        id: "child".into(),
                        kind: String::new(),
                        description: String::new(),
                        status: Some(ToolStatus::Running),
                        background: Some(true),
                        model: None,
                        waiting: None,
                        detail: None,
                    },
                ],
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step {
                events: self.0.pop_front().ok_or("frames exhausted")?,
                ..Default::default()
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let status = |busy| Event::Status {
        busy,
        detail: String::new(),
    };
    let request = || {
        Event::Request(super::super::Request {
            kind: super::super::RequestKind::Approval,
            id: "approval".into(),
            title: String::new(),
            description: String::new(),
            fields: vec![],
            actions: vec![],
        })
    };
    let child = || Event::Subagent {
        id: "child".into(),
        kind: String::new(),
        description: "Still working".into(),
        status: Some(ToolStatus::Running),
        background: Some(true),
        model: None,
        waiting: None,
        detail: None,
    };
    let mut adapter = Frames(VecDeque::from([
        vec![child()],
        vec![status(false), request()], // End-of-step request defeats transient eligibility.
        vec![child()],
        vec![Event::RequestResolved {
            id: "approval".into(),
        }],
        vec![child()],
        vec![status(true)],
        vec![status(false)],
        vec![Event::Waiting(true)],
        vec![Event::Waiting(false)],
        vec![Event::Ready(false)],
        vec![Event::Ready(true)],
    ]));
    let directory = tempfile::tempdir().unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: vec![],
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let updates = Arc::new(Mutex::new(Vec::new()));
    let observed = updates.clone();
    let handle = LocalHandle::start(
        spec,
        Box::new(adapter),
        initial,
        store,
        None,
        move |update| {
            observed.lock().unwrap().push(update);
        },
    )
    .unwrap();
    handle.check_update_readiness().unwrap(); // Actor initialization barrier.
    let available = || {
        updates
            .lock()
            .unwrap()
            .iter()
            .filter(|update| matches!(update, Update::PromptAvailable))
            .count()
    };
    assert_eq!(available(), 0);
    for (count, expected_idle) in [
        (0, false),
        (0, false),
        (0, false),
        (1, true),
        (1, true),
        (1, false),
        (2, true),
        (2, false),
        (3, true),
        (3, false),
        (4, true),
    ] {
        handle.command(Command::Compact).unwrap();
        assert_eq!(available(), count);
        assert_eq!(handle.is_idle(), expected_idle);
    }
    assert_eq!(
        handle.activity(),
        ActivityState::Working,
        "child activity remains authoritative"
    );
    assert_eq!(
        updates
            .lock()
            .unwrap()
            .iter()
            .filter(|update| matches!(
                update,
                Update::Activity {
                    state: ActivityState::Idle,
                    ..
                }
            ))
            .count(),
        0
    );
    handle.close().unwrap();
}

#[test]
fn notification_title_matches_resident_session_identity_across_metadata_only_frames() {
    struct Names;
    impl Adapter for Names {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("fixture uses actual decoded metadata frames".into())
        }
        fn receive(&mut self, frame: Value) -> Result<Step, String> {
            let index = frame.as_u64().ok_or("expected fixture frame index")?;
            let session = |id: &str, title: Option<&str>| Event::Session {
                id: id.into(),
                title: title.map(str::to_owned),
            };
            let mut events = vec![Event::TurnStarted];
            events.extend(match index {
                0 => vec![session("native-a", Some("Provider A"))],
                1 => vec![session("native-a", None)],
                2 => vec![session("native-b", None)],
                3 => vec![
                    session("native-b", Some("Provider B")),
                    session("native-b", None),
                ],
                4 => vec![
                    Event::Reset {
                        reason: "Explicit new conversation".into(),
                    },
                    session("native-b", None),
                ],
                _ => return Err("unexpected fixture frame".into()),
            });
            events.extend([
                Event::TurnSettled,
                Event::TurnFinished {
                    key: Some(format!("completed-{index}")),
                    outcome: crate::notifications::Kind::Succeeded,
                },
            ]);
            Ok(Step {
                events,
                ..Default::default()
            })
        }
    }
    // The real Actor/Store callback captures each immutable completion
    // frame before the next command, then retires its actual cat child.
    let (receipts, snapshot) = notification_frames_with_snapshot_for_test(
        Box::new(Names),
        (0..5).map(|index| serde_json::json!(index)).collect(),
        false,
    );
    assert_eq!(receipts.len(), 5);
    assert_eq!(
        receipts
            .iter()
            .map(|(_, content)| content.conversation.as_str())
            .collect::<Vec<_>>(),
        ["Provider A", "Provider A", "", "Provider B", ""]
    );
    assert_eq!(snapshot.session_id.as_deref(), Some("native-b"));
    assert_eq!(snapshot.title, None);
}

#[test]
fn notification_local_worker_loss_is_unconfirmed_without_reclassifying_known_provider_failure() {
    struct LostTransport {
        emit_terminal: bool,
    }
    impl Adapter for LostTransport {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                writes: vec![serde_json::json!({"fixture":"receive"})],
                events: vec![Event::Ready(true), Event::TurnStarted],
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("fixture uses its actual pipe".into())
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            if std::mem::take(&mut self.emit_terminal) {
                return Ok(Step {
                    writes: vec![serde_json::json!({"fixture":"disconnect-after-proof"})],
                    events: vec![
                        Event::TurnSettled,
                        Event::Error {
                            message: "Provider reported migration failure".into(),
                            fatal: false,
                        },
                        Event::TurnFinished {
                            key: Some("confirmed-failed-turn".into()),
                            outcome: crate::notifications::Kind::Failed,
                        },
                    ],
                    ..Default::default()
                });
            }
            Err("Fixture local transport failed before further terminal proof".into())
        }
    }
    for known_failure in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let spec = ProcessSpec {
            stdout_noise: None,
            followed: false,
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: vec![],
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(LostTransport {
            emit_terminal: known_failure,
        });
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let (send, receive) = crossbeam_channel::bounded(8);
        let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
            if let Update::AttentionContext { evidence, content } = update {
                let _ = send.try_send((evidence, content));
            }
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_completed() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let finished_before_close = handle.is_completed();
        let failure = handle.failure();
        let closed = handle.close();
        // Original process/worker retirement precedes every assertion.
        closed.unwrap();
        assert!(handle.is_completed());
        assert!(
            finished_before_close,
            "fixture must actually reach its failing receive before close"
        );
        assert!(
            failure
                .as_deref()
                .is_some_and(|error| error.contains("Fixture local transport failed"))
        );
        let notices = receive.try_iter().collect::<Vec<_>>();
        assert_eq!(notices.len(), 1);
        let (evidence, content) = &notices[0];
        if known_failure {
            assert_eq!(evidence.kind, crate::notifications::Kind::Failed);
            assert_eq!(
                content.as_ref().unwrap().preview,
                "Provider reported migration failure"
            );
        } else {
            assert_eq!(evidence.kind, crate::notifications::Kind::Interrupted);
            assert!(
                content
                    .as_ref()
                    .unwrap()
                    .preview
                    .contains("Fixture local transport failed")
            );
        }
    }
}

#[test]
fn notification_final_provider_proof_survives_a_pending_transcript_flush_failure() {
    struct FinalFrame {
        run: Option<crate::model::RunId>,
    }
    impl Adapter for FinalFrame {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::Ready(true)],
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("use a correlated run".into())
        }
        fn command_for_run(
            &mut self,
            run: crate::model::RunId,
            _: Command,
        ) -> Result<Step, String> {
            self.run = Some(run);
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![serde_json::json!({"final":true})],
                events: vec![
                    Event::Execution {
                        run,
                        event: crate::agent_execution::ProviderEvent::Started {
                            key: "confirmed-provider-turn".into(),
                        },
                    },
                    Event::TurnStarted,
                ],
            })
        }
        fn receive(&mut self, frame: Value) -> Result<Step, String> {
            assert_eq!(frame, serde_json::json!({"final":true}));
            let run = self.run.take().unwrap();
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![
                    Event::Execution {
                        run,
                        event: crate::agent_execution::ProviderEvent::Scope,
                    },
                    Event::TextDelta {
                        id: "answer".into(),
                        role: Role::Assistant,
                        thinking: false,
                        delta: "Partial response".into(),
                    },
                    // Store flushes the pending delta here and fails. The
                    // remaining typed provider proof must still be processed.
                    Event::TurnSettled,
                    Event::Item(crate::chat::Item {
                        id: "answer".into(),
                        role: Role::Assistant,
                        started_at: None,
                        at: None,
                        content: ItemContent::Text {
                            text: "Exact final response".into(),
                            thinking: false,
                            phase: Phase::Final,
                        },
                    }),
                    Event::Usage(Default::default()),
                    Event::TurnFinished {
                        key: Some("confirmed-provider-turn".into()),
                        outcome: crate::notifications::Kind::Succeeded,
                    },
                    Event::Execution {
                        run,
                        event: crate::agent_execution::ProviderEvent::Settled,
                    },
                    Event::Execution {
                        run,
                        event: crate::agent_execution::ProviderEvent::Finished {
                            outcome: crate::notifications::Kind::Succeeded,
                        },
                    },
                ],
            })
        }
    }
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(FinalFrame { run: None });
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let (updates, receiver) = crossbeam_channel::unbounded();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        let _ = updates.send(update);
    })
    .unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(2);
    while !handle.0.state.lock().unwrap().ready && Instant::now() < ready_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let ready = handle.0.state.lock().unwrap().ready;
    if !ready {
        let closed = handle.close();
        assert!(
            ready,
            "actual actor initialization must complete; close: {closed:?}"
        );
    }
    handle.0.store.lock().unwrap().read_only_for_test().unwrap();
    let run = crate::model::RunId::new();
    handle
        .command_for_run(
            run,
            Command::Prompt {
                text: "One accepted prompt".into(),
                attachments: Vec::new(),
                mode: crate::chat::PromptMode::Send,
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.is_completed() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let completed = handle.is_completed();
    let closed = handle.close();
    assert!(
        completed,
        "the real actor must settle after reporting its cache failure"
    );
    closed.unwrap();
    let updates = receiver.try_iter().collect::<Vec<_>>();
    let finished = updates
        .iter()
        .filter_map(|update| match update {
            Update::Execution {
                run: actual,
                update: crate::agent_execution::Update::Finished { state, result },
            } if *actual == run => Some((state, result)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        finished.len(),
        1,
        "the cache failure cannot synthesize a second terminal outcome: {updates:?}"
    );
    assert_eq!(*finished[0].0, crate::agent_execution::State::Succeeded);
    assert_eq!(finished[0].1.text, "Exact final response");
    let evidence = updates
        .iter()
        .filter_map(|update| match update {
            Update::AttentionContext { evidence, content } => Some((evidence, content)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        evidence.len(),
        1,
        "known success must not be replaced by a cache-error failure alert"
    );
    assert_eq!(evidence[0].0.kind, crate::notifications::Kind::Succeeded);
    assert_eq!(
        evidence[0].1.as_ref().unwrap().preview,
        "Exact final response"
    );
    assert!(
        handle.failure().is_some(),
        "the transcript I/O failure remains visible independently of provider success"
    );
}

#[test]
fn resident_title_admission_refuses_without_waiting_for_the_state_owner() {
    struct Idle;
    impl Adapter for Idle {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        transport: TransportSpec::History,
        program: "unused-history-reader".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(Idle), initial, store, None, |_| {}).unwrap();
    let state = handle.0.state.lock().unwrap();
    let probe = handle.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || sender.send(probe.conversation_title()).unwrap());
    let outcome = receiver.recv_timeout(Duration::from_millis(250));
    // Release before joining/asserting: the old blocking implementation
    // fails the test without stranding its thread or the runtime worker.
    drop(state);
    worker.join().unwrap();
    handle.close().unwrap();
    assert_eq!(outcome.unwrap(), Err(TitleUnavailable::Busy));
}

#[test]
fn identity_only_frames_preserve_a_name_until_actual_session_change_or_reset() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    apply_event(
        &mut state,
        &mut indices,
        Event::Session {
            id: "a".into(),
            title: Some("Bundle review".into()),
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::Session {
            id: "a".into(),
            title: None,
        },
    );
    assert_eq!(state.title.as_deref(), Some("Bundle review"));
    apply_event(
        &mut state,
        &mut indices,
        Event::Session {
            id: "b".into(),
            title: None,
        },
    );
    assert!(state.title.is_none());
    apply_event(
        &mut state,
        &mut indices,
        Event::Session {
            id: "b".into(),
            title: Some("New task".into()),
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::Reset {
            reason: "Reset".into(),
        },
    );
    assert!(state.title.is_none());
}

#[test]
fn prompt_suggestion_metadata_cannot_survive_a_turn_session_or_process_boundary() {
    let mut state = Snapshot {
        session_id: Some("session".into()),
        ready: true,
        ..Default::default()
    };
    let mut indices = BTreeMap::new();
    let suggestion = || Event::PromptSuggestion {
        session_id: "session".into(),
        source: "turn-one".into(),
        text: "Run tests".into(),
    };
    apply_event(&mut state, &mut indices, suggestion());
    assert_eq!(state.prompt_suggestion.as_ref().unwrap().turn_generation, 0);
    assert!(state.items.is_empty());
    apply_event(&mut state, &mut indices, Event::TurnStarted);
    assert!(state.prompt_suggestion.is_none());
    apply_event(
        &mut state,
        &mut indices,
        Event::Status {
            busy: true,
            detail: "working".into(),
        },
    );
    apply_event(&mut state, &mut indices, suggestion());
    assert!(state.prompt_suggestion.is_none());
    apply_event(
        &mut state,
        &mut indices,
        Event::Status {
            busy: false,
            detail: "idle".into(),
        },
    );
    apply_event(&mut state, &mut indices, suggestion());
    apply_event(
        &mut state,
        &mut indices,
        Event::Session {
            id: "different".into(),
            title: None,
        },
    );
    assert!(state.prompt_suggestion.is_none());
    apply_event(&mut state, &mut indices, suggestion());
    assert!(state.prompt_suggestion.is_none());
    state.session_id = Some("session".into());
    apply_event(&mut state, &mut indices, suggestion());
    apply_event(&mut state, &mut indices, Event::Exited { code: None });
    assert!(state.prompt_suggestion.is_none());
    apply_event(&mut state, &mut indices, suggestion());
    assert!(state.prompt_suggestion.is_none());
}

#[test]
fn each_check_that_finds_stopped_commands_gives_a_new_notice() {
    use crate::daemon::process_scope::{CommandStop, Killer};
    let oomd = |processes| CommandStop {
        killer: Killer::Oomd,
        processes,
    };
    let first = command_stop_report(&[oomd(Some(3))], 0);
    assert_eq!(
        first,
        format!(
            "{} systemd-oomd ended it (3 processes); the agent is still running.",
            super::super::failure::COMMAND_OUT_OF_MEMORY
        )
    );
    // The agent retries the command and it is stopped the same way.
    let again = command_stop_report(&[oomd(Some(3))], 1);
    assert_ne!(again, first, "the card for a repeat must be a new one");
    assert!(again.ends_with("since the agent started: 2."), "{again}");

    let both = command_stop_report(
        &[
            oomd(Some(1)),
            CommandStop {
                killer: Killer::Kernel,
                processes: None,
            },
        ],
        0,
    );
    assert!(
        both.contains(
            "systemd-oomd and the kernel's out-of-memory killer ended 2 commands; the agent"
        ),
        "{both}"
    );
    for report in [first, again, both] {
        assert!(super::super::failure::is_notice(&report), "{report}");
    }
}

#[test]
fn steps_left_running_by_an_ended_provider_read_as_stopped() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let tool = |id: &str, status| Item {
        started_at: None,
        id: id.into(),
        role: crate::chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Bash".into(),
            input: "sleep 6001".into(),
            output: String::new(),
            status,
        },
        at: None,
    };
    apply_event(
        &mut state,
        &mut indices,
        Event::Item(tool("running", ToolStatus::Running)),
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::Item(tool("done", ToolStatus::Succeeded)),
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::Item(tool("waiting", ToolStatus::Waiting)),
    );
    let finished = Arc::clone(&state.items[1]);
    let agent = subagent(&mut state, "scout");
    agent.waiting = true;
    agent
        .items
        .push(Arc::new(tool("child", ToolStatus::Running)));
    subagent(&mut state, "builder").status = ToolStatus::Failed;
    stop_snapshot_steps(&mut state);
    apply_event(&mut state, &mut indices, Event::Exited { code: None });
    let statuses: Vec<_> = state
        .items
        .iter()
        .map(|item| item.content.status())
        .collect();
    assert_eq!(
        statuses,
        [
            Some(ToolStatus::Stopped),
            Some(ToolStatus::Succeeded),
            Some(ToolStatus::Stopped)
        ]
    );
    assert!(
        Arc::ptr_eq(&finished, &state.items[1]),
        "settled items are not copied"
    );
    let scout = &state.subagents[0];
    assert_eq!(scout.status, ToolStatus::Stopped);
    assert!(!scout.waiting);
    assert_eq!(scout.items[0].content.status(), Some(ToolStatus::Stopped));
    assert_eq!(state.subagents[1].status, ToolStatus::Failed);
}

#[test]
fn read_only_history_reason_survives_exit_and_snapshot_serialization() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let reason = "The saved working directory is unavailable. Conversation history is read-only.";
    apply_event(
        &mut state,
        &mut indices,
        Event::ReadOnlyHistory {
            reason: reason.into(),
        },
    );
    apply_event(&mut state, &mut indices, Event::Exited { code: None });
    assert_eq!(state.read_only_reason.as_deref(), Some(reason));
    let mut wire = serde_json::to_value(&state).unwrap();
    let restored: Snapshot = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(restored.read_only_reason, state.read_only_reason);
    wire.as_object_mut().unwrap().remove("read_only_reason");
    let legacy: Snapshot = serde_json::from_value(wire).unwrap();
    assert!(legacy.read_only_reason.is_none());
    apply_event(
        &mut state,
        &mut indices,
        Event::Reset {
            reason: "New conversation".into(),
        },
    );
    assert!(state.read_only_reason.is_none());
}

#[test]
fn incomplete_stdio_frames_only_scan_the_new_suffix() {
    let mut bytes = Vec::new();
    let mut scanned = 0;
    for _ in 0..1024 {
        let before = bytes.len();
        bytes.extend_from_slice(&[b'x'; 8192]);
        assert_eq!(scanned, before);
        assert_eq!(super::next_newline(&bytes, &mut scanned), None);
        assert_eq!(scanned, bytes.len());
    }
    bytes.extend_from_slice(b"\n{}\n");
    assert_eq!(
        super::next_newline(&bytes, &mut scanned),
        Some(8 * 1024 * 1024)
    );
    bytes.drain(..=scanned);
    scanned = 0;
    assert_eq!(super::next_newline(&bytes, &mut scanned), Some(2));
}

#[test]
fn update_readiness_fences_a_prompt_whose_caller_stopped_waiting() {
    struct GatedPrompt {
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    }
    impl Adapter for GatedPrompt {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            match command {
                Command::Compact => Ok(Step {
                    events: vec![Event::Ready(true)],
                    ..Step::default()
                }),
                Command::Prompt { .. } => {
                    self.entered.send(()).map_err(|error| error.to_string())?;
                    self.release
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|error| error.to_string())?;
                    Ok(Step::default())
                }
                Command::Interrupt => Ok(Step {
                    events: vec![Event::Status {
                        busy: false,
                        detail: String::new(),
                    }],
                    ..Step::default()
                }),
                _ => Ok(Step::default()),
            }
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(
        spec,
        Box::new(GatedPrompt {
            entered: entered_tx,
            release: release_rx,
        }),
        initial,
        store,
        None,
        |_| {},
    )
    .unwrap();
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        UpdateReadiness::Unready,
        "initialization is not idle"
    );
    handle.command(Command::Compact).unwrap();
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        UpdateReadiness::Idle
    );
    assert!(
        !handle.0.stop.load(Ordering::Acquire),
        "readiness must not close the actor"
    );

    // The request was accepted into the actor queue, but its caller has
    // stopped waiting for acknowledgment. Its snapshot is still idle.
    let (reply, timed_out) = crossbeam_channel::bounded(1);
    drop(timed_out);
    handle
        .0
        .commands
        .send((
            ActorCommand::Provider {
                run: None,
                decline_only: false,
                command: Command::Prompt {
                    text: "accepted work".into(),
                    attachments: Vec::new(),
                    mode: super::super::PromptMode::Send,
                },
            },
            reply,
        ))
        .unwrap();
    handle.wake();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!handle.snapshot(None).unwrap().busy);
    let (reply, acknowledgment) = crossbeam_channel::bounded(1);
    let (result, readiness) = crossbeam_channel::bounded(1);
    handle
        .0
        .commands
        .send((ActorCommand::CheckUpdateReadiness(result), reply))
        .unwrap();
    handle.wake();
    assert!(matches!(
        readiness.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty)
    ));
    release_tx.send(()).unwrap();
    assert_eq!(
        readiness.recv_timeout(Duration::from_secs(5)).unwrap(),
        UpdateReadiness::Busy
    );
    acknowledgment
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert!(handle.snapshot(None).unwrap().busy);
    handle.close().unwrap();
}

#[test]
fn startup_cleanup_joins_the_exact_actor_before_releasing_ownership() {
    use super::super::protocol::{ExecutionContext, LaunchOutcome};
    let directory = tempfile::tempdir().unwrap();
    let context = Arc::new(ExecutionContext::default());
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: Some(context.clone()),
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
    assert_eq!(context.outcome(), LaunchOutcome::Uncertain);
    assert_eq!(
        context.reconcile_cleanup().unwrap(),
        LaunchOutcome::TeardownProved
    );
    assert!(handle.is_completed());
    assert_eq!(
        context.reconcile_cleanup().unwrap(),
        LaunchOutcome::TeardownProved
    );
    assert!(
        handle.command(Command::Refresh).is_ok(),
        "retired transcript remains readable"
    );
}

#[test]
fn completion_time_waits_for_provider_settlement_and_background_work() {
    let mut attention = TurnAttention::default();
    let mut state = Snapshot::default();
    attention.apply(&Event::TurnFinished {
        key: None,
        outcome: crate::notifications::Kind::Succeeded,
    });
    assert!(
        !attention.finish(&mut state, Some(10)),
        "imported history has no observed start"
    );
    attention.apply(&Event::TurnStarted);
    state.busy = true;
    attention.apply(&Event::TurnSettled);
    assert!(!attention.finish(&mut state, Some(20)));
    state.busy = false;
    state.subagents.push(crate::chat::Subagent {
        model: None,
        waiting: false,
        detail: String::new(),
        id: "child".into(),
        kind: "test".into(),
        description: "Background work".into(),
        status: crate::chat::ToolStatus::Running,
        background: true,
        items: Vec::new(),
    });
    assert!(!attention.finish(&mut state, Some(30)));
    state.subagents[0].status = crate::chat::ToolStatus::Succeeded;
    state.waiting = true;
    assert!(
        !attention.finish(&mut state, Some(40)),
        "attention still required"
    );
    state.waiting = false;
    assert!(attention.finish(&mut state, Some(50)));
    assert_eq!(state.finished_at, Some(50));
    assert!(
        !attention.finish(&mut state, Some(60)),
        "heartbeat cannot move completion"
    );
    attention.apply(&Event::TurnFinished {
        key: Some("delayed-outcome".into()),
        outcome: crate::notifications::Kind::Cancelled,
    });
    assert!(!attention.finish(&mut state, Some(70)));
    assert_eq!(state.finished_at, Some(50));
    state.subagents[0].status = crate::chat::ToolStatus::Running;
    assert!(attention.finish(&mut state, Some(75)));
    assert_eq!(state.finished_at, None);
    state.subagents[0].status = crate::chat::ToolStatus::Succeeded;
    assert!(attention.finish(&mut state, Some(76)));
    assert_eq!(state.finished_at, Some(76));
    attention.apply(&Event::TurnStarted);
    apply_event(&mut state, &mut BTreeMap::new(), Event::TurnStarted);
    assert_eq!(state.finished_at, None);
    assert!(!attention.finish(&mut state, Some(80)));
    attention.apply(&Event::Reset {
        reason: "reset".into(),
    });
    apply_event(
        &mut state,
        &mut BTreeMap::new(),
        Event::Reset {
            reason: "reset".into(),
        },
    );
    assert!(!attention.finish(&mut state, Some(90)));
}

#[test]
fn late_outcome_does_not_settle_a_new_turn_with_continuous_activity() {
    use super::*;
    let mut attention = TurnAttention::default();
    let old = attention.key.clone();
    attention.apply(&Event::TurnSettled);
    assert!(
        attention.settled,
        "idle transport loss must not duplicate an outcome"
    );
    // Aggregate activity may remain Working because background tasks run
    // across the result and the next accepted user prompt.
    attention.apply(&Event::TurnStarted);
    let current = attention.key.clone();
    assert_ne!(current, old);
    assert!(!attention.settled);
    let delayed = attention
        .apply(&Event::TurnFinished {
            key: Some(old.clone()),
            outcome: crate::notifications::Kind::Cancelled,
        })
        .unwrap();
    assert_eq!(delayed.key, old);
    assert_eq!(attention.key, current);
    assert!(
        !attention.settled,
        "current transport failure still needs an outcome"
    );
    assert!(!attention.finish(&mut Snapshot::default(), Some(100)));
}

use super::super::Role;
use super::*;
use serde_json::{Value, json};

#[test]
fn refusals_name_a_waiting_subagent_and_how_to_let_it_go() {
    let mut state = Snapshot::default();
    let subagent = |id: &str, description: &str, status| super::super::Subagent {
        id: id.into(),
        kind: description.into(),
        description: description.into(),
        status,
        model: None,
        waiting: false,
        detail: String::new(),
        background: true,
        items: Vec::new(),
    };
    state.subagents = vec![subagent(
        "toolu_dev",
        "Run the dev server",
        ToolStatus::Waiting,
    )];
    assert_eq!(
        idle_close_refusal(UpdateReadiness::Busy, &state),
        "Background subagent \u{201c}Run the dev server\u{201d} is waiting on its own background work, which may not finish on its own. Stop it from its tab in the conversation to let it close."
    );
    state.subagents.push(subagent(
        "toolu_audit",
        "Audit the parser",
        ToolStatus::Running,
    ));
    let hold = state.work_hold();
    let refused = interrupt_refusal(&hold.subagents());
    assert_eq!(
        refused,
        "Interrupting would also stop 1 running and 1 waiting background subagent. Background subagent \u{201c}Audit the parser\u{201d} is still running, and \u{201c}Run the dev server\u{201d} is waiting on its own background work, which may not finish on its own. Use --including-subagents to permit this, or stop them from their tabs in the app first."
    );
    assert!(!refused.contains("toolu_"));
    // A turn still under way is what holds it; its subagents are not
    // offered as the reason.
    state.busy = true;
    assert_eq!(
        idle_close_refusal(UpdateReadiness::Busy, &state),
        "The conversation is busy or waiting for an answer, so it was left open."
    );
    assert!(idle_close_refusal(UpdateReadiness::Unready, &state).contains("in flight"));
}

#[test]
fn queued_interrupt_rechecks_background_tasks_at_provider_dispatch() {
    struct GatedBackground {
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
        interrupts: Arc<std::sync::atomic::AtomicUsize>,
        started: bool,
    }
    impl Adapter for GatedBackground {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![
                    Event::Ready(true),
                    Event::Capabilities(super::super::Capabilities {
                        interrupt_stops_background: true,
                        ..Default::default()
                    }),
                ],
                ..Default::default()
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            match command {
                Command::Compact if self.started => Ok(Step {
                    events: (8..40)
                        .map(|index| Event::Subagent {
                            model: None,
                            waiting: None,
                            detail: None,
                            id: format!("late-background-{index}"),
                            kind: String::new(),
                            description: String::new(),
                            status: Some(ToolStatus::Succeeded),
                            background: None,
                        })
                        .collect(),
                    ..Default::default()
                }),
                Command::Compact => {
                    self.entered.send(()).unwrap();
                    self.release.recv_timeout(Duration::from_secs(5)).unwrap();
                    self.started = true;
                    Ok(Step {
                        events: (0..40)
                            .map(|index| Event::Subagent {
                                model: None,
                                waiting: None,
                                detail: None,
                                id: format!("late-background-{index}"),
                                kind: "Started after the caller observed idle".into(),
                                description: "Started after the caller observed idle".into(),
                                status: Some(ToolStatus::Running),
                                background: Some(true),
                            })
                            .collect(),
                        ..Default::default()
                    })
                }
                Command::Interrupt => {
                    self.interrupts.fetch_add(1, Ordering::Relaxed);
                    Ok(Step {
                        events: (0..40)
                            .map(|index| Event::Subagent {
                                model: None,
                                waiting: None,
                                detail: None,
                                id: format!("late-background-{index}"),
                                kind: "Explore".into(),
                                description: String::new(),
                                status: Some(ToolStatus::Stopped),
                                background: Some(true),
                            })
                            .collect(),
                        ..Default::default()
                    })
                }
                _ => Ok(Step::default()),
            }
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    let interrupts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut adapter = GatedBackground {
        entered: entered_tx,
        release: release_rx,
        interrupts: interrupts.clone(),
        started: false,
    };
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(adapter), initial, store, None, |_| {}).unwrap();
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        UpdateReadiness::Idle
    );
    let (reply, compact) = crossbeam_channel::bounded(1);
    handle
        .0
        .commands
        .send((
            ActorCommand::Provider {
                run: None,
                decline_only: false,
                command: Command::Compact,
            },
            reply,
        ))
        .unwrap();
    handle.wake();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        handle.interrupt_subagents().is_empty(),
        "caller snapshot precedes task arrival"
    );
    let (reply, interrupt) = crossbeam_channel::bounded(1);
    handle
        .0
        .commands
        .send((
            ActorCommand::Interrupt {
                run: None,
                including_subagents: false,
            },
            reply,
        ))
        .unwrap();
    handle.wake();
    release_tx.send(()).unwrap();
    compact
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    let refused = String::from(
        interrupt
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err(),
    );
    assert!(
        refused.starts_with("Interrupting would also stop 40 running background subagents.")
            && refused.contains("\u{201c}Started after the caller observed idle\u{201d}")
            && refused.contains("and 37 more are still running")
            && refused.contains("--including-subagents"),
        "{refused}"
    );
    assert!(
        !refused.contains("late-background"),
        "transport IDs are not names: {refused}"
    );
    assert_eq!(interrupts.load(Ordering::Relaxed), 0);
    assert_eq!(handle.background_subagents().len(), 40);
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        UpdateReadiness::Busy,
        "idle parent with background work must hold a When idle update"
    );
    handle.command(Command::Compact).unwrap();
    assert_eq!(
        handle.background_subagents().len(),
        8,
        "finishing the newest32 must leave the older8 authoritative"
    );
    assert_eq!(
        handle.check_update_readiness().unwrap(),
        UpdateReadiness::Busy
    );
    let refused = handle.interrupt_owned(None, false).unwrap_err();
    assert!(
        refused.contains("8 running background subagents")
            && refused.contains("\u{201c}Started after the caller observed idle\u{201d}"),
        "{refused}"
    );
    assert_eq!(interrupts.load(Ordering::Relaxed), 0);
    handle.interrupt_owned(None, true).unwrap();
    assert_eq!(interrupts.load(Ordering::Relaxed), 1);
    assert!(handle.background_subagents().is_empty());
    handle.interrupt_owned(None, false).unwrap();
    assert_eq!(
        interrupts.load(Ordering::Relaxed),
        2,
        "completed tasks no longer require consent"
    );
    handle.close().unwrap();
}

fn text(id: &str, value: &str) -> Item {
    Item {
        id: id.into(),
        role: Role::Assistant,
        content: ItemContent::Text {
            text: value.into(),
            thinking: false,
            phase: Phase::Final,
        },
        started_at: None,
        at: None,
    }
}
/// Stamps are wall-clock readings, so structural comparisons drop them and
/// leave the timing contract to the test that is actually about timing.
fn unstamped(items: &[Arc<Item>]) -> Vec<Item> {
    items
        .iter()
        .map(|item| {
            let mut item = (**item).clone();
            item.at = None;
            item.started_at = None;
            item
        })
        .collect()
}
#[test]
fn child_transcript_is_separate_and_bounded() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    apply_event(
        &mut state,
        &mut indices,
        Event::Subagent {
            model: None,
            waiting: None,
            detail: None,
            id: "agent-tool".into(),
            kind: "Explore".into(),
            description: "Inspect".into(),
            status: None,
            background: Some(true),
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::SubagentItem {
            id: "agent-tool".into(),
            item: text("child-one", "Found a path"),
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::SubagentTextDelta {
            id: "agent-tool".into(),
            item_id: "child-one".into(),
            role: Role::Assistant,
            thinking: false,
            delta: " and a test".into(),
        },
    );
    assert!(state.items.is_empty());
    assert_eq!(
        state.subagents[0].items[0].content,
        text("child-one", "Found a path and a test").content
    );
    for index in 0..80 {
        apply_event(
            &mut state,
            &mut indices,
            Event::SubagentItem {
                id: "agent-tool".into(),
                item: text(&format!("child-{index}"), "x"),
            },
        );
    }
    assert_eq!(state.subagents[0].items.len(), 64);
    apply_event(
        &mut state,
        &mut indices,
        Event::Subagent {
            model: None,
            waiting: None,
            detail: None,
            id: "agent-tool".into(),
            kind: String::new(),
            description: String::new(),
            status: Some(ToolStatus::Succeeded),
            background: None,
        },
    );
    assert_eq!(state.subagents[0].status, ToolStatus::Succeeded);
    // An update that does not say leaves what is known in place.
    assert!(state.subagents[0].background);
}
#[test]
fn running_subagent_ownership_outlives_bounded_rich_history() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    for index in 0..40 {
        let id = format!("child-{index}");
        apply_event(
            &mut state,
            &mut indices,
            Event::Subagent {
                model: None,
                waiting: None,
                detail: None,
                id: id.clone(),
                kind: "Explore".into(),
                description: "Background".into(),
                status: Some(ToolStatus::Running),
                background: Some(true),
            },
        );
        apply_event(
            &mut state,
            &mut indices,
            Event::SubagentItem {
                id,
                item: text("answer", "Retained child history"),
            },
        );
    }
    assert_eq!(state.subagents.len(), 40);
    assert_eq!(
        state
            .subagents
            .iter()
            .filter(|agent| !agent.items.is_empty())
            .count(),
        32
    );
    assert_eq!(state.subagents[0].id, "child-0");
    assert!(state.subagents[0].items.is_empty());
    // Additional output from an old active child must not grow its rich
    // history again merely because its identity remains live.
    apply_event(
        &mut state,
        &mut indices,
        Event::SubagentItem {
            id: "child-0".into(),
            item: text("new-answer", "More background output"),
        },
    );
    assert!(state.subagents[0].items.is_empty());
    for index in 8..40 {
        apply_event(
            &mut state,
            &mut indices,
            Event::Subagent {
                model: None,
                waiting: None,
                detail: None,
                id: format!("child-{index}"),
                kind: String::new(),
                description: String::new(),
                status: Some(ToolStatus::Succeeded),
                background: None,
            },
        );
    }
    assert!(state.is_working(), "the eight older tasks still own work");
    let running: Vec<_> = state
        .subagents
        .iter()
        .filter(|agent| agent.status == ToolStatus::Running)
        .map(|agent| agent.id.as_str())
        .collect();
    assert_eq!(
        running,
        (0..8)
            .map(|index| format!("child-{index}"))
            .collect::<Vec<_>>()
    );
    let capabilities = super::super::Capabilities {
        interrupt_stops_background: true,
        ..Default::default()
    };
    assert_eq!(
        state
            .subagents
            .iter()
            .filter(|agent| agent.stopped_by_interrupt(&capabilities))
            .count(),
        8
    );
    for index in 0..8 {
        apply_event(
            &mut state,
            &mut indices,
            Event::Subagent {
                model: None,
                waiting: None,
                detail: None,
                id: format!("child-{index}"),
                kind: String::new(),
                description: String::new(),
                status: Some(ToolStatus::Stopped),
                background: None,
            },
        );
    }
    assert!(!state.is_working());
    assert!(state.subagents.len() <= 32);
}

#[test]
fn turn_generation_retains_boundaries_across_coalesced_publications() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    for _ in 0..2 {
        apply_event(&mut state, &mut indices, Event::TurnStarted);
        apply_event(&mut state, &mut indices, Event::TurnSettled);
    }
    apply_event(&mut state, &mut indices, Event::TurnStarted);
    assert_eq!(state.turn_generation, 3);
    assert_eq!(state.generation, 0, "publication generation is independent");
}

#[test]
fn the_provider_exiting_closes_an_agent_waiting_on_its_background_work() {
    let mut state = Snapshot::default();
    let mut indices = Default::default();
    for (id, status) in [
        ("waiting", ToolStatus::Waiting),
        ("done", ToolStatus::Succeeded),
    ] {
        apply_event(
            &mut state,
            &mut indices,
            Event::Subagent {
                id: id.into(),
                kind: String::new(),
                description: String::new(),
                status: Some(status),
                background: Some(true),
                model: None,
                waiting: None,
                detail: Some("may be interim".into()),
            },
        );
    }
    apply_event(&mut state, &mut indices, Event::Exited { code: None });
    assert_eq!(state.subagents[0].status, ToolStatus::Stopped);
    assert!(state.subagents[0].detail.contains("provider exited"));
    assert_eq!(state.subagents[1].status, ToolStatus::Succeeded);
}

#[test]
fn a_waiting_agent_outlives_the_finished_stamp_and_is_counted_as_background_work() {
    let mut state = Snapshot::default();
    let mut indices = Default::default();
    apply_event(
        &mut state,
        &mut indices,
        Event::Subagent {
            id: "tool".into(),
            kind: String::new(),
            description: String::new(),
            status: Some(ToolStatus::Waiting),
            background: Some(true),
            model: None,
            waiting: None,
            detail: None,
        },
    );
    assert_eq!(state.activity(), ActivityState::Idle);
    assert!(state.subagents[0].status.is_live());
}

/// Readiness is what a sender waits on, so it ends with the provider.
#[test]
fn a_snapshot_is_ready_from_its_providers_word_until_it_exits() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    assert!(!state.ready);
    apply_event(&mut state, &mut indices, Event::Ready(true));
    assert!(state.ready);
    apply_event(&mut state, &mut indices, Event::Exited { code: Some(0) });
    assert!(!state.ready);
}
#[test]
fn permission_denials_are_bounded_deduplicated_and_dismissible() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let denial = |id: &str| super::super::PermissionDenial {
        tool_use_id: id.into(),
        tool_name: "Bash".into(),
        tool_input: Some("{\"command\":\"git push\"}".into()),
        reason: "Classifier denied push".into(),
        reason_type: Some("classifier".into()),
        agent_id: Some("child".into()),
    };
    apply_event(
        &mut state,
        &mut indices,
        Event::PermissionDenied(denial("one")),
    );
    let mut duplicate = denial("one");
    duplicate.reason.clear();
    duplicate.reason_type = None;
    duplicate.agent_id = None;
    apply_event(&mut state, &mut indices, Event::PermissionDenied(duplicate));
    assert_eq!(state.permission_denials.len(), 1);
    assert_eq!(state.permission_denials[0].reason, "Classifier denied push");
    assert_eq!(
        state.permission_denials[0].agent_id.as_deref(),
        Some("child")
    );
    apply_event(&mut state, &mut indices, Event::TurnStarted);
    assert_eq!(state.permission_denials.len(), 1);
    let encoded = postcard::to_stdvec(&state).unwrap();
    assert_eq!(postcard::from_bytes::<Snapshot>(&encoded).unwrap(), state);
    apply_event(
        &mut state,
        &mut indices,
        Event::PermissionDenialDismissed {
            tool_use_id: "one".into(),
        },
    );
    assert!(state.permission_denials.is_empty());
    for id in 0..70 {
        apply_event(
            &mut state,
            &mut indices,
            Event::PermissionDenied(denial(&id.to_string())),
        );
    }
    assert_eq!(
        state.permission_denials.len(),
        super::super::PermissionDenial::LIMIT
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::Reset {
            reason: "Reset".into(),
        },
    );
    assert!(state.permission_denials.is_empty());
}
#[test]
fn asynchronous_history_loading_ends_on_response_reset_or_provider_exit() {
    for completion in [
        Event::HistoryLoading(false),
        Event::History {
            items: Vec::new(),
            has_earlier: false,
        },
        Event::Reset {
            reason: "new conversation".into(),
        },
        Event::Error {
            message: "provider failed".into(),
            fatal: true,
        },
        Event::Exited { code: None },
    ] {
        let mut state = Snapshot::default();
        let mut indices = BTreeMap::new();
        apply_event(&mut state, &mut indices, Event::HistoryLoading(true));
        assert!(state.history_loading);
        apply_event(&mut state, &mut indices, completion);
        assert!(!state.history_loading);
    }
}

#[test]
fn a_new_provider_turn_recovers_from_a_prior_nonfatal_error() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let mut history_error = None;
    let mut apply = |state: &mut Snapshot, event: Event| {
        recover_history_error(&mut state.error, &mut history_error, &event);
        apply_event(state, &mut indices, event);
    };
    apply(
        &mut state,
        Event::Error {
            message: "Prior turn failed".into(),
            fatal: false,
        },
    );
    assert_eq!(state.error.as_deref(), Some("Prior turn failed"));
    apply(
        &mut state,
        Event::Status {
            busy: false,
            detail: String::new(),
        },
    );
    assert!(state.error.is_some());
    apply(&mut state, Event::TurnStarted);
    apply(
        &mut state,
        Event::Status {
            busy: true,
            detail: String::new(),
        },
    );
    assert_eq!(state.error, None);
    assert_eq!(state.activity(), ActivityState::Working);
    apply(&mut state, Event::TurnSettled);
    apply(
        &mut state,
        Event::Status {
            busy: false,
            detail: String::new(),
        },
    );
    assert_eq!(state.error, None);
    assert_eq!(state.activity(), ActivityState::Idle);
    apply(
        &mut state,
        Event::HistoryError("History page failed".into()),
    );
    apply(&mut state, Event::TurnStarted);
    assert_eq!(state.error.as_deref(), Some("History page failed"));
    apply(&mut state, Event::HistoryRecovered);
    assert_eq!(state.error, None);
}

fn codex_child_name_adapter(directory: &std::path::Path) -> Box<dyn Adapter> {
    let launch = super::super::Launch {
        provider: crate::agent::Provider::Codex,
        session_id: Some("parent-exact".into()),
        recipe: crate::model::LaunchRecipe {
            program: "codex".into(),
            arguments: Vec::new(),
            working_directory: directory.into(),
            environment: BTreeMap::from([(
                "CODEX_HOME".into(),
                directory.to_str().unwrap().into(),
            )]),
            unset_environment: Default::default(),
        },
    };
    // Build an owned-listener spec without creating a process or socket.
    super::super::protocol::codex::create_with_listener(&launch, directory.join("unused.sock"))
        .unwrap()
        .1
}

#[test]
fn codex_child_name_survives_sparse_metadata_and_takes_a_genuine_rename() {
    let directory = tempfile::tempdir().unwrap();
    let mut adapter = codex_child_name_adapter(directory.path());
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let hello = adapter.initialize().unwrap();
    let initialized = adapter
        .receive(json!({"id":hello.writes[0]["id"],"result":{
            "codexHome":directory.path().to_str().unwrap(),"userAgent":"codex-child-name-fixture"
        }}))
        .unwrap();
    let attach = initialized
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/resume")
        .unwrap();
    let attached = adapter
        .receive(json!({"id":attach["id"],"result":{
            "thread":{"id":"parent-exact","status":{"type":"idle"}},
            "initialTurnsPage":{"data":[],"nextCursor":null}
        }}))
        .unwrap();
    let skills = initialized
        .writes
        .iter()
        .find(|frame| frame["method"] == "skills/list")
        .unwrap();
    let ready = adapter
        .receive(json!({"id":skills["id"],"result":{"data":[]}}))
        .unwrap();
    assert!(ready.events.contains(&Event::Ready(true)));
    for event in attached.events.into_iter().chain(ready.events) {
        apply_event(&mut state, &mut indices, event);
    }
    let mut publish = |thread: Value| {
        let mut thread = thread;
        thread["parentThreadId"] = json!("parent-exact");
        let step = adapter
            .receive(json!({"method":"thread/started","params":{"thread":thread}}))
            .unwrap();
        for event in step.events {
            apply_event(&mut state, &mut indices, event);
        }
        state
            .subagents
            .iter()
            .map(|agent| (agent.id.clone(), agent.kind.clone()))
            .collect::<Vec<_>>()
    };
    let names = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect::<Vec<_>>()
    };
    // A role is not a name: the unnamed child stays unnamed for the tabs
    // to label, rather than showing "worker" or a provider default.
    assert_eq!(
        publish(json!({"id":"unnamed","agentRole":"worker"})),
        names(&[("unnamed", "")])
    );
    assert_eq!(
        publish(json!({"id":"child","agentNickname":" Scout ","agentRole":"worker"})),
        names(&[("unnamed", ""), ("child", "Scout")])
    );
    for sparse in [
        json!({"id":"child","agentRole":"explorer"}),
        json!({"id":"child","agentNickname":null}),
        json!({"id":"child","agentNickname":" \t\n"}),
    ] {
        assert_eq!(
            publish(sparse),
            names(&[("unnamed", ""), ("child", "Scout")]),
            "sparse metadata keeps the known name"
        );
    }
    assert_eq!(
        publish(json!({"id":"child","agentNickname":"Pilot"})),
        names(&[("unnamed", ""), ("child", "Pilot")]),
        "a genuine new nickname replaces the old one"
    );
}

/// A Codex parser attached to `parent-exact` with `latest` as its latest
/// history page, older history and its child listing still outstanding,
/// and the snapshot it has built. The value is the listing request's ID.
fn codex_child_name_session(
    directory: &std::path::Path,
    latest: Vec<Value>,
) -> (Box<dyn Adapter>, Snapshot, BTreeMap<String, usize>, Value) {
    let mut adapter = codex_child_name_adapter(directory);
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let hello = adapter.initialize().unwrap();
    let initialized = adapter
        .receive(json!({"id":hello.writes[0]["id"],"result":{
            "codexHome":directory.to_str().unwrap(),"userAgent":"codex-child-name-fixture"
        }}))
        .unwrap();
    let attach = initialized
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/resume")
        .unwrap();
    let attached = adapter
        .receive(json!({"id":attach["id"],"result":{
            "thread":{"id":"parent-exact","status":{"type":"idle"}},
            "initialTurnsPage":{"data":latest,"nextCursor":"older"}
        }}))
        .unwrap();
    let list = attached
        .writes
        .iter()
        .find(|frame| frame["method"] == "thread/list")
        .unwrap()["id"]
        .clone();
    let skills = initialized
        .writes
        .iter()
        .find(|frame| frame["method"] == "skills/list")
        .unwrap();
    let ready = adapter
        .receive(json!({"id":skills["id"],"result":{"data":[]}}))
        .unwrap();
    assert!(ready.events.contains(&Event::Ready(true)));
    for event in attached.events.into_iter().chain(ready.events) {
        apply_event(&mut state, &mut indices, event);
    }
    (adapter, state, indices, list)
}

/// A completed parent turn that spawned children, as Codex records it in
/// history, with each child's nickname at the time.
fn codex_spawn_turn(turn: &str, children: &[(&str, &str)]) -> Value {
    let ids = children.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let states = children
        .iter()
        .map(|(id, name)| ((*id).to_owned(), json!({"agentNickname":name})))
        .collect::<serde_json::Map<_, _>>();
    json!({"id":turn,"status":"completed","itemsView":"full","items":[{
        "id":format!("spawn-{turn}"),"type":"collabAgentToolCall","tool":"spawnAgent",
        "status":"completed","senderThreadId":"parent-exact","receiverThreadIds":ids,
        "agentsStates":states
    }]})
}

fn codex_child_names(state: &Snapshot) -> Vec<(String, String)> {
    state
        .subagents
        .iter()
        .map(|agent| (agent.id.clone(), agent.kind.clone()))
        .collect()
}

fn codex_child_read(step: &Step, child: &str) -> Value {
    step.writes
        .iter()
        .find(|frame| frame["method"] == "thread/resume" && frame["params"]["threadId"] == child)
        .unwrap()["id"]
        .clone()
}

#[test]
fn codex_history_and_listing_replies_cannot_rewind_a_live_child_name() {
    let directory = tempfile::tempdir().unwrap();
    let (mut adapter, mut state, mut indices, list) = codex_child_name_session(
        directory.path(),
        vec![codex_spawn_turn("latest", &[("other", "Mapper")])],
    );
    let mut apply = |step: Step, state: &mut Snapshot| {
        for event in step.events {
            apply_event(state, &mut indices, event);
        }
    };
    let names = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(codex_child_names(&state), names(&[("other", "Mapper")]));
    // Older history and the child listing are requested before Codex
    // reports the child live under its current name.
    let earlier = adapter.command(Command::LoadEarlier).unwrap().writes[0]["id"].clone();
    let started = adapter
        .receive(json!({"method":"thread/started","params":{"thread":{
            "id":"child","parentThreadId":"parent-exact","agentNickname":"Navigator",
            "status":{"type":"active"}
        }}}))
        .unwrap();
    let read = codex_child_read(&started, "child");
    apply(started, &mut state);
    // The listing is newer than any recorded call, older than live news.
    let listed = adapter
        .receive(json!({"id":list,"result":{"data":[
            {"id":"child","parentThreadId":"parent-exact","agentNickname":"Scout"},
            {"id":"other","parentThreadId":"parent-exact","agentNickname":"Cartographer"}
        ]}}))
        .unwrap();
    apply(listed, &mut state);
    assert_eq!(
        codex_child_names(&state),
        names(&[("other", "Cartographer"), ("child", "Navigator")]),
    );
    let page = adapter
        .receive(json!({"id":earlier,"result":{
            "data":[
                codex_spawn_turn("newer", &[("child", "Scout"), ("other", "Old mapper"), ("third", "Second")]),
                codex_spawn_turn("older", &[("third", "First")]),
            ],
            "nextCursor":null
        }}))
        .unwrap();
    apply(page, &mut state);
    assert_eq!(
        codex_child_names(&state),
        names(&[
            ("other", "Cartographer"),
            ("child", "Navigator"),
            ("third", "Second")
        ]),
        "a page names an unknown child by its latest call, and no other"
    );
    // The thread read requested with the live name cannot replace it.
    let resumed = adapter
        .receive(json!({"id":read,"result":{
            "thread":{"id":"child","agentNickname":"Scout","status":{"type":"active"}},
            "initialTurnsPage":{"data":[],"nextCursor":null}
        }}))
        .unwrap();
    apply(resumed, &mut state);
    assert_eq!(state.subagents[1].kind, "Navigator");
    let renamed = adapter
        .receive(json!({"method":"thread/started","params":{"thread":{
            "id":"child","parentThreadId":"parent-exact","agentNickname":"Pilot"
        }}}))
        .unwrap();
    apply(renamed, &mut state);
    assert_eq!(
        state.subagents[1].kind, "Pilot",
        "a live rename still applies"
    );
}

#[test]
fn codex_child_thread_read_names_a_child_discovered_without_one() {
    let directory = tempfile::tempdir().unwrap();
    let (mut adapter, mut state, mut indices, _) =
        codex_child_name_session(directory.path(), Vec::new());
    let mut apply = |step: Step, state: &mut Snapshot| {
        for event in step.events {
            apply_event(state, &mut indices, event);
        }
    };
    let started = adapter
        .receive(json!({"method":"thread/started","params":{"thread":{
            "id":"unnamed","parentThreadId":"parent-exact","agentRole":"worker",
            "status":{"type":"active"}
        }}}))
        .unwrap();
    let unnamed = codex_child_read(&started, "unnamed");
    apply(started, &mut state);
    let earlier = adapter.command(Command::LoadEarlier).unwrap().writes[0]["id"].clone();
    let page = adapter
        .receive(json!({"id":earlier,"result":{
            "data":[codex_spawn_turn("spawn", &[("recorded", "Scout")])],
            "nextCursor":"oldest"
        }}))
        .unwrap();
    let recorded = codex_child_read(&page, "recorded");
    apply(page, &mut state);
    assert_eq!(
        codex_child_names(&state),
        [("unnamed", ""), ("recorded", "Scout")].map(|(id, name)| (id.into(), name.into())),
    );
    for (read, id, name) in [
        (unnamed, "unnamed", " Quinn "),
        (recorded, "recorded", "Navigator"),
    ] {
        let resumed = adapter
            .receive(json!({"id":read,"result":{
                "thread":{"id":id,"agentNickname":name,"status":{"type":"idle"}},
                "initialTurnsPage":{"data":[],"nextCursor":null}
            }}))
            .unwrap();
        apply(resumed, &mut state);
    }
    assert_eq!(
        codex_child_names(&state),
        [("unnamed", "Quinn"), ("recorded", "Navigator")]
            .map(|(id, name)| (id.into(), name.into())),
        "the child's own thread read reports its current name"
    );
    let earlier = adapter.command(Command::LoadEarlier).unwrap().writes[0]["id"].clone();
    let page = adapter
        .receive(json!({"id":earlier,"result":{
            "data":[codex_spawn_turn("first", &[("recorded", "Even older")])],
            "nextCursor":null
        }}))
        .unwrap();
    apply(page, &mut state);
    assert_eq!(state.subagents[1].kind, "Navigator");
}

#[test]
fn child_metadata_survives_sparse_updates_and_completion_clears_waiting() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    apply_event(
        &mut state,
        &mut indices,
        Event::Subagent {
            id: "child".into(),
            kind: "Codex agent".into(),
            description: "Inspect repository".into(),
            status: Some(ToolStatus::Running),
            background: Some(true),
            model: Some("gpt-child".into()),
            waiting: Some(true),
            detail: Some("Waiting for your approval".into()),
        },
    );
    assert_eq!(state.activity(), ActivityState::Waiting);
    apply_event(
        &mut state,
        &mut indices,
        Event::Subagent {
            id: "child".into(),
            kind: String::new(),
            description: String::new(),
            status: None,
            background: None,
            model: None,
            waiting: None,
            detail: None,
        },
    );
    assert_eq!(state.subagents[0].model.as_deref(), Some("gpt-child"));
    assert!(state.subagents[0].waiting);
    apply_event(
        &mut state,
        &mut indices,
        Event::Subagent {
            id: "child".into(),
            kind: String::new(),
            description: String::new(),
            status: Some(ToolStatus::Succeeded),
            background: None,
            model: None,
            waiting: None,
            detail: None,
        },
    );
    assert_eq!(state.activity(), ActivityState::Idle);
    assert_eq!(state.subagents[0].model.as_deref(), Some("gpt-child"));
    assert!(!state.subagents[0].waiting);
    assert!(state.subagents[0].detail.is_empty());
}

#[test]
fn outstanding_decisions_remain_waiting_after_streaming_stops() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    apply_event(
        &mut state,
        &mut indices,
        Event::Status {
            busy: true,
            detail: String::new(),
        },
    );
    assert_eq!(state.activity(), ActivityState::Working);
    apply_event(
        &mut state,
        &mut indices,
        Event::Request(super::super::Request {
            kind: crate::chat::RequestKind::Approval,
            id: "approval".into(),
            title: String::new(),
            description: String::new(),
            fields: Vec::new(),
            actions: Vec::new(),
        }),
    );
    assert_eq!(state.activity(), ActivityState::Waiting);
    apply_event(
        &mut state,
        &mut indices,
        Event::Status {
            busy: false,
            detail: String::new(),
        },
    );
    assert_eq!(state.activity(), ActivityState::Waiting);
    apply_event(
        &mut state,
        &mut indices,
        Event::RequestResolved {
            id: "approval".into(),
        },
    );
    assert_eq!(state.activity(), ActivityState::Idle);
    apply_event(&mut state, &mut indices, Event::Waiting(true));
    assert_eq!(state.activity(), ActivityState::Waiting);
    apply_event(&mut state, &mut indices, Event::Exited { code: None });
    assert_eq!(state.activity(), ActivityState::Unknown);
}
#[test]
fn older_history_cannot_replace_a_live_stream_and_reset_invalidates_identity() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    apply_event(
        &mut state,
        &mut indices,
        Event::Session {
            id: "exact-old".into(),
            title: None,
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::TextDelta {
            id: "live".into(),
            role: Role::Assistant,
            thinking: false,
            delta: "live".into(),
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::History {
            items: vec![
                text("older", "history"),
                text("live", "stale"),
                text("older", "duplicate"),
            ],
            has_earlier: true,
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::TextDelta {
            id: "live".into(),
            role: Role::Assistant,
            thinking: false,
            delta: " update".into(),
        },
    );
    assert_eq!(
        unstamped(&state.items),
        vec![text("older", "history"), text("live", "live update")]
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::Reset {
            reason: "upstream clear".into(),
        },
    );
    assert!(state.session_id.is_none());
    assert!(state.items.is_empty());
    apply_event(
        &mut state,
        &mut indices,
        Event::Item(text("live", "new conversation")),
    );
    assert_eq!(
        unstamped(&state.items),
        vec![text("live", "new conversation")]
    );
}
#[test]
fn replayed_parent_and_child_items_keep_the_first_live_delta_start() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let tool = Item {
        id: "tool".into(),
        role: Role::Assistant,
        content: ItemContent::Tool {
            name: "exec".into(),
            input: "run".into(),
            output: String::new(),
            status: super::super::ToolStatus::Running,
        },
        started_at: None,
        at: None,
    };
    let replayed = vec![text("text", ""), tool];
    apply_event(
        &mut state,
        &mut indices,
        Event::History {
            items: replayed.clone(),
            has_earlier: false,
        },
    );
    subagent(&mut state, "child").items = replayed.into_iter().map(Arc::new).collect();
    let mut initial_starts = Vec::new();
    for offset in 0..2 {
        for event in [
            Event::TextDelta {
                id: "text".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "x".into(),
            },
            Event::OutputDelta {
                id: "tool".into(),
                offset,
                delta: "x".into(),
            },
            Event::SubagentTextDelta {
                id: "child".into(),
                item_id: "text".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "x".into(),
            },
            Event::SubagentOutputDelta {
                id: "child".into(),
                item_id: "tool".into(),
                offset,
                delta: "x".into(),
            },
        ] {
            apply_event(&mut state, &mut indices, event);
        }
        let starts: Vec<_> = state
            .items
            .iter()
            .chain(&state.subagents[0].items)
            .map(|item| {
                item.started_at
                    .expect("the first live delta stamps replayed items")
            })
            .collect();
        if offset == 0 {
            initial_starts = starts;
            for item in state.items.iter_mut().chain(&mut state.subagents[0].items) {
                let item = Arc::make_mut(item);
                item.at = Some(item.started_at.unwrap() + 1_000);
            }
        } else {
            assert_eq!(
                starts, initial_starts,
                "later output must not move the start"
            );
        }
    }
}

#[test]
fn observed_items_are_stamped_and_replayed_history_is_not() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    apply_event(&mut state, &mut indices, Event::Item(text("live", "first")));
    let created = state.items[0].at.expect("an observed item carries a stamp");
    assert_eq!(state.items[0].started_at, Some(created));
    apply_event(
        &mut state,
        &mut indices,
        Event::TextDelta {
            id: "live".into(),
            role: Role::Assistant,
            thinking: false,
            delta: " more".into(),
        },
    );
    let streamed = state.items[0]
        .at
        .expect("accumulating a delta keeps the item stamped");
    assert!(streamed >= created);
    assert_eq!(state.items[0].started_at, Some(created));
    apply_event(
        &mut state,
        &mut indices,
        Event::History {
            items: vec![text("older", "replay")],
            has_earlier: false,
        },
    );
    assert_eq!(state.items[0].at, None);
    assert_eq!(state.items[1].at, Some(streamed));
}

#[test]
fn confirmed_default_reasoning_clears_override_but_catalog_refresh_does_not() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    apply_event(
        &mut state,
        &mut indices,
        Event::Models {
            models: Vec::new(),
            selected: Some("model".into()),
            reasoning: Some("high".into()),
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::Models {
            models: Vec::new(),
            selected: None,
            reasoning: None,
        },
    );
    assert_eq!(state.reasoning.as_deref(), Some("high"));
    apply_event(
        &mut state,
        &mut indices,
        Event::Models {
            models: Vec::new(),
            selected: Some("model".into()),
            reasoning: None,
        },
    );
    assert_eq!(state.selected_model.as_deref(), Some("model"));
    assert!(state.reasoning.is_none());
}

#[test]
fn original_launch_directory_survives_rename_and_replacement_without_recapture() {
    use std::os::unix::fs::PermissionsExt;
    for (admitted, relative_path, symlinked, cleanup_only, traverse_only) in [
        (true, false, false, false, false),
        (false, false, false, false, false),
        (false, true, false, false, false),
        (false, true, true, false, false),
        (false, true, true, true, false),
        (false, true, false, false, true),
        (false, true, true, true, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let moved = root.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("marker"), b"{\"text\":\"original cwd\"}\n").unwrap();
        std::fs::write(
            original.join("provider"),
            "#!/bin/sh\n/bin/cat marker\nexec /bin/cat\n",
        )
        .unwrap();
        std::fs::set_permissions(
            original.join("provider"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        struct RestorePermissions([std::path::PathBuf; 2]);
        impl Drop for RestorePermissions {
            fn drop(&mut self) {
                for path in &self.0 {
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
                }
            }
        }
        let _permissions = RestorePermissions([original.clone(), moved.clone()]);
        if traverse_only {
            std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o111)).unwrap();
        }
        let selected = if symlinked {
            let link = root.path().join("selected");
            std::os::unix::fs::symlink(&original, &link).unwrap();
            link
        } else {
            original.clone()
        };
        let expected = crate::agent_execution::DirectoryIdentity::from_metadata(
            &std::fs::metadata(&original).unwrap(),
        );
        let spec = ProcessSpec {
            stdout_noise: None,
            followed: false,
            execution: if cleanup_only {
                Some(Arc::new(super::super::protocol::ExecutionContext::default()))
            } else {
                admitted.then(|| {
                    Arc::new(super::super::protocol::ExecutionContext::capture(&selected).unwrap())
                })
            },
            messages: None,
            transport: TransportSpec::JsonLines,
            program: if relative_path {
                "provider"
            } else {
                "./provider"
            }
            .into(),
            arguments: Vec::new(),
            working_directory: selected,
            environment: BTreeMap::from([("PATH".into(), ".".into())]),
            unset_environment: Default::default(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let initial = adapter.initialize().unwrap();
        let handle =
            LocalHandle::start(spec, adapter, initial, Store::new().unwrap(), None, |_| {})
                .unwrap();
        until(&handle, |snapshot| {
            snapshot.items.iter().any(|item|
            matches!(&item.content, ItemContent::Text { text, .. } if text.contains("original cwd")))
        });
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("marker"), "replacement").unwrap();
        let (identity, pin) = handle.pinned_launch_directory().unwrap();
        assert_eq!(identity, expected);
        if !admitted {
            assert_ne!(
                unsafe { libc::fcntl(pin.as_raw_fd(), libc::F_GETFL) } & libc::O_PATH,
                0,
                "ordinary launch pins require traversal, not directory-list permission"
            );
        }

        let fd = unsafe {
            libc::openat(
                pin.as_raw_fd(),
                c"marker".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        assert!(fd >= 0);
        let mut contents = String::new();
        unsafe { std::fs::File::from_raw_fd(fd) }
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "{\"text\":\"original cwd\"}\n");
        assert_eq!(
            std::fs::read_to_string(original.join("marker")).unwrap(),
            "replacement"
        );
        handle.command(Command::Refresh).unwrap();
        until(&handle, |snapshot| {
            snapshot.items.iter().any(|item|
            matches!(&item.content, ItemContent::Text { text, .. } if text.contains(" world")))
        });
        handle.close().unwrap();
        assert!(handle.pinned_launch_directory().is_err());
    }
}

#[test]
fn identity_only_context_never_recaptures_a_replacement_directory() {
    let root = tempfile::tempdir().unwrap();
    let expected = crate::agent_execution::DirectoryIdentity::from_metadata(
        &std::fs::metadata(root.path()).unwrap(),
    );
    let context = Arc::new(super::super::protocol::ExecutionContext::restored(expected));
    assert!(context.pinned_launch_directory().is_err());
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: Some(context),
        messages: None,
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.path().into(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    assert!(spawn_provider(&spec, &AtomicBool::new(false), None).is_err());
}

struct Echo;
#[test]
fn successful_history_retry_clears_its_error_but_preserves_unrelated_errors() {
    struct HistoryFixture {
        attempts: usize,
    }
    impl Adapter for HistoryFixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![
                    Event::Ready(true),
                    // A full window, so only the reader asks for the earlier
                    // page.
                    Event::History {
                        items: (0..super::store::WINDOW_ITEMS)
                            .map(|index| text(&format!("current-{index}"), "current"))
                            .collect(),
                        has_earlier: true,
                    },
                ],
                ..Step::default()
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            let events = match command {
                Command::LoadEarlier => {
                    self.attempts += 1;
                    if self.attempts <= 2 {
                        vec![
                            Event::HistoryLoading(false),
                            Event::HistoryError("page failed".into()),
                        ]
                    } else {
                        vec![Event::History {
                            items: vec![text("older", "older")],
                            has_earlier: false,
                        }]
                    }
                }
                Command::SetModel { model, .. } => vec![Event::Error {
                    message: model,
                    fatal: false,
                }],
                Command::Compact => vec![Event::TurnStarted],
                _ => return Err("unexpected fixture command".into()),
            };
            Ok(Step {
                events,
                ..Step::default()
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    for unrelated in [None, Some("different failure"), Some("page failed")] {
        let spec = ProcessSpec {
            stdout_noise: None,
            followed: false,
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(HistoryFixture { attempts: 0 });
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        until(&handle, |snapshot| snapshot.ready);
        for _ in 0..2 {
            handle.command(Command::LoadEarlier).unwrap();
            assert_eq!(
                handle.snapshot(None).unwrap().error.as_deref(),
                Some("page failed")
            );
        }
        // Starting work cannot repair a failed history page.
        handle.command(Command::Compact).unwrap();
        assert_eq!(
            handle.snapshot(None).unwrap().error.as_deref(),
            Some("page failed")
        );
        if let Some(message) = unrelated {
            handle
                .command(Command::SetModel {
                    model: message.into(),
                    reasoning: None,
                })
                .unwrap();
        }
        handle.command(Command::LoadEarlier).unwrap();
        let snapshot = handle.snapshot(None).unwrap();
        assert_eq!(snapshot.error.as_deref(), unrelated);
        assert_eq!(snapshot.items[0].id, "older");
        handle.command(Command::Compact).unwrap();
        assert!(handle.snapshot(None).unwrap().error.is_none());
        handle.close().unwrap();
    }
}
/// A provider hands saved history over in pages of its own records, which
/// can amount to a few rows: a Claude page is 100 records, and in a working
/// session most are tool calls and bookkeeping the transcript does not show.
/// The window fills itself from those pages until its budget is met, once it
/// has been read and without the reader asking, then asks for nothing more.
#[test]
fn a_window_fills_itself_from_short_provider_pages_once_it_is_read() {
    const PAGE: usize = 10;
    const TOTAL: usize = 300;
    struct Pages {
        next: usize,
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Pages {
        fn page(&mut self) -> Event {
            let end = self.next;
            self.next -= PAGE;
            Event::History {
                items: (self.next..end)
                    .map(|index| text(&format!("row-{index:03}"), "row"))
                    .collect(),
                has_earlier: self.next > 0,
            }
        }
    }
    impl Adapter for Pages {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::Ready(true), self.page()],
                ..Step::default()
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            if command != Command::LoadEarlier {
                return Err("unexpected fixture command".into());
            }
            self.asked.fetch_add(1, Ordering::SeqCst);
            Ok(Step {
                events: vec![self.page()],
                ..Step::default()
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
        followed: false,
    };
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut adapter: Box<dyn Adapter> = Box::new(Pages {
        next: TOTAL,
        asked: Arc::clone(&asked),
    });
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    // The host reads windows of conversations nobody shows, as release and
    // retirement do to hold their generation. Those fetch nothing: each
    // acknowledged command below follows a whole pass of the actor.
    until(&handle, |snapshot| snapshot.items.len() == PAGE);
    handle.check_update_readiness().unwrap();
    let hidden = handle.snapshot(None).unwrap();
    handle.check_update_readiness().unwrap();
    handle.check_update_readiness().unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert_eq!(handle.snapshot(None).unwrap().generation, hidden.generation);
    handle.set_visible(true);
    let window = until(&handle, |snapshot| {
        snapshot.items.len() == super::store::WINDOW_ITEMS
    });
    let ids: Vec<&str> = window.items.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids.first(), Some(&"row-172"));
    assert_eq!(ids.last(), Some(&"row-299"));
    assert!(window.has_earlier);
    assert!(!window.has_later);
    // Until the cache holds more than the window can show, and no more.
    let pages = (super::store::WINDOW_ITEMS + 1).div_ceil(PAGE) - 1;
    assert_eq!(asked.load(Ordering::SeqCst), pages);
    // Paging back slides the window by half to end at row-236, past the
    // start of what is cached, so it fills itself again: until rows 100 to
    // 236 are cached, and it shows the newest 128 of them.
    handle.command(Command::LoadEarlier).unwrap();
    let window = until(&handle, |snapshot| {
        snapshot.items.len() == super::store::WINDOW_ITEMS
            && snapshot.items.last().unwrap().id == "row-236"
    });
    assert_eq!(window.items[0].id, "row-109");
    assert!(window.has_later);
    assert_eq!(asked.load(Ordering::SeqCst), pages + 7);
    handle.close().unwrap();
}
/// A provider that answers history later, as Codex does, refuses a second
/// request while one is loading. A reader who pages back while a page asked
/// for to fill the window is on its way is answered by that page, rather than
/// shown the provider's refusal.
#[test]
fn a_reader_paging_back_during_a_fill_waits_for_the_page_on_its_way() {
    struct Later {
        loading: bool,
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Adapter for Later {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![
                    Event::Ready(true),
                    Event::History {
                        items: vec![text("newest", "newest")],
                        has_earlier: true,
                    },
                ],
                ..Step::default()
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            match command {
                Command::LoadEarlier if self.loading => Err("history is already loading".into()),
                Command::LoadEarlier => {
                    self.loading = true;
                    self.asked.fetch_add(1, Ordering::SeqCst);
                    Ok(Step {
                        events: vec![Event::HistoryLoading(true)],
                        ..Step::default()
                    })
                }
                // Answers the outstanding request.
                Command::Compact => Ok(Step {
                    writes: vec![json!({"page": true})],
                    ..Step::default()
                }),
                _ => Err("unexpected fixture command".into()),
            }
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            self.loading = false;
            Ok(Step {
                events: vec![
                    Event::HistoryLoading(false),
                    Event::History {
                        items: vec![text("older", "older")],
                        has_earlier: false,
                    },
                ],
                ..Step::default()
            })
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
        followed: false,
    };
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut adapter: Box<dyn Adapter> = Box::new(Later {
        loading: false,
        asked: Arc::clone(&asked),
    });
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    handle.set_visible(true);
    // Reading the one-row window starts a fill.
    until(&handle, |snapshot| snapshot.history_loading);
    handle.command(Command::LoadEarlier).unwrap();
    assert!(handle.snapshot(None).unwrap().error.is_none());
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    handle.command(Command::Compact).unwrap();
    let window = until(&handle, |snapshot| snapshot.items.len() == 2);
    assert_eq!(window.items[0].id, "older");
    assert!(!window.history_loading);
    assert!(window.error.is_none());
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    handle.close().unwrap();
}
/// A provider that answers history later, as Codex does, ends a failed
/// request and then reports its error. A failed page that nobody asked for is
/// not shown and not asked for again; the reader who then asks sees why.
#[test]
fn a_failed_fill_is_not_shown_or_retried_but_the_readers_request_is() {
    struct Failing {
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Adapter for Failing {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![
                    Event::Ready(true),
                    Event::History {
                        items: vec![text("newest", "newest")],
                        has_earlier: true,
                    },
                ],
                ..Step::default()
            })
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            match command {
                Command::LoadEarlier => {
                    self.asked.fetch_add(1, Ordering::SeqCst);
                    Ok(Step {
                        events: vec![Event::HistoryLoading(true)],
                        ..Step::default()
                    })
                }
                // Answers the outstanding request.
                Command::Compact => Ok(Step {
                    writes: vec![json!({"error": true})],
                    ..Step::default()
                }),
                _ => Err("unexpected fixture command".into()),
            }
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step {
                events: vec![
                    Event::HistoryLoading(false),
                    Event::HistoryError("history refused".into()),
                ],
                ..Step::default()
            })
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
        followed: false,
    };
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut adapter: Box<dyn Adapter> = Box::new(Failing {
        asked: Arc::clone(&asked),
    });
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    handle.set_visible(true);
    // Reading the one-row window starts a fill, which fails.
    until(&handle, |snapshot| snapshot.history_loading);
    handle.command(Command::Compact).unwrap();
    until(&handle, |snapshot| !snapshot.history_loading);
    // Each acknowledged command follows a whole pass of the actor, and
    // reading the window between them would start any retry.
    handle.check_update_readiness().unwrap();
    handle.snapshot(None).unwrap();
    handle.check_update_readiness().unwrap();
    let window = handle.snapshot(None).unwrap();
    assert!(window.error.is_none(), "{:?}", window.error);
    assert!(!window.history_loading);
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // The reader's own request fails visibly.
    handle.command(Command::LoadEarlier).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    handle.command(Command::Compact).unwrap();
    let window = until(&handle, |snapshot| snapshot.error.is_some());
    assert_eq!(window.error.as_deref(), Some("history refused"));
    handle.close().unwrap();
}
impl Adapter for Echo {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: vec![json!({"text": "hello"})],
            events: Vec::new(),
        })
    }
    fn command(&mut self, command: Command) -> Result<Step, String> {
        if command != Command::Refresh {
            return Err("fixture only supports refresh".into());
        }
        Ok(Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: vec![json!({"text": " world"})],
            events: Vec::new(),
        })
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        Ok(Step {
            suppress_current_notification: false,
            notification_preview: None,
            writes: Vec::new(),
            events: vec![Event::TextDelta {
                id: "echo".into(),
                role: Role::Assistant,
                thinking: false,
                delta: frame["text"].as_str().ok_or("missing echo text")?.into(),
            }],
        })
    }
}
fn until(handle: &LocalHandle, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = handle.snapshot(None).unwrap();
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "fixture timed out: {snapshot:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn actor_publishes_completion_only_after_the_entire_frame_and_late_children() {
    struct CompletionFixture(usize);
    impl Adapter for CompletionFixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::Ready(true)],
                ..Step::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            self.0 += 1;
            let child = |status| Event::Subagent {
                model: None,
                waiting: None,
                detail: None,
                id: "late-child".into(),
                kind: "test".into(),
                description: "Work".into(),
                status: Some(status),
                background: Some(true),
            };
            let events = match self.0 {
                1 => vec![
                    Event::TurnStarted,
                    Event::TurnSettled,
                    Event::Status {
                        busy: false,
                        detail: "Idle".into(),
                    },
                    child(crate::chat::ToolStatus::Running),
                ],
                2 | 5 => vec![child(crate::chat::ToolStatus::Succeeded)],
                3 => vec![Event::Ready(true)],
                4 => vec![child(crate::chat::ToolStatus::Running)],
                _ => unreachable!(),
            };
            Ok(Step {
                events,
                ..Step::default()
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(CompletionFixture(0));
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    until(&handle, |snapshot| snapshot.ready);
    handle.command(Command::Refresh).unwrap();
    let working = handle.snapshot(None).unwrap();
    assert!(working.is_working());
    assert_eq!(working.finished_at, None);
    handle.command(Command::Refresh).unwrap();
    let first = handle.snapshot(None).unwrap().finished_at.unwrap();
    handle.command(Command::Refresh).unwrap();
    assert_eq!(handle.snapshot(None).unwrap().finished_at, Some(first));
    handle.command(Command::Refresh).unwrap();
    assert_eq!(handle.snapshot(None).unwrap().finished_at, None);
    handle.command(Command::Refresh).unwrap();
    assert!(handle.snapshot(None).unwrap().finished_at.unwrap() >= first);
    handle.close().unwrap();
}

#[test]
fn notification_preview_uses_live_store_events_without_paging_or_tool_output() {
    struct NotificationFixture(usize);
    impl Adapter for NotificationFixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::Ready(true)],
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            self.0 += 1;
            let events = match self.0 {
                1 => vec![
                    Event::Session {
                        id: "notification-fixture".into(),
                        title: Some("Repair authentication".into()),
                    },
                    Event::TurnStarted,
                    Event::Item(Item {
                        id: "tool".into(),
                        role: Role::Assistant,
                        at: None,
                        started_at: None,
                        content: ItemContent::Tool {
                            name: "test".into(),
                            input: "private arguments".into(),
                            output: "raw-tool-garbage".repeat(10000),
                            status: ToolStatus::Succeeded,
                        },
                    }),
                    Event::TextDelta {
                        id: "answer".into(),
                        role: Role::Assistant,
                        thinking: false,
                        delta: format!(
                            "## Done\nRepaired authentication.\n```sh\nraw-code-garbage\n```\nAll tests passed. {}",
                            "More result. ".repeat(10000)
                        ),
                    },
                    Event::TurnFinished {
                        key: Some("one".into()),
                        outcome: crate::notifications::Kind::Succeeded,
                    },
                ],
                2 => vec![Event::History {
                    items: vec![text("old", "Historical answer must not notify")],
                    has_earlier: false,
                }],
                3 => vec![Event::Request(super::super::Request {
                    kind: super::super::RequestKind::Question,
                    id: "question".into(),
                    title: "Migration strategy".into(),
                    description: "Choose the migration strategy?".into(),
                    fields: vec![super::super::Field {
                        id: "secret".into(),
                        label: "Secret".into(),
                        description: String::new(),
                        choices: vec![],
                        multiple: false,
                        secret: true,
                        initial: "fixture-secret".into(),
                    }],
                    actions: vec![],
                })],
                4 => vec![
                    Event::RequestResolved {
                        id: "question".into(),
                    },
                    Event::TurnStarted,
                    Event::Item(Item {
                        id: "long-command".into(),
                        role: Role::Assistant,
                        at: None,
                        started_at: None,
                        content: ItemContent::Command {
                            label: "Long command label ".repeat(1000),
                            command: "private arguments".into(),
                            working_directory: String::new(),
                            action: super::super::CommandAction::Run,
                            status: ToolStatus::Succeeded,
                            exit_code: Some(0),
                            duration_ms: None,
                            output: "raw output".into(),
                        },
                    }),
                    Event::TurnFinished {
                        key: Some("two".into()),
                        outcome: crate::notifications::Kind::Succeeded,
                    },
                ],
                _ => return Err("fixture exhausted".into()),
            };
            Ok(Step {
                events,
                ..Default::default()
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: vec![],
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(NotificationFixture(0));
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    until(&handle, |snapshot| snapshot.ready);
    handle.command(Command::Refresh).unwrap();
    let response = handle.notification_content(crate::notifications::Kind::Succeeded);
    assert_eq!(response.conversation, "Repair authentication");
    assert!(
        response
            .preview
            .starts_with("Done Repaired authentication. All tests passed.")
    );
    assert!(!response.preview.contains("garbage"));
    assert!(response.preview.chars().count() <= crate::notifications::presentation::PREVIEW_LIMIT);
    handle.command(Command::Refresh).unwrap();
    assert_eq!(
        handle.notification_content(crate::notifications::Kind::Succeeded),
        response
    );
    handle.command(Command::Refresh).unwrap();
    let waiting = handle.notification_content(crate::notifications::Kind::Waiting);
    assert_eq!(
        waiting.request,
        Some(crate::notifications::RequestType::Question)
    );
    assert_eq!(waiting.preview, "Choose the migration strategy?");
    assert!(!waiting.preview.contains("fixture-secret"));
    handle.command(Command::Refresh).unwrap();
    let content = handle.notification_content(crate::notifications::Kind::Succeeded);
    assert!(
        content
            .preview
            .starts_with("Command completed: Long command label")
    );
    assert!(content.preview.chars().count() <= crate::notifications::presentation::PREVIEW_LIMIT);
    let mut event = crate::notifications::Event::session(
        crate::model::TerminalId::new(),
        crate::agent::Provider::Codex,
        directory.path().into(),
        crate::notifications::Evidence {
            key: "long-command".into(),
            kind: crate::notifications::Kind::Succeeded,
        },
    );
    event.content = Some(content);
    event.validate().unwrap();
    assert!(
        !crate::notifications::presentation::message(&event, false)
            .body
            .contains("Long command")
    );
    handle.close().unwrap();
}

#[test]
fn notification_waiting_context_revises_without_an_activity_transition() {
    struct Requests(usize);
    impl Adapter for Requests {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::Ready(true)],
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            self.0 += 1;
            let request = |id: &str, description: &str| {
                Event::Request(super::super::Request {
                    kind: super::super::RequestKind::Question,
                    id: id.into(),
                    title: "Question".into(),
                    description: description.into(),
                    fields: vec![],
                    actions: vec![],
                })
            };
            let events = match self.0 {
                1 => vec![
                    Event::TurnStarted,
                    request("a", "First question?"),
                    request("b", "Second question?"),
                ],
                2 => vec![Event::RequestResolved { id: "a".into() }],
                3 => vec![request("b", "Updated second question?")],
                4 => vec![Event::Status {
                    busy: false,
                    detail: "Unrelated metadata".into(),
                }],
                _ => return Err("fixture exhausted".into()),
            };
            Ok(Step {
                events,
                ..Default::default()
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: vec![],
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Requests(0));
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let (send, receive) = crossbeam_channel::bounded(8);
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        if let Update::AttentionContext { evidence, .. } = update {
            let _ = send.try_send(evidence);
        }
    })
    .unwrap();
    until(&handle, |snapshot| snapshot.ready);
    let mut keys = Vec::new();
    for expected in [
        "First question?",
        "Second question?",
        "Updated second question?",
    ] {
        handle.command(Command::Refresh).unwrap();
        let evidence = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(evidence.kind, crate::notifications::Kind::Waiting);
        assert_eq!(handle.notification_content(evidence.kind).preview, expected);
        assert_eq!(
            handle.snapshot(None).unwrap().activity(),
            ActivityState::Waiting
        );
        keys.push(evidence.key);
    }
    assert!(keys[0] != keys[1] && keys[1] != keys[2]);
    handle.command(Command::Refresh).unwrap();
    assert!(
        receive.try_recv().is_err(),
        "unrelated metadata must not repeat an actionable alert"
    );
    handle.close().unwrap();
}

#[test]
fn presentation_serves_multiple_readers_and_resyncs_missed_revisions() {
    struct Updates;
    impl Adapter for Updates {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![
                    Event::Item(text("fixed", &"x".repeat(128 * 1024))),
                    Event::Item(text("live", "hello")),
                    Event::Ready(true),
                ],
                ..Step::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::TextDelta {
                    id: "live".into(),
                    role: Role::Assistant,
                    thinking: false,
                    delta: " world".into(),
                }],
                ..Step::default()
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Updates);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    let baseline = until(&handle, |snapshot| snapshot.ready);
    let fixed = baseline.items[0].clone();
    let id = crate::model::TerminalId::new();
    handle.command(Command::Refresh).unwrap();
    let Some(crate::terminal_ipc::ServerResponse::ChatUpdate { update, .. }) =
        handle.presentation(id, Some(baseline.generation))
    else {
        panic!("expected incremental update");
    };
    assert!(postcard::to_stdvec(&update).unwrap().len() < 4096);
    let first = update.clone().apply(&baseline).unwrap();
    assert!(
        Arc::ptr_eq(&fixed, &first.items[0]),
        "unchanged payload is shared across snapshots"
    );
    // A console/full-snapshot reader at the same revision must not
    // erase the incremental patch another subscriber is about to read.
    assert_eq!(handle.snapshot(None).unwrap().generation, first.generation);
    let Some(crate::terminal_ipc::ServerResponse::ChatUpdate {
        update: mut second, ..
    }) = handle.presentation(id, Some(baseline.generation))
    else {
        panic!("second reader lost its update");
    };
    // The transcript patch is retained; its source clock is freshly
    // sampled for each reader, including readers of cached revisions.
    assert!(update.metadata.sampled_at.is_some());
    assert!(second.metadata.sampled_at.is_some());
    second.metadata.sampled_at = update.metadata.sampled_at;
    assert_eq!(update, second);
    handle.command(Command::Refresh).unwrap();
    assert!(matches!(
        handle.presentation(id, Some(baseline.generation)),
        Some(crate::terminal_ipc::ServerResponse::ChatSnapshot {
            snapshot: Some(_),
            ..
        })
    ));
    assert!(matches!(
        handle.presentation(id, None),
        Some(crate::terminal_ipc::ServerResponse::ChatSnapshot {
            snapshot: Some(_),
            ..
        })
    ));
    handle.close().unwrap();
}

#[test]
fn native_child_receives_only_the_preserved_source_environment() {
    let environment = BTreeMap::from([
        ("HOME".into(), "/source-account".into()),
        ("ANTHROPIC_API_KEY".into(), "explicit-fixture-key".into()),
        ("LC_ALL".into(), "C.UTF-8".into()),
    ]);
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/usr/bin/python3".into(),
        arguments: vec!["-c".into(), "import json,os,sys; sys.stdin.readline(); print(json.dumps({'text':json.dumps(dict(os.environ))}),flush=True); sys.stdin.read()".into()],
        working_directory: std::env::temp_dir(),
        environment: environment.clone(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Echo);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| !snapshot.items.is_empty());
    let ItemContent::Text { text, .. } = &snapshot.items[0].content else {
        panic!("environment fixture did not return text");
    };
    assert_eq!(
        serde_json::from_str::<BTreeMap<String, String>>(text).unwrap(),
        environment
    );
    handle.close().unwrap();
}
#[test]
fn selected_account_removals_win_at_the_native_child_boundary() {
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        transport: TransportSpec::JsonLines,
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "read line; [ -z \"${ANTHROPIC_API_KEY+x}\" ] && [ \"$CLAUDE_CONFIG_DIR\" = /selected ] && printf '%s\\n' '{\"text\":\"isolated\"}'; cat >/dev/null".into()],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::from([
            ("ANTHROPIC_API_KEY".into(), "fixture-merged-credential".into()),
            ("CLAUDE_CONFIG_DIR".into(), "/selected".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]),
        unset_environment: ["ANTHROPIC_API_KEY".into()].into(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Echo);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| !snapshot.items.is_empty());
    assert!(
        matches!(&snapshot.items[0].content, ItemContent::Text { text, .. } if text == "isolated")
    );
    assert!(
        !handle
            .execution_environment()
            .0
            .contains_key("ANTHROPIC_API_KEY")
    );
    handle.close().unwrap();
}

#[test]
fn legacy_managed_recipe_removes_daemon_credentials_when_restarted() {
    const CHILD: &str = "CAGENTS_LEGACY_ACCOUNT_ENV_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "chat::runtime::tests::legacy_managed_recipe_removes_daemon_credentials_when_restarted", "--nocapture"])
            .env_clear().env(CHILD, "1").env("HOME", directory.path())
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("PATH", "/usr/bin:/bin").env("ANTHROPIC_API_KEY", "daemon-fixture")
            .status().unwrap().success());
        return;
    }
    let home = crate::instance::project_dirs()
        .unwrap()
        .data_dir()
        .join("accounts/claude/12345678-1234-4234-8234-123456789abc");
    crate::account::registry::Store::installation()
        .unwrap()
        .update(|registry| {
            registry.begin(
                crate::agent::Provider::Claude,
                uuid::Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap(),
                1,
            );
        })
        .unwrap();
    let legacy = serde_json::json!({
        "program": "/bin/sh",
        "arguments": ["-c", "read line; [ -z \"${ANTHROPIC_API_KEY+x}\" ] && printf '%s\\n' '{\"text\":\"isolated\"}'; cat >/dev/null"],
        "working_directory": std::env::temp_dir(),
        "environment": {"CLAUDE_CONFIG_DIR": home}
    });
    let recipe: crate::model::LaunchRecipe = serde_json::from_value(legacy).unwrap();
    assert!(
        recipe.unset_environment.is_empty(),
        "fixture must predate explicit policy"
    );
    let mut spec = ProcessSpec::from(&recipe);
    spec.environment = crate::model::agent_recovery::fresh_native_environment(
        crate::agent::Provider::Claude,
        false,
    );
    spec.environment.extend(recipe.environment.clone());
    let mut adapter: Box<dyn Adapter> = Box::new(Echo);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| !snapshot.items.is_empty());
    assert!(
        matches!(&snapshot.items[0].content, ItemContent::Text { text, .. } if text == "isolated")
    );
    assert_eq!(
        handle.execution_environment().0["CLAUDE_CONFIG_DIR"],
        home.to_str().unwrap()
    );
    assert!(
        !handle
            .execution_environment()
            .0
            .contains_key("ANTHROPIC_API_KEY")
    );
    assert!(
        recipe.unset_environment.is_empty(),
        "spawn must not rewrite live descriptor evidence"
    );
    handle.close().unwrap();
}
#[test]
fn owned_listener_startup_failure_is_reported_and_releases_its_private_directory() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("server").join("control.sock");
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::OwnedUnixWebSocket {
            socket: socket.clone(),
        },
        program: "/bin/false".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.exited);
    assert!(
        snapshot
            .error
            .as_deref()
            .unwrap()
            .contains("before opening its local socket")
    );
    let _ = handle.close();
    drop(handle);
    assert!(!socket.parent().unwrap().exists());
}
#[test]
fn an_account_report_reaches_the_surfaces_without_touching_the_conversation() {
    struct Reporter;
    impl Adapter for Reporter {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![json!({})],
                events: Vec::new(),
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("fixture takes no commands".into())
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![Event::AccountUsage(report())],
            })
        }
    }
    fn report() -> LiveReport {
        LiveReport::Claude(crate::account::claude::LiveUpdate {
            windows: Vec::new(),
            status: None,
        })
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Reporter);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let generation = |handle: &LocalHandle| {
        handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation
    };
    let (notify, notified) = crossbeam_channel::bounded(8);
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        let _ = notify.try_send(update);
    })
    .unwrap();
    let before = generation(&handle);
    let Update::AccountUsage {
        report: reported,
        observed_at,
    } = notified.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!("the report was not forwarded");
    };
    assert_eq!(reported, report());
    assert!(observed_at > 0);
    // Neither the pane nor its snapshot learns of it: a report is not
    // output an offscreen conversation would have to be read for.
    assert!(notified.recv_timeout(Duration::from_millis(200)).is_err());
    assert_eq!(generation(&handle), before);
    assert!(handle.snapshot(None).unwrap().items.is_empty());
    handle.close().unwrap();
}
/// How the provider addresses a subagent reaches the surfaces as every
/// address known so far, once each is learned, and never the snapshot,
/// whose layout older peers share.
#[test]
fn an_agent_address_reaches_the_surfaces_once_and_never_the_snapshot() {
    fn address(address: &str, subagent: &str) -> Event {
        Event::AgentAddress(AgentAddress {
            address: address.into(),
            subagent: subagent.into(),
        })
    }
    struct Addresser;
    impl Adapter for Addresser {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![json!({})],
                // As a reopened history says of the agents it started.
                events: vec![address("a1892b93e88999a7d", "toolu_relocate")],
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("fixture takes no commands".into())
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![
                    // Said again, and a later agent's first word.
                    address("a1892b93e88999a7d", "toolu_relocate"),
                    address("a77f00d1e2b3c4d5a", "toolu_review"),
                ],
            })
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Addresser);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let (notify, notified) = crossbeam_channel::unbounded();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        if let Update::AgentAddresses(addresses) = update {
            let _ = notify.send(addresses);
        }
    })
    .unwrap();
    let listed = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(address, subagent)| AgentAddress {
                address: (*address).into(),
                subagent: (*subagent).into(),
            })
            .collect::<Vec<_>>()
    };
    let first = listed(&[("a1892b93e88999a7d", "toolu_relocate")]);
    let both = listed(&[
        ("a1892b93e88999a7d", "toolu_relocate"),
        ("a77f00d1e2b3c4d5a", "toolu_review"),
    ]);
    let timeout = Duration::from_secs(2);
    assert_eq!(notified.recv_timeout(timeout).unwrap(), first);
    assert_eq!(notified.recv_timeout(timeout).unwrap(), both);
    assert!(
        notified.recv_timeout(Duration::from_millis(200)).is_err(),
        "an address already known is not news"
    );
    assert_eq!(handle.agent_addresses(), both);
    assert!(handle.snapshot(None).unwrap().subagents.is_empty());
    handle.close().unwrap();
}
/// The host records a run's outcome the moment it is announced and then
/// asks the handle which conversation produced it.
#[test]
fn a_frame_confirms_its_conversation_before_announcing_its_outcome() {
    use crate::agent_execution::ProviderEvent;
    struct WholeTurn(crate::model::RunId);
    impl Adapter for WholeTurn {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![json!({})],
                events: Vec::new(),
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Err("fixture takes no commands".into())
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            let execution = |event| Event::Execution { run: self.0, event };
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: Vec::new(),
                events: vec![
                    Event::Session {
                        id: "confirmed".into(),
                        title: None,
                    },
                    execution(ProviderEvent::Started {
                        key: "request".into(),
                    }),
                    execution(ProviderEvent::Settled),
                    execution(ProviderEvent::Finished {
                        outcome: crate::notifications::Kind::Succeeded,
                    }),
                ],
            })
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(WholeTurn(crate::model::RunId::new()));
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let (started, actor) = crossbeam_channel::bounded::<LocalHandle>(1);
    let (observe, observed) = crossbeam_channel::bounded(1);
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        if let Update::Execution {
            update: crate::agent_execution::Update::Finished { .. },
            ..
        } = update
        {
            let handle = actor.recv_timeout(Duration::from_secs(5)).unwrap();
            let _ = observe.try_send(handle.confirmed_session_id());
        }
    })
    .unwrap();
    started.send(handle.clone()).unwrap();
    assert_eq!(
        observed
            .recv_timeout(Duration::from_secs(5))
            .expect("the outcome was never announced")
            .as_deref(),
        Some("confirmed")
    );
    handle.close().unwrap();
}
#[test]
fn real_json_lines_pipe_keeps_hidden_state_without_presentation() {
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Echo);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let (notify, notified) = crossbeam_channel::bounded(8);
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        let _ = notify.try_send(update);
    })
    .unwrap();
    assert!(matches!(
        notified.recv_timeout(Duration::from_secs(2)).unwrap(),
        Update::Observed { .. }
    ));
    let generation = handle
        .0
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .generation;
    handle.command(Command::Refresh).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while handle
        .0
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .generation
        < generation + 2
    {
        assert!(Instant::now() < deadline, "hidden echo did not arrive");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(notified.try_recv().is_err(), "hidden deltas must coalesce");
    // Acknowledging an offscreen pane rearms activity, not presentation.
    handle.observe_next_update();
    handle.command(Command::Refresh).unwrap();
    assert!(matches!(
        notified.recv_timeout(Duration::from_secs(2)).unwrap(),
        Update::Observed { .. }
    ));
    while handle
        .0
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .generation
        < generation + 4
    {
        assert!(
            Instant::now() < deadline,
            "acknowledged echo did not arrive"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .items
            .is_empty()
    );
    assert!(
        handle
            .0
            .store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .dirty
    );
    handle.set_visible(true);
    assert_eq!(
        notified.recv_timeout(Duration::from_secs(2)).unwrap(),
        Update::Presentation
    );
    assert_eq!(
        unstamped(&handle.snapshot(None).unwrap().items),
        vec![text("echo", "hello world world")]
    );
    handle.close().unwrap();
    assert!(handle.is_completed());
    assert_eq!(
        unstamped(&handle.snapshot(None).unwrap().items),
        vec![text("echo", "hello world world")]
    );
}
struct WebSocketFixture;
impl Adapter for WebSocketFixture {
    fn initialize(&mut self) -> Result<Step, String> {
        Echo.initialize()
    }
    fn command(&mut self, command: Command) -> Result<Step, String> {
        if command == Command::Interrupt {
            Ok(Step {
                suppress_current_notification: false,
                notification_preview: None,
                writes: vec![json!({"interrupt": true})],
                events: Vec::new(),
            })
        } else {
            Echo.command(command)
        }
    }
    fn receive(&mut self, frame: Value) -> Result<Step, String> {
        let mut step = Echo.receive(frame.clone())?;
        step.events.push(Event::Status {
            busy: frame["busy"].as_bool().unwrap_or(false),
            detail: String::new(),
        });
        Ok(step)
    }
}

#[test]
fn real_unix_websocket_preserves_hidden_history_pongs_and_interrupts_before_close() {
    use std::os::unix::{fs::MetadataExt, net::UnixListener};
    use tungstenite::Message;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("codex.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::UnixWebSocket {
            socket: path,
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        // This executable does not exist: a socket connection must never spawn it.
        program: "/nonexistent/codex-websocket-fixture".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let (pong, pong_received) = crossbeam_channel::bounded(1);
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = tungstenite::accept_hdr(
            stream,
            |request: &tungstenite::handshake::server::Request, response| {
                assert_eq!(request.uri().path(), "/");
                assert_eq!(request.headers()["host"], "localhost");
                Ok(response)
            },
        )
        .unwrap();
        let request = socket.read().unwrap();
        assert!(matches!(&request, Message::Text(_)));
        assert_eq!(
            serde_json::from_str::<Value>(request.to_text().unwrap()).unwrap(),
            json!({"text": "hello"})
        );
        socket
            .send(Message::Text(
                json!({"text": "hello", "busy": true}).to_string().into(),
            ))
            .unwrap();
        socket
            .send(Message::Ping(b"exact-ping".to_vec().into()))
            .unwrap();
        assert_eq!(
            socket.read().unwrap(),
            Message::Pong(b"exact-ping".to_vec().into())
        );
        pong.send(()).unwrap();
        let request = socket.read().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(request.to_text().unwrap()).unwrap(),
            json!({"interrupt": true})
        );
        socket
            .send(Message::Text(
                json!({"text": " stopped", "busy": false})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        assert!(matches!(socket.read().unwrap(), Message::Close(_)));
        // Match the real Codex server: it closes the connection without
        // echoing our close frame after confirming the turn is idle.
    });
    let mut adapter: Box<dyn Adapter> = Box::new(WebSocketFixture);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let (notify, notified) = crossbeam_channel::bounded(8);
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        let _ = notify.try_send(update);
    })
    .unwrap();
    until(&handle, |snapshot| {
        snapshot.busy && unstamped(&snapshot.items) == vec![text("echo", "hello")]
    });
    pong_received.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        notified.recv_timeout(Duration::from_secs(2)).unwrap(),
        Update::Activity {
            state: ActivityState::Working,
            ..
        }
    ));
    assert!(matches!(
        notified.recv_timeout(Duration::from_secs(2)).unwrap(),
        Update::Observed { .. }
    ));
    assert!(notified.try_recv().is_err());
    let attached = handle.clone();
    attached.set_visible(false);
    drop(attached);
    assert!(!handle.is_completed());
    handle.close().unwrap();
    server.join().unwrap();
    let transitions: Vec<_> = notified
        .try_iter()
        .filter_map(|update| match update {
            Update::Activity { state, .. } => Some(state),
            Update::Presentation => panic!("hidden close must not request presentation"),
            Update::Observed { .. }
            | Update::AccountUsage { .. }
            | Update::AgentAddresses(_)
            | Update::ActiveModel { .. }
            | Update::Attention(_)
            | Update::AttentionContext { .. }
            | Update::Ready
            | Update::PromptAvailable
            | Update::RetainedOutcomes
            | Update::Execution { .. } => None,
        })
        .collect();
    assert_eq!(
        transitions,
        vec![ActivityState::Idle, ActivityState::Unknown]
    );
    let snapshot = handle.snapshot(None).unwrap();
    assert!(snapshot.exited);
    assert!(!snapshot.busy);
    assert_eq!(
        unstamped(&snapshot.items),
        vec![text("echo", "hello stopped")]
    );
}

#[test]
fn unix_websocket_rejects_oversized_frame_before_receiving_its_payload() {
    use std::os::unix::{fs::MetadataExt, net::UnixListener};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("codex.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::UnixWebSocket {
            socket: path,
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        program: "unused".into(),
        arguments: Vec::new(),
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let (release, released) = crossbeam_channel::bounded(1);
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = tungstenite::accept(stream).unwrap();
        socket.read().unwrap();
        // An unmasked final text frame with only its oversized length.
        socket.get_mut().write_all(&[0x81, 127]).unwrap();
        socket
            .get_mut()
            .write_all(&((FRAME_LIMIT + 1) as u64).to_be_bytes())
            .unwrap();
        // Keep the connection open: rejection cannot rely on EOF or timeout.
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let mut adapter: Box<dyn Adapter> = Box::new(Echo);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.exited);
    assert!(snapshot.error.is_some());
    assert!(snapshot.items.is_empty());
    handle
        .close()
        .expect("failed protocol actor can be retired after teardown");
    assert!(
        handle.failure().is_some(),
        "closing retains the failure diagnosis"
    );
    release.send(()).unwrap();
    server.join().unwrap();
}

// Use the real Claude adapter with a private synthetic executable. The
// production launcher supplies its explicit child environment; this test
// never changes caller authority or performs discovery or authentication.
fn claude_initialization_pipe(body: &str) -> (tempfile::TempDir, LocalHandle) {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("claude");
    std::fs::write(&provider, format!(
        "#!/usr/bin/python3\nimport json, sys\nrequest = json.loads(sys.stdin.readline())\nassert request['type'] == 'control_request'\nassert request['request']['subtype'] == 'initialize'\n{body}"
    )).unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launch = super::super::Launch {
        provider: crate::agent::Provider::Claude,
        recipe: crate::model::LaunchRecipe {
            program: provider.to_string_lossy().into_owned(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        },
        session_id: None,
    };
    let (spec, mut adapter) = super::super::protocol::claude::create(&launch).unwrap();
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    (directory, handle)
}

/// Wait for the fixture's initialization to end, however it ends. The
/// error says which deadline or failure ended it, so how long it took is
/// not part of any assertion.
fn finish_initialization_pipe(handle: &LocalHandle) -> Snapshot {
    let deadline = Instant::now() + IO_TIMEOUT + crate::test_timing::EVENTUALLY;
    while !handle.is_completed() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let completed = handle.is_completed();
    let snapshot = handle.snapshot(None).unwrap();
    // Retire/reap the exact fixture before an assertion can fail.
    handle.close().unwrap();
    assert!(completed, "initialization fixture timed out: {snapshot:?}");
    snapshot
}

#[test]
fn claude_advisories_do_not_disable_the_real_pipe_initialization_deadline() {
    let (_directory, handle) = claude_initialization_pipe(
        "print(json.dumps({'type': 'keep_alive'}), flush=True)\n\
             print(json.dumps({'type': 'future_advisory'}), flush=True)\n\
             sys.stdin.read()\n",
    );
    let snapshot = finish_initialization_pipe(&handle);
    assert!(!snapshot.ready);
    assert!(
        snapshot.error.as_deref().is_some_and(|error| error
            .contains("initialization timed out before the provider reported it was ready")),
        "{snapshot:?}"
    );
    assert!(handle.failure().is_some());
}

#[test]
fn claude_clean_pipe_eof_requires_initialization_but_requested_close_does_not() {
    for advisory in [false, true] {
        for noise in [0, 1, 7] {
            let body = format!(
                "if {advisory}:\n    print(json.dumps({{'type': 'future_advisory'}}), flush=True)\n\
                     for _ in range({noise}):\n    print('wrapper diagnostic', flush=True)\n",
                advisory = if advisory { "True" } else { "False" },
            );
            let (_directory, handle) = claude_initialization_pipe(&body);
            let snapshot = finish_initialization_pipe(&handle);
            assert!(!snapshot.ready);
            assert!(
                snapshot
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("exited before completing initialization")),
                "{snapshot:?}"
            );
        }
    }

    let (_directory, handle) = claude_initialization_pipe(
        "print(json.dumps({'type': 'future_advisory'}), flush=True)\n\
             for _ in range(7):\n    print('wrapper diagnostic', flush=True)\n\
             print(json.dumps({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': request['request_id'], 'response': {}}}), flush=True)\n",
    );
    let snapshot = finish_initialization_pipe(&handle);
    assert!(snapshot.error.is_none(), "{snapshot:?}");
    assert!(handle.failure().is_none());

    let (_directory, handle) = claude_initialization_pipe("sys.stdin.read()\n");
    handle.close().unwrap();
    let snapshot = handle.snapshot(None).unwrap();
    assert!(!snapshot.ready);
    assert!(snapshot.error.is_none(), "{snapshot:?}");
    assert!(handle.failure().is_none());
}

// Closing a busy turn sends Claude an interrupt. When the turn's result was
// already on its way, it arrives before the interrupt's acknowledgement,
// and only that acknowledgement decides whether the turn was cancelled.
// Close must not return in between, or the settled turn has no outcome.
#[test]
fn claude_close_keeps_a_settled_turn_outcome_until_its_interrupt_is_acknowledged() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("claude");
    std::fs::write(
        &provider,
        r#"#!/usr/bin/python3
import json, sys, time
def send(value): print(json.dumps(value), flush=True)
session = next(argument.split('=', 1)[1] for argument in sys.argv[1:]
               if argument.startswith('--session-id='))
for line in sys.stdin:
    frame = json.loads(line)
    if frame.get('type') == 'user':
        send({'type': 'system', 'subtype': 'init', 'session_id': session})
        send(dict(frame, session_id=session))
        send({'type': 'assistant', 'uuid': 'answer', 'session_id': session,
              'message': {'id': 'answer', 'role': 'assistant',
                          'content': [{'type': 'text', 'text': 'working'}]}})
    elif frame.get('type') == 'control_request':
        if frame['request'].get('subtype') == 'interrupt':
            send({'type': 'result', 'uuid': 'final', 'subtype': 'success', 'is_error': False,
                  'session_id': session, 'usage': {'input_tokens': 1, 'output_tokens': 1}})
            time.sleep(0.5)
        send({'type': 'control_response', 'response': {'subtype': 'success',
              'request_id': frame['request_id'], 'response': {}}})
"#,
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launch = super::super::Launch {
        provider: crate::agent::Provider::Claude,
        recipe: crate::model::LaunchRecipe {
            program: provider.to_string_lossy().into_owned(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        },
        session_id: None,
    };
    let (spec, mut adapter) = super::super::protocol::claude::create(&launch).unwrap();
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let (send, receive) = crossbeam_channel::unbounded();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, move |update| {
        if let Update::AttentionContext { evidence, .. } = update {
            let _ = send.send(evidence);
        }
    })
    .unwrap();
    until(&handle, |snapshot| snapshot.ready);
    handle
        .command(Command::Prompt {
            text: "A turn that settles as it is closed".into(),
            attachments: Vec::new(),
            mode: super::super::PromptMode::Send,
        })
        .unwrap();
    until(&handle, |snapshot| {
        snapshot.busy
            && snapshot.items.iter().any(
                |item| matches!(&item.content, ItemContent::Text { text, .. } if text == "working"),
            )
    });
    handle.close().unwrap();
    assert!(handle.failure().is_none());
    let outcomes: Vec<_> = receive.try_iter().map(|evidence| evidence.kind).collect();
    assert_eq!(outcomes, vec![crate::notifications::Kind::Cancelled]);
}

#[test]
fn json_line_noise_and_unknown_claude_frames_finish_through_the_real_pipe() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("claude");
    std::fs::write(
        &provider,
        include_str!("../../../tests/fixtures/claude-unknown-frames.py"),
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launch = super::super::Launch {
        provider: crate::agent::Provider::Claude,
        recipe: crate::model::LaunchRecipe {
            program: provider.to_string_lossy().into_owned(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        },
        session_id: None,
    };
    let (spec, mut adapter) = super::super::protocol::claude::create(&launch).unwrap();
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    until(&handle, |snapshot| snapshot.ready);
    handle
        .command(Command::Prompt {
            text: "Exercise unknown frames".into(),
            attachments: Vec::new(),
            mode: super::super::PromptMode::Send,
        })
        .unwrap();
    let snapshot = until(&handle, |snapshot| {
        !snapshot.busy
            && snapshot.items.iter().any(|item| {
                matches!(&item.content, ItemContent::Text { text, .. }
                    if text == "Fixture answer arrived after unknown frames and wrapper output.")
            })
    });
    assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
    assert!(!snapshot.exited);
    assert!(
        snapshot
            .items
            .iter()
            .all(|item| { !format!("{:?}", item.content).contains("fixture-private-payload") })
    );
    // Claude Code 2.1.296 sends `post_turn_summary` before each result.
    handle
        .command(Command::Prompt {
            text: "post-turn-summary check".into(),
            attachments: Vec::new(),
            mode: super::super::PromptMode::Send,
        })
        .unwrap();
    let snapshot = until(&handle, |snapshot| {
        !snapshot.busy
            && snapshot.items.iter().any(
                |item| matches!(&item.content, ItemContent::Text { text, .. } if text == "ready"),
            )
    });
    assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
    assert!(!snapshot.exited);
    handle.close().unwrap();
    assert!(handle.failure().is_none());
}

#[test]
fn an_acp_agent_works_answers_stops_and_resumes_through_the_real_pipe() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let agent = directory.path().join("fake-acp-agent");
    std::fs::write(
        &agent,
        include_str!("../../../tests/fixtures/fake-acp-agent.py"),
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launch = |session_id: Option<String>| super::super::Launch {
        // No provider runs ACP yet; the profile, not the provider, chooses it.
        provider: crate::agent::Provider::Omp,
        recipe: crate::model::LaunchRecipe {
            program: agent.to_string_lossy().into_owned(),
            arguments: vec!["--model".into(), "gemini-3.8-flash".into()],
            working_directory: directory.path().into(),
            environment: BTreeMap::from([
                (
                    "FAKE_ACP_STATE".into(),
                    directory
                        .path()
                        .join("state")
                        .to_string_lossy()
                        .into_owned(),
                ),
                // A wrapper's banner before the first reply.
                ("FAKE_ACP_NOISE".into(), "1".into()),
            ]),
            unset_environment: Default::default(),
        },
        session_id,
    };
    let start = |session_id| {
        let (spec, mut adapter) = super::super::protocol::acp::create(
            &super::super::protocol::acp::TEST_PROFILE,
            &launch(session_id),
        )
        .unwrap();
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap()
    };
    let prompt = |handle: &LocalHandle, text: &str| {
        handle
            .command(Command::Prompt {
                text: text.into(),
                attachments: Vec::new(),
                mode: super::super::PromptMode::Send,
            })
            .unwrap()
    };
    let says = |snapshot: &Snapshot, wanted: &str| {
        snapshot.items.iter().any(|item| {
            item.role == super::super::Role::Assistant
                && matches!(&item.content, ItemContent::Text { text, thinking: false, .. } if text == wanted)
        })
    };
    let handle = start(None);
    let snapshot = until(&handle, |snapshot| snapshot.ready);
    assert_eq!(snapshot.selected_model.as_deref(), Some("gemini-3.8-flash"));
    assert!(snapshot.capabilities.compact);
    let session = snapshot.session_id.clone().unwrap();

    prompt(&handle, "Use some tools");
    let snapshot = until(&handle, |snapshot| {
        !snapshot.busy && says(snapshot, "Listed the files and updated notes.md.")
    });
    assert!(snapshot.items.iter().any(|item| matches!(&item.content,
        ItemContent::Command { command, output, exit_code: Some(0), .. }
            if command == "ls -1" && output == "README\nnotes.md")));
    assert!(snapshot.items.iter().any(|item| matches!(&item.content,
        ItemContent::Changes { changes, .. } if changes[0].diff.contains("+new line"))));
    assert!(snapshot.usage.context_tokens > 0);

    prompt(&handle, "Ask permission");
    let snapshot = until(&handle, |snapshot| !snapshot.requests.is_empty());
    assert!(snapshot.waiting);
    let request = snapshot.requests[0].clone();
    assert_eq!(request.title, "Edit config.toml");
    handle
        .command(Command::Answer {
            request_id: request.id,
            action: "proceed_once".into(),
            answers: Vec::new(),
        })
        .unwrap();
    until(&handle, |snapshot| {
        !snapshot.busy
            && snapshot.requests.is_empty()
            && says(snapshot, "Turned on debugging in config.toml.")
    });

    prompt(&handle, "Ask permission again");
    until(&handle, |snapshot| !snapshot.requests.is_empty());
    handle.command(Command::Interrupt).unwrap();
    let snapshot = until(&handle, |snapshot| {
        !snapshot.busy && snapshot.requests.is_empty()
    });
    assert!(!snapshot.waiting);

    prompt(&handle, "hold on");
    until(&handle, |snapshot| {
        snapshot.items.iter().any(|item| {
            matches!(&item.content,
            ItemContent::Command { command, .. } if command == "sleep 60")
        })
    });
    handle.command(Command::Interrupt).unwrap();
    let snapshot = until(&handle, |snapshot| !snapshot.busy);
    assert!(snapshot.items.iter().any(|item| matches!(&item.content,
        ItemContent::Command { command, status: super::super::ToolStatus::Stopped, .. }
            if command == "sleep 60")));

    prompt(&handle, "garbage please");
    let snapshot = until(&handle, |snapshot| {
        !snapshot.busy
            && says(snapshot, "Before the stray line. ")
            && says(snapshot, "After the stray line.")
    });
    prompt(&handle, "Read some files");
    let snapshot_files = until(&handle, |snapshot| {
        !snapshot.busy
            && says(
                snapshot,
                "The client answered fs/read_text_file with error -32601.",
            )
    });
    assert!(
        snapshot.error.is_none() && snapshot_files.error.is_none(),
        "{:?}",
        snapshot_files.error
    );
    assert!(!snapshot_files.exited);
    handle.close().unwrap();
    assert!(handle.failure().is_none());

    let handle = start(Some(session.clone()));
    let snapshot = until(&handle, |snapshot| snapshot.ready);
    assert_eq!(snapshot.session_id.as_deref(), Some(session.as_str()));
    assert!(says(&snapshot, "Turned on debugging in config.toml."));
    // The saved conversation left the stopped step running; it shows as
    // stopped, not as still working.
    assert!(snapshot.items.iter().any(|item| matches!(&item.content,
        ItemContent::Command { command, status: super::super::ToolStatus::Stopped, .. }
            if command == "sleep 60")));
    assert!(!snapshot.busy);
    assert!(snapshot.items.iter().any(|item| item.role == super::super::Role::User
        && matches!(&item.content, ItemContent::Text { text, .. } if text == "Use some tools")));
    assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
    handle.close().unwrap();
    assert!(handle.failure().is_none());
}

fn json_lines_spec(arguments: Vec<String>, program: &str) -> ProcessSpec {
    ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: program.into(),
        arguments,
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    }
}

// Providers that did not opt in keep the immediate failure on non-JSON
// output, both when the process exits cleanly and while it stays alive.
#[test]
fn non_json_stdout_fails_visibly_instead_of_scraping_ansi() {
    for spec in [
        json_lines_spec(vec!["not-json\\n".into()], "/usr/bin/printf"),
        json_lines_spec(
            vec!["-c".into(), "echo not-json; exec sleep 60".into()],
            "/bin/sh",
        ),
    ] {
        let mut initial = Step::default();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle =
            LocalHandle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
        let snapshot = until(&handle, |snapshot| snapshot.error.is_some());
        assert!(
            snapshot
                .error
                .as_deref()
                .unwrap()
                .contains("Unsupported provider stdout"),
            "{snapshot:?}"
        );
        handle
            .close()
            .expect("failed provider can be explicitly retired");
    }
}

#[test]
fn only_providers_that_opt_in_tolerate_stdout_noise() {
    let recipe = crate::model::LaunchRecipe {
        program: "/bin/false".into(),
        arguments: Vec::new(),
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
        unset_environment: Default::default(),
    };
    assert!(ProcessSpec::from(&recipe).stdout_noise.is_none());
    let launch = super::super::Launch {
        provider: crate::agent::Provider::Claude,
        recipe,
        session_id: None,
    };
    let (spec, _) = super::super::protocol::claude::create(&launch).unwrap();
    assert!(spec.stdout_noise.is_some());
}

#[test]
fn a_backlog_beyond_the_frame_budget_drains_after_eof() {
    // Larger than one bounded parse pass and one read, so EOF is observed
    // while complete frames are still queued.
    let frames = 3_000;
    let spec = json_lines_spec(
        vec![
            "%s".into(),
            format!("{}\n", json!({"text":"x"})).repeat(frames),
        ],
        "/usr/bin/printf",
    );
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.exited);
    assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
    assert_eq!(
        unstamped(&snapshot.items),
        vec![text("echo", &"x".repeat(frames))]
    );
    handle.close().unwrap();
}

#[test]
fn startup_frames_extend_the_deadline_only_within_the_overall_limit() {
    let started = Instant::now();
    let at = |seconds| started + Duration::from_secs(seconds);
    // No progress: the original 30 seconds.
    assert!(!startup_expired(started, started, at(30)));
    assert!(startup_expired(started, started, at(31)));
    // A frame earns another 30 seconds from when it arrived.
    assert!(!startup_expired(started, at(29), at(59)));
    assert!(startup_expired(started, at(29), at(60)));
    // Frames forever cannot outlast the overall limit.
    assert!(!startup_expired(started, at(299), at(300)));
    assert!(startup_expired(started, at(300), at(301)));
}

#[test]
fn claude_frames_keep_a_slow_startup_alive_through_the_real_pipe() {
    let (_directory, handle) = claude_initialization_pipe(
        "import time\n\
             time.sleep(20)\n\
             print(json.dumps({'type': 'future_advisory'}), flush=True)\n\
             time.sleep(20)\n\
             print(json.dumps({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': request['request_id'], 'response': {}}}), flush=True)\n\
             sys.stdin.read()\n",
    );
    // The fixture alone takes 40 seconds; a deadline it failed to renew
    // ends the loop early with an error.
    let deadline = Instant::now() + Duration::from_secs(40) + crate::test_timing::EVENTUALLY;
    let snapshot = loop {
        let snapshot = handle.snapshot(None).unwrap();
        if snapshot.ready || snapshot.error.is_some() || Instant::now() > deadline {
            break snapshot;
        }
        thread::sleep(Duration::from_millis(50));
    };
    handle.close().unwrap();
    assert!(snapshot.ready && snapshot.error.is_none(), "{snapshot:?}");
}

#[test]
fn claude_garbage_only_startup_still_fails_at_the_first_deadline_with_its_preview() {
    // Wrapper output never stops, so a deadline that it renewed would
    // never expire.
    let (_directory, handle) = claude_initialization_pipe(
        "import time\n\
             while True:\n    print('wrapper diagnostic', flush=True)\n    time.sleep(7)\n",
    );
    let snapshot = finish_initialization_pipe(&handle);
    let error = snapshot.error.as_deref().unwrap();
    assert!(
        error.contains("timed out without a JSON protocol frame"),
        "{error}"
    );
    assert!(
        error.contains("Last ignored output: wrapper diagnostic"),
        "{error}"
    );
}

#[test]
fn claude_exit_errors_include_the_last_ignored_output() {
    for (body, expected) in [
        (
            "print('wrapper diagnostic', flush=True)\n",
            "exited before completing initialization",
        ),
        (
            "print('wrapper diagnostic', flush=True)\nsys.exit(3)\n",
            "Provider exited with",
        ),
        (
            "print('wrapper diagnostic', flush=True)\nsys.stdout.write('unterminated')\n",
            "incomplete JSON line",
        ),
    ] {
        let (_directory, handle) = claude_initialization_pipe(body);
        let snapshot = finish_initialization_pipe(&handle);
        let error = snapshot.error.as_deref().unwrap();
        assert!(error.contains(expected), "{error}");
        assert!(
            error.contains("Last ignored output: wrapper diagnostic"),
            "{error}"
        );
    }
}

#[test]
fn claude_frame_glued_to_wrapper_text_fails_immediately_with_a_preview() {
    for line in [
        "'wrapper diag' + json.dumps({'type': 'result', 'subtype': 'success', 'is_error': False})",
        "json.dumps({'type': 'control_request', 'request_id': 'r', 'request': {}}) + ' wrapper tail'",
    ] {
        let (_directory, handle) =
            claude_initialization_pipe(&format!("print({line}, flush=True)\nsys.stdin.read()\n"));
        let snapshot = finish_initialization_pipe(&handle);
        let error = snapshot.error.as_deref().unwrap();
        assert!(
            error.contains("inside one of its conversation messages"),
            "{error}"
        );
        // Waiting out the deadline instead would report a timeout.
        assert!(error.contains("type"), "{error}");
    }
}

#[test]
fn claude_fixture_noise_limit_fails_a_turn_visibly_through_the_real_pipe() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let provider = directory.path().join("claude");
    std::fs::write(
        &provider,
        include_str!("../../../tests/fixtures/claude-unknown-frames.py"),
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let launch = super::super::Launch {
        provider: crate::agent::Provider::Claude,
        recipe: crate::model::LaunchRecipe {
            program: provider.to_string_lossy().into_owned(),
            arguments: Vec::new(),
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
            unset_environment: Default::default(),
        },
        session_id: None,
    };
    let (spec, mut adapter) = super::super::protocol::claude::create(&launch).unwrap();
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    until(&handle, |snapshot| snapshot.ready);
    handle
        .command(Command::Prompt {
            text: "noise-limit".into(),
            attachments: Vec::new(),
            mode: super::super::PromptMode::Send,
        })
        .unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.error.is_some());
    handle.close().unwrap();
    let error = snapshot.error.as_deref().unwrap();
    assert!(
        error.contains("8 lines in a row that are not part of the conversation"),
        "{error}"
    );
    assert!(error.contains("offline wrapper diagnostic 7"), "{error}");
}

#[test]
fn json_line_noise_resets_its_failure_streak_after_each_valid_frame() {
    let mut output = String::new();
    for text in ["hello", " world"] {
        output.push_str(&"wrapper diagnostic\n".repeat(7));
        output.push_str(&format!("{}\n", json!({"text":text})));
    }
    let spec = ProcessSpec {
        stdout_noise: Some(super::super::protocol::claude::embedded_protocol_frame),
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/usr/bin/printf".into(),
        arguments: vec!["%s".into(), output],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.exited);
    assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
    assert_eq!(
        unstamped(&snapshot.items),
        vec![text("echo", "hello world")]
    );
    handle.close().unwrap();
    assert!(handle.failure().is_none());
}

#[test]
fn json_line_noise_fails_after_eight_lines_with_a_bounded_escaped_preview() {
    let output = format!(
        "{}\u{1b}[31m🦀{}\n",
        "not-json\n".repeat(7),
        "x".repeat(10_000)
    );
    let spec = ProcessSpec {
        stdout_noise: Some(super::super::protocol::claude::embedded_protocol_frame),
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/usr/bin/printf".into(),
        arguments: vec!["%s".into(), output],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.exited);
    let error = snapshot.error.as_deref().unwrap();
    assert!(error.contains("8 lines in a row that are not part of the conversation"));
    assert!(error.contains("\\u{1b}[31m🦀"));
    assert!(error.len() < 512);
    assert!(!error.chars().any(char::is_control));
    assert!(snapshot.items.is_empty());
    handle
        .close()
        .expect("failed provider can be explicitly retired");
    handle.close().expect("completed close is idempotent");
    assert!(
        handle.failure().is_some(),
        "closing retains the failure diagnosis"
    );
}

#[test]
fn json_line_noise_does_not_swallow_a_valid_frames_adapter_error() {
    let spec = ProcessSpec {
        stdout_noise: Some(super::super::protocol::claude::embedded_protocol_frame),
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/usr/bin/printf".into(),
        arguments: vec!["%s".into(), "wrapper noise\n{}\n".into()],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.exited);
    assert_eq!(snapshot.error.as_deref(), Some("missing echo text"));
    handle.close().unwrap();
    assert!(handle.failure().is_some());
}

#[test]
fn a_panicked_join_cannot_be_retired_even_after_a_completion_publication() {
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/usr/bin/printf".into(),
        arguments: vec!["not-json\\n".into()],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step::default();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, Box::new(Echo), initial, store, None, |_| {}).unwrap();
    until(&handle, |snapshot| snapshot.exited);
    handle.close().unwrap();
    // Model a panic after the worker published completion, such as a
    // teardown notification failing. Join outcome must take precedence.
    *handle.0.worker.lock().unwrap() = Some(thread::spawn(|| panic!("fixture join failure")));
    assert!(handle.close().unwrap_err().contains("unconfirmed"));
    assert!(!handle.is_completed());
    assert!(
        handle.close().is_err(),
        "a retry cannot erase uncertain teardown"
    );
}
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires permitted user/PID namespaces and writable namespace ns_last_pid; run explicitly on Linux"]
fn direct_provider_teardown_never_signals_a_reused_process_group() {
    use std::os::fd::{FromRawFd, OwnedFd};
    const MARKER: &str = "CAGENTS_AUDIT_PGID_REUSE_CHILD";
    if std::env::var_os(MARKER).is_none() {
        for scenario in ["already-reaped", "retained-leader"] {
            let output = Process::new("/usr/bin/unshare")
                .args(["--user", "--map-root-user", "--pid", "--fork", "--mount-proc"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", "chat::runtime::tests::direct_provider_teardown_never_signals_a_reused_process_group", "--nocapture", "--ignored"])
                .env(MARKER, scenario).output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("owned PID-reuse fixture started"),
                "the exact child test must run: {stdout}; {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success(),
                "owned PID namespace {scenario} regression failed: {stdout}; {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let scenario = std::env::var(MARKER).unwrap();
    let already_reaped = scenario == "already-reaped";
    println!("owned PID-reuse fixture started");
    assert_eq!(
        std::process::id(),
        1,
        "only the isolated namespace init may reset PID allocation"
    );
    std::fs::write("/proc/sys/kernel/ns_last_pid", b"199").unwrap();
    let mut original = Process::new("/bin/true");
    // SAFETY: only the async-signal-safe setsid syscall runs after fork.
    unsafe {
        original.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut provider = ChildGuard::spawn(&mut original);
    let pid = provider.child.id();
    assert_eq!(pid, 200);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // The retained case uses the same non-reaping observer as both
        // actual actor paths. The reaped case deliberately models old
        // callers and must refuse group signals after losing ownership.
        let status = if already_reaped {
            provider.child.try_wait().unwrap()
        } else {
            provider.observed_exit_status().unwrap()
        };
        if let Some(status) = status {
            assert_eq!(status.code(), Some(0));
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    std::fs::write("/proc/sys/kernel/ns_last_pid", (pid - 1).to_string()).unwrap();
    let mut command = Process::new("/bin/sleep");
    command.arg("30");
    // SAFETY: only the async-signal-safe setsid syscall runs after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let replacement = command.spawn().unwrap();
    if already_reaped {
        assert_eq!(
            replacement.id(),
            pid,
            "the kernel must actually reuse the exited PID"
        );
    } else {
        assert_ne!(
            replacement.id(),
            pid,
            "the observer must keep the exited leader's PID reserved"
        );
    }
    assert_eq!(
        unsafe { libc::getpgid(replacement.id() as i32) },
        replacement.id() as i32
    );
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, replacement.id(), 0) } as i32;
    assert!(raw >= 0);
    struct Replacement {
        child: Child,
        pidfd: OwnedFd,
    }
    impl Drop for Replacement {
        fn drop(&mut self) {
            // SAFETY: the pidfd pins only our namespace replacement child.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
            let _ = self.child.wait();
        }
    }
    let mut replacement = Replacement {
        child: replacement,
        // SAFETY: pidfd_open returned a new owned descriptor.
        pidfd: unsafe { OwnedFd::from_raw_fd(raw) },
    };
    if already_reaped {
        let cleanup = provider.terminate().unwrap_err();
        assert_eq!(cleanup.raw_os_error(), Some(libc::ECHILD));
    } else {
        provider.terminate().unwrap();
        assert_eq!(provider.child.try_wait().unwrap().unwrap().code(), Some(0));
    }
    let mut poll = libc::pollfd {
        fd: replacement.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let exited = unsafe { libc::poll(&mut poll, 1, 100) };
    let status = replacement.child.try_wait().unwrap();
    println!("verified reused PID/PGID={pid}; replacement readiness={exited}; status={status:?}");
    assert_eq!(
        exited, 0,
        "closing the original provider signalled its unrelated replacement process group"
    );
    assert!(status.is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn direct_provider_group_cleanup_retains_descendants_and_refuses_lost_ownership() {
    use std::os::fd::{FromRawFd, OwnedFd};
    const MARKER: &str = "CAGENTS_PROVIDER_GROUP_FIXTURE_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let output = Process::new(std::env::current_exe().unwrap())
            .args(["--exact", "chat::runtime::tests::direct_provider_group_cleanup_retains_descendants_and_refuses_lost_ownership", "--nocapture"])
            .env(MARKER, "1").output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("owned provider subreaper fixture started"),
            "exact fixture must run: {stdout}"
        );
        assert!(
            output.status.success(),
            "owned provider cleanup fixture failed: {stdout}; {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // SAFETY: this isolated one-test process becomes the sole reaper of
    // its own fixture descendants, without changing the parent test runner.
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
    println!("owned provider subreaper fixture started");
    for already_reaped in [false, true] {
        use std::io::{BufRead, BufReader};
        let script = "import os,signal,sys,time\npid=os.fork()\nif pid==0:\n signal.signal(signal.SIGTERM,signal.SIG_DFL if sys.argv[1]=='default' else signal.SIG_IGN)\n print(os.getpid(),flush=True)\n time.sleep(30)\n os._exit(0)\nsys.stdin.readline()\nos._exit(7)\n";
        let mut provider = ChildGuard::spawn(
            Process::new("/usr/bin/python3")
                .args([
                    "-c",
                    script,
                    if already_reaped { "default" } else { "ignore" },
                ])
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped()),
        );
        let mut line = String::new();
        BufReader::new(provider.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let descendant: i32 = line.trim().parse().unwrap();
        assert_eq!(
            unsafe { libc::getpgid(descendant) },
            provider.child.id() as i32
        );
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, descendant, 0) } as i32;
        assert!(raw >= 0);
        struct Descendant {
            pid: i32,
            pidfd: OwnedFd,
            reaped: bool,
        }
        impl Drop for Descendant {
            fn drop(&mut self) {
                if self.reaped {
                    return;
                }
                // SAFETY: only the fixture child's pidfd is signalled; this
                // isolated subreaper can reap that exact adopted child afterward.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                    libc::waitpid(self.pid, std::ptr::null_mut(), 0);
                }
            }
        }
        let mut descendant = Descendant {
            pid: descendant,
            reaped: false,
            // SAFETY: pidfd_open returned a new owned descriptor.
            pidfd: unsafe { OwnedFd::from_raw_fd(raw) },
        };
        provider
            .child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"exit\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = if already_reaped {
                provider.child.try_wait().unwrap()
            } else {
                provider.observed_exit_status().unwrap()
            } {
                assert_eq!(status.code(), Some(7));
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        if already_reaped {
            assert_eq!(
                provider.terminate().unwrap_err().raw_os_error(),
                Some(libc::ECHILD)
            );
            let mut poll = libc::pollfd {
                fd: descendant.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 100) },
                0,
                "lost leader ownership must refuse every group signal"
            );
            continue;
        }
        provider.terminate().unwrap();
        assert_eq!(provider.child.try_wait().unwrap().unwrap().code(), Some(7));
        let mut poll = libc::pollfd {
            fd: descendant.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, 1000) },
            1,
            "cleanup must retain its existing SIGKILL escalation for original group descendants"
        );
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(descendant.pid, &mut status, 0) },
            descendant.pid
        );
        descendant.reaped = true;
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    }
}

#[test]
fn pipe_eof_waits_for_exact_exit_but_preserves_unobservable_writers() {
    for (tail, expected) in [
        ("sleep 0.02; exit 0", None),
        ("sleep 0.02; exit 7", Some("Provider exited with")),
        ("sleep 5", Some("unobservable writer")),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let spec = ProcessSpec {
            stdout_noise: None,
            followed: false,
            execution: None,
            messages: None,
            unset_environment: Default::default(),
            transport: TransportSpec::JsonLines,
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), format!("read -r init; exec 1>&-; {tail}")],
            working_directory: directory.path().into(),
            environment: BTreeMap::new(),
        };
        let mut adapter: Box<dyn Adapter> = Box::new(Echo);
        let mut initial = adapter.initialize().unwrap();
        let store = preflight(&spec, &mut initial).unwrap();
        let handle = Handle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !handle.is_completed() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let completed = handle.is_completed();
        let failure = handle.failure();
        // Join and reap the owned actor before checking any failure path.
        handle.close().unwrap();
        assert!(completed, "EOF classification exceeded its bound: {tail}");
        match expected {
            None => {
                assert_eq!(failure, None);
                assert!(!handle.owns_work());
            }
            Some(message) => {
                assert!(
                    failure
                        .as_deref()
                        .is_some_and(|error| error.contains(message)),
                    "{tail}: {failure:?}"
                );
                assert!(
                    handle.owns_work(),
                    "uncertain/failed writer must remain protected"
                );
            }
        }
    }
}

#[test]
fn provider_exit_during_a_write_reports_its_exit_status_not_the_broken_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let release = directory.path().join("release");
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/sh".into(),
        // Close stdin before announcing it, so the next host write
        // always meets EPIPE; exit only after that write is queued.
        arguments: vec![
            "-c".into(),
            format!(
                "read -r init; exec 0<&-; printf '%s\\n' '{{\"text\":\"closed\"}}'; \
                     until [ -e '{}' ]; do sleep 0.01; done; exit 75",
                release.display()
            ),
        ],
        working_directory: directory.path().into(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Echo);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    until(&handle, |snapshot| !snapshot.items.is_empty());
    handle.command(Command::Refresh).unwrap();
    std::fs::write(&release, b"").unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.exited);
    let failure = handle.failure();
    handle.close().unwrap();
    assert!(
        failure
            .as_deref()
            .is_some_and(|error| error.contains("Provider exited with exit status: 75")),
        "{failure:?} {:?}",
        snapshot.error
    );
}

#[test]
fn direct_provider_non_reaping_observation_preserves_exit_status() {
    for script in ["exit 7", "kill -TERM $$"] {
        let mut provider = ChildGuard::spawn(
            Process::new("/bin/sh")
                .args(["-c", script])
                .process_group(0),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let observed = loop {
            if let Some(status) = provider.observed_exit_status().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        };
        provider.terminate().unwrap();
        assert_eq!(
            provider.child.try_wait().unwrap().unwrap(),
            observed,
            "non-reaping observation must match the real Child exit status for {script}"
        );
    }
}

#[test]
fn a_provider_is_never_handed_a_parent_session() {
    use std::io::Read;
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        transport: TransportSpec::JsonLines,
        program: "/usr/bin/env".into(),
        arguments: Vec::new(),
        working_directory: std::env::current_dir().unwrap(),
        environment: [
            ("CLAUDECODE".into(), "1".into()),
            (
                "CLAUDE_CODE_MESSAGING_SOCKET".into(),
                "/run/parent.sock".into(),
            ),
            ("CLAUDE_CONFIG_DIR".into(), "/home/a/.claude".into()),
        ]
        .into(),
        unset_environment: Default::default(),
    };
    let mut provider = spawn_provider(&spec, &AtomicBool::new(false), None)
        .unwrap()
        .unwrap();
    let mut output = String::new();
    provider
        .child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    provider.terminate().unwrap();
    let received = output.lines().collect::<Vec<_>>();
    assert!(received.contains(&"CLAUDE_CONFIG_DIR=/home/a/.claude"));
    for name in ["CLAUDECODE=", "CLAUDE_CODE_MESSAGING_SOCKET="] {
        assert!(
            !received.iter().any(|line| line.starts_with(name)),
            "{name} reached the provider"
        );
    }
}

#[test]
fn direct_provider_scope_preserves_pipes_and_process_group() {
    use std::io::{BufRead, Write};
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: std::env::current_dir().unwrap(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let mut provider = spawn_provider(&spec, &AtomicBool::new(false), None)
        .unwrap()
        .unwrap();
    let pid = provider.child.id();
    crate::daemon::process_scope::tests::assert_separate(pid, pid);
    // SAFETY: querying our unreaped child, never changing its process group.
    assert_eq!(unsafe { libc::getpgid(pid as i32) }, pid as i32);
    provider
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"scope transport preserved\n")
        .unwrap();
    let mut reply = String::new();
    std::io::BufReader::new(provider.child.stdout.take().unwrap())
        .read_line(&mut reply)
        .unwrap();
    assert_eq!(reply, "scope transport preserved\n");
    provider.terminate().unwrap();
}

/// The commands a direct provider starts leave its cgroup, so a memory
/// kill of one ends that command alone and the conversation says so; a
/// memory kill of the provider itself is named as one when it exits.
/// The kills are made the way systemd-oomd makes them: it marks the
/// cgroup, then kills every process in it.
#[test]
fn a_memory_kill_of_a_command_spares_its_provider_and_is_reported() {
    use std::io::{BufRead, Read, Write};
    use std::os::unix::process::ExitStatusExt;
    fn cgroup(pid: u32) -> String {
        std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap()
            .to_owned()
    }
    fn parent(pid: u32) -> u32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after = stat.rsplit_once(") ").unwrap().1;
        after.split_whitespace().nth(1).unwrap().parse().unwrap()
    }
    fn mark_and_kill(leaf: &str) {
        let path = std::path::Path::new("/sys/fs/cgroup").join(leaf.trim_start_matches('/'));
        for (name, value) in [("user.oomd_ooms", "1"), ("user.oomd_kill", "1")] {
            let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            let name = std::ffi::CString::new(name).unwrap();
            // SAFETY: NUL-terminated path and name and a live value buffer.
            let set = unsafe {
                libc::setxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                )
            };
            assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
        }
        std::fs::write(path.join("cgroup.kill"), "1").unwrap();
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        transport: TransportSpec::JsonLines,
        program: "/bin/sh".into(),
        // A one-process command, and a shell running a command of its
        // own, as an agent's Bash tool does. That one leaves the
        // provider's process group, as a tool may start its commands.
        arguments: vec![
            "-c".into(),
            "read go; sleep 300 & echo $!; sh -c 'setsid sleep 300 & echo $!; wait' & echo $!; read done"
                .into(),
        ],
        working_directory: std::env::current_dir().unwrap(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let (wake, mut woken) = UnixStream::pair().unwrap();
    let mut provider = spawn_provider(&spec, &AtomicBool::new(false), Some(wake))
        .unwrap()
        .unwrap();
    let root = provider.child.id();
    let Some(scope) = provider
        .scope
        .as_ref()
        .filter(|scope| scope.keeps_commands_apart())
    else {
        eprintln!("command cgroups unqualified: no delegated provider scope here");
        provider.terminate().unwrap();
        return;
    };
    let provider_leaf = cgroup(root);
    assert!(provider_leaf.ends_with("/provider"), "{provider_leaf}");
    let scope_path = provider_leaf.trim_end_matches("/provider").to_owned();
    let unit = scope_path.rsplit('/').next().unwrap();
    let policy = std::process::Command::new("systemctl")
        .args(["--user", "show", "-P", "OOMPolicy", unit])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&policy.stdout).trim(),
        "continue",
        "a memory kill in one leaf must not stop the provider's scope"
    );
    // Commands start only after the split, as a provider's do.
    writeln!(provider.child.stdin.as_mut().unwrap(), "go").unwrap();
    let mut stdout = std::io::BufReader::new(provider.child.stdout.take().unwrap());
    let mut pids = Vec::new();
    for _ in 0..3 {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        pids.push(line.trim().parse::<u32>().unwrap());
    }
    // A shell prints a command's PID when it forks it, and a fork runs as
    // `sh` until it execs, so wait for both sleeps to have started.
    let command = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap();
    let (alone, shell, inner) = crate::test_timing::wait_for("the command tree", || {
        let find = |wanted: &dyn Fn(u32) -> bool| pids.iter().copied().find(|pid| wanted(*pid));
        let alone = find(&|pid| parent(pid) == root && command(pid) == "sleep\n")?;
        let shell = find(&|pid| pid != alone && parent(pid) == root)?;
        let inner = find(&|pid| parent(pid) == shell && command(pid) == "sleep\n")?;
        Some((alone, shell, inner))
    });
    assert_eq!(command(shell), "sh\n");
    crate::test_timing::eventually("commands leaving the provider's cgroup", || {
        [alone, shell, inner]
            .iter()
            .all(|pid| cgroup(*pid) != provider_leaf)
    });
    let alone_leaf = cgroup(alone);
    let shell_leaf = cgroup(shell);
    for leaf in [&alone_leaf, &shell_leaf] {
        let name = leaf.strip_prefix(&format!("{scope_path}/")).unwrap();
        assert!(name.starts_with("command-"), "{leaf}");
    }
    assert_ne!(alone_leaf, shell_leaf, "each command has its own cgroup");
    assert_eq!(
        cgroup(inner),
        shell_leaf,
        "a command's own commands stay with it"
    );
    assert_eq!(cgroup(root), provider_leaf, "the provider stays alone");

    mark_and_kill(&alone_leaf);
    let stops = crate::test_timing::wait_for("the stopped command's report", || {
        Some(scope.take_command_stops()).filter(|stops| !stops.is_empty())
    });
    assert_eq!(
        stops,
        vec![crate::daemon::process_scope::CommandStop {
            killer: crate::daemon::process_scope::Killer::Oomd,
            processes: Some(1),
        }]
    );
    let mut byte = [0];
    woken
        .set_read_timeout(Some(crate::test_timing::EVENTUALLY))
        .unwrap();
    woken.read_exact(&mut byte).unwrap();
    let report = command_stop_report(&stops, 0);
    assert!(
        report.starts_with(super::super::failure::COMMAND_OUT_OF_MEMORY),
        "{report}"
    );
    assert!(
        provider.child.try_wait().unwrap().is_none(),
        "the provider kept running"
    );
    assert_eq!(cgroup(inner), shell_leaf, "the other command kept running");

    // The kernel's OOM killer ends one process of a command, not all of
    // it: here a process over a small limit on the shell's leaf, which
    // it joins before it allocates.
    let leaf_path = std::path::Path::new("/sys/fs/cgroup").join(shell_leaf.trim_start_matches('/'));
    std::fs::write(leaf_path.join("memory.swap.max"), "0").unwrap();
    std::fs::write(leaf_path.join("memory.max"), "32M").unwrap();
    let mut hog = std::process::Command::new("/usr/bin/python3")
        .args([
            "-I",
            "-c",
            "import sys; sys.stdin.read(1); b = bytearray(256 << 20)",
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::fs::write(leaf_path.join("cgroup.procs"), hog.id().to_string()).unwrap();
    hog.stdin.take().unwrap().write_all(b"x").unwrap();
    assert_eq!(hog.wait().unwrap().signal(), Some(libc::SIGKILL));
    let stops = crate::test_timing::wait_for("the kernel's kill being reported", || {
        Some(scope.take_command_stops()).filter(|stops| !stops.is_empty())
    });
    assert_eq!(
        stops,
        vec![crate::daemon::process_scope::CommandStop {
            killer: crate::daemon::process_scope::Killer::Kernel,
            processes: Some(1),
        }],
        "reported while the rest of the command runs on"
    );
    assert_eq!(cgroup(inner), shell_leaf);

    // The provider's own leaf, and with it the provider.
    mark_and_kill(&provider_leaf);
    let status = crate::test_timing::wait_for("the provider exiting from its kill", || {
        provider.observed_exit_status().unwrap()
    });
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    let report = exit_report(&provider, status);
    assert!(
        report.starts_with(super::super::failure::PROVIDER_OUT_OF_MEMORY),
        "{report}"
    );
    assert!(
        report.ends_with("Provider exited with signal: 9 (SIGKILL)"),
        "{report}"
    );
    // The command in a session of its own outlived the provider's kill.
    assert_eq!(cgroup(inner), shell_leaf);
    let ended = |pid: u32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(") ").unwrap().1.starts_with('Z')
        })
    };
    assert!(!ended(inner));
    // The force-stopped provider's teardown ends what it left, as
    // nothing else would.
    provider.terminate().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ended(inner) {
        assert!(
            Instant::now() < deadline,
            "a command outlived the provider that started it"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A process the agent detached, such as a dev server, outlives a chat
/// the user closes and a provider that finishes, as it asked to: only a
/// force-stopped provider's leftovers are ended, or those of a provider
/// whose owner decided to stop them, such as an unattended agent's.
#[test]
fn a_detached_command_outlives_a_closed_or_finished_provider() {
    use std::io::{BufRead, Write};
    fn ended(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(") ").unwrap().1.starts_with('Z')
        })
    }
    for (case, before, after, stop) in [
        ("closed", "", "", false),
        // cagents' own escalation to SIGKILL is not a forced stop.
        ("closed while ignoring SIGTERM", "trap '' TERM; ", "", false),
        ("finished", "", "; exit 0", false),
        ("closed by an owner stopping it", "", "", true),
    ] {
        let spec = ProcessSpec {
            stdout_noise: None,
            followed: false,
            execution: None,
            messages: None,
            transport: TransportSpec::JsonLines,
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                // The command reports its PID once it has left the
                // provider's process group, which teardown signals.
                format!(
                    "read go; {before}setsid sh -c 'echo $$; exec sleep 300' & read done{after}"
                ),
            ],
            working_directory: std::env::current_dir().unwrap(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let mut provider = spawn_provider(&spec, &AtomicBool::new(false), None)
            .unwrap()
            .unwrap();
        if !provider
            .scope
            .as_ref()
            .is_some_and(|scope| scope.keeps_commands_apart())
        {
            eprintln!("command cgroups unqualified: no delegated provider scope here");
            provider.terminate().unwrap();
            return;
        }
        let mut stdin = provider.child.stdin.take().unwrap();
        writeln!(stdin, "go").unwrap();
        let mut line = String::new();
        let mut stdout = std::io::BufReader::new(provider.child.stdout.take().unwrap());
        stdout.read_line(&mut line).unwrap();
        let detached: u32 = line.trim().parse().unwrap();
        if case == "finished" {
            writeln!(stdin, "done").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while provider.observed_exit_status().unwrap().is_none() {
                assert!(Instant::now() < deadline, "the provider never finished");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        provider.stop_detached = stop;
        provider.terminate().unwrap();
        let survived = if stop {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ended(detached) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            !ended(detached)
        } else {
            // Give a scope kill, were there one, time to land.
            std::thread::sleep(Duration::from_millis(200));
            !ended(detached)
        };
        // SAFETY: a plain signal to the test's own detached command.
        unsafe { libc::kill(detached as i32, libc::SIGKILL) };
        assert_eq!(
            survived,
            !stop,
            "a detached command {} with a {case} provider",
            if stop { "survived" } else { "ended" }
        );
    }
}

#[test]
fn logical_copy_serves_parent_and_complete_child_but_refuses_trimmed_child_after_exit() {
    struct CopyFixture;
    let body = format!("{}{}", "a".repeat(128 * 1024 - 1), "🦀界".repeat(30_000));
    fn message(text: &str) -> Item {
        Item {
            id: "same".into(),
            role: super::super::Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Phase::Final,
            },
        }
    }
    impl Adapter for CopyFixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            panic!("copy reached provider")
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/true".into(),
        arguments: vec![],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step {
        events: vec![
            Event::Item(message("parent")),
            Event::SubagentItem {
                id: "child".into(),
                item: message("complete child 🦀"),
            },
            Event::SubagentItem {
                id: "trimmed".into(),
                item: message(&body),
            },
        ],
        ..Default::default()
    };
    let store = preflight(&spec, &mut initial).unwrap();
    let handle =
        LocalHandle::start(spec, Box::new(CopyFixture), initial, store, None, |_| {}).unwrap();
    let snapshot = until(&handle, |snapshot| snapshot.exited);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_completed() {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let child = snapshot
        .subagents
        .iter()
        .find(|child| child.id == "child")
        .unwrap();
    assert_eq!(child.items[0].content, message("complete child 🦀").content);
    let trimmed = snapshot
        .subagents
        .iter()
        .find(|child| child.id == "trimmed")
        .unwrap();
    assert!(trimmed.items[0].content.byte_len() < 64 * 1024);
    assert_ne!(trimmed.items[0].content, message(&body).content);
    assert!(
        snapshot.text_copy_sources.children["trimmed"][0].is_none(),
        "a presentation placeholder is not complete text"
    );
    let parent_source = snapshot.text_copy_sources.conversation[0].as_ref().unwrap();
    let child_source = snapshot.text_copy_sources.children["child"][0]
        .as_ref()
        .unwrap();
    assert_eq!(
        parent_source.scope,
        super::super::TextCopyScope::Conversation
    );
    assert_eq!(
        child_source.scope,
        super::super::TextCopyScope::Subagent { id: "child".into() }
    );
    for (source, expected) in [
        (parent_source, "parent"),
        (child_source, "complete child 🦀"),
    ] {
        let mut copied = String::new();
        loop {
            let page = handle.text_copy(source, copied.len() as u64).unwrap();
            copied.push_str(&page.text);
            if page.done {
                break;
            }
        }
        assert_eq!(copied.as_bytes(), expected.as_bytes());
    }
    let mut unavailable = child_source.clone();
    unavailable.scope = super::super::TextCopyScope::Subagent {
        id: "trimmed".into(),
    };
    assert!(handle.text_copy(&unavailable, 0).is_err());
    assert!(
        handle
            .command(Command::ReadTextCopy {
                source: child_source.clone(),
                offset: 0
            })
            .unwrap_err()
            .contains("requester-specific")
    );
}

#[test]
fn provider_exit_stops_pending_claude_file_edits_in_cached_and_fresh_queries() {
    use super::super::FileChangeReply;
    for (name, input) in [
        (
            "Edit",
            serde_json::json!({"file_path":"a.rs","old_string":"old","new_string":"new"}),
        ),
        (
            "Write",
            serde_json::json!({"file_path":"a.rs","content":"new\n"}),
        ),
    ] {
        let call = serde_json::json!({
            "type":"assistant", "uuid":"call", "message":{
                "id":"message", "role":"assistant", "content":[{
                    "type":"tool_use", "id":"edit", "name":name, "input":input
                }]
            }
        });
        let (_directory, handle) = claude_initialization_pipe(&format!(
            "print(json.dumps({{'type': 'control_response', 'response': {{'subtype': 'success', 'request_id': request['request_id'], 'response': {{}}}}}}), flush=True)\n\
             print({call:?}, flush=True)\n\
             json.loads(sys.stdin.readline())\n\
             sys.exit(7)\n",
            call = call.to_string(),
        ));
        // Keep the provider blocked until its running edit has been published.
        let running = until(&handle, |snapshot| {
            snapshot.file_changes.files.len() == 1 && snapshot.file_changes.files[0].pending == 1
        });
        let file = &running.file_changes.files[0];
        let Some(FileChangeReply::Patches(before)) = handle
            .file_query(&Command::LoadFileChangePatches {
                request_id: 1,
                key: file.key.clone(),
                cursor: None,
                revision: file.revision,
            })
            .unwrap()
        else {
            panic!("expected running patch");
        };
        assert!(before.error.is_none());
        assert_eq!(before.patches[0].status, ToolStatus::Running);
        handle.command(Command::Refresh).unwrap();
        let stopped = until(&handle, |snapshot| snapshot.exited && handle.is_completed());
        handle.close().unwrap();
        assert!(
            stopped.items.iter().any(|item| matches!(
                item.content,
                ItemContent::Changes {
                    status: ToolStatus::Stopped,
                    ..
                }
            )),
            "the transcript must stop the {name} call"
        );
        let Some(FileChangeReply::Files(fresh)) = handle
            .file_query(&Command::LoadFileChanges {
                request_id: 2,
                after: None,
            })
            .unwrap()
        else {
            panic!("expected retained file list");
        };
        let Some(FileChangeReply::Patches(after)) = handle
            .file_query(&Command::LoadFileChangePatches {
                request_id: 3,
                key: file.key.clone(),
                cursor: None,
                revision: file.revision,
            })
            .unwrap()
        else {
            panic!("expected retained patch");
        };
        assert!(after.error.is_none());
        assert_eq!(after.patches[0].diff, before.patches[0].diff);
        let published = &stopped.file_changes.files[0];
        let queried = &fresh.files[0];
        assert_eq!(
            (
                (published.pending, published.failed, published.succeeded),
                (queried.pending, queried.failed, queried.succeeded),
                after.patches[0].status,
            ),
            ((0, 1, 0), (0, 1, 0), ToolStatus::Stopped),
            "provider exit must stop {name} in the published summary, fresh list and patch"
        );
        assert_eq!(stopped.file_changes.files, fresh.files);
        assert_eq!(stopped.file_changes.revision, fresh.revision);
        assert!(fresh.revision > running.file_changes.revision);
        assert!(queried.revision > file.revision);
        assert_eq!(after.revision, queried.revision);
        assert_eq!(fresh.total_files, 1);
        assert_eq!(fresh.applied_files, 0);
    }
}

#[test]
fn file_changes_cache_queries_remain_available_after_provider_exit() {
    struct CacheFixture;
    impl Adapter for CacheFixture {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step {
                events: vec![Event::Item(Item {
                    id: "edit".into(),
                    role: super::super::Role::Assistant,
                    started_at: None,
                    at: None,
                    content: ItemContent::Changes {
                        changes: vec![super::super::FileChange {
                            path: "a.rs".into(),
                            kind: super::super::ChangeKind::Updated,
                            diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
                        }],
                        status: ToolStatus::Succeeded,
                    },
                })],
                ..Default::default()
            })
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            panic!("cache query reached provider")
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let spec = ProcessSpec {
        stdout_noise: None,
        followed: false,
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/true".into(),
        arguments: vec![],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(CacheFixture);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    until(&handle, |snapshot| snapshot.exited);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_completed() {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let Some(super::super::FileChangeReply::Files(summary)) = handle
        .file_query(&Command::LoadFileChanges {
            request_id: 1,
            after: None,
        })
        .unwrap()
    else {
        panic!("expected cached list")
    };
    assert_eq!(summary.applied_files, 1);
    let Some(super::super::FileChangeReply::Patches(page)) = handle
        .file_query(&Command::LoadFileChangePatches {
            request_id: 2,
            key: summary.files[0].key.clone(),
            cursor: None,
            revision: summary.files[0].revision,
        })
        .unwrap()
    else {
        panic!("expected cached patch")
    };
    assert!(page.error.is_none());
    assert!(page.patches[0].diff.contains("+new"));
}
#[test]
fn login_recovery_clears_only_captured_failure_without_new_error_or_turn() {
    let failure = "Authentication required. Please sign in again.".to_owned();
    let proof = Event::LoginRecovered {
        failure: failure.clone(),
    };
    let mut error = Some(failure.clone());
    let mut pending = Some(failure.clone());
    recover_login_error(&mut error, &mut pending, &proof);
    assert_eq!(error, None);
    assert_eq!(pending, None);
    let mut error = Some(failure.clone());
    let mut pending = Some(failure.clone());
    recover_login_error(
        &mut error,
        &mut pending,
        &Event::LoginVerificationFailed {
            failure: failure.clone(),
            detail: "finish signing in".into(),
        },
    );
    assert_eq!(
        error,
        Some(format!(
            "{failure}\nSign-in verification: finish signing in"
        ))
    );
    assert_eq!(pending, None);
    for invalidation in [
        Event::Error {
            message: failure.clone(),
            fatal: false,
        },
        Event::HistoryError("disk read failed".into()),
        Event::TurnStarted,
        Event::Waiting(true),
        Event::Status {
            busy: true,
            detail: "new turn".into(),
        },
        Event::Reset {
            reason: "new thread".into(),
        },
        Event::Exited { code: None },
    ] {
        let mut error = Some(failure.clone());
        let mut pending = Some(failure.clone());
        recover_login_error(&mut error, &mut pending, &invalidation);
        recover_login_error(&mut error, &mut pending, &proof);
        recover_login_error(
            &mut error,
            &mut pending,
            &Event::LoginVerificationFailed {
                failure: failure.clone(),
                detail: "late failed refresh".into(),
            },
        );
        assert_eq!(
            error,
            Some(failure.clone()),
            "late proof cannot clear a later lifecycle or identical error"
        );
    }
    let mut error = Some("unrelated provider error".into());
    let mut pending = Some(failure.clone());
    recover_login_error(&mut error, &mut pending, &proof);
    assert_eq!(error, Some("unrelated provider error".into()));
    let mut error = Some(failure);
    let mut pending = None;
    recover_login_error(&mut error, &mut pending, &proof);
    assert!(error.is_some(), "unsolicited proof grants no recovery");
}
#[test]
fn repeated_failed_verifications_keep_only_bounded_latest_detail_and_history() {
    let failure = "Authentication required. Please sign in again.";
    let mut state = Snapshot {
        error: Some(failure.into()),
        session_id: Some("retained-thread".into()),
        ..Default::default()
    };
    state
        .items
        .push(Arc::new(text("retained", "original history")));
    let retained = state.items.clone();
    let mut indices = BTreeMap::new();
    for attempt in 0..100 {
        let captured = state.error.clone().unwrap();
        let mut pending = Some(captured.clone());
        let event = Event::LoginVerificationFailed {
            failure: captured,
            detail: format!("Attempt {attempt}: {}", "é".repeat(2000)),
        };
        recover_login_error(&mut state.error, &mut pending, &event);
        apply_event(&mut state, &mut indices, event);
        let message = state.error.as_deref().unwrap();
        let detail = super::super::sign_in_verification_detail(message).unwrap();
        assert!(detail.starts_with(&format!("Attempt {attempt}:")));
        assert_eq!(detail.chars().count(), 512);
        assert_eq!(message.matches("Sign-in verification: ").count(), 1);
        assert!(message.starts_with(failure));
        assert_eq!(state.items, retained);
        assert_eq!(state.session_id.as_deref(), Some("retained-thread"));
        assert_eq!(pending, None);
    }
    let captured = state.error.clone().unwrap();
    let mut pending = Some(captured.clone());
    recover_login_error(
        &mut state.error,
        &mut pending,
        &Event::LoginRecovered { failure: captured },
    );
    assert_eq!(state.error, None);
    assert_eq!(state.items, retained);
}
