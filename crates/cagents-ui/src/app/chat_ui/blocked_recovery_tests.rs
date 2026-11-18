use super::tests::{
    KNOWN_THREAD, answer_start, chat_runtime, chat_runtime_attempt, fresh_close_answer,
    fresh_close_request, live_chat,
};
use super::*;
use iced_accessibility::{
    A11yTree,
    accesskit::{Action as A11yAction, ActionRequest, NodeId, Role, TreeId},
};

/// Dispatch the real accessibility action from the rendered control, rather
/// than accepting a label on a disabled or unrelated widget as recovery.
fn frame(
    element: Element<'_, Message>,
    width: f32,
    click: Option<&str>,
) -> (A11yTree, Vec<Message>) {
    use iced::{
        Size,
        advanced::clipboard,
        mouse,
        runtime::user_interface::{Cache, UserInterface},
    };
    let mut renderer = crate::app::tests::headless_renderer();
    let mut ui = UserInterface::build(
        element,
        Size::new(width, 800.0),
        Cache::default(),
        &mut renderer,
    );
    let mut messages = Vec::new();
    ui.update(
        &[],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    ui.draw(
        &mut renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        mouse::Cursor::Unavailable,
    );
    let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
    if let Some(label) = click {
        let matches = tree
            .root()
            .iter()
            .chain(tree.children())
            .filter(|node| {
                node.node().role() == Role::Button
                    && node.node().label() == Some(label)
                    && !node.node().is_disabled()
                    && !node.node().is_hidden()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            1,
            "one enabled recovery action: {label}; nodes: {:?}",
            tree.root()
                .iter()
                .chain(tree.children())
                .filter(|node| node.node().role() == Role::Button)
                .map(|node| (
                    node.node().label(),
                    node.node().is_disabled(),
                    node.node().is_hidden(),
                    node.node().bounds()
                ))
                .collect::<Vec<_>>()
        );
        let target_node = NodeId::from(matches[0].id().clone());
        let event = iced::Event::A11y(
            iced::advanced::widget::Id::from(u128::from(target_node.0) as u64),
            ActionRequest {
                action: A11yAction::Click,
                target_tree: TreeId::ROOT,
                target_node,
                data: None,
            },
        );
        let (_, statuses) = ui.update(
            &[event],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses, [iced::event::Status::Captured]);
        assert_eq!(messages.len(), 1, "the enabled action actually dispatches");
    }
    (tree, messages)
}

fn labels(tree: &A11yTree) -> Vec<&str> {
    tree.root()
        .iter()
        .chain(tree.children())
        .filter_map(|node| node.node().label())
        .collect()
}

pub(in crate::app) fn blocked_app(
    named: bool,
    valid_recipe: bool,
    runtime: bool,
) -> (Cagents, CardId) {
    blocked_app_with(
        named,
        valid_recipe,
        runtime,
        blocked_recovery::Reopen::Unproven,
    )
}

fn blocked_app_with(
    named: bool,
    valid_recipe: bool,
    runtime: bool,
    reopen: blocked_recovery::Reopen,
) -> (Cagents, CardId) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1200.0);
    let card = live_chat(&mut app, named.then_some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let mut source = chat_runtime(&app, card).session.clone();
    if !valid_recipe {
        source.launch.recipe.arguments = vec!["--resume".into(), "wrong-thread".into()];
    }
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
    let view = app.chat_runtime_mut(target).unwrap();
    view.session = source.clone();
    view.start_blocked = Some(reopen);
    view.error = Some("Daemon proof changed".into());
    view.replace_draft(text_editor::Content::with_text("Keep this unsent note."));
    if !runtime {
        app.chat_forget(card);
        app.cards
            .insert(card, CardRuntime::Failed("Daemon proof changed".into()));
        app.chat_ui
            .blocked_starts
            .insert(card, blocked_recovery::BlockedStart { source, reopen });
    }
    (app, card)
}

#[test]
fn blocked_recovery_actions_are_enabled_and_dispatch_for_every_settled_surface() {
    for width in [900.0, 1200.0] {
        for runtime in [false, true] {
            for (named, valid_recipe) in [(true, true), (true, false), (false, true)] {
                let (mut app, card) = blocked_app(named, valid_recipe, runtime);
                app.workspace.resize_viewport(width);
                // A blocked, connected, non-exited publication needs recovery
                // too; the generic stopped/transport predicates are insufficient.
                if runtime {
                    let target = chat_runtime(&app, card).target;
                    let view = app.chat_runtime_mut(target).unwrap();
                    view.disconnected = false;
                    let snapshot = Arc::make_mut(&mut view.snapshot);
                    snapshot.exited = false;
                    snapshot.history_loading = true;
                    snapshot.read_only_reason = Some("Saved history is read-only".into());
                }
                let fresh = named && valid_recipe;
                assert_eq!(app.chat_offers_fresh_conversation(card), fresh);
                let close_label = if runtime { "Close" } else { "Close Chat" };
                let (_, close) = frame(app.card_view(card, true, false), width, Some(close_label));
                assert!(matches!(close.as_slice(), [Message::CloseCard(id)] if *id == card));
                if fresh {
                    // History loading cannot make all recovery disappear: Close
                    // is independent of optional body loading. Fresh may wait.
                    if runtime {
                        let target = chat_runtime(&app, card).target;
                        Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot)
                            .history_loading = false;
                    }
                    let (_, mut messages) = frame(
                        app.card_view(card, true, false),
                        width,
                        Some("Start a new conversation"),
                    );
                    let message = messages.pop().unwrap();
                    assert!(
                        matches!(&message, Message::ChatUi(Action::StartFresh(id)) if *id == card)
                    );
                    drop(app.update(message));
                    assert!(matches!(
                        app.chat_ui.phases.get(&card),
                        Some(Phase::Closing {
                            restart: Some(_),
                            ..
                        })
                    ));
                    assert!(
                        app.chat_ui.attempt(card).is_none(),
                        "no start before checked Close"
                    );
                } else {
                    let (tree, _) = frame(app.card_view(card, true, false), width, None);
                    assert!(!labels(&tree).contains(&"Start a new conversation"));
                    assert!(labels(&tree).contains(&blocked_recovery::CLOSE_GUIDANCE));
                    drop(app.handle_chat_ui(Action::StartFresh(card)));
                    assert!(!app.chat_start_pending(card));
                }
            }
        }
    }
}

/// A blocked failed card leads with Start a new conversation, and neither of
/// its actions can be pressed while Close is checking the source.
#[test]
fn blocked_failed_card_leads_with_fresh_and_disables_it_while_closing() {
    let buttons = |tree: &A11yTree| {
        let mut buttons = tree
            .root()
            .iter()
            .chain(tree.children())
            .filter(|node| node.node().role() == Role::Button && !node.node().is_hidden())
            .filter_map(|node| {
                let label = node.node().label()?;
                (label == "Start a new conversation"
                    || label == "Close Chat"
                    || label.starts_with("Retry"))
                .then(|| {
                    (
                        label.to_owned(),
                        node.node().is_disabled(),
                        node.node().bounds().map_or(0.0, |bounds| bounds.x0),
                    )
                })
            })
            .collect::<Vec<_>>();
        buttons.sort_by(|a, b| a.2.total_cmp(&b.2));
        buttons
            .into_iter()
            .map(|(label, disabled, _)| (label, disabled))
            .collect::<Vec<_>>()
    };
    let (mut app, card) = blocked_app(true, true, false);
    assert!(app.chat_offers_fresh_conversation(card));
    let (tree, _) = frame(app.card_view(card, true, false), 1200.0, None);
    assert_eq!(
        buttons(&tree),
        [
            ("Start a new conversation".to_owned(), false),
            ("Close Chat".to_owned(), false)
        ],
        "Fresh leads, Close follows, and no Retry is offered"
    );
    app.completed_release = Some(card);
    assert!(app.terminal_close_in_progress(card));
    let (tree, _) = frame(app.card_view(card, true, false), 1200.0, None);
    assert_eq!(
        buttons(&tree),
        [
            ("Start a new conversation".to_owned(), true),
            ("Close Chat".to_owned(), true)
        ],
        "Close owns the card until it settles"
    );
}

#[test]
fn blocked_recovery_keeps_permanent_diagnostics_only_under_details() {
    use iced::futures::StreamExt;
    for raw in [
        "Daemon proof changed",
        "The saved conversation could not be reopened yet, so nothing was restarted or resent. Retained actor requires completed retirement",
    ] {
        let (mut app, card) = blocked_app(true, true, false);
        app.cards.insert(card, CardRuntime::Failed(raw.into()));
        let source = app.chat_return_session(card).unwrap();
        let (tree, mut details) = frame(app.card_view(card, true, false), 900.0, Some("Details"));
        assert!(labels(&tree).contains(&blocked_recovery::HEADLINE));
        assert!(
            !labels(&tree).contains(&raw),
            "collapsed primary must be plain"
        );
        drop(app.update(details.pop().unwrap()));
        let (tree, mut copy) = frame(
            app.card_view(card, true, false),
            900.0,
            Some("Copy details"),
        );
        assert!(labels(&tree).contains(&raw), "exact original is disclosed");
        let task = app.update(copy.pop().unwrap());
        let effects = iced::futures::executor::block_on(
            iced::runtime::task::into_stream(task)
                .unwrap()
                .collect::<Vec<_>>(),
        );
        assert!(effects.iter().any(|effect| matches!(effect,
            iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write { contents, .. }) if contents == raw)));

        // Same native id, different launch is a different source. Neither a
        // stale Copy nor expanded state is inherited by that replacement.
        let mut changed = source.clone();
        changed
            .launch
            .recipe
            .environment
            .insert("CHANGED".into(), "1".into());
        app.card_state_mut(card).unwrap().kind = CardKind::Chat(changed);
        assert!(app.blocked_failure_source(card).is_none());
        let task = app.handle_chat_ui(Action::CopyBlockedDetails(card, source));
        let effects = iced::runtime::task::into_stream(task)
            .map(|stream| iced::futures::executor::block_on(stream.collect::<Vec<_>>()))
            .unwrap_or_default();
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, iced::runtime::Action::Clipboard(_))),
            "a refused stale Copy cannot write the clipboard"
        );
    }
}

#[test]
fn blocked_fresh_keeps_source_and_draft_until_checked_close_and_rejects_stale_metadata() {
    for runtime in [false, true] {
        let (mut app, card) = blocked_app(true, true, runtime);
        let original = app.chat_return_session(card).unwrap();
        drop(app.handle_chat_ui(Action::StartFresh(card)));
        let first = fresh_close_request(&app, card);
        drop(app.handle_chat_ui(Action::StartFresh(card)));
        assert_eq!(
            fresh_close_request(&app, card),
            first,
            "no duplicate cleanup"
        );
        drop(app.handle_chat_ui(fresh_close_answer(
            first.clone(),
            Err("checked retirement refused".into()),
        )));
        assert_eq!(app.chat_return_session(card), Some(original.clone()));
        assert!(!app.chat_start_pending(card));
        if runtime {
            assert_eq!(
                chat_runtime(&app, card).composer.text(),
                "Keep this unsent note."
            );
        } else {
            assert_eq!(app.chat_ui.drafts[&card], "Keep this unsent note.");
        }
        assert!(app.chat_offers_fresh_conversation(card));
        drop(app.handle_chat_ui(Action::StartFresh(card)));
        let second = fresh_close_request(&app, card);
        assert_ne!(first.0, second.0);
        drop(app.handle_chat_ui(fresh_close_answer(first, Ok(()))));
        assert_eq!(fresh_close_request(&app, card), second);
        let mut changed = second.2.clone();
        changed
            .launch
            .recipe
            .environment
            .insert("CHANGED".into(), "1".into());
        assert_eq!(changed.id, original.id);
        app.card_state_mut(card).unwrap().kind = CardKind::Chat(changed.clone());
        drop(app.handle_chat_ui(fresh_close_answer(second, Ok(()))));
        assert!(app.chat_ui.attempt(card).is_none());
        assert!(!app.chat_start_pending(card));
        assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(changed));
        let (tree, _) = frame(app.card_view(card, true, false), 900.0, None);
        assert!(
            !labels(&tree).contains(&"Start a new conversation"),
            "the shared policy must remove a stale enabled offer"
        );
        if runtime {
            assert_eq!(
                chat_runtime(&app, card).composer.text(),
                "Keep this unsent note."
            );
        } else {
            assert_eq!(app.chat_ui.drafts[&card], "Keep this unsent note.");
        }
    }
}

#[test]
fn blocked_runtime_primary_hides_temporal_and_internal_reasons() {
    for raw in [
        "Daemon proof changed",
        "The saved conversation could not be reopened yet, so nothing was restarted or resent. Retained actor requires completed retirement",
    ] {
        let (mut app, card) = blocked_app(true, true, true);
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.error = Some(raw.into());
        runtime.refresh_failure_ui();
        let incident = runtime.failure_ui.incidents.last().unwrap();
        let (tree, _) = frame(runtime.failure_view(&incident), 900.0, None);
        assert!(labels(&tree).contains(&blocked_recovery::HEADLINE));
        assert!(!labels(&tree).contains(&raw));
        assert!(!runtime.stopped(false, false).guidance.contains("actor"));
        assert!(!runtime.stopped(false, false).guidance.contains("yet"));
        let key = incident.key.clone();
        drop(app.handle_chat_ui(Action::ToggleFailure(target, key.clone())));
        let runtime = chat_runtime(&app, card);
        let incident = runtime
            .failure_ui
            .incidents
            .iter()
            .find(|incident| incident.key == key)
            .unwrap();
        let (tree, _) = frame(runtime.failure_view(incident), 900.0, None);
        assert!(labels(&tree).iter().any(|text| text.contains(raw)));
        assert!(
            runtime
                .failure_details(&incident.key)
                .unwrap()
                .contains(raw)
        );
    }
}

#[test]
fn blocked_saved_history_loading_keeps_enabled_close() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1200.0);
    let card = tests::live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let mut source = chat_runtime(&app, card).session.clone();
    source.native_continuity = chat::NativeContinuity::ReconnectOnly;
    source.terminal = None;
    source.owned_codex_listener = None;
    assert!(source.is_saved_claude_history());
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.session = source.clone();
    runtime.start_blocked = Some(blocked_recovery::Reopen::Unproven);
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.exited = true;
    snapshot.history_loading = true;
    snapshot.read_only_reason = Some("Saved history is read-only".into());
    snapshot.native_continuity = source.native_continuity.clone();
    snapshot.error = None;
    snapshot.busy = false;
    snapshot.waiting = false;
    snapshot.requests.clear();
    // A saved-history reader attaches the named source. The live-start helper
    // above attached before learning the thread ID from its first snapshot.
    runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(
        source,
        runtime.snapshot.as_ref().clone(),
    )));
    assert!(runtime.has_connected_saved_history(&runtime.snapshot));
    let (_, messages) = frame(app.card_view(card, true, false), 900.0, Some("Close"));
    assert!(matches!(messages.as_slice(), [Message::CloseCard(id)] if *id == card));
}

#[test]
fn blocked_first_message_disables_unchanged_send_and_close_requires_discard() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1200.0);
    let card = tests::draft_chat(&mut app);
    tests::write(&mut app, card, "Keep this unsent note.");
    tests::submit(&mut app, card);
    let attempt = tests::chat_runtime_attempt(&app, card);
    tests::answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure {
            message: "Codex RPC error: Not logged in".into(),
            recovery: chat::Recovery::Blocked,
        }),
    );
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh_failure_ui();
    assert!(
        runtime.composer_available(false),
        "editing and copying stay available"
    );
    assert!(
        runtime.auth_error().is_none(),
        "raw text cannot authorize an unknown remedy"
    );
    let (tree, mut close) = frame(app.card_view(card, true, false), 900.0, Some("Close"));
    assert!(labels(&tree).contains(&blocked_recovery::DRAFT_HEADLINE));
    assert!(labels(&tree).contains(&blocked_recovery::DRAFT_GUIDANCE));
    let send = tree
        .root()
        .iter()
        .chain(tree.children())
        .filter(|node| {
            node.node().role() == Role::Button
                && node
                    .node()
                    .label()
                    .is_some_and(|text| text.starts_with("Send message"))
        })
        .collect::<Vec<_>>();
    assert_eq!(send.len(), 1);
    assert!(send[0].node().is_disabled());
    // Exercise the mounted editor's actual Enter bindings as well as the
    // stale/forged Submit guard below. Editing remains independently usable.
    {
        use iced::{
            advanced::{Layout, Shell, clipboard, layout, widget::Tree},
            keyboard,
        };
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app.card_view(card, true, false);
        let mut tree = Tree::new(&element);
        let viewport = iced::Rectangle::with_size(iced::Size::new(900.0, 800.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, viewport.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                card,
                "chat-composer",
            )),
        );
        for modifiers in [keyboard::Modifiers::empty(), keyboard::Modifiers::CTRL] {
            let event = iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            });
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                iced::mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            if modifiers.is_empty() {
                assert!(messages.iter().any(|message| matches!(message,
                    Message::ChatUi(Action::Composer(actual, text_editor::Action::Edit(text_editor::Edit::Enter))) if *actual == target)),
                    "the focused editor receives Enter as an edit, without sending");
            }
            assert!(messages.iter().all(|message| !matches!(
                message,
                Message::ChatUi(Action::Submit(..) | Action::SubmitControl(..))
            )));
        }
    }
    drop(app.handle_chat_ui(Action::Composer(
        target,
        text_editor::Action::Move(text_editor::Motion::DocumentStart),
    )));
    drop(app.handle_chat_ui(Action::Composer(
        target,
        text_editor::Action::Select(text_editor::Motion::DocumentEnd),
    )));
    assert_eq!(
        chat_runtime(&app, card).composer.selection().as_deref(),
        Some("Keep this unsent note.")
    );

    drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
    assert!(
        app.chat_ui.attempt(card).is_none(),
        "forged unchanged Send cannot restart"
    );
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "Keep this unsent note."
    );
    drop(app.update(close.pop().unwrap()));
    assert!(app.chat_close_dialog_visible());
    let (tree, mut cancel) = frame(app.chat_close_dialog(), 900.0, Some("Cancel"));
    assert!(labels(&tree).contains(&"Discard this message?"));
    assert!(labels(&tree).contains(&"Closing this tab discards your unsent message."));
    drop(app.update(cancel.pop().unwrap()));
    assert!(!app.chat_close_dialog_visible());
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "Keep this unsent note."
    );
    assert!(app.card_state(card).unwrap().existence.is_open());
    drop(app.update(Message::CloseCard(card)));
    let (_, mut discard) = frame(app.chat_close_dialog(), 900.0, Some("Discard"));
    drop(app.update(discard.pop().unwrap()));
    assert!(!app.card_state(card).unwrap().existence.is_open());
}

#[test]
fn typed_remediable_first_message_keeps_send_enabled() {
    for recovery in [chat::Recovery::Account, chat::Recovery::Retry] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.workspace.resize_viewport(1200.0);
        let card = tests::draft_chat(&mut app);
        tests::write(&mut app, card, "Keep this unsent note.");
        tests::submit(&mut app, card);
        let attempt = tests::chat_runtime_attempt(&app, card);
        tests::answer_start(
            &mut app,
            attempt,
            Err(chat::StartFailure {
                message: "fixture typed remediable refusal".into(),
                recovery,
            }),
        );
        assert!(!chat_runtime(&app, card).start_blocked.is_some());
        let (tree, _) = frame(app.card_view(card, true, false), 900.0, None);
        assert!(tree.root().iter().chain(tree.children()).any(|node| {
            node.node().role() == Role::Button
                && node
                    .node()
                    .label()
                    .is_some_and(|text| text.starts_with("Send message"))
                && !node.node().is_disabled()
                && !node.node().is_hidden()
        }));
        assert_eq!(
            chat_runtime(&app, card).composer.text(),
            "Keep this unsent note."
        );
    }
}

#[test]
fn choosing_another_model_lifts_a_blocked_first_start() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1200.0);
    let card = tests::draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let provider = chat_runtime(&app, card).session.launch.provider;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.start_blocked = Some(blocked_recovery::Reopen::Unproven);
    runtime.error = Some("permanent launch refusal".into());
    runtime.replace_draft(text_editor::Content::with_text("Keep this unsent note."));
    assert!(app.chat_retry_blocked(card));
    let current = chat_runtime(&app, card).snapshot.selected_model.clone();
    drop(app.handle_chat_ui(Action::DraftModel(target, provider, current)));
    assert!(
        app.chat_retry_blocked(card),
        "re-picking the current model names the same refused launch"
    );
    assert_eq!(
        chat_runtime(&app, card).error.as_deref(),
        Some("permanent launch refusal")
    );
    drop(app.handle_chat_ui(Action::DraftModel(
        target,
        provider,
        Some("another-model".into()),
    )));
    let runtime = chat_runtime(&app, card);
    assert!(
        !app.chat_retry_blocked(card),
        "the refusal answered the previous launch, not the one now chosen"
    );
    assert!(
        runtime.error.is_none(),
        "the old diagnostic no longer applies"
    );
    assert_eq!(runtime.composer.text().trim_end(), "Keep this unsent note.");
}

#[test]
fn blocked_attachment_only_draft_close_requires_discard_and_cancel_keeps_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1200.0);
    let card = tests::draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.start_blocked = Some(blocked_recovery::Reopen::Unproven);
    runtime
        .attachments
        .push(Attachment::file("/project/unsent.txt"));
    drop(app.update(Message::CloseCard(card)));
    assert!(app.chat_close_dialog_visible());
    let (_, mut cancel) = frame(app.chat_close_dialog(), 900.0, Some("Cancel"));
    drop(app.update(cancel.pop().unwrap()));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert_eq!(
        chat_runtime(&app, card).attachments,
        [Attachment::file("/project/unsent.txt")]
    );
    assert!(chat_runtime(&app, card).composer_available(false));
}

#[test]
fn blocked_saved_source_cannot_retry_through_auth_text_or_stale_account_recovery() {
    for snapshot_auth in [false, true] {
        let (mut app, card) = blocked_app(true, true, true);
        let target = chat_runtime(&app, card).target;
        let raw = "Codex RPC error: Not logged in";
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.start_blocked = None;
        runtime.disconnected = true;
        if snapshot_auth {
            Arc::make_mut(&mut runtime.snapshot).error = Some(raw.into());
            runtime.error = Some("permanent launch refusal".into());
        } else {
            runtime.error = Some(raw.into());
        }
        assert_eq!(
            runtime.auth_error(),
            Some(raw),
            "prove the heuristic would recognize these bytes"
        );
        assert!(
            app.chat_auth_resume_enabled(target),
            "the same raw bytes on an ordinary remediable source have a valid account recovery path"
        );
        let (tree, messages) = frame(
            app.card_view(card, true, false),
            1200.0,
            Some("Resume chat"),
        );
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::ResumeAfterSignIn(actual))] if *actual == target)
        );
        assert!(
            tree.root().iter().chain(tree.children()).any(|node| {
                node.node().role() == Role::Button
                    && node.node().label() == Some("Sign in")
                    && !node.node().is_disabled()
                    && !node.node().is_hidden()
            }),
            "known remediable sign-in remains accessible"
        );
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.start_blocked = Some(blocked_recovery::Reopen::Unproven);
        let source = runtime.return_session();
        // Exercise the replay handler before display assertions: the negative
        // control must reach the actual forbidden retry, not stop at a label.
        drop(app.handle_chat_ui(Action::ResumeAfterSignIn(target)));
        assert!(
            app.chat_ui.attempt(card).is_none(),
            "permanent refusal cannot generate a new attempt through auth recovery"
        );
        assert!(
            app.chat_retry_blocked(card),
            "auth text cannot reset the permanent refusal"
        );
        assert!(chat_runtime(&app, card).auth_error().is_none());
        assert!(!app.chat_auth_recovery_enabled(target));
        assert!(!app.chat_auth_resume_enabled(target));
        assert!(app.chat_auth_recovery(chat_runtime(&app, card)).is_none());
        drop(app.handle_chat_ui(Action::SignIn(target, Account::system(Provider::Codex))));
        assert!(!app.account_sign_in_running(&Account::system(Provider::Codex)));
        let (tree, _) = frame(app.card_view(card, true, false), 1200.0, Some("Close"));
        assert!(
            labels(&tree)
                .iter()
                .all(|text| !text.starts_with("Sign in") && *text != "Resume chat")
        );
        assert!(app.chat_offers_fresh_conversation(card));
        drop(app.handle_chat_ui(Action::Reconnect(target)));
        drop(app.handle_chat_ui(Action::ResumeSaved(target)));
        assert!(app.chat_retry_blocked(card));
        assert!(app.chat_ui.attempt(card).is_none());

        // These entries belonged to a previous typed Account failure. They
        // cannot authorize an action after a permanent refusal supersedes it.
        let other = app.account_ui.add_managed_fixture(Provider::Codex, 7844);
        let directory = tempfile::tempdir().unwrap();
        let homes = crate::account::home::Homes::at(directory.path().join("accounts"));
        let (key, home) = homes.create(Provider::Codex, None).unwrap();
        app.account_ui.use_homes(homes.clone());
        app.chat_ui.removed_accounts.insert(card);
        app.chat_ui.restorable_accounts.insert(
            card,
            RestorableAccount {
                provider: Provider::Codex,
                key,
                home: home.to_string_lossy().into_owned(),
                restoring: false,
                failure: None,
                resend: None,
            },
        );
        assert!(app.account_recovery_controls(card).is_none());
        drop(app.handle_chat_ui(Action::RecoverAccount(card, other)));
        drop(app.handle_chat_ui(Action::RestoreAccount(card)));
        assert!(!app.chat_ui.restorable_accounts[&card].restoring);
        assert!(app.chat_ui.attempt(card).is_none());
        app.chat_ui
            .restorable_accounts
            .get_mut(&card)
            .unwrap()
            .restoring = true;
        let reason = chat_runtime(&app, card).error.clone();
        drop(app.handle_chat_ui(Action::AccountRestored(
            card,
            Err("stale account failure".into()),
        )));
        assert_eq!(chat_runtime(&app, card).error, reason);
        let mut registry = crate::account::registry::Registry::default();
        assert!(
            registry
                .restore(
                    crate::account::registry::Registered {
                        key,
                        provider: Provider::Codex,
                        linked_home: None,
                        label: None,
                        email: Some("fixture@example.invalid".into()),
                        organization: None,
                        added_at: 1,
                    },
                    &[],
                    &homes
                )
                .is_none()
        );
        drop(app.handle_chat_ui(Action::AccountRestored(
            card,
            Ok(crate::account::remote::RestoredHome {
                registry,
                restored: crate::account::registry::Restored::Listed,
            }),
        )));
        let restored = Account::managed(Provider::Codex, key, home).id();
        assert!(
            app.known_account(&restored).is_some(),
            "global registry completion is preserved"
        );
        assert!(app.chat_retry_blocked(card));
        assert!(app.chat_ui.attempt(card).is_none());
        assert_eq!(app.chat_return_session(card), Some(source));
        assert_eq!(chat_runtime(&app, card).error, reason);
        assert_eq!(
            chat_runtime(&app, card).composer.text(),
            "Keep this unsent note."
        );
    }
}

/// Each permanent refusal's visible headline and caption say what the reason
/// does, and none promises Resume conversation from the new tab (+) menu: it
/// prepares or starts the same saved conversation and meets the same refusal.
#[test]
fn each_blocked_refusal_kind_renders_its_own_headline_and_caption() {
    use blocked_recovery::{
        CLOSE_GUIDANCE, FRESH_EXPLANATION, HEADLINE, RESTART_CLOSE_TRADEOFF,
        RESTART_FRESH_TRADEOFF, RESTART_GUIDANCE, RESTART_HEADLINE, Reopen,
    };
    let fresh_restart = format!("{RESTART_GUIDANCE} {RESTART_FRESH_TRADEOFF}");
    let close_restart = format!("{RESTART_GUIDANCE} {RESTART_CLOSE_TRADEOFF}");
    for (recovery, reopen, headline, fresh, close_only) in [
        (
            chat::Recovery::Blocked,
            Reopen::Unproven,
            HEADLINE,
            FRESH_EXPLANATION,
            CLOSE_GUIDANCE,
        ),
        (
            chat::Recovery::AfterRestart,
            Reopen::AfterRestart,
            RESTART_HEADLINE,
            fresh_restart.as_str(),
            close_restart.as_str(),
        ),
    ] {
        assert_eq!(Reopen::of(recovery), Some(reopen));
        assert_eq!(reopen.headline(), headline);
        // Waiting is the way back only after a restart; no button then
        // stands out as the next step.
        assert_eq!(reopen.waits(), reopen == Reopen::AfterRestart);
        for (valid_recipe, caption) in [(true, fresh), (false, close_only)] {
            for runtime in [false, true] {
                for (width, inspector) in [(900.0, false), (1280.0, false), (1280.0, true)] {
                    let (mut app, card) = blocked_app_with(true, valid_recipe, runtime, reopen);
                    app.workspace.resize_viewport(width);
                    assert!(app.chat_retry_blocked(card));
                    assert_eq!(app.chat_offers_fresh_conversation(card), valid_recipe);
                    let (tree, _) = if inspector {
                        drop(app.set_pane_inspector_open(card, true));
                        assert!(app.pane_inspectors.is_visible(card));
                        frame(cosmic::Application::view(&app), width, None)
                    } else {
                        frame(app.card_view(card, true, false), width, None)
                    };
                    if runtime {
                        assert_eq!(
                            chat_runtime(&app, card).emphasizes_recovery(),
                            !reopen.waits()
                        );
                    }
                    let mut labels = labels(&tree);
                    // A runtime puts the headline on its failure incident.
                    let incident_tree;
                    if runtime {
                        let target = chat_runtime(&app, card).target;
                        let runtime = app.chat_runtime_mut(target).unwrap();
                        runtime.refresh_failure_ui();
                        let incident = runtime.failure_ui.incidents.last().unwrap();
                        incident_tree = frame(runtime.failure_view(incident), width, None).0;
                        labels.extend(self::labels(&incident_tree));
                    }
                    let case = format!(
                        "{reopen:?}, fresh={valid_recipe}, runtime={runtime}, inspector={inspector}, width={width}: {labels:?}"
                    );
                    assert!(labels.contains(&headline), "{case}");
                    assert!(labels.contains(&caption), "{case}");
                    assert!(
                        labels
                            .iter()
                            .all(|label| !label.contains("Resume conversation")),
                        "{case}"
                    );
                }
            }
        }
    }
    // Every other recovery has an action of its own and is no refusal kind.
    for recovery in [
        chat::Recovery::Account,
        chat::Recovery::Retry,
        chat::Recovery::Busy,
        chat::Recovery::Unresponsive,
        chat::Recovery::Unreached,
        chat::Recovery::Capacity,
        chat::Recovery::Fresh,
    ] {
        assert_eq!(Reopen::of(recovery), None, "{recovery:?}");
    }
}

/// Run the actual abandoned Fresh task in an isolated process: no daemon can
/// provide a receipt for the captured pair. Generic legacy cleanup would
/// incorrectly report success here, before any replacement could be guarded.
#[test]
fn blocked_fresh_abandoned_cleanup_keeps_captured_pair_receipt_required() {
    const CHILD: &str = "CAGENTS_TEST_ABANDONED_FRESH_CLOSE";
    let Some(directory) = std::env::var_os(CHILD) else {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::chat_ui::blocked_recovery_tests::blocked_fresh_abandoned_cleanup_keeps_captured_pair_receipt_required",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD, directory.path())
            .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_STATE_HOME", directory.path().join("state"))
            .env("XDG_CACHE_HOME", directory.path().join("cache"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated abandoned Fresh task failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let directory = std::path::PathBuf::from(directory);
    assert!(close_fresh_conversation(None, None).is_err());
    let socket = crate::terminal::ipc::daemon_socket_path().unwrap();
    assert!(socket.starts_with(directory.join("runtime")));
    assert!(!socket.exists());
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = tests::live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let mut source = chat_runtime(&app, card).session.clone();
    source.native_continuity = chat::NativeContinuity::ReconnectOnly;
    source.owned_codex_listener = None;
    let proof = source.proof.clone().unwrap();
    source.terminal = Some(Box::new(crate::model::TerminalSession {
        id: source.id,
        launch: source.launch.recipe.clone(),
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id,
            boot_id: proof.boot_id,
            host_incarnation: proof.host_incarnation,
        },
    }));
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.session = source.clone();
    runtime.start_blocked = Some(blocked_recovery::Reopen::Unproven);
    Arc::make_mut(&mut runtime.snapshot).exited = true;
    // Close consults the attached client's publication even while another
    // operation is pending; establish the same stopped pair there too.
    runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(
        source.clone(),
        runtime.snapshot.as_ref().clone(),
    )));
    assert!(app.chat_offers_fresh_conversation(card));
    drop(app.begin_fresh_conversation(card));
    let closing = fresh_close_request(&app, card);
    drop(app.update(Message::CloseCard(card)));
    assert!(!app.chat_close_dialog_visible());
    assert!(app.chat_ui.abandoned.contains_key(&closing.0));
    let task = app.finish_fresh_conversation(
        closing.0,
        closing.1,
        closing.2,
        Err("initial exact close refused".into()),
    );
    use cosmic::iced::futures::StreamExt;
    let message = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut stream = cosmic::iced::runtime::task::into_stream(task).unwrap();
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::ChatUi(Action::Retired(..)),
                    )) = action
                    {
                        return message;
                    }
                }
                panic!("abandoned Fresh cleanup produced no retirement result");
            })
            .await
            .expect("abandoned Fresh cleanup did not finish")
        });
    let Message::ChatUi(Action::Retired(retired, ref result)) = message else {
        unreachable!()
    };
    assert!(
        result.is_err(),
        "abandoned Fresh must not treat missing daemon as a captured pair close receipt"
    );
    assert!(app.chat_ui.retiring.contains(&retired));
    drop(app.update(message));
    assert!(!app.chat_ui.retiring.contains(&retired));
    assert!(app.notices.entries.iter().any(|entry| {
        entry
            .notice
            .message
            .contains("could not be confirmed stopped")
    }));
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(
        matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(current) if current == &source)
    );
    assert!(
        !app.chat_ui
            .phases
            .values()
            .any(|phase| matches!(phase, Phase::Starting(_)))
    );
}

/// Closing an open agent's conversation retires the agent and its pane, so a
/// blocked agent pane offers Close rather than a Fresh with no card to start in.
#[test]
fn a_blocked_agent_pane_offers_close_but_not_fresh() {
    for runtime in [false, true] {
        for by_card in [false, true] {
            let (mut app, card) = blocked_app(true, true, runtime);
            assert!(app.chat_offers_fresh_conversation(card));
            let source = app.chat_return_session(card).unwrap();
            let mut agent = crate::app::channel_ui::close_tests::agent(&source);
            if by_card {
                agent.id = crate::model::AgentId::from_uuid(card.as_uuid());
                agent.session = None;
            }
            app.workspace_model.agents.push(agent.clone());
            assert!(
                !app.chat_offers_fresh_conversation(card),
                "runtime {runtime}, by card {by_card}"
            );
            let (tree, _) = frame(app.card_view(card, true, false), 1200.0, None);
            let labels = tree
                .root()
                .iter()
                .chain(tree.children())
                .filter(|node| node.node().role() == Role::Button && !node.node().is_hidden())
                .filter_map(|node| node.node().label().map(str::to_owned))
                .collect::<Vec<_>>();
            assert!(
                !labels
                    .iter()
                    .any(|label| label == "Start a new conversation")
            );
            assert!(
                labels.iter().any(|label| label.starts_with("Close")),
                "{labels:?}"
            );
            app.workspace_model.agents[0].closed = true;
            assert!(
                app.chat_offers_fresh_conversation(card),
                "a closed agent owns nothing"
            );
        }
    }
}

/// A pane that explains why its conversation couldn't open does not also
/// invite a first message, as if the conversation were simply empty.
#[test]
fn a_failed_empty_pane_does_not_invite_a_first_message() {
    const INVITATION: &str = "Your conversation appears here. Send a message to begin.";
    let (mut app, card) = blocked_app(true, true, true);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::new());
    runtime.refresh_failure_ui();
    let (tree, _) = frame(app.card_view(card, true, false), 1200.0, None);
    assert!(labels(&tree).contains(&blocked_recovery::HEADLINE));
    assert!(!labels(&tree).contains(&INVITATION));

    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.start_blocked = None;
    runtime.error = None;
    runtime.refresh_failure_ui();
    let (tree, _) = frame(app.card_view(card, true, false), 1200.0, None);
    assert!(
        labels(&tree).contains(&INVITATION),
        "an empty healthy pane still invites one"
    );
}

/// A saved conversation that couldn't reopen says its draft is kept, so its
/// Close asks before discarding typed text, as a blocked first message does.
#[test]
fn closing_a_blocked_saved_conversation_asks_before_discarding_its_draft() {
    let (mut app, card) = blocked_app(true, true, true);
    assert!(!chat_runtime(&app, card).is_draft());
    drop(app.update(Message::CloseCard(card)));
    assert!(app.chat_close_dialog_visible());
    let (tree, mut cancel) = frame(app.chat_close_dialog(), 900.0, Some("Cancel"));
    assert!(labels(&tree).contains(&"Discard this message?"));
    drop(app.update(cancel.pop().unwrap()));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "Keep this unsent note."
    );

    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::new());
    assert_eq!(
        app.blocked_draft_discard(card),
        None,
        "nothing typed, nothing to ask"
    );
}

/// The restart caption promises the unsent message comes back with the
/// conversation: it is saved with the workspace whether the refusal left a
/// runtime or a failed card, and the next start's restore puts it back.
#[test]
fn a_restart_blocked_draft_is_saved_and_returns_with_the_restore() {
    for runtime in [true, false] {
        let (mut app, card) =
            blocked_app_with(true, true, runtime, blocked_recovery::Reopen::AfterRestart);
        let source = match &app.card_state(card).unwrap().kind {
            CardKind::Chat(source) => source.clone(),
            other => panic!("the card is a chat: {other:?}"),
        };
        let saved = app.persistent_workspace();
        assert_eq!(
            saved.chat_drafts.get(&card).map(String::as_str),
            Some("Keep this unsent note."),
            "runtime: {runtime}"
        );

        // The next start: only what was saved, and this card's restore.
        app.chat_forget(card);
        app.chat_ui.blocked_starts.remove(&card);
        app.cards.insert(card, CardRuntime::Loading);
        app.chat_ui.drafts = saved.chat_drafts.clone();
        drop(app.open_chat(card, source.clone(), chat::Intent::Restore));
        let attempt = chat_runtime_attempt(&app, card);
        let restored = Snapshot {
            ready: true,
            session_id: source.launch.session_id.clone(),
            native_continuity: source.native_continuity.clone(),
            ..Snapshot::default()
        };
        answer_start(
            &mut app,
            attempt,
            Ok(Arc::new(ClientHandle::fixture(source, restored))),
        );
        assert_eq!(
            chat_runtime(&app, card).composer.text(),
            "Keep this unsent note.",
            "runtime: {runtime}"
        );
    }
}

/// A paste still arriving disables Fresh where it is, rather than withdrawing
/// it and moving Close under the pointer.
#[test]
fn a_pending_paste_disables_fresh_in_place() {
    let (mut app, card) = blocked_app(true, true, true);
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target).unwrap().pastes_pending = 1;
    assert!(app.chat_offers_fresh_conversation(card));
    let stopped = chat_runtime(&app, card).stopped(false, false);
    assert!(matches!(
        stopped.primary,
        Some((
            "Start a new conversation",
            stopped::Press::StartFresh,
            false
        ))
    ));
    assert!(matches!(
        stopped.secondary,
        Some(("Close", stopped::Press::Close, true))
    ));
    drop(app.handle_chat_ui(Action::StartFresh(card)));
    assert!(
        app.chat_ui.phases.get(&card).is_none(),
        "nothing closes mid-paste"
    );
}

/// While a chat closes, its unsent draft stays in the composer, read-only but
/// selectable. It is the composer's own editor, which shapes only the rows it
/// shows, not a text copy of the draft that shaped all of it at once (#2973).
#[test]
fn a_closing_chat_keeps_its_draft_in_the_read_only_composer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1200.0);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let draft = "A line of a long unsent draft.\n".repeat(2_000);
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text(&draft));
    let composer = |tree: &A11yTree| {
        tree.root()
            .iter()
            .chain(tree.children())
            .filter(|node| {
                node.node().role() == Role::MultilineTextInput
                    && node.node().label() == Some("Message")
            })
            .map(|node| (node.node().is_read_only(), node.node().is_disabled()))
            .collect::<Vec<_>>()
    };
    let (tree, _) = frame(app.card_view(card, true, false), 1200.0, None);
    assert_eq!(composer(&tree), [(false, false)], "an editable draft");

    app.chat_ui.phases.insert(
        card,
        Phase::Closing {
            target,
            restart: None,
        },
    );
    let (tree, _) = frame(app.card_view(card, true, false), 1200.0, None);
    assert_eq!(
        composer(&tree),
        [(true, false)],
        "the same draft, read-only"
    );
    assert!(
        !labels(&tree)
            .iter()
            .any(|label| label.contains("A line of a long unsent draft.")),
        "no text copy of the draft"
    );

    // It can still be selected and copied, but not changed.
    drop(app.handle_chat_ui(Action::ReadComposer(target, text_editor::Action::SelectAll)));
    drop(app.handle_chat_ui(Action::ReadComposer(
        target,
        text_editor::Action::Edit(text_editor::Edit::Delete),
    )));
    let composer = &chat_runtime(&app, card).composer;
    assert_eq!(composer.text(), draft);
    assert_eq!(composer.selection().as_deref(), Some(draft.as_str()));
}
