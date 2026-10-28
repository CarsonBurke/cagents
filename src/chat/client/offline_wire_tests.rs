use super::tests::{host_proof, omp_card};
use super::*;

#[test]
fn hidden_socket_eof_restores_only_its_cached_readonly_view_and_revocation_fences_it() {
    use crate::chat::{
        cache::{
            SourceEpoch, SourceKey,
            raw::{RawRepresentation, RawSourceField, RawSourceMetadata, RawSourcePart},
        },
        source_owners::{self, ResidentRaw, ResidentRawCalls},
    };
    let original: Arc<str> = Arc::from("{ \"command\": \"retained original\" }");
    let record = ResidentRaw {
        source: SourceKey {
            agent_id: None,
            source_id: "offline-original".into(),
        },
        field: RawSourceField::ToolInput,
        epoch: SourceEpoch {
            incarnation: uuid::Uuid::from_u128(41),
            revision: 9,
        },
        source_len: original.len(),
        metadata: RawSourceMetadata {
            tool_name: "Bash".into(),
            representation: RawRepresentation::StreamJson,
            complete_call: true,
        },
    };
    assert_eq!(
        source_owners::admit_raw(&original, &record, None),
        source_owners::Admission::Ready
    );
    let part = RawSourcePart {
        item_id: "offline-tool".into(),
        source: record.source.clone(),
        field: record.field,
        epoch: record.epoch,
        source_len: record.source_len,
        metadata: record.metadata.clone(),
    };
    let (client, mut server) = UnixStream::pair().unwrap();
    let peer = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("offline-view");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, id, response| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(id),
                        proof: proof.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            connect.request_id,
            ServerResponse::Connected {
                proof: proof.clone(),
                resumed: vec![],
            },
        );
        let attach: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = attach.request else {
            panic!("start");
        };
        let chat_id = session.id;
        respond(
            &mut codec,
            &mut server,
            attach.request_id,
            ServerResponse::ChatAttached {
                session,
                snapshot: Snapshot {
                    generation: 77,
                    session_id: Some("cached-exact-session".into()),
                    ready: true,
                    busy: true,
                    turn_generation: 7,
                    sampled_at: Some(1000),
                    raw_source_parts: vec![part],
                    items: vec![Arc::new(super::super::Item {
                        id: "offline-item".into(),
                        role: super::super::Role::Assistant,
                        content: super::super::ItemContent::Text {
                            text: "private cached words".repeat(8192),
                            thinking: false,
                            phase: Default::default(),
                        },
                        started_at: None,
                        at: None,
                    })],
                    ..Default::default()
                },
            },
        );
        let hidden: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            hidden.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            hidden.request_id,
            ServerResponse::Accepted,
        );
        codec
            .write(
                &mut server,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: None,
                    proof: proof.clone(),
                    message: ServerMessage::ChatChanged { chat_id },
                },
            )
            .unwrap();
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ChatRead {
                after_generation: Some(77),
                ..
            }
        ));
        respond(
            &mut codec,
            &mut server,
            read.request_id,
            ServerResponse::ChatUpdate {
                chat_id,
                update: super::super::presentation::Presentation {
                    base_generation: 77,
                    file_changes_unchanged: false,
                    order: vec!["offline-item".into()],
                    changes: vec![],
                    children: vec![],
                    metadata: Snapshot {
                        generation: 78,
                        session_id: Some("cached-exact-session".into()),
                        ready: true,
                        busy: false,
                        turn_generation: 8,
                        sampled_at: Some(2000),
                        finished_at: Some(1900),
                        error: Some("Latest qualified outcome".into()),
                        ..Default::default()
                    },
                },
            },
        );
        // Real transport EOF occurs only after the live client evicts its
        // body. No server remains to answer a reveal or start a provider.
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        omp_card(None, None),
        Intent::Start,
        None,
        false,
        None,
        None,
        move |wake| {
            let _ = tx.send(wake);
        },
    )
    .unwrap();
    let weak_original = Arc::downgrade(&original);
    handle.remember_visible_view_with_sources(
        handle.snapshot(),
        None,
        Default::default(),
        ResidentRawCalls {
            calls: vec![(original, record)],
        },
    );
    let weak_body = Arc::downgrade(&handle.snapshot().items[0]);
    handle.set_interest(true, false).unwrap();
    while !matches!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Wake::Disconnected(_)
    ) {}
    assert!(handle.resident_snapshot_with_clock().is_none());
    assert_eq!(handle.snapshot().generation, 78);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while weak_body.upgrade().is_some() || weak_original.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "the spill worker must release its raw Arc after writing"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        handle.offline_view_metrics().3 > 0,
        "the fallback must live on an anonymous disk file"
    );
    for _ in 0..2 {
        handle.set_visible(true).unwrap();
        while handle.resident_snapshot_with_clock().is_none() {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let snapshot = handle.snapshot();
        assert!(
            snapshot.raw_source_parts.is_empty(),
            "the hidden metadata removed the original without changing prose revision"
        );
        assert_eq!(snapshot.source_revision, 0);
        assert!(
            handle.take_restored_raw_calls().unwrap().calls.is_empty(),
            "actual EOF restoration cannot resurrect the removed original"
        );
        assert!(handle.take_restored_raw_calls().is_none());
        assert_eq!(
            snapshot.generation, 78,
            "cached body must not roll back hidden control metadata"
        );
        assert!(!snapshot.busy);
        assert_eq!(snapshot.turn_generation, 8);
        assert_eq!(snapshot.finished_at, Some(1900));
        assert_eq!(snapshot.error.as_deref(), Some("Latest qualified outcome"));
        assert!(handle.snapshot_with_clock().1.unwrap().sampled_at >= 2000);
        assert_eq!(snapshot.session_id.as_deref(), Some("cached-exact-session"));
        assert!(snapshot.transcript_bytes() > 0);
        assert!(
            snapshot
                .read_only_reason
                .as_deref()
                .unwrap()
                .contains("last cached view")
        );
        assert!(
            handle.send(Command::Interrupt).is_err(),
            "cached history grants no provider authority"
        );
        drop(snapshot);
        handle.set_visible(false).unwrap();
        assert_eq!(handle.snapshot().transcript_bytes(), 0);
    }
    handle.revoke_local_history();
    assert!(handle.set_visible(true).is_err());
    assert!(handle.resident_snapshot_with_clock().is_none());
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert_eq!(handle.offline_view_metrics().3, 0);
    peer.join().unwrap();
}

#[test]
fn hidden_final_publication_keeps_authoritative_body_while_restoring_raw_once() {
    use crate::chat::{
        Item, ItemContent, Role, Subagent, ToolStatus,
        cache::{
            SourceEpoch, SourceKey, SourceMetadata, SourcePart,
            raw::{RawRepresentation, RawSourceField, RawSourceMetadata, RawSourcePart},
        },
        source_owners::{self, ResidentRaw, ResidentRawCalls},
    };
    fn item(id: &str, body: &str) -> Arc<Item> {
        Arc::new(Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Default::default(),
            },
            started_at: None,
            at: None,
        })
    }
    fn parts(items: &[Arc<Item>], epoch: SourceEpoch) -> Vec<SourcePart> {
        items
            .iter()
            .map(|item| SourcePart {
                item_id: item.id.clone(),
                source: SourceKey {
                    agent_id: None,
                    source_id: item.id.clone(),
                },
                start: 0,
                end: item.content.byte_len(),
                source_len: item.content.byte_len(),
                replacement: false,
                epoch,
                metadata: SourceMetadata::from_item(item).unwrap(),
            })
            .collect()
    }
    fn child(items: Vec<Arc<Item>>) -> Subagent {
        Subagent {
            id: "same-child".into(),
            kind: "fixture".into(),
            description: "Retained child".into(),
            status: ToolStatus::Succeeded,
            model: None,
            waiting: false,
            detail: String::new(),
            background: false,
            items,
        }
    }
    for empty_final in [false, true] {
        let epoch = SourceEpoch {
            incarnation: uuid::Uuid::from_u128(55),
            revision: 3,
        };
        let original: Arc<str> = Arc::from("{  \"command\": \"original unchanged call\" }");
        let record = ResidentRaw {
            source: SourceKey {
                agent_id: None,
                source_id: "final-raw".into(),
            },
            field: RawSourceField::ToolInput,
            epoch,
            source_len: original.len(),
            metadata: RawSourceMetadata {
                tool_name: "Bash".into(),
                representation: RawRepresentation::StreamJson,
                complete_call: true,
            },
        };
        assert_eq!(
            source_owners::admit_raw(&original, &record, None),
            source_owners::Admission::Ready
        );
        let raw_part = RawSourcePart {
            item_id: "final-tool".into(),
            source: record.source.clone(),
            field: record.field,
            epoch,
            source_len: record.source_len,
            metadata: record.metadata.clone(),
        };
        let (client, mut server) = UnixStream::pair().unwrap();
        let (hidden_tx, hidden_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = host_proof("authoritative-final");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let respond =
                |codec: &mut FrameCodec, server: &mut UnixStream, request_id, response| {
                    codec
                        .write(
                            server,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id,
                                proof: proof.clone(),
                                message: ServerMessage::Response { response },
                            },
                        )
                        .unwrap();
                };
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            respond(
                &mut codec,
                &mut server,
                Some(connect.request_id),
                ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            );
            let attach: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = attach.request else {
                panic!("start");
            };
            let chat_id = session.id;
            let old_items = vec![item("body", "old rendered body")];
            respond(
                &mut codec,
                &mut server,
                Some(attach.request_id),
                ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 40,
                        ready: true,
                        session_id: Some("same-final-session".into()),
                        source_incarnation: Some(epoch.incarnation),
                        source_revision: 11,
                        source_parts: parts(&old_items, epoch),
                        items: old_items,
                        raw_source_parts: vec![raw_part.clone()],
                        subagents: vec![child(vec![item("child-body", "old child body")])],
                        ..Default::default()
                    },
                },
            );
            let hidden: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            assert!(matches!(
                hidden.request,
                ClientRequest::ChatVisibility { visible: false, .. }
            ));
            respond(
                &mut codec,
                &mut server,
                Some(hidden.request_id),
                ServerResponse::Accepted,
            );
            hidden_tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let items = if empty_final {
                vec![]
            } else {
                vec![item("body", "new authoritative final body")]
            };
            let child_items = if empty_final {
                vec![]
            } else {
                vec![item("child-body", "new authoritative child body")]
            };
            respond(
                &mut codec,
                &mut server,
                None,
                ServerResponse::ChatClosed {
                    chat_id,
                    snapshot: Snapshot {
                        generation: 42,
                        ready: true,
                        exited: true,
                        session_id: Some("same-final-session".into()),
                        finished_at: Some(99),
                        source_incarnation: Some(epoch.incarnation),
                        source_revision: 12,
                        source_parts: parts(
                            &items,
                            SourceEpoch {
                                revision: 12,
                                ..epoch
                            },
                        ),
                        items,
                        raw_source_parts: vec![raw_part],
                        subagents: vec![child(child_items)],
                        read_only_reason: (!empty_final)
                            .then(|| "Authoritative closed transcript".into()),
                        ..Default::default()
                    },
                },
            );
        });
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle = ClientHandle::connect_on(
            client,
            omp_card(None, None),
            Intent::Start,
            None,
            false,
            None,
            None,
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        let old_body = Arc::downgrade(&handle.snapshot().items[0]);
        let old_raw = Arc::downgrade(&original);
        handle.remember_visible_view_with_sources(
            handle.snapshot(),
            None,
            Default::default(),
            ResidentRawCalls {
                calls: vec![(original, record.clone())],
            },
        );
        handle.set_visible(false).unwrap();
        hidden_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while old_body.upgrade().is_some() || old_raw.upgrade().is_some() {
            assert!(
                Instant::now() < deadline,
                "the old rendered view must spill before final publication"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert!(handle.offline_view_metrics().3 > 0);
        finish_tx.send(()).unwrap();
        worker.join().unwrap();
        while handle.inner.alive.load(Ordering::Acquire) {
            wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let authoritative = handle.resident_snapshot_with_clock().unwrap().0;
        assert_eq!(authoritative.generation, 42);
        assert!(authoritative.exited);
        assert_eq!(authoritative.items.is_empty(), empty_final);
        assert_eq!(authoritative.source_revision, 12);
        handle.set_visible(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let restored = loop {
            if let Some(restored) = handle.take_restored_raw_calls() {
                break restored;
            }
            wake_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
        };
        let after = handle.snapshot();
        assert!(Arc::ptr_eq(&after, &authoritative));
        assert_eq!(
            after.as_ref(),
            authoritative.as_ref(),
            "cached sidecars must not roll back a final body, descriptors or read-only reason"
        );
        for (after, before) in after.items.iter().zip(&authoritative.items) {
            assert!(Arc::ptr_eq(after, before));
        }
        assert_eq!(restored.calls.len(), 1);
        assert_eq!(
            restored.calls[0].0.as_ref(),
            "{  \"command\": \"original unchanged call\" }"
        );
        assert_eq!(
            source_owners::resident_raw(&restored.calls[0].0),
            Some(record)
        );
        assert!(handle.take_restored_raw_calls().is_none());
        assert!(handle.send(Command::Interrupt).is_err());
    }
}

#[test]
fn hidden_wire_client_releases_history_and_reveals_at_the_same_generation() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let (hidden_tx, hidden_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("live");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("chat start expected");
        };
        let chat_id = session.id;
        let snapshot = || Snapshot {
            generation: 41,
            session_id: Some("exact-session".into()),
            ready: true,
            items: vec![Arc::new(super::super::Item {
                id: "history".into(),
                role: super::super::Role::Assistant,
                content: super::super::ItemContent::Text {
                    text: "body".repeat(64 * 1024),
                    thinking: false,
                    phase: Default::default(),
                },
                started_at: None,
                at: None,
            })],
            ..Default::default()
        };
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: snapshot(),
                },
            },
        );
        let hidden: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            hidden.request,
            ClientRequest::ChatVisibility { visible: false, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(hidden.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        hidden_tx.send(()).unwrap();
        let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            shown.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(shown.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                read.request,
                ClientRequest::ChatRead {
                    after_generation: None,
                    ..
                }
            ),
            "the current generation is metadata only and cannot suppress hydration"
        );
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(snapshot()),
                },
            },
        );
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        omp_card(None, None),
        Intent::Start,
        None,
        false,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    let body = Arc::downgrade(&handle.snapshot().items[0]);
    handle.set_visible(false).unwrap();
    hidden_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(handle.snapshot().generation, 41);
    assert_eq!(
        handle.snapshot().session_id.as_deref(),
        Some("exact-session")
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while body.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "the hidden body must finish spilling"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(handle.resident_snapshot_with_clock().is_none());
    handle.set_visible(true).unwrap();
    while handle.resident_snapshot_with_clock().is_none() {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert_eq!(handle.snapshot().generation, 41);
    assert_eq!(handle.snapshot().transcript_bytes(), 256 * 1024);
    done_tx.send(()).unwrap();
    worker.join().unwrap();
    while !matches!(
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Wake::Disconnected(_)
    ) {}
    let last_body = Arc::downgrade(&handle.snapshot().items[0]);
    handle.set_visible(false).unwrap();
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert!(handle.resident_snapshot_with_clock().is_none());
    let deadline = Instant::now() + Duration::from_secs(5);
    while last_body.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "the disconnected body must finish spilling"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(handle.offline_view_metrics().3 > 0);
    // The only socket peer has exited. The first reveal must use the private
    // cache, without a replacement connection or provider authority.
    handle.set_visible(true).unwrap();
    while handle.resident_snapshot_with_clock().is_none() {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let restored = handle.snapshot();
    assert_eq!(restored.generation, 41);
    assert_eq!(restored.session_id.as_deref(), Some("exact-session"));
    assert_eq!(
        restored.transcript_bytes(),
        256 * 1024,
        "a disconnected reveal restores the last readable publication"
    );
    let super::super::ItemContent::Text { text, .. } = &restored.items[0].content else {
        panic!("the restored history must remain text");
    };
    assert_eq!(text, &"body".repeat(64 * 1024));
    assert!(
        restored
            .read_only_reason
            .as_deref()
            .unwrap()
            .contains("last cached view")
    );
    assert!(handle.send(Command::Interrupt).is_err());
}

#[test]
fn hidden_control_subscribers_advance_metadata_without_rehydration_loops() {
    use super::super::{
        Item, ItemContent, Role,
        presentation::{ItemChange, Presentation},
    };
    let (client, mut server) = UnixStream::pair().unwrap();
    let (advance_tx, advance_rx) = std::sync::mpsc::channel();
    let (stage_tx, stage_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let proof = host_proof("control-reader");
        let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
        let respond = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
            codec
                .write(
                    server,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id,
                        proof: proof.clone(),
                        message,
                    },
                )
                .unwrap();
        };
        let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        respond(
            &mut codec,
            &mut server,
            Some(connect.request_id),
            ServerMessage::Response {
                response: ServerResponse::Connected {
                    proof: proof.clone(),
                    resumed: vec![],
                },
            },
        );
        let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        let ClientRequest::ChatStart { session, .. } = start.request else {
            panic!("chat start expected");
        };
        let chat_id = session.id;
        respond(
            &mut codec,
            &mut server,
            Some(start.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatAttached {
                    session,
                    snapshot: Snapshot {
                        generation: 1,
                        ..Default::default()
                    },
                },
            },
        );
        let interested: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            interested.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(interested.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ChatRead {
                after_generation: Some(1),
                ..
            }
        ));
        let prompt = Item {
            id: "received-prompt".into(),
            role: Role::User,
            content: ItemContent::Text {
                text: "prompt".repeat(64 * 1024),
                thinking: false,
                phase: Default::default(),
            },
            started_at: None,
            at: None,
        };
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatUpdate {
                    chat_id,
                    update: Presentation {
                        base_generation: 99,
                        file_changes_unchanged: true,
                        metadata: Snapshot {
                            generation: 100,
                            ready: true,
                            ..Default::default()
                        },
                        order: vec![],
                        changes: vec![],
                        children: vec![],
                    },
                },
            },
        );
        let resync: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                resync.request,
                ClientRequest::ChatRead {
                    after_generation: None,
                    ..
                }
            ),
            "hidden controls must reject an update with a mismatched metadata base"
        );
        respond(
            &mut codec,
            &mut server,
            Some(resync.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(Snapshot {
                        generation: 1,
                        ..Default::default()
                    }),
                },
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ChatRead {
                after_generation: Some(1),
                ..
            }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatUpdate {
                    chat_id,
                    update: Presentation {
                        base_generation: 1,
                        file_changes_unchanged: true,
                        metadata: Snapshot {
                            generation: 2,
                            ready: true,
                            ..Default::default()
                        },
                        order: vec![prompt.id.clone()],
                        changes: vec![ItemChange::Replace(prompt.clone())],
                        children: vec![],
                    },
                },
            },
        );
        let command: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(&command.request, ClientRequest::ChatCommand {
                command: Command::Prompt { text, .. }, ..
            } if text.len() > 1024 * 1024));
        respond(
            &mut codec,
            &mut server,
            Some(command.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let unchanged: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(
            matches!(
                unchanged.request,
                ClientRequest::ChatRead {
                    after_generation: Some(2),
                    ..
                }
            ),
            "discarded bodies must not cause hidden control resync loops"
        );
        respond(
            &mut codec,
            &mut server,
            Some(unchanged.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: None,
                },
            },
        );
        // This ordered control edge lets the client prove it consumed the
        // unchanged response while still hidden before requesting reveal.
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatActivity {
                chat_id,
                state: ActivityState::Working,
                generation: 2,
                outcome: None,
            },
        );
        stage_tx.send(()).unwrap();
        // The next frame must be reveal, not another automatic full read.
        let shown: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            shown.request,
            ClientRequest::ChatVisibility { visible: true, .. }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(shown.request_id),
            ServerMessage::Response {
                response: ServerResponse::Accepted,
            },
        );
        respond(
            &mut codec,
            &mut server,
            None,
            ServerMessage::ChatChanged { chat_id },
        );
        let read: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
        assert!(matches!(
            read.request,
            ClientRequest::ChatRead {
                after_generation: None,
                ..
            }
        ));
        respond(
            &mut codec,
            &mut server,
            Some(read.request_id),
            ServerMessage::Response {
                response: ServerResponse::ChatSnapshot {
                    chat_id,
                    snapshot: Some(Snapshot {
                        generation: 2,
                        ready: true,
                        items: vec![Arc::new(prompt)],
                        ..Default::default()
                    }),
                },
            },
        );
        advance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let handle = ClientHandle::connect_on(
        client,
        omp_card(None, None),
        Intent::Start,
        None,
        false,
        None,
        None,
        move |wake| {
            let _ = wake_tx.send(wake);
        },
    )
    .unwrap();
    {
        let mut wire = handle.inner.wire.lock().unwrap();
        wire.codec
            .write(&mut std::io::sink(), &"large prompt".repeat(128 * 1024))
            .unwrap();
        assert!(wire.codec.buffer_capacities_for_test().1 > 1024 * 1024);
    }
    handle.set_interest(true, false).unwrap();
    while handle.snapshot().generation != 2 {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle
        .inner
        .wire
        .lock()
        .unwrap()
        .codec
        .buffer_capacities_for_test()
        .1
        > 256 * 1024
    {
        assert!(
            Instant::now() < deadline,
            "hidden interested transport retained its large prompt buffer"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(handle.snapshot().ready);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert!(handle.has_observed_user_item());
    assert!(handle.resident_snapshot_with_clock().is_none());
    handle
        .send(Command::Prompt {
            text: "large prompt".repeat(128 * 1024),
            attachments: vec![],
            mode: super::super::PromptMode::Send,
        })
        .unwrap();
    assert!(
        handle
            .inner
            .wire
            .lock()
            .unwrap()
            .codec
            .buffer_capacities_for_test()
            .1
            <= 256 * 1024,
        "new large hidden commands release scratch without another visibility transition"
    );
    advance_tx.send(()).unwrap();
    stage_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    while !matches!(
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Wake::Activity
    ) {}
    handle.set_visible(true).unwrap();
    while handle.resident_snapshot_with_clock().is_none() {
        wake_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    assert_eq!(handle.snapshot().generation, 2);
    assert_eq!(handle.snapshot().transcript_bytes(), 6 * 64 * 1024);
    advance_tx.send(()).unwrap();
    worker.join().unwrap();
}
