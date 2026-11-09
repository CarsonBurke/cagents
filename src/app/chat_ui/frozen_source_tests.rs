//! A resume may retire the source while its daemon socket remains connected.
//! Keep one already-presented window, without retaining the old client body,
//! raw logical-message owners, read subscriptions or hidden render caches.
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
    state.replace_draft(text_editor::Content::with_text(DRAFT));
    assert!(state.resumes_in_place());
    assert!(state.logical_prose.raw().is_some());
    assert_eq!(state.logical_prose.request().is_some(), partial);
    assert_eq!(state.snapshot.items.len(), 1);
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
            handle.publish_snapshot_for_test(Snapshot {
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
                    state.refresh();
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
    assert!(state.history.is_empty());
    assert!(state.history_images.is_empty());
}
