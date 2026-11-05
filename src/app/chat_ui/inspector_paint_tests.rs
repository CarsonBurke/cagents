// These regressions cross the real app reducers with resident ClientHandle
// publications. All returned tasks are dropped: no daemon or provider runs.

fn inspector_paint_app() -> Cagents {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1280.0);
    drop(app.update(Message::WindowResized(1280.0, 900.0)));
    app
}

fn inspector_paint_snapshot(generation: u64) -> Snapshot {
    Snapshot {
        generation,
        ready: true,
        session_id: Some(KNOWN_THREAD.into()),
        items: vec![
            Arc::new(chat::Item {
                id: "inspector-question".into(),
                role: chat::Role::User,
                content: text("Inspect the retained conversation"),
                at: None,
                started_at: None,
            }),
            Arc::new(chat::Item {
                id: "inspector-tool".into(),
                role: chat::Role::Assistant,
                content: ItemContent::Tool {
                    name: "shell".into(),
                    input: "inspect".into(),
                    output: "Retained tool details".into(),
                    status: chat::ToolStatus::Succeeded,
                },
                at: None,
                started_at: None,
            }),
            presentation_text(
                "inspector-answer",
                "The **existing answer** remains readable.",
            ),
        ],
        ..Snapshot::default()
    }
}

fn inspector_paint_close(app: &mut Cagents) {
    drop(app.update(Message::ContextInspector(InspectorAction::Close)));
    drop(app.advance_context_inspector_transition(Instant::now() + DIALOG_TRANSITION_DURATION));
    assert!(!app.context_inspector_is_overlay());
}

#[test]
fn inspector_paint_reports_an_accepted_handoff_failure_without_navigating() {
    let mut app = inspector_paint_app();
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    drop(app.update(Message::WindowResized(960.0, 900.0)));
    assert!(app.context_inspector_is_overlay());
    drop(app.update(Message::ChatUi(Action::SessionLinkOpened(Err(
        "The session's window did not finish opening.".into(),
    )))));
    assert_eq!(
        app.status.as_ref().unwrap().message,
        "The session's window did not finish opening."
    );
    assert_eq!(app.workspace.focused(), Some(card));
    assert_eq!(chat_runtime(&app, card).target, target);
    assert!(app.context_inspector_is_overlay());
}

#[test]
fn inspector_paint_retains_populated_reader_across_overlay_close_and_redock() {
    let mut app = inspector_paint_app();
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    show_chat_snapshot_for_test(&mut app, card, inspector_paint_snapshot(17));
    write(&mut app, card, "Unsent draft");
    drop(app.update(Message::ChatUi(Action::Toggle(
        target,
        "inspector-tool".into(),
    ))));
    drop(app.update(Message::ChatUi(Action::ToggleTurn(
        target,
        "inspector-question".into(),
    ))));
    let scroll = view::Scroll::new(275.0, 1800.0, 600.0);
    drop(app.update(Message::ChatUi(Action::Scrolled(
        target, None, scroll, true,
    ))));
    let runtime = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(runtime);
    runtime
        .scroll_memory
        .request_edge(None, scroll::Edge::Start);
    assert!(!runtime.cache["inspector-answer"].placeholder);
    let items = runtime.snapshot.items.clone();
    let handle = runtime.handle().unwrap().clone();
    let revision = runtime.revision;
    let presentation_revision = runtime.presentation_revision;
    let opened = runtime.opened.clone();
    assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));

    // Window size selects the modal Inspector. Native strip measurements
    // continue afterward, so both visibility paths must preserve the reader.
    drop(app.update(Message::WindowResized(960.0, 900.0)));
    assert!(app.context_inspector_is_overlay());
    assert!(app.workspace_inputs_blocked());
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::Resized(960.0),
    }));
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );

    for step in 0..3 {
        if step == 1 {
            inspector_paint_close(&mut app);
        } else if step == 2 {
            drop(app.update(Message::ToggleContextInspector));
            assert!(app.context_inspector_is_overlay());
            drop(app.update(Message::WindowResized(1280.0, 900.0)));
            assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
        }
        let runtime = chat_runtime(&app, card);
        assert!(runtime.visible.load(Ordering::Relaxed));
        assert!(handle.is_visible_for_test());
        assert!(Arc::ptr_eq(runtime.handle().unwrap(), &handle));
        assert_eq!(runtime.target, target);
        assert_eq!(runtime.snapshot.items.len(), items.len());
        for (actual, original) in runtime.snapshot.items.iter().zip(&items) {
            assert!(
                Arc::ptr_eq(actual, original),
                "no hydration replaced the body"
            );
        }
        assert!(!runtime.cache["inspector-answer"].placeholder);
        assert!(shown(&runtime.cache["inspector-answer"]).contains("existing answer"));
        assert!(runtime.expanded.contains("inspector-tool"));
        assert_eq!(runtime.opened, opened);
        assert_eq!(runtime.scroll, scroll);
        assert_eq!(runtime.scroll_memory.edge(None), Some(scroll::Edge::Start));
        assert_eq!(runtime.composer.text(), "Unsent draft");
        assert_eq!(runtime.revision, revision);
        assert_eq!(runtime.presentation_revision, presentation_revision);
    }
}

#[test]
fn inspector_paint_blocks_queued_native_composer_input_until_overlay_closes() {
    let renderer = crate::app::tests::headless_renderer();
    for started in [false, true] {
        let mut app = inspector_paint_app();
        let card = if started {
            live_chat(&mut app, Some(KNOWN_THREAD))
        } else {
            draft_chat(&mut app)
        };
        write(&mut app, card, "unsent");
        let character = || keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("x".into()),
            modified_key: keyboard::Key::Character("x".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyX),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("x".into()),
            repeat: false,
        };
        let edit = queued_composer_key(&app, card, character(), &renderer);
        let submit = queued_composer_key(
            &app,
            card,
            permission_key(keyboard::key::Named::Enter, keyboard::Modifiers::empty()),
            &renderer,
        );
        assert!(edit.iter().any(|message| matches!(
            message,
            Message::ChatUi(Action::Composer(_, action)) if action.is_edit()
        )));
        assert!(submit.iter().any(|message| matches!(
            message,
            Message::ChatUi(Action::Submit(_, PromptMode::Send))
        )));
        let revision = chat_runtime(&app, card).revision;
        drop(app.update(Message::WindowResized(960.0, 900.0)));
        assert!(app.context_inspector_is_overlay());
        for message in edit.into_iter().chain(submit) {
            drop(app.update(message));
        }
        let runtime = chat_runtime(&app, card);
        assert!(runtime.visible.load(Ordering::Relaxed));
        assert_eq!(runtime.composer.text(), "unsent");
        assert_eq!(runtime.revision, revision);
        assert!(runtime.pending.is_none() && runtime.queued.is_none());
        assert!(!app.chat_start_pending(card));

        inspector_paint_close(&mut app);
        for message in queued_composer_key(&app, card, character(), &renderer) {
            drop(app.update(message));
        }
        assert_eq!(chat_runtime(&app, card).composer.text(), "unsentx");
    }
}

#[test]
fn inspector_paint_accepts_paste_send_and_ack_completions_behind_overlay() {
    let _admission = attachment_draft::test_admission();
    let mut app = inspector_paint_app();
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    show_chat_snapshot_for_test(&mut app, card, inspector_paint_snapshot(17));
    write(&mut app, card, "accepted ");
    let bookmark = app
        .chat_runtime_mut(target)
        .unwrap()
        .begin_attachment_insert();
    submit(&mut app, card);
    assert_eq!(
        chat_runtime(&app, card).submit_after_paste,
        Some(PromptMode::Send)
    );
    drop(app.update(Message::WindowResized(960.0, 900.0)));
    assert!(app.context_inspector_is_overlay());

    drop(app.update(Message::ChatUi(Action::PastedText(
        target,
        bookmark,
        Some("paste".into()),
    ))));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.pastes_pending, 0);
    assert!(runtime.attachment_bookmarks.is_empty());
    assert_eq!(runtime.submit_after_paste, None);
    assert_eq!(
        runtime.pending.as_ref().unwrap().sent.as_deref(),
        Some("accepted paste")
    );
    assert!(runtime.visible.load(Ordering::Relaxed));
    settle(&mut app, card, Ok(()));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.pending.is_none());
    assert_eq!(runtime.composer.text(), "");
    assert_eq!(runtime.snapshot.items.len(), 3);
    assert!(runtime.visible.load(Ordering::Relaxed));

    drop(app.update(Message::ChatUi(Action::UpdatesAcknowledged(
        target,
        Err("fixture acknowledgement failed".into()),
    ))));
    assert_eq!(
        chat_runtime(&app, card).error.as_deref(),
        Some("fixture acknowledgement failed")
    );
    assert!(app.context_inspector_is_overlay());
}

#[test]
fn inspector_paint_ready_keeps_existing_and_restored_runtime_bodies_under_overlay() {
    for existing_runtime in [false, true] {
        let mut app = inspector_paint_app();
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        write(&mut app, card, "Retained reconnect draft");
        let old = chat_runtime(&app, card).target;
        if existing_runtime {
            drop(app.update(Message::ChatUi(Action::Reconnect(old))));
        } else {
            let session = chat_runtime(&app, card).session.clone();
            app.chat_forget(card);
            app.cards.insert(card, CardRuntime::Loading);
            drop(app.open_chat(card, session, chat::Intent::Restore));
        }
        let target = chat_runtime_attempt(&app, card);
        let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
        handle.replace_snapshot_for_test(inspector_paint_snapshot(23));
        drop(app.update(Message::WindowResized(960.0, 900.0)));
        assert!(app.context_inspector_is_overlay());
        answer_start(&mut app, target, Ok(handle.clone()));
        assert!(
            !app.chat_start_pending(card),
            "Ready must reach the reducer"
        );
        let runtime = app.chat_runtime_mut(target).unwrap();
        preparation::drain_fixture(runtime);
        assert!(runtime.visible.load(Ordering::Relaxed));
        assert!(handle.is_visible_for_test());
        assert!(Arc::ptr_eq(runtime.handle().unwrap(), &handle));
        assert_eq!(runtime.snapshot.items.len(), 3);
        assert!(!runtime.cache["inspector-answer"].placeholder);
        assert_eq!(runtime.composer.text(), "Retained reconnect draft");
    }
}

#[test]
fn inspector_paint_opens_drafts_and_late_recovery_behind_overlay() {
    let mut app = inspector_paint_app();
    let card = live_chat(&mut app, None);
    let session = chat_runtime(&app, card).session.clone();
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    drop(app.open_chat(card, session, chat::Intent::Restore));
    let target = chat_runtime_attempt(&app, card);
    drop(app.update(Message::WindowResized(960.0, 900.0)));
    assert!(app.context_inspector_is_overlay());
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure::fresh("fixture: no prior conversation")),
    );
    assert!(chat_runtime(&app, card).is_draft());
    assert!(chat_runtime(&app, card).visible.load(Ordering::Relaxed));
    assert!(!app.chat_start_pending(card));

    // A draft can also arrive from accepted navigation while the Inspector
    // is already modal; opening it must use the same painting policy.
    let added = draft_chat(&mut app);
    assert!(app.context_inspector_is_overlay());
    assert_eq!(
        app.workspace.visibility(added),
        Some(VisibilityClass::Visible)
    );
    assert!(chat_runtime(&app, added).visible.load(Ordering::Relaxed));
}

#[test]
fn inspector_paint_installs_late_draft_skills_without_accepting_queued_model_changes() {
    let mut app = inspector_paint_app();
    let card = draft_chat(&mut app);
    write(&mut app, card, "/rev");
    let runtime = chat_runtime(&app, card);
    let target = runtime.target;
    let launch = runtime.session.launch.clone();
    let skill = chat::Skill {
        name: "review".into(),
        description: "Review this change".into(),
        invocation: "[$review](/skills/review/SKILL.md)".into(),
    };
    drop(app.update(Message::WindowResized(960.0, 900.0)));
    assert!(app.context_inspector_is_overlay());
    // This is the one-shot discovery result started when the draft opened.
    // Covering the reader cannot turn its completed catalog into lost work.
    drop(app.update(Message::ChatUi(Action::DraftSkills(
        target,
        launch.clone(),
        vec![skill.clone()],
    ))));
    drop(app.update(Message::ChatUi(Action::DraftModel(
        target,
        Provider::Claude,
        None,
    ))));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.snapshot.skills, [skill.clone()]);
    assert_eq!(runtime.session.launch, launch);
    assert!(runtime.is_draft() && runtime.handle().is_none());
    assert!(!app.chat_start_pending(card));

    inspector_paint_close(&mut app);
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.snapshot.skills, [skill]);
    let (_, candidates) = runtime
        .completion()
        .expect("the completed catalog survives closing");
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.insertion == "$review")
    );
    assert_eq!(runtime.composer.text(), "/rev");
}

#[test]
fn inspector_paint_reapplication_keeps_unpainted_readers_hidden_and_releases_bodies() {
    for outside_workspace in [false, true] {
        let mut app = inspector_paint_app();
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        show_chat_snapshot_for_test(&mut app, card, inspector_paint_snapshot(17));
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        if outside_workspace {
            drop(app.select_worktree("/workspace/elsewhere".into(), false));
            assert!(app.workspace.visibility(card).is_none());
            // A former strip's durable exposure can lag its native unmount.
            // Current workspace membership still has to prevent resurrection.
            app.card_state_mut(card).unwrap().exposure = Exposure::Visible;
        } else {
            for _ in 0..5 {
                draft_chat(&mut app);
            }
            let frame = Instant::now();
            for step in 0..=4 {
                drop(app.update(Message::Frame(frame + Duration::from_millis(step * 250))));
            }
            let width = app.workspace.viewport_width();
            let offset = app.workspace.geometry().max_offset(width);
            drop(app.update(Message::Viewport {
                generation: app.workspace_view_generation,
                event: WorkspaceViewportEvent::Scrolled(workspace_widget::ViewportSample {
                    offset,
                    width,
                    user_input: true,
                }),
            }));
            assert_eq!(
                app.workspace.visibility(card),
                Some(VisibilityClass::Offscreen)
            );
        }
        app.set_workspace_surfaces_visible(true);
        let runtime = chat_runtime(&app, card);
        assert!(!runtime.visible.load(Ordering::Relaxed));
        assert!(!handle.is_visible_for_test());
        assert!(runtime.snapshot.items.is_empty());
        assert!(runtime.cache.is_empty() && runtime.child_cache.is_empty());
        assert_eq!(runtime.snapshot.transcript_bytes(), 0);
    }
}

#[test]
fn inspector_paint_parked_reader_stays_hidden_until_main_work_returns_to_overlay() {
    let mut app = inspector_paint_app();
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    show_chat_snapshot_for_test(&mut app, card, inspector_paint_snapshot(17));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    open_work_over_chat(&mut app);
    let runtime = chat_runtime(&app, card);
    assert!(!runtime.visible.load(Ordering::Relaxed));
    assert!(runtime.snapshot.items.is_empty() && runtime.cache.is_empty());
    let Some(CardRuntime::Chat(runtime)) = app.cards.remove(&card) else {
        unreachable!();
    };
    app.chat_ui.parked.insert(card, runtime);
    app.cards.insert(card, CardRuntime::Loading);
    app.set_workspace_surfaces_visible(true);
    let parked = app.chat_ui.parked.get(&card).unwrap();
    assert!(!parked.visible.load(Ordering::Relaxed));
    assert!(parked.snapshot.items.is_empty() && parked.cache.is_empty());

    // Exercise the restoration reducer for work accepted before the pane was
    // covered; Main Work still prevents that restored reader being painted.
    drop(app.handle_chat_ui(Action::Switch(card)));
    assert!(!app.chat_has_parked_runtime(card));
    assert!(!chat_runtime(&app, card).visible.load(Ordering::Relaxed));
    app.set_workspace_surfaces_visible(true);
    assert!(!chat_runtime(&app, card).visible.load(Ordering::Relaxed));
    assert!(!handle.is_visible_for_test());
    drop(app.update(Message::WindowResized(960.0, 900.0)));

    let owner = app.channel_work.view.owner().unwrap();
    drop(app.update_channel_work(crate::channel_work_ui::Action::Close(owner)));
    assert!(!app.channel_work_is_main());
    assert!(app.context_inspector_is_overlay());
    assert!(chat_runtime(&app, card).visible.load(Ordering::Relaxed));
    assert!(handle.is_visible_for_test());
    handle.replace_snapshot_for_test(inspector_paint_snapshot(18));
    drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
    let runtime = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(runtime);
    assert_eq!(runtime.snapshot.items.len(), 3);
    assert!(!runtime.cache["inspector-answer"].placeholder);
}
