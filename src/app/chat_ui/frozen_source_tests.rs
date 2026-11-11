//! A resume may retire the source while its daemon socket remains connected.
//! Keep one already-presented window, without retaining the old client body,
//! raw logical-message owners or read subscriptions. Hidden rendering is warm
//! only within the ordinary cache budget; cooling keeps the frozen source.
use super::*;

const THREAD: &str = "6f1f6f1e-4a4b-4c4d-8e8f-909192939495";
const DRAFT: &str = "Keep this unsent draft.";
const REFUSAL: &str = "The exact saved conversation could not be resumed";

fn runtime(app: &Cagents, card: CardId) -> &Runtime {
    match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime,
        _ => panic!("the conversation must keep its runtime"),
    }
}

fn source(
    partial: bool,
) -> (
    Cagents,
    CardId,
    Target,
    Arc<ClientHandle>,
    String,
    chat::TextCopySource,
) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1280.0);
    let card = tests::live_chat_for_provider(&mut app, Provider::Claude, Some(THREAD));
    let target = runtime(&app, card).target;
    let mut session = runtime(&app, card).session.clone();
    session.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    let body = format!(
        "Before the fence.\n\n```rust\n{}```\nAfter the fence.\n",
        "let kept = 1;\n".repeat(3 * chat::TRANSCRIPT_PART_BYTES / 14 + 1)
    );
    let copied = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: chat::TextCopyScope::Conversation,
        token: 7,
        revision: 1,
        bytes: body.len() as u64,
    };
    let first = usize::from(partial);
    let items = logical_prose::parts("answer", &body, first..usize::MAX);
    let mut snapshot = Snapshot {
        generation: 19,
        ready: true,
        exited: true,
        session_id: Some(THREAD.into()),
        native_continuity: session.native_continuity.clone(),
        sampled_at: Some(10_000),
        error: Some("The source provider stopped".into()),
        has_earlier: partial,
        ..Snapshot::default()
    };
    snapshot.text_copy_sources.conversation = vec![Some(copied.clone()); items.len()];
    snapshot.items = items;
    let handle = Arc::new(ClientHandle::fixture(session.clone(), snapshot));
    let mut state = Runtime::new(
        target,
        session.clone(),
        Connection::Live(handle.clone()),
        true,
        app.settings.editor_syntax_theme,
    );
    state.refresh();
    preparation::drain_fixture(&mut state);
    state.replace_draft(text_editor::Content::with_text(DRAFT));
    assert!(state.resumes_in_place());
    assert!(state.logical_prose.raw().is_some());
    assert_eq!(state.logical_prose.request().is_some(), partial);
    assert_eq!(state.snapshot.items.len(), 1);
    assert!(state.source_clock.is_some());
    let shown = match &state.snapshot.items[0].content {
        ItemContent::Text { text, .. } => text.clone(),
        _ => panic!("the last readable window is logical prose"),
    };
    assert_eq!(shown, body[first * chat::TRANSCRIPT_PART_BYTES..]);
    app.replace_chat_conversation(card, session);
    app.cards.insert(card, CardRuntime::Chat(Box::new(state)));
    (app, card, target, handle, shown, copied)
}

fn assert_no_source_read_owners(runtime: &Runtime, handle: &ClientHandle) {
    assert!(
        handle.is_connected(),
        "the retired actor's socket stays open"
    );
    assert!(
        !handle.is_visible_for_test(),
        "old-source reads are inactive"
    );
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert!(handle.resident_snapshot_with_clock().is_none());
    assert!(runtime.logical_prose.raw().is_none());
    assert!(runtime.logical_prose.request().is_none());
    assert!(logical_prose::subscriptions(runtime).is_none());
}

#[test]
fn in_place_resume_keeps_one_frozen_projection_without_old_source_owners() {
    let _context = text_copy::isolated_context();
    for partial in [false, true] {
        for recovery in [chat::Recovery::Retry, chat::Recovery::Blocked] {
            let (mut app, card, target, handle, shown, _) = source(partial);
            let session = runtime(&app, card).session.clone();
            let frozen = runtime(&app, card).snapshot.clone();
            let clock = format!("{:?}", runtime(&app, card).source_clock);
            drop(app.start_chat(card, session.clone(), chat::Intent::Resume, None, false));
            let attempt = tests::chat_runtime_attempt(&app, card);
            assert!(runtime(&app, card).resume_source_pinned);
            assert_no_source_read_owners(runtime(&app, card), &handle);

            // Metadata from the old connected handle is not a new source for
            // the frozen window, including its clock and provider diagnosis.
            handle.deliver_snapshot_for_test(Snapshot {
                generation: 20,
                sampled_at: Some(9_000_000),
                session_id: Some(THREAD.into()),
                ready: true,
                exited: true,
                ..Snapshot::default()
            });
            for phase in 0..2 {
                if phase == 1 {
                    tests::answer_start(
                        &mut app,
                        attempt,
                        Err(chat::StartFailure {
                            message: REFUSAL.into(),
                            recovery,
                        }),
                    );
                }
                let state = app.chat_runtime_mut(target).unwrap();
                state.history = vec!["inactive recall body".repeat(4096)];
                state.history_images = vec![vec![Attachment::file("inactive-history-image.png")]];
                state.history_cursor = None;
                // Exercise repeated hiding even when presentation caches are
                // already empty; logical-only owners must be released too.
                for _ in 0..3 {
                    state.set_visible(false).unwrap();
                    assert!(state.warm_stamp().is_some());
                    assert_no_source_read_owners(state, &handle);
                    state.cool();
                    state.refresh();
                    assert!(state.warm_stamp().is_none());
                    assert!(state.cache.is_empty());
                    assert!(state.child_cache.is_empty());
                    assert!(state.selection_document.borrow().is_none());
                    assert!(state.failure_ui.incidents.is_empty());
                    assert!(state.history.is_empty());
                    assert!(state.history_images.is_empty());
                    assert_eq!(state.snapshot.items, frozen.items);
                    assert_eq!(state.snapshot.error, frozen.error);
                    assert_eq!(format!("{:?}", state.source_clock), clock);
                    assert_eq!(state.composer.text(), DRAFT);
                    assert_no_source_read_owners(state, &handle);
                    state.set_visible(true).unwrap();
                    preparation::drain_fixture(state);
                    assert_eq!(state.snapshot.items.len(), 1);
                    assert!(Arc::ptr_eq(&state.snapshot.items[0], &frozen.items[0]));
                    assert!(
                        matches!(&state.snapshot.items[0].content, ItemContent::Text { text, .. } if text == &shown)
                    );
                    assert!(state.cache.contains_key("answer"));
                    assert!(!state.cache["answer"].placeholder);
                    assert_eq!(format!("{:?}", state.source_clock), clock);
                    assert_no_source_read_owners(state, &handle);
                }
                if phase == 1 {
                    assert_eq!(state.error.as_deref(), Some(REFUSAL));
                }
                assert_eq!(state.session, session);
            }
        }
    }
}

#[test]
fn a_blocked_restore_uses_normal_interest_and_eviction_without_a_source_pin() {
    let _context = text_copy::isolated_context();
    let (mut app, card, target, handle, _, _) = source(true);
    let session = runtime(&app, card).session.clone();
    drop(app.start_chat(card, session, chat::Intent::Restore, None, false));
    let attempt = tests::chat_runtime_attempt(&app, card);
    tests::answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure {
            message: REFUSAL.into(),
            recovery: chat::Recovery::Blocked,
        }),
    );
    let state = app.chat_runtime_mut(target).unwrap();
    assert!(state.start_retry_blocked);
    assert!(!state.resume_source_pinned);
    assert!(!state.retains_source_body());
    assert!(handle.resident_snapshot_with_clock().is_some());
    assert!(state.logical_prose.raw().is_some());
    state.set_visible(false).unwrap();
    assert!(state.warm_stamp().is_some());
    assert!(state.snapshot.transcript_bytes() > 0);
    assert!(handle.resident_snapshot_with_clock().is_some());
    hide_newer_chats_and_trim(&mut app);
    let state = runtime(&app, card);
    assert!(state.warm_stamp().is_none());
    assert_eq!(state.snapshot.transcript_bytes(), 0);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert!(state.cache.is_empty());
    assert!(state.logical_prose.raw().is_none());
}

#[test]
fn a_successor_attachment_clears_the_frozen_source_before_projection() {
    let _context = text_copy::isolated_context();
    let (mut app, card, target, old, _, copied) = source(true);
    let session = runtime(&app, card).session.clone();
    drop(app.start_chat(card, session.clone(), chat::Intent::Resume, None, false));
    let attempt = tests::chat_runtime_attempt(&app, card);
    assert_no_source_read_owners(runtime(&app, card), &old);
    let body = "A new authoritative answer.\n".repeat(chat::TRANSCRIPT_PART_BYTES / 20 + 1);
    let items = logical_prose::parts("answer", &body, 0..usize::MAX);
    let mut snapshot = Snapshot {
        generation: 1,
        ready: true,
        session_id: Some(THREAD.into()),
        native_continuity: session.native_continuity.clone(),
        sampled_at: Some(20_000),
        ..Snapshot::default()
    };
    let successor_source = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        bytes: body.len() as u64,
        ..copied.clone()
    };
    snapshot.text_copy_sources.conversation = vec![Some(successor_source); items.len()];
    snapshot.items = items;
    let successor = Arc::new(ClientHandle::fixture(session, snapshot));
    tests::answer_start(&mut app, attempt, Ok(successor.clone()));
    let state = runtime(&app, card);
    assert!(!state.resume_source_pinned);
    assert!(!state.retains_source_body());
    assert!(state.logical_prose.raw().is_some());
    assert!(state.logical_prose.request().is_none());
    assert!(successor.resident_snapshot_with_clock().is_some());
    assert!(
        matches!(&state.snapshot.items[0].content, ItemContent::Text { text, .. } if text == &body)
    );
    assert_eq!(state.composer.text(), DRAFT);
    drop(app.handle_chat_ui(Action::LogicalProseRead(
        target,
        copied,
        Ok("late source read".into()),
    )));
    assert!(
        matches!(&runtime(&app, card).snapshot.items[0].content, ItemContent::Text { text, .. } if text == &body)
    );
}

#[test]
fn a_frozen_window_preserves_active_recall_until_the_draft_is_restored() {
    let _context = text_copy::isolated_context();
    let (mut app, card, target, _, _, _) = source(false);
    let state = app.chat_runtime_mut(target).unwrap();
    state.remember_with_attachments(
        "Previously accepted prompt".into(),
        vec![Attachment::file("recalled-note.txt")],
    );
    state.recall(true);
    assert_eq!(state.composer.text(), "Previously accepted prompt");
    let session = state.session.clone();
    drop(app.start_chat(card, session, chat::Intent::Resume, None, false));
    let state = app.chat_runtime_mut(target).unwrap();
    state.set_visible(false).unwrap();
    assert_eq!(state.history, ["Previously accepted prompt"]);
    assert_eq!(state.history_images.len(), 1);
    state.set_visible(true).unwrap();
    state.recall(false);
    assert_eq!(state.composer.text(), DRAFT);
    assert!(state.attachments.is_empty());
    state.set_visible(false).unwrap();
    assert!(state.warm_stamp().is_some());
    state.cool();
    assert!(state.history.is_empty());
    assert!(state.history_images.is_empty());
}

/// Apply the real app budget after each newer hide, as channel switches do.
/// These are small ordinary chats, so count, rather than bytes, cools the
/// older target. They do not share its source or its frozen-body exception.
fn hide_newer_chats_and_trim(app: &mut Cagents) {
    app.trim_warm_chats();
    let mut newer = Vec::new();
    for index in 0..=warm::CHATS {
        let session = fresh_chat_session(
            Provider::Claude,
            TerminalId::new(),
            "/project".into(),
            &Account::system(Provider::Claude),
            false,
        )
        .unwrap();
        let snapshot = Snapshot {
            generation: 1,
            ready: true,
            items: vec![Arc::new(chat::Item {
                id: format!("newer-{index}"),
                role: chat::Role::Assistant,
                content: ItemContent::Text {
                    text: "A newer ordinary warm conversation.".into(),
                    thinking: false,
                    phase: chat::Phase::Final,
                },
                at: None,
                started_at: None,
            })],
            ..Snapshot::default()
        };
        let handle = Arc::new(ClientHandle::fixture(session.clone(), snapshot));
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let mut state = Runtime::new(
            target,
            session,
            Connection::Live(handle),
            true,
            app.settings.editor_syntax_theme,
        );
        preparation::drain_fixture(&mut state);
        state.set_visible(false).unwrap();
        assert!(state.warm_stamp().is_some());
        assert!(state.warm_bytes() < warm::CHAT_BYTES);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(state)));
        newer.push(target.card);
        app.trim_warm_chats();
    }
    assert!(runtime(app, newer[0]).warm_stamp().is_none());
    assert!(
        newer[1..]
            .iter()
            .all(|card| runtime(app, *card).warm_stamp().is_some()),
        "the eight most recently hidden chats stay warm"
    );
}

fn assert_frozen_window(
    state: &Runtime,
    frozen: &Arc<Snapshot>,
    clock: &str,
    handle: &ClientHandle,
) {
    assert!(state.resume_source_pinned);
    assert!(Arc::ptr_eq(&state.snapshot, frozen));
    assert_eq!(state.snapshot.items, frozen.items);
    assert_eq!(state.snapshot.error, frozen.error);
    assert_eq!(format!("{:?}", state.source_clock), clock);
    assert_eq!(state.composer.text(), DRAFT);
    assert_no_source_read_owners(state, handle);
}

#[test]
fn the_warm_budget_cools_frozen_resume_rendering_without_losing_the_source() {
    let _context = text_copy::isolated_context();
    for partial in [false, true] {
        for recovery in [chat::Recovery::Retry, chat::Recovery::Blocked] {
            let (mut app, card, target, old, shown, _) = source(partial);
            let frozen = runtime(&app, card).snapshot.clone();
            let clock = format!("{:?}", runtime(&app, card).source_clock);
            let session = runtime(&app, card).session.clone();
            drop(app.start_chat(card, session, chat::Intent::Resume, None, false));
            let attempt = tests::chat_runtime_attempt(&app, card);
            let state = app.chat_runtime_mut(target).unwrap();
            state.history = vec!["inactive recall".repeat(4096)];
            state.history_images = vec![vec![Attachment::file("old-history-image.png")]];
            app.window_width = 640.0;
            drop(app.sync_context_overlay_state());
            assert!(!app.workspace_surfaces_visible());
            let state = app.chat_runtime_mut(target).unwrap();
            let stamp = state.warm_stamp().expect("frozen rendering may stay warm");
            assert!(!state.cache.is_empty());
            assert_frozen_window(state, &frozen, &clock, &old);

            // A retired actor's connected socket can still publish metadata.
            // It neither invalidates this frozen warm rendering nor grants
            // the old client renewed body interest.
            old.deliver_snapshot_for_test(Snapshot {
                generation: 90_000,
                sampled_at: Some(90_000_000),
                ready: true,
                exited: true,
                session_id: Some(THREAD.into()),
                items: vec![Arc::new(chat::Item {
                    id: "answer".into(),
                    role: chat::Role::Assistant,
                    content: ItemContent::Text {
                        text: "A late old-source body must not be retained.".into(),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                    at: None,
                    started_at: None,
                })],
                ..Snapshot::default()
            });
            state.refresh();
            assert_eq!(state.warm_stamp(), Some(stamp));
            assert_frozen_window(state, &frozen, &clock, &old);

            hide_newer_chats_and_trim(&mut app);
            let state = runtime(&app, card);
            assert!(
                state.warm_stamp().is_none(),
                "the old target was LRU-cooled"
            );
            assert!(state.cache.is_empty());
            assert!(state.child_cache.is_empty());
            assert!(state.selection_document.borrow().is_none());
            assert!(state.failure_ui.incidents.is_empty());
            assert!(state.history.is_empty());
            assert!(state.history_images.is_empty());
            assert_frozen_window(state, &frozen, &clock, &old);

            tests::answer_start(
                &mut app,
                attempt,
                Err(chat::StartFailure {
                    message: REFUSAL.into(),
                    recovery,
                }),
            );
            for generation in [90_001, 90_002] {
                old.deliver_snapshot_for_test(Snapshot {
                    generation,
                    sampled_at: Some(100_000_000 + generation),
                    error: Some("Late metadata must not replace the readable source".into()),
                    ..Snapshot::default()
                });
                let state = app.chat_runtime_mut(target).unwrap();
                state.refresh();
                assert!(state.warm_stamp().is_none());
                assert!(state.cache.is_empty());
                assert_eq!(state.error.as_deref(), Some(REFUSAL));
                assert_eq!(
                    state.start_retry_blocked,
                    recovery == chat::Recovery::Blocked
                );
                assert_frozen_window(state, &frozen, &clock, &old);
            }
            app.window_width = 1600.0;
            drop(app.sync_context_overlay_state());
            assert!(app.workspace_surfaces_visible());
            let state = app.chat_runtime_mut(target).unwrap();
            assert!(state.visible.load(Ordering::Relaxed));
            preparation::drain_fixture(state);
            assert!(state.warm_stamp().is_none());
            assert!(state.cache.contains_key("answer"));
            assert!(!state.cache["answer"].placeholder);
            assert!(
                matches!(&state.snapshot.items[0].content, ItemContent::Text { text, .. } if text == &shown),
                "the exact full or part-joined window is shown without projecting it again"
            );
            assert_eq!(state.error.as_deref(), Some(REFUSAL));
            assert_frozen_window(state, &frozen, &clock, &old);
        }
    }
}

#[test]
fn a_hidden_successor_clears_the_old_warm_stamp_before_reusing_ids_and_generations() {
    let _context = text_copy::isolated_context();
    let (mut app, card, _, old, _, copied) = source(true);
    let session = runtime(&app, card).session.clone();
    let frozen = runtime(&app, card).snapshot.clone();
    drop(app.start_chat(card, session.clone(), chat::Intent::Resume, None, false));
    let attempt = tests::chat_runtime_attempt(&app, card);
    app.window_width = 640.0;
    drop(app.sync_context_overlay_state());
    assert!(!app.workspace_surfaces_visible());
    assert!(runtime(&app, card).warm_stamp().is_some());
    assert_no_source_read_owners(runtime(&app, card), &old);

    let body =
        "A successor owns this reused message ID.\n".repeat(chat::TRANSCRIPT_PART_BYTES / 24 + 1);
    let items = logical_prose::parts("answer", &body, 0..usize::MAX);
    let mut next = Snapshot {
        generation: 1,
        sampled_at: Some(20_000),
        ready: true,
        session_id: Some(THREAD.into()),
        native_continuity: session.native_continuity.clone(),
        items,
        ..Snapshot::default()
    };
    let next_source = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        bytes: body.len() as u64,
        ..copied.clone()
    };
    next.text_copy_sources.conversation = vec![Some(next_source); next.items.len()];
    let successor = Arc::new(ClientHandle::fixture(session, next.clone()));
    tests::answer_start(&mut app, attempt, Ok(successor.clone()));
    let state = runtime(&app, card);
    assert!(!state.visible.load(Ordering::Relaxed));
    assert!(!state.resume_source_pinned);
    assert!(
        state.warm_stamp().is_none(),
        "warmth belonged to the old attachment"
    );
    assert!(!state.retains_source_body());
    assert!(state.cache.is_empty());
    assert!(state.snapshot.items.is_empty());
    assert_eq!(state.snapshot.generation, 1);
    assert!(state.logical_prose.raw().is_none());
    let successor_target = state.target;

    // An ordinary hidden successor needs hydration on reveal. Its lower
    // generation and reused IDs must not be mistaken for the frozen source.
    app.window_width = 1600.0;
    drop(app.sync_context_overlay_state());
    assert!(app.workspace_surfaces_visible());
    successor.deliver_snapshot_for_test(next);
    let state = app.chat_runtime_mut(successor_target).unwrap();
    state.refresh();
    preparation::drain_fixture(state);
    assert_eq!(state.snapshot.generation, 1);
    assert_eq!(state.snapshot.items.len(), 1);
    assert!(!Arc::ptr_eq(&state.snapshot, &frozen));
    assert!(
        matches!(&state.snapshot.items[0].content, ItemContent::Text { text, .. } if text == &body)
    );
    assert_eq!(state.composer.text(), DRAFT);
    assert!(state.logical_prose.raw().is_some());
    assert!(state.logical_prose.request().is_none());
    assert_eq!(
        format!("{:?}", state.source_clock),
        format!("{:?}", successor.resident_snapshot_with_clock().unwrap().1)
    );
    assert_eq!(old.snapshot().transcript_bytes(), 0);
    drop(app.handle_chat_ui(Action::LogicalProseRead(
        successor_target,
        copied,
        Ok("late old-source prose must not enter the successor".into()),
    )));
    assert!(
        matches!(&runtime(&app, card).snapshot.items[0].content, ItemContent::Text { text, .. } if text == &body)
    );
}

#[test]
fn reconnect_before_the_disconnect_wake_releases_only_the_retired_raw_source() {
    let _context = text_copy::isolated_context();
    for (partial, recovery) in [
        (false, chat::Recovery::Retry),
        (true, chat::Recovery::Blocked),
    ] {
        let (mut app, card, target, old, shown, _) = source(partial);
        let frozen = runtime(&app, card).snapshot.clone();
        let clock = format!("{:?}", runtime(&app, card).source_clock);
        let session = runtime(&app, card).session.clone();
        let raw_part = {
            let raw = runtime(&app, card).logical_prose.raw().unwrap();
            assert!(raw.items.len() > 1, "this fixture joins several raw parts");
            let part = raw.items.last().unwrap();
            assert!(
                frozen.items.iter().all(|shown| !Arc::ptr_eq(shown, part)),
                "this raw-only item differs from the readable joined projection"
            );
            Arc::downgrade(part)
        };
        let retired = old.observe_snapshot_retirement_for_test();

        // The reader has exited, but its UI Wake has not run. Ordinary dead
        // sources retain their readable publication until explicitly released.
        old.disconnect_for_test();
        assert!(!old.is_connected());
        assert!(old.is_visible_for_test());
        assert!(old.resident_snapshot_with_clock().is_some());
        assert!(raw_part.upgrade().is_some());
        assert!(!runtime(&app, card).disconnected);
        assert!(!runtime(&app, card).resume_source_pinned);
        assert!(!app.chat_start_pending(card));

        // This is the stopped pane's actual Resume action, ahead of Wake.
        drop(app.handle_chat_ui(Action::Reconnect(target)));
        let attempt = app.chat_ui.attempt(card).expect("Resume must be in flight");
        assert!(matches!(attempt.intent, chat::Intent::Resume));
        assert_eq!(attempt.session, session);
        let attempt = attempt.target;
        let assert_source = |app: &Cagents| {
            let state = runtime(app, card);
            assert!(state.resume_source_pinned);
            assert!(Arc::ptr_eq(&state.snapshot, &frozen));
            assert_eq!(state.snapshot.items, frozen.items);
            assert_eq!(state.snapshot.error, frozen.error);
            assert_eq!(format!("{:?}", state.source_clock), clock);
            assert_eq!(state.composer.text(), DRAFT);
            assert!(!old.is_visible_for_test(), "old-source reads are inactive");
            assert!(old.resident_snapshot_with_clock().is_none());
            assert_eq!(old.snapshot().transcript_bytes(), 0);
            assert!(state.logical_prose.raw().is_none());
            assert!(state.logical_prose.request().is_none());
            assert!(logical_prose::subscriptions(state).is_none());
        };
        assert_source(&app);

        // Policy release is immediate; destruction follows the existing
        // asynchronous publication reaper. The signal observes this exact
        // publication, not process RSS or universal duplicated text bytes.
        retired
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the explicitly released dead-client publication must retire");
        assert!(
            raw_part.upgrade().is_none(),
            "the raw-only part is gone while the joined projection remains readable"
        );
        assert_source(&app);

        // A full late frame still obeys the body policy: false retention is
        // local state even though this dead wire cannot send visibility.
        old.deliver_snapshot_for_test(Snapshot {
            generation: 90_000,
            sampled_at: Some(90_000_000),
            ready: true,
            exited: true,
            session_id: Some(THREAD.into()),
            items: frozen.items.clone(),
            ..Snapshot::default()
        });
        assert_source(&app);
        drop(app.handle_chat_ui(Action::Wake(
            target,
            Wake::Disconnected("Delayed old-reader disconnect".into()),
        )));
        assert_eq!(tests::chat_runtime_attempt(&app, card), attempt);
        assert_source(&app);

        for failed in [false, true] {
            if failed {
                tests::answer_start(
                    &mut app,
                    attempt,
                    Err(chat::StartFailure {
                        message: REFUSAL.into(),
                        recovery,
                    }),
                );
            }
            let state = app.chat_runtime_mut(target).unwrap();
            state.set_visible(false).unwrap();
            assert!(state.warm_stamp().is_some());
            assert!(!state.cache.is_empty());
            assert_source(&app);
            let state = app.chat_runtime_mut(target).unwrap();
            state.cool();
            state.refresh();
            assert!(state.warm_stamp().is_none());
            assert!(state.cache.is_empty());
            assert!(state.child_cache.is_empty());
            assert!(state.selection_document.borrow().is_none());
            assert_source(&app);
            let state = app.chat_runtime_mut(target).unwrap();
            state.set_visible(true).unwrap();
            preparation::drain_fixture(state);
            assert!(!state.cache["answer"].placeholder);
            assert!(
                matches!(&state.snapshot.items[0].content, ItemContent::Text { text, .. } if text == &shown)
            );
            if failed {
                assert_eq!(state.error.as_deref(), Some(REFUSAL));
                assert_eq!(
                    state.start_retry_blocked,
                    recovery == chat::Recovery::Blocked
                );
            }
            assert_source(&app);
        }
    }
}

/// The framed client contract covers retaining ChatClosed's final publication
/// when close arrives while hidden. This fixture starts at that boundary and
/// exercises the ordinary Runtime consumer, without a frozen Resume source.
#[test]
fn an_ordinary_cold_hidden_chat_reveals_its_dead_clients_final_publication() {
    let mut session = fresh_chat_session(
        Provider::Claude,
        TerminalId::new(),
        "/project".into(),
        &Account::system(Provider::Claude),
        false,
    )
    .unwrap();
    session.launch.session_id = Some(THREAD.into());
    let item = Arc::new(chat::Item {
        id: "ordinary-final-answer".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "The final answer arrived while this ordinary card was hidden.".into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        at: None,
        started_at: None,
    });
    let final_item = Arc::downgrade(&item);
    let handle = Arc::new(ClientHandle::fixture(
        session.clone(),
        Snapshot {
            generation: 29,
            sampled_at: Some(70_000),
            ready: true,
            exited: true,
            session_id: Some(THREAD.into()),
            error: Some("The provider stopped after its final answer".into()),
            items: vec![item],
            ..Snapshot::default()
        },
    ));
    handle.disconnect_for_test();
    let clock = format!("{:?}", handle.resident_snapshot_with_clock().unwrap().1);
    let mut state = Runtime::new(
        Target {
            card: CardId::new(),
            generation: 1,
        },
        session,
        Connection::Live(handle.clone()),
        false,
        crate::settings::EditorSyntaxTheme::default(),
    );
    state.replace_draft(text_editor::Content::with_text(DRAFT));
    for _ in 0..3 {
        state.set_visible(false).unwrap();
        state.cool();
        state.refresh();
        assert!(!state.resume_source_pinned);
        assert!(!state.retains_source_body());
        assert!(state.warm_stamp().is_none());
        assert!(state.snapshot.items.is_empty());
        assert!(state.cache.is_empty());
        assert!(state.logical_prose.raw().is_none());
        assert!(!handle.is_connected());
        assert!(handle.resident_snapshot_with_clock().is_some());
        assert!(handle.snapshot().transcript_bytes() > 0);
        assert!(final_item.upgrade().is_some());
    }

    state.set_visible(true).unwrap();
    preparation::drain_fixture(&mut state);
    assert!(!state.resume_source_pinned);
    assert_eq!(state.snapshot.generation, 29);
    assert_eq!(state.snapshot.items.len(), 1);
    assert!(Arc::ptr_eq(
        &state.snapshot.items[0],
        &final_item.upgrade().unwrap()
    ));
    assert!(
        matches!(&state.snapshot.items[0].content, ItemContent::Text { text, .. } if text == "The final answer arrived while this ordinary card was hidden.")
    );
    assert!(!state.cache["ordinary-final-answer"].placeholder);
    assert_eq!(
        state.snapshot.error.as_deref(),
        Some("The provider stopped after its final answer")
    );
    assert_eq!(format!("{:?}", state.source_clock), clock);
    assert_eq!(state.composer.text(), DRAFT);
}
