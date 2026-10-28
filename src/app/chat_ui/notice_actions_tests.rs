fn failed_reconnect_notice(app: &mut Cagents, card: CardId, reason: &str) -> u64 {
    let target = chat_runtime(app, card).target;
    app.chat_runtime_mut(target).unwrap().disconnected = true;
    drop(app.update(Message::ChatUi(Action::Reconnect(target))));
    let attempt = chat_runtime_attempt(app, card);
    answer_start(app, attempt, Err(chat::StartFailure::retry(reason)));
    let entry = app
        .notices
        .recent()
        .find(|entry| entry.notice.key == Some(NoticeKey::ChatAttach(card)))
        .expect("the failed actual reconnect records a typed, card-scoped notice");
    assert_eq!(entry.notice.details.as_deref(), Some(reason));
    assert_eq!(
        entry.notice.message,
        format!("Could not reopen {}.", app.notice_pane_name(card).unwrap())
    );
    entry.id
}

#[test]
fn reconnect_notice_dismissal_preserves_raw_auth_failure_account_and_unsent_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text(
            "Keep this unsent recovery draft",
        ));
    let launch = chat_runtime(&app, card).session.launch.clone();
    let reason = "Codex RPC error: Not logged in";
    let notice_id = failed_reconnect_notice(&mut app, card, reason);
    let target = chat_runtime(&app, card).target;
    assert_eq!(chat_runtime(&app, card).auth_error(), Some(reason));
    assert!(app.chat_auth_recovery_enabled(target));
    drop(app.update(Message::ChromeUi(chrome_ui::Action::ToggleNotices)));
    drop(app.update(Message::ToggleNoticeDetails(notice_id)));
    assert!(
        app.notices
            .recent()
            .find(|entry| entry.id == notice_id)
            .unwrap()
            .details_open
    );
    drop(app.update(Message::DismissNotice(notice_id)));
    assert!(
        !app.notices
            .recent()
            .any(|entry| entry.notice.key == Some(NoticeKey::ChatAttach(card)))
    );
    assert_eq!(chat_runtime(&app, card).auth_error(), Some(reason));
    assert!(app.chat_auth_recovery_enabled(target));
    let wrong = app.account_ui.add_managed_fixture(Provider::Codex, 9903);
    let wrong = app.known_account(&wrong).unwrap();
    let task = app.handle_chat_ui(Action::SignIn(target, wrong));
    assert_eq!(
        task.units(),
        0,
        "notice dismissal does not authorize another account"
    );
    assert!(app.chat_ui.attempt(card).is_none());
    assert_eq!(chat_runtime(&app, card).session.launch, launch);
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "Keep this unsent recovery draft"
    );
}

#[test]
fn successful_reconnect_resolves_only_its_typed_notice_and_keeps_the_recovery_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text(
            "Still unsent after reconnect",
        ));
    failed_reconnect_notice(&mut app, card, "Exact native chat not found");
    app.notices
        .record(TransientNotice::warning("Unrelated saved warning"));
    let target = chat_runtime(&app, card).target;
    drop(app.update(Message::ChatUi(Action::Reconnect(target))));
    let attempt = chat_runtime_attempt(&app, card);
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    answer_start(&mut app, attempt, Ok(handle));
    assert!(chat_runtime(&app, card).connected());
    assert!(
        !app.notices
            .recent()
            .any(|entry| entry.notice.key == Some(NoticeKey::ChatAttach(card)))
    );
    assert!(
        app.notices
            .recent()
            .any(|entry| entry.notice.message == "Unrelated saved warning")
    );
    assert_eq!(
        chat_runtime(&app, card)
            .session
            .launch
            .session_id
            .as_deref(),
        Some(KNOWN_THREAD)
    );
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "Still unsent after reconnect"
    );
}

#[test]
fn notice_open_pane_crosses_filtered_channel_worktree_and_clean_work_without_spending_attention() {
    use crate::model::{ChannelId, ProjectChannel, ProjectId, ProjectKind};
    use crate::repository::RepositoryId;
    use crate::sidebar::SidebarProject;
    use crate::workspace_model::{
        ModelChannel,
        views::{Pane, Scope, View},
    };
    for destination in ["channel", "worktree", "clean work"] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.subagents = vec![presentation_child(
            "selected-child",
            presentation_text("child-body", "Keep this child transcript"),
        )];
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.refresh();
        runtime.selected_subagent = Some("selected-child".into());
        runtime.replace_draft(text_editor::Content::with_text(
            "Unsent while visiting notices",
        ));
        let notice =
            failed_reconnect_notice(&mut app, card, "Saved conversation could not be attached");
        let session = chat_runtime(&app, card).session.clone();
        let project = ProjectId::new();
        let repository = PathBuf::from(FIXTURE_DIRECTORY);
        app.sidebar_model.projects.push(
            SidebarProject::new(
                project,
                RepositoryId::new(),
                "Notice target project".into(),
                repository.clone(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        let channel = ChannelId::new();
        if destination != "worktree" {
            let durable = Arc::make_mut(&mut app.durable);
            durable.channels.push(ProjectChannel {
                id: channel,
                project_id: project,
                title: "Target conversation".into(),
                title_source: Default::default(),
                ordinal: 1,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
            durable.active_channel = Some(channel);
            app.workspace_model.identity = uuid::Uuid::new_v4();
            app.workspace_model.channels.push(ModelChannel {
                id: channel,
                repository: repository.clone(),
                title: "Target conversation".into(),
                title_source: Default::default(),
                ordinal: 1,
            });
            let mut view = View::empty(Scope::Channel(channel));
            view.panes.push(
                Pane::from_card(app.card_state(card).unwrap(), Some(repository.clone())).unwrap(),
            );
            app.workspace_model.views.push(view);
        }
        let elsewhere = tempfile::tempdir().unwrap();
        drop(app.new_native_chat(Provider::Codex, Some(elsewhere.path().into())));
        let other = app.workspace.focused().unwrap();
        assert_ne!(other, card);
        assert!(!app.workspace.order().any(|current| current == card));
        app.sidebar_state.apply(&SidebarAction::SetFilter(
            sidebar::SidebarFilter::PullRequests,
        ));
        app.sidebar_state.observe_card_update(card);
        let unread = app.sidebar_state.unread_updates().clone();
        if destination == "clean work" {
            // Reuse the existing real Work adapter fixture. This case covers
            // a clean leave; dirty-form confirmation/cancellation is separate.
            open_work_over_chat(&mut app);
        }
        drop(app.update(Message::ChromeUi(chrome_ui::Action::ToggleNotices)));
        assert!(app.chrome_ui.is_open());
        drop(app.update(Message::OpenNoticePane(notice)));
        assert!(!app.chrome_ui.is_open());
        assert!(!app.channel_work_is_main());
        assert_eq!(
            app.workspace.focused(),
            Some(card),
            "{destination}: the notice opens its current pane despite the sidebar filter"
        );
        assert_eq!(
            app.durable.active_channel,
            (destination != "worktree").then_some(channel)
        );
        assert_eq!(chat_runtime(&app, card).session, session);
        assert_eq!(
            chat_runtime(&app, card).composer.text(),
            "Unsent while visiting notices"
        );
        assert_eq!(
            chat_runtime(&app, card).selected_subagent.as_deref(),
            Some("selected-child")
        );
        assert_eq!(app.sidebar_state.unread_updates(), &unread);
        assert!(app.sidebar_state.has_card_update(card));
        assert!(app.notices.recent().any(|entry| entry.id == notice));
    }
}

#[test]
fn notice_actions_refuse_stale_ids_closed_popups_upper_modals_and_deleted_targets() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let stale = failed_reconnect_notice(&mut app, card, "First reconnect failure");
    let current = failed_reconnect_notice(&mut app, card, "A different reconnect failure");
    assert_ne!(stale, current);
    let other = draft_chat(&mut app);
    assert_ne!(other, card);
    drop(app.update(Message::OpenNoticePane(current)));
    drop(app.update(Message::ToggleNoticeDetails(current)));
    assert_eq!(app.workspace.focused(), Some(other));
    assert!(
        !app.notices
            .recent()
            .find(|entry| entry.id == current)
            .unwrap()
            .details_open
    );
    drop(app.update(Message::ChromeUi(chrome_ui::Action::ToggleNotices)));
    for id in [stale, current.wrapping_add(1000)] {
        drop(app.update(Message::OpenNoticePane(id)));
        drop(app.update(Message::ToggleNoticeDetails(id)));
        assert_eq!(app.workspace.focused(), Some(other));
        assert!(
            !app.notices
                .recent()
                .find(|entry| entry.id == current)
                .unwrap()
                .details_open
        );
    }
    app.settings_reset_confirm = true;
    drop(app.update(Message::OpenNoticePane(current)));
    drop(app.update(Message::ToggleNoticeDetails(current)));
    assert!(app.settings_reset_confirm);
    assert_eq!(app.workspace.focused(), Some(other));
    assert!(
        !app.notices
            .recent()
            .find(|entry| entry.id == current)
            .unwrap()
            .details_open
    );
    app.settings_reset_confirm = false;
    // Every retirement route converges on current CardState existence. This
    // directly tests that consumer boundary, without claiming a daemon close.
    app.card_state_mut(card).unwrap().existence =
        super::super::project_ui::removed_existence("notice target fixture retired");
    drop(app.update(Message::OpenNoticePane(current)));
    assert_eq!(app.workspace.focused(), Some(other));
    assert!(!app.notices.recent().any(|entry| entry.id == current));
}
