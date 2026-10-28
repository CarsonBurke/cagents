// Real App/StateStore/ordered-writer tests with a private framed daemon peer.
// The peer is synthetic: these tests do not certify Host retirement, accounts,
// provider behavior, native rendering or recovery of the production eight.
struct RetainedHistoryFixture {
    app: Cagents,
    directory: tempfile::TempDir,
    path: PathBuf,
    original: CardId,
    card: CardId,
    source: chat::Session,
}

fn retained_history_fixture(image: &Attachment, cold: bool) -> RetainedHistoryFixture {
    retained_history_fixture_text(image, cold, "Saved original draft λ")
}

fn retained_history_fixture_text(
    image: &Attachment,
    cold: bool,
    draft: &str,
) -> RetainedHistoryFixture {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let original = live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, original).target;
    let mut source = chat_runtime(&app, original).session.clone();
    source.terminal = None;
    source.owned_codex_listener = None;
    source.native_continuity = chat::NativeContinuity::ReconnectOnly;
    source.proof = Some(crate::model::HostSessionProof {
        host_session_id: "retained-old-host".into(),
        host_incarnation: "retained-old-instance".into(),
        boot_id: "retained-old-boot".into(),
    });
    app.install_chat_session(original, source.clone());
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.session = source.clone();
    runtime.replace_draft(text_editor::Content::with_text(draft));
    runtime.replace_attachments(vec![image.clone()]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("retained-history.sqlite3");
    let (writer, _) =
        crate::session::SessionRuntime::open(&path, "retained-old-boot".into()).unwrap();
    app.session = Some(writer);
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(10))
        .unwrap();

    app.chat_runtime_mut(target).unwrap().connection = Connection::AwaitingAdmission;
    Arc::make_mut(&mut app.durable)
        .pending_chat_admissions
        .insert(original, source.clone());
    let mut canonical_card = app.card_state(original).unwrap().clone();
    let canonical = retained_reader_session(&source);
    // Typed fixture setup of an already advanced canonical card; no Host proof
    // or production DB is manufactured by this App/storage regression.
    app.session
        .as_ref()
        .unwrap()
        .operation_store()
        .rebind_chat_upgrade(original, &source, &canonical)
        .unwrap();
    canonical_card.kind = CardKind::Chat(canonical);
    let pane = crate::workspace_model::views::Pane::from_card(&canonical_card, None).unwrap();
    let (task, card) = app.retain_unresolved_shared_upgrade(&pane);
    let card = card.expect("actual incompatible shared source retains a new local card");
    let receipt = account_commit_reply(task);
    assert!(
        matches!(&receipt, Message::ChatUi(Action::RetainedCopyCommitted(id, Ok(()))) if *id == card)
    );
    drop(app.update(receipt));
    // The canonical card need not have a live pane for its retained copy.
    app.card_state_mut(original).unwrap().existence = Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 1,
            reason: Some("Fixture original pane closed".into()),
        },
    };
    app.cards.remove(&original);
    Arc::make_mut(&mut app.durable).focused = Some(card);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.resize_viewport(1600.0);
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(10))
        .unwrap();
    if cold {
        drop(app.session.take());
        let (writer, restored) =
            crate::session::SessionRuntime::open(&path, "retained-old-boot".into()).unwrap();
        let restored = restored.unwrap();
        let mut fresh = crate::app::tests::context_inspector_test_app(false);
        fresh.cards.clear();
        fresh.chat_ui = State::with_draft_attachments(
            restored.chat_drafts.clone(),
            restored.chat_draft_attachments.clone(),
        );
        fresh.durable = Arc::new(restored);
        fresh.workspace = Cagents::rebuild_workspace(&fresh.durable);
        fresh.workspace.resize_viewport(1600.0);
        fresh.session = Some(writer);
        // This is the production cold-load entry, not a directly made reader.
        drop(fresh.initialize_runtimes());
        app = fresh;
    }
    assert!(!app.cards.contains_key(&original));
    assert!(app.chat_awaiting_admission(card));
    assert!(chat_runtime(&app, card).retained_history_candidate);
    assert_eq!(chat_runtime(&app, card).session, source);
    RetainedHistoryFixture {
        app,
        directory,
        path,
        original,
        card,
        source,
    }
}

fn retained_reader_session(source: &chat::Session) -> chat::Session {
    let mut reader = source.clone();
    reader.proof = Some(crate::model::HostSessionProof {
        host_session_id: "retained-current-host".into(),
        host_incarnation: "retained-current-instance".into(),
        boot_id: "retained-current-boot".into(),
    });
    reader
}

fn retained_history_snapshot(reader: &chat::Session, generation: u64) -> Snapshot {
    Snapshot {
        generation,
        session_id: reader.launch.session_id.clone(),
        exited: true,
        read_only_reason: Some("Saved history; no provider is running".into()),
        items: vec![Arc::new(chat::Item {
            id: "retained-saved-answer".into(),
            role: chat::Role::Assistant,
            content: text("Saved answer"),
            started_at: None,
            at: None,
        })],
        ..Default::default()
    }
}

fn begin_retained_history(app: &mut Cagents, card: CardId) -> Target {
    let target = chat_runtime(app, card).target;
    drop(app.handle_chat_ui(Action::OpenSavedHistory(target)));
    let operation = app.chat_ui.phases[&card].target();
    assert_ne!(operation, target);
    drop(app.handle_chat_ui(Action::OpenSavedHistory(target)));
    assert_eq!(
        app.chat_ui.phases[&card].target(),
        operation,
        "duplicate clicks must not replace the operation"
    );
    operation
}

fn framed_retained_reader(
    app: &Cagents,
    source: &chat::Session,
    operation: Target,
    delayed: bool,
) -> (
    Arc<ClientHandle>,
    chat::client::SavedHistoryAdmission,
    chat::client::saved_history_fixture::Peer,
) {
    let reader = retained_reader_session(source);
    let snapshot = retained_history_snapshot(&reader, 3);
    let (stream, peer) = chat::client::saved_history_fixture::Peer::new(
        source.clone(),
        reader.clone(),
        reader,
        snapshot,
        delayed,
    );
    let sender = app.chat_ui.sender.clone();
    let (handle, witness) =
        ClientHandle::connect_saved_history_on(stream, source.clone(), move |wake| {
            let _ = sender.try_send((operation, wake));
        })
        .unwrap();
    (Arc::new(handle), witness, peer)
}

fn retained_open_reply(
    app: &mut Cagents,
    operation: Target,
    handle: Arc<ClientHandle>,
    witness: chat::client::SavedHistoryAdmission,
) -> Task<Message> {
    app.handle_chat_ui(Action::SavedHistoryOpened(
        operation,
        HistoryReady(Arc::new(Mutex::new(Some(Ok((handle, witness)))))),
    ))
}

#[test]
fn cold_retained_copy_opens_history_after_atomic_ack_then_requires_explicit_resume() {
    chat::attachment_assets::with_test_image(|image| {
        let mut f = retained_history_fixture(&image, true);
        let before = f
            .app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_card(f.original, None)
            .unwrap()
            .unwrap();
        let runtime = chat_runtime(&f.app, f.card);
        let allocation = runtime as *const Runtime as usize;
        let operation = begin_retained_history(&mut f.app, f.card);
        let (handle, witness, peer) = framed_retained_reader(&f.app, &f.source, operation, true);
        assert!(
            peer.requests.lock().unwrap().contains(&"Read"),
            "loading must settle over actual framed hydration"
        );
        while let Ok((target, wake)) = f.app.chat_ui.receiver.try_recv() {
            drop(f.app.handle_chat_ui(Action::Wake(target, wake)));
        }
        assert!(
            f.app.chat_awaiting_admission(f.card),
            "pre-ACK wakes do not install the pending handle"
        );
        let checkpoint =
            retained_open_reply(&mut f.app, operation, handle.clone(), witness.clone());
        assert_eq!(chat_runtime(&f.app, f.card).session, f.source);
        assert!(f.app.chat_awaiting_admission(f.card));
        let target = chat_runtime(&f.app, f.card).target;
        f.app
            .chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text(
                "Latest draft while ACK is pending",
            ));
        let late_image =
            chat::attachment_assets::test_image("Late image while history save is pending");
        let original_image_bytes = std::fs::read(&image.path).unwrap();
        let late_image_bytes = std::fs::read(&late_image.path).unwrap();
        f.app
            .chat_runtime_mut(target)
            .unwrap()
            .replace_attachments(vec![image.clone(), late_image.clone()]);
        f.app.persist();
        let receipt = account_commit_reply(checkpoint);
        assert!(matches!(
            &receipt,
            Message::ChatUi(Action::SavedHistoryCommitted(_, Ok(())))
        ));
        drop(f.app.update(receipt));
        let runtime = chat_runtime(&f.app, f.card);
        assert_eq!(runtime as *const Runtime as usize, allocation);
        assert_eq!(runtime.target, operation);
        assert_eq!(runtime.composer.text(), "Latest draft while ACK is pending");
        assert_eq!(runtime.attachments, vec![image.clone(), late_image.clone()]);
        assert!(runtime.has_connected_saved_history(&runtime.snapshot));
        assert!(!f.app.chat_start_pending(f.card));
        assert!(!f.app.chat_ui.explicit_starts.contains_key(&f.card));
        f.app
            .session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(10))
            .unwrap();
        let store = f.app.session.as_ref().unwrap().operation_store();
        let saved = store
            .load_workspace(f.app.durable.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(saved.pending_chat_admissions.get(&f.card), None);
        assert!(saved.local_only_cards.contains(&f.card));
        assert_eq!(
            saved.chat_drafts[&f.card],
            "Latest draft while ACK is pending"
        );
        assert_eq!(std::fs::read(&image.path).unwrap(), original_image_bytes);
        assert_eq!(std::fs::read(&late_image.path).unwrap(), late_image_bytes);
        assert_eq!(
            saved.chat_draft_attachments[&f.card],
            vec![image, late_image]
        );
        assert_eq!(store.load_card(f.original, None).unwrap().unwrap(), before);
        // Post-ACK updates use the newly owned operation target and actual wire.
        let mut updated = retained_history_snapshot(witness.reader(), 5);
        updated.items[0] = Arc::new(chat::Item {
            id: "retained-saved-answer".into(),
            role: chat::Role::Assistant,
            content: text("Hydrated after ACK"),
            started_at: None,
            at: None,
        });
        peer.publish(updated);
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.snapshot().generation < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(handle.snapshot().generation, 5);
        drop(f.app.handle_chat_ui(Action::Wake(operation, Wake::Changed)));
        assert_eq!(chat_runtime(&f.app, f.card).snapshot.generation, 5);
        // An explicit separate Resume starts existing acknowledged A→B flow.
        drop(f.app.handle_chat_ui(Action::ResumeSaved(operation)));
        let preparing = f.app.chat_ui.phases[&f.card].target();
        assert!(matches!(
            f.app.chat_ui.phases[&f.card],
            Phase::PreparingResume(_)
        ));
        let mut successor = witness.reader().clone();
        successor.id = TerminalId::new();
        let checkpoint = f
            .app
            .handle_chat_ui(Action::ResumePrepared(preparing, Ok(successor.clone())));
        let receipt = account_commit_reply(checkpoint);
        drop(f.app.update(receipt));
        let attempt = f.app.chat_ui.attempt(f.card).unwrap();
        assert_eq!(attempt.intent, chat::Intent::Start);
        assert_eq!(attempt.session, successor);
        assert_eq!(chat_runtime(&f.app, f.card).attachments.len(), 2);
        // No fixture provider process: returned Start task deliberately unpolled.
        let requests = peer.requests.lock().unwrap();
        assert!(requests.starts_with(&["Connect", "Resolve", "Attach"]));
        assert!(requests[3..].iter().all(|request| *request == "Read"));
    });
}

#[test]
fn retained_history_lost_ack_needs_fresh_explicit_retry_and_keeps_latest_content() {
    retained_history_ack_retry_case(false);
}

#[test]
fn retained_history_coalesced_aba_does_not_rearm_cancelled_snapshot_provenance() {
    retained_history_ack_retry_case(true);
}

fn retained_history_ack_retry_case(aba: bool) {
    chat::attachment_assets::with_test_image(|image| {
        let mut f = retained_history_fixture(&image, false);
        let operation = begin_retained_history(&mut f.app, f.card);
        let (handle, witness, _peer) = framed_retained_reader(&f.app, &f.source, operation, false);
        let checkpoint = retained_open_reply(&mut f.app, operation, handle, witness.clone());
        let old_ack = account_commit_reply(checkpoint);
        assert!(matches!(
            &old_ack,
            Message::ChatUi(Action::SavedHistoryCommitted(_, Ok(())))
        ));
        // Storage succeeded but its UI acknowledgement is lost/rejected.
        if aba {
            let mut other = f.source.clone();
            other
                .launch
                .recipe
                .environment
                .insert("EXPLICIT_OTHER".into(), "yes".into());
            f.app.install_chat_session(f.card, other);
            f.app.install_chat_session(f.card, f.source.clone());
            // Only the final A snapshot reaches the actual coalescing queue.
            // No intermediate B/other snapshot can retire a descriptor alias.
            f.app.persist();
            let _ = f
                .app
                .session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(10));
            let saved = f
                .app
                .session
                .as_ref()
                .unwrap()
                .operation_store()
                .load_workspace(f.app.durable.id, None)
                .unwrap()
                .unwrap();
            assert!(!saved.pending_chat_admissions.contains_key(&f.card));
            assert_eq!(
                saved
                    .cards
                    .iter()
                    .find(|card| card.id == f.card)
                    .unwrap()
                    .kind,
                CardKind::Chat(witness.reader().clone())
            );
        }
        drop(f.app.handle_chat_ui(Action::SavedHistoryCommitted(
            operation,
            Err("Fixture lost acknowledgement".into()),
        )));
        assert!(f.app.chat_awaiting_admission(f.card));
        let target = chat_runtime(&f.app, f.card).target;
        f.app
            .chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text(
                "Newest draft after lost ACK",
            ));
        let retry = begin_retained_history(&mut f.app, f.card);
        drop(f.app.update(old_ack));
        assert_eq!(f.app.chat_ui.phases[&f.card].target(), retry);
        let (handle, retry_witness, _retry_peer) =
            framed_retained_reader(&f.app, &f.source, retry, false);
        assert_eq!(retry_witness.reader(), witness.reader());
        let checkpoint = retained_open_reply(&mut f.app, retry, handle, retry_witness);
        let receipt = account_commit_reply(checkpoint);
        assert!(matches!(
            &receipt,
            Message::ChatUi(Action::SavedHistoryCommitted(_, Ok(())))
        ));
        drop(f.app.update(receipt));
        f.app
            .session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(10))
            .unwrap();
        let saved = f
            .app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_workspace(f.app.durable.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(saved.chat_drafts[&f.card], "Newest draft after lost ACK");
        assert_eq!(saved.chat_draft_attachments[&f.card], vec![image]);
        assert!(!saved.pending_chat_admissions.contains_key(&f.card));
        assert!(f.app.chat_ui.attempt(f.card).is_none());
    });
}

#[test]
fn retained_history_stale_context_and_changed_ack_connection_remain_inert() {
    chat::attachment_assets::with_test_image(|image| {
        for case in [
            "writer",
            "local",
            "pending",
            "source",
            "closed-elsewhere",
            "deleting",
            "window",
            "actor",
            "busy",
            "error",
            "disconnected",
        ] {
            let mut f = retained_history_fixture(&image, true);
            let operation = begin_retained_history(&mut f.app, f.card);
            let (handle, witness, peer) =
                framed_retained_reader(&f.app, &f.source, operation, false);
            let checkpoint =
                retained_open_reply(&mut f.app, operation, handle.clone(), witness.clone());
            let receipt = account_commit_reply(checkpoint);
            match case {
                "writer" => {
                    let (writer, _) = crate::session::SessionRuntime::open(
                        f.directory.path().join("other.sqlite3"),
                        "retained-old-boot".into(),
                    )
                    .unwrap();
                    f.app.session = Some(writer);
                }
                "local" => {
                    Arc::make_mut(&mut f.app.durable)
                        .local_only_cards
                        .remove(&f.card);
                }
                "pending" => {
                    Arc::make_mut(&mut f.app.durable)
                        .pending_chat_admissions
                        .remove(&f.card);
                }
                "source" => {
                    let mut other = f.source.clone();
                    other
                        .launch
                        .recipe
                        .environment
                        .insert("CHANGED".into(), "yes".into());
                    f.app.install_chat_session(f.card, other);
                }
                "window" => f
                    .app
                    .core
                    .set_main_window_id(Some(iced::window::Id::unique())),
                "deleting" => f
                    .app
                    .project_ui
                    .begin_deleting(f.source.launch.recipe.working_directory.clone()),
                "closed-elsewhere" => {
                    let target = chat_runtime(&f.app, f.card).target;
                    f.app.chat_runtime_mut(target).unwrap().closed_elsewhere = true;
                }
                "disconnected" => peer.disconnect(),
                _ => {
                    let mut changed = retained_history_snapshot(witness.reader(), 8);
                    match case {
                        "actor" => {
                            changed.native_continuity =
                                chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
                        }
                        "busy" => changed.busy = true,
                        "error" => changed.error = Some("Fixture changed while saving".into()),
                        _ => unreachable!(),
                    };
                    peer.publish(changed);
                }
            }
            if matches!(case, "actor" | "busy" | "error" | "disconnected") {
                let deadline = Instant::now() + Duration::from_secs(5);
                while handle.is_connected()
                    && handle.snapshot().generation < 8
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(!handle.is_connected() || handle.snapshot().generation >= 8);
            }
            drop(f.app.update(receipt));
            assert!(f.app.chat_ui.attempt(f.card).is_none(), "{case}");
            assert!(
                !f.app.chat_ui.explicit_starts.contains_key(&f.card),
                "{case}"
            );
            assert!(f.app.chat_awaiting_admission(f.card), "{case}");
            assert_eq!(
                chat_runtime(&f.app, f.card).attachments,
                vec![image.clone()],
                "{case}"
            );
            assert!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| ["Connect", "Resolve", "Attach", "Read"].contains(request))
            );
        }
    });
}

#[test]
fn strict_retained_history_wire_refuses_actor_mismatch_and_unsettled_results() {
    chat::attachment_assets::with_test_image(|image| {
        let f = retained_history_fixture(&image, true);
        for case in [
            "same-proof",
            "actor-resolve",
            "wrong-launch",
            "actor-attach",
            "wrong-id",
            "busy",
            "waiting",
            "error",
            "wrong-thread",
            "requests",
            "live",
        ] {
            let mut resolved = if case == "same-proof" {
                f.source.clone()
            } else {
                retained_reader_session(&f.source)
            };
            match case {
                "actor-resolve" => {
                    resolved.native_continuity =
                        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
                }
                "wrong-launch" => {
                    resolved
                        .launch
                        .recipe
                        .environment
                        .insert("DIFFERENT".into(), "true".into());
                }
                _ => (),
            }
            let mut attached = resolved.clone();
            if case == "actor-attach" {
                attached.native_continuity =
                    chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
            }
            if case == "wrong-id" {
                attached.id = TerminalId::new();
            }
            let mut snapshot = retained_history_snapshot(&resolved, 1);
            match case {
                "busy" => snapshot.busy = true,
                "waiting" => snapshot.waiting = true,
                "error" => snapshot.error = Some("Fixture history error".into()),
                "wrong-thread" => snapshot.session_id = Some(uuid::Uuid::new_v4().to_string()),
                "requests" => snapshot.requests.push(chat::Request {
                    kind: Default::default(),
                    id: "unsettled".into(),
                    title: "Fixture pending approval".into(),
                    description: String::new(),
                    fields: Vec::new(),
                    actions: Vec::new(),
                }),
                "live" => {
                    snapshot.exited = false;
                    snapshot.read_only_reason = None;
                }
                _ => (),
            }
            let (stream, peer) = chat::client::saved_history_fixture::Peer::new(
                f.source.clone(),
                resolved,
                attached,
                snapshot,
                false,
            );
            let result = ClientHandle::connect_saved_history_on(stream, f.source.clone(), |_| {});
            assert_eq!(result.is_ok(), case == "same-proof", "{case}");
            assert!(
                peer.requests
                    .lock()
                    .unwrap()
                    .starts_with(&["Connect", "Resolve"]),
                "same proof still must Resolve"
            );
        }
    });
}

#[test]
fn retained_history_atomic_refusal_preserves_both_copies_and_latest_retry() {
    chat::attachment_assets::with_test_image(|image| {
        let mut f = retained_history_fixture(&image, false);
        let mut second = f.app.card_state(f.card).unwrap().clone();
        second.id = CardId::new();
        let second_id = second.id;
        let second = crate::persistence::RetainedChatCopy {
            workspace: f.app.durable.id,
            source_card: f.original,
            source: f.source.clone(),
            copy: second,
            draft: "Other retained copy must remain pending".into(),
            attachments: vec![image.clone()],
            scope: Some(f.source.launch.recipe.working_directory.clone()),
            channel: None,
        };
        let writer = f.app.session.as_ref().unwrap();
        writer
            .preserve_chat_copy(second.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let store = writer.operation_store();
        let operation = begin_retained_history(&mut f.app, f.card);
        let (handle, witness, _peer) = framed_retained_reader(&f.app, &f.source, operation, false);
        // Refuse after descriptor CAS, inside the complete snapshot transaction.
        let duplicate = f.app.card_state(f.card).unwrap().clone();
        Arc::make_mut(&mut f.app.durable).cards.push(duplicate);
        let checkpoint = retained_open_reply(&mut f.app, operation, handle, witness);
        let receipt = account_commit_reply(checkpoint);
        assert!(matches!(
            &receipt,
            Message::ChatUi(Action::SavedHistoryCommitted(_, Err(_)))
        ));
        Arc::make_mut(&mut f.app.durable).cards.pop();
        drop(f.app.update(receipt));
        let saved = store
            .load_workspace(f.app.durable.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.load_card(f.card, None).unwrap().unwrap().kind,
            CardKind::Chat(f.source.clone())
        );
        assert_eq!(saved.pending_chat_admissions[&f.card], f.source);
        assert_eq!(saved.pending_chat_admissions[&second_id], f.source);
        assert_eq!(saved.chat_draft_attachments[&f.card], vec![image.clone()]);
        assert!(f.app.chat_awaiting_admission(f.card));
        let target = chat_runtime(&f.app, f.card).target;
        f.app
            .chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text(
                "New retry text after refusal",
            ));
        let retry = begin_retained_history(&mut f.app, f.card);
        let (handle, witness, _retry_peer) =
            framed_retained_reader(&f.app, &f.source, retry, false);
        let checkpoint = retained_open_reply(&mut f.app, retry, handle, witness);
        let receipt = account_commit_reply(checkpoint);
        assert!(matches!(
            &receipt,
            Message::ChatUi(Action::SavedHistoryCommitted(_, Ok(())))
        ));
        drop(f.app.update(receipt));
        f.app
            .session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(10))
            .unwrap();
        let saved = store
            .load_workspace(f.app.durable.id, None)
            .unwrap()
            .unwrap();
        assert!(!saved.pending_chat_admissions.contains_key(&f.card));
        assert_eq!(saved.pending_chat_admissions[&second_id], f.source);
        assert_eq!(saved.chat_drafts[&second_id], second.draft);
        assert_eq!(saved.chat_draft_attachments[&second_id], second.attachments);
        assert_eq!(saved.chat_drafts[&f.card], "New retry text after refusal");
        assert!(
            store.preserve_chat_copy(&second).is_ok(),
            "unrelated copy receipt remains valid"
        );
    });
}

#[test]
fn retained_history_close_while_ack_pending_persists_only_closed_reader() {
    chat::attachment_assets::with_test_image(|image| {
        let mut f = retained_history_fixture(&image, false);
        let operation = begin_retained_history(&mut f.app, f.card);
        let (handle, witness, _peer) = framed_retained_reader(&f.app, &f.source, operation, false);
        let reader = witness.reader().clone();
        let checkpoint = retained_open_reply(&mut f.app, operation, handle, witness);
        let receipt = account_commit_reply(checkpoint);
        drop(f.app.begin_chat_close(f.card));
        f.app.persist();
        drop(f.app.update(receipt));
        f.app
            .session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(10))
            .unwrap();
        let saved = f
            .app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_workspace(f.app.durable.id, None)
            .unwrap()
            .unwrap();
        let card = saved.cards.iter().find(|card| card.id == f.card).unwrap();
        assert!(!card.existence.is_open());
        assert_eq!(card.kind, CardKind::Chat(reader));
        assert!(!saved.pending_chat_admissions.contains_key(&f.card));
        assert!(!f.app.cards.contains_key(&f.card));
        assert!(f.app.chat_ui.attempt(f.card).is_none());
    });
}

#[test]
fn cold_image_only_retained_copy_can_open_history_without_a_text_draft() {
    chat::attachment_assets::with_test_image(|image| {
        let mut f = retained_history_fixture_text(&image, true, "");
        assert!(chat_runtime(&f.app, f.card).unsent_text().is_empty());
        let operation = begin_retained_history(&mut f.app, f.card);
        let (handle, witness, _peer) = framed_retained_reader(&f.app, &f.source, operation, false);
        let checkpoint = retained_open_reply(&mut f.app, operation, handle, witness);
        let receipt = account_commit_reply(checkpoint);
        assert!(matches!(
            &receipt,
            Message::ChatUi(Action::SavedHistoryCommitted(_, Ok(())))
        ));
        drop(f.app.update(receipt));
        assert_eq!(chat_runtime(&f.app, f.card).attachments, vec![image]);
        assert!(!f.app.chat_awaiting_admission(f.card));
        assert!(f.app.chat_ui.attempt(f.card).is_none());
    });
}

#[test]
fn generic_cached_pending_pane_cannot_spend_a_retained_copy_admission() {
    chat::attachment_assets::with_test_image(|image| {
        let mut f = retained_history_fixture(&image, true);
        let mut generic = f.app.card_state(f.card).unwrap().clone();
        generic.id = CardId::new();
        let card = generic.id;
        let durable = Arc::make_mut(&mut f.app.durable);
        durable.cards.push(generic);
        durable.local_only_cards.insert(card);
        durable
            .pending_chat_admissions
            .insert(card, f.source.clone());
        f.app.persist();
        let writer = f.app.session.as_ref().unwrap();
        writer.flush(Duration::from_secs(10)).unwrap();
        let store = writer.operation_store();
        let operation = f.app.chat_ui.next_target(card);
        let (_handle, witness, _peer) = framed_retained_reader(&f.app, &f.source, operation, false);
        let mut snapshot = store
            .load_workspace(f.app.durable.id, None)
            .unwrap()
            .unwrap();
        let generations = store.stored_generations(snapshot.id).unwrap();
        snapshot
            .cards
            .iter_mut()
            .find(|saved| saved.id == card)
            .unwrap()
            .kind = CardKind::Chat(witness.reader().clone());
        snapshot.pending_chat_admissions.remove(&card);
        assert!(
            store
                .save_retained_chat_history(
                    &snapshot,
                    crate::persistence::SnapshotBase {
                        workspace: generations.workspace,
                        cards: &generations.cards,
                    },
                    card,
                    &witness
                )
                .is_err()
        );
        let saved = store.load_workspace(snapshot.id, None).unwrap().unwrap();
        assert_eq!(saved.pending_chat_admissions[&card], f.source);
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(f.source.clone())
        );
        assert_eq!(saved.pending_chat_admissions[&f.card], f.source);
    });
}

#[test]
fn admitted_retained_history_detaches_later_actor_frames_and_restores_with_strict_policy() {
    use crate::terminal_ipc::{ServerMessage, ServerResponse};
    chat::attachment_assets::with_test_image(|image| {
        for case in ["snapshot", "delta", "activity", "waiting"] {
            let mut f = retained_history_fixture(&image, true);
            let operation = begin_retained_history(&mut f.app, f.card);
            let (handle, witness, peer) =
                framed_retained_reader(&f.app, &f.source, operation, false);
            let reader = witness.reader().clone();
            let checkpoint = retained_open_reply(&mut f.app, operation, handle.clone(), witness);
            let receipt = account_commit_reply(checkpoint);
            drop(f.app.update(receipt));
            let before = handle.snapshot();
            match case {
                "snapshot" => {
                    let mut actor = (*before).clone();
                    actor.generation += 1;
                    actor.native_continuity =
                        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
                    actor.exited = false;
                    actor.read_only_reason = None;
                    peer.publish(actor);
                }
                "delta" => {
                    let mut metadata = (*before).clone();
                    metadata.generation += 1;
                    metadata.items.clear();
                    metadata.native_continuity =
                        chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
                    peer.message(ServerMessage::Response {
                        response: ServerResponse::ChatUpdate {
                            chat_id: reader.id,
                            update: chat::presentation::Presentation {
                                base_generation: before.generation,
                                file_changes_unchanged: false,
                                metadata,
                                order: before.items.iter().map(|item| item.id.clone()).collect(),
                                changes: Vec::new(),
                                children: Vec::new(),
                            },
                        },
                    });
                }
                "activity" | "waiting" => peer.message(ServerMessage::ChatActivity {
                    chat_id: reader.id,
                    generation: before.generation + 1,
                    state: if case == "activity" {
                        crate::agent::ActivityState::Working
                    } else {
                        crate::agent::ActivityState::Waiting
                    },
                }),
                _ => unreachable!(),
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while handle.is_connected() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!handle.is_connected(), "{case}");
            assert_eq!(
                handle.session(),
                reader,
                "unexpected actor capability must never publish: {case}"
            );
            assert_eq!(handle.snapshot(), before, "{case}");
            while let Ok((target, wake)) = f.app.chat_ui.receiver.try_recv() {
                drop(f.app.handle_chat_ui(Action::Wake(target, wake)));
            }
            assert!(f.app.chat_ui.attempt(f.card).is_none());
            let target = chat_runtime(&f.app, f.card).target;
            drop(f.app.handle_chat_ui(Action::Reconnect(target)));
            assert!(
                matches!(
                    f.app.chat_ui.phases.get(&f.card),
                    Some(Phase::OpeningHistory(_))
                ),
                "Reconnect must keep exact R read-only: {case}"
            );
            assert!(f.app.chat_ui.attempt(f.card).is_none());
            assert!(
                peer.requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| ["Connect", "Resolve", "Attach", "Read"].contains(request))
            );
            // Cold restore must carry the same narrow policy, independent of
            // the now-detached Runtime, without classifying Resume successor B.
            f.app
                .session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(10))
                .unwrap();
            let path = f.path.clone();
            drop(f.app.session.take());
            let (writer, restored) =
                crate::session::SessionRuntime::open(path, "retained-old-boot".into()).unwrap();
            let restored = restored.unwrap();
            assert!(writer.is_retained_history_reader(f.card, &reader));
            let mut successor = reader.clone();
            successor.id = TerminalId::new();
            assert!(!writer.is_retained_history_reader(f.card, &successor));
            let mut cold = crate::app::tests::context_inspector_test_app(false);
            cold.cards.clear();
            cold.chat_ui = State::with_draft_attachments(
                restored.chat_drafts.clone(),
                restored.chat_draft_attachments.clone(),
            );
            cold.durable = Arc::new(restored);
            cold.workspace = Cagents::rebuild_workspace(&cold.durable);
            cold.session = Some(writer);
            drop(cold.initialize_runtimes());
            assert!(matches!(
                cold.chat_ui.phases.get(&f.card),
                Some(Phase::OpeningHistory(_))
            ));
            assert!(cold.chat_ui.attempt(f.card).is_none());
            assert!(!cold.durable.pending_chat_admissions.contains_key(&f.card));
            assert_eq!(chat_runtime(&cold, f.card).attachments, vec![image.clone()]);
        }
    });
}

#[test]
fn explicit_same_allocation_resume_spends_only_reader_policy_with_content_ack() {
    chat::attachment_assets::with_test_image(|image| {
        for refuse in [false, true] {
            let mut f = retained_history_fixture(&image, true);
            let opening = begin_retained_history(&mut f.app, f.card);
            let (handle, witness, _peer) =
                framed_retained_reader(&f.app, &f.source, opening, false);
            let reader = witness.reader().clone();
            let checkpoint = retained_open_reply(&mut f.app, opening, handle, witness);
            let receipt = account_commit_reply(checkpoint);
            drop(f.app.update(receipt));
            let target = chat_runtime(&f.app, f.card).target;
            f.app
                .chat_runtime_mut(target)
                .unwrap()
                .replace_draft(text_editor::Content::with_text(
                    "Latest draft before explicit same-A Resume",
                ));
            drop(f.app.handle_chat_ui(Action::ResumeSaved(target)));
            let operation = f.app.chat_ui.phases[&f.card].target();
            if refuse {
                let duplicate = f.app.card_state(f.card).unwrap().clone();
                Arc::make_mut(&mut f.app.durable).cards.push(duplicate);
            }
            let checkpoint = f
                .app
                .handle_chat_ui(Action::ResumePrepared(operation, Ok(reader.clone())));
            assert!(
                f.app.chat_ui.attempt(f.card).is_none(),
                "same-A Resume must await associated-content ACK"
            );
            let receipt = account_commit_reply(checkpoint);
            let store = f.app.session.as_ref().unwrap().operation_store();
            let saved = store
                .load_workspace(f.app.durable.id, None)
                .unwrap()
                .unwrap();
            assert_eq!(
                store
                    .retained_history_readers(&saved)
                    .unwrap()
                    .contains_key(&f.card),
                refuse
            );
            if refuse {
                assert!(matches!(
                    &receipt,
                    Message::ChatUi(Action::ResumeCommitted(_, Err(_)))
                ));
                Arc::make_mut(&mut f.app.durable).cards.pop();
                drop(f.app.update(receipt));
                assert!(f.app.chat_ui.attempt(f.card).is_none());
                assert!(f.app.is_retained_history_reader(f.card, &reader));
                continue;
            }
            assert!(matches!(
                &receipt,
                Message::ChatUi(Action::ResumeCommitted(_, Ok(())))
            ));
            assert_eq!(
                saved.chat_drafts[&f.card],
                "Latest draft before explicit same-A Resume"
            );
            assert_eq!(saved.chat_draft_attachments[&f.card], vec![image.clone()]);
            // A fresh cold window can ordinarily attach a legitimate actor
            // after a lost successful Start; it never recreates Start intent.
            assert!(
                !store
                    .retained_history_readers(&saved)
                    .unwrap()
                    .contains_key(&f.card)
            );
            drop(f.app.update(receipt));
            assert_eq!(f.app.chat_ui.attempt(f.card).unwrap().session, reader);
            assert_eq!(
                f.app.chat_ui.attempt(f.card).unwrap().intent,
                chat::Intent::Start
            );
            assert!(
                !f.app.is_retained_history_reader(f.card, &reader),
                "warm cached policy must be spent too"
            );
            let started = f.app.chat_ui.attempt(f.card).unwrap().target;
            answer_start(
                &mut f.app,
                started,
                Err(chat::StartFailure::retry("Fixture lost Start reply")),
            );
            let runtime_target = chat_runtime(&f.app, f.card).target;
            drop(f.app.handle_chat_ui(Action::Reconnect(runtime_target)));
            assert_eq!(
                f.app.chat_ui.attempt(f.card).unwrap().intent,
                chat::Intent::RetryStart
            );
            assert_eq!(f.app.chat_ui.attempt(f.card).unwrap().session, reader);
            f.app
                .session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(10))
                .unwrap();
            drop(f.app.session.take());
            let (writer, restored) =
                crate::session::SessionRuntime::open(&f.path, "retained-old-boot".into()).unwrap();
            assert!(!writer.is_retained_history_reader(f.card, &reader));
            let restored = restored.unwrap();
            let mut cold = crate::app::tests::context_inspector_test_app(false);
            cold.cards.clear();
            cold.chat_ui = State::with_draft_attachments(
                restored.chat_drafts.clone(),
                restored.chat_draft_attachments.clone(),
            );
            cold.durable = Arc::new(restored);
            cold.workspace = Cagents::rebuild_workspace(&cold.durable);
            cold.session = Some(writer);
            drop(cold.initialize_runtimes());
            assert_eq!(
                cold.chat_ui.attempt(f.card).unwrap().intent,
                chat::Intent::Restore,
                "lost same-A Start may attach an actor, but cold restore cannot recreate Start intent"
            );
        }
    });
}

#[test]
fn retained_history_full_checkpoint_projects_prior_rebinding_and_unrelated_copy() {
    chat::attachment_assets::with_test_image(|image| {
        let mut f = retained_history_fixture(&image, false);
        let mut other = f.app.card_state(f.card).unwrap().clone();
        other.id = CardId::new();
        let other_id = other.id;
        let mut other_source = f.source.clone();
        other_source.id = TerminalId::new();
        other.kind = CardKind::Chat(other_source.clone());
        Arc::make_mut(&mut f.app.durable).cards.push(other);
        f.app.chat_ui.drafts.insert(
            other_id,
            "Other pane draft while its receipt is pending".into(),
        );
        f.app
            .chat_ui
            .draft_attachments
            .insert(other_id, vec![image.clone()]);
        f.app.persist();
        let writer = f.app.session.as_ref().unwrap();
        writer.flush(Duration::from_secs(10)).unwrap();
        let other_replacement = retained_reader_session(&other_source);
        writer
            .rebind_chat_upgrade(other_id, other_source, other_replacement.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        // Neither the existing rebind receipt nor this copy has reached the UI.
        let mut copy_card = f.app.card_state(f.card).unwrap().clone();
        copy_card.id = CardId::new();
        let copy_id = copy_card.id;
        let copy = crate::persistence::RetainedChatCopy {
            workspace: f.app.durable.id,
            source_card: f.original,
            source: f.source.clone(),
            copy: copy_card,
            draft: "Unrelated retained draft remains reachable".into(),
            attachments: vec![image.clone()],
            scope: Some(f.source.launch.recipe.working_directory.clone()),
            channel: None,
        };
        writer
            .preserve_chat_copy(copy.clone())
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let store = writer.operation_store();
        let operation = begin_retained_history(&mut f.app, f.card);
        let (handle, witness, _peer) = framed_retained_reader(&f.app, &f.source, operation, false);
        let checkpoint = retained_open_reply(&mut f.app, operation, handle, witness.clone());
        let receipt = account_commit_reply(checkpoint);
        assert!(matches!(
            &receipt,
            Message::ChatUi(Action::SavedHistoryCommitted(_, Ok(())))
        ));
        drop(f.app.update(receipt));
        f.app
            .session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(10))
            .unwrap();
        let saved = store
            .load_workspace(f.app.durable.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            saved
                .cards
                .iter()
                .find(|card| card.id == other_id)
                .unwrap()
                .kind,
            CardKind::Chat(other_replacement)
        );
        assert_eq!(
            saved.chat_drafts[&other_id],
            "Other pane draft while its receipt is pending"
        );
        assert_eq!(saved.chat_draft_attachments[&other_id], vec![image.clone()]);
        assert!(
            saved
                .cards
                .iter()
                .any(|card| card.id == copy_id && card.existence.is_open())
        );
        assert_eq!(saved.chat_drafts[&copy_id], copy.draft);
        assert_eq!(saved.chat_draft_attachments[&copy_id], vec![image]);
        assert_eq!(saved.pending_chat_admissions[&copy_id], f.source);
        assert!(!saved.pending_chat_admissions.contains_key(&f.card));
        // A separate explicitly selected copy can use the same authenticated
        // allocation reader, but its own receipt and current snapshot must land.
        // End the UI writer first; this last section owns only the store contract.
        drop(f.app.session.take());
        let store =
            crate::persistence::StateStore::open(&f.path, "retained-old-boot".into()).unwrap();
        let mut second = store
            .load_workspace(f.app.durable.id, None)
            .unwrap()
            .unwrap();
        second
            .cards
            .iter_mut()
            .find(|saved| saved.id == copy_id)
            .unwrap()
            .kind = CardKind::Chat(witness.reader().clone());
        second.pending_chat_admissions.remove(&copy_id);
        let base = store.stored_generations(second.id).unwrap();
        store
            .save_retained_chat_history(
                &second,
                crate::persistence::SnapshotBase {
                    workspace: base.workspace,
                    cards: &base.cards,
                },
                copy_id,
                &witness,
            )
            .unwrap();
        assert!(
            store.preserve_chat_copy(&copy).is_err(),
            "spent copy cannot be recreated by its old exact receipt"
        );
    });
}
