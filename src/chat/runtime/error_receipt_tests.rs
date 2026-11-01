use super::*;
use crate::chat::{Role, protocol::Adapter};

struct Receipts;

impl Adapter for Receipts {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step {
            events: vec![Event::Ready(true)],
            ..Default::default()
        })
    }
    fn command(&mut self, command: Command) -> Result<Step, String> {
        let events = match command {
            Command::SetModel { model, .. } if model == "orphan-source" => {
                vec![Event::ErrorSource {
                    item_id: "wrong-provider-incident".into(),
                }]
            }
            Command::SetModel { model, .. } if model.starts_with("unscoped:") => {
                vec![Event::Error {
                    message: model["unscoped:".len()..].into(),
                    fatal: false,
                }]
            }
            Command::SetModel { model, .. } => vec![
                Event::Error {
                    message: model.clone(),
                    fatal: false,
                },
                Event::ErrorSource {
                    item_id: format!("diagnostic:{model}"),
                },
            ],
            Command::Compact => vec![
                Event::HistoryError("history failure".into()),
                Event::ErrorSource {
                    item_id: "diagnostic:history failure".into(),
                },
            ],
            Command::Prompt { text, .. } => vec![Event::TextDelta {
                id: "pending-text".into(),
                role: Role::Assistant,
                thinking: false,
                delta: text,
            }],
            _ => return Err("unexpected receipt fixture command".into()),
        };
        Ok(Step {
            events,
            ..Default::default()
        })
    }
    fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
        Ok(Step::default())
    }
}

fn handle() -> LocalHandle {
    let spec = ProcessSpec {
        execution: None,
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::History,
        program: "not-launched".into(),
        arguments: vec![],
        working_directory: std::env::temp_dir(),
        environment: BTreeMap::new(),
    };
    let mut adapter: Box<dyn Adapter> = Box::new(Receipts);
    let mut initial = adapter.initialize().unwrap();
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = LocalHandle::start(spec, adapter, initial, store, None, |_| {}).unwrap();
    handle.check_update_readiness().unwrap();
    handle
}

fn scalar(handle: &LocalHandle, message: &str) {
    handle
        .command(Command::SetModel {
            model: message.into(),
            reasoning: None,
        })
        .unwrap();
}

fn pending_text(handle: &LocalHandle) {
    handle
        .command(Command::Prompt {
            text: "cache flush fault".into(),
            attachments: vec![],
            mode: crate::chat::PromptMode::Send,
        })
        .unwrap();
}

#[test]
fn adjacent_scalar_sources_publish_coherently_through_actor_store_and_concurrent_readers() {
    let handle = handle();
    scalar(&handle, "provider-seed");
    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (ready, first_read) = crossbeam_channel::bounded(1);
    let reader = {
        let handle = handle.clone();
        let done = done.clone();
        let reads = reads.clone();
        thread::spawn(move || {
            let mut first = true;
            while !done.load(Ordering::Acquire) {
                let snapshot = handle.snapshot(None).unwrap();
                if let Some(message) = &snapshot.error {
                    assert_eq!(
                        snapshot.error_item.as_deref(),
                        Some(format!("diagnostic:{message}").as_str()),
                        "a read cannot observe the scalar paired with another receipt"
                    );
                    reads.fetch_add(1, Ordering::Relaxed);
                    if first {
                        ready.send(()).unwrap();
                        first = false;
                    }
                }
                thread::yield_now();
            }
        })
    };
    first_read.recv_timeout(Duration::from_secs(2)).unwrap();
    for index in 0..64 {
        scalar(&handle, &format!("provider-{index}"));
        handle.command(Command::Compact).unwrap();
    }
    let final_snapshot = handle.snapshot(None).unwrap();
    done.store(true, Ordering::Release);
    reader.join().unwrap();
    assert!(reads.load(Ordering::Relaxed) > 0);
    assert_eq!(final_snapshot.error_generation, 129);
    assert_eq!(
        final_snapshot.error_item.as_deref(),
        Some("diagnostic:history failure")
    );
    handle.close().unwrap();
}

#[test]
fn owned_cache_read_failures_replace_sources_once_per_receipt_for_both_publication_apis() {
    for presentation in [false, true] {
        let handle = handle();
        scalar(&handle, "provider failed");
        let provider = handle.snapshot(None).unwrap();
        handle.0.store.lock().unwrap().read_only_for_test().unwrap();
        let publish = |handle: &LocalHandle| {
            if presentation {
                match handle
                    .presentation(crate::model::TerminalId::new(), None)
                    .unwrap()
                {
                    crate::terminal_ipc::ServerResponse::ChatSnapshot {
                        snapshot: Some(snapshot),
                        ..
                    } => snapshot,
                    other => panic!("expected full fixture snapshot: {other:?}"),
                }
            } else {
                handle.snapshot(None).unwrap()
            }
        };
        pending_text(&handle);
        let first = publish(&handle);
        assert!(
            first
                .error
                .as_deref()
                .unwrap()
                .starts_with("Cannot page owned transcript cache:")
        );
        assert_eq!(first.error_item, None);
        assert!(first.error_generation > provider.error_generation);
        assert!(first.generation > provider.generation);
        pending_text(&handle);
        let repeated = publish(&handle);
        assert_eq!(repeated.error, first.error);
        assert_eq!(
            repeated.error_generation, first.error_generation,
            "repeated owned-cache faults retain one scalar receipt"
        );
        scalar(&handle, "orphan-source");
        let unclaimed = handle.snapshot(None).unwrap();
        assert_eq!(unclaimed.error_item, None);
        assert_eq!(unclaimed.error_generation, first.error_generation);

        handle.close().unwrap();
    }
}

#[test]
fn same_text_provider_and_local_errors_have_distinct_receipts() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    let mut previous_read = None;
    publish_read_error(&mut state, &mut previous_read, "same diagnostic".into());
    let first = state.error_generation;
    publish_read_error(&mut state, &mut previous_read, "same diagnostic".into());
    assert_eq!(state.error_generation, first);
    apply_event(
        &mut state,
        &mut indices,
        Event::Error {
            message: "same diagnostic".into(),
            fatal: false,
        },
    );
    let intervening = state.error_generation;
    assert!(intervening > first);
    publish_read_error(&mut state, &mut previous_read, "same diagnostic".into());
    assert!(state.error_generation > intervening);
    let unlinked = state.error_generation;
    apply_event(
        &mut state,
        &mut indices,
        Event::ErrorSource {
            item_id: "provider-incident".into(),
        },
    );
    publish_read_error(&mut state, &mut previous_read, "same diagnostic".into());
    assert!(state.error_generation > unlinked);
    assert_eq!(state.error_item, None);
}

#[test]
fn malformed_text_deltas_replace_provider_sources_with_distinct_scalar_receipts() {
    let mut state = Snapshot::default();
    let mut indices = BTreeMap::new();
    apply_event(
        &mut state,
        &mut indices,
        Event::Item(Item {
            id: "non-text".into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name: "fixture".into(),
                input: String::new(),
                output: String::new(),
                status: ToolStatus::Succeeded,
            },
            started_at: None,
            at: None,
        }),
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::Error {
            message: "provider failed".into(),
            fatal: false,
        },
    );
    apply_event(
        &mut state,
        &mut indices,
        Event::ErrorSource {
            item_id: "provider-incident".into(),
        },
    );
    let provider_receipt = state.error_generation;
    for offset in 1..=2 {
        apply_event(
            &mut state,
            &mut indices,
            Event::TextDelta {
                id: "non-text".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "invalid".into(),
            },
        );
        assert_eq!(state.error_item, None);
        assert_eq!(state.error_generation, provider_receipt + offset);
        assert_eq!(
            state.error.as_deref(),
            Some("Provider reused non-text item ID non-text for a text delta")
        );
    }
}
