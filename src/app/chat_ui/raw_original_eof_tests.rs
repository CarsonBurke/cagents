//! Runtime integration for the private transport-EOF view cache. The client
//! fixture simulates EOF; wire-reader behavior has separate client tests.
use super::*;
use chat::{
    cache::{
        SourceEpoch, SourceKey,
        raw::{RawRepresentation, RawSourceField, RawSourceMetadata, RawSourcePart},
    },
    source_owners::{self, ResidentRaw, ResidentRawCalls},
};

fn fixture() -> (Runtime, Arc<ClientHandle>, ResidentRaw, Arc<str>) {
    let text: Arc<str> = Arc::from("{  \"exact\": \"original λ\" }");
    let proof = ResidentRaw {
        source: SourceKey {
            agent_id: None,
            source_id: "opaque-original".into(),
        },
        field: RawSourceField::ToolInput,
        epoch: SourceEpoch {
            incarnation: uuid::Uuid::from_u128(81313),
            revision: 7,
        },
        source_len: text.len(),
        metadata: RawSourceMetadata {
            tool_name: "Write".into(),
            representation: RawRepresentation::StreamJson,
            complete_call: true,
        },
    };
    assert_eq!(
        source_owners::admit_raw(&text, &proof, None),
        source_owners::Admission::Ready
    );
    let session = fresh_chat_session(
        Provider::Claude,
        TerminalId::new(),
        "/project".into(),
        &Account::system(Provider::Claude),
        false,
    )
    .unwrap();
    let snapshot = Snapshot {
        ready: true,
        session_id: Some("native-session".into()),
        source_incarnation: Some(proof.epoch.incarnation),
        raw_source_parts: vec![RawSourcePart {
            item_id: "visible-tool".into(),
            source: proof.source.clone(),
            field: proof.field,
            epoch: proof.epoch,
            source_len: proof.source_len,
            metadata: proof.metadata.clone(),
        }],
        items: vec![Arc::new(chat::Item {
            id: "visible-tool".into(),
            role: chat::Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Tool {
                name: "Write".into(),
                input: "bounded summary".into(),
                output: String::new(),
                status: chat::ToolStatus::Succeeded,
            },
        })],
        ..Default::default()
    };
    let handle = Arc::new(ClientHandle::fixture(session.clone(), snapshot));
    let mut runtime = Runtime::new(
        Target {
            card: CardId::new(),
            generation: 1,
        },
        session,
        Connection::Live(handle.clone()),
        true,
        crate::settings::EditorSyntaxTheme::default(),
    );
    runtime
        .raw_original
        .restore(ResidentRawCalls {
            calls: vec![(text.clone(), proof.clone())],
        })
        .unwrap();
    (runtime, handle, proof, text)
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "the bounded offline worker did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn runtime_hide_captures_original_before_eviction_and_eof_reveal_adopts_it_once() {
    let (mut runtime, handle, proof, text) = fixture();
    runtime.set_visible(false).unwrap();
    assert!(runtime.raw_original.capture().calls.is_empty());
    // A redundant hidden refresh must not replace the captured sidecar with
    // the empty UI state left by the first eviction.
    runtime.set_visible(false).unwrap();
    wait_until(|| handle.offline_view_metrics().3 > 0);
    handle.fixture_disconnect_for_test();
    runtime.set_visible(true).unwrap();
    wait_until(|| {
        runtime.refresh();
        !runtime.raw_original.capture().calls.is_empty()
    });
    let restored = runtime.raw_original.capture();
    assert_eq!(restored.calls.len(), 1);
    assert_eq!(restored.calls[0].0.as_ref(), text.as_ref());
    assert_eq!(restored.calls[0].1, proof);
    assert_eq!(
        source_owners::resident_raw(&restored.calls[0].0),
        Some(proof)
    );
    assert!(
        handle.take_restored_raw_calls().is_none(),
        "the runtime consumed the transfer"
    );
    runtime.refresh();
    assert_eq!(runtime.raw_original.capture().calls.len(), 1);
    assert_eq!(runtime.snapshot.items[0].id, "visible-tool");
}

#[test]
fn runtime_eof_reveal_installs_current_descriptors_before_restoring_originals() {
    for changed_epoch in [false, true] {
        let (mut runtime, handle, _proof, _text) = fixture();
        runtime.set_visible(false).unwrap();
        wait_until(|| handle.offline_view_metrics().3 > 0);
        let mut latest = (*runtime.snapshot).clone();
        latest.generation += 1;
        if changed_epoch {
            latest.raw_source_parts[0].epoch.revision += 1;
        } else {
            latest.raw_source_parts.clear();
        }
        let expected = latest.raw_source_parts.clone();
        handle.replace_snapshot_for_test(latest);
        handle.fixture_disconnect_for_test();
        runtime.set_visible(true).unwrap();
        wait_until(|| {
            runtime.refresh();
            handle.resident_snapshot_with_clock().is_some()
                && runtime.snapshot.raw_source_parts == expected
        });
        assert!(
            runtime.raw_original.capture().calls.is_empty(),
            "an old original must not regain authority after descriptor removal or replacement"
        );
        assert!(handle.take_restored_raw_calls().is_none());
        assert_eq!(
            runtime.snapshot.items[0].id, "visible-tool",
            "prose recovery is independent"
        );
    }
}
