#[test]
fn remote_resume_command_preserves_draft_without_opening_desktop_history() {
    const CHILD: &str = "CAGENTS_TEST_REMOTE_RESUME_COMMAND";
    let Some(directory) = std::env::var_os(CHILD) else {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::chat_ui::tests::remote_resume_command_preserves_draft_without_opening_desktop_history",
                "--nocapture",
            ])
            .env(CHILD, directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_STATE_HOME", directory.path().join("state"))
            .env("XDG_CACHE_HOME", directory.path().join("cache"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated remote resume test failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let directory = PathBuf::from(directory);
    let account_home = directory.join("claude");
    std::fs::create_dir(&account_home).unwrap();
    let account = crate::account::Account {
        provider: Provider::Claude,
        home: crate::account::AccountHome::Linked {
            key: uuid::Uuid::new_v4(),
            path: account_home,
        },
    };
    let session = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        directory.clone(),
        &account,
    )
    .unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    drop(app.insert_chat_card(session, None, None));
    let card = app.workspace.focused().unwrap();
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    let target = runtime.target;
    let draft = "/resume private project";
    runtime.replace_draft(text_editor::Content::with_text(draft));
    let original_draft = runtime.composer.text();

    let configuration = crate::device::configuration_path().unwrap();
    std::fs::create_dir_all(configuration.parent().unwrap()).unwrap();
    std::fs::write(
        configuration,
        br#"{"devices":[{"name":"fixture","host":"fixture.invalid"}]}"#,
    )
    .unwrap();
    // Selection is configuration-only: no SSH or provider is started.
    crate::device::select(Some("fixture")).unwrap();

    drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
    assert!(!app.resume_ui.open);
    assert_eq!(chat_runtime(&app, card).composer.text(), original_draft);
    assert!(chat_runtime(&app, card).queued.is_none());
    assert!(chat_runtime(&app, card).handle().is_none());
    assert_eq!(
        app.notices.recent().next().unwrap().notice.message,
        "Conversation history is not available on remote devices yet"
    );

    // The picker entry itself also refuses callers that bypass update.
    app.status = None;
    drop(app.update_resume(super::super::resume_ui::Action::Open));
    assert!(!app.resume_ui.open);
    assert!(app.status.is_some());
    assert!(app.open_resume_from(card, "private".into()).is_none());
    assert_eq!(chat_runtime(&app, card).composer.text(), original_draft);
}

#[test]
fn accepting_next_prompt_suggestion_preserves_newer_user_input_and_requires_exact_observation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("session"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    let suggestion = chat::PromptSuggestion {
        session_id: "session".into(),
        source: "turn".into(),
        turn_generation: 3,
        text: "Run the focused tests\nThen inspect the diff".into(),
    };
    let state = Arc::make_mut(&mut runtime.snapshot);
    state.session_id = Some("session".into());
    state.turn_generation = 3;
    state.busy = false;
    state.ready = true;
    state.prompt_suggestion = Some(suggestion.clone());
    assert_eq!(runtime.prompt_suggestion(), Some(&suggestion));
    let mut stale = suggestion.clone();
    stale.source = "older".into();
    assert!(!runtime.accept_suggestion(&stale));
    assert!(runtime.composer.text().trim().is_empty());
    runtime.disconnected = true;
    assert!(!runtime.accept_suggestion(&suggestion));
    runtime.disconnected = false;
    let revision = runtime.revision;
    assert!(runtime.accept_suggestion(&suggestion));
    assert_eq!(runtime.revision, revision.wrapping_add(1));
    assert_eq!(runtime.composer.text().trim_end(), suggestion.text);
    assert!(!runtime.accept_suggestion(&suggestion));
    runtime.replace_draft(text_editor::Content::with_text("My own newer prompt"));
    assert!(!runtime.accept_suggestion(&suggestion));
    assert_eq!(runtime.composer.text().trim_end(), "My own newer prompt");
}

#[test]
fn typing_and_clearing_before_delayed_suggestion_keeps_it_dismissed_for_that_turn() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("session"));
    let target = chat_runtime(&app, card).target;
    show_chat_snapshot_for_test(
        &mut app,
        card,
        chat::Snapshot {
            session_id: Some("session".into()),
            turn_generation: 7,
            ready: true,
            ..Default::default()
        },
    );
    write(&mut app, card, "Typed before the prediction");
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::new());
    let state = Arc::make_mut(&mut runtime.snapshot);
    state.ready = true;
    state.busy = false;
    state.prompt_suggestion = Some(chat::PromptSuggestion {
        session_id: "session".into(),
        source: "turn7".into(),
        turn_generation: 7,
        text: "A delayed prediction".into(),
    });
    assert!(runtime.prompt_suggestion().is_none());
    let state = Arc::make_mut(&mut runtime.snapshot);
    state.turn_generation = 8;
    let suggestion = state.prompt_suggestion.as_mut().unwrap();
    suggestion.turn_generation = 8;
    suggestion.source = "turn8".into();
    assert!(runtime.prompt_suggestion().is_some());
    Arc::make_mut(&mut runtime.snapshot).session_id = Some("other-session".into());
    assert!(runtime.prompt_suggestion().is_none());
}

#[test]
fn native_permissions_opens_locally_even_while_busy_and_preserves_attachments() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("permissions"));
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = true;
    snapshot.capabilities.permission_modes = vec!["manual".into(), "auto".into()];
    snapshot.commands.push(chat::SlashCommand {
        name: "permissions".into(),
        description: "Provider dialog".into(),
    });
    runtime.replace_draft(text_editor::Content::with_text("/PERMISSIONS"));
    runtime.attachments.push(Attachment {
        path: "retained.png".into(),
        reference: None,
    });
    let target = runtime.target;
    drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.permission_picker);
    assert!(runtime.composer.text().trim().is_empty());
    assert_eq!(runtime.attachments[0].path, PathBuf::from("retained.png"));
    assert!(runtime.followups.is_empty());
    assert!(runtime.commands.is_idle());
    assert!(app.dismiss_chat_picker());
    assert!(!chat_runtime(&app, card).permission_picker);
    assert!(
        submission(
            "/permissions allow Bash(*)",
            &chat_runtime(&app, card).snapshot,
            &[],
            PromptMode::Steer
        )
        .is_err()
    );
}

#[test]
fn permission_retry_preserves_the_draft_and_requires_a_current_exact_action() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("permissions"));
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    Arc::make_mut(&mut runtime.snapshot)
        .permission_denials
        .push(chat::PermissionDenial {
            tool_use_id: "blocked".into(),
            tool_name: "Bash".into(),
            tool_input: Some("{\"command\":\"git push origin topic\"}".into()),
            reason: "Auto denied".into(),
            reason_type: Some("classifier".into()),
            agent_id: None,
        });
    runtime.replace_draft(text_editor::Content::with_text("Keep this next task"));
    runtime.attachments.push(Attachment {
        path: "retained.png".into(),
        reference: None,
    });
    let target = runtime.target;
    drop(app.handle_chat_ui(Action::PermissionRetry(target, "stale".into())));
    assert_eq!(
        chat_runtime(&app, card).composer.text().trim(),
        "Keep this next task"
    );
    drop(app.handle_chat_ui(Action::PermissionRetry(target, "blocked".into())));
    let runtime = chat_runtime(&app, card);
    let draft = runtime.composer.text();
    assert!(draft.starts_with("Keep this next task\n\nI authorize"));
    assert!(draft.contains("git push origin topic"));
    assert_eq!(runtime.attachments.len(), 1);
    assert!(runtime.commands.is_idle());
    assert_eq!(runtime.snapshot.permission_denials.len(), 1);
}

#[test]
fn permission_retry_promotes_recalled_text_for_navigation_and_persistence() {
    for recalled_steps in [1, 2] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("permissions"));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.history = vec!["older sent prompt".into(), "newest sent prompt".into()];
        runtime.replace_draft(text_editor::Content::with_text("original unsent draft"));
        runtime.attachments.push(Attachment {
            path: "retained.png".into(),
            reference: None,
        });
        Arc::make_mut(&mut runtime.snapshot)
            .permission_denials
            .push(chat::PermissionDenial {
                tool_use_id: "blocked".into(),
                tool_name: "Bash".into(),
                tool_input: Some(r#"{"command":"git status"}"#.into()),
                reason: "Auto denied".into(),
                reason_type: None,
                agent_id: None,
            });
        for _ in 0..recalled_steps {
            drop(app.handle_chat_ui(Action::HistoryOlder(target)));
        }
        let recalled = chat_runtime(&app, card).composer.text();
        assert_eq!(
            chat_runtime(&app, card).unsent_text(),
            "original unsent draft"
        );
        // A stale denial does not promote a merely browsed prompt.
        drop(app.handle_chat_ui(Action::PermissionRetry(target, "stale".into())));
        assert_eq!(
            chat_runtime(&app, card).unsent_text(),
            "original unsent draft"
        );
        assert!(chat_runtime(&app, card).history_cursor.is_some());

        drop(app.handle_chat_ui(Action::PermissionRetry(target, "blocked".into())));
        let runtime = chat_runtime(&app, card);
        let prepared = runtime.composer.text();
        assert!(prepared.starts_with(&format!("{recalled}\n\nI authorize")));
        assert!(prepared.contains("git status"));
        assert_eq!(runtime.unsent_text(), prepared);
        assert!(runtime.history_cursor.is_none());
        assert!(runtime.history_stash.is_none());
        assert_eq!(runtime.attachments.len(), 1);
        assert!(runtime.commands.is_idle());
        assert!(runtime.followups.is_empty());
        assert_eq!(app.persistent_workspace().chat_drafts[&card], prepared);

        // Down cannot recover the pre-retry stash, including after a
        // fresh Up/Down visit to the sent history.
        drop(app.handle_chat_ui(Action::HistoryNewer(target)));
        assert_eq!(chat_runtime(&app, card).composer.text(), prepared);
        drop(app.handle_chat_ui(Action::HistoryOlder(target)));
        drop(app.handle_chat_ui(Action::HistoryNewer(target)));
        assert_eq!(chat_runtime(&app, card).composer.text(), prepared);

        let session = chat_runtime(&app, card).session.clone();
        app.chat_forget(card);
        assert_eq!(app.chat_ui.drafts[&card], prepared);
        let saved = app.persistent_workspace();
        app.chat_ui = State::with_drafts(saved.chat_drafts.clone());
        drop(app.open_chat_draft(card, session));
        assert_eq!(chat_runtime(&app, card).composer.text(), prepared);
    }
}

#[test]
fn composer_thumbnail_lifecycle_tracks_attach_remove_submit_and_take_back() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    let path = PathBuf::from("/fixture/image.png");
    runtime.attach([path.clone()]);
    assert_eq!(runtime.thumbnails.subscriptions(runtime.target).len(), 1);
    runtime.attach([path.clone()]);
    assert_eq!(runtime.attachments.len(), 1);
    let queued = runtime.take_attachments();
    assert!(runtime.thumbnails.subscriptions(runtime.target).is_empty());
    runtime.take_back("queued", queued, PromptMode::Queue);
    assert_eq!(runtime.thumbnails.subscriptions(runtime.target).len(), 1);
    let target = runtime.target;
    runtime.visible.store(true, Ordering::Relaxed);
    let fingerprints = |subscription: Subscription<Message>| {
        cosmic::iced::advanced::subscription::into_recipes(subscription)
            .into_iter()
            .map(|recipe| {
                let mut hash = cosmic::iced::advanced::subscription::Hasher::default();
                recipe.hash(&mut hash);
                std::hash::Hasher::finish(&hash)
            })
            .collect::<HashSet<_>>()
    };
    let thumbnails = fingerprints(Subscription::batch(
        runtime.thumbnails.subscriptions(target),
    ));
    assert!(!thumbnails.is_empty());
    let visible = fingerprints(app.chat_subscription());
    assert!(thumbnails.is_subset(&visible));
    app.chat_runtime_mut(target)
        .unwrap()
        .visible
        .store(false, Ordering::Relaxed);
    assert!(thumbnails.is_disjoint(&fingerprints(app.chat_subscription())));
    app.chat_runtime_mut(target)
        .unwrap()
        .visible
        .store(true, Ordering::Relaxed);
    drop(app.handle_chat_ui(Action::RemoveAttachment(target, 0)));
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(runtime.attachments.is_empty());
    assert!(runtime.thumbnails.subscriptions(target).is_empty());
    runtime.attach([path]);
    assert_eq!(runtime.thumbnails.subscriptions(target).len(), 1);
}

#[test]
fn image_gallery_is_modal_and_reuses_the_transcript_handle() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    let content = ItemContent::Image {
        path: "/fixture/image.png".into(),
        description: "Generated image".into(),
        reference: None,
    };
    let mut cached = CachedItem::new(&content, runtime.syntax_theme);
    cached.image = Some(iced::widget::image::Handle::from_rgba(
        1,
        1,
        vec![0, 0, 0, 255],
    ));
    runtime.cache.insert("image".into(), cached);
    Arc::make_mut(&mut runtime.snapshot).items = vec![
        chat::Item {
            id: "image".into(),
            role: chat::Role::Assistant,
            content,
            started_at: None,
            at: None,
        }
        .into(),
    ];
    let target = runtime.target;
    drop(app.handle_chat_ui(Action::OpenImage(target, "image".into())));
    assert!(app.chat_ui.image_viewer.is_some());
    assert!(app.modal_is_open());
    assert!(app.dialog().is_some());
    let before = app.pane_inspectors.visible().count();
    drop(app.update(Message::ToggleContextInspector));
    assert_eq!(app.pane_inspectors.visible().count(), before);
    assert!(Action::ImageNext(true).allowed_while_modal());
    assert!(!Action::OpenImage(target, "image".into()).allowed_while_modal());
    drop(app.update(Message::ChatUi(Action::CloseImage)));
    assert!(!app.modal_is_open());
}
#[test]
fn image_actions_follow_the_displayed_pixels_into_the_clipboard() {
    use iced::clipboard::mime::AsMimeTypes;
    use iced::futures::StreamExt;
    type Effect = iced::runtime::Action<cosmic::Action<Message>>;
    fn run(task: Task<Message>) -> Vec<Effect> {
        let Some(stream) = iced::runtime::task::into_stream(task) else {
            return Vec::new();
        };
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
            .block_on(stream.collect())
    }
    fn copied(app: &mut Cagents, effects: Vec<Effect>) -> Vec<String> {
        let mut offered = Vec::new();
        for effect in effects {
            let iced::runtime::Action::Output(cosmic::Action::App(message)) = effect else {
                panic!("encoding reports back before touching the clipboard");
            };
            for effect in run(app.update(message)) {
                if let iced::runtime::Action::Clipboard(
                    iced::runtime::clipboard::Action::WriteData(data, _),
                ) = effect
                {
                    let png = data.as_bytes("image/png").expect("PNG is offered");
                    let decoded = image::load_from_memory(&png).unwrap().into_rgba8();
                    assert_eq!(decoded.into_raw(), [9, 8, 7, 255]);
                    offered.extend(data.available().iter().cloned());
                }
            }
        }
        offered
    }
    // A notice raised directly, or one `update` has already moved to the log.
    fn notice(app: &Cagents) -> (NoticeSeverity, String) {
        app.status
            .as_ref()
            .or_else(|| app.notices.entries.back().map(|entry| &entry.notice))
            .map(|notice| (notice.severity, notice.message.clone()))
            .expect("the action reports its outcome")
    }
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    let content = ItemContent::Image {
        path: "shots/result.png".into(),
        description: "Generated image".into(),
        reference: None,
    };
    let mut cached = CachedItem::new(&content, runtime.syntax_theme);
    let shown = iced::widget::image::Handle::from_rgba(1, 1, vec![9, 8, 7, 255]);
    cached.image = Some(shown.clone());
    runtime.cache.insert("image".into(), cached);
    Arc::make_mut(&mut runtime.snapshot).items = vec![
        chat::Item {
            id: "image".into(),
            role: chat::Role::Assistant,
            content,
            started_at: None,
            at: None,
        }
        .into(),
    ];
    let target = runtime.target;
    let expected_path = runtime
        .session
        .launch
        .recipe
        .working_directory
        .join("shots/result.png");
    let preview = images::ImageRef::Transcript(target, shown.id());

    // Copy image path: a relative provider path resolves in its own cwd.
    let effects = run(app.image_command(preview, images::ImageCommand::CopyPath));
    assert!(matches!(
        effects.as_slice(),
        [iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write { contents, .. })]
            if Path::new(contents) == expected_path
    ));
    assert_eq!(notice(&app).1, "Copied image path");

    // Copy image: encoded off the UI thread, then offered as image/png.
    let effects = run(app.image_command(preview, images::ImageCommand::Copy));
    assert_eq!(copied(&mut app, effects), ["image/png"]);
    assert_eq!(notice(&app).1, "Copied image");

    // A menu opened on pixels that have since been replaced acts on nothing.
    app.chat_runtime_mut(target)
        .unwrap()
        .cache
        .get_mut("image")
        .unwrap()
        .image = Some(iced::widget::image::Handle::from_rgba(
        1,
        1,
        vec![1, 1, 1, 255],
    ));
    assert!(run(app.image_command(preview, images::ImageCommand::Copy)).is_empty());
    assert_eq!(notice(&app).0, NoticeSeverity::Warning);
    app.chat_runtime_mut(target)
        .unwrap()
        .cache
        .get_mut("image")
        .unwrap()
        .image = Some(shown.clone());

    // Open image from the menu opens the viewer on the same pixels.
    drop(app.image_command(preview, images::ImageCommand::Open));
    let viewer = app.chat_ui.image_viewer.as_ref().unwrap().selected_image();
    assert_eq!(viewer, images::ImageRef::Viewer(shown.id()));
    assert!(Action::Image(viewer, images::ImageCommand::Copy).allowed_while_modal());
    assert!(Action::ImageCopied(text_copy::Intent::begin(), Ok(None)).allowed_while_modal());
    assert!(!Action::Image(preview, images::ImageCommand::Copy).allowed_while_modal());

    // Ctrl+C in the viewer copies the image it shows, by physical key on
    // any layout (here Cyrillic), once per press rather than per repeat.
    app.status = None;
    app.notices.entries.clear();
    let control_c = |repeat| keyboard::Event::KeyPressed {
        key: keyboard::Key::Character("с".into()),
        modified_key: keyboard::Key::Character("с".into()),
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyC),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::CTRL,
        text: None,
        repeat,
    };
    let press = app.update(Message::KeyEvent {
        event: control_c(false),
        captured: false,
    });
    // The chord's repeats, arriving while the press is still encoding,
    // neither copy again nor cancel the copy the press started.
    assert!(
        run(app.update(Message::KeyEvent {
            event: control_c(true),
            captured: false,
        }))
        .is_empty()
    );
    assert_eq!(copied(&mut app, run(press)), ["image/png"]);
    assert_eq!(notice(&app).1, "Copied image");
    assert!(
        app.chat_ui.image_viewer.is_some(),
        "copying keeps the viewer open"
    );

    // Escape is the viewer's own rather than the app-wide key handler's,
    // so nothing focused beneath the viewer can claim it first, while the
    // Escape that its open context menu claims closes only that menu.
    let escape = keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(keyboard::key::Named::Escape),
        modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    };
    drop(app.update(Message::KeyEvent {
        event: escape.clone(),
        captured: true,
    }));
    assert!(app.chat_ui.image_viewer.is_some());
    {
        use iced::advanced::layout::{Layout, Limits};
        use iced::advanced::widget::Tree;
        use iced::advanced::{Shell, clipboard, mouse};
        let renderer = crate::app::tests::headless_renderer();
        let window = iced::Size::new(1000.0, 800.0);
        let viewport = iced::Rectangle::with_size(window);
        let gallery = app.chat_ui.image_viewer.as_ref().unwrap();
        let mut element = gallery.view();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &Limits::new(iced::Size::ZERO, window),
        );
        let pointer = mouse::Cursor::Available(viewport.center());
        let send = |element: &mut Element<'_, Message>, tree: &mut Tree, event: iced::Event| {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(&node),
                pointer,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            messages
                .iter()
                .filter(|message| matches!(message, Message::ChatUi(Action::CloseImage)))
                .count()
        };
        let menu_open = |element: &mut Element<'_, Message>, tree: &mut Tree| {
            element
                .as_widget_mut()
                .overlay(
                    tree,
                    Layout::new(&node),
                    &renderer,
                    &viewport,
                    iced::Vector::ZERO,
                )
                .is_some()
        };
        send(
            &mut element,
            &mut tree,
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
        );
        assert!(
            menu_open(&mut element, &mut tree),
            "the image's menu opened"
        );
        assert_eq!(
            send(
                &mut element,
                &mut tree,
                iced::Event::Keyboard(escape.clone())
            ),
            0
        );
        assert!(
            !menu_open(&mut element, &mut tree),
            "Escape closed the menu"
        );
        assert_eq!(
            send(&mut element, &mut tree, iced::Event::Keyboard(escape)),
            1
        );
    }
}

#[test]
fn only_literal_image_paths_are_offered_for_copying() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    let image = |path: &str, reference: Option<chat::ImageReference>| ItemContent::Image {
        path: path.into(),
        description: String::new(),
        reference,
    };
    let path = |runtime: &Runtime, content: ItemContent| {
        images::clipboard::copyable_path(runtime, &content)
    };
    assert_eq!(
        path(&*runtime, image("/abs/shot.png", None)).as_deref(),
        Some("/abs/shot.png")
    );
    assert!(path(&*runtime, image("data:image/png;base64,AAAA", None)).is_none());
    assert!(path(&*runtime, image("https://example.com/a.png", None)).is_none());
    assert!(path(&*runtime, image("", None)).is_none());
    let managed = chat::ImageReference {
        id: uuid::Uuid::new_v4(),
        number: 1,
        device: uuid::Uuid::new_v4(),
        name: "Pasted image".into(),
        sha256: "a".repeat(64),
        extension: "png".into(),
    };
    assert!(
        path(&*runtime, image("/private/asset-digest.png", Some(managed))).is_none(),
        "a managed asset's snapshot path is not an authority to expose"
    );
    let cached = format!(
        "/run/user/1000/cagents/chat-{}/image-{}.png",
        uuid::Uuid::new_v4(),
        "0f".repeat(32)
    );
    assert!(
        path(&*runtime, image(&cached, None)).is_none(),
        "a provider image decoded into the private transcript cache is ephemeral"
    );
    assert!(
        path(&*runtime, image(&format!("file://{cached}"), None)).is_none(),
        "nor is its file URL"
    );
    assert!(path(&*runtime, image("/abs/image-0f.png", None)).is_some());
    assert_eq!(
        path(&*runtime, image("file:///abs/shot%20one.png", None)).as_deref(),
        Some("/abs/shot one.png"),
        "a file URL names the same file the viewer offers"
    );
    assert!(path(&*runtime, image("relative.png", None)).is_some());
    runtime.selected_subagent = Some("child".into());
    assert!(
        path(&*runtime, image("relative.png", None)).is_none(),
        "a child's relative path has no known working directory"
    );
}

#[test]
fn image_loads_ignore_replaced_items_and_retired_runtime_generations() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let image = |path: &str| {
        Arc::new(chat::Item {
            id: "image".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Image {
                path: path.into(),
                description: "Image".into(),
                reference: None,
            },
            started_at: None,
            at: None,
        })
    };
    let old = image("/old.png");
    let current = image("/current.png");
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    runtime.visible.store(true, Ordering::Relaxed);
    let mut cached = CachedItem::new(&current.content, runtime.syntax_theme);
    cached.image_source = Some(current.clone());
    cached.image_item = Some(current.clone());
    Arc::make_mut(&mut runtime.snapshot).items = vec![current.clone()];
    runtime.cache.insert(current.id.clone(), cached);
    let target = runtime.target;
    let handle = || iced::widget::image::Handle::from_rgba(1, 1, vec![0; 4]);
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        old,
        0,
        images::Loaded::Ready(handle()),
    )));
    assert!(chat_runtime(&app, card).cache["image"].image.is_none());
    let retired = Target {
        generation: target.generation.wrapping_add(1),
        ..target
    };
    drop(app.handle_chat_ui(Action::ImageLoaded(
        retired,
        current.clone(),
        0,
        images::Loaded::Ready(handle()),
    )));
    assert!(chat_runtime(&app, card).cache["image"].image.is_none());
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        current,
        0,
        images::Loaded::Ready(handle()),
    )));
    assert!(chat_runtime(&app, card).cache["image"].image.is_some());
    assert!(
        chat_runtime(&app, card).cache["image"]
            .image_source
            .is_none()
    );
    assert!(
        Action::ImageLoaded(target, image("/unused"), 0, images::Loaded::Unavailable)
            .allowed_while_modal()
    );
}
#[test]
fn image_refresh_invalidates_tail_token_collisions_and_accepts_equal_content_replacements() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("images"));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let image = |prefix: &str| {
        Arc::new(chat::Item {
            id: "image".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Image {
                path: format!("/{prefix}/{}.png", "same-tail".repeat(20)),
                description: "Image".into(),
                reference: None,
            },
            started_at: None,
            at: None,
        })
    };
    let old = image("old");
    let changed = image("new");
    assert_eq!(
        ChangeToken::of(&old.content),
        ChangeToken::of(&changed.content)
    );
    let publish = |app: &mut Cagents, item: Arc<chat::Item>, generation| {
        app.chat_runtime_mut(target).unwrap().show(true);
        handle.replace_snapshot_for_test(Snapshot {
            items: vec![item],
            generation,
            ready: true,
            ..Snapshot::default()
        });
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.visible.store(true, Ordering::Relaxed);
        runtime.refresh();
        preparation::drain_fixture(runtime);
    };
    publish(&mut app, old.clone(), 10);
    publish(&mut app, changed.clone(), 11);
    let cached = &chat_runtime(&app, card).cache["image"];
    assert!(Arc::ptr_eq(cached.image_source.as_ref().unwrap(), &changed));
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        old,
        0,
        images::Loaded::Ready(iced::widget::image::Handle::from_rgba(1, 1, vec![0; 4])),
    )));
    assert!(chat_runtime(&app, card).cache["image"].image.is_none());
    // A new Arc with identical content must not cancel/restart the worker.
    let equal = image("new");
    publish(&mut app, equal.clone(), 12);
    let cached = &chat_runtime(&app, card).cache["image"];
    assert!(Arc::ptr_eq(cached.image_item.as_ref().unwrap(), &equal));
    assert!(Arc::ptr_eq(cached.image_source.as_ref().unwrap(), &changed));
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        changed.clone(),
        0,
        images::Loaded::Ready(iced::widget::image::Handle::from_rgba(1, 1, vec![0; 4])),
    )));
    assert!(chat_runtime(&app, card).cache["image"].image.is_some());
    publish(&mut app, image("new"), 13);
    assert!(chat_runtime(&app, card).cache["image"].image.is_some());
    assert!(
        chat_runtime(&app, card).cache["image"]
            .image_source
            .is_none()
    );
    publish(&mut app, image("old"), 14);
    assert!(chat_runtime(&app, card).cache["image"].image.is_none());
    assert!(
        chat_runtime(&app, card).cache["image"]
            .image_source
            .is_some()
    );
    let cached = &chat_runtime(&app, card).cache["image"];
    let failed_source = cached.image_source.as_ref().unwrap().clone();
    let attempt = cached.image_attempt;
    let error = "Image belongs to another device; open its producing worktree";
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        failed_source,
        attempt,
        images::Loaded::Failed(error.into()),
    )));
    assert!(chat_runtime(&app, card).cache["image"].image_unavailable);
    publish(&mut app, image("old"), 15);
    let cached = &chat_runtime(&app, card).cache["image"];
    assert!(cached.image_unavailable);
    assert_eq!(cached.image_error.as_deref(), Some(error));
    assert!(cached.image_source.is_none());
}
fn preview_item(id: &str) -> Arc<chat::Item> {
    Arc::new(chat::Item {
        id: id.into(),
        role: chat::Role::Assistant,
        content: ItemContent::Image {
            path: format!("/{id}.png"),
            description: id.into(),
            reference: None,
        },
        started_at: None,
        at: None,
    })
}
fn preview_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::write_buffer_with_format(
        &mut std::io::Cursor::new(&mut bytes),
        &[1, 2, 3, 255],
        1,
        1,
        image::ExtendedColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}
fn preview_items(runtime: &mut Runtime, items: &[Arc<chat::Item>]) {
    Arc::make_mut(&mut runtime.snapshot).items = items.to_vec();
    for item in items {
        runtime.cache.entry(item.id.clone()).or_insert_with(|| {
            let mut cached = CachedItem::new(&item.content, runtime.syntax_theme);
            cached.image_item = Some(item.clone());
            cached.image_source = Some(item.clone());
            cached
        });
    }
    runtime.visible.store(true, Ordering::Relaxed);
    images::sync(runtime);
}
fn finish_preview(runtime: &mut Runtime, item: &Arc<chat::Item>, loaded: images::Loaded) {
    let attempt = runtime.presentation_cache()[&item.id].image_attempt;
    images::complete(runtime, item, attempt, loaded);
}
#[test]
fn preview_pressure_admits_newer_images_only_after_retained_trees_release_pixels() {
    use iced::advanced::{layout, widget::Tree};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    let items: Vec<_> = (0..3)
        .map(|index| preview_item(&format!("image-{index}")))
        .collect();
    let bytes = preview_png();
    let budget = images::Budget::new(1024, 8);
    preview_items(runtime, &items[..2]);
    for item in items[..2].iter().rev() {
        finish_preview(
            runtime,
            item,
            images::bounded_handle(bytes.clone(), &budget),
        );
    }
    let gallery = images::Gallery::new(runtime, &items[0].id).unwrap();
    let renderer = crate::app::tests::headless_renderer();
    let mut trees = Vec::new();
    for item in &items[..2] {
        let handle = runtime.cache[&item.id].image.clone().unwrap();
        // Exercise the real Memo's retained content, not just a handle
        // variable: dropping the frame leaves its tree owning pixels.
        let mut frame = retained::kept(1, move || iced::widget::image(handle.clone()).into());
        let mut tree = Tree::new(frame.as_widget());
        frame.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(200.0, 200.0)),
        );
        drop(frame);
        trees.push(tree);
    }
    preview_items(runtime, &items);
    let deferred = images::bounded_handle(bytes.clone(), &budget);
    assert!(matches!(deferred, images::Loaded::Deferred(_)));
    finish_preview(runtime, &items[2], deferred);
    let pressure = runtime.cache[&items[2].id].image_wait.clone().unwrap();
    assert!(runtime.cache[&items[0].id].image.is_none());
    assert!(runtime.cache[&items[0].id].image_source.is_none());
    assert!(runtime.cache[&items[0].id].image_evicted);
    assert!(
        runtime.cache[&items[1].id].image.is_some(),
        "only the oldest owner needs eviction"
    );
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert!(
        executor
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(10), pressure.available()).await
            })
            .is_err(),
        "evicting application handles does not retire retained pixels"
    );
    drop(gallery);
    assert!(
        executor
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(10), pressure.available()).await
            })
            .is_err(),
        "retained Memo trees still own the released gallery's pixels"
    );
    for _ in 0..10 {
        images::sync(runtime);
        assert_eq!(images::subscriptions(runtime).len(), 1);
        assert!(runtime.cache[&items[0].id].image_source.is_none());
    }
    drop(trees.remove(0));
    executor.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), pressure.available())
            .await
            .unwrap();
    });
    finish_preview(
        runtime,
        &items[2],
        images::bounded_handle(bytes.clone(), &budget),
    );
    assert!(runtime.cache[&items[2].id].image.is_some());
    assert!(images::subscriptions(runtime).is_empty());
    // Even after the other stale tree goes away, pressure-evicted recent
    // candidates do not automatically recreate an eviction/decode loop.
    drop(trees);
    images::sync(runtime);
    assert!(images::subscriptions(runtime).is_empty());
    images::request(runtime, &items[0].id);
    finish_preview(
        runtime,
        &items[0],
        images::bounded_handle(bytes.clone(), &budget),
    );
    let pressure = runtime.cache[&items[0].id].image_wait.clone().unwrap();
    executor.block_on(pressure.available());
    finish_preview(runtime, &items[0], images::bounded_handle(bytes, &budget));
    assert!(runtime.cache[&items[0].id].image.is_some());
    assert_eq!(
        runtime.preview_request.as_deref(),
        Some(items[0].id.as_str())
    );
}
#[test]
fn preview_interest_bounds_manual_requests_and_suppresses_over_budget_recent_candidates() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    let items: Vec<_> = (0..20)
        .map(|index| preview_item(&format!("image-{index}")))
        .collect();
    preview_items(runtime, &items);
    let bytes = preview_png();
    let budget = images::Budget::new(1024, 8);
    // Newest first: two fit, the third cannot displace higher-priority
    // previews and does not keep retrying just because it remains recent.
    for item in items.iter().rev().take(3) {
        finish_preview(
            runtime,
            item,
            images::bounded_handle(bytes.clone(), &budget),
        );
    }
    assert!(runtime.cache[&items[17].id].image_evicted);
    assert!(runtime.cache[&items[17].id].image_source.is_none());
    for item in items.iter().take(12) {
        images::request(runtime, &item.id);
        assert_eq!(runtime.preview_request.as_deref(), Some(item.id.as_str()));
        assert!(
            runtime
                .cache
                .values()
                .filter(|cached| cached.image_source.is_some())
                .count()
                <= 9
        );
    }
    assert!(runtime.cache[&items[0].id].image_source.is_none());
    assert_eq!(images::subscriptions(runtime).len(), 1);
    let requested = &items[11];
    finish_preview(
        runtime,
        requested,
        images::bounded_handle(bytes.clone(), &budget),
    );
    let pressure = runtime.cache[&requested.id].image_wait.clone().unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    executor.block_on(pressure.available());
    finish_preview(runtime, requested, images::bounded_handle(bytes, &budget));
    assert!(runtime.cache[&requested.id].image.is_some());
    assert!(runtime.cache[&items[19].id].image_source.is_none());
}
#[test]
fn preview_transient_pressure_retries_without_retiring_ready_images() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    let items: Vec<_> = (0..3)
        .map(|index| preview_item(&format!("image-{index}")))
        .collect();
    let bytes = preview_png();
    let budget = images::Budget::new(1024, 12);
    preview_items(runtime, &items[..2]);
    for item in &items[..2] {
        finish_preview(
            runtime,
            item,
            images::bounded_handle(bytes.clone(), &budget),
        );
    }
    preview_items(runtime, &items);
    let decoder = budget.block_decoder();
    finish_preview(
        runtime,
        &items[2],
        images::bounded_handle(bytes.clone(), &budget),
    );
    assert!(runtime.cache[&items[0].id].image.is_some());
    assert!(runtime.cache[&items[1].id].image.is_some());
    assert!(!runtime.cache[&items[2].id].image_evicted);
    let pressure = runtime.cache[&items[2].id].image_wait.clone().unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    assert!(
        executor
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(10), pressure.available()).await
            })
            .is_err()
    );
    drop(decoder);
    executor.block_on(pressure.available());
    finish_preview(runtime, &items[2], images::bounded_handle(bytes, &budget));
    assert!(runtime.cache[&items[2].id].image.is_some());
}

#[test]
fn preview_hide_and_subagent_switch_release_owners_and_fence_late_attempts() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let parent = preview_item("parent");
    let child = preview_item("child-image");
    let bytes = preview_png();
    let budget = images::Budget::new(1024, 4);
    let runtime = app.chat_runtime_mut(target).unwrap();
    preview_items(runtime, std::slice::from_ref(&parent));
    let parent_attempt = runtime.cache[&parent.id].image_attempt;
    finish_preview(
        runtime,
        &parent,
        images::bounded_handle(bytes.clone(), &budget),
    );
    Arc::make_mut(&mut runtime.snapshot)
        .subagents
        .push(chat::Subagent {
            model: None,
            waiting: false,
            detail: String::new(),
            id: "child".into(),
            kind: "fixture".into(),
            description: "Child".into(),
            status: chat::ToolStatus::Succeeded,
            background: false,
            items: vec![child.clone()],
        });
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
    assert!(chat_runtime(&app, card).cache[&parent.id].image.is_none());
    // The real lease can now be acquired for the selected transcript.
    let runtime = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(runtime);
    let child_attempt = runtime.child_cache[&child.id].image_attempt;
    finish_preview(
        runtime,
        &child,
        images::bounded_handle(bytes.clone(), &budget),
    );
    assert!(runtime.child_cache[&child.id].image.is_some());
    runtime.set_visible(false).unwrap();
    assert!(runtime.child_cache[&child.id].image.is_none());
    runtime.set_visible(true).unwrap();
    assert_ne!(runtime.child_cache[&child.id].image_attempt, child_attempt);
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        child.clone(),
        child_attempt,
        images::bounded_handle(bytes.clone(), &budget),
    )));
    assert!(
        chat_runtime(&app, card).child_cache[&child.id]
            .image
            .is_none()
    );
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        parent.clone(),
        parent_attempt,
        images::bounded_handle(bytes.clone(), &budget),
    )));
    assert!(chat_runtime(&app, card).cache[&parent.id].image.is_none());
    let runtime = app.chat_runtime_mut(target).unwrap();
    finish_preview(runtime, &child, images::bounded_handle(bytes, &budget));
    assert!(runtime.child_cache[&child.id].image.is_some());
}
#[test]
fn explicit_preview_pressure_can_retire_automatic_owners_in_other_panes() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let owner = draft_chat(&mut app);
    let owner_target = chat_runtime(&app, owner).target;
    let requested_card = draft_chat(&mut app);
    let target = chat_runtime(&app, requested_card).target;
    let old = preview_item("old");
    let requested = preview_item("requested");
    let bytes = preview_png();
    let budget = images::Budget::new(1024, 4);
    let runtime = app.chat_runtime_mut(owner_target).unwrap();
    preview_items(runtime, std::slice::from_ref(&old));
    finish_preview(
        runtime,
        &old,
        images::bounded_handle(bytes.clone(), &budget),
    );
    preview_items(
        app.chat_runtime_mut(target).unwrap(),
        std::slice::from_ref(&requested),
    );
    drop(app.handle_chat_ui(Action::LoadImage(target, requested.id.clone())));
    let attempt = chat_runtime(&app, requested_card).cache[&requested.id].image_attempt;
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        requested.clone(),
        attempt,
        images::bounded_handle(bytes.clone(), &budget),
    )));
    assert!(chat_runtime(&app, owner).cache[&old.id].image.is_none());
    assert!(chat_runtime(&app, owner).cache[&old.id].image_evicted);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let pressure = runtime.cache[&requested.id].image_wait.clone().unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    executor.block_on(pressure.available());
    finish_preview(runtime, &requested, images::bounded_handle(bytes, &budget));
    assert!(runtime.cache[&requested.id].image.is_some());
}

#[test]
fn image_loading_only_subscribes_recent_visible_images_and_explicit_older_requests() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    let items: Vec<Arc<chat::Item>> = (0..20)
        .map(|index| {
            Arc::new(chat::Item {
                id: format!("image-{index}"),
                role: chat::Role::Assistant,
                content: ItemContent::Image {
                    path: format!("/image-{index}.png"),
                    description: "Image".into(),
                    reference: None,
                },
                started_at: None,
                at: None,
            })
        })
        .collect();
    for item in &items {
        let mut cached = CachedItem::new(&item.content, runtime.syntax_theme);
        cached.image_source = Some(item.clone());
        runtime.cache.insert(item.id.clone(), cached);
    }
    Arc::make_mut(&mut runtime.snapshot).items = items;
    runtime.visible.store(true, Ordering::Relaxed);
    assert_eq!(images::subscriptions(runtime).len(), 1);
    images::request(runtime, "image-0");
    assert_eq!(images::subscriptions(runtime).len(), 1);
    let child = Arc::new(chat::Item {
        id: "child-image".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Image {
            path: "/child.png".into(),
            description: "Child".into(),
            reference: None,
        },
        started_at: None,
        at: None,
    });
    let mut cached = CachedItem::new(&child.content, runtime.syntax_theme);
    cached.image_source = Some(child.clone());
    runtime.child_cache.insert(child.id.clone(), cached);
    Arc::make_mut(&mut runtime.snapshot)
        .subagents
        .push(chat::Subagent {
            model: None,
            waiting: false,
            detail: String::new(),
            id: "child".into(),
            kind: "fixture".into(),
            description: "Child".into(),
            status: chat::ToolStatus::Succeeded,
            background: false,
            items: vec![child],
        });
    // Hidden child transcripts and the explicit parent request do not
    // consume worker slots while another transcript is selected.
    assert_eq!(images::subscriptions(runtime).len(), 1);
    runtime.selected_subagent = Some("child".into());
    assert_eq!(images::subscriptions(runtime).len(), 1);
    runtime.selected_subagent = None;
    assert_eq!(images::subscriptions(runtime).len(), 1);
    runtime.visible.store(false, Ordering::Relaxed);
    assert!(images::subscriptions(runtime).is_empty());
}
fn followup(id: u64, text: &str, paths: &[&str]) -> Followup {
    Followup {
        id,
        text: text.into(),
        preview: prompt::queue_preview(text),
        attachments: paths
            .iter()
            .map(|path| Attachment {
                path: path.into(),
                reference: None,
            })
            .collect(),
    }
}

/// An image whose receipt names nothing, which no draft can save.
fn malformed_image(path: &str) -> Attachment {
    let path = PathBuf::from(path);
    Attachment {
        reference: Some(chat::ImageReference {
            id: uuid::Uuid::nil(),
            number: 0,
            device: uuid::Uuid::nil(),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            sha256: String::new(),
            extension: "png".into(),
        }),
        path,
    }
}

fn paths(attachments: &[Attachment]) -> Vec<&Path> {
    attachments
        .iter()
        .map(|attachment| attachment.path.as_path())
        .collect()
}

/// The send controls show only beside a draft, so the one a keyboard
/// user pressed is gone once the draft is sent. Its press hands the
/// keyboard back to the composer rather than to nothing, unless the draft
/// named a picker that takes the keyboard itself.
#[test]
fn a_pressed_send_control_returns_the_keyboard_to_the_composer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("send-control"));
    let composer = card_control_id(card, "chat-composer");
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = true;
    snapshot.capabilities.queue = false;
    snapshot.capabilities.permission_modes = vec!["manual".into(), "auto".into()];
    runtime.replace_draft(text_editor::Content::with_text("next task"));
    let target = runtime.target;
    let task = app.handle_chat_ui(Action::SubmitControl(target, PromptMode::Queue));
    assert_eq!(chat_runtime(&app, card).followups[0].text, "next task");
    assert!(chat_runtime(&app, card).composer.text().trim().is_empty());
    assert_eq!(task_focuses(task, &composer), (true, false));

    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("/permissions"));
    let task = app.handle_chat_ui(Action::SubmitControl(target, PromptMode::Queue));
    assert!(chat_runtime(&app, card).permission_picker);
    assert!(
        !task_focuses(task, &composer).0,
        "the permissions picker keeps the keyboard it was opened with"
    );
}

#[test]
fn a_busy_provider_without_native_queue_keeps_a_followup_editable() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
    runtime.replace_draft(text_editor::Content::with_text("next task"));
    let target = runtime.target;
    let _ = app.handle_chat_ui(Action::Submit(target, PromptMode::Queue));
    assert_eq!(chat_runtime(&app, card).followups[0].text, "next task");
    assert!(chat_runtime(&app, card).composer.text().trim().is_empty());
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("later draft"));
    let _ = app.handle_chat_ui(Action::TakeBackQueued(target, None));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.followups.is_empty());
    assert_eq!(runtime.composer.text().trim(), "next task\nlater draft");
    // The caret ends the message that came back, where revising it goes on.
    let caret = runtime.composer.cursor().position;
    assert_eq!((caret.line, caret.column), (0, "next task".len()));
}

#[test]
fn local_followups_stay_separate_and_are_taken_back_or_removed_one_at_a_time() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
    let target = runtime.target;
    for (text, paths) in [
        ("first task", vec!["first.png"]),
        ("second task", vec!["first.png", "second.png"]),
        ("", vec!["third.png"]),
    ] {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.replace_draft(text_editor::Content::with_text(text));
        runtime.attach(paths.into_iter().map(PathBuf::from));
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
    }
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime
            .followups
            .iter()
            .map(|followup| followup.text.as_str())
            .collect::<Vec<_>>(),
        ["first task", "second task", ""]
    );
    // They go to the provider as one prompt carrying each file once.
    let (text, attachments) = runtime.combined_followups();
    assert_eq!(text, "first task\nsecond task");
    assert_eq!(
        paths(&attachments),
        [
            Path::new("first.png"),
            Path::new("second.png"),
            Path::new("third.png")
        ]
    );
    assert!(runtime.commands.is_idle());
    assert!(runtime.composer.text().trim().is_empty());
    assert!(runtime.transition_blocker().is_some());
    assert!(runtime.handoff_blocker().is_some());
    let second = runtime.followups[1].id;
    let first = runtime.followups[0].id;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text("later draft"));
    runtime.attach([PathBuf::from("later.png")]);
    drop(app.handle_chat_ui(Action::TakeBackQueued(
        target,
        Some(QueueEntry::Local(second)),
    )));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text().trim(), "second task\nlater draft");
    assert_eq!(
        paths(&runtime.attachments),
        [
            Path::new("first.png"),
            Path::new("second.png"),
            Path::new("later.png")
        ]
    );
    assert_eq!(runtime.followups.len(), 2);
    drop(app.handle_chat_ui(Action::RemoveQueued(target, QueueEntry::Local(first))));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.followups.len(), 1);
    assert_eq!(runtime.composer.text().trim(), "second task\nlater draft");
    // A message of files alone brings its files back and no text.
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.followups.is_empty());
    assert_eq!(runtime.composer.text().trim(), "second task\nlater draft");
    assert_eq!(runtime.attachments.len(), 4);
    assert_eq!(runtime.attachments[0].path, Path::new("third.png"));
    // The files returned to the composer are saved with its draft.
    assert!(runtime.transition_blocker().is_none());
    assert_eq!(runtime.persistent_draft().1, runtime.attachments);
}

#[test]
fn taking_a_message_back_keeps_the_draft_a_history_walk_set_aside() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.remember("sent earlier".into());
    runtime.followups.push(followup(0, "waiting", &[]));
    runtime.replace_draft(text_editor::Content::with_text("unsent"));
    runtime.recall(true);
    assert_eq!(runtime.composer.text().trim(), "sent earlier");
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text().trim(), "waiting\nunsent");
    assert!(runtime.history_cursor.is_none());
}

#[test]
fn a_local_followup_resumes_when_idle_arrives_before_an_unrelated_ack() {
    for refreshing in [false, true] {
        for result in [Ok(()), Err("fixture refused selection".into())] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, Some("followup"));
            let target = chat_runtime(&app, card).target;
            let runtime = app.chat_runtime_mut(target).unwrap();
            Arc::make_mut(&mut runtime.snapshot).busy = true;
            Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
            runtime.replace_draft(text_editor::Content::with_text("next task"));
            drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
            drop(app.chat_send(
                target,
                if refreshing {
                    Command::Refresh
                } else {
                    Command::SetPermissionMode {
                        mode: "manual".into(),
                    }
                },
                false,
            ));
            let id = chat_runtime(&app, card).commands.in_flight().unwrap().id;
            drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
            assert_eq!(
                chat_runtime(&app, card).commands.in_flight().unwrap().id,
                id
            );
            settle(&mut app, card, result);
            let pending = chat_runtime(&app, card).commands.in_flight().unwrap();
            assert_ne!(pending.id, id);
            assert!(!pending.followups.is_empty());
            assert_eq!(pending.sent.as_deref().unwrap().trim(), "next task");
            settle(&mut app, card, Ok(()));
            assert!(chat_runtime(&app, card).followups.is_empty());
        }
    }
}

#[test]
fn a_combined_followup_is_sent_as_one_prompt_and_keeps_the_new_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target).unwrap().show(true);
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
    Arc::make_mut(&mut runtime.snapshot).capabilities.compact = true;
    for text in ["/compact", "also include the second task"] {
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text(text));
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
    }
    drop(app.dispatch_queued(target));
    let runtime = app.chat_runtime_mut(target).unwrap();
    let pending = runtime.commands.in_flight().unwrap();
    assert!(!pending.followups.is_empty());
    assert_eq!(
        pending.sent.as_deref().unwrap().trim(),
        "/compact\nalso include the second task"
    );
    runtime.replace_draft(text_editor::Content::with_text("later draft"));
    settle(&mut app, card, Ok(()));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.followups.is_empty());
    assert_eq!(runtime.composer.text().trim(), "later draft");
    assert_eq!(
        runtime.history.last().unwrap().trim(),
        "/compact\nalso include the second task"
    );
}

#[test]
fn a_rejected_combined_followup_restores_order_without_consuming_an_inflight_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.followups = vec![
        followup(0, "first", &["first.png"]),
        followup(1, "second", &[]),
    ];
    runtime.next_written = 2;
    drop(app.dispatch_queued(target));
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text("later"));
    runtime.attach([PathBuf::from("later.png")]);
    // Written while the others are on their way, it waits behind them.
    drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
    assert_eq!(chat_runtime(&app, card).followups.len(), 3);
    assert!(chat_runtime(&app, card).composer.text().trim().is_empty());
    for action in [
        Action::TakeBackQueued(target, Some(QueueEntry::Local(0))),
        Action::RemoveQueued(target, QueueEntry::Local(1)),
    ] {
        drop(app.handle_chat_ui(action));
        assert_eq!(
            chat_runtime(&app, card).followups.len(),
            3,
            "an in-flight send keeps what it is carrying"
        );
    }
    // The one not yet on its way is still the reader's to take back.
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.followups.len(), 2);
    assert_eq!(runtime.composer.text().trim(), "later");
    settle(&mut app, card, Err("fixture rejected".into()));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text().trim(), "first\nsecond\nlater");
    assert_eq!(
        paths(&runtime.attachments),
        [Path::new("first.png"), Path::new("later.png")]
    );
    assert!(runtime.followups.is_empty());
}

/// A follow-up written while the held ones are on their way is not sent
/// with them, so it stays held once they are taken, and waits for the
/// turn they start rather than going out beside them.
#[test]
fn a_followup_written_while_others_are_sent_waits_for_the_next_turn() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.followups = vec![followup(0, "first", &[])];
    runtime.next_written = 1;
    drop(app.dispatch_queued(target));
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(runtime.turn_under_way(), "the held follow-up starts a turn");
    runtime.replace_draft(text_editor::Content::with_text("second"));
    drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
    settle(&mut app, card, Ok(()));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime
            .followups
            .iter()
            .map(|followup| followup.text.as_str())
            .collect::<Vec<_>>(),
        ["second"]
    );
    assert!(runtime.commands.is_idle(), "nothing more is sent yet");
    assert!(runtime.turn_under_way(), "until the first shows");
    let local = runtime.local_prompt().expect("the sent follow-up shows");
    assert_eq!(local.text(), "first");
}

#[test]
fn combined_followup_attachment_limit_keeps_both_messages_editable() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.session.launch.provider = Provider::Claude;
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
    let attachments = (0..8)
        .map(|index| Attachment {
            path: format!("{index}.png").into(),
            reference: None,
        })
        .collect::<Vec<_>>();
    runtime.followups = vec![Followup {
        id: 0,
        text: "first".into(),
        preview: "first".into(),
        attachments: attachments.clone(),
    }];
    runtime.replace_draft(text_editor::Content::with_text("second"));
    runtime.attach([PathBuf::from("extra.png")]);
    drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.followups.len(), 1);
    assert_eq!(runtime.followups[0].attachments, attachments);
    assert_eq!(runtime.composer.text().trim(), "second");
    assert_eq!(
        runtime.attachments,
        [Attachment {
            path: "extra.png".into(),
            reference: None,
        }]
    );
    assert!(runtime.error.as_ref().unwrap().contains("attachment limit"));
}

#[test]
fn sending_a_local_followup_preserves_the_later_composer_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.followups.push(followup(0, "queued task", &[]));
    runtime.replace_draft(text_editor::Content::with_text("later draft"));
    let _ = app.dispatch_queued(target);
    assert!(
        chat_runtime(&app, card)
            .commands
            .in_flight()
            .unwrap()
            .followups
            .contains(&0)
    );
    settle(&mut app, card, Ok(()));
    assert!(chat_runtime(&app, card).followups.is_empty());
    assert_eq!(
        chat_runtime(&app, card).composer.text().trim(),
        "later draft"
    );
}
fn parsed_codes(cached: &super::CachedItem) -> Vec<String> {
    let mut codes = Vec::new();
    super::for_each_fence(cached.markdown.items(), &mut |_, code| {
        codes.push(code.to_owned())
    });
    codes
}
use super::*;

#[test]
fn new_chat_pins_selected_home_credential_policy_and_system_resolves_the_default() {
    for provider in Account::PROVIDERS {
        for account in [
            Account::system(provider),
            Account::managed(provider, uuid::Uuid::new_v4(), "/selected".into()),
            Account::linked(provider, uuid::Uuid::new_v4(), "/linked".into()),
        ] {
            let session = fresh_chat_session(
                provider,
                TerminalId::new(),
                "/project".into(),
                &account,
                false,
            )
            .unwrap();
            let mut expected: std::collections::BTreeSet<String> = account
                .credential_overrides()
                .iter()
                .map(|name| (*name).to_owned())
                .collect();
            // The system Claude login resolves the CLI's default home
            // rather than one the spawning process inherited.
            if account.is_system()
                && provider == Provider::Claude
                && account.launch_home().is_none()
            {
                expected.insert("CLAUDE_CONFIG_DIR".to_owned());
                assert!(
                    !session
                        .launch
                        .recipe
                        .environment
                        .contains_key("CLAUDE_CONFIG_DIR")
                );
            }
            assert_eq!(session.launch.recipe.unset_environment, expected);
            for name in account.credential_overrides() {
                assert!(!session.launch.recipe.environment.contains_key(*name));
            }
            if !account.is_system() {
                let home = account.resolved_home().unwrap();
                assert_eq!(
                    session.launch.recipe.environment[account::home_variable(provider).unwrap()],
                    home.to_str().unwrap()
                );
            }
            let returned = local_return_launch(&session).unwrap();
            assert_eq!(returned.program, provider.executable());
            assert_eq!(
                returned.working_directory,
                session.launch.recipe.working_directory
            );
            assert_eq!(returned.environment, session.launch.recipe.environment);
            assert!(!returned.arguments.iter().any(|arg| arg == "unix://"));
            assert_eq!(
                returned.unset_environment,
                session.launch.recipe.unset_environment
            );
        }
    }
}

#[test]
fn resumed_and_cli_paired_snapshots_supply_channel_names() {
    let mut snapshot = Snapshot::default();
    assert_eq!(snapshot_channel_name(&snapshot), None);
    snapshot.items.push(
        chat::Item {
            id: "prompt".into(),
            role: chat::Role::User,
            content: text("Implement the requested feature"),
            started_at: None,
            at: None,
        }
        .into(),
    );
    assert_eq!(
        snapshot_channel_name(&snapshot),
        Some((
            "Implement the requested feature".into(),
            crate::model::ChannelTitleSource::Prompt
        ))
    );
    snapshot.title = Some("Provider conversation name".into());
    assert_eq!(
        snapshot_channel_name(&snapshot),
        Some((
            "Provider conversation name".into(),
            crate::model::ChannelTitleSource::Provider
        ))
    );
    for title in ["1341", "🔐", "🧑‍💻"] {
        snapshot.title = Some(title.into());
        assert_eq!(
            snapshot_channel_name(&snapshot),
            Some((title.into(), crate::model::ChannelTitleSource::Provider))
        );
    }
    snapshot.title = Some("\u{202e}\u{2066}".into());
    assert_eq!(
        snapshot_channel_name(&snapshot).unwrap().0,
        "Implement the requested feature"
    );
}

#[test]
fn automatic_channel_names_observe_metadata_without_replaying_old_snapshots() {
    let mut observation = ChannelNameObservation::default();
    let mut snapshot = Snapshot {
        session_id: Some("session-a".into()),
        ..Default::default()
    };
    assert!(observation.observe(&snapshot).is_none());
    snapshot.items.push(Arc::new(chat::Item {
        id: "first-task".into(),
        role: chat::Role::User,
        content: text("Review bundle encryption and performance"),
        started_at: None,
        at: None,
    }));
    assert_eq!(
        observation.observe(&snapshot).unwrap().0,
        "Review bundle encryption and performance"
    );
    observation.pending = false;
    // Streaming assistant changes and newer shared-model reads must not
    // re-publish an older name over a newer name from another window.
    snapshot.generation += 1;
    assert!(observation.observe(&snapshot).is_none());
    snapshot.title = Some("Bundle encryption audit".into());
    assert_eq!(
        observation.observe(&snapshot).unwrap().1,
        crate::model::ChannelTitleSource::Provider
    );
    observation.pending = false;
    assert!(observation.observe(&snapshot).is_none());
    snapshot.title = Some("Encryption and throughput".into());
    assert_eq!(
        observation.observe(&snapshot).unwrap().0,
        "Encryption and throughput"
    );
    snapshot.session_id = Some("successor-session".into());
    snapshot.title = None;
    snapshot.items.clear();
    assert!(observation.observe(&snapshot).is_none());
    assert!(observation.title.is_none());
}

/// A streaming answer changes only its conversation's transcript, so its
/// wakes and the background work they start skip the workspace
/// projection; a wake that changes what the sidebar shows still takes it.
#[test]
fn streaming_wakes_skip_the_workspace_projection() {
    use crate::agent_execution::directory::UsageLimit;
    use crate::app::terminal_output_tests::projections;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let mut answer = String::new();
    let mut stream = |app: &mut Cagents, delta: &str| {
        answer.push_str(delta);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.items.retain(|item| item.id != "answer");
        snapshot.items.push(
            chat::Item {
                id: "answer".into(),
                role: chat::Role::Assistant,
                content: text(&answer),
                started_at: None,
                at: None,
            }
            .into(),
        );
        handle.publish_snapshot_for_test(snapshot);
        drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
    };
    // The first wake finishes attaching the conversation.
    stream(&mut app, "Streaming");
    let before = projections();
    for delta in [" answers", " arrive", " in", " small", " deltas."] {
        stream(&mut app, delta);
        drop(app.update(Message::ChatUi(Action::FlowAvailable)));
        drop(app.update(Message::ChatUi(Action::PreparationAvailable)));
        drop(app.update(Message::ChatUi(Action::Tick)));
    }
    // Typing alongside the stream stays in the conversation too.
    for character in "next".chars() {
        drop(app.update(Message::ChatUi(Action::Composer(
            target,
            text_editor::Action::Edit(text_editor::Edit::Insert(character)),
        ))));
    }
    assert_eq!(chat_runtime(&app, card).composer.text(), "next");
    assert_eq!(projections(), before);
    // Nothing the skipped projections would have shown is missing.
    let sidebar = app.sidebar_model.clone();
    app.refresh_sidebar_channels();
    app.refresh_sidebar_worktree_panes();
    assert_eq!(app.sidebar_model, sidebar);
    // A pane name the provider reports reaches the tab through the
    // chrome alone.
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.title = Some("Renamed by the provider".into());
    handle.publish_snapshot_for_test(snapshot);
    drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
    assert_eq!(
        app.card_state(card).unwrap().title,
        "Renamed by the provider"
    );
    assert!(
        app.card_chrome[&card]
            .tooltip
            .contains("Renamed by the provider")
    );
    assert_eq!(projections(), before);
    // Every delta still reached the conversation.
    assert_eq!(
        chat_runtime(&app, card).snapshot.generation,
        handle.snapshot().generation
    );

    handle.judge_limit_for_test(Some(UsageLimit {
        summary: "Claude weekly limit reached".into(),
        reset: None,
        resets_at: None,
    }));
    drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Limit))));
    assert_eq!(projections(), before + 1);
    assert_eq!(
        app.sidebar_state.card_attention(card).state(),
        Some(crate::sidebar::AttentionState::Limited)
    );
}

#[test]
fn chat_actions_name_their_variant_without_their_payload() {
    let action = Action::OpenUrl("https://example.com/?token=secret".into());
    assert_eq!(format!("{action:?}"), "OpenUrl");
    assert_eq!(format!("{:?}", Action::FlowAvailable), "FlowAvailable");
}

/// A message card names the agent it reached by the address the daemon
/// says the provider gave it, which arrives apart from the transcript.
#[test]
fn a_message_card_resolves_its_recipient_once_the_daemon_says_how() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents.push(chat::Subagent {
        id: "toolu_relocate".into(),
        kind: "general-purpose".into(),
        description: "Relocate the repo".into(),
        status: chat::ToolStatus::Running,
        model: None,
        waiting: false,
        detail: String::new(),
        background: true,
        items: Vec::new(),
    });
    handle.publish_snapshot_for_test(snapshot);
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    let agent = "a1892b93e88999a7d";
    assert_eq!(chat_runtime(&app, card).listed_agent_name(agent), None);
    handle.learn_agent_addresses_for_test(vec![chat::AgentAddress {
        address: agent.into(),
        subagent: "toolu_relocate".into(),
    }]);
    drop(app.handle_chat_ui(Action::Wake(target, Wake::AgentAddresses)));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.listed_agent_name(agent).as_deref(),
        runtime.listed_agent_name("toolu_relocate").as_deref()
    );
    assert!(runtime.listed_agent_name(agent).is_some());
}

#[test]
fn a_usage_limit_the_daemon_judges_reads_limited_until_it_lifts_even_unseen() {
    use crate::agent_execution::directory::UsageLimit;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let failure = "You've hit your weekly limit · resets Nov 8, 8am (America/Vancouver)";
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.error = Some(failure.into());
    handle.publish_snapshot_for_test(snapshot);
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    // The provider's words alone do not make it limited: the daemon,
    // which knows the account's readings, judges that.
    assert_eq!(app.sidebar_state.card_attention(card).limited, 0);
    assert_eq!(
        chat_runtime(&app, card).conversation_status(false).1,
        "Account limit reached"
    );
    let judge = |app: &mut Cagents, limit: Option<UsageLimit>| {
        handle.judge_limit_for_test(limit);
        // Without a transcript read, as a hidden conversation hears of it.
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Limit)));
        app.sidebar_state.card_attention(card)
    };
    let limited = judge(
        &mut app,
        Some(UsageLimit {
            summary: "Claude weekly limit reached".into(),
            reset: Some("Nov 8, 8am (America/Vancouver)".into()),
            resets_at: None,
        }),
    );
    assert_eq!(
        limited.state(),
        Some(crate::sidebar::AttentionState::Limited)
    );
    assert_eq!(limited.working, 0);
    assert_eq!(
        chat_runtime(&app, card).conversation_status(false).1,
        "Account limit reached · resets Nov 8, 8am (America/Vancouver)"
    );
    // Once the reset passes, the failure is still the last outcome, but
    // nothing stops the conversation.
    assert_eq!(judge(&mut app, None).limited, 0);
    assert_eq!(
        chat_runtime(&app, card).conversation_status(false).1,
        "Account limit reached"
    );
}

#[test]
fn tail_only_history_cannot_replace_a_channels_first_task() {
    use crate::model::{ChannelId, ChannelTitleSource, ProjectChannel, ProjectId};
    use crate::workspace_model::views::{Pane, Scope, View};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let id = ChannelId::new();
    Arc::make_mut(&mut app.durable).channels = vec![ProjectChannel {
        id,
        project_id: ProjectId::new(),
        title: "Review original encryption task".into(),
        title_source: ChannelTitleSource::Prompt,
        ordinal: 0,
        shared: true,
        cards: vec![card],
        focused: Some(card),
    }];
    app.workspace_model.views = vec![View {
        scope: Scope::Channel(id),
        revision: 1,
        retired: Vec::new(),
        panes: vec![
            Pane::from_card(
                app.card_state(card).unwrap(),
                Some(FIXTURE_DIRECTORY.into()),
            )
            .unwrap(),
        ],
    }];
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.title = None;
    snapshot.has_earlier = true;
    snapshot.items = vec![Arc::new(chat::Item {
        id: "visible-window-edge".into(),
        role: chat::Role::User,
        content: text("Fix a later unrelated task"),
        started_at: None,
        at: None,
    })];
    handle.publish_snapshot_for_test(snapshot.clone());
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    assert_eq!(
        app.durable.channels[0].title,
        "Review original encryption task"
    );
    assert!(app.chat_ui.channel_names[&target].title.is_none());
    // A confirmed provider name does not require the first history page.
    let mut provider = snapshot.clone();
    provider.title = Some("Provider encryption audit".into());
    assert_eq!(
        snapshot_channel_name(&provider).unwrap(),
        (
            "Provider encryption audit".into(),
            ChannelTitleSource::Provider
        )
    );
    // The same edge can become a complete head after a provider/history
    // observation; neither a new turn nor another item identity is needed.
    snapshot.has_earlier = false;
    let observed = app.observe_channel_name(target, &snapshot).unwrap();
    assert_eq!(
        observed,
        (
            "Fix a later unrelated task".into(),
            ChannelTitleSource::Prompt
        )
    );
    // Unavailable content is also never an instruction to erase a manual name.
    let channel = &mut Arc::make_mut(&mut app.durable).channels[0];
    channel.title = "My manual channel".into();
    channel.title_source = ChannelTitleSource::Manual;
    snapshot.has_earlier = true;
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    assert_eq!(app.durable.channels[0].title, "My manual channel");
}

fn text(body: &str) -> ItemContent {
    ItemContent::Text {
        text: body.to_owned(),
        thinking: false,
        phase: chat::Phase::Final,
    }
}

#[test]
fn hidden_children_prepare_no_transcript_cache_until_their_tab_is_selected() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("lazy-child-cache"));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let children = (0..5)
        .map(|index| chat::Subagent {
            id: format!("child-{index}"),
            kind: "fixture".into(),
            description: "Lazy child".into(),
            status: chat::ToolStatus::Running,
            background: true,
            model: None,
            waiting: false,
            detail: String::new(),
            items: vec![Arc::new(chat::Item {
                id: format!("body-{index}"),
                role: chat::Role::Assistant,
                content: text(&"source ".repeat(20000)),
                started_at: None,
                at: None,
            })],
        })
        .collect();
    app.chat_runtime_mut(target).unwrap().show(true);
    handle.publish_snapshot_for_test(Snapshot {
        generation: 20,
        ready: true,
        subagents: children,
        items: vec![Arc::new(chat::Item {
            id: "body-2".into(),
            role: chat::Role::Assistant,
            content: text("Parent source"),
            started_at: None,
            at: None,
        })],
        ..Snapshot::default()
    });
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    preparation::drain_fixture(runtime);
    assert_eq!(
        runtime.cache.len(),
        1,
        "only parent source is prepared before child selection"
    );
    assert!(!runtime.cache["body-2"].markdown.items().is_empty());
    for selected in [Some("child-2"), Some("child-3"), None] {
        drop(app.handle_chat_ui(Action::SelectSubagent(target, selected.map(str::to_owned))));
        let runtime = app.chat_runtime_mut(target).unwrap();
        preparation::drain_fixture(runtime);
        assert_eq!(runtime.cache.len(), 1);
        assert!(
            matches!(&runtime.cache["body-2"].item.as_ref().unwrap().content,
            ItemContent::Text { text, .. } if text == "Parent source"),
            "colliding child IDs never replace the parent's source"
        );
        if let Some(selected) = selected {
            let id = selected.replace("child", "body");
            assert_eq!(runtime.child_cache.len(), 1);
            assert!(
                matches!(&runtime.child_cache[&id].item.as_ref().unwrap().content,
                ItemContent::Text { text, .. } if text.len() > 10000)
            );
            assert!(!runtime.child_cache[&id].placeholder);
        } else {
            assert!(runtime.child_cache.is_empty());
        }
    }
}

#[test]
fn streaming_children_keep_only_their_newest_fence_live_after_parent_completion() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("streaming-children"));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let item = |id: &str, body: &str| {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::Assistant,
            content: text(body),
            started_at: None,
            at: None,
        })
    };
    let live = |runtime: &Runtime, id: &str| {
        runtime
            .preparation
            .growing_canonical_for_test(runtime.active_item(id).unwrap())
            .is_some()
    };
    let body = "```rust\nfn first() {}\n";
    let mut snapshot = Snapshot {
        generation: 10,
        session_id: Some("streaming-children".into()),
        ready: true,
        busy: false,
        items: vec![item("parent", body)],
        subagents: vec![
            chat::Subagent {
                model: None,
                waiting: false,
                detail: String::new(),
                id: "running-child".into(),
                kind: "task".into(),
                description: String::new(),
                status: chat::ToolStatus::Running,
                background: true,
                items: vec![item("child-first", body)],
            },
            chat::Subagent {
                model: None,
                waiting: false,
                detail: String::new(),
                id: "finished-child".into(),
                kind: "task".into(),
                description: String::new(),
                status: chat::ToolStatus::Succeeded,
                background: true,
                items: vec![item("finished", body)],
            },
        ],
        ..Snapshot::default()
    };
    let publish = |app: &mut Cagents, snapshot: &Snapshot| {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.show(true);
        handle.publish_delta_snapshot_for_test(snapshot.clone());
        runtime.refresh();
        preparation::drain_fixture(runtime);
    };
    publish(&mut app, &snapshot);
    assert!(chat_runtime(&app, card).child_cache.is_empty());
    drop(app.handle_chat_ui(Action::SelectSubagent(
        target,
        Some("finished-child".into()),
    )));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    assert!(
        chat_runtime(&app, card).child_cache["finished"]
            .open
            .is_none()
    );
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("running-child".into()))));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    let runtime = chat_runtime(&app, card);
    assert!(
        live(runtime, "child-first"),
        "an idle parent must not retire its running child's fence"
    );
    let parser_work = runtime
        .preparation
        .growing_canonical_for_test(runtime.active_item("child-first").unwrap())
        .unwrap();
    assert!(runtime.cache["parent"].open.is_none());
    assert!(!runtime.child_cache.contains_key("finished"));
    // An unchanged snapshot must also preserve the child's continuation.
    snapshot.generation += 1;
    publish(&mut app, &snapshot);
    assert!(live(chat_runtime(&app, card), "child-first"));
    let longer = format!("{body}fn second() {{}}\n");
    snapshot.subagents[0].items[0] = item("child-first", &longer);
    snapshot.generation += 1;
    publish(&mut app, &snapshot);
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime
            .preparation
            .growing_canonical_for_test(runtime.active_item("child-first").unwrap()),
        Some(parser_work),
        "verified child append continues the actual retained Markdown fence without reparsing it"
    );
    let expected = CachedItem::new(&text(&longer), runtime.syntax_theme);
    let cached = &chat_runtime(&app, card).child_cache["child-first"];
    assert_eq!(parsed_codes(cached), parsed_codes(&expected));
    for (key, block) in &expected.blocks {
        for dark in [false, true] {
            assert_eq!(cached.blocks[key].highlight(dark), block.highlight(dark));
        }
    }
    // A new child item ends the previous item's continuation, even while
    // both the child and its parent are running.
    snapshot.busy = true;
    snapshot.items.push(item("parent-new", body));
    snapshot.subagents[0].items.push(item("child-new", body));
    snapshot.generation += 1;
    publish(&mut app, &snapshot);
    let runtime = chat_runtime(&app, card);
    assert!(runtime.child_cache["child-first"].open.is_none());
    assert!(!runtime.preparation.contains("child-first"));
    assert!(live(runtime, "child-new"));
    assert!(runtime.cache["parent"].open.is_none());
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    assert!(live(chat_runtime(&app, card), "parent-new"));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("running-child".into()))));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    // Each terminal status retires an unchanged item. A resumed child
    // with the same item ID can subsequently append without losing text.
    for (index, status) in [
        chat::ToolStatus::Succeeded,
        chat::ToolStatus::Stopped,
        chat::ToolStatus::Failed,
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("child-ending-{index}");
        snapshot.subagents[0].status = chat::ToolStatus::Running;
        snapshot.subagents[0].items.push(item(&id, body));
        snapshot.generation += 1;
        publish(&mut app, &snapshot);
        assert!(live(chat_runtime(&app, card), &id));
        snapshot.subagents[0].status = status;
        snapshot.generation += 1;
        publish(&mut app, &snapshot);
        let runtime = chat_runtime(&app, card);
        assert!(runtime.child_cache[&id].open.is_none());
        assert!(!runtime.preparation.contains(&id));
        assert!(
            !runtime.preparation.contains("parent-new"),
            "inactive parent continuation does not own the selected child's parser"
        );
        snapshot.subagents[0].status = chat::ToolStatus::Running;
        snapshot.generation += 1;
        publish(&mut app, &snapshot);
        *snapshot.subagents[0].items.last_mut().unwrap() = item(&id, &longer);
        snapshot.generation += 1;
        publish(&mut app, &snapshot);
        let cached = &chat_runtime(&app, card).child_cache[&id];
        assert!(live(chat_runtime(&app, card), &id));
        assert_eq!(parsed_codes(cached), parsed_codes(&expected));
        for (key, block) in &expected.blocks {
            for dark in [false, true] {
                assert_eq!(cached.blocks[key].highlight(dark), block.highlight(dark));
            }
        }
    }
    // A tool can become newest while its preceding answer's fence is
    // still open. Being in a running child does not keep old text live.
    snapshot.subagents[0].items.push(Arc::new(chat::Item {
        id: "child-tool".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "fixture".into(),
            input: "{}".into(),
            output: String::new(),
            status: chat::ToolStatus::Running,
        },
        started_at: None,
        at: None,
    }));
    snapshot.generation += 1;
    publish(&mut app, &snapshot);
    let runtime = chat_runtime(&app, card);
    assert!(runtime.child_cache["child-ending-2"].open.is_none());
    assert!(!runtime.preparation.contains("child-ending-2"));
}

#[test]
fn ending_a_child_item_flushes_its_provisional_markdown_tail() {
    for another_item in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("child-tail"));
        let target = chat_runtime(&app, card).target;
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        let child_item = Arc::new(chat::Item {
            id: "unfinished".into(),
            role: chat::Role::Assistant,
            content: text("Use <Type"),
            started_at: None,
            at: None,
        });
        let mut snapshot = Snapshot {
            generation: 10,
            ready: true,
            subagents: vec![chat::Subagent {
                model: None,
                waiting: false,
                detail: String::new(),
                id: "child".into(),
                kind: "task".into(),
                description: String::new(),
                status: chat::ToolStatus::Running,
                background: true,
                items: vec![child_item.clone()],
            }],
            ..Snapshot::default()
        };
        let publish = |app: &mut Cagents, snapshot: &Snapshot| {
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.show(true);
            handle.publish_snapshot_for_test(snapshot.clone());
            runtime.refresh();
            preparation::drain_fixture(runtime);
        };
        app.chat_runtime_mut(target).unwrap().selected_subagent = Some("child".into());
        publish(&mut app, &snapshot);
        drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
        assert!(
            chat_runtime(&app, card).child_cache["unfinished"]
                .source
                .provisional()
        );
        if another_item {
            let mut next = (*child_item).clone();
            next.id = "next".into();
            next.content = text("Next answer");
            snapshot.subagents[0].items.push(Arc::new(next));
        } else {
            snapshot.subagents[0].status = chat::ToolStatus::Succeeded;
        }
        snapshot.generation += 1;
        publish(&mut app, &snapshot);
        let cached = &chat_runtime(&app, card).child_cache["unfinished"];
        assert!(!cached.source.provisional());
        let expected = CachedItem::new(&child_item.content, chat_runtime(&app, card).syntax_theme);
        assert_eq!(shown(cached), shown(&expected));
        assert!(shown(cached).contains("<Type"));
    }
}

#[test]
fn a_short_historical_page_can_explicitly_return_to_latest() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("history"));
    let target = chat_runtime(&app, card).target;
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = true;
    let _ = app.handle_chat_ui(Action::Latest(target));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Latest)
    );
}

pub(super) fn history_fixture(app: &mut Cagents) -> (CardId, Target, Arc<ClientHandle>) {
    history_fixture_for_provider(app, Provider::Codex)
}
fn history_fixture_for_provider(
    app: &mut Cagents,
    provider: Provider,
) -> (CardId, Target, Arc<ClientHandle>) {
    let card = live_chat_for_provider(app, provider, Some(KNOWN_THREAD));
    let runtime = app
        .chat_runtime_mut(chat_runtime(app, card).target)
        .unwrap();
    runtime.show(true);
    let target = runtime.target;
    let handle = runtime.handle().unwrap().clone();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.has_earlier = true;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    (card, target, handle)
}

/// A chat whose provider holds `messages` and lets them be removed.
fn provider_queue(
    app: &mut Cagents,
    messages: Vec<chat::QueuedMessage>,
) -> (CardId, Target, Arc<ClientHandle>) {
    let (card, target, handle) = history_fixture(app);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.capabilities.queue_remove = true;
    snapshot.queued_messages = messages;
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    (card, target, handle)
}

fn provider_message(id: &str, text: &str) -> chat::QueuedMessage {
    chat::QueuedMessage::new(id.into(), text.into())
}

/// Publishes the provider's queue with `id` gone from it and, where
/// `removed` says, its answer to the request to remove it.
fn provider_drops(
    app: &mut Cagents,
    target: Target,
    handle: &ClientHandle,
    id: &str,
    removed: Option<bool>,
) {
    let mut snapshot = (*handle.snapshot()).clone();
    if removed != Some(false) {
        snapshot.queued_messages.retain(|message| message.id != id);
    }
    if let Some(removed) = removed {
        snapshot.queue_removals.push(chat::QueueRemoval {
            id: id.into(),
            removed,
        });
    }
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
}

#[test]
fn a_provider_queued_message_returns_to_the_composer_once_the_provider_lets_go() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut newest = provider_message("newest", "revise me");
    newest.attachments = vec![Attachment {
        path: "queued.png".into(),
        reference: None,
    }];
    let (card, target, handle) =
        provider_queue(&mut app, vec![provider_message("older", "stay"), newest]);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text("typed since"));
    assert_eq!(
        runtime.newest_take_back(),
        Some(QueueEntry::Provider("newest".into()))
    );
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    // The command being accepted says only that the request was written.
    settle(&mut app, card, Ok(()));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.is_recalling("newest"));
    assert_eq!(
        runtime.composer.text().trim(),
        "typed since",
        "nothing returns before the provider has let go of it"
    );
    // The next press reaches past the one already on its way back.
    assert_eq!(
        runtime.newest_take_back(),
        Some(QueueEntry::Provider("older".into()))
    );
    provider_drops(&mut app, target, &handle, "newest", Some(true));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text().trim(), "revise me\ntyped since");
    assert_eq!(paths(&runtime.attachments), [Path::new("queued.png")]);
    assert!(runtime.recalling.is_empty() && runtime.taken_back.is_empty());
    // Sent again, it waits for the next turn as it did before.
    assert_eq!(runtime.resend, Some(PromptMode::Queue));
}

#[test]
fn a_message_the_agent_started_on_is_not_taken_back() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = provider_queue(
        &mut app,
        vec![provider_message("queued", "already running")],
    );
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("draft"));
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    settle(&mut app, card, Ok(()));
    // The message left the queue because it was delivered, not removed.
    provider_drops(&mut app, target, &handle, "queued", None);
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text().trim(), "draft");
    assert!(runtime.recalling.is_empty());
    assert!(runtime.error.as_ref().unwrap().contains("already started"));
}

#[test]
fn a_refused_take_back_leaves_the_message_waiting_and_the_composer_alone() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) =
        provider_queue(&mut app, vec![provider_message("queued", "still waiting")]);
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("draft"));
    // Refused before it reached the provider.
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    settle(&mut app, card, Err("fixture refused".into()));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.recalling.is_empty());
    assert!(runtime.error.as_ref().unwrap().contains("fixture refused"));
    // Refused by the provider: the entry is still listed, and offered.
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    settle(&mut app, card, Ok(()));
    provider_drops(&mut app, target, &handle, "queued", Some(false));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.recalling.is_empty());
    assert_eq!(runtime.composer.text().trim(), "draft");
    assert_eq!(
        runtime.newest_take_back(),
        Some(QueueEntry::Provider("queued".into()))
    );
    // That earlier answer does not settle a second attempt.
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    settle(&mut app, card, Ok(()));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert!(chat_runtime(&app, card).is_recalling("queued"));
    provider_drops(&mut app, target, &handle, "queued", Some(true));
    assert_eq!(
        chat_runtime(&app, card).composer.text().trim(),
        "still waiting\ndraft"
    );
}

#[test]
fn only_a_whole_removable_message_can_be_taken_back() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut partial = provider_message("partial", "preview only");
    partial.editable = false;
    let (card, target, handle) = provider_queue(&mut app, vec![partial]);
    drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
    drop(app.handle_chat_ui(Action::TakeBackQueued(
        target,
        Some(QueueEntry::Provider("partial".into())),
    )));
    assert!(chat_runtime(&app, card).commands.is_idle());
    // A provider that cannot remove a message cannot give it back either.
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.capabilities.queue_remove = false;
    snapshot.queued_messages = vec![provider_message("whole", "text")];
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert_eq!(chat_runtime(&app, card).newest_take_back(), None);
}

#[test]
fn upgrade_gate_requires_all_hundred_saved_panes_and_preserves_their_composers() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut cards = Vec::new();
    for index in 0..100 {
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text(&format!("Draft {index}")));
        cards.push((card, target));
    }
    assert_eq!(app.startup_pane_check(), Ok(true));
    let (card, target) = cards[99];
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.disconnected = true;
    runtime.error = Some("Upgrade replacement changed configuration".into());
    let reason = app.startup_pane_check().unwrap_err();
    let title = app.card_display_title(app.card_state(card).unwrap());
    assert!(
        reason.starts_with(&format!(
            "recovery-incomplete: 1 saved pane could not reconnect: \u{201c}{title}\u{201d}."
        )),
        "{reason}"
    );
    assert_eq!(chat_runtime(&app, card).composer.text().trim(), "Draft 99");
    app.chat_runtime_mut(target).unwrap().disconnected = false;
    assert_eq!(app.startup_pane_check(), Ok(true));
}

#[test]
fn upgrade_gate_requires_every_live_terminal_without_provider_activity_heuristics() {
    for program in ["omp", "my-agent-wrapper", "bash"] {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = app.durable.cards[0].id;
        let CardKind::Terminal(session) = &mut Arc::make_mut(&mut app.durable).cards[0].kind else {
            unreachable!()
        };
        session.launch.program = program.into();
        assert_eq!(
            app.startup_pane_check(),
            Ok(false),
            "{program} is still loading"
        );
        app.cards.insert(
            card,
            CardRuntime::Failed("Fixture terminal failed to attach".into()),
        );
        let reason = app.startup_pane_check().unwrap_err();
        let title = app.card_display_title(app.card_state(card).unwrap());
        assert!(
            reason.starts_with(&format!(
                "recovery-incomplete: 1 saved pane could not reconnect: \u{201c}{title}\u{201d}."
            )),
            "{program}: {reason}"
        );
        app.cards.remove(&card);
        assert_eq!(
            app.startup_pane_check(),
            Ok(false),
            "missing runtime is not ready"
        );
    }
}

#[test]
fn a_closed_updater_pipe_after_surface_open_keeps_the_conversation_and_unsaved_draft() {
    use iced::futures::StreamExt;

    let drain = |task| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                match iced::runtime::task::into_stream(task) {
                    Some(stream) => {
                        tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
                            .await
                            .expect("startup report effects must settle")
                    }
                    None => Vec::new(),
                }
            })
    };
    let failure = || {
        Message::StartupReported(Err(
            std::io::Error::from(std::io::ErrorKind::BrokenPipe).to_string()
        ))
    };
    let mut unopened = crate::app::tests::context_inspector_test_app(false);
    assert!(
        drain(unopened.update(failure()))
            .iter()
            .any(|effect| matches!(effect, iced::runtime::Action::Exit)),
        "the control must observe the real exit before a recovery window opens"
    );

    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text(
            "  Unsent recovery draft 🦀\n\n",
        ));
    let draft = chat_runtime(&app, card).composer.text();
    let handle = chat_runtime(&app, card).handle().cloned().unwrap();
    app.startup_gate = crate::app::startup_gate::State::fixture_opened();
    let cards = app.durable.cards.clone();
    // Public dispatch also schedules ordinary attention effects. Inspect
    // what the runtime receives, not whether a batch allocated a stream.
    let effects = drain(app.update(failure()));
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            iced::runtime::Action::Exit
                | iced::runtime::Action::Window(iced::runtime::window::Action::Close(_))
        )),
        "report failure must not issue window exit or close: {effects:?}"
    );
    assert!(app.exit.is_none());
    assert_eq!(app.durable.cards, cards);
    assert!(Arc::ptr_eq(
        chat_runtime(&app, card).handle().unwrap(),
        &handle
    ));
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        draft,
        "unsent whitespace and UTF-8 must survive the failed report"
    );
    assert!(app.status.is_some());
}

#[test]
fn history_reconnect_discards_old_request_generation_and_navigation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.page_request = Some((view::Page::Earlier, 100));
    runtime.deferred_page = Some(HistoryNavigation::Latest);
    runtime.earlier_asked = Some(1000.0);
    runtime.later_asked = Some(1000.0);
    drop(app.handle_chat_ui(Action::Reconnect(target)));
    let reconnect = app.chat_ui.attempt(card).unwrap().target;
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation = 1;
    snapshot.has_earlier = true;
    handle.publish_snapshot_for_test(snapshot);
    answer_start(&mut app, reconnect, Ok(handle));
    let runtime = app.chat_runtime_mut(reconnect).unwrap();
    runtime.show(true);
    runtime.refresh();
    assert!(runtime.page_request.is_none());
    assert!(runtime.deferred_page.is_none());
    assert!(runtime.earlier_asked.is_none());
    assert!(runtime.later_asked.is_none());
    drop(app.handle_chat_ui(Action::Command(reconnect, Command::LoadEarlier)));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Earlier)
    );
}

#[test]
fn history_loading_outlives_ack_and_releases_on_provider_response() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.history_loading = true;
    handle.publish_snapshot_for_test(snapshot.clone());
    settle(&mut app, card, Ok(()));
    assert_eq!(
        chat_runtime(&app, card).page_request.map(|(page, _)| page),
        Some(view::Page::Earlier)
    );
    drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
    assert!(
        chat_runtime(&app, card).commands.is_idle(),
        "duplicate request is suppressed after command ACK"
    );
    snapshot.generation += 1;
    snapshot.history_loading = false;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert!(chat_runtime(&app, card).page_request.is_none());
}

#[test]
fn history_cached_response_before_ack_does_not_latch_loading_forever() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert!(chat_runtime(&app, card).page_request.is_some());
    settle(&mut app, card, Ok(()));
    assert!(chat_runtime(&app, card).page_request.is_none());
    drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Earlier)
    );
}

#[test]
fn history_edge_and_short_page_click_wait_for_unrelated_command() {
    for explicit in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = history_fixture(&mut app);
        let scroll = view::Scroll::new(0.0, if explicit { 200.0 } else { 1000.0 }, 400.0);
        app.chat_runtime_mut(target).unwrap().scroll = scroll;
        drop(app.chat_send(
            target,
            Command::SetPermissionMode {
                mode: "manual".into(),
            },
            false,
        ));
        drop(app.handle_chat_ui(if explicit {
            Action::Command(target, Command::LoadEarlier)
        } else {
            Action::Scrolled(target, None, scroll, Some(view::Page::Earlier))
        }));
        assert_eq!(
            chat_runtime(&app, card).deferred_page,
            Some(if explicit {
                HistoryNavigation::EarlierExplicit
            } else {
                HistoryNavigation::EarlierEdge
            })
        );
        assert!(chat_runtime(&app, card).earlier_asked.is_none());
        settle(&mut app, card, Ok(()));
        assert_eq!(
            chat_runtime(&app, card).commands.in_flight().unwrap().page,
            Some(view::Page::Earlier)
        );
    }
}

#[test]
fn history_explicit_navigation_survives_wheeling_away_from_the_edge() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    drop(app.chat_send(
        target,
        Command::SetPermissionMode {
            mode: "manual".into(),
        },
        false,
    ));
    drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(0.0, 1000.0, 400.0),
        Some(view::Page::Earlier),
    )));
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(200.0, 1000.0, 400.0),
        Some(view::Page::Newer),
    )));
    assert_eq!(
        chat_runtime(&app, card).deferred_page,
        Some(HistoryNavigation::EarlierExplicit)
    );
    settle(&mut app, card, Ok(()));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Earlier)
    );
}

#[test]
fn history_failure_can_retry_without_leaving_the_boundary() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    let scroll = view::Scroll::new(0.0, 1000.0, 400.0);
    app.chat_runtime_mut(target).unwrap().scroll = scroll;
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        scroll,
        Some(view::Page::Earlier),
    )));
    settle(&mut app, card, Err("temporary history failure".into()));
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        scroll,
        Some(view::Page::Earlier),
    )));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Earlier)
    );
}

#[test]
fn provider_failure_stays_at_its_turn_after_recovery_and_new_work() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items.push(Arc::new(chat::Item {
        id: "failed-prompt".into(),
        role: chat::Role::User,
        content: text("Investigate"),
        started_at: None,
        at: None,
    }));
    snapshot.error = Some("Provider failure".into());
    handle.publish_snapshot_for_test(snapshot.clone());
    app.chat_runtime_mut(target).unwrap().refresh();
    let recorded = chat_runtime(&app, card).failures.clone();
    assert_eq!(recorded.len(), 1);
    let drawn = chat_runtime(&app, card).failure_ui.incidents[0]
        .anchor
        .clone();
    assert_eq!(
        drawn.as_deref(),
        Some("failed-prompt"),
        "drawn after the work that failed"
    );
    snapshot.generation += 1;
    snapshot.error = None;
    snapshot.items.push(Arc::new(chat::Item {
        id: "recovered-reply".into(),
        role: chat::Role::Assistant,
        content: text("Signed in again."),
        started_at: None,
        at: None,
    }));
    snapshot.items.push(Arc::new(chat::Item {
        id: "next-prompt".into(),
        role: chat::Role::User,
        content: text("Try again"),
        started_at: None,
        at: None,
    }));
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.failures, recorded);
    let incident = runtime
        .failure_ui
        .incidents
        .iter()
        .find(|incident| incident.summary.contains("Provider failure"))
        .unwrap();
    assert_eq!(incident.anchor, drawn);
    assert_ne!(
        incident.anchor.as_ref(),
        runtime.turns.last().map(|turn| &turn.key)
    );
    assert!(runtime.snapshot.error.is_none());
}

#[test]
fn history_provider_failure_can_retry_at_the_same_edge_after_ack() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let scroll = view::Scroll::new(0.0, 1000.0, 400.0);
    app.chat_runtime_mut(target).unwrap().scroll = scroll;
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        scroll,
        Some(view::Page::Earlier),
    )));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.history_loading = true;
    handle.publish_snapshot_for_test(snapshot.clone());
    settle(&mut app, card, Ok(()));
    snapshot.generation += 1;
    snapshot.history_loading = false;
    snapshot.error = Some("temporary provider error".into());
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        scroll,
        Some(view::Page::Earlier),
    )));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Earlier)
    );
}

#[test]
fn history_geometry_changes_never_trigger_paging_even_when_page_heights_match() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = true;
    for scroll in [
        view::Scroll::new(0.0, 1000.0, 400.0),
        view::Scroll::new(600.0, 1000.0, 400.0),
        view::Scroll::new(0.0, 100.0, 400.0),
    ] {
        drop(app.handle_chat_ui(Action::Scrolled(target, None, scroll, None)));
        assert!(chat_runtime(&app, card).commands.is_idle());
    }
}

#[test]
fn history_latest_gesture_cancels_when_reader_leaves_bottom() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = true;
    drop(app.chat_send(
        target,
        Command::SetPermissionMode {
            mode: "manual".into(),
        },
        false,
    ));
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(600.0, 1000.0, 400.0),
        Some(view::Page::Newer),
    )));
    assert_eq!(
        chat_runtime(&app, card).deferred_page,
        Some(HistoryNavigation::NewerEdge)
    );
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(300.0, 1000.0, 400.0),
        Some(view::Page::Earlier),
    )));
    assert!(chat_runtime(&app, card).deferred_page.is_none());
    settle(&mut app, card, Ok(()));
    assert!(chat_runtime(&app, card).commands.is_idle());
}

#[test]
fn history_bottom_requests_cached_next_page_instead_of_refreshing_to_latest() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = true;
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(600.0, 1000.0, 400.0),
        Some(view::Page::Newer),
    )));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Newer)
    );
}

#[test]
fn history_explicit_latest_survives_leaving_bottom() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.has_later = true;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    drop(app.chat_send(
        target,
        Command::SetPermissionMode {
            mode: "manual".into(),
        },
        false,
    ));
    drop(app.handle_chat_ui(Action::Latest(target)));
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(300.0, 1000.0, 400.0),
        Some(view::Page::Earlier),
    )));
    assert_eq!(
        chat_runtime(&app, card).deferred_page,
        Some(HistoryNavigation::Latest)
    );
    settle(&mut app, card, Ok(()));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Latest)
    );
}

#[test]
fn history_bottom_does_not_mark_unloaded_latest_activity_seen() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.unseen_activity = true;
    Arc::make_mut(&mut runtime.snapshot).has_later = true;
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(600.0, 1000.0, 400.0),
        None,
    )));
    assert!(chat_runtime(&app, card).unseen_activity);
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = false;
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(600.0, 1000.0, 400.0),
        None,
    )));
    assert!(!chat_runtime(&app, card).unseen_activity);
}

#[test]
fn history_repeated_latest_after_ack_waits_for_provider_history() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.has_later = true;
    handle.publish_snapshot_for_test(snapshot.clone());
    app.chat_runtime_mut(target).unwrap().refresh();
    drop(app.handle_chat_ui(Action::Latest(target)));
    snapshot.generation += 1;
    snapshot.history_loading = true;
    handle.publish_snapshot_for_test(snapshot.clone());
    settle(&mut app, card, Ok(()));
    assert_eq!(
        chat_runtime(&app, card).page_request.map(|(page, _)| page),
        Some(view::Page::Latest)
    );
    drop(app.handle_chat_ui(Action::Latest(target)));
    assert!(chat_runtime(&app, card).commands.is_idle());
    snapshot.generation += 1;
    snapshot.history_loading = false;
    snapshot.has_later = false;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert!(chat_runtime(&app, card).page_request.is_none());
}

#[test]
fn history_repeated_latest_is_fulfilled_by_the_pending_refresh() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.has_later = true;
    handle.publish_snapshot_for_test(snapshot.clone());
    app.chat_runtime_mut(target).unwrap().refresh();
    drop(app.handle_chat_ui(Action::Latest(target)));
    drop(app.handle_chat_ui(Action::Latest(target)));
    snapshot.generation += 1;
    snapshot.has_later = false;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    settle(&mut app, card, Ok(()));
    assert!(chat_runtime(&app, card).deferred_page.is_none());
    assert!(chat_runtime(&app, card).page_request.is_none());
    drop(app.chat_send(
        target,
        Command::SetPermissionMode {
            mode: "manual".into(),
        },
        false,
    ));
    drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
    assert_eq!(
        chat_runtime(&app, card).deferred_page,
        Some(HistoryNavigation::EarlierExplicit)
    );
}

#[test]
fn history_latest_cancels_earlier_even_before_historical_page_arrives() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.history_loading = true;
    assert!(!snapshot.has_later);
    handle.publish_snapshot_for_test(snapshot);
    settle(&mut app, card, Ok(()));
    drop(app.handle_chat_ui(Action::Latest(target)));
    assert_eq!(
        chat_runtime(&app, card).commands.in_flight().unwrap().page,
        Some(view::Page::Latest)
    );
}

#[test]
fn history_subagent_scroll_and_latest_never_page_parent() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.selected_subagent = Some("child".into());
    Arc::make_mut(&mut runtime.snapshot).has_later = true;
    let parent = view::Scroll::new(500.0, 1000.0, 400.0);
    runtime.scroll = parent;
    assert_eq!(runtime.active_scroll(), view::Scroll::default());
    let child = view::Scroll::new(0.0, 200.0, 400.0);
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        Some("child".into()),
        child,
        Some(view::Page::Earlier),
    )));
    drop(app.handle_chat_ui(Action::Scrolled(
        target,
        None,
        view::Scroll::new(0.0, 1000.0, 400.0),
        None,
    )));
    assert_eq!(
        chat_runtime(&app, card).scroll,
        parent,
        "queued parent measurement is stale after tab switch"
    );
    assert_eq!(chat_runtime(&app, card).active_scroll(), child);
    drop(app.handle_chat_ui(Action::Latest(target)));
    assert!(chat_runtime(&app, card).commands.is_idle());
}

#[test]
fn finding_collapsed_reasoning_reveals_it_and_restores_disclosure() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("find-reasoning"));
    let target = chat_runtime(&app, card).target;
    let mut snapshot = (*chat_runtime(&app, card).snapshot).clone();
    snapshot.generation += 1;
    snapshot.busy = true;
    snapshot.items = vec![Arc::new(chat::Item {
        id: "reasoning".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: "Unique reasoning needle".into(),
            thinking: true,
            phase: chat::Phase::Commentary,
        },
        started_at: None,
        at: None,
    })];
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    assert!(!chat_runtime(&app, card).expanded.contains("reasoning"));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "needle".into(),
    ));
    assert!(chat_runtime(&app, card).expanded.contains("reasoning"));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(!chat_runtime(&app, card).expanded.contains("reasoning"));
}

#[test]
fn parent_find_reveals_completed_large_thinking_without_reparsing_its_prepared_graph() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let body=format!("{}\n\nCOMPLETED_THINKING_NEEDLE λ\n", "> Rich **reasoning** café é.\n>\n> - nested `source` leaf\n\n| exact | table |\n| --- | --- |\n| value | λ |\n\n```text\noriginal fenced source\n```\n\n".repeat(2400));
    assert!(body.len() > 256 * 1024);
    let source = Arc::new(chat::Item {
        id: "completed-thinking".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: body.clone(),
            thinking: true,
            phase: chat::Phase::Final,
        },
        started_at: None,
        at: None,
    });
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.busy = false;
    snapshot.items = vec![source.clone()];
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    let runtime = chat_runtime(&app, card);
    let cached = &runtime.cache[&source.id];
    assert!(!cached.preparing && !cached.placeholder);
    assert!(!runtime.expanded.contains(&source.id));
    assert_eq!(runtime.selected_subagent, None);
    let document = cached.document.as_ref().unwrap().clone();
    let revision = cached.revision;
    let canonical = cached.markdown.items().as_ptr();
    let parsed = cached.markdown.parsed_work_bytes();
    let mut expected = selection::Catalog::builder();
    expected.markdown(&source.id, &[0], cached.markdown.items());
    let expected = expected.finish();
    let (first, last) = expected.endpoints().unwrap();
    let expected_copy = expected.copy(first, last).unwrap();
    let parser_work = markdown::parser_work_bytes_on_thread();
    let materialized = markdown::materialized_bytes();
    let clones = markdown::cloned_items();
    for forward in [true, false] {
        // Find itself performs the first reveal; explicit Next also runs
        // the real navigation path and wraps the one matching parent turn.
        drop(find_navigation::find_for_test(
            &mut app,
            target,
            "COMPLETED_THINKING_NEEDLE".into(),
        ));
        drop(app.handle_chat_ui(Action::FindNext(target, forward)));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.expanded.contains(&source.id));
        assert_eq!(runtime.find_matches.len(), 1);
        assert!(
            runtime.find_navigation.pending(),
            "actual Find navigation is scheduled after disclosure"
        );
        assert_eq!(
            markdown::parser_work_bytes_on_thread(),
            parser_work,
            "no Markdown parse work runs on the UI for Find reveal"
        );
        let cached = &runtime.cache[&source.id];
        assert_eq!(
            cached.revision, revision,
            "disclosure must reuse prepared presentation metadata"
        );
        assert_eq!(cached.markdown.items().as_ptr(), canonical);
        assert!(
            std::ptr::eq(cached.document.as_ref().unwrap().items(), document.items()),
            "same immutable prepared publication root"
        );
        assert_eq!(cached.markdown.parsed_work_bytes(), parsed);
        assert!(Arc::ptr_eq(cached.item.as_ref().unwrap(), &source));
        assert!(
            matches!(&cached.item.as_ref().unwrap().content,ItemContent::Text{text,..} if text==&body)
        );
        let catalog = runtime.selection_document().0;
        let (first, last) = catalog.endpoints().unwrap();
        assert_eq!(
            catalog.copy(first, last).as_deref(),
            Some(expected_copy.as_str()),
            "actual revealed selection retains every original rendered source block"
        );

        assert_eq!(
            markdown::cloned_items(),
            clones,
            "no replacement AST snapshot is cloned on reveal"
        );
        assert_eq!(
            markdown::materialized_bytes(),
            materialized,
            "no styled full-source materialization is hidden in reveal"
        );
        drop(app.handle_chat_ui(Action::CloseFind(target)));
        let runtime = chat_runtime(&app, card);
        assert!(
            !runtime.expanded.contains(&source.id),
            "Find restores its own temporary disclosure"
        );
        assert!(!runtime.find_navigation.pending());
        assert_eq!(runtime.cache[&source.id].revision, revision);
    }
}

#[test]
fn find_actions_restore_automatic_disclosures_and_keep_explicit_choices() {
    fn output_count(
        runtime: &Runtime,
        renderer: &cosmic::Renderer,
        tree: &mut Option<iced::advanced::widget::Tree>,
    ) -> usize {
        use iced::advanced::{
            Layout,
            widget::{Operation, Tree},
        };
        struct Output(usize);
        impl Operation for Output {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(prose) = state.downcast_mut::<selectable_prose::State>()
                    && prose.plain.as_ref() == "Straße Needle"
                {
                    self.0 += 1;
                }
            }
        }
        let mut element = runtime.find_turn_view_for_test();
        let tree = tree.get_or_insert_with(|| Tree::new(&element));
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &iced::advanced::layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, 4000.0)),
        );
        let mut output = Output(0);
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), renderer, &mut output);
        output.0
    }
    let renderer = crate::app::tests::headless_renderer();
    let mut rendered_tree = None;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("find-disclosure"));
    let target = chat_runtime(&app, card).target;
    let mut snapshot = (*chat_runtime(&app, card).snapshot).clone();
    snapshot.generation += 1;
    snapshot.items = std::iter::once(Arc::new(chat::Item {
        id: "prompt".into(),
        role: chat::Role::User,
        content: text("question"),
        started_at: None,
        at: None,
    }))
    .chain(["automatic", "manual", "changed"].map(|id| {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: "Inspect".into(),
                input: "{}".into(),
                output: "Straße Needle".into(),
                status: chat::ToolStatus::Succeeded,
            },
            started_at: None,
            at: None,
        })
    }))
    .collect();
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    drop(app.handle_chat_ui(Action::Toggle(target, "manual".into())));
    app.chat_runtime_mut(target)
        .unwrap()
        .opened
        .edit()
        .insert("prompt".into(), false);
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "STRASSE".into(),
    ));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.find_matches.iter().collect::<Vec<_>>(),
        vec!["prompt"]
    );
    assert!(
        ["automatic", "manual", "changed"]
            .iter()
            .all(|id| runtime.expanded.contains(*id))
    );
    assert_eq!(runtime.opened.get("prompt"), Some(&true));
    assert!(
        runtime.commands.is_idle(),
        "Find must not send provider commands"
    );
    assert_eq!(output_count(runtime, &renderer, &mut rendered_tree), 3);
    // Explicitly close and reopen one automatic row: its final open choice
    // belongs to the user and survives a new query and search closure.
    drop(app.handle_chat_ui(Action::Toggle(target, "changed".into())));
    drop(app.handle_chat_ui(Action::Toggle(target, "changed".into())));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "absent".into(),
    ));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.expanded,
        HashSet::from(["manual".into(), "changed".into()])
    );
    assert_eq!(runtime.opened.get("prompt"), Some(&false));
    assert_eq!(output_count(runtime, &renderer, &mut rendered_tree), 0);
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "needle".into(),
    ));
    drop(app.handle_chat_ui(Action::ToggleTurn(target, "prompt".into())));
    drop(app.handle_chat_ui(Action::ToggleTurn(target, "prompt".into())));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.expanded,
        HashSet::from(["manual".into(), "changed".into()])
    );
    assert_eq!(runtime.opened.get("prompt"), Some(&true));
    assert_eq!(output_count(runtime, &renderer, &mut rendered_tree), 2);
    assert!(runtime.find.is_none() && runtime.find_matches.is_empty());
    assert!(runtime.commands.is_idle());
}

#[test]
fn find_matches_displayed_command_labels_directories_and_changed_paths() {
    let command = ItemContent::Command {
        label: "Verify RELEASE".into(),
        command: "true".into(),
        working_directory: "/Workspace/Project".into(),
        action: chat::CommandAction::Run,
        status: chat::ToolStatus::Succeeded,
        exit_code: Some(0),
        duration_ms: None,
        output: String::new(),
    };
    assert!(contains_text(&command, "release"));
    assert!(contains_text(&command, "workspace"));
    let change = ItemContent::Changes {
        changes: vec![chat::FileChange {
            path: "src/Über.rs".into(),
            kind: chat::ChangeKind::Updated,
            diff: "+line".into(),
        }],
        status: chat::ToolStatus::Succeeded,
    };
    assert!(contains_text(&change, "ÜBER"));
    assert!(!contains_text(&change, ""));
}

#[test]
fn tool_group_toggles_override_both_default_choices() {
    for default in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("disclosure"));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.tool_groups_expanded = default;
        runtime.turns = turn::group(
            &[chat::Item {
                id: "u1".into(),
                role: chat::Role::User,
                content: text("hello"),
                started_at: None,
                at: None,
            }],
            &HashSet::new(),
            Presentations(&HashMap::new()),
        )
        .into();
        let _ = app.handle_chat_ui(Action::ToggleTurn(target, "u1".into()));
        assert_eq!(chat_runtime(&app, card).opened.get("u1"), Some(&!default));
        let _ = app.handle_chat_ui(Action::ToggleTurn(target, "u1".into()));
        assert_eq!(chat_runtime(&app, card).opened.get("u1"), Some(&default));
    }
}

#[test]
fn paged_tool_groups_keep_reader_choices_when_their_prompt_is_evicted() {
    for default in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("paged-disclosure"));
        let target = chat_runtime(&app, card).target;
        app.chat_runtime_mut(target).unwrap().tool_groups_expanded = default;
        let item = |id: &str, role, content| {
            Arc::new(chat::Item {
                id: id.into(),
                role,
                content,
                at: None,
                started_at: None,
            })
        };
        let tool = |id| {
            item(
                id,
                chat::Role::Assistant,
                ItemContent::Tool {
                    name: "shell".into(),
                    input: "run".into(),
                    output: "saved output".into(),
                    status: chat::ToolStatus::Succeeded,
                },
            )
        };
        let full = vec![
            item("prompt", chat::Role::User, text("Work")),
            tool("first-tool"),
            tool("boundary-tool"),
        ];
        let mut snapshot = (*chat_runtime(&app, card).snapshot).clone();
        snapshot.generation += 1;
        snapshot.items = full.clone();
        show_chat_snapshot_for_test(&mut app, card, snapshot.clone());
        drop(app.handle_chat_ui(Action::ToggleTurn(target, "prompt".into())));
        drop(app.handle_chat_ui(Action::Toggle(target, "boundary-tool".into())));
        app.chat_runtime_mut(target)
            .unwrap()
            .unfolded
            .insert("boundary-tool#output".into());
        snapshot.generation += 1;
        snapshot.items = vec![full[2].clone()];
        snapshot.has_later = true;
        show_chat_snapshot_for_test(&mut app, card, snapshot.clone());
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.turns[0].key, "boundary-tool");
        assert_eq!(runtime.opened.get("boundary-tool"), Some(&!default));
        assert!(runtime.expanded.contains("boundary-tool"));
        assert!(runtime.unfolded.contains("boundary-tool#output"));
        // A new explicit choice on the partial group supersedes the stale
        // remembered choice when its original prompt returns.
        drop(app.handle_chat_ui(Action::ToggleTurn(target, "boundary-tool".into())));
        snapshot.generation += 1;
        snapshot.items = full;
        snapshot.has_later = false;
        show_chat_snapshot_for_test(&mut app, card, snapshot);
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.opened.get("prompt"), Some(&default));
        assert!(runtime.expanded.contains("boundary-tool"));
    }
}

#[test]
fn selection_catalog_is_rebuilt_only_when_exposed_text_or_disclosure_changes() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("selection-catalog"));
    let target = chat_runtime(&app, card).target;
    let mut snapshot = (*chat_runtime(&app, card).snapshot).clone();
    snapshot.generation += 1;
    snapshot.items = vec![
        Arc::new(chat::Item {
            id: "prompt".into(),
            role: chat::Role::User,
            content: text("question"),
            started_at: None,
            at: None,
        }),
        Arc::new(chat::Item {
            id: "tool".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: "Inspect".into(),
                input: "{}".into(),
                output: "tool output".into(),
                status: chat::ToolStatus::Succeeded,
            },
            started_at: None,
            at: None,
        }),
    ];
    show_chat_snapshot_for_test(&mut app, card, snapshot.clone());
    // An open diagnostic whose bounded preview a Find query can move.
    let runtime = app.chat_runtime_mut(target).unwrap();
    let anchor = runtime.turns[0].key.clone();
    runtime.failures = vec![failure::Note {
        message: Some(anchor.clone()),
        after: Some(anchor),
        text: format!(
            "Provider failure\n{}\nDIAGNOSTIC_TAIL",
            "diagnostic bytes ".repeat(4096)
        ),
    }];
    runtime.refresh_failure_ui();
    let key = runtime.failure_ui.incidents[0].key.clone();
    drop(app.handle_chat_ui(Action::ToggleFailure(target, key.clone())));
    let catalog = |app: &Cagents| chat_runtime(app, card).selection_document().0;
    let preview = |catalog: &selection::Catalog| {
        catalog
            .text(failure::diagnostic_preview_identity(&key))
            .expect("the open diagnostic preview is selectable")
    };

    let built = catalog(&app);
    assert!(!preview(&built).contains("DIAGNOSTIC_TAIL"));
    assert!(
        Arc::ptr_eq(&built, &catalog(&app)),
        "an unchanged view reuses it"
    );

    snapshot.generation += 1;
    snapshot.detail = "Still working".into();
    show_chat_snapshot_for_test(&mut app, card, snapshot.clone());
    assert!(
        Arc::ptr_eq(&built, &catalog(&app)),
        "a metadata-only publication exposes nothing new"
    );

    for query in ["absent", "absentee"] {
        drop(find_navigation::find_for_test(
            &mut app,
            target,
            query.into(),
        ));
        assert!(
            Arc::ptr_eq(&built, &catalog(&app)),
            "a Find keystroke that reveals and restores nothing keeps it"
        );
    }

    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "DIAGNOSTIC_TAIL".into(),
    ));
    let found = catalog(&app);
    assert!(
        !Arc::ptr_eq(&built, &found),
        "a query that moves the preview's window changes exposed text"
    );
    assert!(preview(&found).contains("DIAGNOSTIC_TAIL"));
    assert!(Arc::ptr_eq(&found, &catalog(&app)));

    drop(app.handle_chat_ui(Action::CloseFind(target)));
    let closed = catalog(&app);
    assert!(!preview(&closed).contains("DIAGNOSTIC_TAIL"));
    assert!(Arc::ptr_eq(&closed, &catalog(&app)));

    drop(app.handle_chat_ui(Action::Toggle(target, "tool".into())));
    let disclosed = catalog(&app);
    assert!(
        !Arc::ptr_eq(&closed, &disclosed),
        "opening a step changes what is exposed"
    );
    assert!(Arc::ptr_eq(&disclosed, &catalog(&app)));

    // A question regroups the transcript around its boundary without
    // touching any item, so no presented text moves; the grouping does.
    let presented = chat_runtime(&app, card).presentation_revision;
    snapshot.generation += 1;
    snapshot.requests = vec![chat::Request {
        kind: crate::chat::RequestKind::Approval,
        id: "approval".into(),
        title: "Allow Bash?".into(),
        description: String::new(),
        fields: vec![],
        actions: vec![],
    }];
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    assert_eq!(chat_runtime(&app, card).presentation_revision, presented);
    let regrouped = catalog(&app);
    assert!(
        !Arc::ptr_eq(&disclosed, &regrouped),
        "a regroup with unchanged items is still a new catalog"
    );
    assert!(Arc::ptr_eq(&regrouped, &catalog(&app)));
}

#[test]
fn paging_inherits_reader_choices_without_retaining_temporary_find_disclosure() {
    for explicit in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("paged-find-disclosure"));
        let target = chat_runtime(&app, card).target;
        let mut snapshot = (*chat_runtime(&app, card).snapshot).clone();
        snapshot.generation += 1;
        snapshot.items = vec![
            Arc::new(chat::Item {
                id: "prompt".into(),
                role: chat::Role::User,
                content: text("Work"),
                at: None,
                started_at: None,
            }),
            Arc::new(chat::Item {
                id: "tool".into(),
                role: chat::Role::Assistant,
                content: ItemContent::Tool {
                    name: "shell".into(),
                    input: "run".into(),
                    output: "search needle".into(),
                    status: chat::ToolStatus::Succeeded,
                },
                at: None,
                started_at: None,
            }),
        ];
        app.chat_runtime_mut(target).unwrap().tool_groups_expanded = false;
        show_chat_snapshot_for_test(&mut app, card, snapshot.clone());
        if explicit {
            drop(app.handle_chat_ui(Action::ToggleTurn(target, "prompt".into())));
            drop(app.handle_chat_ui(Action::ToggleTurn(target, "prompt".into())));
        }
        drop(find_navigation::find_for_test(
            &mut app,
            target,
            "needle".into(),
        ));
        assert_eq!(chat_runtime(&app, card).opened.get("prompt"), Some(&true));
        snapshot.generation += 1;
        snapshot.items.remove(0);
        show_chat_snapshot_for_test(&mut app, card, snapshot);
        drop(app.handle_chat_ui(Action::CloseFind(target)));
        assert_eq!(
            chat_runtime(&app, card).opened.get("tool"),
            explicit.then_some(&false)
        );
        assert_eq!(
            chat_runtime(&app, card).opened.get("prompt"),
            explicit.then_some(&false)
        );
    }
}

#[test]
fn disclosure_choices_survive_history_eviction_and_are_bounded() {
    let mut opened = Revised::new(HashMap::from([("u1".to_owned(), true)]));
    let stamp = opened.stamp();
    reconcile_disclosure(&mut opened, &[]);
    assert_eq!(opened.get("u1"), Some(&true));
    assert_eq!(opened.stamp(), stamp, "within budget, nothing is pruned");
    opened.edit().insert("u1".to_owned(), false);
    reconcile_disclosure(&mut opened, &[]);
    assert_eq!(opened.get("u1"), Some(&false));
    opened
        .edit()
        .extend((0..4097).map(|index| (format!("old-{index}"), true)));
    reconcile_disclosure(&mut opened, &[]);
    assert!(opened.is_empty());
}

const FIXTURE_DIRECTORY: &str = "/workspace/chat-fixture";
/// Providers name conversations by UUID; resume refuses anything else.
pub(super) const KNOWN_THREAD: &str = "0199a2b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b";

#[test]
fn new_panes_keep_the_selected_channel_when_launch_directory_changes() {
    for chat in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let source = app.workspace.focused().unwrap();
        let root = PathBuf::from(FIXTURE_DIRECTORY);
        let nested = root.join("src");
        let channel = crate::model::ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: crate::model::ProjectId::new(),
            title: "Conversation".into(),
            ordinal: 0,
            shared: false,
            cards: vec![source],
            focused: Some(source),
        });
        durable.active_channel = Some(channel);
        app.active_workspace = Some(root.clone());
        if chat {
            drop(app.new_native_chat(Provider::Codex, Some(nested.clone())));
        } else {
            drop(app.add_terminal_recipe(LaunchRecipe::shell(nested.clone()).unwrap()));
        }
        let added = app.workspace.focused().unwrap();
        assert_ne!(added, source);
        assert_eq!(app.active_workspace, Some(root));
        assert_eq!(app.durable.active_channel, Some(channel));
        assert!(app.durable.channels[0].cards.contains(&added));
        assert!(app.durable.channels[0].cards.contains(&source));
        assert_eq!(
            crate::app::worktree_ui::card_directory(app.card_state(added).unwrap()),
            Some(nested.as_path())
        );
    }
}

#[test]
fn automatic_draft_routing_waits_for_untouched_send_and_same_account_choice_pins() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let alternate = app.account_ui.add_managed_fixture(Provider::Codex, 105);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let now = crate::chat::now_ms().unwrap();
    for (id, status) in [
        (
            Account::system(Provider::Codex).id(),
            account::UsageStatus::Limited,
        ),
        (alternate.clone(), account::UsageStatus::Ok),
    ] {
        let (owner, ticket) =
            app.account_ui
                .begin_probe_for_test(&id, crate::account::schedule::Trigger::Focus, now);
        let _ = app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
            owner,
            ticket,
            id,
            outcome: account::probe::Outcome::Answered {
                identity: None,
                catalog: None,
                snapshot: Box::new(account::codex::parse_rate_limits(
                    &serde_json::json!({"ordinaryUsageAllowed": status == account::UsageStatus::Ok}),
                    now,
                )),
            },
        }));
    }
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text(
        "Retain this unsent message",
    ));
    assert_eq!(
        app.automatic_chat_account(target, PromptMode::Send)
            .unwrap()
            .0
            .id(),
        alternate
    );
    assert!(
        app.automatic_chat_account(target, PromptMode::Steer)
            .is_none()
    );
    app.chat_runtime_mut(target)
        .unwrap()
        .attachment_chooser_pending = Some(uuid::Uuid::new_v4());
    assert!(
        app.automatic_chat_account(target, PromptMode::Send)
            .is_none()
    );
    app.chat_runtime_mut(target)
        .unwrap()
        .attachment_chooser_pending = None;
    drop(app.choose_chat_account(target, Account::system(Provider::Codex).id()));
    assert!(
        app.automatic_chat_account(target, PromptMode::Send)
            .is_none()
    );
    assert_eq!(
        chat_runtime(&app, card).composer.text().trim(),
        "Retain this unsent message"
    );
    assert!(
        !chat_runtime(&app, card)
            .session
            .launch
            .recipe
            .environment
            .contains_key(account::routing::AUTOMATIC_MARKER)
    );
}

/// A live Codex conversation on automatic routing whose own account has
/// run out while another has quota, so sending `draft` would move it.
fn limited_automatic_chat(app: &mut Cagents, draft: &str) -> (CardId, Target) {
    let alternate = app.account_ui.add_managed_fixture(Provider::Codex, 105);
    let card = live_chat(app, Some(KNOWN_THREAD));
    let target = chat_runtime(app, card).target;
    let now = crate::chat::now_ms().unwrap();
    for (id, allowed) in [
        (Account::system(Provider::Codex).id(), false),
        (alternate.clone(), true),
    ] {
        let (owner, ticket) =
            app.account_ui
                .begin_probe_for_test(&id, crate::account::schedule::Trigger::Focus, now);
        let _ = app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
            owner,
            ticket,
            id,
            outcome: account::probe::Outcome::Answered {
                identity: None,
                catalog: None,
                snapshot: Box::new(account::codex::parse_rate_limits(
                    &serde_json::json!({"ordinaryUsageAllowed": allowed}),
                    now,
                )),
            },
        }));
    }
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime
        .session
        .launch
        .recipe
        .environment
        .insert(account::routing::AUTOMATIC_MARKER.into(), "1".into());
    runtime.replace_draft(text_editor::Content::with_text(draft));
    (card, target)
}

#[test]
fn automatic_account_switch_names_the_subagent_holding_the_conversation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = limited_automatic_chat(&mut app, "Keep this message");
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot).subagents = vec![chat::Subagent {
        id: "toolu_dev_server".into(),
        kind: "Run the dev server".into(),
        description: "Run the dev server".into(),
        status: chat::ToolStatus::Waiting,
        model: None,
        waiting: false,
        detail: String::new(),
        background: true,
        items: Vec::new(),
    }];
    let (_, generation, _) = app
        .automatic_chat_account(target, PromptMode::Send)
        .expect("the limited account would be replaced");
    assert!(generation.is_some(), "the conversation is live");
    drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.error.as_deref(),
        Some(
            "Sending would move this conversation to another account, but it can't move while background work is still going. Background subagent \u{201c}Run the dev server\u{201d} is waiting on its own background work, which may not finish on its own. Stop it from its tab in the conversation, then send again."
        )
    );
    assert_eq!(runtime.composer.text().trim(), "Keep this message");
    assert!(!app.chat_start_pending(card), "no switch was attempted");
}

/// The text `card`'s view lays out within 900 by 980, in order. The
/// footer's fit follows the workspace's geometry for the card, not this
/// limit, so a test of the wide footer resizes the workspace first.
pub(super) fn visible_text(app: &Cagents, card: CardId) -> Vec<String> {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree},
    };
    #[derive(Default)]
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let mut element = app.chat_view(card);
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(900.0, 980.0)),
    );
    let mut text = Texts::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut text);
    text.0
}

/// The footer keeps naming the account the conversation is on while an
/// automatic switch away from it waits, and after the daemon refuses it,
/// as the conversation never left that account.
#[test]
fn the_footer_names_the_conversations_account_through_a_pending_switch() {
    for refusal in [
        chat::Recovery::Busy,
        chat::Recovery::Unresponsive,
        chat::Recovery::Unreached,
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = limited_automatic_chat(&mut app, "Keep this message");
        // Wide enough that the footer names the account beside its
        // avatar, rather than only to assistive technology.
        let _ = app.workspace.resize_viewport(1200.0);
        let launch = &chat_runtime(&app, card).session.launch;
        let accounts = app
            .chat_accounts(launch.provider, &launch.recipe)
            .expect("two accounts to choose between");
        let current = accounts.current.to_owned();
        let candidate = accounts
            .choices
            .into_iter()
            .find(|choice| !choice.selected)
            .expect("an account to switch to")
            .label
            .to_owned();
        assert_ne!(current, candidate);
        // Names the conversation's account, and not the one it may move
        // to, which it has not reached.
        let named = |app: &Cagents| {
            let text = visible_text(app, card);
            text.contains(&current) && !text.contains(&candidate)
        };
        assert!(
            named(&app),
            "{current} before sending: {:?}",
            visible_text(&app, card)
        );

        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
        assert!(app.chat_start_pending(card), "sending asked for a switch");
        assert!(
            named(&app),
            "{current} while the switch waits: {:?}",
            visible_text(&app, card)
        );

        let attempt = chat_runtime_attempt(&app, card);
        answer_start(
            &mut app,
            attempt,
            Err(chat::StartFailure {
                message: crate::terminal_ipc::DAEMON_BUSY.into(),
                recovery: refusal,
            }),
        );
        assert!(!app.chat_start_pending(card));
        assert!(
            named(&app),
            "{current} once {refusal:?} refused the switch: {:?}",
            visible_text(&app, card)
        );
    }
}

/// A switch the daemon refused before changing anything moved nothing,
/// whether it was finishing another operation or refused it for a reason
/// of its own. The conversation stays on its own account and attached,
/// offers no restore of an account that was never removed, says plainly
/// that the message wasn't sent, and sending again retries. A refusal for
/// a reason of its own keeps the daemon's sentence under Details.
#[test]
fn a_busy_daemon_refusing_an_automatic_switch_keeps_the_conversation_on_its_account() {
    const REASON: &str = "The conversation history could not be prepared on the alternate account: Transcript diverged from the existing account-switch copy";
    for (recovery, message, notice, guidance, explained) in [
        (
            chat::Recovery::Busy,
            crate::terminal_ipc::DAEMON_BUSY,
            chat::failure::delivery::ACCOUNT_SWITCH_BUSY.to_owned(),
            "Send again in a moment.",
            true,
        ),
        (
            chat::Recovery::NotAdmitted,
            REASON,
            chat::failure::account_switch_refused(REASON),
            chat::failure::ACCOUNT_SWITCH_REFUSED_ACTION,
            false,
        ),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = limited_automatic_chat(&mut app, "Keep this message");
        app.chat_runtime_mut(target)
            .unwrap()
            .attachments
            .push(Attachment {
                path: "kept.png".into(),
                reference: None,
            });
        let source = chat_runtime(&app, card).session.clone();
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
        let attempt = chat_runtime_attempt(&app, card);
        assert!(
            app.chat_ui.attempt(card).unwrap().recovery.is_some(),
            "sending asked for an account switch"
        );
        answer_start(
            &mut app,
            attempt,
            Err(chat::StartFailure {
                message: message.into(),
                recovery,
            }),
        );
        assert!(!app.chat_start_pending(card));
        assert!(
            !app.chat_ui.removed_accounts.contains(&card),
            "{recovery:?}"
        );
        assert!(!app.chat_ui.restorable_accounts.contains_key(&card));
        assert!(app.account_recovery_controls(card).is_none());
        assert!(
            app.chat_ui
                .retiring
                .iter()
                .all(|retiring| retiring.card != card),
            "the daemon admitted nothing, so nothing is given up"
        );
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.target, target, "the original attachment is kept");
        assert!(Arc::ptr_eq(runtime.handle().unwrap(), &handle));
        assert!(!runtime.disconnected);
        assert_eq!(runtime.session, source);
        assert_eq!(runtime.error.as_deref(), Some(notice.as_str()));
        assert_eq!(runtime.composer.text().trim(), "Keep this message");
        assert_eq!(runtime.attachments.len(), 1, "the attachment is kept too");
        assert_eq!(runtime.attachments[0].path, PathBuf::from("kept.png"));
        // It answers the send; the conversation and its last turn are fine.
        assert_eq!(runtime.reported_errors().next(), None);
        // As drawn on screen.
        app.chat_runtime_mut(target).unwrap().reconcile_failure_ui();
        let notice = chat_runtime(&app, card)
            .failure_ui
            .incidents
            .iter()
            .find(|incident| incident.guidance == guidance)
            .expect("the refusal is shown");
        assert!(notice.summary.starts_with("Your message wasn't sent."));
        // A fixed outcome has no details that would only repeat it; the
        // daemon's own reason is kept under Details.
        assert_eq!(notice.explained, explained, "{recovery:?}");
        assert!(!notice.summary.contains("diverged"));
        if !explained {
            // Details carry only what the notice doesn't already say.
            let key = notice.key.clone();
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.failure_ui.expanded.edit().insert(key.clone());
            runtime.cache_failure_preview(&key);
            assert_eq!(runtime.failure_preview(&key), Some((REASON, false)));
        }

        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
        assert!(
            app.chat_ui
                .attempt(card)
                .is_some_and(|attempt| attempt.recovery.is_some()),
            "sending again retries the switch"
        );
        assert_eq!(
            chat_runtime(&app, card).error,
            None,
            "the earlier refusal does not stand over the retry"
        );

        // The operation the daemon was finishing ended this conversation's
        // connection during the wait. Its wake arrives while the switch is
        // pending and goes unread there, so the refusal reads the connection
        // itself: the card offers its reconnect rather than advice to send
        // again, which a disconnected card can't, and the account is still
        // not called removed.
        handle.disconnect_for_test();
        drop(app.update(Message::ChatUi(Action::Wake(
            target,
            Wake::Disconnected("The daemon stopped".into()),
        ))));
        assert!(app.chat_start_pending(card));
        assert!(
            !chat_runtime(&app, card).disconnected,
            "the wake was not read while the switch was pending"
        );
        let attempt = chat_runtime_attempt(&app, card);
        answer_start(
            &mut app,
            attempt,
            Err(chat::StartFailure {
                message: message.into(),
                recovery,
            }),
        );
        assert!(!app.chat_ui.removed_accounts.contains(&card));
        assert!(
            app.chat_ui
                .retiring
                .iter()
                .all(|retiring| retiring.card != card)
        );
        let runtime = chat_runtime(&app, card);
        assert!(runtime.disconnected, "the lost connection is shown");
        assert_eq!(runtime.error, None, "no advice to send again");
        assert_eq!(runtime.composer.text().trim(), "Keep this message");
        assert!(
            app.automatic_chat_account(target, PromptMode::Send)
                .is_none(),
            "a disconnected card starts no switch from its old connection"
        );
    }
}

/// The conversation's provider exited while the daemon was busy, which
/// the reader learns only after the refusal. Its exit and recovery then
/// replace the advice to send again; the message stays in the composer.
#[test]
fn a_busy_refusal_gives_way_to_an_exit_reported_after_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = limited_automatic_chat(&mut app, "Keep this message");
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
    let attempt = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure {
            message: crate::terminal_ipc::DAEMON_BUSY.into(),
            recovery: chat::Recovery::Busy,
        }),
    );
    assert_eq!(
        chat_runtime(&app, card).error.as_deref(),
        Some(chat::failure::delivery::ACCOUNT_SWITCH_BUSY)
    );
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.exited = true;
    handle.publish_snapshot_for_test(snapshot);
    drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.snapshot.exited);
    assert_eq!(runtime.error, None);
    assert_eq!(runtime.composer.text().trim(), "Keep this message");
    assert!(!app.chat_ui.removed_accounts.contains(&card));
    assert!(!app.chat_ui.restorable_accounts.contains_key(&card));
}

/// A conversation whose provider exited during the wait keeps its exit as
/// the reason, not advice to send again.
#[test]
fn a_refusal_after_a_known_exit_leaves_the_exit_as_the_reason() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = limited_automatic_chat(&mut app, "Keep this message");
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
    let attempt = chat_runtime_attempt(&app, card);
    // The provider exits while the switch waits; its wake goes unread
    // while the switch is pending.
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.exited = true;
    handle.publish_snapshot_for_test(snapshot);
    drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
    assert!(!chat_runtime(&app, card).snapshot.exited);
    answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure {
            message: crate::terminal_ipc::DAEMON_BUSY.into(),
            recovery: chat::Recovery::Busy,
        }),
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.snapshot.exited, "the refusal reads the exit");
    assert_eq!(runtime.error, None);
    assert_eq!(runtime.composer.text().trim(), "Keep this message");
    assert!(!app.chat_ui.removed_accounts.contains(&card));
    assert!(!app.chat_ui.restorable_accounts.contains_key(&card));
}

/// A switch that never reached a daemon able to answer it asked nothing,
/// so the conversation stays as it was, as with a busy refusal, but the
/// notice doesn't claim another operation held it: one that took the
/// connection and never answered may be hung, and one that couldn't be
/// reached may be gone.
#[test]
fn an_unadmitted_switch_keeps_the_conversation_without_claiming_busy() {
    for (recovery, message, notice) in [
        (
            chat::Recovery::Unresponsive,
            "could not verify the running cagents daemon: the daemon did not answer within 30 s",
            chat::failure::delivery::ACCOUNT_SWITCH_UNANSWERED,
        ),
        (
            chat::Recovery::Unreached,
            "could not verify the running cagents daemon: connection closed",
            chat::failure::delivery::ACCOUNT_SWITCH_UNREACHED,
        ),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = limited_automatic_chat(&mut app, "Keep this message");
        let source = chat_runtime(&app, card).session.clone();
        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
        let attempt = chat_runtime_attempt(&app, card);
        answer_start(
            &mut app,
            attempt,
            Err(chat::StartFailure {
                message: message.into(),
                recovery,
            }),
        );
        assert!(
            !app.chat_ui.removed_accounts.contains(&card),
            "{recovery:?}"
        );
        assert!(!app.chat_ui.restorable_accounts.contains_key(&card));
        assert!(
            app.chat_ui
                .retiring
                .iter()
                .all(|retiring| retiring.card != card)
        );
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.target, target);
        assert!(!runtime.disconnected);
        assert_eq!(runtime.session, source);
        assert_eq!(runtime.error.as_deref(), Some(notice));
        assert!(!notice.contains("busy"));
        assert!(chat::failure::answers_one_send(notice));
        assert_eq!(runtime.reported_errors().next(), None);
        assert_eq!(runtime.composer.text().trim(), "Keep this message");
    }
}

/// Any other failed switch still leaves the removed-account recovery in
/// place, because the daemon may have closed the original by then.
#[test]
fn a_failed_automatic_switch_that_was_not_busy_still_offers_account_recovery() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = limited_automatic_chat(&mut app, "Keep this message");
    drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
    let attempt = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure::retry(
            "The replacement provider did not start",
        )),
    );
    assert!(app.chat_ui.removed_accounts.contains(&card));
    assert!(chat_runtime(&app, card).disconnected);
    retired(&mut app, card);
}

/// A new conversation, which runs nothing until its first message is
/// sent. Tasks are dropped throughout: nothing here starts a provider or
/// reaches a daemon.
pub(super) fn draft_chat(app: &mut Cagents) -> CardId {
    draft_chat_for_provider(app, Provider::Codex)
}
/// A key nothing took, typed while a conversation is focused, goes into
/// its message input, and that update projects everything: only a key
/// nothing answered projects nothing, so declaring this one narrower
/// fails here.
#[test]
fn a_key_typed_into_an_unfocused_composer_projects_everything() {
    let mut app = super::super::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let composer = |app: &Cagents| chat_runtime(app, card).composer.text();
    let before = composer(&app);
    let projections = super::super::terminal_output_tests::projections();
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("x".into()),
            modified_key: keyboard::Key::Character("x".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyX),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("x".into()),
            repeat: false,
        },
        captured: false,
    }));
    assert_eq!(composer(&app).trim_end(), format!("{}x", before.trim_end()));
    assert_eq!(
        super::super::terminal_output_tests::projections(),
        projections + 1
    );
}

#[test]
fn a_pane_inspector_overlay_keeps_typing_out_of_the_covered_chat() {
    let mut app = super::super::tests::context_inspector_test_app(false);
    let card = draft_chat_for_provider(&mut app, crate::agent::Provider::Claude);
    let resize = app.workspace.resize_viewport(500.0);
    drop(app.apply_viewport(resize, false, DurableChange::None));
    app.open_pane_inspector_now(card);
    assert_eq!(
        app.pane_inspector_layout(card),
        Some(InspectorLayout::Overlay)
    );
    let press = || Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("x".into()),
            modified_key: keyboard::Key::Character("x".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyX),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("x".into()),
            repeat: false,
        },
        captured: false,
    };
    let composer = |app: &Cagents| match app.cards.get(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime.composer.text(),
        _ => panic!("the draft stays a chat"),
    };
    let before = composer(&app);
    drop(app.open_chat_control(crate::workspace_widget::GlobalShortcut::ChatModel));
    assert!(
        chat_runtime(&app, card).model_picker.is_none(),
        "a shortcut cannot open a picker behind the inspector"
    );
    drop(app.update(press()));
    assert_eq!(
        composer(&app),
        before,
        "the hidden composer must not receive typing"
    );
    assert_eq!(app.focus_pane_keyboard_on_activation().units(), 0);
    if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
        runtime.picker_keyboard = true;
        runtime.account_picker = true;
    }
    assert!(
        !app.chat_picker_owns_keyboard(),
        "the covered picker cannot trap Tab"
    );
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        },
        captured: false,
    }));
    assert!(
        !app.pane_inspectors.is_open(card),
        "Escape closes the visible inspector"
    );
    if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
        assert!(
            runtime.account_picker,
            "Escape leaves the covered picker alone"
        );
        runtime.account_picker = false;
        runtime.picker_keyboard = false;
    }
    drop(app.set_pane_inspector_open(card, false));
    app.settle_pane_inspectors();
    drop(app.update(press()));
    assert!(
        composer(&app).starts_with('x'),
        "unclaimed typing works after closing the overlay"
    );
}

#[test]
fn a_pane_inspector_overlay_excludes_covered_widgets_and_keeps_other_panes_reachable() {
    use iced::advanced::widget::operation::{self, Focusable};
    use iced::advanced::{
        Layout, Shell, clipboard, layout,
        widget::{Operation, Tree},
    };

    #[derive(Default)]
    struct Controls {
        ids: Vec<widget::Id>,
        focused: Vec<widget::Id>,
    }
    impl Operation for Controls {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn Focusable,
        ) {
            if let Some(id) = id {
                self.ids.push(id.clone());
                if state.is_focused() {
                    self.focused.push(id.clone());
                }
            }
        }
    }
    fn view(app: &Cagents, card: CardId) -> Element<'_, Message> {
        widget::row::with_children(vec![
            widget::container(app.card_view(card, true, false))
                .width(500)
                .into(),
            crate::ui_controls::button::secondary("Other pane")
                .id(widget::Id::new("inspector-test.other-pane"))
                .on_press(Message::ToggleContextInspector)
                .into(),
        ])
        .into()
    }
    let renderer = crate::app::tests::headless_renderer();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat_for_provider(&mut app, Provider::Claude);
    let resize = app.workspace.resize_viewport(500.0);
    drop(app.apply_viewport(resize, false, DurableChange::None));
    write(&mut app, card, "keep");
    let composer = card_control_id(card, "chat-composer");
    let sibling = widget::Id::new("inspector-test.other-pane");
    let viewport = iced::Rectangle::with_size(iced::Size::new(640.0, 720.0));
    let mut tree = Tree::new(view(&app, card));
    {
        let mut element = view(&app, card);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, viewport.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(composer.clone()),
        );
        let mut controls = Controls::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut controls);
        assert!(
            controls.focused.contains(&composer),
            "start with the real editor focused"
        );
    }
    let key = iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Character("x".into()),
        modified_key: keyboard::Key::Character("x".into()),
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyX),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: Some("x".into()),
        repeat: false,
    });
    for phase in 0..3 {
        match phase {
            0 => app.open_pane_inspector_now(card),
            1 => drop(app.set_pane_inspector_open(card, false)),
            _ => app.settle_pane_inspectors(),
        }
        let covered = phase < 2;
        let mut messages = Vec::new();
        {
            let mut element = view(&app, card);
            element.as_widget_mut().diff(&mut tree);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, viewport.size()),
            );
            let mut controls = Controls::default();
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut controls,
            );
            assert_eq!(controls.ids.contains(&composer), !covered, "phase {phase}");
            assert!(
                controls.ids.contains(&sibling),
                "another pane stays in Tab traversal"
            );
            assert!(
                !controls.focused.contains(&composer),
                "covered focus cannot return after closing"
            );
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::focusable::focus::<()>(sibling.clone()),
            );
            let mut controls = Controls::default();
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut controls,
            );
            assert!(
                controls.focused.contains(&sibling),
                "the inspector does not trap another pane's keyboard"
            );
            // A forced focus request must not make a covered native editor
            // reachable; the same request works once the slide has ended.
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::focusable::focus::<()>(composer.clone()),
            );
            element.as_widget_mut().update(
                &mut tree,
                &key,
                Layout::new(&node),
                iced::mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        for message in messages {
            drop(app.update(message));
        }
        assert_eq!(
            chat_runtime(&app, card).composer.text().trim(),
            if covered { "keep" } else { "keepx" },
            "phase {phase}"
        );
    }
}

#[test]
fn workspace_switcher_preview_cleanup_preserves_visible_chats_with_pane_inspectors() {
    for (case, returned, covered) in [
        ("peer inspector overlay", true, false),
        ("same-pane inspector overlay", true, true),
        ("outside the returned workspace", false, false),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let left = draft_chat_for_provider(&mut app, Provider::Claude);
        let right = draft_chat_for_provider(&mut app, Provider::Claude);
        let durable = Arc::make_mut(&mut app.durable);
        durable.focused = Some(left);
        for card in durable.cards.iter_mut() {
            card.width_fraction = 0.5;
        }
        let mut workspace = (*app.durable).clone();
        if !returned {
            // A cancelled preview can belong to another workspace while
            // its runtime remains retained by the window.
            workspace.cards.retain(|card| card.id != right);
        }
        app.workspace = Cagents::rebuild_workspace(&workspace);
        let resize = app.workspace.resize_viewport(900.0);
        drop(app.apply_viewport(resize, false, DurableChange::None));
        let preview = if covered { left } else { right };
        assert_eq!(
            app.workspace.visibility(preview),
            returned.then_some(VisibilityClass::Visible),
            "{case}"
        );
        if returned {
            app.open_pane_inspector_now(left);
            assert_eq!(
                app.pane_inspector_layout(left),
                Some(InspectorLayout::Overlay),
                "{case}"
            );
            assert_eq!(app.pane_content_inputs_blocked(preview), covered, "{case}");
        }
        app.set_exposure(preview, Exposure::Visible);
        let target = chat_runtime(&app, preview).target;
        app.chat_runtime_mut(target)
            .unwrap()
            .set_visible(true)
            .unwrap();
        assert!(chat_runtime(&app, preview).visible.load(Ordering::Relaxed));
        app.chrome_ui.workspace_switcher_preview = Some(preview);

        drop(app.settle_workspace_switcher_preview());

        assert!(app.chrome_ui.workspace_switcher_preview.is_none(), "{case}");
        assert_eq!(
            app.exposure(preview),
            if returned {
                Exposure::Visible
            } else {
                Exposure::Offscreen
            },
            "{case}"
        );
        assert_eq!(
            chat_runtime(&app, preview).visible.load(Ordering::Relaxed),
            returned,
            "{case}"
        );
        if returned {
            assert!(app.pane_inspectors.is_open(left), "{case}");
            assert!(!app.pane_inspectors.is_open(right), "{case}");
        }
    }
}

pub(super) fn draft_chat_for_provider(app: &mut Cagents, provider: Provider) -> CardId {
    draft_chat_in(app, provider, Path::new(FIXTURE_DIRECTORY))
}

pub(super) fn draft_chat_in(app: &mut Cagents, provider: Provider, directory: &Path) -> CardId {
    let _ = app.new_native_chat(provider, Some(directory.to_owned()));
    let card = app.workspace.focused().unwrap();
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Chat(runtime)) if runtime.is_draft()),
        "a new conversation opens as a draft"
    );
    assert!(!app.chat_start_pending(card), "and starts nothing");
    card
}

pub(super) fn chat_runtime(app: &Cagents, card: CardId) -> &Runtime {
    match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime,
        _ => panic!("the card holds a chat"),
    }
}

fn presentation_child(id: &str, item: Arc<chat::Item>) -> chat::Subagent {
    chat::Subagent {
        id: id.into(),
        kind: "fixture".into(),
        description: id.into(),
        model: None,
        waiting: false,
        detail: String::new(),
        status: chat::ToolStatus::Running,
        background: true,
        items: vec![item],
    }
}
fn presentation_text(id: &str, text: &str) -> Arc<chat::Item> {
    Arc::new(chat::Item {
        id: id.into(),
        role: chat::Role::Assistant,
        content: ItemContent::Text {
            text: text.into(),
            thinking: false,
            phase: chat::Phase::Final,
        },
        at: None,
        started_at: None,
    })
}
fn child_find_tool(text: &str) -> Arc<chat::Item> {
    Arc::new(chat::Item {
        id: "same#opaque-id".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Inspect".into(),
            input: "{}".into(),
            output: text.into(),
            status: chat::ToolStatus::Succeeded,
        },
        at: None,
        started_at: None,
    })
}

#[test]
fn child_find_ownership_follows_window_pruning_and_preserves_manual_choices() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    app.chat_runtime_mut(target)
        .unwrap()
        .session
        .launch
        .provider = Provider::Claude;
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.items = vec![child_find_tool("parent-only")];
    let mut last_item = String::new();
    let mut last_part = String::new();
    for window in 0..4100 {
        let item = format!("window#{window}-tool");
        let coordinated = format!("window#{window}-card");
        let mut tool = (*child_find_tool("needle")).clone();
        tool.id = item.clone();
        let card_item = Arc::new(chat::Item {
            id: coordinated.clone(), role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: "SendMessage".into(),
                input: serde_json::json!({"to":"reviewer", "message": format!("{} needle", "Coordination body\n\n".repeat(16))}).to_string(),
                output: String::new(), status: chat::ToolStatus::Succeeded,
            }, at: None, started_at: None,
        });
        snapshot.generation += 1;
        snapshot.subagents = vec![chat::Subagent {
            items: vec![Arc::new(tool), card_item],
            ..presentation_child("same-child", child_find_tool("unused"))
        }];
        show_chat_snapshot_for_test(&mut app, card, snapshot.clone());
        if window == 0 {
            drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("same-child".into()))));
            drop(find_navigation::find_for_test(
                &mut app,
                target,
                "needle".into(),
            ));
        } else {
            drop(find_navigation::settle_current_find_for_test(
                &mut app, target,
            ));
            drop(app.handle_chat_ui(Action::FindNext(target, true)));
        }
        drop(app.handle_chat_ui(Action::FindNext(target, true)));
        last_item = item;
        last_part = coordinated;
    }
    let runtime = chat_runtime(&app, card);
    let choices = runtime.child_expanded["same-child"].len()
        + runtime
            .child_unfolded
            .get("same-child")
            .map_or(0, HashSet::len);
    assert!(
        choices <= 4098,
        "the existing choice budget prunes retired windows"
    );
    assert!(
        runtime.child_find_disclosure["same-child"].owned_count() <= choices,
        "Find ownership must follow the pruned child choice budget"
    );
    assert!(runtime.child_expanded["same-child"].contains(&last_item));
    // A step drawn as one row opens its tool call for the match.
    assert!(runtime.child_expanded["same-child"].contains(&last_part));
    drop(app.handle_chat_ui(Action::Toggle(target, last_item.clone())));
    drop(app.handle_chat_ui(Action::Toggle(target, last_item.clone())));
    drop(app.handle_chat_ui(Action::Toggle(target, last_part.clone())));
    drop(app.handle_chat_ui(Action::Toggle(target, last_part.clone())));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.child_expanded["same-child"],
        HashSet::from([last_item, last_part])
    );
    assert!(runtime.child_find_disclosure.is_empty());
    assert!(!runtime.expanded.contains("same#opaque-id"));
}

#[test]
fn selected_child_find_searches_visible_scope_and_preserves_explicit_disclosures() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![child_find_tool("parent-only needle")];
    snapshot.subagents = vec![
        presentation_child("one", child_find_tool("child-one-only needle")),
        presentation_child("two", child_find_tool("child-two-only needle")),
    ];
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("one".into()))));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "child-one-only".into(),
    ));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.find_matches.iter().collect::<Vec<_>>(),
        vec!["same#opaque-id"],
        "Find must search the visible child, not the parent"
    );
    assert!(runtime.child_expanded["one"].contains("same#opaque-id"));
    assert!(!runtime.expanded.contains("same#opaque-id"));
    drop(app.handle_chat_ui(Action::FindNext(target, false)));
    assert_eq!(chat_runtime(&app, card).find_index, 0);
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "parent-only".into(),
    ));
    assert!(chat_runtime(&app, card).find_matches.is_empty());
    assert!(!chat_runtime(&app, card).child_expanded["one"].contains("same#opaque-id"));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "needle".into(),
    ));
    drop(app.handle_chat_ui(Action::Toggle(target, "same#opaque-id".into())));
    drop(app.handle_chat_ui(Action::Toggle(target, "same#opaque-id".into())));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("two".into()))));
    assert_eq!(
        chat_runtime(&app, card)
            .find_matches
            .iter()
            .collect::<Vec<_>>(),
        vec!["same#opaque-id"]
    );
    drop(app.handle_chat_ui(Action::FindNext(target, true)));
    assert!(chat_runtime(&app, card).child_expanded["two"].contains("same#opaque-id"));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("one".into()))));
    let runtime = chat_runtime(&app, card);
    assert!(!runtime.child_expanded["two"].contains("same#opaque-id"));
    assert!(runtime.child_expanded["one"].contains("same#opaque-id"));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(chat_runtime(&app, card).child_expanded["one"].contains("same#opaque-id"));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "parent-only".into(),
    ));
    assert!(!chat_runtime(&app, card).find_matches.is_empty());
    assert!(chat_runtime(&app, card).expanded.contains("same#opaque-id"));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("one".into()))));
    assert!(chat_runtime(&app, card).find_matches.is_empty());
    assert!(!chat_runtime(&app, card).expanded.contains("same#opaque-id"));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(chat_runtime(&app, card).child_expanded["one"].contains("same#opaque-id"));
    assert!(
        chat_runtime(&app, card).commands.is_idle(),
        "Find is local UI state"
    );
}

#[test]
fn selected_child_find_restores_coordination_disclosures_only_in_their_original_scope() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture_for_provider(&mut app, Provider::Claude);
    let card_item = |scope: &str| {
        Arc::new(chat::Item {
        id: "same#opaque-id".into(), role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "SendMessage".into(),
            input: serde_json::json!({"to":"reviewer", "message": format!("{} {scope}-only", "Long coordination body\n\n".repeat(32))}).to_string(),
            output: String::new(), status: chat::ToolStatus::Succeeded,
        }, at: None, started_at: None,
    })
    };
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![card_item("parent")];
    snapshot.subagents = vec![
        presentation_child("one", card_item("one")),
        presentation_child("two", card_item("two")),
    ];
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    let part = "same#opaque-id";
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("one".into()))));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "one-only".into(),
    ));
    assert!(chat_runtime(&app, card).child_expanded["one"].contains(part));
    assert!(!chat_runtime(&app, card).expanded.contains(part));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("two".into()))));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    assert!(!chat_runtime(&app, card).child_expanded["one"].contains(part));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "two-only".into(),
    ));
    drop(app.handle_chat_ui(Action::Toggle(target, part.into())));
    drop(app.handle_chat_ui(Action::Toggle(target, part.into())));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(chat_runtime(&app, card).child_expanded["two"].contains(part));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "parent-only".into(),
    ));
    assert!(chat_runtime(&app, card).expanded.contains(part));
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(!chat_runtime(&app, card).expanded.contains(part));
    assert!(chat_runtime(&app, card).child_expanded["two"].contains(part));
}

#[test]
fn selected_child_find_refreshes_on_live_changes_and_child_retirement() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![child_find_tool("parent-only")];
    snapshot.subagents = vec![presentation_child("one", child_find_tool("before"))];
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("one".into()))));
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "streamed-needle".into(),
    ));
    assert!(chat_runtime(&app, card).find_matches.is_empty());
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents[0].items = vec![child_find_tool("before streamed-needle")];
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    drop(find_navigation::settle_current_find_for_test(
        &mut app, target,
    ));
    assert_eq!(
        chat_runtime(&app, card)
            .find_matches
            .iter()
            .collect::<Vec<_>>(),
        vec!["same#opaque-id"]
    );
    drop(app.handle_chat_ui(Action::FindNext(target, true)));
    assert!(chat_runtime(&app, card).child_expanded["one"].contains("same#opaque-id"));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents[0].items.clear();
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    assert!(chat_runtime(&app, card).find_matches.is_empty());
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "parent-only".into(),
    ));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.subagents.clear();
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    drop(find_navigation::settle_current_find_for_test(
        &mut app, target,
    ));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.selected_subagent.is_none());
    assert!(
        !runtime.find_matches.is_empty(),
        "retirement recomputes parent matches"
    );
    assert!(
        !runtime.expanded.contains("same#opaque-id"),
        "retired child's temporary choice must not open colliding parent"
    );
    assert!(runtime.child_expanded.is_empty());
    drop(app.handle_chat_ui(Action::CloseFind(target)));
    assert!(chat_runtime(&app, card).find_matches.is_empty());
}

#[test]
fn selected_presentation_admits_only_current_child_and_prunes_old_backing() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let parent = presentation_text("parent-answer", "Parent answer");
    let output = "complete tool output\n".repeat(8192);
    let tool = Arc::new(chat::Item {
        id: "child-tool".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "fixture".into(),
            input: "read".into(),
            output,
            status: chat::ToolStatus::Running,
        },
        at: None,
        started_at: None,
    });
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![parent.clone()];
    snapshot.subagents = (0..16)
        .map(|i| {
            presentation_child(
                &format!("child-{i}"),
                if i == 0 {
                    tool.clone()
                } else {
                    presentation_text(
                        &format!("answer-{i}"),
                        "**unselected** text\n".repeat(2048).as_str(),
                    )
                },
            )
        })
        .collect();
    handle.publish_snapshot_for_test(snapshot.clone());
    app.chat_runtime_mut(target).unwrap().refresh();
    let runtime = chat_runtime(&app, card);
    assert!(runtime.child_cache.is_empty());
    assert_eq!(runtime.cache.len(), 1);
    let parent_revision = runtime.cache[&parent.id].revision;
    let raw = runtime.snapshot.clone();
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child-0".into()))));
    drop(app.handle_chat_ui(Action::Toggle(target, tool.id.clone())));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.child_cache.len(), 1);
    assert!(Arc::ptr_eq(&raw, &runtime.snapshot));
    assert_eq!(runtime.cache[&parent.id].revision, parent_revision);
    let child_revision = runtime.child_cache[&tool.id].revision;
    let backing = Arc::downgrade(
        &runtime.child_cache[&tool.id]
            .tool_output
            .as_ref()
            .unwrap()
            .text,
    );
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child-0".into()))));
    assert_eq!(
        chat_runtime(&app, card).child_cache[&tool.id].revision,
        child_revision
    );
    snapshot.generation += 1;
    snapshot.subagents[15].items = vec![presentation_text("latest-hidden", "Hidden append")];
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert_eq!(
        chat_runtime(&app, card).child_cache[&tool.id].revision,
        child_revision
    );
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child-1".into()))));
    preparation::retirement_barrier();
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.child_cache.len(), 1);
    assert!(runtime.child_cache.contains_key("answer-1"));
    assert!(backing.upgrade().is_none());
    assert_eq!(runtime.snapshot.subagents.len(), 16);
    assert!(Arc::ptr_eq(&runtime.snapshot.subagents[0].items[0], &tool));
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    assert!(chat_runtime(&app, card).child_cache.is_empty());
    assert_eq!(
        chat_runtime(&app, card).cache[&parent.id].revision,
        parent_revision
    );
}
#[test]
fn selected_presentation_same_item_id_keeps_parent_and_child_copy_independent() {
    use iced::futures::StreamExt;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let parent = presentation_text("same-id", "Parent source");
    let child = presentation_text("same-id", "Child source");
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![parent.clone()];
    snapshot.subagents = vec![presentation_child("child", child.clone())];
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let turns = chat_runtime(&app, card)
        .turns
        .iter()
        .map(|turn| (turn.key.clone(), turn.summary.clone()))
        .collect::<Vec<_>>();
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
    let runtime = chat_runtime(&app, card);
    assert!(Arc::ptr_eq(
        runtime.cache["same-id"].item.as_ref().unwrap(),
        &parent
    ));
    assert!(Arc::ptr_eq(
        runtime.child_cache["same-id"].item.as_ref().unwrap(),
        &child
    ));
    assert_eq!(
        turns,
        runtime
            .turns
            .iter()
            .map(|turn| (turn.key.clone(), turn.summary.clone()))
            .collect::<Vec<_>>()
    );
    let copy = app.handle_chat_ui(Action::Copy(target, "same-id".into(), CopyPart::Text));
    let text = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let mut stream = iced::runtime::task::into_stream(copy).unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while let Some(effect) = stream.next().await {
                    if let iced::runtime::Action::Clipboard(
                        iced::runtime::clipboard::Action::Write { contents, .. },
                    ) = effect
                    {
                        return contents;
                    }
                }
                panic!("copy must emit a clipboard write");
            })
            .await
            .expect("copy effect deadline")
        });
    assert_eq!(text, "Child source");
    drop(app.handle_chat_ui(Action::Toggle(target, "same-id".into())));
    assert!(Arc::ptr_eq(
        chat_runtime(&app, card).cache["same-id"]
            .item
            .as_ref()
            .unwrap(),
        &parent
    ));
    assert!(Arc::ptr_eq(
        chat_runtime(&app, card).child_cache["same-id"]
            .item
            .as_ref()
            .unwrap(),
        &child
    ));
}
#[test]
fn selected_presentation_bounds_child_disclosure_history_without_crossing_transcripts() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture_for_provider(&mut app, Provider::Claude);
    let item = Arc::new(chat::Item {
        id: "same#opaque-id".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "SendMessage".into(),
            input: serde_json::json!({
                "to": "reviewer",
                "message": "Current coordination body\n\n".repeat(32),
            })
            .to_string(),
            output: String::new(),
            status: chat::ToolStatus::Succeeded,
        },
        at: None,
        started_at: None,
    });
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![item.clone()];
    snapshot.subagents = vec![
        presentation_child("long-running-child", item.clone()),
        presentation_child("other-child", item.clone()),
    ];
    handle.publish_snapshot_for_test(snapshot.clone());
    app.chat_runtime_mut(target).unwrap().refresh();
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
    drop(app.handle_chat_ui(Action::Unfold(target, format!("{}#body", item.id))));
    for child in ["long-running-child", "other-child"] {
        drop(app.handle_chat_ui(Action::SelectSubagent(target, Some(child.into()))));
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
        drop(app.handle_chat_ui(Action::Unfold(target, format!("{}#body", item.id))));
    }
    let runtime = app.chat_runtime_mut(target).unwrap();
    // Represent choices accumulated over evicted windows of the same
    // stable child. Each collection stays below the shared limit alone.
    runtime
        .child_expanded
        .get_mut("long-running-child")
        .unwrap()
        .edit()
        .extend((0..2048).map(|index| format!("retired-item-{index}")));
    runtime
        .child_unfolded
        .get_mut("long-running-child")
        .unwrap()
        .extend((0..2048).map(|index| format!("retired-item-{index}#body")));
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert!(runtime.expanded.contains(&item.id));
    assert!(runtime.unfolded.contains(&format!("{}#body", item.id)));
    for child in ["long-running-child", "other-child"] {
        assert_eq!(
            runtime.child_expanded[child],
            HashSet::from([item.id.clone()])
        );
        assert_eq!(
            runtime.child_unfolded[child],
            HashSet::from([format!("{}#body", item.id)]),
        );
    }
    assert_eq!(runtime.selected_subagent.as_deref(), Some("other-child"));
    assert_eq!(chat_runtime(&app, card).cache.len(), 1);
}

#[test]
fn selected_presentation_hide_releases_bodies_and_rehydrates_history_draft_and_selection() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let snapshot = || Snapshot {
        generation: 17,
        ready: true,
        session_id: Some(KNOWN_THREAD.into()),
        items: vec![Arc::new(chat::Item {
            id: "prompt".into(),
            role: chat::Role::User,
            content: text("Saved prompt"),
            at: None,
            started_at: None,
        })],
        subagents: vec![presentation_child(
            "child",
            presentation_text("answer", "Selected **answer**"),
        )],
        ..Snapshot::default()
    };
    show_chat_snapshot_for_test(&mut app, card, snapshot());
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
    let runtime = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(runtime);
    runtime.replace_draft(text_editor::Content::with_text("Unsent draft"));
    assert_eq!(runtime.history, ["Saved prompt"]);
    assert!(!runtime.child_cache["answer"].placeholder);
    let old_answer = Arc::downgrade(&runtime.snapshot.subagents[0].items[0]);
    runtime.set_visible(false).unwrap();
    runtime.set_visible(false).unwrap();
    runtime.cool();
    preparation::retirement_barrier();
    assert!(runtime.cache.is_empty() && runtime.child_cache.is_empty());
    assert_eq!(runtime.snapshot.transcript_bytes(), 0);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    // Client publications retire independently from presentation/parser
    // state. Observe this exact old body leaving both ownership graphs;
    // the presentation barrier alone cannot acknowledge client disposal.
    let deadline = Instant::now() + Duration::from_secs(5);
    while old_answer.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "the selected child's client retirement did not complete"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(old_answer.upgrade().is_none());
    assert_eq!(runtime.selected_subagent.as_deref(), Some("child"));
    assert_eq!(runtime.composer.text(), "Unsent draft");
    runtime.set_visible(true).unwrap();
    assert!(
        runtime.child_cache.is_empty(),
        "reveal awaits the authoritative body"
    );
    // Rehydration can legitimately answer at the same daemon generation.
    handle.replace_snapshot_for_test(snapshot());
    runtime.refresh();
    preparation::drain_fixture(runtime);
    assert_eq!(runtime.history, ["Saved prompt"]);
    assert_eq!(runtime.composer.text(), "Unsent draft");
    assert_eq!(runtime.selected_subagent.as_deref(), Some("child"));
    assert!(!runtime.child_cache["answer"].placeholder);
    assert_eq!(
        runtime.child_cache["answer"].item.as_ref().unwrap().content,
        text("Selected **answer**")
    );
    // An actual new publication removing the selected child reverts to parent.
    let mut removed = snapshot();
    removed.generation += 1;
    removed.subagents.clear();
    handle.publish_delta_snapshot_for_test(removed);
    runtime.refresh();
    preparation::drain_fixture(runtime);
    assert!(runtime.selected_subagent.is_none() && runtime.child_cache.is_empty());
    assert!(!runtime.cache["prompt"].placeholder);
    assert_eq!(
        runtime.cache["prompt"].item.as_ref().unwrap().content,
        text("Saved prompt")
    );
}

#[test]
fn pane_menu_actions_recheck_fork_availability_and_ignore_replaced_targets() {
    fn assert_no_effects(task: Task<Message>) {
        use cosmic::iced::futures::StreamExt;
        let count = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                match cosmic::iced::runtime::task::into_stream(task) {
                    Some(stream) => stream.count().await,
                    None => 0,
                }
            });
        assert_eq!(
            count, 0,
            "obsolete or unavailable menu action must not emit effects"
        );
    }
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    {
        let runtime = app.chat_runtime_mut(target).unwrap();
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.ready = true;
        snapshot.capabilities.branch = true;
        Arc::make_mut(&mut snapshot.file_changes).total_files = 1;
    }
    assert_eq!(app.chat_fork_target(card), Some(target));
    assert_eq!(app.chat_changes_target(card), Some(target));
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).busy = true;
    assert_eq!(app.chat_fork_target(card), None);
    let task = app.handle_chat_ui(Action::BranchPicker(target, true));
    assert_no_effects(task);
    assert!(!chat_runtime(&app, card).branch_picker);
    let replaced = Target {
        generation: target.generation + 1,
        ..target
    };
    for open in [true, false] {
        let task = app.handle_chat_ui(Action::ChangesPicker(replaced, open));
        assert_no_effects(task);
    }
    assert!(!chat_runtime(&app, card).changes.open);
    let Some(CardRuntime::Chat(runtime)) = app.cards.remove(&card) else {
        panic!("chat runtime");
    };
    app.chat_ui.parked.insert(card, runtime);
    let task = app.handle_chat_ui(Action::ChangesPicker(target, true));
    assert_no_effects(task);
    assert!(!app.chat_ui.parked.get(&card).unwrap().changes.open);
}

#[test]
fn chat_selection_commands_open_exclusive_pickers_without_changing_the_draft() {
    use crate::workspace_widget::GlobalShortcut;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.account_ui.add_managed_fixture(Provider::Codex, 22);
    let card = draft_chat(&mut app);
    let original = chat_runtime(&app, card).session.clone();
    let permission_mode = chat_runtime(&app, card).snapshot.permission_mode.clone();
    for command in [
        GlobalShortcut::ChatAccount,
        GlobalShortcut::ChatModel,
        GlobalShortcut::ChatPermissions,
    ] {
        drop(app.open_chat_control(command));
        let runtime = chat_runtime(&app, card);
        assert_eq!(
            runtime.account_picker,
            command == GlobalShortcut::ChatAccount
        );
        assert_eq!(
            runtime.model_picker.is_some(),
            command == GlobalShortcut::ChatModel
        );
        assert_eq!(
            runtime.permission_picker,
            command == GlobalShortcut::ChatPermissions
        );
        assert_eq!(runtime.session, original);
        assert_eq!(runtime.snapshot.permission_mode, permission_mode);
        assert!(!app.chat_start_pending(card));
        drop(app.open_chat_control(command));
        let runtime = chat_runtime(&app, card);
        assert!(
            !runtime.account_picker && runtime.model_picker.is_none() && !runtime.permission_picker
        );
        assert!(!runtime.picker_keyboard);
    }
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target)
        .unwrap()
        .attachment_chooser_pending = Some(uuid::Uuid::new_v4());
    drop(app.open_chat_control(GlobalShortcut::ChatAccount));
    assert!(!chat_runtime(&app, card).account_picker);
}

#[test]
fn chat_keyboard_pickers_own_tab_and_restore_the_shell_preference_on_close() {
    use crate::workspace_widget::GlobalShortcut;
    for initial in [false, true] {
        for command in [GlobalShortcut::ChatAccount, GlobalShortcut::ChatPermissions] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.account_ui.add_managed_fixture(Provider::Codex, 22);
            let card = draft_chat(&mut app);
            app.core.set_keyboard_nav(initial);
            drop(app.update(Message::RunCommand(command)));
            assert!(app.chat_picker_owns_keyboard());
            assert!(!app.core.keyboard_nav());
            drop(app.update(Message::RunCommand(command)));
            assert!(!app.chat_picker_owns_keyboard());
            assert_eq!(app.core.keyboard_nav(), initial);

            drop(app.update(Message::RunCommand(command)));
            assert!(!app.core.keyboard_nav());
            assert!(app.dismiss_chat_picker());
            app.sync_popup_keyboard_nav();
            assert_eq!(app.core.keyboard_nav(), initial);
            assert!(!chat_runtime(&app, card).picker_keyboard);
        }
    }
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    app.core.set_keyboard_nav(true);
    drop(app.update(Message::RunCommand(GlobalShortcut::ChatPermissions)));
    let target = chat_runtime(&app, card).target;
    drop(app.update(Message::ChatUi(Action::PermissionPage(
        target,
        permissions::Page::Rules,
    ))));
    assert!(!app.chat_picker_owns_keyboard());
    assert!(
        app.core.keyboard_nav(),
        "Rules page fields retain ordinary Tab traversal"
    );
}

#[test]
fn captured_escape_closes_model_search_without_closing_the_inspector() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let resize = app.workspace.resize_viewport(1600.0);
    drop(app.apply_viewport(resize, false, DurableChange::None));
    app.open_pane_inspector_now(card);
    assert_eq!(
        app.pane_inspector_layout(card),
        Some(InspectorLayout::Docked)
    );
    drop(app.open_chat_control(crate::workspace_widget::GlobalShortcut::ChatModel));
    assert!(chat_runtime(&app, card).model_picker.is_some());
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        },
        captured: true,
    }));
    assert!(chat_runtime(&app, card).model_picker.is_none());
    assert!(app.pane_inspectors.is_open(card));
}

#[test]
fn account_picker_keyboard_reaches_manage_accounts_and_activates_it() {
    use crate::workspace_widget::GlobalShortcut;
    use cosmic::iced::advanced::clipboard;
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    use cosmic::iced::{Event, Size, futures::StreamExt, mouse};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.account_ui.add_managed_fixture(Provider::Codex, 22);
    let mut renderer = crate::app::tests::headless_renderer();
    let card = draft_chat(&mut app);
    drop(app.open_chat_control(GlobalShortcut::ChatAccount));
    let press = |key, code| keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(key),
        modified_key: keyboard::Key::Named(key),
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    };
    let up = press(keyboard::key::Named::ArrowUp, keyboard::key::Code::ArrowUp);
    let down = press(
        keyboard::key::Named::ArrowDown,
        keyboard::key::Code::ArrowDown,
    );
    let runtime = chat_runtime(&app, card);
    let accounts = app
        .chat_accounts(
            runtime.session.launch.provider,
            &runtime.session.launch.recipe,
        )
        .unwrap();
    let choices = accounts.choices.len();
    assert!(app.chat_picker_key(&up).is_some());
    assert_eq!(chat_runtime(&app, card).picker_focus, choices);
    assert!(app.chat_picker_key(&down).is_some());
    assert_eq!(chat_runtime(&app, card).picker_focus, 0);
    let mut navigation = None;
    for _ in 0..choices {
        navigation = Some(app.chat_picker_key(&down).expect("next account control"));
    }
    assert_eq!(chat_runtime(&app, card).picker_focus, choices);
    let mut messages = Vec::new();
    {
        let mut ui = UserInterface::build(
            app.chat_view(card),
            Size::new(320.0, 600.0),
            Cache::new(),
            &mut renderer,
        );
        // At compact widths the account menu is nested inside More.
        // UserInterface traverses those live overlays when applying the
        // actual arrow-navigation task and dispatching Enter.
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            let mut stream = cosmic::iced::runtime::task::into_stream(
                navigation.expect("the account ring has a next control"),
            )
            .expect("navigation produces a widget operation");
            let mut applied = false;
            while let Ok(Some(cosmic::iced::runtime::Action::Widget(mut operation))) =
                tokio::time::timeout(Duration::from_millis(250), stream.next()).await
            {
                ui.operate(&renderer, operation.as_mut());
                applied = true;
            }
            assert!(applied, "apply the native account-navigation operation");
        });
        executor.shutdown_background();
        ui.update(
            &[Event::Keyboard(press(
                keyboard::key::Named::Enter,
                keyboard::key::Code::Enter,
            ))],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
    }
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::AccountUi(crate::app::account_ui::Action::OpenPane)
    )));
    assert!(!app.chat_start_pending(card));
}

pub(super) fn permission_key(
    key: keyboard::key::Named,
    modifiers: keyboard::Modifiers,
) -> keyboard::Event {
    keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(key),
        modified_key: keyboard::Key::Named(key),
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    }
}

/// Run the actual navigation widget operation on the rendered popup and
/// press Enter on its focused button, rather than checking ID arithmetic.
fn permission_navigation_messages(
    app: &mut Cagents,
    event: &keyboard::Event,
    enabled: bool,
) -> Vec<Message> {
    use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
    use cosmic::iced::{Event, Rectangle, Size, futures::StreamExt};
    let task = app.chat_picker_key(event).unwrap();
    let card = app.workspace.focused().unwrap();
    let renderer = crate::app::tests::headless_renderer();
    let mut element = chat_runtime(app, card).permissions_view(enabled, 520.0);
    let mut tree = Tree::new(element.as_widget());
    let viewport = Rectangle::with_size(Size::new(520.0, 1600.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) else {
            panic!("navigation did not produce a widget operation")
        };
        while let Ok(Some(cosmic::iced::runtime::Action::Widget(mut operation))) =
            tokio::time::timeout(Duration::from_millis(250), stream.next()).await
        {
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                operation.as_mut(),
            );
        }
    });
    executor.shutdown_background();
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        &mut tree,
        &Event::Keyboard(permission_key(
            keyboard::key::Named::Enter,
            keyboard::Modifiers::empty(),
        )),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    messages
}

#[test]
fn permission_approval_popup_and_transcript_keep_distinct_named_widget_state() {
    use cosmic::iced::advanced::{Layout, layout, mouse, renderer, widget::Tree};
    use cosmic::iced::{Rectangle, Size};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("permissions"));
    let runtime = match app.cards.get_mut(&card).unwrap() {
        CardRuntime::Chat(runtime) => runtime,
        _ => unreachable!(),
    };
    runtime.session.launch.provider = Provider::Claude;
    Arc::make_mut(&mut runtime.snapshot).requests = vec![chat::Request {
        kind: crate::chat::RequestKind::Approval,
        id: "approval".into(),
        title: "Allow Bash?".into(),
        description: String::new(),
        fields: vec![],
        actions: ["deny", "allow", "allowForSession"]
            .map(|id| chat::Choice {
                id: id.into(),
                label: id.into(),
                description: String::new(),
            })
            .to_vec(),
    }];
    let runtime = chat_runtime(&app, card);
    let render = || -> Element<'_, Message> {
        widget::column::with_children(vec![
            view::request_view(
                runtime.target,
                &runtime.snapshot.requests[0],
                &runtime.answers,
                true,
                view::RequestSurface::Transcript,
            ),
            runtime.permissions_view(true, 520.0),
        ])
        .into()
    };
    let mut renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(render().as_widget());
    let viewport = Rectangle::with_size(Size::new(520.0, 1600.0));
    // Native UI extracts named states globally between frames. Duplicate
    // transcript/popup answer IDs previously left one copy stateless and
    // crashed its next button draw despite a correct initial tree.
    for _ in 0..3 {
        cosmic::iced::advanced::widget::tree::NAMED.with(|named| {
            *named.borrow_mut() = tree.take_all_named();
        });
        let mut element = render();
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::theme::Theme::default(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
    }
    cosmic::iced::advanced::widget::tree::NAMED.with(|named| named.borrow_mut().clear());
}

#[test]
fn permission_keyboard_traverses_visible_approvals_pages_retry_and_close_both_ways() {
    use crate::workspace_widget::GlobalShortcut;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("permissions"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.show(true);
    let handle = runtime.handle().unwrap().clone();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.permission_mode = Some("default".into());
    snapshot.capabilities.permission_modes = vec!["default".into(), "plan".into()];
    snapshot.requests = vec![chat::Request {
        kind: crate::chat::RequestKind::Approval,
        id: "approval".into(),
        title: "Allow Bash?".into(),
        description: String::new(),
        fields: vec![],
        actions: ["allowForSession", "allow", "deny"]
            .map(|id| chat::Choice {
                id: id.into(),
                label: id.into(),
                description: String::new(),
            })
            .to_vec(),
    }];
    snapshot.permission_denials = [false, true]
        .into_iter()
        .map(|exact| chat::PermissionDenial {
            tool_use_id: if exact { "exact" } else { "unknown" }.into(),
            tool_name: "Bash".into(),
            tool_input: exact.then(|| r#"{"command":"git status"}"#.into()),
            reason: "Blocked".into(),
            reason_type: None,
            agent_id: None,
        })
        .collect();
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    drop(app.open_chat_control(GlobalShortcut::ChatPermissions));
    settle(&mut app, card, Ok(()));
    let describe = |messages: Vec<Message>| {
        assert_eq!(
            messages.len(),
            1,
            "exactly the focused popup button should activate"
        );
        match messages.into_iter().next().unwrap() {
            Message::ChatUi(Action::PermissionPicker(_, false)) => "close".into(),
            Message::ChatUi(Action::PermissionPage(_, page)) => format!("page:{page:?}"),
            Message::ChatUi(Action::Answer(_, request, answer)) => {
                format!("answer:{request}:{answer}")
            }
            Message::ChatUi(Action::PermissionRetry(_, id)) => format!("retry:{id}"),
            Message::ChatUi(Action::PermissionDismiss(_, id)) => format!("dismiss:{id}"),
            Message::ChatUi(Action::PermissionMode(_, mode)) => format!("mode:{mode}"),
            _ => panic!("unexpected popup action"),
        }
    };
    let expected = [
        "mode:plan",
        "close",
        "page:Blocked",
        "page:Rules",
        "page:Environment",
        "answer:approval:deny",
        "answer:approval:allow",
        "answer:approval:allowForSession",
        "retry:exact",
        "dismiss:exact",
        "dismiss:unknown",
        "mode:default",
    ];
    for name in expected {
        let messages = permission_navigation_messages(
            &mut app,
            &permission_key(keyboard::key::Named::Tab, keyboard::Modifiers::empty()),
            true,
        );
        assert_eq!(describe(messages), name);
        assert!(
            chat_runtime(&app, card).commands.is_idle(),
            "traversal itself submits no provider action"
        );
    }
    for name in expected
        .into_iter()
        .rev()
        .cycle()
        .skip(1)
        .take(expected.len())
    {
        assert_eq!(
            describe(permission_navigation_messages(
                &mut app,
                &permission_key(keyboard::key::Named::Tab, keyboard::Modifiers::SHIFT),
                true
            )),
            name
        );
    }
}

#[test]
fn permission_keyboard_skips_disabled_actions_and_closes_an_unsent_popup() {
    use crate::workspace_widget::GlobalShortcut;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    drop(app.open_chat_control(GlobalShortcut::ChatPermissions));
    // A disconnected/exited runtime leaves only Close enabled on Codex.
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).exited = true;
    let messages = permission_navigation_messages(
        &mut app,
        &permission_key(
            keyboard::key::Named::ArrowDown,
            keyboard::Modifiers::empty(),
        ),
        false,
    );
    assert!(matches!(
        messages.as_slice(),
        [Message::ChatUi(Action::PermissionPicker(_, false))]
    ));
    for message in messages {
        drop(app.handle_chat_ui(match message {
            Message::ChatUi(action) => action,
            _ => unreachable!(),
        }));
    }
    assert!(!chat_runtime(&app, card).permission_picker);
    assert!(!app.chat_start_pending(card));
}

#[test]
fn permission_picker_keyboard_navigation_selects_a_mode_on_enter() {
    use crate::workspace_widget::GlobalShortcut;
    use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
    use cosmic::iced::{Event, Rectangle, Size};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let renderer = crate::app::tests::headless_renderer();
    let card = draft_chat(&mut app);
    drop(app.open_chat_control(GlobalShortcut::ChatPermissions));
    let press = |key, code| keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(key),
        modified_key: keyboard::Key::Named(key),
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    };
    let down = press(
        keyboard::key::Named::ArrowDown,
        keyboard::key::Code::ArrowDown,
    );
    let original = chat_runtime(&app, card).snapshot.permission_mode.clone();
    let count = chat_runtime(&app, card).permission_controls(true).len();
    assert!(count > 1);
    let initial = chat_runtime(&app, card).picker_focus;
    assert!(app.chat_picker_key(&down).is_some());
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.picker_focus, (initial + 1) % count);
    assert_eq!(runtime.snapshot.permission_mode, original);
    // The close control now participates in traversal too. Continue to
    // the next mode when the current mode was the last visible control.
    if runtime.picker_focus == 0 {
        drop(app.chat_picker_key(&down));
    }
    let runtime = chat_runtime(&app, card);
    let chosen_id = runtime.permission_controls(true)[runtime.picker_focus].clone();
    let chosen = runtime
        .snapshot
        .capabilities
        .permission_modes
        .iter()
        .find(|mode| card_control_id(card, &format!("chat-permission-mode-{mode}")) == chosen_id)
        .unwrap()
        .clone();
    let mut messages = Vec::new();
    {
        let mut element = runtime.permissions_view(true, 320.0);
        let mut tree = Tree::new(element.as_widget());
        let viewport = Rectangle::with_size(Size::new(320.0, 600.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                card,
                &format!("chat-permission-mode-{chosen}"),
            )),
        );
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(press(
                keyboard::key::Named::Enter,
                keyboard::key::Code::Enter,
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &viewport,
        );
    }
    assert!(messages.iter().any(|message| matches!(message, Message::ChatUi(Action::PermissionMode(_, mode)) if mode == &chosen)));
    for message in messages {
        drop(app.update(message));
    }
    assert_eq!(
        chat_runtime(&app, card).snapshot.permission_mode.as_ref(),
        Some(&chosen)
    );
    assert!(!app.chat_start_pending(card));
}

#[test]
fn default_pane_is_ephemeral_but_explicit_chats_are_not() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
    let automatic = app.workspace.focused().unwrap();
    assert!(chat_runtime(&app, automatic).is_draft());
    assert!(!app.chat_start_pending(automatic));
    assert!(
        !app.persistent_workspace()
            .cards
            .iter()
            .any(|card| card.id == automatic)
    );
    let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
    assert_eq!(app.workspace.focused(), Some(automatic));
    let explicit = draft_chat(&mut app);
    let _ = app.select_worktree("/workspace/elsewhere".into(), false);
    assert!(app.card_state(automatic).is_none());
    assert!(!app.cards.contains_key(&automatic));
    assert!(app.card_state(explicit).unwrap().existence.is_open());
    let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
    assert_eq!(app.workspace.focused(), Some(explicit));
}

/// Entering an empty worktree from the sidebar opens a placeholder, and
/// leaving discards it. Neither is ever saved, so both wait for the
/// navigation's own deferred save; a placeholder someone typed into is
/// an ordinary pane from then on and is saved as the worktree is left.
#[test]
fn placeholders_save_with_the_navigation_that_opens_and_leaves_them() {
    use crate::app::terminal_output_tests::persists;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let saved = persists();
    drop(app.navigate_to_worktree(FIXTURE_DIRECTORY.into(), true));
    let automatic = app.workspace.focused().unwrap();
    assert!(app.chat_is_unused_default(automatic));
    assert_eq!(persists(), saved, "opening the placeholder waits");
    assert!(app.navigation_save.is_some());
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert_eq!(persists(), saved + 1, "the navigation is saved once");
    let written = app.persistent_workspace();
    assert!(written.cards.iter().all(|card| card.id != automatic));
    assert_ne!(written.focused, Some(automatic));

    drop(app.navigate_to_worktree("/workspace/elsewhere".into(), true));
    assert!(app.card_state(automatic).is_none());
    assert_eq!(persists(), saved + 1, "discarding the placeholder waits");
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert_eq!(persists(), saved + 2);

    drop(app.navigate_to_worktree(FIXTURE_DIRECTORY.into(), true));
    let used = app.workspace.focused().unwrap();
    write(&mut app, used, "keep this message");
    let before = persists();
    drop(app.navigate_to_worktree("/workspace/elsewhere".into(), true));
    assert_eq!(
        persists(),
        before + 1,
        "a used placeholder is saved at once"
    );
    assert!(
        app.persistent_workspace()
            .cards
            .iter()
            .any(|state| state.id == used)
    );
}

#[test]
fn default_pane_keeps_unsent_work_and_in_flight_starts() {
    for send in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
        let card = app.workspace.focused().unwrap();
        write(&mut app, card, "keep this message");
        if send {
            submit(&mut app, card);
            assert!(app.chat_start_pending(card));
        }
        let _ = app.select_worktree("/workspace/elsewhere".into(), false);
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(
            app.persistent_workspace()
                .cards
                .iter()
                .any(|state| state.id == card)
        );
        let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
        assert_eq!(app.workspace.focused(), Some(card));
        if !send {
            assert_eq!(
                chat_runtime(&app, card).composer.text(),
                "keep this message"
            );
        }
    }
}

#[test]
fn window_close_protects_attachments_local_followups_and_request_answers() {
    for kind in 0..3 {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("expected chat");
        };
        match kind {
            // A file is saved with the draft; an image the workspace
            // cannot save is not.
            0 => runtime
                .attachments
                .push(malformed_image("/workspace/draft.png")),
            1 => runtime
                .followups
                .push(followup(0, "accepted local task", &[])),
            _ => {
                runtime.answers.insert(
                    ("request".into(), "question".into()),
                    vec!["partial answer".into()],
                );
            }
        }
        let window = cosmic::iced::window::Id::unique();
        app.core.set_main_window_id(Some(window));
        drop(app.update(Message::WindowCloseRequested(window)));
        assert!(app.exit.is_none());
        assert_eq!(app.transient_work_close, Some(window));
        drop(app.update(Message::CancelTransientWorkClose));
        assert!(app.window_close_transient_blocker().is_some());
        if let Some(CardRuntime::Chat(runtime)) = app.cards.remove(&card) {
            app.chat_ui.parked.insert(card, runtime);
        }
        drop(app.update(Message::WindowCloseRequested(window)));
        assert!(app.exit.is_none(), "hidden chat input also blocks closing");
        assert_eq!(app.transient_work_close, Some(window));
    }
}

#[test]
fn managed_image_drafts_can_close_with_their_saved_receipts() {
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let text = format!("keep {} here", image.reference.as_ref().unwrap().token());
        write(&mut app, card, &text);
        let target = chat_runtime(&app, card).target;
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_attachments(vec![image.clone()]);
        assert!(app.window_close_transient_blocker().is_none());
        let saved = app.persistent_workspace();
        assert_eq!(saved.chat_drafts[&card], text);
        assert_eq!(saved.chat_draft_attachments[&card], [image.clone()]);
        let CardRuntime::Chat(runtime) = app.cards.remove(&card).unwrap() else {
            unreachable!()
        };
        let session = runtime.session.clone();
        app.chat_ui.parked.insert(card, runtime);
        assert!(app.parked_chat_restart_blocker().is_none());
        app.chat_ui = State::with_draft_attachments(
            saved.chat_drafts.clone(),
            saved.chat_draft_attachments.clone(),
        );
        drop(app.open_chat_draft(card, session));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text(), text);
        assert_eq!(runtime.attachments, [image]);
        assert!(app.window_close_transient_blocker().is_none());
    });
}

/// A file the person chose has no managed identity, but it is as much a
/// part of the draft as its text and images (#2202).
/// A draft of only a file is work in this window, which another window
/// must not adopt or close over it, now that the file no longer blocks.
#[test]
fn a_draft_of_only_a_file_is_local_work() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    assert!(!app.shared_chat_has_local_work(card));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_attachments(vec![Attachment::file("/project/notes.txt")]);
    assert!(chat_runtime(&app, card).transition_blocker().is_none());
    assert!(app.shared_chat_has_local_work(card));
}

#[test]
fn unsent_files_survive_saving_reopening_and_closing_with_the_draft() {
    chat::attachment_assets::with_test_image(|image| {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes.txt");
        std::fs::write(&notes, "notes").unwrap();
        let file = Attachment::file(&notes);
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat_for_provider(&mut app, Provider::Claude);
        let text = format!(
            "read this with {}",
            image.reference.as_ref().unwrap().token()
        );
        write(&mut app, card, &text);
        let target = chat_runtime(&app, card).target;
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_attachments(vec![image.clone(), file.clone()]);
        let saved = app.persistent_workspace();
        assert_eq!(saved.chat_drafts[&card], text);
        assert_eq!(
            saved.chat_draft_attachments[&card],
            [image.clone(), file.clone()]
        );
        assert_eq!(
            app.window_close_transient_blocker(),
            None,
            "a saved file no longer needs the live window"
        );

        // The runtime is dropped while the card lives on.
        app.chat_forget(card);
        assert_eq!(
            app.chat_ui.draft_attachments[&card],
            [image.clone(), file.clone()]
        );

        // The window reopens from what was saved.
        let CardRuntime::Chat(runtime) = app.cards.remove(&card).unwrap() else {
            unreachable!()
        };
        let session = runtime.session.clone();
        app.chat_ui = State::with_draft_attachments(
            saved.chat_drafts.clone(),
            saved.chat_draft_attachments.clone(),
        );
        drop(app.open_chat_draft(card, session));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text(), text);
        assert_eq!(runtime.attachments, [image, file]);
    });
}

#[test]
fn image_draft_close_keeps_pending_and_unpersistable_work_protected() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.replace_attachments(vec![image.clone()]);
        let ticket = runtime.begin_attachment_insert();
        assert!(
            runtime
                .restart_blocker()
                .unwrap()
                .contains("image attachment")
        );
        assert!(runtime.finish_attachment_insert(ticket.id, "", vec![]));
        assert!(runtime.restart_blocker().is_none());
        // A file is saved with the draft, so it no longer holds the
        // window open.
        runtime.replace_attachments(vec![Attachment::file("/unsaved.txt")]);
        assert!(runtime.restart_blocker().is_none());
        // A recalled managed image must not hide the original unsent
        // malformed image, which the workspace cannot preserve.
        let mut malformed = image.clone();
        malformed.reference.as_mut().unwrap().sha256.clear();
        runtime.remember_with_attachments("previous image".into(), vec![image]);
        runtime.replace_attachments(vec![malformed.clone()]);
        runtime.recall(true);
        assert!(
            runtime
                .attachments
                .iter()
                .all(|attachment| attachment != &malformed)
        );
        assert!(
            runtime
                .restart_blocker()
                .unwrap()
                .contains("attached images")
        );
        runtime.recall(false);
        assert!(
            runtime
                .restart_blocker()
                .unwrap()
                .contains("attached images")
        );
    });
}

#[test]
fn update_snapshot_restores_live_stashed_and_parked_composers() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let live = draft_chat(&mut app);
    write(&mut app, live, "live draft");
    assert!(chat_runtime(&app, live).restart_blocker().is_none());
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&live) else {
        panic!("expected live chat runtime");
    };
    // A file is saved with the draft; an image the workspace cannot save
    // holds the update back.
    runtime
        .attachments
        .push(Attachment::file("/workspace/example.txt"));
    assert!(runtime.restart_blocker().is_none());
    runtime
        .attachments
        .push(malformed_image("/workspace/example.png"));
    assert!(runtime.restart_blocker().is_some());
    runtime.attachments.clear();

    let stashed = draft_chat(&mut app);
    write(&mut app, stashed, "stashed draft");
    let stashed_session = chat_runtime(&app, stashed).session.clone();
    app.chat_forget(stashed);
    app.cards.insert(stashed, CardRuntime::Loading);

    let parked = draft_chat(&mut app);
    write(&mut app, parked, "parked draft");
    let CardRuntime::Chat(runtime) = app.cards.remove(&parked).unwrap() else {
        panic!("expected chat runtime");
    };
    app.chat_ui.parked.insert(parked, runtime);

    let saved = app.persistent_workspace();
    assert_eq!(saved.chat_drafts.get(&live).unwrap(), "live draft");
    assert_eq!(saved.chat_drafts.get(&stashed).unwrap(), "stashed draft");
    assert_eq!(saved.chat_drafts.get(&parked).unwrap(), "parked draft");

    app.chat_ui = State::with_drafts(saved.chat_drafts.clone());
    let _ = app.open_chat_draft(stashed, stashed_session);
    assert_eq!(chat_runtime(&app, stashed).composer.text(), "stashed draft");

    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&live) else {
        panic!("expected live chat runtime");
    };
    runtime.replace_draft(text_editor::Content::new());
    let saved = app.persistent_workspace();
    assert!(!saved.chat_drafts.contains_key(&live));

    close_chat(&mut app, stashed);
    assert!(
        !app.persistent_workspace()
            .chat_drafts
            .contains_key(&stashed)
    );
}

/// A draft is copied into the saved workspace when it changes. Saves
/// after that which change nothing hand the writer the workspace already
/// held instead of a fresh copy of every card in it.
#[test]
fn saves_after_a_draft_change_copy_the_workspace_only_when_it_changes_again() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    write(&mut app, card, "kept draft");
    let first = app.persistent_workspace();
    assert_eq!(first.chat_drafts[&card], "kept draft");
    assert!(
        Arc::ptr_eq(&first, &app.durable),
        "durable holds the draft it saved"
    );
    let again = app.persistent_workspace();
    assert!(
        Arc::ptr_eq(&again, &first),
        "an unchanged save copies nothing"
    );

    write(&mut app, card, " and more");
    let changed = app.persistent_workspace();
    assert!(!Arc::ptr_eq(&changed, &first));
    assert_eq!(changed.chat_drafts[&card], "kept draft and more");
    assert_eq!(
        first.chat_drafts[&card], "kept draft",
        "a snapshot already handed to the writer stays as it was saved"
    );
    assert!(Arc::ptr_eq(&app.persistent_workspace(), &changed));

    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("expected live chat runtime");
    };
    runtime.replace_draft(text_editor::Content::new());
    let cleared = app.persistent_workspace();
    assert!(!cleared.chat_drafts.contains_key(&card));
    assert!(!app.durable.chat_drafts.contains_key(&card));
    assert!(Arc::ptr_eq(&app.persistent_workspace(), &cleared));
}

/// Repeatable measurement for #2633: what a save that changes nothing
/// costs once a draft has been written, in a workspace holding as many
/// cards as a long-used desktop one.
#[test]
#[ignore = "measurement: run with --ignored --nocapture under --profile dev-runtime"]
fn measure_unchanged_saves_after_a_draft_at_desktop_inventory_scale() {
    crate::app::projection_oracle::disable();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    write(&mut app, card, "kept draft");
    let durable = Arc::make_mut(&mut app.durable);
    for index in 0..555 {
        let mut closed = crate::app::tests::terminal_card_state();
        closed.existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("closed inventory fixture".into()),
            },
        };
        durable.card_scopes.insert(
            closed.id,
            PathBuf::from(format!("/workspace/scope-{index}")),
        );
        durable.cards.push(closed);
    }
    let mut held = app.persistent_workspace();
    const SAVES: u32 = 200;
    let started = std::time::Instant::now();
    let mut copies = 0;
    for _ in 0..SAVES {
        let saved = app.persistent_workspace();
        copies += usize::from(!Arc::ptr_eq(&saved, &held));
        // The writer holds the latest snapshot until a newer one replaces it.
        held = saved;
    }
    let elapsed = started.elapsed();
    println!(
        "unchanged saves: {SAVES} in {:.3} ms, {:.1} us each, {copies} copied the workspace",
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e6 / f64::from(SAVES),
    );
}

#[test]
fn default_pane_keeps_attached_files() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
    let card = app.workspace.focused().unwrap();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("chat")
    };
    runtime.attachments.push(Attachment {
        path: "/workspace/example.txt".into(),
        reference: None,
    });
    let _ = app.select_worktree("/workspace/elsewhere".into(), false);
    assert!(app.card_state(card).unwrap().existence.is_open());
}

#[test]
fn a_draft_completes_a_skill_and_queues_it_without_starting_early() {
    const INVOCATION: &str = "[$review](/skills/review/SKILL.md)";
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let runtime = chat_runtime(&app, card);
    let target = runtime.target;
    let launch = runtime.session.launch.clone();
    let _ = app.update(Message::ChatUi(Action::DraftSkills(
        target,
        launch,
        vec![chat::Skill {
            name: "review".into(),
            description: "Review the change".into(),
            invocation: INVOCATION.into(),
        }],
    )));
    write(&mut app, card, "/rev");
    let runtime = chat_runtime(&app, card);
    let (_, rows) = runtime
        .completion()
        .expect("skills are offered before startup");
    assert_eq!(rows.len(), 1);
    assert!(runtime.is_draft());
    assert!(runtime.handle().is_none());
    assert!(!app.chat_start_pending(card));
    assert_eq!(rows[0].insertion, "$review");
    assert!(
        app.chat_runtime_mut(target)
            .unwrap()
            .complete("$review", false)
    );
    write(&mut app, card, "this change");
    assert_eq!(
        chat_runtime(&app, card).composer.text().trim(),
        "$review this change"
    );
    submit(&mut app, card);
    assert!(app.chat_start_pending(card));
    assert_eq!(
        chat_runtime(&app, card)
            .queued
            .as_ref()
            .unwrap()
            .prompt
            .source
            .text()
            .trim(),
        "$review this change"
    );
    assert_eq!(
        chat_runtime(&app, card)
            .queued
            .as_ref()
            .unwrap()
            .resolved_text
            .trim(),
        format!("{INVOCATION} this change")
    );
}

#[test]
fn a_provider_command_takes_precedence_over_a_queued_draft_skill() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let runtime = chat_runtime(&app, card);
    let target = runtime.target;
    let launch = runtime.session.launch.clone();
    let _ = app.update(Message::ChatUi(Action::DraftSkills(
        target,
        launch,
        vec![chat::Skill {
            name: "review".into(),
            description: String::new(),
            invocation: "[$review](/skills/review/SKILL.md)".into(),
        }],
    )));
    write(&mut app, card, "/review this change");
    submit(&mut app, card);
    let queued = chat_runtime(&app, card).queued.as_ref().unwrap();
    assert_eq!(
        queued.resolved_text,
        "[$review](/skills/review/SKILL.md) this change"
    );
    let provider = Snapshot {
        commands: vec![chat::SlashCommand {
            name: "review".into(),
            description: String::new(),
        }],
        ..Snapshot::default()
    };
    assert_eq!(queued.submission_text(&provider), "/review this change");
    assert!(matches!(
        submission(queued.submission_text(&provider), &provider, &[], PromptMode::Send),
        Ok(Command::Slash { command }) if command == "/review this change"
    ));
}

#[test]
fn draft_skill_results_are_scoped_to_the_current_launch_and_card_generation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let runtime = chat_runtime(&app, card);
    let target = runtime.target;
    let launch = runtime.session.launch.clone();
    let skill = chat::Skill {
        name: "review".into(),
        description: String::new(),
        invocation: "$review".into(),
    };
    let mut other_account = launch.clone();
    other_account
        .recipe
        .environment
        .insert("CODEX_HOME".into(), "/another/account".into());
    for (recipient, source) in [
        (target, other_account),
        (
            Target {
                generation: target.generation + 1,
                ..target
            },
            launch.clone(),
        ),
    ] {
        let _ = app.update(Message::ChatUi(Action::DraftSkills(
            recipient,
            source,
            vec![skill.clone()],
        )));
        assert!(chat_runtime(&app, card).snapshot.skills.is_empty());
    }
    let _ = app.update(Message::ChatUi(Action::DraftSkills(
        target,
        launch.clone(),
        vec![skill.clone()],
    )));
    assert_eq!(
        chat_runtime(&app, card).snapshot.skills,
        vec![skill.clone()]
    );
    let _ = app.update(Message::ChatUi(Action::DraftModel(
        target,
        Provider::Codex,
        None,
    )));
    assert_eq!(
        chat_runtime(&app, card).snapshot.skills,
        vec![skill.clone()]
    );
    let _ = app.update(Message::ChatUi(Action::DraftModel(
        target,
        Provider::Claude,
        None,
    )));
    assert!(chat_runtime(&app, card).snapshot.skills.is_empty());
    assert_eq!(
        chat_runtime(&app, card).snapshot.capabilities.skill_reach,
        chat::SkillReach::Leading
    );
    let _ = app.update(Message::ChatUi(Action::DraftSkills(
        target,
        launch,
        vec![skill],
    )));
    assert!(chat_runtime(&app, card).snapshot.skills.is_empty());
    assert!(!app.chat_start_pending(card));
}

#[test]
fn a_running_chat_ignores_late_draft_skill_discovery() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let runtime = chat_runtime(&app, card);
    let target = runtime.target;
    let launch = runtime.session.launch.clone();
    let skills = runtime.snapshot.skills.clone();
    let _ = app.update(Message::ChatUi(Action::DraftSkills(
        target,
        launch,
        vec![chat::Skill {
            name: "late".into(),
            description: String::new(),
            invocation: "$late".into(),
        }],
    )));
    assert_eq!(chat_runtime(&app, card).snapshot.skills, skills);
}

fn local_return(app: &Cagents) -> (Target, &LocalReturn) {
    app.chat_ui
        .phases
        .values()
        .find_map(|phase| match phase {
            Phase::Returning {
                target,
                local: Some(plan),
                ..
            } => Some((*target, plan)),
            _ => None,
        })
        .expect("a local return owns the card")
}

/// Types `text` into the composer of `card`.
pub(super) fn write(app: &mut Cagents, card: CardId, text: &str) {
    let target = chat_runtime(app, card).target;
    let _ = app.update(Message::ChatUi(Action::Composer(
        target,
        text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(text.to_owned()))),
    )));
}

/// Lay out the real chat and dispatch a key through its focused composer.
/// This uses native text shaping and widget bindings, without host input.
fn composer_history_key(
    app: &mut Cagents,
    card: CardId,
    width: f32,
    key: keyboard::key::Named,
    renderer: &cosmic::Renderer,
) {
    let messages = composer_key_messages(
        app,
        card,
        width,
        permission_key(key, keyboard::Modifiers::empty()),
        renderer,
    );
    assert!(
        !messages.is_empty(),
        "the focused native composer must handle the arrow"
    );
    for message in messages {
        drop(app.update(message));
    }
}

/// What the focused native composer of the real chat, laid out `width`
/// wide, publishes for `event`, before any of it is handled.
pub(super) fn composer_key_messages(
    app: &Cagents,
    card: CardId,
    width: f32,
    event: keyboard::Event,
    renderer: &cosmic::Renderer,
) -> Vec<Message> {
    use iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};
    let viewport = iced::Rectangle::with_size(iced::Size::new(width, 720.0));
    let mut messages = Vec::new();
    let mut element = app.chat_view(card);
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(iced::Size::ZERO, viewport.size()),
    );
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        renderer,
        &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
            card,
            "chat-composer",
        )),
    );
    element.as_widget_mut().update(
        &mut tree,
        &iced::Event::Keyboard(event),
        Layout::new(&node),
        iced::mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    messages
}

#[test]
fn native_composer_up_crosses_wrapped_rows_before_history_and_restores_the_draft() {
    use keyboard::key::Named;
    let renderer = crate::app::tests::headless_renderer();
    for width in [240.0, 420.0, 760.0] {
        for paragraph in [
            "A paragraph whose soft wrapped rows belong to the unsent draft. ".repeat(12),
            "Unicode café 🦀 日本語 Ελληνικά keeps its native shaped rows. ".repeat(12),
            format!(
                "explicit first line\n{}",
                "a long second line wraps too ".repeat(20)
            ),
        ] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, Some("wrapped-history"));
            write(&mut app, card, &paragraph);
            let target = chat_runtime(&app, card).target;
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.history = vec!["previous sent prompt".into()];
            runtime
                .composer
                .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));

            let mut boundary = None;
            for moved in 0..256 {
                let before = chat_runtime(&app, card).composer.cursor();
                composer_history_key(&mut app, card, width, Named::ArrowUp, &renderer);
                let runtime = chat_runtime(&app, card);
                if runtime.history_cursor.is_some() {
                    assert!(
                        moved > 1,
                        "Up must traverse multiple native rows at {width}"
                    );
                    boundary = Some(before);
                    break;
                }
                assert_eq!(runtime.composer.text(), paragraph);
                assert_ne!(
                    runtime.composer.cursor(),
                    before,
                    "Up must move within the draft"
                );
            }
            let boundary = boundary.expect("Up eventually reaches native history");
            assert_eq!(
                chat_runtime(&app, card).composer.text(),
                "previous sent prompt"
            );
            composer_history_key(&mut app, card, width, Named::ArrowDown, &renderer);
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.composer.text(), paragraph);
            assert_eq!(runtime.composer.cursor(), boundary);
            assert_eq!(runtime.history_cursor, None);
        }
    }
}

#[test]
fn native_composer_up_collapses_a_selection_before_starting_history() {
    use keyboard::key::Named;
    let renderer = crate::app::tests::headless_renderer();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("selected-history"));
    write(&mut app, card, "selected draft");
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.history = vec!["sent prompt".into()];
    runtime.composer.perform(text_editor::Action::Move(
        text_editor::Motion::DocumentStart,
    ));
    runtime
        .composer
        .perform(text_editor::Action::Select(text_editor::Motion::Right));
    composer_history_key(&mut app, card, 420.0, Named::ArrowUp, &renderer);
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text(), "selected draft");
    assert_eq!(runtime.composer.cursor().selection, None);
    assert_eq!(runtime.history_cursor, None);
    composer_history_key(&mut app, card, 420.0, Named::ArrowUp, &renderer);
    assert_eq!(chat_runtime(&app, card).composer.text(), "sent prompt");
}

#[test]
fn native_composer_up_keeps_whitespace_drafts_out_of_queue_take_back() {
    let renderer = crate::app::tests::headless_renderer();
    for text in [" ".repeat(1200), "\n\n\n".into()] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("whitespace-queue"));
        write(&mut app, card, &text);
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.followups.push(Followup {
            id: 1,
            text: "waiting follow-up".into(),
            preview: "waiting follow-up".into(),
            attachments: Vec::new(),
        });
        runtime
            .composer
            .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        let before = runtime.composer.cursor();
        composer_history_key(
            &mut app,
            card,
            240.0,
            keyboard::key::Named::ArrowUp,
            &renderer,
        );
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text(), text);
        assert_eq!(runtime.followups.len(), 1);
        if text.contains('\n') {
            assert_ne!(runtime.composer.cursor(), before);
        }
    }
}

#[test]
fn native_composer_up_uses_native_layout_at_different_font_sizes() {
    use iced::advanced::{layout, widget::Tree};
    let renderer = crate::app::tests::headless_renderer();
    // The app currently fixes the composer to TEXT_PROSE; there is no
    // chat-font setting to vary. Exercise the native field's supported
    // size API against the same Content and controller, in addition to
    // the full chat-view key-binding regressions above.
    for font_size in [13.0, 23.0] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("font-size-history"));
        let draft =
            "Unicode café 🦀 日本語 remains a draft while moving through native rows. ".repeat(12);
        write(&mut app, card, &draft);
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.history = vec!["previous prompt".into()];
        runtime
            .composer
            .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        {
            let mut field: iced::Element<'_, Message, cosmic::Theme, cosmic::Renderer> =
                crate::ui_controls::field::editor("Message", &runtime.composer)
                    .size(font_size)
                    .on_action(move |event| Message::ChatUi(Action::Composer(target, event)))
                    .into();
            let mut tree = Tree::new(field.as_widget());
            let _ = field.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(320.0, 200.0)),
            );
        }
        let before = chat_runtime(&app, card).composer.cursor();
        drop(app.update(Message::ChatUi(Action::ComposerUp(target))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text(), draft);
        assert_ne!(runtime.composer.cursor(), before);
        assert_eq!(runtime.history_cursor, None);

        app.chat_runtime_mut(target)
            .unwrap()
            .composer
            .perform(text_editor::Action::Move(
                text_editor::Motion::DocumentStart,
            ));
        drop(app.update(Message::ChatUi(Action::ComposerUp(target))));
        assert_eq!(chat_runtime(&app, card).composer.text(), "previous prompt");
    }
}

#[test]
fn multiline_prompt_history_restores_the_exact_unsent_draft_and_attachments() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("history"));
    let original = "unsent message\nsecond line\n\n";
    write(&mut app, card, original);
    let target = chat_runtime(&app, card).target;
    let (cursor, selection) = {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.history = vec!["older\nmultiline".into(), "latest\nmultiline".into()];
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        Arc::make_mut(&mut runtime.snapshot).capabilities.steer = true;
        runtime.attach([PathBuf::from("/workspace/attachment.txt")]);
        runtime.composer.perform(text_editor::Action::Move(
            text_editor::Motion::DocumentStart,
        ));
        runtime
            .composer
            .perform(text_editor::Action::Select(text_editor::Motion::Right));
        assert_eq!(runtime.history_navigation(), (true, false));
        (runtime.composer.cursor(), runtime.composer.selection())
    };
    for (action, expected) in [
        (Action::HistoryOlder(target), "latest\nmultiline"),
        (Action::HistoryOlder(target), "older\nmultiline"),
        (Action::HistoryOlder(target), "older\nmultiline"),
        (Action::HistoryNewer(target), "latest\nmultiline"),
    ] {
        drop(app.update(Message::ChatUi(action)));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text(), expected);
        assert_eq!(runtime.history_navigation(), (true, true));
        assert_eq!(
            runtime.attachments[0].path,
            PathBuf::from("/workspace/attachment.txt")
        );
    }
    assert_eq!(app.persistent_workspace().chat_drafts[&card], original);

    drop(app.update(Message::ChatUi(Action::HistoryNewer(target))));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text(), original);
    assert_eq!(runtime.composer.cursor(), cursor);
    assert_eq!(runtime.composer.selection(), selection);
    assert_eq!(runtime.history_cursor, None);
    assert!(runtime.history_stash.is_none());
    assert!(runtime.snapshot.busy && runtime.snapshot.capabilities.steer);
    drop(app.update(Message::ChatUi(Action::HistoryNewer(target))));
    assert_eq!(chat_runtime(&app, card).composer.text(), original);
}

#[test]
fn appended_provider_draft_leaves_history_and_persists_the_visible_text() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("history"));
    write(&mut app, card, "original unsent draft");
    let target = chat_runtime(&app, card).target;
    {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.history = vec!["sent prompt".into()];
        Arc::make_mut(&mut runtime.snapshot).composer = Some("provider suggestion".into());
    }
    drop(app.update(Message::ChatUi(Action::HistoryOlder(target))));
    drop(app.update(Message::ChatUi(Action::AppendProviderDraft(target))));

    let expected = "sent prompt\nprovider suggestion";
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text(), expected);
    assert_eq!(runtime.unsent_text(), expected);
    assert_eq!(runtime.history_cursor, None);
    assert!(runtime.history_stash.is_none());
    assert_eq!(app.persistent_workspace().chat_drafts[&card], expected);

    drop(app.update(Message::ChatUi(Action::HistoryNewer(target))));
    assert_eq!(chat_runtime(&app, card).composer.text(), expected);
    app.chat_forget(card);
    assert_eq!(app.chat_ui.drafts[&card], expected);
}

#[test]
fn dropped_mentions_restart_completion_and_promote_the_recalled_prompt() {
    let _admission = attachment_draft::test_admission();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    write(&mut app, card, "original unsent draft");
    let target = chat_runtime(&app, card).target;
    {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.history = vec!["sent prompt".into()];
    }
    drop(app.update(Message::ChatUi(Action::HistoryOlder(target))));
    {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.composer.perform(text_editor::Action::Select(
            text_editor::Motion::DocumentEnd,
        ));
        runtime.completion.dismissed = Some((0, 0));
        assert!(runtime.files.is_none());
    }
    drop(app.update(Message::ChatUi(Action::Dropped(
        target,
        file_drop::Payload::new("text/plain".into(), b"@src".to_vec()),
    ))));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text(), "@src");
    assert_eq!(runtime.completion.dismissed, None);
    assert!(runtime.files.is_some());
    assert_eq!(runtime.history_cursor, None);
    assert!(runtime.history_stash.is_none());
    assert_eq!(app.persistent_workspace().chat_drafts[&card], "@src");
}

#[test]
fn prompt_history_keeps_multiline_draft_navigation_and_saved_text() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    write(&mut app, card, "unsent\nlast line");
    let target = chat_runtime(&app, card).target;
    {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.history = vec!["sent prompt".into()];
        runtime
            .composer
            .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        assert_eq!(
            runtime.history_navigation(),
            (false, false),
            "a multiline draft keeps caret navigation"
        );
        runtime.composer.perform(text_editor::Action::Move(
            text_editor::Motion::DocumentStart,
        ));
    }
    drop(app.update(Message::ChatUi(Action::HistoryOlder(target))));
    app.chat_forget(card);
    assert_eq!(app.chat_ui.drafts[&card], "unsent\nlast line");
}

/// Sends what the composer of `card` holds.
pub(super) fn submit(app: &mut Cagents, card: CardId) {
    let target = chat_runtime(app, card).target;
    submit_target(app, target);
}

fn settle_first_start_checkpoint(app: &mut Cagents, card: CardId, task: Task<Message>) {
    if matches!(
        app.chat_ui.phases.get(&card),
        Some(Phase::SavingDraft { .. })
    ) {
        let (target, attempt, result) = super::first_start_checkpoint_tests::checkpoint(task);
        assert!(
            result.is_ok(),
            "real first-start fixture checkpoint: {result:?}"
        );
        drop(app.draft_start_checkpointed(target, attempt, result));
    }
}

fn submit_target(app: &mut Cagents, target: Target) {
    start_with(app, target, Action::Submit(target, PromptMode::Send));
}

/// Takes `action`, which may start `target`'s draft for its first message.
fn start_with(app: &mut Cagents, target: Target, action: Action) {
    // Existing start/Ready tests exercise the provider boundary, not a
    // fabricated save receipt. Supply and drive the real writer barrier
    // first, while never polling the returned provider-start Task.
    let directory = app.session.is_none().then(|| tempfile::tempdir().unwrap());
    if let Some(directory) = &directory {
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("first-start-test.sqlite3"),
            crate::model::BootId::new("chat-submit-test"),
        )
        .unwrap();
        app.session = Some(writer);
    }
    let task = app.update(Message::ChatUi(action));
    settle_first_start_checkpoint(app, target.card, task);
    if directory.is_some() {
        drop(app.session.take());
    }
}

#[test]
fn late_image_insert_preserves_a_later_native_word_selection() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        write(&mut app, card, "start middle end");
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.composer.move_to(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 0 },
            selection: None,
        });
        let ticket = runtime.begin_attachment_insert();
        runtime.composer.move_to(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 8 },
            selection: None,
        });
        runtime.composer.perform(text_editor::Action::SelectWord);
        assert_eq!(runtime.composer.selection().as_deref(), Some("middle"));
        assert!(runtime.finish_attachment_insert(ticket.id, "", vec![image.clone()]));
        assert_eq!(
            runtime.composer.selection().as_deref(),
            Some("middle"),
            "late image callback preserves the user's exact expanded selection"
        );
        assert_eq!(
            runtime.composer.text(),
            format!(
                "{} start middle end",
                image.reference.as_ref().unwrap().token()
            )
        );
        assert!(runtime.remove_image_token(&image.reference.as_ref().unwrap().token()));
        assert_eq!(runtime.composer.selection().as_deref(), Some("middle"));
    });
}

#[test]
fn ambiguous_native_word_selection_refuses_image_removal_without_losing_metadata_or_text() {
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        let token = image.reference.as_ref().unwrap().token();
        let text = format!("{token} 中中中");
        write(&mut app, card, &text);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.merge_attachments([image.clone()]);
        runtime.composer.move_to(text_editor::Cursor {
            position: text_editor::Position {
                line: 0,
                column: token.len() + 1 + "中".len(),
            },
            selection: None,
        });
        runtime.composer.perform(text_editor::Action::SelectWord);
        let selected = runtime.composer.selection().unwrap();
        assert_eq!(
            selected, "中中",
            "native boundary-expanded selection is the ambiguity fixture"
        );
        assert!(!attachment_draft::Bookmark::capture(&runtime.composer).valid);
        drop(app.handle_chat_ui(Action::RemoveAttachment(target, 0)));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.attachments, [image.clone()]);
        assert_eq!(runtime.composer.text(), text);
        assert_eq!(
            runtime.composer.selection().as_deref(),
            Some(selected.as_str())
        );
        assert!(
            runtime
                .error
                .as_deref()
                .unwrap()
                .contains("Clear it before removing")
        );
    });
}

#[test]
fn same_caret_image_pastes_follow_initiation_order_in_both_completion_orders() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second sequence image.png");
        for second_finishes_first in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = draft_chat(&mut app);
            let target = chat_runtime(&app, card).target;
            write(&mut app, card, "tail");
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.composer.move_to(text_editor::Cursor {
                position: text_editor::Position { line: 0, column: 0 },
                selection: None,
            });
            let a = runtime.begin_attachment_insert();
            let b = runtime.begin_attachment_insert();
            if second_finishes_first {
                assert!(runtime.finish_attachment_insert(b.id, "", vec![second.clone()]));
                assert!(runtime.finish_attachment_insert(a.id, "", vec![first.clone()]));
            } else {
                assert!(runtime.finish_attachment_insert(a.id, "", vec![first.clone()]));
                assert!(runtime.finish_attachment_insert(b.id, "", vec![second.clone()]));
            }
            assert_eq!(
                runtime.composer.text(),
                format!(
                    "{} {} tail",
                    first.reference.as_ref().unwrap().token(),
                    second.reference.as_ref().unwrap().token()
                ),
                "completion order must not reverse paste initiation order"
            );
            assert_eq!(runtime.pastes_pending, 0);
            assert!(runtime.attachment_order.is_empty());
        }
    });
}

#[test]
fn completing_an_image_selection_cancels_an_overlapping_jobs_lease_before_its_io() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("overlapping second.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        write(&mut app, card, "alpha beta gamma");
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.composer.move_to(text_editor::Cursor {
            position: text_editor::Position {
                line: 0,
                column: 10,
            },
            selection: Some(text_editor::Position { line: 0, column: 6 }),
        });
        let a = runtime.begin_attachment_insert();
        let b = runtime.begin_attachment_insert();
        assert!(b.active());
        assert!(runtime.finish_attachment_insert(a.id, "", vec![first.clone()]));
        assert!(
            !b.active(),
            "overlap must cancel the actual worker lease, not only its eventual callback"
        );
        assert!(
            clipboard_image::stage_file_active(
                PathBuf::from("/must-not-read-after-overlapping-paste.png"),
                u64::MAX,
                &b
            )
            .unwrap_err()
            .contains("cancelled")
        );
        assert!(!runtime.finish_attachment_insert(b.id, "", vec![second]));
        assert_eq!(runtime.attachments, [first]);
        assert_eq!(runtime.pastes_pending, 0);
        assert!(runtime.attachment_order.is_empty());
    });
}

#[test]
fn managed_images_keep_caret_identity_through_out_of_order_pastes_remove_and_draft_restore() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second.png");
        let first_ref = first.reference.as_ref().unwrap();
        let second_ref = second.reference.as_ref().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        write(&mut app, card, "alpha omega");
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.composer.move_to(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 6 },
            selection: None,
        });
        let first_id = runtime.begin_attachment_insert();
        runtime.composer_action(text_editor::Action::Edit(text_editor::Edit::Insert('λ')));
        runtime.composer_action(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
        let second_id = runtime.begin_attachment_insert();
        assert!(runtime.finish_attachment_insert(second_id.id, "", vec![second.clone()]));
        assert!(runtime.finish_attachment_insert(first_id.id, "", vec![first.clone()]));
        let expected = format!("alpha {} λomega {} ", first_ref.token(), second_ref.token());
        assert_eq!(runtime.composer.text(), expected);
        assert_eq!(runtime.pastes_pending, 0);
        assert_eq!(runtime.attachments, [second.clone(), first.clone()]);
        let session = runtime.session.clone();
        let saved = app.persistent_workspace();
        assert_eq!(
            saved.chat_draft_attachments[&card],
            [second.clone(), first.clone()]
        );
        app.chat_forget(card);
        assert_eq!(
            app.chat_ui.draft_attachments[&card],
            [second.clone(), first.clone()]
        );
        app.chat_ui = State::with_draft_attachments(
            saved.chat_drafts.clone(),
            saved.chat_draft_attachments.clone(),
        );
        drop(app.open_chat_draft(card, session));
        let target = chat_runtime(&app, card).target;
        assert_eq!(chat_runtime(&app, card).composer.text(), expected);
        assert_eq!(
            chat_runtime(&app, card).attachments,
            [second.clone(), first.clone()]
        );
        drop(app.handle_chat_ui(Action::RemoveAttachment(target, 1)));
        let runtime = chat_runtime(&app, card);
        assert!(!runtime.composer.text().contains(&first_ref.token()));
        assert!(runtime.composer.text().contains(&second_ref.token()));
        assert_eq!(runtime.attachments, [second.clone()]);
        assert_eq!(
            runtime.attachments[0].reference.as_ref().unwrap().number,
            second_ref.number
        );
        chat::attachment_assets::validate(&first).unwrap();
        chat::attachment_assets::validate(&second).unwrap();
    });
}

#[test]
fn image_history_recall_takeback_and_retry_preserve_full_receipts_and_unsent_stash() {
    chat::attachment_assets::with_test_image(|sent| {
        let draft = chat::attachment_assets::test_image("draft.png");
        let sent_text = format!("Describe {}", sent.reference.as_ref().unwrap().token());
        let draft_text = format!("Unsent {}", draft.reference.as_ref().unwrap().token());
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.remember_with_attachments(sent_text.clone(), vec![sent.clone()]);
        runtime.replace_draft(text_editor::Content::with_text(&draft_text));
        runtime.replace_attachments(vec![draft.clone()]);
        runtime.recall(true);
        assert_eq!(runtime.composer.text(), sent_text);
        assert_eq!(runtime.attachments, [sent.clone()]);
        assert_eq!(runtime.unsent_text(), draft_text);
        assert_eq!(runtime.unsent_attachments(), [draft.clone()]);
        runtime.recall(false);
        assert_eq!(runtime.composer.text(), draft_text);
        assert_eq!(runtime.attachments, [draft.clone()]);
        runtime.take_back(&sent_text, vec![sent.clone()], PromptMode::Queue);
        assert_eq!(runtime.attachments, [sent.clone(), draft.clone()]);
        let Command::Prompt { attachments, .. } = submission(
            &runtime.composer.text(),
            &runtime.snapshot,
            &runtime.attachments,
            PromptMode::Queue,
        )
        .unwrap() else {
            panic!("prompt expected");
        };
        assert_eq!(attachments, [sent.clone(), draft.clone()]);
        assert_eq!(attachments[0].reference, sent.reference);
        assert_eq!(attachments[1].reference, draft.reference);
    });
}

#[test]
fn identical_history_prose_does_not_borrow_an_earlier_turns_image_authority() {
    chat::attachment_assets::with_test_image(|image| {
        let text = format!("Compare {}", image.reference.as_ref().unwrap().token());
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.remember_with_attachments(text.clone(), vec![image.clone()]);
        runtime.remember_with_attachments(text.clone(), Vec::new());
        assert_eq!(
            runtime.history.len(),
            2,
            "same words can represent different image inputs"
        );
        runtime.recall(true);
        assert_eq!(runtime.composer.text(), text);
        assert!(
            runtime.attachments.is_empty(),
            "the latest prose-only message gains no image from prior history"
        );
        runtime.recall(true);
        assert_eq!(runtime.attachments, [image]);
    });
}

#[test]
fn refreshed_history_keeps_identical_prose_turns_separate_and_replays_all_image_only_inputs() {
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second history.png");
        let first_ref = first.reference.as_ref().unwrap();
        let second_ref = second.reference.as_ref().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("image-history"));
        let target = chat_runtime(&app, card).target;
        app.chat_runtime_mut(target)
            .unwrap()
            .session
            .launch
            .provider = Provider::Codex;
        let text = format!("Compare {}", first_ref.token());
        let item = |id: &str, role, content| {
            Arc::new(chat::Item {
                id: id.into(),
                role,
                content,
                started_at: None,
                at: None,
            })
        };
        let image = |id: &str, attachment: &Attachment| {
            item(
                id,
                chat::Role::User,
                ItemContent::Image {
                    path: attachment.path.to_str().unwrap().into(),
                    description: attachment.label(),
                    reference: attachment.reference.clone(),
                },
            )
        };
        let snapshot = Snapshot {
            generation: 91,
            items: vec![
                item(
                    "old/input/0",
                    chat::Role::User,
                    ItemContent::Text {
                        text: text.clone(),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                ),
                image("old/input/2", &first),
                item(
                    "answer",
                    chat::Role::Assistant,
                    ItemContent::Text {
                        text: "answer".into(),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                ),
                item(
                    "new/input/0",
                    chat::Role::User,
                    ItemContent::Text {
                        text: text.clone(),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                ),
            ],
            ..Default::default()
        };
        show_chat_snapshot_for_test(&mut app, card, snapshot);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.recall(true);
        assert_eq!(runtime.composer.text(), text);
        assert!(
            runtime.attachments.is_empty(),
            "later plain prose cannot inherit earlier real image input"
        );
        runtime.recall(true);
        assert_eq!(runtime.attachments, [first.clone()]);
        runtime.recall(false);
        runtime.recall(false);
        let snapshot = Snapshot {
            generation: 92,
            items: vec![
                image("only/input/0", &first),
                image("only/input/1", &second),
            ],
            ..Default::default()
        };
        show_chat_snapshot_for_test(&mut app, card, snapshot);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.recall(true);
        assert_eq!(
            runtime.composer.text(),
            format!("{} {}", first_ref.token(), second_ref.token())
        );
        assert_eq!(runtime.attachments, [first.clone(), second.clone()]);
        runtime.recall(false);
        let snapshot = Snapshot {
            generation: 93,
            items: vec![image("one/input/0", &first), image("two/input/0", &second)],
            ..Default::default()
        };
        show_chat_snapshot_for_test(&mut app, card, snapshot);
        let runtime = app.chat_runtime_mut(target).unwrap();
        let draft = chat::attachment_assets::test_image("unsent while hidden.png");
        runtime.replace_draft(text_editor::Content::with_text("Unsent image draft"));
        runtime.replace_attachments(vec![draft.clone()]);
        runtime.recall(true);
        assert_eq!(runtime.composer.text(), second_ref.token());
        assert_eq!(runtime.attachments, [second.clone()]);
        runtime.recall(true);
        assert_eq!(runtime.composer.text(), first_ref.token());
        assert_eq!(runtime.attachments, [first.clone()]);

        let handle = runtime.handle().unwrap().clone();
        let publication = runtime.snapshot.as_ref().clone();
        let history = runtime.history.clone();
        let history_images = runtime.history_images.clone();
        runtime.set_visible(false).unwrap();
        runtime.cool();
        assert_eq!(runtime.history_cursor, Some(0));
        assert_eq!(runtime.history, history);
        assert_eq!(runtime.history_images, history_images);
        assert_eq!(runtime.attachments, [first.clone()]);
        assert_eq!(runtime.unsent_attachments(), [draft.clone()]);
        assert_eq!(runtime.unsent_text(), "Unsent image draft");
        runtime.set_visible(true).unwrap();
        handle.replace_snapshot_for_test(publication.clone());
        runtime.refresh();
        runtime.recall(false);
        runtime.recall(false);
        assert!(runtime.history_cursor.is_none());
        assert_eq!(runtime.composer.text(), "Unsent image draft");
        assert_eq!(runtime.attachments, [draft.clone()]);
        assert!(runtime.history_attachment_stash.is_none());

        runtime.set_visible(false).unwrap();
        runtime.cool();
        assert!(runtime.history.is_empty());
        assert!(runtime.history_images.is_empty());
        assert_eq!(runtime.composer.text(), "Unsent image draft");
        assert_eq!(runtime.attachments, [draft.clone()]);
        runtime.set_visible(true).unwrap();
        handle.replace_snapshot_for_test(publication);
        runtime.refresh();
        assert_eq!(runtime.history, history);
        assert_eq!(runtime.history_images, history_images);
        assert_eq!(runtime.attachments, [draft]);
    });
}

#[test]
fn retargeted_runtime_retires_old_insert_leases_before_ignoring_old_callbacks() {
    let _admission = attachment_draft::test_admission();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let old_target = chat_runtime(&app, card).target;
    write(&mut app, card, "retained prose");
    let runtime = app.chat_runtime_mut(old_target).unwrap();
    let first = runtime.begin_attachment_insert();
    let second = runtime.begin_attachment_insert();
    runtime.attachment_chooser_pending = Some(first.id);
    runtime.submit_after_paste = Some(PromptMode::Send);
    let target = Target {
        generation: old_target.generation.wrapping_add(1),
        ..old_target
    };
    runtime.retarget(target);
    assert!(!first.active());
    assert!(!second.active());
    assert_eq!(runtime.pastes_pending, 0);
    assert!(runtime.attachment_jobs.is_empty());
    assert!(runtime.attachment_bookmarks.is_empty());
    assert!(runtime.attachment_chooser_pending.is_none());
    assert!(runtime.submit_after_paste.is_none());
    assert!(
        runtime.try_attachment_insert().is_none(),
        "old callbacks still own their global slots until completion"
    );
    drop(app.handle_chat_ui(Action::PastedImage(old_target, first, None)));
    let next = app
        .chat_runtime_mut(target)
        .unwrap()
        .try_attachment_insert()
        .unwrap();
    drop(app.handle_chat_ui(Action::PastedImage(old_target, second, None)));
    let runtime = app.chat_runtime_mut(target).unwrap();
    let another = runtime.try_attachment_insert().unwrap();
    assert!(next.active());
    assert!(another.active());
    assert_eq!(runtime.pastes_pending, 2);
    assert_eq!(runtime.composer.text(), "retained prose");
    assert!(runtime.attachments.is_empty());
}

#[test]
fn replacing_drafts_cannot_admit_more_owned_clipboard_callbacks() {
    let _admission = attachment_draft::test_admission();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    let first = runtime.begin_attachment_insert();
    let second = runtime.begin_attachment_insert();
    for _ in 0..20 {
        runtime.replace_draft(text_editor::Content::with_text("replacement"));
        assert_eq!(runtime.pastes_pending, 0);
        assert!(
            runtime.try_attachment_insert().is_none(),
            "cancelled pending callbacks retain their global admission"
        );
    }
    assert!(!first.active());
    assert!(!second.active());
    drop(first);
    let next = runtime.try_attachment_insert().unwrap();
    assert!(runtime.try_attachment_insert().is_none());
    drop(second);
    let another = runtime.try_attachment_insert().unwrap();
    assert!(next.active());
    assert!(another.active());
}

#[test]
fn cancelled_empty_image_callback_does_not_read_a_later_clipboard() {
    let _admission = attachment_draft::test_admission();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    let ticket = runtime.begin_attachment_insert();
    runtime.replace_draft(text_editor::Content::with_text("replacement draft"));
    let task = app.handle_chat_ui(Action::PastedImage(target, ticket, None));
    assert_eq!(
        task.units(),
        0,
        "stale callback must not schedule another clipboard read"
    );
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text(), "replacement draft");
    assert_eq!(runtime.pastes_pending, 0);
}

#[test]
fn valid_empty_paste_preserves_later_native_selection_and_caret() {
    let _admission = attachment_draft::test_admission();
    let thumbnail_key = |runtime: &Runtime| {
        let mut subscriptions = runtime.thumbnails.subscriptions(runtime.target);
        assert_eq!(subscriptions.len(), 1);
        let mut recipes = iced::advanced::subscription::into_recipes(subscriptions.pop().unwrap());
        assert_eq!(recipes.len(), 1);
        let mut hasher = iced::advanced::subscription::Hasher::default();
        recipes.pop().unwrap().hash(&mut hasher);
        std::hash::Hasher::finish(&hasher)
    };
    for empty in [None, Some(String::new())] {
        for selection in 0..3 {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = draft_chat(&mut app);
            let target = chat_runtime(&app, card).target;
            write(&mut app, card, "start middle end\nsecond line");
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.attach([PathBuf::from("/fixture/existing.png")]);
            runtime.composer.move_to(text_editor::Cursor {
                position: text_editor::Position { line: 0, column: 0 },
                selection: None,
            });
            let ticket = runtime.begin_attachment_insert();
            drop(app.handle_chat_ui(Action::ComposerCursor(
                target,
                text_editor::Cursor {
                    position: text_editor::Position { line: 0, column: 8 },
                    selection: None,
                },
            )));
            let action = match selection {
                0 => text_editor::Action::Move(text_editor::Motion::DocumentEnd),
                1 => text_editor::Action::SelectWord,
                _ => text_editor::Action::SelectLine,
            };
            drop(app.handle_chat_ui(Action::Composer(target, action)));
            let runtime = app.chat_runtime_mut(target).unwrap();
            match selection {
                0 => assert!(runtime.composer.selection().is_none()),
                1 => assert_eq!(runtime.composer.selection().as_deref(), Some("middle")),
                _ => assert!(
                    runtime
                        .composer
                        .selection()
                        .unwrap()
                        .contains("start middle end")
                ),
            }
            runtime.completion.dismissed = Some((0, 0));
            let before_text = runtime.composer.text();
            let before_cursor = runtime.composer.cursor();
            let before_selection = runtime.composer.selection();
            let before_attachments = runtime.attachments.clone();
            let before_revision = runtime.revision;
            let before_thumbnail = thumbnail_key(runtime);
            assert!(runtime.attachment_bookmarks[&ticket.id].valid);

            drop(app.handle_chat_ui(Action::PastedText(target, ticket, empty.clone())));
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.composer.text(), before_text);
            assert_eq!(runtime.composer.cursor(), before_cursor);
            assert_eq!(runtime.composer.selection(), before_selection);
            assert_eq!(runtime.attachments, before_attachments);
            assert_eq!(thumbnail_key(runtime), before_thumbnail);
            assert_eq!(runtime.revision, before_revision);
            assert_eq!(runtime.completion.dismissed, Some((0, 0)));
            assert_eq!(runtime.pastes_pending, 0);
            assert!(runtime.attachment_jobs.is_empty());
            assert!(runtime.attachment_bookmarks.is_empty());
            assert!(runtime.attachment_order.is_empty());
            assert!(runtime.submit_after_paste.is_none());
            assert!(runtime.commands.is_idle());
            assert!(runtime.error.is_none());
        }
    }
}

#[test]
fn valid_empty_paste_keeps_recalled_history_and_down_restores_the_image_draft() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|sent| {
        let draft = chat::attachment_assets::test_image("unsent-stash.png");
        let sent_text = format!("Sent {}", sent.reference.as_ref().unwrap().token());
        let draft_text = format!(
            "draft words {}\nsecond line",
            draft.reference.as_ref().unwrap().token()
        );
        for empty in [None, Some(String::new())] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = draft_chat(&mut app);
            let target = chat_runtime(&app, card).target;
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.remember_with_attachments(sent_text.clone(), vec![sent.clone()]);
            runtime.replace_draft(text_editor::Content::with_text(&draft_text));
            runtime.replace_attachments(vec![draft.clone()]);
            runtime.composer.move_to(text_editor::Cursor {
                position: text_editor::Position { line: 0, column: 8 },
                selection: None,
            });
            runtime.composer.perform(text_editor::Action::SelectWord);
            assert_eq!(runtime.composer.selection().as_deref(), Some("words"));
            let draft_cursor = runtime.composer.cursor();
            let draft_selection = runtime.composer.selection();
            drop(app.handle_chat_ui(Action::HistoryOlder(target)));
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(runtime.history_cursor, Some(0));
            assert_eq!(runtime.composer.text(), sent_text);
            assert_eq!(runtime.attachments, [sent.clone()]);
            let recalled_cursor = runtime.composer.cursor();
            let recalled_selection = runtime.composer.selection();
            let ticket = runtime.begin_attachment_insert();
            let before_revision = runtime.revision;

            drop(app.handle_chat_ui(Action::PastedText(target, ticket, empty)));
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.history_cursor, Some(0));
            assert_eq!(runtime.history, [sent_text.clone()]);
            assert_eq!(runtime.history_images, [vec![sent.clone()]]);
            assert_eq!(runtime.composer.text(), sent_text);
            assert_eq!(runtime.composer.cursor(), recalled_cursor);
            assert_eq!(runtime.composer.selection(), recalled_selection);
            let stash = runtime
                .history_stash
                .as_ref()
                .expect("unsent editor retained");
            assert_eq!(stash.text(), draft_text);
            assert_eq!(stash.cursor(), draft_cursor);
            assert_eq!(stash.selection(), draft_selection);
            assert_eq!(
                runtime.history_attachment_stash.as_deref(),
                Some([draft.clone()].as_slice())
            );
            assert_eq!(runtime.unsent_text(), draft_text);
            assert_eq!(runtime.unsent_attachments(), [draft.clone()]);
            assert_eq!(runtime.attachments, [sent.clone()]);
            assert_eq!(runtime.revision, before_revision);
            assert_eq!(runtime.pastes_pending, 0);
            assert!(runtime.attachment_jobs.is_empty());
            assert!(runtime.attachment_bookmarks.is_empty());
            assert!(runtime.attachment_order.is_empty());

            // HistoryNewer is the composer's normal Down action while recalling.
            drop(app.handle_chat_ui(Action::HistoryNewer(target)));
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.composer.text(), draft_text);
            assert_eq!(runtime.composer.cursor(), draft_cursor);
            assert_eq!(runtime.composer.selection(), draft_selection);
            assert_eq!(runtime.attachments, [draft.clone()]);
            assert!(runtime.history_cursor.is_none());
            assert!(runtime.history_stash.is_none());
            assert!(runtime.history_attachment_stash.is_none());
            assert!(runtime.commands.is_idle());
        }
    });
}

#[test]
fn empty_paste_callbacks_resume_explicit_submit_once_after_the_last_job() {
    let _admission = attachment_draft::test_admission();
    for text in ["send after both empty callbacks", ""] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("empty-paste-submit"));
        let target = chat_runtime(&app, card).target;
        write(&mut app, card, text);
        let runtime = app.chat_runtime_mut(target).unwrap();
        let first = runtime.begin_attachment_insert();
        let second = runtime.begin_attachment_insert();
        let replay = second.clone();
        submit(&mut app, card);
        let runtime = chat_runtime(&app, card);
        assert!(runtime.commands.is_idle());
        assert_eq!(runtime.submit_after_paste, Some(PromptMode::Send));

        drop(app.handle_chat_ui(Action::PastedText(target, first, None)));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.pastes_pending, 1);
        assert_eq!(runtime.submit_after_paste, Some(PromptMode::Send));
        assert!(runtime.commands.is_idle());
        assert_eq!(runtime.composer.text(), text);

        drop(app.handle_chat_ui(Action::PastedText(target, second, Some(String::new()))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.pastes_pending, 0);
        assert!(runtime.attachment_jobs.is_empty());
        assert!(runtime.attachment_bookmarks.is_empty());
        assert!(runtime.attachment_order.is_empty());
        assert!(runtime.submit_after_paste.is_none());
        if text.is_empty() {
            assert!(
                runtime.commands.is_idle(),
                "empty clipboard cannot invent a prompt"
            );
        } else {
            assert_eq!(
                runtime
                    .commands
                    .in_flight()
                    .unwrap()
                    .sent
                    .as_deref()
                    .map(str::trim_end),
                Some(text)
            );
        }
        let pending_id = runtime.commands.in_flight().map(|pending| pending.id);
        drop(app.handle_chat_ui(Action::PastedText(target, replay, None)));
        let runtime = chat_runtime(&app, card);
        assert_eq!(
            runtime.commands.in_flight().map(|pending| pending.id),
            pending_id
        );
        assert!(
            runtime.followups.is_empty(),
            "replayed completion must not submit twice"
        );
        assert!(runtime.attachments.is_empty());
    }
}

#[test]
fn cancelled_chooser_and_repeated_drops_cannot_queue_unbounded_image_staging() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        Arc::make_mut(&mut runtime.snapshot)
            .capabilities
            .attachments = true;
        let old_id = runtime.begin_attachment_insert();
        runtime.attachment_chooser_pending = Some(old_id.id);
        runtime.replace_draft(text_editor::Content::with_text("replacement"));
        let task = app.handle_chat_ui(Action::Attached(
            target,
            old_id,
            Ok(Some(vec![PathBuf::from("/must-not-read-after-cancel.png")])),
        ));
        assert_eq!(
            task.units(),
            0,
            "stale chooser result must schedule no blocking file read"
        );
        assert_eq!(chat_runtime(&app, card).pastes_pending, 0);
        assert!(
            chat_runtime(&app, card)
                .attachment_chooser_pending
                .is_none(),
            "cancelled chooser completion settles its exact pending ID"
        );
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.begin_attachment_insert();
        runtime.begin_attachment_insert();
        for _ in 0..10 {
            drop(
                app.handle_chat_ui(Action::Dropped(
                    target,
                    file_drop::Payload::new(
                        "text/uri-list".into(),
                        Url::from_file_path(&image.path)
                            .unwrap()
                            .as_str()
                            .as_bytes()
                            .to_vec(),
                    ),
                )),
            );
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.pastes_pending, 2);
            assert_eq!(runtime.attachment_bookmarks.len(), 2);
            assert!(runtime.attachments.is_empty());
        }
        assert_eq!(
            chat::attachment_assets::list().unwrap().images.len(),
            1,
            "drops at capacity must not allocate invisible receipt blobs"
        );
        assert!(
            chat_runtime(&app, card)
                .error
                .as_deref()
                .unwrap()
                .contains("already loading")
        );
    });
}

#[test]
fn a_mixed_drop_replaces_selection_once_and_keeps_its_images_after_its_own_prose() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        write(&mut app, card, "replace me");
        let runtime = app.chat_runtime_mut(target).unwrap();
        Arc::make_mut(&mut runtime.snapshot)
            .capabilities
            .attachments = true;
        runtime.composer.move_to(text_editor::Cursor {
            position: text_editor::Position {
                line: 0,
                column: 10,
            },
            selection: Some(text_editor::Position { line: 0, column: 0 }),
        });
        let payload = format!(
            "{}\nhttps://example.invalid/comparison\n",
            Url::from_file_path(&image.path).unwrap()
        );
        drop(app.handle_chat_ui(Action::Dropped(
            target,
            file_drop::Payload::new("text/uri-list".into(), payload.into_bytes()),
        )));
        let runtime = chat_runtime(&app, card);
        assert!(!runtime.composer.text().contains("replace me"));
        assert!(
            runtime
                .composer
                .text()
                .contains("https://example.invalid/comparison")
        );
        assert_eq!(runtime.pastes_pending, 1);
        let (&id, bookmark) = runtime.attachment_bookmarks.iter().next().unwrap();
        assert!(
            bookmark.valid,
            "a drop's intentional text replacement is not an independent user conflict"
        );
        assert!(bookmark.cursor.selection.is_none());
        let ticket = runtime.attachment_ticket(id).unwrap();
        assert!(
            ticket.active(),
            "same-drop prose must preserve actual staging admission"
        );
        drop(app.handle_chat_ui(Action::AttachmentsPrepared(
            target,
            ticket,
            Ok(vec![image.clone()]),
        )));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.error, None);
        assert_eq!(runtime.attachments, [image.clone()]);
        let text = runtime.composer.text();
        assert!(
            text.find("https://example.invalid/comparison").unwrap()
                < text
                    .find(&image.reference.as_ref().unwrap().token())
                    .unwrap()
        );
        assert!(!text.contains("replace me"));
    });
}

#[test]
fn metadata_only_stashed_image_draft_blocks_alias_overwrite() {
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let alias = CardId::new();
        let canonical = CardId::new();
        app.chat_ui
            .draft_attachments
            .insert(canonical, vec![image.clone()]);
        assert!(app.shared_chat_has_local_work(canonical));
        assert!(!app.rekey_shared_terminal_draft(alias, canonical));
        assert_eq!(app.chat_ui.draft_attachments[&canonical], [image]);
    });
}

#[test]
fn conflicting_pending_image_selection_refuses_without_erasing_new_prose() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        write(&mut app, card, "alpha beta gamma");
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.composer.move_to(text_editor::Cursor {
            position: text_editor::Position {
                line: 0,
                column: 10,
            },
            selection: Some(text_editor::Position { line: 0, column: 6 }),
        });
        let id = runtime.begin_attachment_insert();
        assert_eq!(runtime.composer.selection().as_deref(), Some("beta"));
        // Moving the raw native caret alone retains its old anchor. Model
        // the user's explicit deselection before typing inside that span.
        runtime.composer_action(text_editor::Action::ClearSelection);
        runtime.composer.move_to(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 8 },
            selection: None,
        });
        assert_eq!(runtime.composer.selection(), None);
        runtime.composer_action(text_editor::Action::Edit(text_editor::Edit::Insert('!')));
        assert_eq!(runtime.composer.text(), "alpha be!ta gamma");
        let before_callback = runtime.composer.text();
        assert!(!runtime.finish_attachment_insert(id.id, "", vec![image]));
        assert_eq!(runtime.composer.text(), "alpha be!ta gamma");
        assert_eq!(runtime.composer.text(), before_callback);
        assert!(runtime.attachments.is_empty());
        assert!(
            runtime
                .error
                .as_deref()
                .unwrap()
                .contains("insertion point changed")
        );
    });
}

#[test]
fn a_pending_image_paste_stays_with_the_message_it_precedes() {
    let _admission = attachment_draft::test_admission();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("thread"));
    let target = chat_runtime(&app, card).target;
    write(&mut app, card, "describe this image");
    let id = app
        .chat_runtime_mut(target)
        .unwrap()
        .begin_attachment_insert();
    {
        let runtime = app.chat_runtime_mut(target).unwrap();
        Arc::make_mut(&mut runtime.snapshot)
            .capabilities
            .attachments = true;
        runtime.image_pastes_saving = 1;
    }
    submit(&mut app, card);
    assert!(chat_runtime(&app, card).commands.is_idle());
    assert_eq!(
        chat_runtime(&app, card).submit_after_paste,
        Some(PromptMode::Send)
    );
    assert_eq!(
        chat_runtime(&app, card).composer.text().trim(),
        "describe this image"
    );

    let path = PathBuf::from("/fixture/pasted.png");
    let _ = app.update(Message::ChatUi(Action::ImageSaved(
        target,
        id,
        Ok(Attachment::file(path.clone())),
    )));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.pastes_pending, 0);
    assert_eq!(runtime.image_pastes_saving, 0);
    assert_eq!(runtime.submit_after_paste, None);
    let sending = runtime
        .commands
        .in_flight()
        .expect("send resumes after the image is ready");
    // The image leaves the composer with the message it was pasted into.
    assert!(runtime.attachments.is_empty());
    let outgoing = &sending.outgoing.as_ref().unwrap().prompt;
    assert_eq!(outgoing.text(), "describe this image");
    assert_eq!(
        outgoing.attachments,
        [Attachment {
            path,
            reference: None,
        }]
    );
}

#[test]
fn an_image_the_chat_cannot_take_falls_back_to_clipboard_text() {
    let _admission = attachment_draft::test_admission();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("thread"));
    let target = chat_runtime(&app, card).target;
    let id = app
        .chat_runtime_mut(target)
        .unwrap()
        .begin_attachment_insert();
    {
        let runtime = app.chat_runtime_mut(target).unwrap();
        Arc::make_mut(&mut runtime.snapshot)
            .capabilities
            .attachments = false;
        runtime.submit_after_paste = Some(PromptMode::Send);
    }
    let image =
        clipboard_image::Image::try_from((b"\x89PNG\r\n\x1a\n".to_vec(), "image/png".to_owned()))
            .unwrap();
    let _ = app.update(Message::ChatUi(Action::PastedImage(
        target,
        id.clone(),
        Some(image),
    )));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.error, None, "the clipboard text is read first");
    assert_eq!(runtime.pastes_pending, 1);
    assert_eq!(runtime.submit_after_paste, Some(PromptMode::Send));

    let _ = app.update(Message::ChatUi(Action::PastedText(
        target,
        id,
        Some("pasted cell".into()),
    )));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.error, None);
    assert_eq!(runtime.pastes_pending, 0);
    assert!(runtime.attachments.is_empty());
    assert!(
        runtime.commands.in_flight().is_some(),
        "the send waiting on the paste goes ahead with its text"
    );

    let runtime = app.chat_runtime_mut(target).unwrap();
    let id = runtime.begin_attachment_insert();
    let _ = app.update(Message::ChatUi(Action::PasteRefused(
        target,
        id,
        "The current model cannot accept images.",
    )));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.pastes_pending, 0);
    assert_eq!(
        runtime.error.as_deref(),
        Some("The current model cannot accept images.")
    );
}

/// Rebuilding the strip hides every surface before it lays the new one
/// out. A conversation on screen both before and after is shown again
/// rather than left blank.
#[test]
fn a_chat_on_screen_across_a_strip_rebuild_is_shown_again() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let handle = Arc::clone(chat_runtime(&app, card).handle().unwrap());
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::Resized(1_600.0),
    }));
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    assert!(handle.is_visible_for_test());
    let directory = super::super::worktree_ui::card_directory(app.card_state(card).unwrap())
        .expect("a conversation has a directory")
        .to_path_buf();

    // A strip that no longer matches its scope is rebuilt even when the
    // same worktree is selected again.
    app.workspace = Cagents::rebuild_workspace(&Default::default());
    app.workspace.resize_viewport(1_600.0);
    drop(app.select_worktree(directory, false));
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    assert!(handle.is_visible_for_test(), "the chat is shown again");
}

/// A new conversation whose start, for its first message, is still in
/// flight.
fn starting_chat(app: &mut Cagents) -> (CardId, Target) {
    starting_chat_for_provider(app, Provider::Codex)
}
fn starting_chat_for_provider(app: &mut Cagents, provider: Provider) -> (CardId, Target) {
    starting_chat_in(app, provider, Path::new(FIXTURE_DIRECTORY))
}
pub(super) fn starting_chat_in(
    app: &mut Cagents,
    provider: Provider,
    directory: &Path,
) -> (CardId, Target) {
    let card = draft_chat_in(app, provider, directory);
    write(app, card, "hello");
    submit(app, card);
    let target = app
        .chat_ui
        .attempt(card)
        .expect("the first message starts the conversation")
        .target;
    (card, target)
}

pub(super) fn answer_start(
    app: &mut Cagents,
    target: Target,
    result: Result<Arc<ClientHandle>, chat::StartFailure>,
) {
    let _ = app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(result)))),
    )));
}

/// Whether `task` asks for the keyboard to go to `control`.
///
/// Focus is a widget operation, not a message. Each operation the task
/// issues is shown two controls the way a widget tree would show them:
/// the one asked about, and another that holds the keyboard.
fn task_focuses(task: Task<Message>, control: &widget::Id) -> (bool, bool) {
    task_focuses_from(task, control, &widget::Id::new("the control in use"))
}

/// Whether `task` focuses `control` while `holder` has the keyboard, and
/// whether `holder` still has it after, following chained operations.
fn task_focuses_from(
    task: Task<Message>,
    control: &widget::Id,
    elsewhere: &widget::Id,
) -> (bool, bool) {
    use cosmic::iced::advanced::widget::operation::Focusable;
    use cosmic::iced::futures::StreamExt;

    struct Control(bool);
    impl Focusable for Control {
        fn is_focused(&self) -> bool {
            self.0
        }
        fn focus(&mut self) {
            self.0 = true;
        }
        fn unfocus(&mut self) {
            self.0 = false;
        }
    }

    let mut asked = Control(false);
    let mut holder = Control(true);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) else {
            return;
        };
        // Whatever else the task carries may wait on a daemon that is not
        // there. A focus operation is ready at once, so a short quiet
        // spell means there is none still to come.
        while let Ok(Some(action)) =
            tokio::time::timeout(Duration::from_millis(250), stream.next()).await
        {
            if let cosmic::iced::runtime::Action::Widget(mut operation) = action {
                loop {
                    let bounds = cosmic::iced::Rectangle::default();
                    operation.focusable(Some(control), bounds, &mut asked);
                    operation.focusable(Some(elsewhere), bounds, &mut holder);
                    match operation.finish() {
                        cosmic::iced::advanced::widget::operation::Outcome::Chain(next) => {
                            operation = next;
                        }
                        _ => break,
                    }
                }
            }
        }
    });
    runtime.shutdown_background();
    (asked.0, holder.0)
}

/// A conversation that finishes attaching while another pane is in use
/// used to focus its own composer, which also blurred whatever the user
/// was typing into.
#[test]
fn an_attachment_behind_another_pane_leaves_the_keyboard_where_it_is() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (background, target) = starting_chat(&mut app);
    let composer = card_control_id(background, "chat-composer");
    let in_use = draft_chat(&mut app);
    app.workspace.resize_viewport(1_600.0);
    assert_eq!(app.workspace.focused(), Some(in_use));
    assert!(
        app.workspace
            .visibility(background)
            .is_some_and(VisibilityClass::is_mounted),
        "the conversation is on screen, so only focus keeps the keyboard from it"
    );

    let handle = fixture_handle(&app, background, Some("background"));
    let task = app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    )));
    assert!(matches!(
        app.cards.get(&background),
        Some(CardRuntime::Chat(runtime)) if !runtime.is_draft()
    ));
    assert_eq!(
        task_focuses(task, &composer),
        (false, true),
        "the attachment took the keyboard from the pane in use"
    );
}

/// The same attachment in the pane the user is in does take the keyboard,
/// which is what the guard has to keep working.
#[test]
fn an_attachment_in_the_focused_pane_takes_the_keyboard() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    app.workspace.resize_viewport(1_600.0);
    assert_eq!(app.workspace.focused(), Some(card));

    // While it starts, the pane's title holds the keyboard in the
    // input's place, and hands it over once the input is drawn.
    let handle = fixture_handle(&app, card, Some("focused"));
    let task = app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    )));
    assert_eq!(
        task_focuses_from(
            task,
            &card_control_id(card, "chat-composer"),
            &app.card_header_widget_id(card),
        ),
        (true, false)
    );
}

/// A control the person chose while the conversation was starting, such
/// as sidebar search, keeps the keyboard when it finishes attaching.
#[test]
fn an_attachment_in_the_focused_pane_leaves_a_chosen_control_alone() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    app.workspace.resize_viewport(1_600.0);
    assert_eq!(app.workspace.focused(), Some(card));

    let handle = fixture_handle(&app, card, Some("focused"));
    let task = app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    )));
    assert_eq!(
        task_focuses(task, &card_control_id(card, "chat-composer")),
        (false, true)
    );
}

/// Pressing the find bar's Previous or Next button blurs its field; the
/// keyboard goes back to the field, not to the composer, where the next
/// Enter would send the draft. Coming back to the pane still lands in the
/// composer: an open find bar does not take over the pane's keyboard.
#[test]
fn stepping_through_find_matches_keeps_the_keyboard_in_the_search() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("find-keyboard"));
    let target = chat_runtime(&app, card).target;
    drop(find_navigation::find_for_test(
        &mut app,
        target,
        "needle".into(),
    ));
    let find = card_control_id(card, "chat-find");
    for forward in [true, false] {
        let task = app.handle_chat_ui(Action::FindNext(target, forward));
        assert_eq!(task_focuses(task, &find), (true, false));
    }
    assert_eq!(
        app.card_keyboard_focus(card),
        card_control_id(card, "chat-composer")
    );
}

/// Moving the pointer into another pane leaves the keyboard with a chat
/// menu that is still open, and follows the pointer again once it closes.
#[test]
fn an_open_chat_menu_holds_focus_against_the_hovering_pointer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.settings.focus_follows_mouse = true;
    let first = live_chat(&mut app, Some("menu-first"));
    let second = live_chat(&mut app, Some("menu-second"));
    let update = app.workspace.resize_viewport(3_000.0);
    let _ = app.apply_viewport(update, false, DurableChange::Content);
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(second),
    });
    assert_eq!(app.workspace.focused(), Some(second));
    let target = chat_runtime(&app, second).target;
    app.chat_runtime_mut(target).unwrap().composer_actions = true;
    assert!(app.chat_menu_open());
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(first),
    });
    assert_eq!(app.workspace.focused(), Some(second));
    app.chat_runtime_mut(target).unwrap().composer_actions = false;
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(first),
    });
    assert_eq!(app.workspace.focused(), Some(first));
    // A menu flagged open in a pane without focus is not on screen, and
    // does not hold focus.
    app.chat_runtime_mut(target).unwrap().composer_actions = true;
    let _ = app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::PointerEntered(second),
    });
    assert_eq!(app.workspace.focused(), Some(second));
}

fn account_commit_reply(task: Task<Message>) -> Message {
    use cosmic::iced::futures::StreamExt;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut stream = cosmic::iced::runtime::task::into_stream(task).unwrap();
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::ChatUi(
                            Action::AccountCommitted(..) | Action::UpgradeCommitted(..),
                        ),
                    )) = action
                    {
                        return message;
                    }
                }
                panic!("account commit produced no acknowledgment");
            })
            .await
            .expect("session writer did not acknowledge the exact transition within 10 seconds")
        })
}

pub(super) fn fixture_handle(
    app: &Cagents,
    card: CardId,
    session_id: Option<&str>,
) -> Arc<ClientHandle> {
    let mut session = app.chat_ui.attempt(card).unwrap().session.clone();
    // The daemon starts a new Codex thread on a listener of its own and
    // hands the thread back naming it.
    let proof = crate::model::HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    };
    if session.launch.provider == Provider::Codex {
        session.claim_owned_codex_listener(std::path::Path::new("/daemon-temp-root"), &proof);
    }
    session.proof = Some(proof);
    Arc::new(ClientHandle::fixture(
        session,
        Snapshot {
            session_id: session_id.map(str::to_owned),
            ready: true,
            ..Snapshot::default()
        },
    ))
}

/// A conversation that started, optionally with a provider-confirmed id.
pub(super) fn live_chat(app: &mut Cagents, session_id: Option<&str>) -> CardId {
    live_chat_for_provider(app, Provider::Codex, session_id)
}
pub(in crate::app) fn live_chat_for_provider(
    app: &mut Cagents,
    provider: Provider,
    session_id: Option<&str>,
) -> CardId {
    live_chat_in(app, provider, Path::new(FIXTURE_DIRECTORY), session_id)
}

/// A conversation that started in `directory`.
pub(in crate::app) fn live_chat_in(
    app: &mut Cagents,
    provider: Provider,
    directory: &Path,
    session_id: Option<&str>,
) -> CardId {
    let (card, target) = starting_chat_in(app, provider, directory);
    let handle = fixture_handle(app, card, session_id);
    answer_start(app, target, Ok(handle));
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Chat(_))));
    settle(app, card, Ok(()));
    if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
        preparation::drain_fixture(runtime);
    }
    assert!(chat_runtime(app, card).queued.is_none());
    card
}

/// A chat still starting, with its first message on its way.
pub(in crate::app) fn chat_sending_its_first_message(app: &mut Cagents) -> CardId {
    let (card, _) = starting_chat(app);
    assert!(app.chat_first_message_on_its_way(card).is_some());
    card
}

/// A focused live chat being closed, and the daemon's refusal of that
/// Close, which keeps it.
pub(in crate::app) fn refusing_close(app: &mut Cagents) -> (CardId, Message) {
    let card = live_chat(app, Some(KNOWN_THREAD));
    drop(app.workspace.focus(card));
    assert_eq!(app.workspace.focused(), Some(card));
    let source = app.chat_return_session(card).unwrap();
    drop(app.begin_chat_close(card));
    let close = app.chat_ui.phases[&card].target();
    (
        card,
        Message::ChatUi(Action::RetainedCloseFinished(
            close,
            source,
            Err("Codex is still running for this conversation.".into()),
        )),
    )
}

/// A live Claude chat whose turn is still running.
pub(in crate::app) fn working_chat(app: &mut Cagents, session_id: &str) -> CardId {
    let card = live_chat_for_provider(app, Provider::Claude, Some(session_id));
    let handle = chat_runtime(app, card).handle().unwrap();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.busy = true;
    handle.publish_snapshot_for_test(snapshot);
    card
}

#[test]
fn a_chat_labels_its_links_from_the_projects_and_the_inspector() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1600.0);
    let card = live_chat(&mut app, Some("session"));
    app.open_pane_inspector_now(card);
    let project = ProjectId::new();
    let mut listed = SidebarPullRequest::new(
        project,
        20,
        "Retry stalled daemon handshakes".into(),
        "https://github.com/acme/widgets/pull/20".parse().unwrap(),
        "retry".into(),
        None,
        false,
        crate::sidebar::CheckStatus::Unknown,
    );
    listed.state = Some(crate::sidebar::WorktreePullRequestState::Merged);
    let mut row = SidebarProject::new(
        project,
        crate::repository::RepositoryId(project.as_uuid()),
        "widgets".into(),
        "/w/widgets".into(),
        Vec::new(),
        Vec::new(),
    );
    row.replace_pull_requests(vec![listed]);
    app.sidebar_model.projects.push(row);
    let label = |app: &Cagents, number: u64| {
        chat_runtime(app, card).pull_request_labels.label(
            &crate::github_identity::PullRequestKey::from_url(&format!(
                "https://github.com/acme/widgets/pull/{number}"
            ))
            .unwrap(),
        )
    };
    assert_eq!(label(&app, 20).glyph, None);
    // Any update brings the chat up to date with what the projects list.
    drop(app.update(Message::ChatUi(Action::Tick)));
    assert_eq!(
        label(&app, 20).glyph,
        Some(crate::sidebar::WorktreePullRequestState::Merged)
    );
    assert!(
        label(&app, 20)
            .text
            .ends_with("Retry stalled daemon handshakes")
    );
    let labels = chat_runtime(&app, card).pull_request_labels.clone();
    drop(app.update(Message::ChatUi(Action::Tick)));
    assert!(
        Arc::ptr_eq(&labels, &chat_runtime(&app, card).pull_request_labels),
        "nothing changed, so nothing is rebuilt"
    );

    // A pull request no project lists reads from the inspector's list
    // for this conversation, and from nothing else.
    assert_eq!(label(&app, 21).glyph, None);
    let conversation = chat_runtime(&app, card).session.id;
    let mut list = pull_request_links::fixture::fixture_list();
    list.repository = None;
    let watch = |conversation| crate::app::pull_request_ui::ConversationWatch::Listed {
        conversation,
        revision: 1,
        list: Arc::new(list.clone()),
        first: true,
    };
    drop(app.update(Message::PullRequestUi(
        crate::app::pull_request_ui::Action::Conversation(watch(TerminalId::new())),
    )));
    assert_eq!(label(&app, 21).glyph, None, "another conversation's list");
    drop(app.update(Message::PullRequestUi(
        crate::app::pull_request_ui::Action::Conversation(watch(conversation)),
    )));
    assert_eq!(
        label(&app, 21).glyph,
        Some(crate::sidebar::WorktreePullRequestState::Merged)
    );
    // What a project lists stays first, merged as its poll says, though
    // the inspector's list still has it open.
    assert_eq!(
        label(&app, 20).glyph,
        Some(crate::sidebar::WorktreePullRequestState::Merged)
    );

    // Once the inspector is closed nothing keeps its list current, so a
    // pull request only it named keeps its title but no longer claims a
    // state; what a project lists is still kept current by its poll.
    assert!(app.inspector_watches(conversation));
    drop(app.set_pane_inspector_open(card, false));
    app.settle_pane_inspectors();
    drop(app.update(Message::ChatUi(Action::Tick)));
    assert!(!app.inspector_watches(conversation));
    assert_eq!(label(&app, 21).glyph, None);
    assert!(label(&app, 21).text.len() > "#21".len(), "keeps its title");
    assert!(
        label(&app, 21)
            .tip("https://github.com/acme/widgets/pull/21")
            .contains("State unknown")
    );
    assert_eq!(
        label(&app, 20).glyph,
        Some(crate::sidebar::WorktreePullRequestState::Merged)
    );
    app.open_pane_inspector_now(card);
    drop(app.update(Message::ChatUi(Action::Tick)));
    assert_eq!(
        label(&app, 21).glyph,
        Some(crate::sidebar::WorktreePullRequestState::Merged),
        "watched again"
    );

    // Each pane's inspector watches its own conversation: focusing
    // another pane, or opening its inspector too, leaves this one's list
    // vouched for.
    let other = live_chat(&mut app, Some("other-session"));
    let other_conversation = chat_runtime(&app, other).session.id;
    drop(app.update(Message::FocusCard(other)));
    drop(app.update(Message::ChatUi(Action::Tick)));
    assert!(app.inspector_watches(conversation));
    assert!(!app.inspector_watches(other_conversation));
    app.open_pane_inspector_now(other);
    drop(app.update(Message::ChatUi(Action::Tick)));
    assert!(app.inspector_watches(conversation) && app.inspector_watches(other_conversation));
    assert_eq!(
        label(&app, 21).glyph,
        Some(crate::sidebar::WorktreePullRequestState::Merged),
        "still watched"
    );
}

#[test]
fn deferred_title_marker_only_rearms_the_exact_current_candidate_without_an_eager_task() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let id = crate::model::ChannelId::new();
    app.chat_ui.channel_names.insert(
        target,
        ChannelNameObservation {
            session: Some(KNOWN_THREAD.into()),
            title: Some((
                "Encryption review".into(),
                crate::model::ChannelTitleSource::Provider,
            )),
            last_view: Some((id, 17)),
            ..Default::default()
        },
    );
    let candidate =
        |revision, name: &str| crate::workspace_model::ModelEdit::SuggestConversationTitle {
            id,
            card,
            expected_revision: revision,
            title: name.into(),
            source: crate::model::ChannelTitleSource::Provider,
        };
    app.defer_channel_title_suggestion(&candidate(16, "Encryption review"));
    app.defer_channel_title_suggestion(&candidate(17, "Older name"));
    assert!(!app.chat_ui.channel_names[&target].pending);
    let task = app.apply_workspace_model_update(
        super::super::workspace_model_sync::ModelUpdate::TitleDeferred(candidate(
            17,
            "Encryption review",
        )),
    );
    assert!(cosmic::iced::runtime::task::into_stream(task).is_none());
    assert!(app.chat_ui.channel_names[&target].pending);
}

#[test]
fn a_refused_first_prompt_name_is_offered_again_from_a_chat_on_no_screen() {
    use crate::model::{ChannelId, ChannelTitleSource, ProjectChannel, ProjectId};
    use crate::workspace_model::{
        ModelEdit,
        views::{Pane, Scope, View},
    };
    let mut app = crate::app::tests::context_inspector_test_app(false);
    // A chat whose provider has not reported its conversation, and whose
    // window has moved on: it reads no snapshot, so nothing it shows
    // can name the channel.
    let card = live_chat_for_provider(&mut app, Provider::Claude, None);
    let target = chat_runtime(&app, card).target;
    let id = ChannelId::new();
    Arc::make_mut(&mut app.durable).channels = vec![ProjectChannel {
        id,
        project_id: ProjectId::new(),
        title: String::new(),
        title_source: ChannelTitleSource::Manual,
        ordinal: 0,
        shared: true,
        cards: vec![card],
        focused: Some(card),
    }];
    let pane = Pane::from_card(
        app.card_state(card).unwrap(),
        app.durable.card_scopes.get(&card).cloned(),
    )
    .unwrap();
    let view = move |revision| View {
        scope: Scope::Channel(id),
        revision,
        retired: Vec::new(),
        panes: vec![pane.clone()],
    };
    app.workspace_model.views = vec![view(5)];
    let (tx, requests) = super::super::workspace_model_sync::test_model_channel();
    app.workspace_model_tx = tx;
    let suggested = |revision| {
        std::iter::from_fn(|| requests.try_recv().ok()).any(|edit| {
            matches!(
                edit,
                ModelEdit::SuggestConversationTitle {
                    id: channel,
                    card: pane,
                    expected_revision,
                    title,
                    source: ChannelTitleSource::Prompt,
                } if channel == id
                    && pane == card
                    && expected_revision == revision
                    && title == "Plan the quarterly roadmap review"
            )
        })
    };

    app.name_channel_from_prompt(target, "Plan the quarterly roadmap review");
    assert!(suggested(5));
    assert_eq!(
        app.durable.channels[0].title,
        "Plan the quarterly roadmap review"
    );
    // The registry refuses it and the window restores the stored name.
    Arc::make_mut(&mut app.durable).channels[0].title = String::new();

    // The same view proof does not retry.
    drop(app.refresh_channel_title_suggestions());
    assert!(!suggested(5));
    // The channel's next view offers the name again.
    app.workspace_model.views = vec![view(6)];
    drop(app.refresh_channel_title_suggestions());
    assert!(suggested(6));
    assert_eq!(
        app.durable.channels[0].title,
        "Plan the quarterly roadmap review"
    );

    // A later prompt never renames a named channel.
    app.chat_ui.channel_names.clear();
    app.name_channel_from_prompt(target, "A later prompt must not replace it");
    assert!(app.chat_ui.channel_names.is_empty());
    assert_eq!(
        app.durable.channels[0].title,
        "Plan the quarterly roadmap review"
    );
}

#[test]
fn a_cleared_channel_name_takes_its_conversation_name_again() {
    use super::super::chrome_ui::Action as Chrome;
    use crate::model::{ChannelId, ChannelTitleSource, ProjectChannel, ProjectId};
    use crate::workspace_model::{
        ModelChannel, ModelEdit,
        views::{Pane, Scope, View},
    };
    const NAME: &str = "Plan the quarterly roadmap review";
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, None);
    let target = chat_runtime(&app, card).target;
    let id = ChannelId::new();
    Arc::make_mut(&mut app.durable).channels = vec![ProjectChannel {
        id,
        project_id: ProjectId::new(),
        title: String::new(),
        title_source: ChannelTitleSource::Manual,
        ordinal: 0,
        shared: true,
        cards: vec![card],
        focused: Some(card),
    }];
    let pane = Pane::from_card(
        app.card_state(card).unwrap(),
        app.durable.card_scopes.get(&card).cloned(),
    )
    .unwrap();
    let view = move |revision| View {
        scope: Scope::Channel(id),
        revision,
        retired: Vec::new(),
        panes: vec![pane.clone()],
    };
    app.workspace_model.views = vec![view(5)];
    let (tx, requests) = super::super::workspace_model_sync::test_model_channel();
    app.workspace_model_tx = tx;
    let edits = || std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    let suggestion = |edit: &ModelEdit| {
        matches!(
            edit,
            ModelEdit::SuggestConversationTitle { id: channel, card: pane, title, .. }
                if *channel == id && *pane == card && title == NAME
        )
    };
    let rename = |app: &mut crate::app::Cagents, name: &str| {
        drop(app.update_chrome(Chrome::RenameChannel(id)));
        drop(app.update_chrome(Chrome::RenameChanged(name.into())));
        drop(app.update_chrome(Chrome::RenameSubmit));
        assert!(app.pane_rename_surface().is_none());
    };
    app.name_channel_from_prompt(target, NAME);
    assert!(edits().iter().any(suggestion));
    assert_eq!(app.automatic_channel_name(id), Some(NAME));

    // A name the person gives is never replaced, even at a later view.
    rename(&mut app, "Roadmap");
    app.workspace_model.views = vec![view(6)];
    drop(app.refresh_channel_title_suggestions());
    assert!(!edits().iter().any(suggestion));
    assert_eq!(app.durable.channels[0].title, "Roadmap");
    // The rename field shows the name clearing gives back.
    let (placeholder, caption, submit) = app.channel_rename_copy(id, true);
    assert_eq!(
        (placeholder.as_ref(), caption, submit),
        (
            NAME,
            "The channel takes its conversation's name.",
            Some("Use automatic name")
        )
    );

    // Clearing it asks the registry to forget it, and the conversation's
    // name follows, though the channel's view did not change.
    rename(&mut app, "  ");
    let sent = edits();
    let cleared = sent.iter().position(|edit| {
        matches!(edit, ModelEdit::RetitleChannel { id: channel, title }
            if *channel == id && title.is_empty())
    });
    let offered = sent.iter().position(suggestion);
    assert!(cleared.is_some() && offered > cleared, "{sent:?}");
    assert_eq!(app.durable.channels[0].title, NAME);

    // Another window's name is taken as it is, and its clear offers the
    // conversation's name again from this window, which runs the
    // conversation.
    let shared = |title: &str| ModelChannel {
        id,
        repository: "/repo/.git".into(),
        title: title.into(),
        title_source: ChannelTitleSource::Manual,
        ordinal: 1,
    };
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.channels = vec![shared("Their name")];
    drop(app.reconcile_workspace_model());
    assert!(!edits().iter().any(suggestion));
    assert_eq!(app.durable.channels[0].title, "Their name");
    app.workspace_model.channels = vec![shared("")];
    drop(app.reconcile_workspace_model());
    assert!(edits().iter().any(suggestion));

    // A model sent before a rename here looks like a clear, but this
    // window keeps that rename, so an unattended channel keeps its name.
    rename(&mut app, "Roadmap");
    drop(edits());
    assert_eq!(app.workspace_model.channels, vec![shared("")]);
    drop(app.reconcile_workspace_model());
    assert!(app.channel_is_named(id));
}

#[test]
fn presentation_projection_retries_a_deferred_title_with_the_ahead_scope_proof() {
    use super::super::workspace_model_sync::{EdgeChanges, ModelUpdate};
    use crate::model::{ChannelId, ChannelTitleSource, ProjectChannel, ProjectId};
    use crate::repository::RepositoryId;
    use crate::workspace_model::{
        ModelChannel, ModelEdit, ModelProject,
        delta::{EditEffects, ViewChanges, ViewRevision},
        views::{Pane, Scope, View},
    };
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let channel = ChannelId::new();
    let project = ProjectId::new();
    let repository = PathBuf::from("/repo/.git");
    Arc::make_mut(&mut app.durable).channels = vec![ProjectChannel {
        id: channel,
        project_id: project,
        title: "Old automatic title".into(),
        title_source: ChannelTitleSource::Prompt,
        ordinal: 1,
        shared: true,
        cards: vec![card],
        focused: Some(card),
    }];
    Arc::make_mut(&mut app.durable).active_channel = Some(channel);
    app.sidebar_model
        .projects
        .push(crate::sidebar::SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            repository.clone(),
            vec![],
            vec![],
        ));
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.projects = vec![ModelProject {
        repository: repository.clone(),
        name: "repo".into(),
        kind: crate::model::ProjectKind::Git,
        ordinal: 0,
    }];
    app.workspace_model.channels = vec![ModelChannel {
        id: channel,
        repository,
        title: "Old automatic title".into(),
        title_source: ChannelTitleSource::Prompt,
        ordinal: 1,
    }];
    let pane = Pane::from_card(
        app.card_state(card).unwrap(),
        app.durable.card_scopes.get(&card).cloned(),
    )
    .unwrap();
    let scope = Scope::Channel(channel);
    let before = View {
        scope: scope.clone(),
        revision: 17,
        panes: vec![pane.clone()],
        retired: vec![],
    };
    app.workspace_model.views = vec![before.clone()];
    app.shared_views = super::super::shared_views::State::for_workspace(&app.durable);
    app.shared_views
        .initialize(scope.clone(), vec![pane.clone()]);
    app.shared_views.attached.insert(card);
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.title = Some("Current provider title".into());
    handle.publish_snapshot_for_test(snapshot);
    app.chat_ui.channel_names.insert(
        target,
        ChannelNameObservation {
            session: Some(KNOWN_THREAD.into()),
            title: Some((
                "Current provider title".into(),
                ChannelTitleSource::Provider,
            )),
            last_view: Some((channel, 17)),
            pending: true,
            ..Default::default()
        },
    );
    let mut accepted = before.clone();
    accepted.revision = 21;
    accepted.panes[0].width_fraction = 0.68;
    assert_eq!(
        app.presentation_projection_ready(&before, &accepted, None),
        Ok(())
    );
    let (tx, rx) = test_model_channel();
    app.workspace_model_tx = tx;
    let update = ModelUpdate::Changed {
        sequence: 18,
        edit: ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: 17,
            operation: uuid::Uuid::new_v4(),
            change: crate::workspace_model::views::Change::Update {
                before: pane,
                pane: accepted.panes[0].clone(),
            },
        },
        changes: EdgeChanges {
            identity: Some(app.workspace_model.identity),
            views: ViewChanges {
                sequence: 21,
                views: vec![accepted],
                ..Default::default()
            },
            effects: EditEffects {
                views: vec![ViewRevision {
                    scope,
                    before: Some(17),
                    after: Some(18),
                }],
                ..Default::default()
            },
        },
    };
    let (_, projection) = app.apply_workspace_model_update_projection(update);
    assert!(matches!(projection, super::super::Projection::Chrome));
    assert_eq!(app.durable.channels[0].title, "Current provider title");
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|edit| matches!(edit,
        ModelEdit::SuggestConversationTitle { id, card: candidate, expected_revision: 21, title, .. }
            if id == channel && candidate == card && title == "Current provider title")));
    assert_eq!(
        app.chat_ui.channel_names[&target].last_view,
        Some((channel, 21))
    );
}

#[test]
fn a_secondary_title_wake_is_used_when_it_becomes_primary_without_another_wake() {
    use crate::model::{ChannelId, ChannelTitleSource, ProjectChannel, ProjectId};
    use crate::workspace_model::views::{Pane, Scope, View};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let a = live_chat(&mut app, Some(KNOWN_THREAD));
    let b = live_chat(&mut app, Some("0299a2b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b"));
    let target = chat_runtime(&app, b).target;
    let original = app.card_state(b).unwrap().kind.clone();
    let id = ChannelId::new();
    Arc::make_mut(&mut app.durable).channels = vec![ProjectChannel {
        id,
        project_id: ProjectId::new(),
        title: "First conversation".into(),
        title_source: ChannelTitleSource::Provider,
        ordinal: 0,
        shared: true,
        cards: vec![a, b],
        focused: Some(b),
    }];
    app.workspace_model.views = vec![View {
        scope: Scope::Channel(id),
        revision: 1,
        retired: Vec::new(),
        panes: [a, b]
            .into_iter()
            .map(|card| {
                Pane::from_card(
                    app.card_state(card).unwrap(),
                    Some(FIXTURE_DIRECTORY.into()),
                )
                .unwrap()
            })
            .collect(),
    }];
    let handle = chat_runtime(&app, b).handle().unwrap().clone();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.title = Some("Secondary encryption review".into());
    handle.publish_snapshot_for_test(snapshot);
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    assert_eq!(app.durable.channels[0].title, "First conversation");
    assert_eq!(
        app.chat_ui.channel_names[&target]
            .title
            .as_ref()
            .map(|(title, _)| title.as_str()),
        Some("Secondary encryption review"),
        "the secondary pane caches its own title without renaming the primary channel"
    );
    // These are the accepted host projection values after its primary
    // changed and its automatic old name was cleared in the same TX.
    app.workspace_model.views[0].panes.swap(0, 1);
    app.workspace_model.views[0].revision += 1;
    let channel = &mut Arc::make_mut(&mut app.durable).channels[0];
    channel.cards = vec![b, a];
    channel.title.clear();
    channel.title_source = ChannelTitleSource::Prompt;
    drop(app.refresh_channel_title_suggestions());
    assert_eq!(app.durable.channels[0].title, "Secondary encryption review");
    assert_eq!(
        app.durable.channels[0].title_source,
        ChannelTitleSource::Provider
    );
    assert_eq!(
        app.card_state(b).unwrap().kind,
        original,
        "naming does not change launch or conversation identity"
    );
}

#[test]
fn prompt_and_provider_titles_name_the_chat_tab_and_window() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.core.set_main_window_id(Some(window::Id::RESERVED));
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target).unwrap().show(true);
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let mut snapshot = Snapshot {
        ready: true,
        ..Snapshot::default()
    };
    snapshot.items.push(Arc::new(chat::Item {
        id: "prompt".into(),
        role: chat::Role::User,
        content: text("Implement conversation titles"),
        started_at: None,
        at: None,
    }));
    for title in [None, Some("Provider summary".to_owned())] {
        snapshot.title = title.clone();
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot.clone());
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
        drop(app.sync_chrome());
        let expected = title.as_deref().unwrap_or("Implement conversation titles");
        assert_eq!(
            app.card_display_title(app.card_state(card).unwrap()),
            expected
        );
        assert_eq!(app.card_chrome[&card].title, expected);
        assert_eq!(app.core.window.header_title, expected);
        assert_eq!(
            app.title(window::Id::RESERVED),
            format!("{expected}{WINDOW_TITLE_SEPARATOR}{APPLICATION_NAME}")
        );
    }
}

#[test]
fn manual_channel_name_titles_its_main_chat_without_hiding_sibling_titles() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    app.card_state_mut(card).unwrap().title = "Provider conversation summary".into();
    let mut sibling = app.card_state(card).unwrap().clone();
    sibling.id = CardId::new();
    sibling.title = "Sibling conversation".into();
    let sibling_id = sibling.id;
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.push(sibling);
    durable.channels.push(crate::model::ProjectChannel {
        id: crate::model::ChannelId::new(),
        project_id: crate::model::ProjectId::new(),
        title: "My renamed conversation".into(),
        title_source: crate::model::ChannelTitleSource::Manual,
        ordinal: 1,
        shared: false,
        cards: vec![card, sibling_id],
        focused: Some(card),
    });
    assert_eq!(
        app.card_display_title(app.card_state(card).unwrap()),
        "My renamed conversation"
    );
    assert_eq!(
        app.card_display_title(app.card_state(sibling_id).unwrap()),
        "Sibling conversation"
    );
    drop(app.sync_chrome());
    assert_eq!(app.core.window.header_title, "My renamed conversation");
}

#[test]
fn manual_channel_title_survives_provider_updates_after_utility_and_closing_panes() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.core.set_main_window_id(Some(window::Id::RESERVED));
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let terminal = crate::app::tests::terminal_card_state();
    let terminal_id = terminal.id;
    let mut closing = app.card_state(card).unwrap().clone();
    closing.id = CardId::new();
    closing.existence = Existence::Closing {
        operation_id: crate::model::OperationId::new(),
    };
    let closing_id = closing.id;
    let mut sibling = app.card_state(card).unwrap().clone();
    sibling.id = CardId::new();
    sibling.title = "Sibling conversation".into();
    let sibling_id = sibling.id;
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.extend([terminal, closing, sibling]);
    durable.channels.push(crate::model::ProjectChannel {
        id: crate::model::ChannelId::new(),
        project_id: crate::model::ProjectId::new(),
        title: "My renamed conversation".into(),
        title_source: crate::model::ChannelTitleSource::Manual,
        ordinal: 1,
        shared: false,
        cards: vec![CardId::new(), terminal_id, closing_id, card, sibling_id],
        focused: Some(card),
    });
    assert_eq!(
        app.channel_primary_chat(app.durable.channels.last().unwrap()),
        Some(card)
    );
    let snapshot = Snapshot {
        ready: true,
        title: Some("New provider conversation summary".into()),
        generation: 1,
        ..Snapshot::default()
    };
    handle.publish_snapshot_for_test(snapshot);
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    drop(app.sync_chrome());
    assert_eq!(
        app.card_state(card).unwrap().title,
        "New provider conversation summary"
    );
    assert_eq!(app.card_chrome[&card].title, "My renamed conversation");
    assert_eq!(app.core.window.header_title, "My renamed conversation");
    assert_eq!(
        app.title(window::Id::RESERVED),
        format!("My renamed conversation{WINDOW_TITLE_SEPARATOR}{APPLICATION_NAME}")
    );
    assert_eq!(
        app.card_display_title(app.card_state(sibling_id).unwrap()),
        "Sibling conversation"
    );
    app.chrome_ui
        .user_titles
        .insert(card, "Explicit pane name".into());
    assert_eq!(
        app.card_display_title(app.card_state(card).unwrap()),
        "Explicit pane name"
    );
}

#[test]
fn cancelling_background_agents_requires_confirmation_and_disarms_at_idle() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.busy = true;
    snapshot.capabilities.interrupt_stops_background = true;
    snapshot.subagents.push(chat::Subagent {
        model: None,
        waiting: false,
        detail: String::new(),
        id: "background".into(),
        kind: String::new(),
        description: String::new(),
        status: chat::ToolStatus::Running,
        background: true,
        items: Vec::new(),
    });
    let busy = (*runtime.snapshot).clone();
    show_chat_snapshot_for_test(&mut app, card, busy);
    drop(app.handle_chat_ui(Action::Interrupt(target)));
    let runtime = chat_runtime(&app, card);
    let armed = runtime
        .interrupt_armed
        .expect("first cancel arms confirmation");
    assert!(runtime.commands.is_idle());
    // An old timer cannot disarm a later confirmation.
    drop(app.handle_chat_ui(Action::DisarmInterrupt(
        target,
        armed - Duration::from_secs(1),
    )));
    assert_eq!(chat_runtime(&app, card).interrupt_armed, Some(armed));
    drop(app.handle_chat_ui(Action::DisarmInterrupt(target, armed)));
    assert!(chat_runtime(&app, card).interrupt_armed.is_none());
    drop(app.handle_chat_ui(Action::Interrupt(target)));
    drop(app.handle_chat_ui(Action::Interrupt(target)));
    assert!(chat_runtime(&app, card).interrupt_armed.is_none());
    assert!(chat_runtime(&app, card).commands.in_flight().is_some());
    settle(&mut app, card, Ok(()));
    drop(app.handle_chat_ui(Action::Interrupt(target)));
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(runtime.interrupt_armed.is_some());
    let mut idle = (*runtime.snapshot).clone();
    idle.busy = false;
    show_chat_snapshot_for_test(&mut app, card, idle);
    assert!(chat_runtime(&app, card).interrupt_armed.is_none());
}

#[test]
fn cancel_confirmation_does_not_cross_a_coalesced_turn_boundary() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    runtime.interrupt_armed = Some(Instant::now());
    let mut next = (*runtime.snapshot).clone();
    next.turn_generation = next.turn_generation.wrapping_add(1);
    // No idle publication, user message, or transcript change is needed
    // for a queued or autonomous turn to invalidate the confirmation.
    show_chat_snapshot_for_test(&mut app, card, next);
    assert!(chat_runtime(&app, card).interrupt_armed.is_none());
}

/// Answers the command `card` is waiting on with `result`.
pub(super) fn settle(app: &mut Cagents, card: CardId, result: Result<(), String>) {
    let runtime = chat_runtime(app, card);
    let id = runtime
        .commands
        .in_flight()
        .expect("a command is pending")
        .id;
    let target = runtime.target;
    let _ = app.update(Message::ChatUi(Action::CommandFinished(target, id, result)));
}

/// A send the conversation declined without harm is told in the
/// transcript, and the message stays to send again, but the conversation
/// does not read as failed. One the conversation can no longer take does.
#[test]
fn a_declined_send_is_a_notice_unless_the_conversation_cannot_go_on() {
    use chat::failure::delivery;
    for (outcome, conversation_error) in [
        (delivery::UNCONFIRMED_INPUT, false),
        (delivery::UNACKNOWLEDGED_INPUT, false),
        (delivery::UNACKNOWLEDGED_ANSWER, false),
        (delivery::UNACKNOWLEDGED_STOP, false),
        (delivery::UNSETTLED_INPUT, false),
        (delivery::RECONNECTING_INPUT, false),
        (delivery::CROWDED_INPUT, false),
        (delivery::RETIRING_INPUT, false),
        (delivery::OVERSIZED_INPUT, false),
        (delivery::HANDOVER_INPUT, false),
        (delivery::DAMAGED_INPUT, false),
        (delivery::UNSETTLEABLE_INPUT, true),
        (delivery::STOPPED_INPUT, true),
        (delivery::CLOSING_INPUT, true),
        (delivery::DISCONTINUOUS_INPUT, true),
        (delivery::UNSUPPORTED_INPUT, true),
        (delivery::UNAVAILABLE_INPUT, true),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("Keep this message"));
        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
        settle(&mut app, card, Err(outcome.into()));
        let runtime = chat_runtime(&app, card);
        assert!(
            runtime.failures.iter().any(|note| note.text == outcome),
            "{outcome}: {:?}",
            runtime.failures
        );
        assert_eq!(runtime.composer.text().trim(), "Keep this message");
        assert_eq!(
            runtime.reported_errors().next().is_some(),
            conversation_error,
            "{outcome}"
        );
        assert_eq!(
            runtime.conversation_status(false).1 == "Error",
            conversation_error,
            "{outcome}: {:?}",
            runtime.conversation_status(false)
        );
        assert_eq!(
            runtime.finished_label(),
            if conversation_error {
                "Finished with error"
            } else {
                "Finished"
            },
            "{outcome}"
        );
    }
}

/// A provider exit reported after a declined send replaces the advice to
/// send again, whatever declined it; an outcome that already says the
/// conversation cannot go on stays beside the exit.
#[test]
fn an_exit_after_a_declined_send_replaces_only_a_send_notice() {
    use chat::failure::delivery;
    for (outcome, kept) in [
        (delivery::CROWDED_INPUT, false),
        (delivery::UNACKNOWLEDGED_INPUT, false),
        (delivery::UNAVAILABLE_INPUT, true),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("Keep this message"));
        drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
        settle(&mut app, card, Err(outcome.into()));
        assert_eq!(chat_runtime(&app, card).error.as_deref(), Some(outcome));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.exited = true;
        handle.publish_snapshot_for_test(snapshot);
        drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.snapshot.exited, "{outcome}");
        assert_eq!(runtime.error.is_some(), kept, "{outcome}");
        assert_eq!(runtime.composer.text().trim(), "Keep this message");
    }
}

/// A restored conversation, begun but never named by its provider, whose
/// start the daemon refused.
fn failed_chat(app: &mut Cagents) -> CardId {
    let card = live_chat(app, None);
    let session = chat_runtime(app, card).session.clone();
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    let _ = app.open_chat(card, session, chat::Intent::Restore);
    let target = chat_runtime_attempt(app, card);
    answer_start(
        app,
        target,
        Err(chat::StartFailure::retry("fixture: executable unavailable")),
    );
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    app.notices.entries.clear();
    card
}

pub(super) fn chat_runtime_attempt(app: &Cagents, card: CardId) -> Target {
    app.chat_ui
        .attempt(card)
        .expect("a start is in flight")
        .target
}

/// Closes `card` the way its close button does, and checks that it is gone
/// for good: off the strip, tombstoned, and held by nothing.
fn close_chat(app: &mut Cagents, card: CardId) {
    let _ = app.update(Message::CloseCard(card));
    if let Some(Phase::Closing {
        target,
        restart: None,
    }) = app.chat_ui.phases.get(&card)
    {
        let target = *target;
        let source = app.chat_return_session(card).unwrap();
        assert!(app.card_state(card).unwrap().existence.is_open());
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            target,
            source,
            Ok(()),
        )));
    }
    assert_eq!(
        app.workspace.pending_close(),
        None,
        "chat removal commits in the same update"
    );
    let _ = app.update(Message::CommitClose(card)); // A queued duplicate is harmless.
    assert!(
        !app.workspace.order().any(|id| id == card),
        "the card leaves the strip"
    );
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(!app.cards.contains_key(&card));
    assert!(
        errors(app).is_empty(),
        "a close is not an error: {:?}",
        errors(app)
    );
    assert!(
        !app.chat_ui.phases.contains_key(&card),
        "no active operation outlives the card"
    );
    assert!(!app.chat_start_pending(card));
}

#[test]
fn late_native_ownership_is_saved_even_while_the_chat_is_hidden() {
    for provider in [Provider::Codex, Provider::Claude] {
        for visible in [true, false] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat_for_provider(&mut app, provider, Some(KNOWN_THREAD));
            let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                unreachable!()
            };
            let target = runtime.target;
            runtime.show(visible);
            let handle = runtime.handle().unwrap().clone();
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            let capability = match provider {
                Provider::Codex => chat::NativeContinuity::Durable {
                    provider_version: "0.157.1".into(),
                },
                Provider::Claude => chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
                _ => unreachable!(),
            };
            snapshot.native_continuity = capability.clone();
            handle.replace_snapshot_for_test(snapshot);
            let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Activity)));
            let CardKind::Chat(saved) = &app.card_state(card).unwrap().kind else {
                unreachable!()
            };
            assert_eq!(saved.native_continuity, capability);
            assert!(saved.has_valid_native_continuity());
            assert_eq!(
                app.chat_return_session(card).unwrap().native_continuity,
                saved.native_continuity
            );
        }
    }
}

/// A restored conversation that cannot be reopened says so in the reader's
/// terms, keeps the daemon's reason as detail, and stops saying it once a
/// retry has reopened it.
#[test]
fn a_successful_retry_clears_the_failed_reopen_notice() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let session = chat_runtime(&app, card).session.clone();
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    let _ = app.open_chat(card, session, chat::Intent::Restore);
    let reason = "Exact native chat not found; no replacement process was spawned";
    let attempt = chat_runtime_attempt(&app, card);
    answer_start(&mut app, attempt, Err(chat::StartFailure::retry(reason)));
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    let pane = app.notice_pane_name(card).unwrap();
    assert_eq!(errors(&app), [format!("Could not reopen {pane}.")]);
    assert_eq!(attach_failure_details(&app, card).as_deref(), Some(reason));

    // A retry that fails for another reason replaces the notice rather
    // than adding one beside it.
    let _ = app.update(Message::RetryCard(card));
    let attempt = chat_runtime_attempt(&app, card);
    let later = "fixture: executable unavailable";
    answer_start(&mut app, attempt, Err(chat::StartFailure::retry(later)));
    assert_eq!(app.notices.len(), 1);
    assert_eq!(attach_failure_details(&app, card).as_deref(), Some(later));

    let _ = app.update(Message::RetryCard(card));
    let attempt = chat_runtime_attempt(&app, card);
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    answer_start(&mut app, attempt, Ok(handle));
    assert!(chat_runtime(&app, card).connected());
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
}

/// Starting over is a start, not a reopen, and says so when it fails.
/// Its cleared ownership also lets a later window open it as a draft
/// without asking the daemon to restore a conversation that never began.
#[test]
fn a_fresh_start_reports_a_start_and_restores_as_an_unstarted_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let session = chat_runtime(&app, card).session.clone();
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    let _ = app.open_chat(card, session, chat::Intent::Restore);
    let attempt = chat_runtime_attempt(&app, card);
    let gone = "the provider no longer has this conversation";
    answer_start(&mut app, attempt, Err(chat::StartFailure::fresh(gone)));
    let pane = app.notice_pane_name(card).unwrap();
    assert_eq!(errors(&app), [format!("Could not reopen {pane}.")]);
    assert!(app.chat_offers_fresh_conversation(card));

    let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
    let closing = fresh_close_request(&app, card);
    let _ = app.update(Message::ChatUi(fresh_close_answer(closing, Ok(()))));
    let attempt = chat_runtime_attempt(&app, card);
    let unavailable = "fixture: executable unavailable";
    answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure::retry(unavailable)),
    );
    assert_eq!(errors(&app), [format!("Could not start {pane}.")]);
    assert_eq!(
        attach_failure_details(&app, card).as_deref(),
        Some(unavailable)
    );

    let Some(CardKind::Chat(unnamed)) = app.card_state(card).map(|state| state.kind.clone()) else {
        unreachable!();
    };
    assert!(unnamed.is_unstarted());
    let mut restored = crate::app::tests::context_inspector_test_app(false);
    restored.durable = app.durable.clone();
    restored
        .chat_ui
        .drafts
        .insert(card, "Keep this unsent note.".into());
    let _ = restored.open_chat(card, unnamed.clone(), chat::Intent::Restore);
    let runtime = chat_runtime(&restored, card);
    assert!(runtime.is_draft());
    assert_eq!(runtime.session, unnamed);
    assert_eq!(runtime.composer.text().trim_end(), "Keep this unsent note.");
    assert!(restored.chat_ui.attempt(card).is_none());
    assert!(errors(&restored).is_empty(), "{:?}", errors(&restored));
}

/// Older versions could begin a provider before its first message or
/// confirmed name. If that actor is gone, its failure notice no longer
/// applies when the daemon tells the card to return to a draft.
#[test]
fn a_begun_but_unnamed_conversation_withdraws_its_failure_when_restored_as_a_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, None);
    let unnamed = chat_runtime(&app, card).session.clone();
    assert_eq!(unnamed.launch.session_id, None);
    assert!(unnamed.proof.is_some());
    assert!(!unnamed.is_unstarted());
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    let _ = app.open_chat(card, unnamed, chat::Intent::Restore);
    let attempt = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure::retry("fixture: executable unavailable")),
    );
    let pane = app.notice_pane_name(card).unwrap();
    assert_eq!(errors(&app), [format!("Could not reopen {pane}.")]);

    let _ = app.update(Message::RetryCard(card));
    let attempt = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure::fresh(
            "the provider no longer has this conversation",
        )),
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.is_draft());
    assert!(runtime.session.is_unstarted());
    assert!(app.chat_ui.attempt(card).is_none());
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
}

/// A conversation whose connection is gone already says so in its pane,
/// and reattaching presents its visibility afresh, so telling it whether
/// it is on screen cannot fail in a way worth a notice.
#[test]
fn a_disconnected_chat_raises_no_visibility_notice() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    chat_runtime(&app, card)
        .handle()
        .unwrap()
        .disconnect_for_test();
    app.set_workspace_surfaces_visible(false);
    app.set_workspace_surfaces_visible(true);
    assert!(
        app.status.is_none(),
        "{:?}",
        app.status.as_ref().map(|notice| &notice.message)
    );
}

#[test]
fn a_metadata_only_disconnected_client_keeps_the_last_readable_runtime_body() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    // An empty headless Workspace starts at width zero. Establish a real
    // visible pane before testing the App's durable exposure gate.
    app.workspace.resize_viewport(1280.0);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let snapshot = Snapshot {
        generation: 17,
        session_id: Some(KNOWN_THREAD.into()),
        ready: true,
        items: vec![
            Arc::new(chat::Item {
                id: "question".into(),
                role: chat::Role::User,
                content: text("Original question"),
                started_at: None,
                at: None,
            }),
            Arc::new(chat::Item {
                id: "answer".into(),
                role: chat::Role::Assistant,
                content: text("Last readable parent answer"),
                started_at: None,
                at: None,
            }),
        ],
        subagents: vec![chat::Subagent {
            id: "child".into(),
            kind: "fixture".into(),
            description: "Child".into(),
            status: chat::ToolStatus::Succeeded,
            model: None,
            waiting: false,
            detail: String::new(),
            background: false,
            items: vec![Arc::new(chat::Item {
                id: "child-answer".into(),
                role: chat::Role::Assistant,
                content: text("Last readable child answer"),
                started_at: None,
                at: None,
            })],
        }],
        ..Snapshot::default()
    };
    let parent = Arc::downgrade(&snapshot.items[1]);
    let child = Arc::downgrade(&snapshot.subagents[0].items[0]);
    show_chat_snapshot_for_test(&mut app, card, snapshot);
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
    let runtime = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(runtime);
    assert!(!runtime.child_cache["child-answer"].placeholder);
    runtime.replace_draft(text_editor::Content::with_text("Unsent draft"));
    let scroll = view::Scroll::new(75.0, 900.0, 400.0);
    runtime.subagent_scroll.insert("child".into(), scroll);
    let handle = runtime.handle().unwrap().clone();
    // The real policy transaction installs metadata before the visibility
    // send can fail. Disconnect at that boundary, while Runtime still owns
    // the full publication: no fabricated metadata-only fixture is needed.
    handle.set_interest(false, false).unwrap();
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert!(handle.resident_snapshot_with_clock().is_none());
    assert!(runtime.snapshot.transcript_bytes() > 0);
    handle.disconnect_for_test();

    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    assert_eq!(app.exposure(card), Exposure::Visible);
    assert!(chat_runtime(&app, card).visible.load(Ordering::Relaxed));
    app.set_workspace_surfaces_visible(false);
    preparation::retirement_barrier();
    let runtime = chat_runtime(&app, card);
    assert!(!runtime.visible.load(Ordering::Relaxed));
    assert!(runtime.cache.is_empty());
    assert!(runtime.child_cache.is_empty());
    assert!(runtime.selection_document.borrow().is_none());
    assert!(Arc::ptr_eq(
        &runtime.snapshot.items[1],
        &parent.upgrade().unwrap()
    ));
    assert!(Arc::ptr_eq(
        &runtime.snapshot.subagents[0].items[0],
        &child.upgrade().unwrap()
    ));
    assert_eq!(runtime.history, ["Original question"]);
    assert_eq!(runtime.composer.text().trim_end(), "Unsent draft");
    assert_eq!(runtime.selected_subagent.as_deref(), Some("child"));
    assert_eq!(runtime.subagent_scroll["child"], scroll);

    app.set_workspace_surfaces_visible(true);
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    assert_eq!(app.exposure(card), Exposure::Visible);
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(runtime.visible.load(Ordering::Relaxed));
    preparation::drain_fixture(runtime);
    assert!(handle.resident_snapshot_with_clock().is_none());
    assert!(!handle.is_connected());
    assert!(!runtime.child_cache["child-answer"].placeholder);
    assert!(shown(&runtime.child_cache["child-answer"]).contains("Last readable child answer"));
    assert_eq!(
        copy_text(
            CopyPart::Text,
            &runtime.snapshot.subagents[0].items[0].content
        )
        .as_deref(),
        Some("Last readable child answer")
    );
    assert_eq!(runtime.selected_subagent.as_deref(), Some("child"));
    assert_eq!(runtime.subagent_scroll["child"], scroll);
    assert_eq!(runtime.composer.text().trim_end(), "Unsent draft");
    assert_eq!(runtime.history, ["Original question"]);
    drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
    let runtime = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(runtime);
    assert!(!runtime.cache["answer"].placeholder);
    assert!(shown(&runtime.cache["answer"]).contains("Last readable parent answer"));
    assert_eq!(
        copy_text(CopyPart::Text, &runtime.snapshot.items[1].content).as_deref(),
        Some("Last readable parent answer")
    );
    assert!(runtime.error.is_none());
    assert!(app.status.is_none());
    assert!(errors(&app).is_empty());
}

#[test]
fn a_disconnected_resident_client_can_still_release_and_restore_runtime_body() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    // An empty headless Workspace starts at width zero. Establish a real
    // visible pane before testing the App's durable exposure gate.
    app.workspace.resize_viewport(1280.0);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    show_chat_snapshot_for_test(
        &mut app,
        card,
        Snapshot {
            ready: true,
            items: vec![Arc::new(chat::Item {
                id: "resident-answer".into(),
                role: chat::Role::Assistant,
                content: text("Resident client answer"),
                started_at: None,
                at: None,
            })],
            ..Snapshot::default()
        },
    );
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    handle.disconnect_for_test();
    assert!(handle.resident_snapshot_with_clock().is_some());
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    assert_eq!(app.exposure(card), Exposure::Visible);
    assert!(chat_runtime(&app, card).visible.load(Ordering::Relaxed));
    app.set_workspace_surfaces_visible(false);
    let runtime = chat_runtime(&app, card);
    assert!(!runtime.visible.load(Ordering::Relaxed));
    assert_eq!(runtime.snapshot.transcript_bytes(), 0);
    assert!(runtime.cache.is_empty());
    assert!(handle.snapshot().transcript_bytes() > 0);
    app.set_workspace_surfaces_visible(true);
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    assert_eq!(app.exposure(card), Exposure::Visible);
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(runtime.visible.load(Ordering::Relaxed));
    preparation::drain_fixture(runtime);
    assert!(!runtime.cache["resident-answer"].placeholder);
    assert!(shown(&runtime.cache["resident-answer"]).contains("Resident client answer"));
    assert!(app.status.is_none());
    assert!(errors(&app).is_empty());
}

#[test]
fn connected_read_only_history_releases_body_until_the_daemon_rehydrates_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
    let source_runtime = chat_runtime(&app, card);
    let missing = tempfile::tempdir().unwrap();
    let mut session = source_runtime.session.clone();
    session.launch.session_id = Some(KNOWN_THREAD.into());
    session.launch.recipe.working_directory = missing.path().join("removed");
    assert!(session.launch.needs_read_only_history());
    let snapshot = || Snapshot {
        generation: 17,
        session_id: Some(KNOWN_THREAD.into()),
        ready: true,
        exited: true,
        read_only_reason: Some("Working directory no longer exists".into()),
        items: vec![Arc::new(chat::Item {
            id: "archived-answer".into(),
            role: chat::Role::Assistant,
            content: text("Archived answer"),
            started_at: None,
            at: None,
        })],
        ..Snapshot::default()
    };
    let first = snapshot();
    let body = Arc::downgrade(&first.items[0]);
    let handle = Arc::new(ClientHandle::fixture(session.clone(), first));
    let mut runtime = Runtime::new(
        source_runtime.target,
        session,
        Connection::Live(handle.clone()),
        true,
        source_runtime.syntax_theme,
    );
    runtime.refresh();
    preparation::drain_fixture(&mut runtime);
    assert!(!runtime.cache["archived-answer"].placeholder);
    assert!(runtime.snapshot.exited);
    assert!(runtime.snapshot.read_only_reason.is_some());
    assert!(
        handle.is_connected(),
        "History keeps its daemon IPC connection"
    );
    runtime.set_visible(false).unwrap();
    runtime.cool();
    preparation::retirement_barrier();
    assert_eq!(runtime.snapshot.transcript_bytes(), 0);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    // The fixture's initial client publication retires on a different
    // queue from parser state. Wait for this exact body, not merely the
    // preparation barrier or the installation of metadata.
    let deadline = Instant::now() + Duration::from_secs(5);
    while body.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "the read-only body's client retirement did not complete"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(body.upgrade().is_none());
    assert!(runtime.cache.is_empty());
    runtime.set_visible(true).unwrap();
    assert!(
        runtime.cache.is_empty(),
        "connected History waits for fresh hydration"
    );
    assert!(handle.resident_snapshot_with_clock().is_none());
    handle.replace_snapshot_for_test(snapshot());
    runtime.refresh();
    preparation::drain_fixture(&mut runtime);
    assert!(!runtime.cache["archived-answer"].placeholder);
    assert!(shown(&runtime.cache["archived-answer"]).contains("Archived answer"));
    assert!(runtime.error.is_none());
}

#[test]
fn non_floor_read_only_snapshot_renders_without_index_reads_or_writer_permission() {
    for provider in [Provider::Claude, Provider::Codex] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat_for_provider(&mut app, provider, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session.reset_owned_codex_listener();
        runtime.session.native_continuity = chat::NativeContinuity::ReconnectOnly;
        let snapshot = Snapshot {
            session_id: runtime.session.launch.session_id.clone(),
            exited: true,
            read_only_reason: Some("Saved history; no provider was started".into()),
            ..Default::default()
        };
        // No floor record exists for this fixture's new allocation. The
        // authenticated attached reader is sufficient for display only.
        runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(
            runtime.session.clone(),
            snapshot.clone(),
        )));
        runtime.snapshot = Arc::new(snapshot);
        runtime.replace_draft(text_editor::Content::with_text("Keep this unsent"));
        let session = runtime.session.clone();
        let capabilities = runtime.snapshot.capabilities.clone();
        let reads = crate::upgrade::floor::index_reads_for_test();
        for _ in 0..100 {
            assert!(runtime.has_connected_saved_history(&runtime.snapshot));
            assert_eq!(
                runtime.composer_action_rows(false),
                vec![(ComposerRow::Attach, true)]
            );
            assert!(!runtime.composer_available(false));
        }
        assert_eq!(crate::upgrade::floor::index_reads_for_test(), reads);
        assert_eq!(runtime.snapshot.capabilities, capabilities);
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text(), "Keep this unsent");
        assert_eq!(runtime.session, session);
        assert!(runtime.commands.is_idle() && runtime.queued.is_none());
        assert!(app.chat_ui.attempt(card).is_none());
    }
}

#[test]
fn saved_history_readiness_requires_exact_settled_read_only_attachment() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
    let original = chat_runtime(&app, card);
    let mut source = original.session.clone();
    source.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let snapshot = Snapshot {
        session_id: source.launch.session_id.clone(),
        exited: true,
        read_only_reason: Some("Saved history; no provider was started".into()),
        ..Default::default()
    };
    for mutation in 0..6 {
        let mut observed = snapshot.clone();
        match mutation {
            1 => observed.history_loading = true,
            2 => observed.session_id = Some(uuid::Uuid::new_v4().to_string()),
            3 => observed.error = Some("History cannot be loaded".into()),
            4 => observed.busy = true,
            _ => {}
        }
        let handle = Arc::new(ClientHandle::fixture(source.clone(), observed));
        let mut runtime = Runtime::new(
            original.target,
            source.clone(),
            Connection::Live(handle),
            false,
            original.syntax_theme,
        );
        if mutation == 5 {
            runtime.disconnected = true;
        }
        let ready = runtime.restored_pane_ready();
        match mutation {
            0 => assert_eq!(ready, Ok(true)),
            1 => assert_eq!(ready, Ok(false)),
            _ => assert!(ready.is_err()),
        }
    }
}

#[test]
fn saved_history_admits_attachment_chooser_and_completion_without_starting_or_sending() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session.launch.session_id = Some(KNOWN_THREAD.into());
        runtime.session.native_continuity = chat::NativeContinuity::ReconnectOnly;
        let snapshot = Snapshot {
            session_id: runtime.session.launch.session_id.clone(),
            exited: true,
            read_only_reason: Some("Working directory no longer exists".into()),
            ..Default::default()
        };
        runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(
            runtime.session.clone(),
            snapshot.clone(),
        )));
        runtime.snapshot = Arc::new(snapshot);
        runtime.replace_draft(text_editor::Content::with_text("Keep this draft unsent"));
        assert!(!runtime.snapshot.capabilities.attachments);
        assert!(runtime.accepts_draft_attachments());

        // Exercise chooser admission and its normal callbacks. The native
        // fixture exercises the portal; this test supplies its chosen path
        // and the real asset-store receipt, without pre-seeding the draft.
        drop(app.handle_chat_ui(Action::Attach(target)));
        let runtime = chat_runtime(&app, card);
        let id = runtime
            .attachment_chooser_pending
            .expect("chooser was admitted");
        let ticket = runtime.attachment_ticket(id).unwrap();
        drop(app.handle_chat_ui(Action::Attached(
            target,
            ticket.clone(),
            Ok(Some(vec![image.path.clone()])),
        )));
        assert!(
            chat_runtime(&app, card)
                .attachment_chooser_pending
                .is_none()
        );
        assert_eq!(chat_runtime(&app, card).pastes_pending, 1);
        drop(app.handle_chat_ui(Action::ResumeSaved(target)));
        drop(app.handle_chat_ui(Action::Reconnect(target)));
        assert!(
            app.chat_ui.attempt(card).is_none(),
            "resume/reconnect cannot cancel an admitted attachment worker"
        );
        assert!(ticket.active());
        drop(app.handle_chat_ui(Action::AttachmentsPrepared(
            target,
            ticket,
            Ok(vec![image.clone()]),
        )));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.attachments, [image]);
        assert!(runtime.composer.text().contains("Keep this draft unsent"));
        assert!(runtime.composer.text().contains("[Image 1]"));
        assert_eq!(runtime.pastes_pending, 0);
        assert!(
            !runtime.snapshot.capabilities.attachments,
            "local draft editing cannot grant provider capabilities"
        );
        let draft = runtime.composer.text();
        let attachments = runtime.attachments.clone();
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        drop(app.chat_send(
            target,
            Command::Prompt {
                text: draft.clone(),
                attachments: attachments.clone(),
                mode: PromptMode::Send,
            },
            true,
        ));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text(), draft);
        assert_eq!(runtime.attachments, attachments);
        assert!(runtime.commands.is_idle() && runtime.queued.is_none());
        assert!(app.chat_ui.attempt(card).is_none());
    });
}

#[test]
fn saved_history_explicit_resume_keeps_the_pane_draft_and_image_unsent() {
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
        let mut source = chat_runtime(&app, card).session.clone();
        source.launch.session_id = Some(KNOWN_THREAD.into());
        source.native_continuity = chat::NativeContinuity::ReconnectOnly;
        app.replace_chat_conversation(card, source.clone());
        let snapshot = Snapshot {
            session_id: source.launch.session_id.clone(),
            exited: true,
            read_only_reason: Some("Saved history; no provider was started".into()),
            ..Default::default()
        };
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session = source.clone();
        runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(
            source.clone(),
            snapshot.clone(),
        )));
        runtime.snapshot = Arc::new(snapshot);
        runtime.replace_draft(text_editor::Content::with_text("Keep this draft unsent"));
        runtime.replace_attachments(vec![image.clone()]);
        runtime.composer.perform(text_editor::Action::Move(
            text_editor::Motion::DocumentStart,
        ));
        runtime
            .composer
            .perform(text_editor::Action::Select(text_editor::Motion::Right));
        let cursor = runtime.composer.cursor();
        let allocation = runtime as *const Runtime as usize;
        assert!(
            app.chat_ui.attempt(card).is_none(),
            "opening history does not start work"
        );
        assert!(
            chat_runtime(&app, card)
                .has_connected_saved_history(&chat_runtime(&app, card).snapshot)
        );

        drop(app.handle_chat_ui(Action::ResumeSaved(target)));
        let attempt = app.chat_ui.attempt(card).unwrap();
        assert_eq!(attempt.intent, chat::Intent::Start);
        assert_eq!(
            attempt.session, source,
            "use the exact authenticated reader descriptor"
        );
        let failed = attempt.target;
        drop(app.handle_chat_ui(Action::ResumeSaved(target)));
        assert_eq!(
            app.chat_ui.attempt(card).unwrap().target,
            failed,
            "one click owns one admission"
        );
        answer_start(
            &mut app,
            failed,
            Err("Named SDK initialization failed: exit 75".into()),
        );
        assert!(app.chat_ui.attempt(card).is_none());
        drop(app.handle_chat_ui(Action::Wake(
            target,
            Wake::Disconnected("Old history reader was replaced".into()),
        )));
        drop(app.handle_chat_ui(Action::Reconnect(target)));
        let retry = app.chat_ui.attempt(card).unwrap().target;
        assert_eq!(
            app.chat_ui.attempt(card).unwrap().intent,
            chat::Intent::RetryStart
        );
        assert_eq!(app.chat_ui.attempt(card).unwrap().session, source);
        let mut confirmed = source.clone();
        confirmed.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        answer_start(
            &mut app,
            retry,
            Ok(Arc::new(ClientHandle::fixture(
                confirmed,
                Snapshot {
                    session_id: source.launch.session_id.clone(),
                    ready: true,
                    native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
                    ..Default::default()
                },
            ))),
        );
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime as *const Runtime as usize, allocation);
        assert_eq!(runtime.session.id, source.id);
        assert_eq!(runtime.session.launch, source.launch);
        assert_eq!(runtime.composer.text(), "Keep this draft unsent");
        assert_eq!(runtime.composer.cursor(), cursor);
        assert_eq!(runtime.attachments, [image]);
        assert!(runtime.queued.is_none() && runtime.commands.is_idle());
        assert!(!runtime.disconnected);
        assert!(runtime.handle().unwrap().snapshot().ready);
    });
}

#[test]
fn saved_history_explicit_resume_refuses_unsettled_or_executing_attachments() {
    for mutation in 0..6 {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session.launch.session_id = Some(KNOWN_THREAD.into());
        runtime.session.native_continuity = chat::NativeContinuity::ReconnectOnly;
        let mut snapshot = Snapshot {
            session_id: runtime.session.launch.session_id.clone(),
            exited: true,
            read_only_reason: Some("Saved history; no provider was started".into()),
            ..Default::default()
        };
        match mutation {
            0 => snapshot.history_loading = true,
            1 => snapshot.session_id = Some("another-conversation".into()),
            2 => snapshot.busy = true,
            3 => snapshot.read_only_reason = None,
            4 => snapshot.exited = false,
            5 => {
                runtime.session.native_continuity =
                    chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
            }
            _ => unreachable!(),
        }
        runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(
            runtime.session.clone(),
            snapshot.clone(),
        )));
        runtime.snapshot = Arc::new(snapshot);
        drop(app.handle_chat_ui(Action::ResumeSaved(target)));
        assert!(
            app.chat_ui.attempt(card).is_none(),
            "mutation {mutation} must not start work"
        );
    }
}

#[test]
fn disconnected_saved_history_reconnects_without_borrowing_explicit_start_permission() {
    for stale_resume_click in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        let mut source = chat_runtime(&app, card).session.clone();
        source.launch.session_id = Some(KNOWN_THREAD.into());
        source.native_continuity = chat::NativeContinuity::ReconnectOnly;
        let mut previous = source.clone();
        previous.proof.as_mut().unwrap().host_incarnation = "previous-reader-host".into();
        let handle = Arc::new(
            ClientHandle::fixture_upgrade_handshake(
                previous,
                source.clone(),
                Snapshot {
                    session_id: source.launch.session_id.clone(),
                    exited: true,
                    read_only_reason: Some("Saved conversation; no provider was started".into()),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        // The scripted peer has actually closed its socket after a framed
        // handshake. Observe the real reader's EOF without consuming a UI
        // wake or assigning its alive/disconnected flags in the fixture.
        let deadline = Instant::now() + Duration::from_secs(2);
        while handle.is_connected() {
            assert!(
                Instant::now() < deadline,
                "framed peer EOF must reach the client"
            );
            std::thread::yield_now();
        }
        app.replace_chat_conversation(card, source.clone());
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session = source.clone();
        runtime.snapshot = handle.snapshot();
        runtime.connection = Connection::Live(handle);
        assert!(!runtime.disconnected, "the UI wake has not been consumed");
        assert!(runtime.is_saved_history_snapshot(&runtime.snapshot));
        assert!(
            !runtime.has_connected_saved_history(&runtime.snapshot),
            "show Reconnect rather than Resume"
        );
        assert!(!app.chat_ui.explicit_starts.contains_key(&card));
        drop(app.handle_chat_ui(if stale_resume_click {
            Action::ResumeSaved(target)
        } else {
            Action::Reconnect(target)
        }));
        let attempt = app.chat_ui.attempt(card).unwrap();
        assert_eq!(attempt.intent, chat::Intent::Restore);
        assert_eq!(attempt.session, source);
        assert!(!app.chat_ui.explicit_starts.contains_key(&card));
    }
}

#[test]
fn failed_retry_adapter_cannot_acknowledge_retained_native_cleanup() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        unreachable!()
    };
    let target = runtime.target;
    runtime.disconnected = true;
    let _ = app.update(Message::ChatUi(Action::Reconnect(target)));
    let retry = chat_runtime_attempt(&app, card);
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.exited = true;
    handle.replace_snapshot_for_test(snapshot);
    answer_start(&mut app, retry, Ok(handle));
    assert!(chat_runtime(&app, card).connected());
    assert!(chat_runtime(&app, card).handle().unwrap().snapshot().exited);
    let source = app.chat_return_session(card).unwrap();
    let _ = app.begin_chat_close(card);
    let close = app.chat_ui.phases[&card].target();
    assert!(app.card_state(card).unwrap().existence.is_open());
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        close,
        source.clone(),
        Err("holder cleanup remains unconfirmed".into()),
    )));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert_eq!(app.chat_return_session(card), Some(source.clone()));
    let _ = app.begin_chat_close(card);
    let close = app.chat_ui.phases[&card].target();
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        close,
        source,
        Ok(()),
    )));
    assert!(!app.card_state(card).unwrap().existence.is_open());
}

/// A Close the daemon refuses because a dead helper left processes
/// running stays on the pane: it names them, ends the proved ones and
/// closes again, and its notice leaves with the pane.
#[test]
fn a_refused_close_stays_on_its_pane_and_ends_what_holds_it() {
    use crate::terminal::ipc::{LostProcess, ProcessStart};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let source = app.chat_return_session(card).unwrap();
    let _ = app.begin_chat_close(card);
    let close = app.chat_ui.phases[&card].target();
    let reason = "Codex is still running for this conversation, but the Cagents helper that controlled it is gone, so the conversation was kept rather than leave Codex running unseen.";
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        close,
        source.clone(),
        Err(reason.into()),
    )));
    assert!(app.card_state(card).unwrap().existence.is_open());
    let runtime = chat_runtime(&app, card);
    let target = runtime.target;
    let refusal = runtime.close_refusal.as_ref().expect("shown on the pane");
    assert_eq!(refusal.reason(), reason);
    assert!(app.notices.has(NoticeKey::ChatClose(card)));

    let codex = LostProcess {
        process: ProcessStart {
            pid: 4045845,
            start: 9000,
        },
        name: "codex".into(),
        command: "codex app-server --listen unix:///tmp/cagents-codex-x/control.sock".into(),
        proved: true,
    };
    let stranger = LostProcess {
        process: ProcessStart {
            pid: 4100000,
            start: 9100,
        },
        name: "sleep".into(),
        command: "sleep 100".into(),
        proved: false,
    };
    let _ = app.update(Message::ChatUi(Action::CloseHolders(
        target,
        source.clone(),
        Ok(vec![codex.clone(), stranger.clone()]),
    )));
    let refusal = chat_runtime(&app, card).close_refusal.as_ref().unwrap();
    assert_eq!(refusal.processes(), [codex, stranger]);
    assert_eq!(refusal.end_action(), Some("End 1 process and close".into()));

    let _ = app.update(Message::ChatUi(Action::EndCloseHolders(target)));
    assert!(
        chat_runtime(&app, card)
            .close_refusal
            .as_ref()
            .unwrap()
            .ending()
    );
    // Once they are ended, it closes again, and that Close says anew
    // whatever it finds.
    let _ = app.update(Message::ChatUi(Action::CloseHoldersEnded(
        target,
        source.clone(),
        Ok(Vec::new()),
    )));
    assert!(chat_runtime(&app, card).close_refusal.is_none());
    let close = app.chat_ui.phases[&card].target();
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        close,
        source,
        Ok(()),
    )));
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(!app.notices.has(NoticeKey::ChatClose(card)));
}

#[test]
fn retained_claude_insert_restores_and_failed_disconnected_close_preserves_its_pane() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        directory.path().into(),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
    source.proof = Some(crate::model::HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    });
    source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    drop(app.insert_chat_card(source.clone(), None, None));
    let card = app.workspace.focused().unwrap();
    assert_eq!(
        app.chat_ui.attempt(card).unwrap().intent,
        chat::Intent::Restore
    );
    // Complete the normal Ready admission before disconnecting the pane.
    // This is a UI fixture; no provider or control task is executed.
    let target = chat_runtime_attempt(&app, card);
    let handle = Arc::new(ClientHandle::fixture(
        source.clone(),
        Snapshot {
            ready: true,
            session_id: source.launch.session_id.clone(),
            native_continuity: source.native_continuity.clone(),
            ..Snapshot::default()
        },
    ));
    answer_start(&mut app, target, Ok(handle));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("native pane")
    };
    runtime.disconnected = true;
    runtime.replace_draft(text_editor::Content::with_text("  keep this draft 🦀\n"));
    runtime
        .attachments
        .push(Attachment::file("/fixture/unsent-image.png"));
    let draft = runtime.composer.text();
    let attachments = runtime.attachments.clone();
    drop(app.begin_chat_close(card));
    assert!(app.chat_close_owned(card));
    assert!(app.card_state(card).unwrap().existence.is_open());
    let target = app.chat_ui.phases[&card].target();
    drop(app.handle_chat_ui(Action::RetainedCloseFinished(
        target,
        source.clone(),
        Err("exact cleanup was refused".into()),
    )));
    assert_eq!(app.chat_return_session(card), Some(source));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert_eq!(chat_runtime(&app, card).composer.text(), draft);
    assert_eq!(chat_runtime(&app, card).attachments, attachments);
}

/// A stranded retained card is rebound to its started successor only once
/// the store has it; until then, and when the store refuses it or the card
/// closes meanwhile, the card names its source and the successor is
/// closed rather than left running beside it.
#[test]
fn a_resumed_successor_replaces_its_card_only_after_the_store_commits_it() {
    for outcome in ["committed", "refused", "closed"] {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot-test"),
        )
        .unwrap();
        app.session = Some(writer);
        let mut source = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            directory.path().into(),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        source.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
        source.proof = Some(crate::model::HostSessionProof {
            host_session_id: "old-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "old-incarnation".into(),
        });
        source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        drop(app.insert_chat_card(source.clone(), None, None));
        let card = app.workspace.focused().unwrap();
        app.persist();
        let store = app.session.as_ref().unwrap().operation_store();
        let stored = |app: &Cagents| {
            app.session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(10))
                .unwrap();
            store.load_card(card, None).unwrap().unwrap()
        };
        assert_eq!(stored(&app).kind, CardKind::Chat(source.clone()));
        let mut successor = source.clone();
        successor.id = TerminalId::new();
        successor.proof = Some(crate::model::HostSessionProof {
            host_session_id: "fixture-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-incarnation".into(),
        });
        let handle = Arc::new(ClientHandle::fixture_resumed(
            source.clone(),
            successor.clone(),
            Snapshot {
                ready: true,
                session_id: successor.launch.session_id.clone(),
                native_continuity: successor.native_continuity.clone(),
                ..Snapshot::default()
            },
        ));
        if outcome == "refused" {
            // Another writer re-pointed the card first.
            let mut competing = source.clone();
            competing.id = TerminalId::new();
            competing.proof = successor.proof.clone();
            store.rebind_chat_resume(card, &source, &competing).unwrap();
        }
        let target = chat_runtime_attempt(&app, card);
        let task = app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        )));
        assert!(app.chat_start_pending(card), "the commit is awaited");
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone()),
            "the card names its source until the store has the successor"
        );
        if outcome == "closed" {
            close_chat(&mut app, card);
        }
        let reply = resume_commit_reply(task);
        let committed = matches!(
            &reply,
            Message::ChatUi(Action::ResumeCommitted(_, _, Ok(())))
        );
        assert_eq!(committed, outcome != "refused", "{outcome}");
        let _ = app.update(reply);
        match outcome {
            "committed" => {
                assert_eq!(
                    app.card_state(card).unwrap().kind,
                    CardKind::Chat(successor.clone())
                );
                assert_eq!(stored(&app).kind, CardKind::Chat(successor.clone()));
                assert!(app.chat_ui.retiring.iter().all(|t| t.card != card));
            }
            "refused" => {
                assert_eq!(
                    app.card_state(card).unwrap().kind,
                    CardKind::Chat(source.clone())
                );
                assert!(matches!(
                    app.cards.get(&card),
                    Some(CardRuntime::Failed(_)) | Some(CardRuntime::Chat(_))
                ));
                retired(&mut app, card);
            }
            _ => {
                assert!(!stored(&app).existence.is_open());
                assert!(!app.cards.contains_key(&card));
                retired(&mut app, card);
            }
        }
    }
}

/// A card whose provider stopped under the running daemon (#1068) offers
/// one labeled Resume. It starts a successor on the same conversation in
/// this card, keeping the transcript on screen and the unsent draft, and a
/// second press while it starts begins nothing more.
#[test]
fn resuming_a_stopped_chat_keeps_its_transcript_and_draft_and_starts_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let live = crate::model::HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    };
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        directory.path().into(),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
    source.proof = Some(live.clone());
    source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    // The card is in a channel shared through the daemon's workspace model.
    let channel = crate::model::ChannelId::new();
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.push(crate::model::ProjectChannel {
        id: channel,
        project_id: crate::model::ProjectId::new(),
        title: "evaluation".into(),
        title_source: Default::default(),
        ordinal: 1,
        shared: true,
        cards: vec![],
        focused: None,
    });
    durable.active_channel = Some(channel);
    drop(app.insert_chat_card(source.clone(), None, None));
    let card = app.workspace.focused().unwrap();
    app.persist();
    // The daemon's view of the channel already holds the card.
    let pane = |app: &Cagents, session: &chat::Session| {
        let mut pane =
            crate::workspace_model::views::Pane::from_card(app.card_state(card).unwrap(), None)
                .unwrap();
        pane.kind = CardKind::Chat(session.clone());
        pane
    };
    let scope = crate::workspace_model::views::Scope::Channel(channel);
    let mut view = crate::workspace_model::views::View::empty(channel);
    view.revision = 1;
    view.panes.push(pane(&app, &source));
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.views = vec![view.clone()];
    app.shared_views.initialize(scope, view.panes.clone());
    drop(app.reconcile_shared_views());
    let text = |id: &str, role, body: &str| {
        Arc::new(chat::Item {
            id: id.into(),
            role,
            content: chat::ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Default::default(),
            },
            started_at: None,
            at: None,
        })
    };
    let transcript = vec![
        text("prompt", chat::Role::User, "Run the full evaluation"),
        text(
            "partial",
            chat::Role::Assistant,
            "Starting the full evaluation run now",
        ),
    ];
    let stopped = Snapshot {
        ready: true,
        exited: true,
        session_id: source.launch.session_id.clone(),
        native_continuity: source.native_continuity.clone(),
        items: transcript.clone(),
        ..Snapshot::default()
    };
    let target = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        target,
        Ok(Arc::new(ClientHandle::fixture(source.clone(), stopped))),
    );
    let runtime = match app.cards.get_mut(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime,
        _ => panic!("the card holds a chat"),
    };
    // The reader is looking at what the ended actor left on screen.
    runtime.visible.store(true, Ordering::Relaxed);
    Arc::make_mut(&mut runtime.snapshot).items = transcript.clone();
    runtime.replace_draft(text_editor::Content::with_text("keep this draft"));
    runtime
        .attachments
        .push(Attachment::file("/fixture/unsent-image.png"));
    let attachments = runtime.attachments.clone();
    let target = runtime.target;
    assert!(runtime.resumes_in_place());
    let offer = runtime.stopped(false, false);
    assert_eq!(
        offer.primary,
        Some(("Resume conversation", stopped::Press::Reconnect, true)),
        "{offer:?}"
    );
    assert_eq!(offer.secondary, None);

    drop(app.handle_chat_ui(Action::Reconnect(target)));
    let attempt = app.chat_ui.attempt(card).expect("Resume starts");
    assert_eq!(attempt.intent, chat::Intent::Resume);
    assert_eq!(attempt.session, source);
    let resume = attempt.target;
    // A second press, by either control, while it starts begins nothing.
    drop(app.handle_chat_ui(Action::Reconnect(target)));
    drop(app.handle_chat_ui(Action::ResumeSaved(target)));
    assert_eq!(chat_runtime_attempt(&app, card), resume);
    let runtime = chat_runtime(&app, card);
    let offer = runtime.stopped(true, true);
    assert_eq!(
        offer.primary,
        Some(("Resuming…", stopped::Press::Reconnect, false))
    );
    assert_eq!(runtime.snapshot.items, transcript);
    assert_eq!(runtime.composer.text(), "keep this draft");

    // The daemon answers with the successor it started on the same
    // conversation, which reads that conversation back.
    let mut successor = source.clone();
    successor.id = TerminalId::new();
    let handle = Arc::new(ClientHandle::fixture_resumed(
        source.clone(),
        successor.clone(),
        Snapshot {
            ready: true,
            session_id: successor.launch.session_id.clone(),
            native_continuity: successor.native_continuity.clone(),
            items: transcript.clone(),
            ..Snapshot::default()
        },
    ));
    // The daemon publishes the successor's pane before the start replies.
    // The echo leaves the card, its runtime and its draft to the start.
    view.revision += 1;
    view.panes = vec![pane(&app, &successor)];
    app.workspace_model.views = vec![view];
    drop(app.reconcile_shared_views());
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(source.clone())
    );
    assert_eq!(chat_runtime_attempt(&app, card), resume);
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text(), "keep this draft");
    assert_eq!(runtime.attachments, attachments);
    // Nor does this window publish the ended conversation back over it.
    app.persist();
    app.publish_view_changes();
    assert!(
        !app.shared_views
            .pending_changes()
            .any(|(_, change)| matches!(
                change,
                crate::workspace_model::views::Change::Update { before, pane }
                    if before.kind == CardKind::Chat(successor.clone())
                        && pane.kind != before.kind
            )),
        "{:?}",
        app.shared_views.pending_changes().collect::<Vec<_>>()
    );
    let task = app.update(Message::ChatUi(Action::Ready(
        resume,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    )));
    let reply = resume_commit_reply(task);
    let _ = app.update(reply);
    assert!(!app.chat_start_pending(card));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(successor.clone()),
        "the same card now names the successor"
    );
    let runtime = chat_runtime(&app, card);
    // The card shows the successor at once, not the ended actor's state,
    // so it does not offer Resume again before the next publication.
    assert!(!runtime.snapshot.exited);
    assert!(!runtime.disconnected);
    assert!(!runtime.resumes_in_place());
    assert_eq!(runtime.composer.text(), "keep this draft");
    assert_eq!(runtime.attachments, attachments);
    assert!(runtime.commands.is_idle(), "the draft was not sent");
}

/// A resume that fails leaves the card stopped with its conversation and
/// draft, says so, and resumes again when pressed, rather than offering to
/// reconnect to the actor the failed resume may already have ended.
#[test]
fn a_failed_resume_offers_to_try_again_and_resumes_when_pressed() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        directory.path().into(),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
    source.proof = Some(crate::model::HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    });
    source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    drop(app.insert_chat_card(source.clone(), None, None));
    let card = app.workspace.focused().unwrap();
    app.persist();
    let target = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        target,
        Ok(Arc::new(ClientHandle::fixture(
            source.clone(),
            Snapshot {
                ready: true,
                exited: true,
                session_id: source.launch.session_id.clone(),
                native_continuity: source.native_continuity.clone(),
                ..Snapshot::default()
            },
        ))),
    );
    let runtime = match app.cards.get_mut(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime,
        _ => panic!("the card holds a chat"),
    };
    runtime.replace_draft(text_editor::Content::with_text("keep this draft"));
    let target = runtime.target;

    drop(app.handle_chat_ui(Action::Reconnect(target)));
    let resume = chat_runtime_attempt(&app, card);
    assert_eq!(
        app.chat_ui.attempt(card).unwrap().intent,
        chat::Intent::Resume
    );
    answer_start(
        &mut app,
        resume,
        Err(chat::StartFailure::retry(
            "fixture: Claude Code did not start",
        )),
    );
    assert!(!app.chat_start_pending(card));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text(), "keep this draft");
    assert!(runtime.resumes_in_place());
    let offer = runtime.stopped(false, false);
    assert_eq!(
        offer.guidance,
        "Claude Code did not start again. The conversation and your draft are kept."
    );
    assert_eq!(
        offer.primary,
        Some(("Try again", stopped::Press::Reconnect, true)),
        "{offer:?}"
    );
    assert_eq!(offer.secondary, None);

    let target = runtime.target;
    drop(app.handle_chat_ui(Action::Reconnect(target)));
    let attempt = app.chat_ui.attempt(card).expect("Try again resumes");
    assert_eq!(attempt.intent, chat::Intent::Resume);
    assert_eq!(attempt.session, source);
}

fn daemon_proof(daemon: &str) -> crate::model::HostSessionProof {
    crate::model::HostSessionProof {
        host_session_id: format!("{daemon}-host").into(),
        boot_id: format!("{daemon}-boot").into(),
        host_incarnation: format!("{daemon}-incarnation").into(),
    }
}

/// A saved chat last connected through `daemon`, whose provider queue was
/// saved there, reopened by the fixture daemon with its provider listing
/// `listed`.
fn reopen_with_saved_queue(
    daemon: &str,
    waiting: Vec<chat::QueuedMessage>,
    listed: Vec<chat::QueuedMessage>,
) -> (Cagents, CardId) {
    reopen_with(
        daemon,
        daemon,
        waiting,
        Snapshot {
            queued_messages: listed,
            ..Snapshot::default()
        },
    )
}

/// As [`reopen_with_saved_queue`], with the queue saved under
/// `queue_daemon` and the reopened provider showing `reopened`.
fn reopen_with(
    daemon: &str,
    queue_daemon: &str,
    waiting: Vec<chat::QueuedMessage>,
    reopened: Snapshot,
) -> (Cagents, CardId) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut session = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        PathBuf::from("/project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
    session.proof = Some(daemon_proof(daemon));
    drop(app.insert_chat_card(session.clone(), None, None));
    let card = app.workspace.focused().unwrap();
    app.chat_ui.drafts.insert(card, "typed after".into());
    app.chat_ui.waiting.insert(
        card,
        SavedQueue {
            host: Some(daemon_proof(queue_daemon)),
            messages: waiting,
            stopped: false,
        },
    );
    let target = chat_runtime_attempt(&app, card);
    // The fixture daemon is "fixture"; the saved session names `daemon`.
    session.proof = Some(daemon_proof("fixture"));
    answer_start(
        &mut app,
        target,
        Ok(Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                session_id: session.launch.session_id.clone(),
                ..reopened
            },
        ))),
    );
    (app, card)
}

fn saved_queue() -> Vec<chat::QueuedMessage> {
    let mut first = provider_message("queued-1", "Then run the tests");
    first.attachments = vec![Attachment::file("/project/notes.txt")];
    vec![first, provider_message("queued-2", "And summarize")]
}

/// Messages a provider held when its daemon went (#2208) come back to the
/// composer ahead of what was typed after them, files included, with a
/// notice that nothing was sent again. They are never sent from here.
#[test]
fn a_queue_the_daemon_lost_comes_back_to_the_draft_unsent() {
    let (mut app, card) = reopen_with_saved_queue("old", saved_queue(), vec![]);
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.composer.text().trim(),
        "Then run the tests\nAnd summarize\ntyped after"
    );
    assert_eq!(
        runtime.attachments,
        [Attachment::file("/project/notes.txt")]
    );
    assert_eq!(
        runtime.restored_waiting,
        Some(RestoredWaiting {
            count: 2,
            whole: true
        })
    );
    assert!(runtime.commands.is_idle());
    assert!(runtime.queued.is_none());
    assert!(runtime.followups.is_empty());
    assert!(!app.chat_ui.waiting.contains_key(&card));
    // The draft now holds them, so the saved queue no longer does.
    let saved = app.persistent_workspace();
    assert!(!saved.chat_waiting_messages.contains_key(&card));
    assert_eq!(
        saved.chat_drafts.get(&card).map(|draft| draft.trim()),
        Some("Then run the tests\nAnd summarize\ntyped after")
    );
    assert_eq!(
        saved.chat_draft_attachments.get(&card).unwrap(),
        &[Attachment::file("/project/notes.txt")]
    );
    // Dismissing the notice puts it away and keeps the draft.
    let target = chat_runtime(&app, card).target;
    drop(app.handle_chat_ui(Action::DismissRestoredWaiting(target)));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.restored_waiting, None);
    assert_eq!(
        runtime.composer.text().trim(),
        "Then run the tests\nAnd summarize\ntyped after"
    );
}

/// The notice describes the draft, so it goes when the draft does: sent,
/// as soon as it leaves the composer, or cleared away by hand.
#[test]
fn the_restored_notice_leaves_with_the_draft_it_describes() {
    let (mut app, card) = reopen_with_saved_queue("old", saved_queue(), vec![]);
    let target = chat_runtime(&app, card).target;
    drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
    assert!(chat_runtime(&app, card).commands.in_flight().is_some());
    assert_eq!(chat_runtime(&app, card).restored_waiting, None);
    app.chat_runtime_mut(target)
        .unwrap()
        .composer_action(text_editor::Action::Edit(text_editor::Edit::Insert('!')));
    settle(&mut app, card, Ok(()));
    assert_eq!(chat_runtime(&app, card).restored_waiting, None);
    assert_eq!(chat_runtime(&app, card).composer.text().trim(), "!");

    let (mut app, card) = reopen_with_saved_queue("old", saved_queue(), vec![]);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.composer_action(text_editor::Action::SelectAll);
    runtime.composer_action(text_editor::Action::Edit(text_editor::Edit::Backspace));
    // The file is still there, and so is what the notice says about it.
    assert!(runtime.restored_waiting.is_some());
    drop(app.handle_chat_ui(Action::RemoveAttachment(target, 0)));
    assert_eq!(chat_runtime(&app, card).restored_waiting, None);
}

/// The notice says what it can promise: nothing was resent, and the agent
/// may have the message anyway; and when only part came back, that too.
#[test]
fn the_restored_notice_says_nothing_was_resent_and_what_is_missing() {
    assert_eq!(
        RestoredWaiting {
            count: 1,
            whole: true
        }
        .caption(),
        "Back in your draft: a message that was still queued when this conversation stopped. It has not been resent; the agent may already have received it."
    );
    assert_eq!(
        RestoredWaiting {
            count: 3,
            whole: false
        }
        .caption(),
        "Back in your draft: 3 messages that were still queued when this conversation stopped. They have not been resent; the agent may already have received some of them. Only part of what was queued could be restored; check it before sending."
    );
    // An attachment listed without a file it can name is part missing.
    let mut message = provider_message("queued-1", "Look at this");
    message.attachments = vec![Attachment {
        path: PathBuf::new(),
        reference: None,
    }];
    let (app, card) = reopen_with_saved_queue("old", vec![message], vec![]);
    let runtime = chat_runtime(&app, card);
    assert!(runtime.attachments.is_empty());
    assert_eq!(
        runtime.restored_waiting,
        Some(RestoredWaiting {
            count: 1,
            whole: false
        })
    );
}

/// A message the reopened provider still lists is waiting there, and only
/// the rest comes back; one listed only in preview says so. A provider
/// whose ids only number places in its queue is matched by the message.
#[test]
fn a_queue_the_new_provider_still_lists_stays_with_the_provider() {
    for renumbered in [false, true] {
        let mut queue = saved_queue();
        queue[1].editable = false;
        let mut still = queue[0].clone();
        if renumbered {
            still.id = "queued-2".into();
        }
        let (app, card) = reopen_with_saved_queue("old", queue, vec![still]);
        let runtime = chat_runtime(&app, card);
        // Renumbered, the listed message carries the other saved
        // message's id, yet its words still claim the message it is: either
        // way only the message the provider no longer holds comes back.
        assert_eq!(
            runtime.composer.text().trim(),
            "And summarize\ntyped after",
            "renumbered: {renumbered}"
        );
        assert!(runtime.attachments.is_empty());
        assert_eq!(
            runtime.restored_waiting,
            Some(RestoredWaiting {
                count: 1,
                whole: false
            })
        );
    }
}

/// A message the reopened provider's history shows it was given arrived,
/// so only what has no trace there comes back.
#[test]
fn a_message_the_history_shows_delivered_is_not_restored() {
    let (app, card) = reopen_with(
        "old",
        "old",
        saved_queue(),
        Snapshot {
            items: vec![Arc::new(chat::Item {
                id: "queued-1:0".into(),
                role: chat::Role::User,
                at: None,
                started_at: None,
                content: ItemContent::Text {
                    text: "Then run the tests".into(),
                    thinking: false,
                    phase: chat::Phase::Final,
                },
            })],
            ..Snapshot::default()
        },
    );
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text().trim(), "And summarize\ntyped after");
    assert!(runtime.attachments.is_empty());
    assert_eq!(runtime.restored_waiting.map(|notice| notice.count), Some(1));
}

/// Each message the new provider lists stands for one saved message, so
/// a message queued twice comes back once when the provider still holds
/// one copy. A listed message's words outrank its id, which a provider
/// may have reused for another message, and its id alone still claims a
/// message it lists only in preview.
#[test]
fn each_listed_message_accounts_for_one_saved_message() {
    let message = |id: &str, text: &str| chat::QueuedMessage {
        id: id.into(),
        text: text.into(),
        editable: true,
        delivery: chat::QueueDelivery::default(),
        attachments: Vec::new(),
    };
    let left = |listed: Vec<chat::QueuedMessage>, saved: Vec<chat::QueuedMessage>| {
        let snapshot = Snapshot {
            queued_messages: listed,
            ..Snapshot::default()
        };
        unaccounted(&snapshot, saved)
            .into_iter()
            .map(|message| message.id)
            .collect::<Vec<_>>()
    };
    // "b" claims its own copy; "x" then stands for "a" by its words.
    assert_eq!(
        left(
            vec![message("b", "again"), message("x", "again")],
            vec![
                message("a", "again"),
                message("b", "again"),
                message("c", "again"),
            ],
        ),
        ["c"]
    );
    // A reused id does not hide the message whose words it now holds.
    assert_eq!(
        left(
            vec![message("1", "second")],
            vec![message("1", "first"), message("2", "second")],
        ),
        ["1"]
    );
    // A preview listed under the message's id still accounts for it.
    assert_eq!(
        left(
            vec![message("1", "long message, cut")],
            vec![message("1", "long message, cut short here")],
        ),
        Vec::<String>::new()
    );
}

/// A queue saved under an earlier daemon than the one the conversation
/// last connected through outlived a build that keeps none, which ran in
/// between; what it lists is long settled and does not come back.
#[test]
fn a_queue_an_earlier_daemon_saved_is_not_restored() {
    let (mut app, card) = reopen_with("old", "older", saved_queue(), Snapshot::default());
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text().trim(), "typed after");
    assert_eq!(runtime.restored_waiting, None);
    assert!(
        !app.persistent_workspace()
            .chat_waiting_messages
            .contains_key(&card)
    );
}

/// The daemon that held the queue still hosts the provider, which lists
/// what it holds now: what it no longer lists was delivered or removed.
#[test]
fn a_queue_saved_under_the_same_daemon_is_not_restored() {
    let (mut app, card) = reopen_with_saved_queue("fixture", saved_queue(), vec![]);
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text().trim(), "typed after");
    assert!(runtime.attachments.is_empty());
    assert_eq!(runtime.restored_waiting, None);
    assert!(!app.chat_ui.waiting.contains_key(&card));
    assert!(
        !app.persistent_workspace()
            .chat_waiting_messages
            .contains_key(&card)
    );
}

/// What the provider holds is saved as it changes, and what has gone from
/// its queue is no longer saved.
#[test]
fn a_provider_queue_is_saved_while_it_holds_messages() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = provider_queue(&mut app, saved_queue());
    // Saved with the daemon whose provider holds it.
    assert_eq!(
        app.persistent_workspace().chat_waiting_messages.get(&card),
        Some(&SavedQueue {
            host: Some(daemon_proof("fixture")),
            messages: saved_queue(),
            stopped: false,
        })
    );
    // The provider's next publication is saved when it arrives.
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.queued_messages.remove(0);
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    let saves = crate::app::terminal_output_tests::persists();
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    assert_eq!(crate::app::terminal_output_tests::persists(), saves + 1);
    assert_eq!(
        app.persistent_workspace().chat_waiting_messages[&card].messages,
        &saved_queue()[1..]
    );
    // A publication that leaves the queue as it was saves nothing more.
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    assert_eq!(crate::app::terminal_output_tests::persists(), saves + 1);
    provider_drops(&mut app, target, &handle, "queued-2", None);
    assert!(
        !app.persistent_workspace()
            .chat_waiting_messages
            .contains_key(&card)
    );
}

/// Held follow-ups are saved with the draft, in the order written, so a
/// restart cannot lose them either.
#[test]
fn held_followups_are_saved_with_the_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("expected chat");
    };
    runtime
        .followups
        .push(followup(0, "first task", &["/project/a.txt"]));
    runtime.followups.push(followup(1, "second task", &[]));
    runtime.replace_draft(text_editor::Content::with_text("still typing"));
    let (draft, attachments) = runtime.persistent_draft();
    assert_eq!(draft.trim(), "first task\nsecond task\nstill typing");
    assert_eq!(attachments, [Attachment::file("/project/a.txt")]);
}

/// Once the provider has taken held follow-ups they are saved without
/// them at once, so a restart cannot offer them to be sent again.
#[test]
fn sent_followups_leave_the_saved_draft_when_the_provider_takes_them() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("followup"));
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.followups.push(followup(0, "first task", &[]));
    runtime.replace_draft(text_editor::Content::with_text("still typing"));
    drop(app.dispatch_queued(target));
    assert!(
        chat_runtime(&app, card)
            .commands
            .in_flight()
            .unwrap()
            .followups
            .contains(&0)
    );
    // Until the provider takes it, the follow-up is saved with the draft.
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&card)
            .map(|draft| draft.trim()),
        Some("first task\nstill typing")
    );
    let saves = crate::app::terminal_output_tests::persists();
    settle(&mut app, card, Ok(()));
    assert!(crate::app::terminal_output_tests::persists() > saves);
    assert_eq!(
        app.persistent_workspace()
            .chat_drafts
            .get(&card)
            .map(|draft| draft.trim()),
        Some("still typing")
    );
}

/// A conversation parked for a terminal handoff refreshes no snapshot of
/// its own, yet what its provider holds is saved as it changes.
#[test]
fn a_parked_conversation_saves_its_provider_queue_as_it_changes() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = provider_queue(&mut app, saved_queue());
    let Some(CardRuntime::Chat(runtime)) = app.cards.remove(&card) else {
        panic!("chat runtime");
    };
    let mut runtime = runtime;
    runtime.parked_queue = saved_queue()
        .into_iter()
        .map(|message| message.id)
        .collect();
    app.chat_ui.parked.insert(card, runtime);
    assert_eq!(
        app.persistent_workspace().chat_waiting_messages[&card].messages,
        saved_queue()
    );
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.queued_messages.remove(0);
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    let saves = crate::app::terminal_output_tests::persists();
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    assert_eq!(crate::app::terminal_output_tests::persists(), saves + 1);
    assert_eq!(
        app.persistent_workspace().chat_waiting_messages[&card].messages,
        &saved_queue()[1..]
    );
    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
    assert_eq!(crate::app::terminal_output_tests::persists(), saves + 1);
}

/// An open conversation whose daemon went away, or whose provider ended,
/// gets back on reconnecting what that provider was holding; one that
/// reconnects to the provider that still holds it gets nothing back.
#[test]
fn an_open_conversation_reconnecting_gets_back_what_its_lost_provider_held() {
    for (daemon, ended, restored) in [
        ("old", false, true),
        ("fixture", true, true),
        ("fixture", false, false),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture_for_provider(&mut app, Provider::Claude);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.queued_messages = saved_queue();
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("typed after"));
        let mut snapshot = (*handle.snapshot()).clone();
        if ended {
            snapshot.exited = true;
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.refresh();
        runtime.disconnected = !ended;
        // The fixture daemon answers the reconnection; this conversation
        // last connected through `daemon`.
        runtime.session.proof = Some(daemon_proof(daemon));
        drop(app.handle_chat_ui(Action::Reconnect(target)));
        let attempt = chat_runtime_attempt(&app, card);
        let session = app.chat_ui.attempt(card).unwrap().session.clone();
        let session_id = session.launch.session_id.clone();
        answer_start(
            &mut app,
            attempt,
            Ok(Arc::new(ClientHandle::fixture(
                session,
                Snapshot {
                    ready: true,
                    session_id,
                    ..Snapshot::default()
                },
            ))),
        );
        let runtime = chat_runtime(&app, card);
        assert!(
            app.chat_ui.attempt(card).is_none()
                && runtime.session.proof == Some(daemon_proof("fixture")),
            "{daemon} ended={ended}: the new connection was not taken (attempt {:?}, proof {:?}, status {:?}, error {:?}, disconnected {}, live {})",
            app.chat_ui.attempt(card).map(|attempt| attempt.intent),
            runtime.session.proof,
            app.status,
            runtime.error,
            runtime.disconnected,
            runtime.handle().is_some(),
        );
        if restored {
            assert_eq!(
                runtime.composer.text().trim(),
                "Then run the tests\nAnd summarize\ntyped after",
                "{daemon} ended={ended}"
            );
            assert_eq!(
                runtime.attachments,
                [Attachment::file("/project/notes.txt")]
            );
            assert_eq!(runtime.restored_waiting.map(|notice| notice.count), Some(2));
        } else {
            assert_eq!(runtime.composer.text().trim(), "typed after");
            assert_eq!(runtime.restored_waiting, None);
        }
    }
}

/// Every state a stopped or disconnected card can be in names its action
/// on a labeled button, and none offers one that would only attach the
/// same stopped actor again.
#[test]
fn stopped_and_disconnected_cards_label_an_action_that_works() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut session = chat::fresh_session(
        Provider::Codex,
        TerminalId::new(),
        PathBuf::from("/project"),
        &crate::account::Account::system(Provider::Codex),
    )
    .unwrap();
    session.launch.session_id = Some("codex-thread".into());
    session.proof = Some(crate::model::HostSessionProof {
        host_session_id: "fixture-host".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "fixture-incarnation".into(),
    });
    drop(app.insert_chat_card(session.clone(), None, None));
    let card = app.workspace.focused().unwrap();
    let target = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        target,
        Ok(Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                exited: true,
                session_id: session.launch.session_id.clone(),
                ..Snapshot::default()
            },
        ))),
    );
    let runtime = match app.cards.get_mut(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime,
        _ => panic!("the card holds a chat"),
    };
    // An exited provider this card cannot start again says what to do.
    assert!(!runtime.resumes_in_place());
    let offer = runtime.stopped(false, false);
    assert_eq!(offer.primary, None, "{offer:?}");
    assert!(offer.guidance.contains("Resume conversation"), "{offer:?}");
    // A lost connection reconnects.
    runtime.disconnected = true;
    let offer = runtime.stopped(false, false);
    assert_eq!(
        offer.primary,
        Some(("Reconnect", stopped::Press::Reconnect, true))
    );
    // A resume that never loaded its conversation retries or starts anew.
    runtime.disconnected = false;
    Arc::make_mut(&mut runtime.snapshot).session_id = None;
    let offer = runtime.stopped(false, false);
    assert_eq!(
        offer.primary,
        Some(("Try again", stopped::Press::Reconnect, true))
    );
    assert_eq!(
        offer.secondary,
        Some(("Start a new conversation", stopped::Press::StartFresh, true))
    );
}

fn resume_commit_reply(task: Task<Message>) -> Message {
    use cosmic::iced::futures::StreamExt;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut stream = cosmic::iced::runtime::task::into_stream(task).unwrap();
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::ChatUi(Action::ResumeCommitted(..)),
                    )) = action
                    {
                        return message;
                    }
                }
                panic!("resume commit produced no acknowledgment");
            })
            .await
            .expect("session writer did not acknowledge the resume within 10 seconds")
        })
}

#[test]
fn retained_chat_close_and_channel_move_do_not_overtake_each_other() {
    use crate::{
        model::{ChannelId, ProjectChannel, ProjectId},
        workspace_model::{
            ModelChannel, ModelEdit,
            views::{Change, Pane, View},
        },
    };
    for close_first in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let saved = app.chat_return_session(card).unwrap();
        let repository = saved.launch.recipe.working_directory.clone();
        let source = ChannelId::new();
        let destination = ChannelId::new();
        let project = ProjectId::new();
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model
            .projects
            .push(crate::workspace_model::ModelProject {
                repository: repository.clone(),
                name: "Fixture".into(),
                kind: Default::default(),
                ordinal: 1,
            });
        app.workspace_model.channels = [source, destination]
            .into_iter()
            .map(|id| ModelChannel {
                id,
                repository: repository.clone(),
                title: "Chat moves".into(),
                title_source: Default::default(),
                ordinal: 0,
            })
            .collect();
        let mut view = View::empty(source);
        view.panes
            .push(Pane::from_card(app.card_state(card).unwrap(), None).unwrap());
        app.workspace_model.views = vec![view, View::empty(destination)];
        let durable = Arc::make_mut(&mut app.durable);
        durable.active_channel = Some(source);
        for id in [source, destination] {
            durable.channels.push(ProjectChannel {
                id,
                project_id: project,
                title: "Chat moves".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: true,
                cards: if id == source { vec![card] } else { vec![] },
                focused: (id == source).then_some(card),
            });
        }
        let (tx, rx) = test_model_channel();
        app.workspace_model_tx = tx;
        assert!(app.can_move_pane_to_channel(card, destination));
        if close_first {
            drop(app.begin_chat_close(card));
            let target = app.chat_ui.phases[&card].target();
            assert!(app.chat_close_owned(card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.pane_move_destinations(card).is_empty());
            drop(app.move_pane_to_channel(card, destination));
            assert!(rx.try_recv().is_err());
            assert!(!app.pane_transfer_pending(card));
            // An exact failed cleanup releases that close intent without
            // deleting or replacing the native descriptor.
            drop(app.update(Message::ChatUi(Action::RetainedCloseFinished(
                target,
                saved.clone(),
                Err("fixture retains native owner".into()),
            ))));
            assert!(!app.chat_close_owned(card));
            assert_eq!(app.chat_return_session(card), Some(saved));
            assert!(app.can_move_pane_to_channel(card, destination));
        } else {
            drop(app.move_pane_to_channel(card, destination));
            assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
                change: Change::Transfer { id, .. }, ..
            } if id == card));
            drop(app.begin_chat_close(card));
            assert!(!app.chat_close_owned(card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert_eq!(app.chat_return_session(card), Some(saved));
            assert!(app.pane_transfer_pending(card));
            assert!(rx.try_recv().is_err());
        }
    }
}

fn share_close_source(app: &mut Cagents, card: CardId) -> crate::workspace_model::views::Pane {
    use crate::workspace_model::{
        ModelProject,
        views::{Pane, Scope, View},
    };
    let repository = match &app.card_state(card).unwrap().kind {
        CardKind::Chat(session) => session.launch.recipe.working_directory.clone(),
        CardKind::Terminal(session) => session.launch.working_directory.clone(),
        _ => unreachable!(),
    };
    let scope = Scope::Worktree {
        repository: repository.clone(),
        path: repository.clone(),
    };
    app.workspace_model.identity = uuid::Uuid::new_v4();
    app.workspace_model.projects = vec![ModelProject {
        repository,
        name: "Fixture".into(),
        kind: Default::default(),
        ordinal: 1,
    }];
    let original = Pane::from_card(
        app.card_state(card).unwrap(),
        app.durable.card_scopes.get(&card).cloned(),
    )
    .unwrap();
    app.workspace_model.views = vec![View {
        panes: vec![original.clone()],
        ..View::empty(scope)
    }];
    original
}

#[test]
fn retained_shared_chat_close_never_recaptures_a_replacement_after_cleanup() {
    use crate::workspace_model::{ModelEdit, views::Change};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let saved = app.chat_return_session(card).unwrap();
    let original = share_close_source(&mut app, card);
    let (tx, published) = async_channel::unbounded();
    app.workspace_model_tx = tx;
    drop(app.begin_chat_close(card));
    let target = app.chat_ui.phases[&card].target();
    let mut replacement = original.clone();
    if let CardKind::Chat(session) = &mut replacement.kind {
        session.id = TerminalId::new();
    }
    app.workspace_model.views[0].panes[0] = replacement.clone();
    app.workspace_model.views[0].revision += 1;
    drop(app.handle_chat_ui(Action::RetainedCloseFinished(target, saved, Ok(()))));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert_eq!(app.workspace_model.views[0].panes[0], replacement);
    assert!(
        !std::iter::from_fn(|| published.try_recv().ok()).any(|edit| matches!(
            edit.edit,
            ModelEdit::ChangeView {
                change: Change::Remove { .. } | Change::RemoveSource { .. },
                ..
            }
        ))
    );
}

#[test]
fn retained_shared_chat_close_parked_continuation_cannot_close_a_replacement() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let Some(CardRuntime::Chat(runtime)) = app.cards.remove(&card) else {
        panic!("chat runtime");
    };
    let target = runtime.target;
    let proof = runtime.session.proof.clone().unwrap();
    app.chat_ui.parked.insert(card, runtime);
    let CardKind::Terminal(mut terminal) = crate::app::tests::terminal_card_state().kind else {
        unreachable!()
    };
    terminal.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id,
        boot_id: proof.boot_id,
        host_incarnation: proof.host_incarnation,
    };
    app.card_state_mut(card).unwrap().kind = CardKind::Terminal(terminal);
    app.cards.insert(card, CardRuntime::Completed);
    let original = share_close_source(&mut app, card);
    drop(app.begin_chat_close(card));
    assert!(app.chat_close_owned(card));
    let mut replacement = original.clone();
    if let CardKind::Terminal(session) = &mut replacement.kind {
        session.id = TerminalId::new();
    }
    app.workspace_model.views[0].panes[0] = replacement.clone();
    app.card_state_mut(card).unwrap().kind = replacement.kind;
    drop(app.handle_chat_ui(Action::CloseFinished(target, Ok(()))));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(app.terminal_close.is_none());
    assert!(app.completed_release.is_none());
}

#[test]
fn retained_shared_chat_close_queued_continuation_cannot_recapture_a_replacement() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let saved = app.chat_return_session(card).unwrap();
    let original = share_close_source(&mut app, card);
    let other = saved_terminal_pane(&mut app);
    app.workspace.request_close(other).unwrap();
    drop(app.begin_chat_close(card));
    let target = app.chat_ui.phases[&card].target();
    drop(app.handle_chat_ui(Action::RetainedCloseFinished(target, saved, Ok(()))));
    assert!(app.terminal_close_in_progress(card));
    let mut replacement = original.clone();
    if let CardKind::Chat(session) = &mut replacement.kind {
        session.id = TerminalId::new();
    }
    app.workspace_model.views[0].panes[0] = replacement.clone();
    app.card_state_mut(card).unwrap().kind = replacement.kind;
    app.workspace.cancel_close(other).unwrap();
    drop(app.next_queued_card_close());
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(!app.chat_ui.phases.contains_key(&card));
    assert!(app.workspace.pending_close().is_none());
}

#[test]
fn retained_shared_chat_close_queue_preserves_unasked_consent() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.waiting = true;
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    share_close_source(&mut app, card);
    assert!(app.chat_working_session(card).is_some());
    assert!(app.capture_shared_removal(card));
    app.queue_card_close(card, true);
    drop(app.next_queued_card_close());
    assert!(!app.chat_close_dialog_visible());
    assert!(
        app.workspace.pending_close().is_some()
            || !app.card_state(card).unwrap().existence.is_open()
    );
}

#[test]
fn failed_retained_native_close_keeps_saved_identity_until_exact_acknowledgment() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let saved = app.chat_return_session(card).unwrap();
    let _ = app.begin_chat_close(card);
    let target = app.chat_ui.phases[&card].target();
    let _ = app.begin_chat_close(card); // A repeated click cannot discard the pending identity.
    assert!(app.card_state(card).unwrap().existence.is_open());
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        target,
        saved.clone(),
        Err("cleanup remains unconfirmed".into()),
    )));
    assert_eq!(app.chat_return_session(card).as_ref(), Some(&saved));
    assert!(app.card_state(card).unwrap().existence.is_open());
    let _ = app.begin_chat_close(card);
    let retry = app.chat_ui.phases[&card].target();
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        target,
        saved.clone(),
        Ok(()),
    )));
    assert!(
        app.card_state(card).unwrap().existence.is_open(),
        "stale acknowledgment cannot complete a retry"
    );
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        retry,
        saved,
        Ok(()),
    )));
    assert!(!app.card_state(card).unwrap().existence.is_open());
}

/// The reason recorded beneath `card`'s failure to open its conversation.
fn attach_failure_details(app: &Cagents, card: CardId) -> Option<String> {
    app.notices
        .recent()
        .find(|entry| entry.notice.key == Some(crate::app::NoticeKey::ChatAttach(card)))
        .and_then(|entry| entry.notice.details.clone())
}

/// The errors the shell has recorded. `status` is drained into the notice
/// log at the end of every update, so this is where a test reads it.
fn errors(app: &Cagents) -> Vec<String> {
    app.notices
        .entries
        .iter()
        .filter(|entry| matches!(entry.notice.severity, crate::app::NoticeSeverity::Error))
        .map(|entry| entry.notice.message.clone())
        .collect()
}

fn retired(app: &mut Cagents, card: CardId) -> Target {
    let targets = app
        .chat_ui
        .retiring
        .iter()
        .copied()
        .filter(|target| target.card == card)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "one retirement per conversation");
    targets[0]
}

/// Closing the card was a silent no-op while the conversation connected,
/// so a slow or hung start held the card hostage.
#[test]
fn a_chat_still_connecting_closes_at_once_and_retires_what_its_start_produced() {
    for started in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        close_chat(&mut app, card);
        assert!(
            app.chat_ui.abandoned.contains_key(&target),
            "the start is still answered for"
        );
        assert!(app.chat_ui.retiring.is_empty(), "nothing is certain yet");
        assert!(app.chat_ui.transition_pending());

        let result = if started {
            Ok(handle)
        } else {
            Err(chat::StartFailure::retry("fixture: executable unavailable"))
        };
        answer_start(&mut app, target, result);
        assert!(app.chat_ui.abandoned.is_empty());
        assert!(
            !app.cards.contains_key(&card),
            "a late answer never brings the card back"
        );
        let retirement = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
        assert!(!app.chat_ui.transition_pending());
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
    }
}

/// The failed card's own Close button did nothing.
#[test]
fn a_chat_whose_start_failed_closes() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let saved = app.chat_return_session(card).unwrap();
    let _ = app.update(Message::CloseCard(card));
    let target = app.chat_ui.phases[&card].target();
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        target,
        saved.clone(),
        Err("fixture: daemon unreachable".into()),
    )));
    assert_eq!(app.chat_return_session(card), Some(saved));
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(
        !errors(&app).is_empty(),
        "a conversation that may still run is reported"
    );
    assert!(!app.chat_ui.transition_pending());
    app.notices.entries.clear();
    close_chat(&mut app, card);
    assert!(
        app.chat_ui.retiring.is_empty(),
        "the acknowledged close already proved retirement"
    );
}

/// A successful close committed a removal the workspace had never been
/// asked for, which failed with "no card removal is pending" and left a
/// tombstoned card on the strip, spinning, with no way to close it.
#[test]
fn a_live_chat_closes_without_leaving_its_card_behind() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("thread"));
    close_chat(&mut app, card);
    assert!(
        app.chat_ui.retiring.is_empty(),
        "the close receipt already retired the owner"
    );
}

/// What a conversation's provider reports about its account reaches the
/// account it runs on, whether it raced the attachment or woke the
/// surface after it.
#[test]
fn a_conversations_account_report_reaches_its_account() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, Some("thread"));
    let account = crate::account::Account::system(Provider::Codex).id();
    let report = |used: u32| {
        crate::account::LiveReport::CodexLimits(
            crate::account::codex::parse_update(&serde_json::json!({"rateLimits": {
                "primary": {"usedPercent": used, "windowDurationMins": 300},
            }}))
            .unwrap(),
        )
    };
    let used = |app: &Cagents| {
        app.account_ui
            .snapshot_for_test(&account)
            .and_then(|snapshot| snapshot.windows.first())
            .map(|window| window.used)
    };
    let now = crate::chat::now_ms().unwrap();
    // Live figures refine a reading the account already has.
    let (owner, ticket) = app.account_ui.begin_probe_for_test(
        &account,
        crate::account::schedule::Trigger::Focus,
        now,
    );
    let _ = app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
        owner,
        ticket,
        id: account.clone(),
        outcome: crate::account::probe::Outcome::Answered {
            catalog: None,
            identity: None,
            snapshot: Box::new(crate::account::UsageSnapshot::state(
                crate::account::UsageStatus::Ok,
                "",
                now - 1,
            )),
        },
    }));
    assert!(handle.push_account_usage_for_test(report(20), now));
    // The wake it sent lands while the start is still pending, which
    // leaves the report to the attachment.
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::AccountUsage)));
    assert_eq!(used(&app), None);
    answer_start(&mut app, target, Ok(handle.clone()));
    assert_eq!(used(&app), Some(0.2));
    assert!(
        handle.push_account_usage_for_test(report(60), now + 1),
        "the queue was drained, so the next report wakes the surface"
    );
    assert!(!handle.push_account_usage_for_test(report(70), now + 2));
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::AccountUsage)));
    assert_eq!(used(&app), Some(0.7));
    assert!(handle.take_account_usage().is_empty());

    // A wake that arrives while the conversation is being reconnected is
    // not read, but its reports do not stay queued to silence the next.
    let _ = app.update(Message::ChatUi(Action::Reconnect(target)));
    assert!(app.chat_start_pending(card));
    assert!(handle.push_account_usage_for_test(report(90), now + 3));
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::AccountUsage)));
    assert_eq!(used(&app), Some(0.7));
    assert!(
        handle.push_account_usage_for_test(report(95), now + 4),
        "the next report wakes the surface again"
    );
}

#[test]
fn a_new_chat_limit_without_usage_frames_forces_one_read_with_usage_views_closed() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut registry = crate::account::registry::Registry::default();
    for provider in crate::account::Account::PROVIDERS {
        registry.set_default(&crate::account::Account::system(provider));
    }
    app.account_ui.set_registry(registry);
    app.refresh_ui.focused = true;
    app.settings.show_status_bar = false;
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::Resized(1_600.0),
    }));
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    let runtime = chat_runtime(&app, card);
    assert!(
        runtime.presents(),
        "fixture chat must follow the visible transcript path"
    );
    assert_eq!(
        app.chat_account(
            runtime.session.launch.provider,
            &runtime.session.launch.recipe
        ),
        Some(crate::account::Account::system(Provider::Codex).id())
    );
    let target = runtime.target;
    let handle = runtime.handle().unwrap().clone();
    let id = crate::account::Account::system(Provider::Codex).id();
    let now = crate::chat::now_ms().unwrap();
    let (owner, ticket) =
        app.account_ui
            .begin_probe_for_test(&id, crate::account::schedule::Trigger::Tick, now);
    drop(
        app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
            id: id.clone(),
            owner,
            ticket,
            outcome: crate::account::probe::Outcome::Answered {
                identity: None,
                catalog: None,
                snapshot: Box::new(crate::account::UsageSnapshot::state(
                    crate::account::UsageStatus::Ok,
                    "cached",
                    now,
                )),
            },
        })),
    );
    assert!(app.utility_card(&CardKind::Accounts).is_none());
    let mut next = (*handle.snapshot()).clone();
    next.generation += 1;
    next.error = Some("Usage limit reached for this plan".into());
    handle.publish_snapshot_for_test(next);
    drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
    assert!(app.refresh_ui.focused);
    assert!(
        chat_runtime(&app, card)
            .failure_ui
            .incidents
            .iter()
            .any(|incident| incident.kind == Some(chat::failure::FailureKind::AccountLimit)),
        "fixture must classify the current provider rejection"
    );
    let first = app
        .account_ui
        .pending_probe_for_test(&id)
        .expect("a visible chat limit needs fresh account inventory");
    assert!(app.account_ui.pending_probe_is_forced_for_test(&id));
    // Repainting, account-limit signaling and identical transcript wakes
    // are all the same classified occurrence, not another network read.
    for wake in [Wake::Changed, Wake::Limit, Wake::Changed] {
        drop(app.update(Message::ChatUi(Action::Wake(target, wake))));
        assert!(
            app.account_ui
                .pending_probe_for_test(&id)
                .unwrap()
                .same(&first)
        );
    }
    drop(
        app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
            id: id.clone(),
            owner,
            ticket: first,
            outcome: crate::account::probe::Outcome::Answered {
                identity: None,
                catalog: None,
                snapshot: Box::new(crate::account::UsageSnapshot::state(
                    crate::account::UsageStatus::Limited,
                    "fresh",
                    now + 1,
                )),
            },
        })),
    );
    assert!(
        app.account_ui.pending_probe_for_test(&id).is_none(),
        "duplicate wakes left no queued read"
    );
    drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
    assert!(app.account_ui.pending_probe_for_test(&id).is_none());
}

#[test]
fn an_active_limit_before_initial_attachment_refreshes_inventory_without_a_changed_wake() {
    for active in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.account_ui
            .set_registry(crate::account::registry::Registry::default());
        app.refresh_ui.focused = true;
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(1_600.0),
        }));
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, Some("fixture-thread"));
        let mut snapshot = (*handle.snapshot()).clone();
        let raw = "Usage limit reached for this plan";
        let mut old_failure = chat::Item {
            id: "historical-request-error".into(),
            role: chat::Role::System,
            at: None,
            started_at: None,
            content: text(raw),
        };
        assert!(chat::failure::mark_provider_error(
            Provider::Codex,
            &mut old_failure
        ));
        snapshot.items.push(Arc::new(old_failure));
        snapshot.error = active.then(|| raw.into());
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        assert_eq!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        answer_start(&mut app, target, Ok(handle));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.presents());
        assert!(
            runtime
                .failure_ui
                .incidents
                .iter()
                .any(|incident| incident.kind == Some(chat::failure::FailureKind::AccountLimit))
        );
        assert_eq!(runtime.failure_ui.active_account_limit(), active);
        assert!(app.refresh_ui.focused);
        let id = crate::account::Account::system(Provider::Codex).id();
        assert_eq!(
            app.chat_account(
                runtime.session.launch.provider,
                &runtime.session.launch.recipe
            ),
            Some(id.clone())
        );
        assert_eq!(
            app.account_ui.pending_probe_is_forced_for_test(&id),
            active,
            "a current failed request needs inventory; reopening a historical notice alone does not"
        );
        assert!(!chat_runtime(&app, card).limit_refresh_pending);
    }
}

#[test]
fn an_unresolved_limit_account_waits_for_its_actual_home_instead_of_guessing_a_default() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.account_ui
        .set_registry(crate::account::registry::Registry::default());
    app.refresh_ui.focused = true;
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::Resized(1_600.0),
    }));
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let key = uuid::Uuid::from_u128(818191);
    let home =
        crate::account::home::Homes::at("/cagents-fixture/accounts").path(Provider::Codex, key);
    if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
        runtime
            .session
            .launch
            .recipe
            .environment
            .insert("CODEX_HOME".into(), home.display().to_string());
        runtime.limit_refresh_pending = true;
    }
    drop(app.refresh_chat_limit_accounts());
    assert!(chat_runtime(&app, card).limit_refresh_pending);
    assert!(
        app.account_ui
            .pending_probe_for_test(&crate::account::Account::system(Provider::Codex).id())
            .is_none()
    );
    let actual = app
        .account_ui
        .add_managed_fixture(Provider::Codex, key.as_u128());
    drop(app.refresh_chat_limit_accounts());
    assert!(!chat_runtime(&app, card).limit_refresh_pending);
    assert!(app.account_ui.pending_probe_is_forced_for_test(&actual));
    assert!(
        app.account_ui
            .pending_probe_for_test(&crate::account::Account::system(Provider::Codex).id())
            .is_none()
    );
}

#[test]
fn revealing_a_cold_disconnected_current_limit_refreshes_once_without_another_client_wake() {
    for active in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut registry = crate::account::registry::Registry::default();
        for provider in crate::account::Account::PROVIDERS {
            registry.set_default(&crate::account::Account::system(provider));
        }
        app.account_ui.set_registry(registry);
        app.refresh_ui.focused = true;
        app.settings.show_status_bar = false;
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(1_600.0),
        }));
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        let handle = runtime.handle().unwrap().clone();
        let raw = "Usage limit reached for this plan";
        if !active {
            runtime.note(raw.into());
            assert!(
                !runtime.failure_ui.active_account_limit(),
                "a retained note is historical"
            );
        }
        runtime.set_visible(false).unwrap();
        runtime.cool();
        assert!(!runtime.presents());
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.error = active.then(|| raw.into());
        handle.publish_snapshot_for_test(snapshot);
        drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
        let id = crate::account::Account::system(Provider::Codex).id();
        assert!(
            app.account_ui.pending_probe_for_test(&id).is_none(),
            "hidden metadata never starts a usage read"
        );
        handle.disconnect_for_test();
        app.set_workspace_surfaces_visible(true);
        assert!(chat_runtime(&app, card).is_visible());
        // This is a normal shell update after visibility changed. There
        // is no Changed/hydration client wake after the reveal.
        drop(app.update(Message::AccountUi(
            crate::app::account_ui::Action::ManualReady,
        )));
        assert_eq!(app.account_ui.pending_probe_is_forced_for_test(&id), active);
        assert_eq!(
            chat_runtime(&app, card).failure_ui.active_account_limit(),
            active
        );
        if active {
            let owner = app.account_ui.probe_owner(&id).unwrap();
            let ticket = app.account_ui.pending_probe_for_test(&id).unwrap();
            drop(
                app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
                    id: id.clone(),
                    owner,
                    ticket,
                    outcome: crate::account::probe::Outcome::Answered {
                        identity: None,
                        catalog: None,
                        snapshot: Box::new(crate::account::UsageSnapshot::state(
                            crate::account::UsageStatus::Limited,
                            "fresh",
                            crate::chat::now_ms().unwrap(),
                        )),
                    },
                })),
            );
        }
        let observed = chat_runtime(&app, card).failure_ui.limit_occurrence;
        app.set_workspace_surfaces_visible(false);
        app.chat_runtime_mut(target).unwrap().cool();
        app.set_workspace_surfaces_visible(true);
        drop(app.update(Message::AccountUi(
            crate::app::account_ui::Action::ManualReady,
        )));
        assert!(
            app.account_ui.pending_probe_for_test(&id).is_none(),
            "showing the same episode again does not spend another read"
        );
        assert!(!chat_runtime(&app, card).limit_refresh_pending);
        assert_eq!(
            chat_runtime(&app, card).failure_ui.limit_occurrence,
            observed
        );
    }
}

#[test]
fn first_cold_limit_hydration_reads_current_rejections_but_not_unseen_historical_items() {
    for active in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut registry = crate::account::registry::Registry::default();
        for provider in crate::account::Account::PROVIDERS {
            registry.set_default(&crate::account::Account::system(provider));
        }
        app.account_ui.set_registry(registry);
        app.refresh_ui.focused = true;
        app.settings.show_status_bar = false;
        // No viewport yet: this is a newly attached, initially cold chat
        // whose historical provider errors have never been classified.
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let runtime = chat_runtime(&app, card);
        let target = runtime.target;
        let handle = runtime.handle().unwrap().clone();
        assert!(!runtime.is_visible());
        assert!(runtime.presentation_evicted);
        assert!(runtime.failure_ui.incidents.is_empty());
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(1_600.0),
        }));
        assert!(chat_runtime(&app, card).is_visible());
        assert!(
            chat_runtime(&app, card).presentation_evicted,
            "fixture waits for the first resident hydration"
        );
        let raw = "Usage limit reached for this plan";
        let mut old_failure = chat::Item {
            id: "old-historical-provider-error".into(),
            role: chat::Role::System,
            at: None,
            started_at: None,
            content: text(raw),
        };
        assert!(chat::failure::mark_provider_error(
            Provider::Codex,
            &mut old_failure
        ));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.items.push(Arc::new(old_failure));
        snapshot.error = active.then(|| raw.into());
        handle.deliver_snapshot_for_test(snapshot);
        drop(app.update(Message::ChatUi(Action::Wake(target, Wake::Changed))));
        assert!(
            chat_runtime(&app, card)
                .failure_ui
                .incidents
                .iter()
                .any(|incident| incident.kind == Some(chat::failure::FailureKind::AccountLimit))
        );
        let id = crate::account::Account::system(Provider::Codex).id();
        assert_eq!(
            app.account_ui.pending_probe_is_forced_for_test(&id),
            active,
            "only the current live rejection warrants a reading on first hydration"
        );
        assert!(!chat_runtime(&app, card).limit_refresh_pending);
    }
}

/// A disconnected conversation's close request failed before it was sent,
/// and the failure only set a status: the card stayed forever.
#[test]
fn a_disconnected_chat_closes() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("thread"));
    let target = match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime.target,
        _ => unreachable!(),
    };
    let _ = app.update(Message::ChatUi(Action::Wake(
        target,
        Wake::Disconnected("fixture: daemon went away".into()),
    )));
    app.notices.entries.clear();
    close_chat(&mut app, card);
    assert!(
        app.chat_ui.retiring.is_empty(),
        "the acknowledged close needs no second retirement"
    );
}

#[test]
fn closing_a_chat_twice_is_one_close() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, None);
    let _ = app.update(Message::CloseCard(card));
    let closing = app.chat_ui.phases[&card].target();
    let _ = app.update(Message::CloseCard(card));
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
    assert_eq!(app.chat_ui.phases[&card].target(), closing);
    let _ = app.update(Message::CommitClose(card));
    assert!(
        app.workspace.order().any(|id| id == card),
        "an unacknowledged owner keeps its card"
    );
    close_chat(&mut app, card);
    assert!(app.chat_ui.retiring.is_empty());
}

/// A new conversation starts on its provider's default account, and a
/// draft moves to another account without starting anything; the
/// account's home is in the recipe its first message starts, which every
/// later resume keeps.
#[test]
fn a_new_chat_runs_on_the_chosen_account_through_its_recipe() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let managed = app.account_ui.add_managed_fixture(Provider::Codex, 21);
    let home = app
        .known_account(&managed)
        .and_then(|account| account.resolved_home())
        .unwrap()
        .display()
        .to_string();

    let card = draft_chat(&mut app);
    let dropped = Attachment {
        path: PathBuf::from("/tmp/dropped.png"),
        reference: None,
    };
    let target = match app.cards.get_mut(&card) {
        Some(CardRuntime::Chat(runtime)) => {
            runtime.attachments.push(dropped.clone());
            runtime.target
        }
        _ => unreachable!(),
    };
    let _ = app.update(Message::ChatUi(Action::ChooseAccount(
        target,
        managed.clone(),
    )));
    assert!(!app.chat_start_pending(card), "a choice starts nothing");
    // The composer outlives the move: what the reader attached is still
    // there on the new account.
    let runtime = chat_runtime(&app, card);
    assert!(runtime.is_draft());
    assert_eq!(runtime.attachments, [dropped]);
    assert_eq!(
        runtime.session.launch.recipe.environment.get("CODEX_HOME"),
        Some(&home)
    );
    write(&mut app, card, "hello");
    submit(&mut app, card);
    let attempt = app.chat_ui.attempt(card).unwrap();
    assert_eq!(
        attempt.session.launch.recipe.environment.get("CODEX_HOME"),
        Some(&home)
    );
    assert_eq!(
        attempt.session.launch.recipe.arguments,
        ["--remote", "unix://"]
    );

    let _ = app.update(Message::AccountUi(
        crate::app::account_ui::Action::SetDefault(managed),
    ));
    let _ = app.new_native_chat(Provider::Codex, Some(PathBuf::from(FIXTURE_DIRECTORY)));
    let fresh = app.workspace.focused().unwrap();
    assert_eq!(
        chat_runtime(&app, fresh)
            .session
            .launch
            .recipe
            .environment
            .get("CODEX_HOME"),
        Some(&home),
        "a new conversation starts on the default account"
    );
}

/// Makes the focused fixture card a stopped agent terminal running
/// `arguments`, and returns it.
fn stopped_agent_terminal(app: &mut Cagents, arguments: &[&str], exited: bool) -> CardId {
    let card = app.workspace.focused().unwrap();
    let launch = LaunchRecipe {
        unset_environment: Default::default(),
        program: "claude".into(),
        arguments: arguments.iter().map(|&argument| argument.into()).collect(),
        working_directory: PathBuf::from(FIXTURE_DIRECTORY),
        environment: Default::default(),
    };
    let mut state = app.card_state(card).unwrap().clone();
    let CardKind::Terminal(terminal) = &mut state.kind else {
        unreachable!()
    };
    terminal.launch = launch.clone();
    terminal.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
        recipe: launch,
        last_exit: exited.then_some(crate::model::TerminalExit {
            code: Some(0),
            signal: None,
            occurred_at_unix_ms: 1,
        }),
    };
    Arc::make_mut(&mut app.durable).cards[0] = state;
    if exited {
        // Its agent has ended, so nothing will start in it.
        app.cards.insert(card, CardRuntime::Completed);
    }
    card
}

/// A stopped agent terminal switched to chat opens a new conversation in
/// a card of its own beside the terminal. It used to become that
/// conversation in place while still naming the terminal as its source,
/// which storage refuses for anything but the exact handoff of one
/// conversation, so every later save of the window failed and the next
/// launch brought back the window as it was before the switch. The
/// terminal stays, because it still relaunches the conversation it ran.
#[test]
fn a_stopped_terminal_switched_to_chat_opens_a_new_conversation_beside_it() {
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let card = stopped_agent_terminal(
        &mut app,
        &["--resume", KNOWN_THREAD, "--dangerously-skip-permissions"],
        true,
    );
    let terminal = app.card_state(card).unwrap().clone();
    let neighbour = live_chat(&mut app, Some("neighbour"));
    // Filed under a worktree its directory is not in, as an adopted
    // terminal is; the filing, not the directory, is what places the
    // conversation with it.
    let filed = PathBuf::from("/workspace/filed");
    Arc::make_mut(&mut app.durable)
        .card_scopes
        .insert(card, filed.clone());
    let _ = app.update(Message::FocusCard(card));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), [card, neighbour]);
    let saved = |app: &Cagents, id: CardId| {
        let writer = app.session.as_ref().unwrap();
        writer.flush(Duration::from_secs(10)).unwrap();
        assert_eq!(writer.last_error(), None, "every save is accepted");
        writer
            .operation_store()
            .load_card(id, None)
            .unwrap()
            .unwrap()
    };

    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
    let warnings = app
        .notices
        .entries
        .iter()
        .filter(|entry| matches!(entry.notice.severity, crate::app::NoticeSeverity::Warning))
        .map(|entry| entry.notice.message.clone())
        .collect::<Vec<_>>();
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("beside it") && warning.contains("approvals")),
        "a carried bypass is named: {warnings:?}"
    );
    let kept = app.card_state(card).unwrap();
    assert_eq!(kept.kind, terminal.kind, "the terminal is kept as it was");
    assert!(kept.existence.is_open());
    let order = app.workspace.order().collect::<Vec<_>>();
    assert_eq!(order.len(), 3);
    assert_eq!((order[0], order[2]), (card, neighbour));
    let chat = order[1];
    assert_eq!(app.workspace.focused(), Some(chat));
    let state = app.card_state(chat).unwrap();
    assert_eq!(
        state.width_fraction,
        app.settings.default_native_pane_fraction
    );
    let CardKind::Chat(session) = &state.kind else {
        panic!("the new card is a chat");
    };
    assert_eq!(session.launch.provider, Provider::Claude);
    assert_eq!(
        session.launch.session_id, None,
        "the stopped thread is not joined"
    );
    assert!(
        !session
            .launch
            .recipe
            .arguments
            .contains(&KNOWN_THREAD.to_owned()),
        "{:?}",
        session.launch.recipe.arguments
    );
    assert_eq!(session.terminal, None, "no source it did not come from");
    assert!(
        chat_runtime(&app, chat).is_draft(),
        "it waits for its first message"
    );
    assert_eq!(app.durable.card_scopes.get(&chat), Some(&filed));
    let position = |id| app.durable.cards.iter().position(|state| state.id == id);
    assert_eq!(position(chat), position(card).map(|index| index + 1));

    // The window saves both, and keeps saving once the conversation has
    // started.
    assert_eq!(saved(&app, card).kind, terminal.kind);
    assert!(saved(&app, card).existence.is_open());
    assert!(matches!(saved(&app, chat).kind, CardKind::Chat(_)));
    write(&mut app, chat, "hello");
    submit(&mut app, chat);
    let target = app.chat_ui.attempt(chat).unwrap().target;
    let handle = fixture_handle(&app, chat, Some("started"));
    answer_start(&mut app, target, Ok(handle));
    let CardKind::Chat(stored) = saved(&app, chat).kind else {
        panic!("the started conversation is saved");
    };
    assert_eq!(stored.launch.session_id.as_deref(), Some("started"));
    assert!(!app.chat_ui.transition_pending());
}

#[test]
fn a_stopped_terminal_moves_its_restored_chat_draft_to_the_new_card() {
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let terminal = stopped_agent_terminal(&mut app, &["--resume", KNOWN_THREAD], true);
    let text = "Continue this thought\n  with indentation";
    // The file attached to that draft moves with it (#2202).
    let file = Attachment::file("/project/notes.txt");
    let durable = Arc::make_mut(&mut app.durable);
    durable.chat_drafts.insert(terminal, text.into());
    durable
        .chat_draft_attachments
        .insert(terminal, vec![file.clone()]);
    app.chat_ui = State::with_draft_attachments(
        app.durable.chat_drafts.clone(),
        app.durable.chat_draft_attachments.clone(),
    );

    let _ = app.update(Message::ChatUi(Action::Switch(terminal)));
    let chat = app.workspace.focused().unwrap();
    assert_ne!(chat, terminal);
    assert!(matches!(
        app.card_state(terminal).unwrap().kind,
        CardKind::Terminal(_)
    ));
    assert_eq!(chat_runtime(&app, chat).composer.text(), text);
    assert_eq!(chat_runtime(&app, chat).attachments, [file.clone()]);
    assert!(!app.chat_ui.drafts.contains_key(&terminal));
    let snapshot = app.persistent_workspace();
    assert!(!snapshot.chat_drafts.contains_key(&terminal));
    assert!(!snapshot.chat_draft_attachments.contains_key(&terminal));
    assert_eq!(snapshot.chat_drafts.get(&chat).unwrap(), text);
    assert_eq!(snapshot.chat_draft_attachments[&chat], [file]);
}

/// A stopped terminal whose relaunch is still starting may yet run its
/// agent, so no second conversation is opened for it meanwhile.
#[test]
fn a_starting_terminal_opens_no_conversation_beside_it() {
    let mut app = crate::app::tests::context_inspector_test_app(true);
    let card = stopped_agent_terminal(&mut app, &[], false);
    app.begin_terminal_start(card);
    assert!(app.pending_terminal_spawn(card));
    let before = app.durable.clone();
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    assert_eq!(app.durable.cards, before.cards);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), [card]);
    assert!(
        !app.chat_ui
            .phases
            .values()
            .any(|phase| matches!(phase, Phase::Starting(_)))
    );
    assert!(errors(&app).is_empty(), "a wait, not a failure");
}

#[test]
fn a_bypass_is_recognised_in_every_form_it_is_carried() {
    let recipe = |arguments: &[&str], environment: &[(&str, &str)]| LaunchRecipe {
        unset_environment: Default::default(),
        program: "agent".into(),
        arguments: arguments.iter().map(|&argument| argument.into()).collect(),
        working_directory: PathBuf::from(FIXTURE_DIRECTORY),
        environment: environment
            .iter()
            .map(|&(key, value)| (key.into(), value.into()))
            .collect(),
    };
    for bypassing in [
        recipe(&["--dangerously-bypass-approvals-and-sandbox"], &[]),
        recipe(&["--yolo"], &[]),
        recipe(&["--dangerously-skip-permissions"], &[]),
        recipe(&["--permission-mode", "bypassPermissions"], &[]),
        recipe(&["--permission-mode=bypassPermissions"], &[]),
        recipe(&[], &[("CLAUDE_CODE_PERMISSION_MODE", "bypassPermissions")]),
    ] {
        assert!(bypasses_approvals(&bypassing), "{bypassing:?}");
    }
    for asking in [
        recipe(&[], &[]),
        recipe(&["--allow-dangerously-skip-permissions"], &[]),
        recipe(&["--permission-mode", "plan"], &[]),
        recipe(&["bypassPermissions"], &[]),
        recipe(&[], &[("CLAUDE_CODE_PERMISSION_MODE", "plan")]),
    ] {
        assert!(!bypasses_approvals(&asking), "{asking:?}");
    }
}

#[test]
fn a_fresh_claude_runtime_displays_the_selected_accounts_auto_default() {
    let config = tempfile::tempdir().unwrap();
    std::fs::write(
        config.path().join("settings.json"),
        r#"{"permissions":{"defaultMode":"auto"}}"#,
    )
    .unwrap();
    let account = crate::account::Account::managed(
        Provider::Claude,
        uuid::Uuid::new_v4(),
        config.path().into(),
    );
    let session = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        config.path().into(),
        &account,
    )
    .unwrap();
    let runtime = Runtime::new(
        Target {
            card: CardId::new(),
            generation: 0,
        },
        session,
        Connection::Draft,
        true,
        Default::default(),
    );
    assert_eq!(runtime.snapshot.permission_mode.as_deref(), Some("auto"));

    let mut recipe = runtime.session.launch.recipe.clone();
    recipe.arguments.clear();
    recipe
        .environment
        .insert("CLAUDE_CODE_PERMISSION_MODE".into(), "manual".into());
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Claude).as_deref(),
        Some("manual")
    );
    recipe
        .unset_environment
        .insert("CLAUDE_CODE_PERMISSION_MODE".into());
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Claude).as_deref(),
        Some("auto")
    );
}

#[test]
fn codex_draft_permissions_wait_for_project_and_custom_policy_layers() {
    let home = tempfile::tempdir().unwrap();
    let account = home.path().join(".codex");
    let project = home.path().join("project");
    let cwd = project.join("nested");
    std::fs::create_dir_all(&account).unwrap();
    std::fs::create_dir_all(project.join(".codex")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let user_policy = "approval_policy = 'never'\nsandbox_mode = 'danger-full-access'\n";
    std::fs::write(account.join("config.toml"), user_policy).unwrap();
    let mut recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "codex".into(),
        arguments: Vec::new(),
        working_directory: cwd,
        environment: [
            ("HOME".into(), home.path().to_string_lossy().into_owned()),
            ("CODEX_HOME".into(), account.to_string_lossy().into_owned()),
        ]
        .into(),
    };
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("yolo")
    );
    for policy in [
        "approval_policy = 'on-request'",
        "sandbox_mode = 'read-only'",
        "default_permissions = 'restricted'",
        "[permissions.restricted]\nnetwork = false",
    ] {
        std::fs::write(project.join(".codex/config.toml"), policy).unwrap();
        assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
        recipe.arguments = vec!["--yolo".into()];
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("yolo")
        );
        recipe.arguments = ["-a", "on-request", "-s", "read-only"]
            .map(String::from)
            .into();
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("readOnly")
        );
        recipe.arguments.clear();
    }
    std::fs::remove_file(project.join(".codex/config.toml")).unwrap();
    for policy in [
        "default_permissions = ':read-only'",
        "[permissions.restricted]\nnetwork = false",
    ] {
        std::fs::write(
            account.join("config.toml"),
            format!("{user_policy}{policy}\n"),
        )
        .unwrap();
        assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
        recipe.arguments = ["-a", "on-request", "-s", "workspace-write"]
            .map(String::from)
            .into();
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("workspaceWrite")
        );
        recipe.arguments.clear();
    }
    std::fs::write(account.join("config.toml"), user_policy).unwrap();
    // An account directory linked into the cwd hierarchy must not be
    // mistaken for an independent project override. HOME sits just above
    // it, so the walk still crosses the account but ends in this fixture
    // instead of reading the real account of a temp root inside a home.
    recipe.working_directory = home.path().join("elsewhere");
    recipe.environment.insert(
        "HOME".into(),
        home.path().parent().unwrap().to_string_lossy().into_owned(),
    );
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("yolo")
    );
}

#[test]
fn codex_draft_permissions_follow_the_selected_account_and_explicit_flags() {
    let home = tempfile::tempdir().unwrap();
    let config = "approval_policy = 'never'\nsandbox_mode = 'danger-full-access'\n";
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    let mut recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "codex".into(),
        arguments: Vec::new(),
        working_directory: PathBuf::from(FIXTURE_DIRECTORY),
        environment: [(
            "CODEX_HOME".into(),
            home.path().to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect(),
    };
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("yolo")
    );
    for arguments in [
        vec!["--profile", "restricted"],
        vec!["-c", "sandbox_mode='read-only'"],
        vec!["--config=approval_policy='on-request'"],
    ] {
        recipe.arguments = arguments.into_iter().map(String::from).collect();
        assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
    }
    recipe.arguments = ["-a", "on-request", "--sandbox=workspace-write"]
        .map(String::from)
        .into();
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("workspaceWrite")
    );
    set_draft_permission_mode(&mut recipe, Provider::Codex, "readOnly");
    assert!(
        !recipe
            .arguments
            .iter()
            .any(|arg| arg.starts_with("--sandbox="))
    );
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("readOnly")
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        config
    );

    recipe.arguments.clear();
    recipe.working_directory = home.path().to_path_buf();
    recipe.environment.insert("CODEX_HOME".into(), ".".into());
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("yolo")
    );
    recipe
        .environment
        .insert("HOME".into(), home.path().to_string_lossy().into_owned());
    recipe.unset_environment = ["CODEX_HOME".into(), "HOME".into()].into();
    assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
    recipe.unset_environment.clear();

    recipe.arguments.clear();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(
        other.path().join("config.toml"),
        "approval_policy = 'on-request'\nsandbox_mode = 'read-only'\n",
    )
    .unwrap();
    recipe.environment.insert(
        "CODEX_HOME".into(),
        other.path().to_string_lossy().into_owned(),
    );
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("readOnly")
    );
    std::fs::write(
        other.path().join("config.toml"),
        "approval_policy = 'untrusted'\nsandbox_mode = 'read-only'\n",
    )
    .unwrap();
    assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
    std::fs::write(other.path().join("config.toml"), "malformed TOML [").unwrap();
    assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
}

#[test]
fn draft_permission_choice_sets_the_first_turns_launch_policy() {
    let mut recipe = LaunchRecipe {
        unset_environment: Default::default(),
        program: "claude".into(),
        arguments: vec!["--permission-mode".into(), "auto".into()],
        working_directory: PathBuf::from(FIXTURE_DIRECTORY),
        environment: Default::default(),
    };
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Claude).as_deref(),
        Some("auto")
    );
    set_draft_permission_mode(&mut recipe, Provider::Claude, "bypassPermissions");
    assert_eq!(recipe.arguments, ["--permission-mode", "bypassPermissions"]);
    set_draft_permission_mode(&mut recipe, Provider::Claude, "manual");
    assert_eq!(recipe.arguments, ["--permission-mode", "manual"]);

    recipe.arguments.clear();
    recipe.environment.insert(
        "CLAUDE_CODE_PERMISSION_MODE".into(),
        "bypassPermissions".into(),
    );
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Claude).as_deref(),
        Some("bypassPermissions")
    );
    set_draft_permission_mode(&mut recipe, Provider::Claude, "manual");
    assert!(
        !recipe
            .environment
            .contains_key("CLAUDE_CODE_PERMISSION_MODE")
    );

    recipe.program = "codex".into();
    recipe.arguments = vec!["--remote".into(), "unix://".into()];
    set_draft_permission_mode(&mut recipe, Provider::Codex, "yolo");
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("yolo")
    );
    assert!(recipe.arguments.contains(&"--yolo".into()));
    set_draft_permission_mode(&mut recipe, Provider::Codex, "workspaceWrite");
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("workspaceWrite")
    );
    assert_eq!(
        &recipe.arguments[2..],
        &[
            "--ask-for-approval",
            "on-request",
            "--sandbox",
            "workspace-write"
        ]
    );
    set_draft_permission_mode(&mut recipe, Provider::Codex, "readOnly");
    assert_eq!(
        draft_permission_mode(&recipe, Provider::Codex).as_deref(),
        Some("readOnly")
    );
}

/// A draft moved to another provider, then to another account, is saved
/// as the conversation it now holds, and so is the conversation its first
/// message starts. Storage used to refuse such moves, which failed every
/// later save of the window and brought the old conversation back on the
/// next launch, where the daemon no longer had it.
#[test]
fn a_draft_moved_to_another_provider_or_account_is_saved_as_its_new_conversation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let managed = app.account_ui.add_managed_fixture(Provider::Claude, 24);
    let home = app
        .known_account(&managed)
        .and_then(|account| account.resolved_home())
        .unwrap()
        .display()
        .to_string();
    let card = draft_chat(&mut app);
    let flushed = |app: &Cagents| {
        let writer = app.session.as_ref().unwrap();
        writer.flush(Duration::from_secs(10)).unwrap();
        assert_eq!(writer.last_error(), None);
        match writer.operation_store().load_card(card, None).unwrap() {
            Some(CardState {
                kind: CardKind::Chat(stored),
                ..
            }) => stored,
            other => panic!("the card is stored as a chat: {other:?}"),
        }
    };
    let target = |app: &Cagents| chat_runtime(app, card).target;
    // The started conversation adds its provider's name for it and the
    // daemon's proof; what a card may not change on its own is the rest.
    let identity = |session: &chat::Session| {
        (
            session.id,
            session.launch.provider,
            session.launch.recipe.clone(),
        )
    };
    assert_eq!(flushed(&app).launch.provider, Provider::Codex);

    let _ = app.update(Message::ChatUi(Action::DraftModel(
        target(&app),
        Provider::Claude,
        None,
    )));
    assert!(!app.chat_start_pending(card), "a choice starts nothing");
    let claude = chat_runtime(&app, card).session.clone();
    assert_eq!(claude.launch.provider, Provider::Claude);
    assert_eq!(identity(&flushed(&app)), identity(&claude));
    assert_eq!(app.card_state(card).unwrap().title, "Claude chat");

    let _ = app.update(Message::ChatUi(Action::ChooseAccount(
        target(&app),
        managed.clone(),
    )));
    assert!(!app.chat_start_pending(card));
    let moved = chat_runtime(&app, card).session.clone();
    assert_eq!(identity(&flushed(&app)), identity(&moved));
    assert_eq!(
        moved.launch.recipe.environment.get("CLAUDE_CONFIG_DIR"),
        Some(&home)
    );

    // The first message starts the conversation under an identity of its
    // own, which is saved before the provider has answered.
    write(&mut app, card, "hello");
    submit(&mut app, card);
    let attempt = app.chat_ui.attempt(card).unwrap();
    let (started, session) = (attempt.target, attempt.session.clone());
    assert_ne!(session.id, moved.id);
    assert_eq!(session.launch.recipe, moved.launch.recipe);
    assert_eq!(identity(&flushed(&app)), identity(&session));
    let replacement = "33333333-4444-4aaa-8bbb-cccccccccccc";
    let handle = fixture_handle(&app, card, Some(replacement));
    answer_start(&mut app, started, Ok(handle));
    let stored = flushed(&app);
    assert_eq!(identity(&stored), identity(&session));
    assert_eq!(stored.launch.session_id.as_deref(), Some(replacement));

    // A resume the provider refused is offered a fresh start, which
    // drops the saved conversation on purpose.
    app.cards.remove(&card);
    app.chat_ui.fresh_offers.insert(card);
    let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
    let closing = fresh_close_request(&app, card);
    let _ = app.update(Message::ChatUi(fresh_close_answer(closing, Ok(()))));
    let attempt = app.chat_ui.attempt(card).unwrap();
    let (target, fresh) = (attempt.target, attempt.session.clone());
    assert_eq!(fresh.launch.session_id, None);
    let fresh_id = "44444444-5555-4bbb-8ccc-dddddddddddd";
    let handle = fixture_handle(&app, card, Some(fresh_id));
    answer_start(&mut app, target, Ok(handle));
    let stored = flushed(&app);
    assert_eq!(identity(&stored), identity(&fresh));
    assert_eq!(stored.launch.session_id.as_deref(), Some(fresh_id));
    // A late or repeated press, once nothing offers it, keeps it.
    let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
    assert!(app.chat_ui.attempt(card).is_none());
    assert_eq!(flushed(&app), stored);
}

/// A conversation that has begun keeps its account.
#[test]
fn a_begun_chat_cannot_change_account() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let managed = app.account_ui.add_managed_fixture(Provider::Codex, 22);
    let card = live_chat(&mut app, Some("thread"));
    let target = match app.cards.get_mut(&card) {
        Some(CardRuntime::Chat(runtime)) => {
            Arc::make_mut(&mut runtime.snapshot).items.push(
                chat::Item {
                    id: "prompt".into(),
                    role: chat::Role::User,
                    content: text("hello"),
                    started_at: None,
                    at: None,
                }
                .into(),
            );
            runtime.target
        }
        _ => unreachable!(),
    };
    let _ = app.update(Message::ChatUi(Action::AccountPicker(target, true)));
    let _ = app.update(Message::ChatUi(Action::ChooseAccount(target, managed)));
    assert!(!app.chat_ui.phases.values().any(|phase| matches!(
        phase,
        Phase::Closing {
            restart: Some(_),
            ..
        }
    )));
    assert!(
        !matches!(app.chat_ui.phases.get(&target.card), Some(Phase::Closing { target: active, .. }) if *active == target)
    );
    match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) => assert!(!runtime.account_picker),
        _ => unreachable!(),
    }
}

/// A live conversation whose provider stopped without ever loading the
/// conversation it was asked to resume, which offers a fresh start.
fn unresumable_chat(app: &mut Cagents) -> (CardId, Target) {
    let card = live_chat(app, Some(KNOWN_THREAD));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        unreachable!()
    };
    runtime.session.launch.session_id = Some(KNOWN_THREAD.into());
    let snapshot = Arc::make_mut(&mut runtime.snapshot);
    snapshot.exited = true;
    snapshot.session_id = None;
    let target = runtime.target;
    assert!(app.chat_offers_fresh_conversation(card));
    (card, target)
}

/// Capture the exact checked-close request before any later mutation.
pub(super) fn fresh_close_request(
    app: &Cagents,
    card: CardId,
) -> (Target, chat::Session, chat::Session) {
    let Some(Phase::Closing {
        target,
        restart: Some(_),
        ..
    }) = app.chat_ui.phases.get(&card)
    else {
        panic!("Fresh must wait for checked retirement");
    };
    let Some(CardKind::Chat(requested)) = app.card_state(card).map(|state| &state.kind) else {
        panic!("the saved descriptor remains until checked retirement");
    };
    (
        *target,
        app.chat_return_session(card).unwrap(),
        requested.clone(),
    )
}

pub(super) fn fresh_close_answer(
    (target, source, requested): (Target, chat::Session, chat::Session),
    result: Result<(), String>,
) -> Action {
    Action::FreshCloseFinished(target, Box::new(source), Box::new(requested), result)
}

/// A fresh start gives up the previous conversation before it starts the
/// new one; a close meanwhile keeps the first half and cancels the
/// second.
#[test]
fn a_chat_closed_mid_restart_never_starts_its_replacement() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, original) = unresumable_chat(&mut app);
    let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
    let closing = fresh_close_request(&app, card);
    let target = closing.0;
    assert_ne!(target, original, "the close owns a new operation identity");
    assert!(
        matches!(app.chat_ui.phases.get(&target.card), Some(Phase::Closing { target: active, .. }) if *active == target)
    );
    close_chat(&mut app, card);
    assert!(
        app.chat_ui.retiring.is_empty(),
        "the restart's close is the retirement"
    );
    let _ = app.update(Message::ChatUi(fresh_close_answer(closing, Ok(()))));
    assert!(
        !app.chat_ui
            .phases
            .values()
            .any(|phase| matches!(phase, Phase::Starting(_))),
        "the replacement never starts"
    );
    assert!(!app.chat_ui.transition_pending());
}

/// A draft's first message starts its conversation, so it is refused in
/// a worktree being deleted, and the message stays where it was written.
#[test]
fn a_draft_in_a_worktree_being_deleted_does_not_start() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    app.project_ui
        .begin_deleting(PathBuf::from(FIXTURE_DIRECTORY));
    write(&mut app, card, "hello");
    submit(&mut app, card);
    assert!(
        !app.chat_ui
            .phases
            .values()
            .any(|phase| matches!(phase, Phase::Starting(_)))
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.queued.is_none());
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert_eq!(
        app.notices
            .entries
            .back()
            .map(|entry| entry.notice.message.as_str()),
        Some("That worktree is being deleted.")
    );
}

/// A fresh start already closing the old conversation when its worktree
/// is confirmed for deletion must not start the replacement there.
#[test]
fn a_restart_under_way_never_starts_its_replacement_in_a_worktree_being_deleted() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, original) = unresumable_chat(&mut app);
    let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
    let closing = fresh_close_request(&app, card);
    let target = closing.0;
    assert_ne!(target, original, "the close owns a new operation identity");
    assert!(
        matches!(app.chat_ui.phases.get(&target.card), Some(Phase::Closing { target: active, .. }) if *active == target)
    );
    app.project_ui
        .begin_deleting(PathBuf::from(FIXTURE_DIRECTORY));
    app.sidebar_state.set_card_activity(
        card,
        crate::agent::Activity {
            provider: Provider::Codex,
            state: crate::agent::ActivityState::Working,
        },
    );

    let _ = app.update(Message::ChatUi(fresh_close_answer(closing, Ok(()))));
    assert!(
        !app.chat_ui
            .phases
            .values()
            .any(|phase| matches!(phase, Phase::Starting(_))),
        "the replacement started in the worktree being deleted"
    );
    assert!(!matches!(
        app.chat_ui.phases.get(&card),
        Some(Phase::Closing {
            restart: Some(_),
            ..
        })
    ));
    assert!(!app.chat_start_pending(card));
    let runtime = chat_runtime(&app, card);
    assert!(!runtime.connected());
    assert_eq!(
        runtime.error.as_deref(),
        Some("Restart cancelled: the worktree was being deleted.")
    );
    assert!(
        app.sidebar_state.card_activity(card).is_none(),
        "the closed conversation still shows as active"
    );
    assert_eq!(
        app.notices
            .entries
            .back()
            .map(|entry| entry.notice.message.as_str()),
        Some("That worktree is being deleted.")
    );
}

/// The terminal cards open in the current scope.
fn terminal_cards(app: &Cagents) -> Vec<CardId> {
    app.workspace
        .order()
        .filter(|id| {
            matches!(
                app.card_state(*id).map(|state| &state.kind),
                Some(CardKind::Terminal(_))
            )
        })
        .collect()
}

/// Checks that a returned chat leaves, and that a new terminal running
/// `launch` takes its place only once it has.
fn assert_replaced_by_terminal(app: &mut Cagents, card: CardId, launch: &LaunchRecipe) {
    let chat = match &app.card_state(card).unwrap().kind {
        CardKind::Chat(chat) => chat.id,
        _ => panic!("the chat card is never converted in place"),
    };
    assert_eq!(app.workspace.pending_close(), None);
    let _ = app.update(Message::CommitClose(card)); // Duplicate must not create another terminal.
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(!app.cards.contains_key(&card));
    assert!(!app.chat_ui.phases.contains_key(&card));
    let terminals = terminal_cards(app);
    assert_eq!(terminals.len(), 1, "one terminal takes the chat's place");
    let terminal = terminals[0];
    let Some(CardKind::Terminal(session)) = app.card_state(terminal).map(|state| &state.kind)
    else {
        unreachable!()
    };
    assert_eq!(&session.launch, launch);
    assert_ne!(
        session.id, chat,
        "a new terminal, not the conversation's identity"
    );
    let order = app.workspace.order().collect::<Vec<_>>();
    assert!(!order.contains(&card));
    assert!(order.contains(&terminal));
    let saved = |id| app.durable.cards.iter().position(|state| state.id == id);
    assert_eq!(saved(terminal), saved(card).map(|index| index + 1));
    assert_eq!(app.workspace.focused(), Some(terminal));
    assert!(matches!(
        app.cards.get(&terminal),
        Some(CardRuntime::Loading)
    ));
    assert!(!app.chat_ui.transition_pending());
}

/// "Return to terminal" refused a conversation the provider had not named
/// yet, and silently did nothing for one that never started, so a chat
/// that could not go back had no terminal to offer at all.
#[test]
fn a_chat_with_no_conversation_to_return_to_opens_its_provider_cli() {
    for failed in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let account = app.account_ui.add_managed_fixture(Provider::Codex, 17);
        let _ = app.update(Message::AccountUi(
            crate::app::account_ui::Action::SetDefault(account),
        ));
        let home = app
            .default_account(Provider::Codex)
            .resolved_home()
            .unwrap();
        let card = if failed {
            failed_chat(&mut app)
        } else {
            live_chat(&mut app, None)
        };
        assert_eq!(app.chat_switch_label(card), "Open in terminal");
        assert!(app.chat_switch_ready(card));
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        let (target, plan) = local_return(&app);
        assert!(!plan.resumes);
        let cli = plan.launch.clone();
        assert_eq!(cli.program, "codex");
        assert_eq!(cli.working_directory, PathBuf::from(FIXTURE_DIRECTORY));
        assert!(
            !cli.arguments
                .iter()
                .any(|argument| argument.starts_with("--remote"))
        );
        assert_eq!(cli.environment["CODEX_HOME"], home.display().to_string());
        assert_eq!(
            cli.unset_environment,
            app.default_account(Provider::Codex)
                .credential_overrides()
                .iter()
                .map(|name| (*name).to_owned())
                .collect()
        );
        assert!(!app.chat_switch_ready(card), "one return at a time");

        // The fresh CLI shares no conversation with the actor, so doubt about the
        // actor does not keep the card a chat; the actor is given up
        // again, and that retirement reports whether it succeeded.
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
            target,
            Err(ReturnRefused::Unconfirmed(
                "fixture: daemon unreachable".into(),
            )),
        )));
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        let retry = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(retry, Ok(()))));
        assert_replaced_by_terminal(&mut app, card, &cli);
    }
}

/// A failed card whose conversation the provider did name goes back to
/// that conversation in its CLI, but never beside an actor that might
/// still be holding it.
#[test]
fn a_failed_chat_with_a_known_conversation_resumes_it_in_its_cli() {
    for retired in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let state = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|state| state.id == card)
            .unwrap();
        let CardKind::Chat(session) = &mut state.kind else {
            unreachable!()
        };
        session.launch.session_id = Some(KNOWN_THREAD.into());
        let resume = session.launch.terminal_recipe().unwrap();
        assert_eq!(app.chat_switch_label(card), "Return to terminal");
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        let (target, plan) = local_return(&app);
        assert!(plan.resumes);
        assert_eq!(plan.launch, resume, "the CLI the daemon would resume");
        assert!(
            resume
                .arguments
                .iter()
                .any(|argument| argument == KNOWN_THREAD),
            "{resume:?}"
        );
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
            target,
            if retired {
                Ok(())
            } else {
                Err(ReturnRefused::Unconfirmed(
                    "fixture: daemon unreachable".into(),
                ))
            },
        )));
        if retired {
            assert_replaced_by_terminal(&mut app, card, &resume);
        } else {
            assert!(terminal_cards(&app).is_empty(), "no second writer");
            assert!(matches!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(_)
            ));
            assert_eq!(app.workspace.pending_close(), None);
            assert!(!errors(&app).is_empty());
            assert!(!app.chat_ui.transition_pending());
        }
        assert!(!app.chat_start_pending(card));
    }
}

/// A Codex chat and the CLI the daemon still runs for its thread end
/// together, so a chat that cannot take the daemon's handoff keeps its
/// card rather than kill that CLI, and a close then closes it as usual.
#[test]
fn a_codex_chat_whose_cli_still_runs_is_kept_rather_than_killing_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    let target = local_return(&app).0;
    let _ = app.update(Message::CloseCard(card));
    let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
        target,
        Err(ReturnRefused::SourceLive),
    )));
    assert!(terminal_cards(&app).is_empty());
    assert!(!errors(&app).is_empty(), "the refusal says why");
    app.notices.entries.clear();
    // The queued close retains ownership until the daemon proves cleanup.
    assert_eq!(app.workspace.pending_close(), None);
    let _ = app.update(Message::CommitClose(card));
    assert!(app.workspace.order().any(|id| id == card));
    assert!(matches!(
        app.chat_ui.phases.get(&card),
        Some(Phase::Closing { restart: None, .. })
    ));
    close_chat(&mut app, card);
    assert!(app.chat_ui.retiring.is_empty());

    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    let target = local_return(&app).0;
    let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
        target,
        Err(ReturnRefused::SourceLive),
    )));
    assert!(terminal_cards(&app).is_empty());
    assert!(matches!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(_)
    ));
    assert_eq!(app.workspace.pending_close(), None);
    assert!(app.chat_switch_ready(card), "it can be tried again");
    assert!(!app.chat_ui.transition_pending());
}

/// Activity that arrives while a chat returns to its terminal is not read
/// then, and the connection wakes for activity again only once it has
/// been read. A chat whose return failed stays on that connection, so it
/// reads it then, or a hidden card would never show activity again.
#[test]
fn a_chat_whose_return_failed_reads_the_activity_it_missed() {
    use crate::agent::ActivityState;
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    let target = app
        .chat_ui
        .phases
        .values()
        .find_map(|phase| match phase {
            Phase::Returning { target, .. } => Some(*target),
            _ => None,
        })
        .unwrap();
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    assert!(handle.observe_activity_for_test(ActivityState::Working));
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Activity)));
    let _ = app.update(Message::ChatUi(Action::HandoffFinished(
        target,
        Err("fixture: the terminal could not start".into()),
    )));
    assert!(matches!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(_)
    ));
    assert_eq!(
        app.sidebar_state
            .card_activity(card)
            .map(|activity| activity.state),
        Some(ActivityState::Working)
    );
    assert!(
        handle.observe_activity_for_test(ActivityState::Idle),
        "the next edge wakes the surface again"
    );
}

/// A close pressed while the card is on its way back to a terminal waits
/// for that return to settle, then closes the card instead of starting a
/// terminal only to stop it.
#[test]
fn a_chat_closed_during_its_return_closes_once_the_return_lands() {
    for confirmed in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, None);
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = local_return(&app).0;
        let _ = app.update(Message::CloseCard(card));
        assert!(matches!(
            app.chat_ui.phases.get(&card),
            Some(Phase::Returning {
                close_requested: true,
                ..
            })
        ));
        assert_eq!(app.workspace.pending_close(), None);
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        let result = if confirmed {
            Ok(())
        } else {
            Err(ReturnRefused::Unconfirmed(
                "fixture: daemon unreachable".into(),
            ))
        };
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, result)));
        assert!(!app.chat_ui.phases.values().any(|phase| matches!(
            phase,
            Phase::Returning {
                close_requested: true,
                ..
            }
        )));
        assert_eq!(
            app.chat_ui.retiring.is_empty(),
            confirmed,
            "a conversation not confirmed stopped is given up again"
        );
        assert_eq!(
            app.workspace.pending_close(),
            None,
            "the close commits with the return"
        );
        let _ = app.update(Message::CommitClose(card));
        assert!(
            terminal_cards(&app).is_empty(),
            "no terminal only to stop it"
        );
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.cards.contains_key(&card));
        if !confirmed {
            let retry = retired(&mut app, card);
            let _ = app.update(Message::ChatUi(Action::Retired(retry, Ok(()))));
        }
        assert!(!app.chat_ui.transition_pending());
    }
}

/// A return that lands once its chat is no longer on screen, or once its
/// removal cannot be reserved, keeps the chat rather than opening a
/// terminal in a strip the chat is not in, or beside a chat that stays.
#[test]
fn a_return_that_cannot_take_the_chats_place_keeps_the_chat() {
    for away in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let other = failed_chat(&mut app);
        let card = failed_chat(&mut app);
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = local_return(&app).0;
        if away {
            // The user has moved to another strip meanwhile.
            app.workspace =
                crate::workspace::Workspace::new(crate::workspace::WorkspaceConfig::default())
                    .unwrap();
        } else {
            // Another card's close is waiting for its commit.
            app.workspace.request_close(other).unwrap();
        }
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
        assert!(terminal_cards(&app).is_empty());
        assert!(!app.chat_ui.phases.contains_key(&card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(matches!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(_)
        ));
        assert!(app.chat_switch_ready(card), "it can be tried again");
        assert!(!app.chat_ui.transition_pending());
    }
}

#[test]
fn resume_is_read_from_a_draft_naming_it() {
    assert_eq!(resume_invocation("/resume").as_deref(), Some(""));
    assert_eq!(
        resume_invocation("  /Resume  flaky tests \n").as_deref(),
        Some("flaky tests")
    );
    assert_eq!(resume_invocation("/resumes"), None);
    assert_eq!(resume_invocation("resume"), None);
    assert_eq!(resume_invocation("please /resume"), None);
}

/// `/resume` sends nothing: it opens the picker on the conversation's
/// project, searching for what follows it, and an untouched
/// draft gives its place to the conversation chosen there.
#[test]
fn resume_command_opens_the_picker_for_this_conversation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let project = tempfile::tempdir().unwrap();
    let card = draft_in(&mut app, project.path());
    write(&mut app, card, "/resume flaky tests");
    submit(&mut app, card);
    assert!(app.resume_ui.open);
    assert_eq!(app.resume_ui.query, "flaky tests");
    let scope = app.resume_ui.scope.as_ref().expect("a narrowed search");
    assert_eq!(scope.reach, crate::app::resume_ui::Reach::Project);
    assert!(
        scope
            .roots
            .iter()
            .any(|root| project.path().starts_with(root))
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.composer.is_empty());
    assert!(runtime.is_draft() && !app.chat_start_pending(card));
    assert_eq!(app.resume_ui.replace, Some(card));
    assert_eq!(app.resume_ui.asked_from, Some(card), "not offered");

    let _ = app.resume_saved_conversation(earlier(&app, card));
    let resumed = app.workspace.focused().unwrap();
    assert_ne!(resumed, card);
    assert_eq!(app.card_state(resumed).unwrap().title, "Earlier");
    assert!(!app.resume_ui.open);
    assert_eq!(
        app.workspace
            .pending_close()
            .map(|handoff| handoff.unmounting),
        Some(card),
        "the draft gives its place to the conversation"
    );
}

/// A draft is replaced only while the picker it asked for is open.
#[test]
fn a_draft_outlives_a_cancelled_resume() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let project = tempfile::tempdir().unwrap();
    let card = draft_in(&mut app, project.path());
    write(&mut app, card, "/resume");
    submit(&mut app, card);
    assert_eq!(app.resume_ui.replace, Some(card));
    let _ = app.update_resume(super::resume_ui::Action::Close);
    assert_eq!(app.resume_ui.replace, None);
    let _ = app.resume_saved_conversation(earlier(&app, card));
    assert!(app.workspace.pending_close().is_none());
    assert!(app.workspace.order().any(|id| id == card));
}

#[test]
fn a_closed_claude_conversation_can_reopen_as_history_without_its_directory() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let project = tempfile::tempdir().unwrap();
    let card = draft_in(&mut app, project.path());
    let mut saved = earlier(&app, card);
    saved.session.launch.provider = Provider::Claude;
    saved.session.launch.recipe.program = "claude".into();
    saved.session.launch.recipe.arguments.clear();
    saved.session.launch.recipe.working_directory = project.path().join("removed");
    // It ran before it was closed, so it carries that start's proof.
    saved.session.proof = Some(crate::model::HostSessionProof {
        host_session_id: "saved-owner".into(),
        boot_id: "fixture-boot".into(),
        host_incarnation: "saved-incarnation".into(),
    });
    let provider_id = saved.session.launch.session_id.clone();
    let _ = app.resume_saved_conversation(saved);
    let reopened = app.workspace.focused().unwrap();
    assert_ne!(reopened, card);
    let CardKind::Chat(session) = &app.card_state(reopened).unwrap().kind else {
        panic!("the saved conversation reopens as a chat");
    };
    assert_eq!(session.launch.session_id, provider_id);
    assert!(session.launch.needs_read_only_history());
    assert!(app.resume_ui.error.is_none());
}

/// A `/resume` the picker cannot answer is left for the writer to send
/// again.
#[test]
fn resume_keeps_its_draft_while_another_dialog_holds_the_window() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let project = tempfile::tempdir().unwrap();
    let card = draft_in(&mut app, project.path());
    write(&mut app, card, "/resume flaky");
    app.resume_ui.open = true;
    submit(&mut app, card);
    assert_eq!(
        chat_runtime(&app, card).composer.text().trim(),
        "/resume flaky"
    );
    assert_eq!(app.resume_ui.replace, None);
    assert!(app.resume_ui.query.is_empty());
}

/// A new conversation's draft in `directory`, a folder that exists.
fn draft_in(app: &mut Cagents, directory: &Path) -> CardId {
    let _ = app.new_native_chat(Provider::Codex, Some(directory.into()));
    let card = app.workspace.focused().unwrap();
    assert!(chat_runtime(app, card).is_draft());
    card
}

/// Another conversation in the project of `card`'s, as resume lists it.
fn earlier(app: &Cagents, card: CardId) -> crate::persistence::SavedConversation {
    let mut session = chat_runtime(app, card).session.clone();
    session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    // Resumed directly, without a daemon to prepare it.
    session.native_continuity = chat::NativeContinuity::ReconnectOnly;
    crate::persistence::SavedConversation {
        card: CardId::new(),
        title: "Earlier".into(),
        session,
        open: false,
        updated_at_unix_ms: 1,
    }
}

/// A send pressed after the removal was reserved never starts a
/// conversation in the card it is removing.
#[test]
fn a_closing_draft_never_starts() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    write(&mut app, card, "hello");
    let target = chat_runtime(&app, card).target;
    let _ = app.update(Message::CloseCard(card));
    let _ = app.handle_chat_ui(Action::Submit(target, PromptMode::Send));
    assert!(!app.chat_start_pending(card));
}

/// Issue #237: a channel whose panes are all closed closes itself, in
/// either order, including when its chat never got as far as starting.
#[test]
fn a_channel_closes_when_its_last_pane_does() {
    use super::super::workspace_model_sync::ModelUpdate;
    use crate::workspace_model::{
        ModelChannel, ModelEdit, ModelProject,
        views::{Pane, Scope, View},
    };

    for (chat_first, chat_state) in [(true, 0), (true, 1), (true, 2), (false, 0), (false, 2)] {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let terminal = app.workspace.focused().unwrap();
        // A shell that already ended. The reducer receives the daemon's
        // positive absence receipt below, without starting a real worker.
        app.cards
            .insert(terminal, CardRuntime::Failed("fixture".into()));
        let CardKind::Terminal(session) = &mut app.card_state_mut(terminal).unwrap().kind else {
            unreachable!()
        };
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        let terminal_source = session.clone();
        let channel = crate::model::ChannelId::new();
        let project = ProjectId::new();
        let repository = PathBuf::from(FIXTURE_DIRECTORY);
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Fixture project".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        ));
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            title_source: Default::default(),
            id: channel,
            project_id: project,
            title: String::new(),
            ordinal: 4,
            shared: true,
            cards: vec![terminal],
            focused: Some(terminal),
        });
        durable.active_channel = Some(channel);
        // Already in the chat's directory, so starting it changes no scope.
        app.active_workspace = Some(PathBuf::from(FIXTURE_DIRECTORY));
        let chat = match chat_state {
            0 => starting_chat(&mut app).0,
            1 => failed_chat(&mut app),
            _ => live_chat(&mut app, Some("thread")),
        };
        // A confirmed canonical Home, rather than an unregistered local
        // channel: only its authoritative disappearance permits retirement.
        let home = crate::workspace_model::ChannelIncarnation {
            registry: uuid::Uuid::new_v4(),
            channel,
            creation_ordinal: 4,
        };
        let scope = Scope::Channel(channel);
        app.workspace_model.identity = home.registry;
        app.workspace_model.sequence = 10;
        app.workspace_model.projects.push(ModelProject {
            repository: repository.clone(),
            name: "Fixture project".into(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        });
        app.workspace_model.channels.push(ModelChannel {
            id: channel,
            repository,
            title: String::new(),
            title_source: Default::default(),
            ordinal: home.creation_ordinal,
        });
        let panes = [terminal, chat]
            .into_iter()
            .map(|card| Pane::from_card(app.card_state(card).unwrap(), None).unwrap())
            .collect();
        app.workspace_model.views.push(View {
            scope: scope.clone(),
            revision: 7,
            panes,
            retired: Vec::new(),
        });
        let (publish, published) = test_model_channel();
        app.workspace_model_tx = publish;
        let occupied = app.workspace_model.clone();
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope: scope.clone(),
            operation: None,
            model: occupied,
        }));
        let channel_exists = |app: &Cagents| {
            app.durable
                .channels
                .iter()
                .any(|candidate| candidate.id == channel)
        };
        let (first, second) = if chat_first {
            (chat, terminal)
        } else {
            (terminal, chat)
        };
        for card in [first, second] {
            if card == chat {
                close_chat(&mut app, card);
            } else {
                let _ = app.update(Message::CloseCard(card));
                let _ = app.update(Message::CompletedTerminalClosed {
                    card,
                    session: terminal_source.clone(),
                    result: Ok(
                        super::super::terminal_close::CompletedCloseOutcome::ClosedWithBasis(
                            app.terminal_cleanup_authority(card).receipt(
                                crate::workspace_model::views::TerminalRemovalBasis::Absent {
                                    owner: HostSessionProof {
                                        host_session_id: "fixture-host".into(),
                                        boot_id: "fixture-boot".into(),
                                        host_incarnation: "fixture-incarnation".into(),
                                    },
                                    latest_worker: None,
                                    history_epoch: 0,
                                },
                            ),
                        ),
                    ),
                });
                let _ = app.update(Message::CommitClose(card));
                assert!(!app.workspace.order().any(|id| id == card));
            }
            assert!(
                channel_exists(&app),
                "no eager deletion before canonical empty-view proof: chat first: {chat_first}, chat state: {chat_state}"
            );
        }
        assert!(!app.channel_ui.was_deleted(channel));
        assert!(app.channel_ui.retire_candidates.contains_key(&home));
        let mut before_ack = Vec::new();
        while let Ok(edit) = published.try_recv() {
            before_ack.push(edit);
        }
        assert!(
            !before_ack
                .iter()
                .any(|edit| matches!(edit, ModelEdit::RetireEmptyChannel { .. })),
            "the occupied canonical view cannot authorize retirement: {before_ack:?}"
        );

        let mut settled = app.workspace_model.clone();
        settled.sequence = 11;
        let view = settled
            .views
            .iter_mut()
            .find(|view| view.scope == scope)
            .unwrap();
        view.panes.clear();
        view.retired = vec![terminal, chat];
        view.revision = 8;
        drop(app.apply_workspace_model_update(ModelUpdate::ViewApplied {
            scope,
            operation: None,
            model: settled.clone(),
        }));
        let mut after_ack = Vec::new();
        while let Ok(edit) = published.try_recv() {
            after_ack.push(edit);
        }
        let retire_requests = after_ack
            .iter()
            .filter_map(|edit| match edit {
                ModelEdit::RetireEmptyChannel {
                    home,
                    expected_view_revision,
                } => Some((*home, *expected_view_revision)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retire_requests, vec![(home, 8)], "{after_ack:?}");

        // These are explicit App model-reply fixtures, not an executed
        // daemon or Work DB. HOST integration owns the actual ledger check.
        // A retained-Work reply preserves the confirmed empty Home.
        settled.sequence = 12;
        drop(app.apply_workspace_model_update(ModelUpdate::Read(settled.clone())));
        assert!(channel_exists(&app));
        assert!(!app.channel_ui.was_deleted(channel));
        assert!(!app.channel_has_open_card(channel));
        // An authoritative no-work retirement reply removes that same Home.
        settled.sequence = 13;
        settled.channels.retain(|entry| entry.id != channel);
        settled
            .views
            .retain(|view| view.scope != Scope::Channel(channel));
        drop(app.apply_workspace_model_update(ModelUpdate::Read(settled)));
        assert!(!channel_exists(&app));
        assert!(app.channel_ui.was_deleted(channel));
    }
}

#[test]
fn failed_workspace_commit_keeps_runtime_operation_and_saved_identity() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let other = failed_chat(&mut app);
    let (card, target) = starting_chat(&mut app);
    let source = app.card_state(card).unwrap().clone();
    let order = app.workspace.order().collect::<Vec<_>>();
    assert!(app.commit_card_removal(card, None).is_none());
    app.workspace.request_close(other).unwrap();
    assert!(app.commit_card_removal(card, None).is_none());
    assert_eq!(app.card_state(card), Some(&source));
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
    assert_eq!(app.chat_ui.attempt(card).unwrap().target, target);
    assert!(app.cards.contains_key(&card));
    assert!(app.chat_ui.abandoned.is_empty());
    assert!(app.chat_ui.retiring.is_empty());
    app.workspace.cancel_close(other).unwrap();
    app.notices.entries.clear();
    app.status = None;
    close_chat(&mut app, card);
}

/// A saved terminal, as a pane restored from an earlier daemon has one.
fn saved_terminal_pane(app: &mut Cagents) -> CardId {
    let mut state = crate::app::tests::terminal_card_state();
    let card = state.id;
    let CardKind::Terminal(session) = &mut state.kind else {
        unreachable!();
    };
    session.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("earlier-host"),
        boot_id: crate::model::BootId::new("earlier-boot"),
        host_incarnation: crate::model::HostIncarnation::new("earlier-incarnation"),
    };
    Arc::make_mut(&mut app.durable).cards.push(state);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    card
}

/// Answers the close `card` is waiting on: its terminal is proven gone.
fn settle_proven_gone(app: &mut Cagents, card: CardId) {
    let intent = app
        .terminal_close
        .clone()
        .expect("this pane's close is running");
    assert_eq!(intent.card, card);
    let _ = app.update(Message::TerminalCloseFinished {
        card,
        terminal: intent.terminal,
        proof: intent.proof,
        generation: intent.generation,
        outcome: crate::app::terminal_close::CloseOutcome::NotHosted,
    });
    let _ = app.update(Message::CommitClose(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());
}

/// "Close other panes" with a retained chat first: the chat holds the
/// workspace's only removal, and its close can fail with nothing left to
/// tell the panes behind it.
#[test]
fn a_failed_retained_chat_close_does_not_strand_the_panes_behind_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let first = saved_terminal_pane(&mut app);
    let second = saved_terminal_pane(&mut app);
    app.chrome_ui.closing_group = vec![card, first, second];
    drop(app.close_pane_group());
    assert_eq!(app.workspace.pending_close().unwrap().unmounting, card);
    let _ = app.update(Message::CommitClose(card));
    let target = app.chat_ui.phases[&card].target();
    let saved = app.chat_return_session(card).unwrap();
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        target,
        saved,
        Err("cleanup remains unconfirmed".into()),
    )));

    assert!(app.card_state(card).unwrap().existence.is_open());
    assert_eq!(
        app.workspace.pending_close(),
        None,
        "a chat that stays gives up its removal"
    );
    settle_proven_gone(&mut app, first);
    settle_proven_gone(&mut app, second);
    assert!(!app.terminal_close_in_progress(second));

    app.notices.entries.clear();
    app.status = None;
    close_chat(&mut app, card);
}

/// A chat's close runs beside other closes and only needs the workspace's
/// removal at the end, when another pane could be holding it.
#[test]
fn a_chat_finishing_during_another_removal_waits_its_turn() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let chat = failed_chat(&mut app);
    let pane = saved_terminal_pane(&mut app);
    app.workspace.request_close(pane).unwrap();
    let _ = app.update(Message::CloseCard(chat));
    let finish = |app: &mut Cagents| {
        let target = app.chat_ui.phases[&chat].target();
        let saved = app.chat_return_session(chat).unwrap();
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            target,
            saved,
            Ok(()),
        )));
    };
    finish(&mut app);
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
    assert!(app.card_state(chat).unwrap().existence.is_open());
    assert!(app.terminal_close_in_progress(chat));

    let _ = app.workspace.cancel_close(pane);
    let _ = app.update(Message::DismissStatus);
    finish(&mut app);
    assert!(!app.card_state(chat).unwrap().existence.is_open());
}

/// A slow chat close takes as long as its daemon takes to answer. Nothing
/// about it is a reason to hold up a terminal in another pane.
#[test]
fn a_chat_closing_does_not_hold_up_another_panes_close() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let chat = failed_chat(&mut app);
    let pane = saved_terminal_pane(&mut app);
    let _ = app.update(Message::CloseCard(chat));
    assert!(app.chat_close_owned(chat));
    assert!(app.pane_close_owned(chat));
    let _ = app.update(Message::CloseCard(pane));
    assert_eq!(
        app.terminal_close.as_ref().map(|intent| intent.card),
        Some(pane)
    );
}

/// A chat runs its close beside the queue, so taking its turn and staying open
/// while it closes isn't being turned away.
#[test]
fn a_queued_chat_that_starts_closing_is_not_reported_as_kept() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let chat = failed_chat(&mut app);
    app.queue_card_close(chat, false);
    let _ = app.next_queued_card_close();
    assert!(app.chat_close_owned(chat));
    assert!(app.status.is_none(), "{:?}", errors(&app));
}

/// Close during a return has to be remembered then. Queued, it would come
/// too late to stop the return starting a terminal in the chat's place.
#[test]
fn a_chat_closed_during_its_return_beside_another_removal_starts_no_terminal() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, None);
    let pane = saved_terminal_pane(&mut app);
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    let target = local_return(&app).0;
    app.workspace.request_close(pane).unwrap();
    let _ = app.update(Message::CloseCard(card));
    assert!(matches!(
        app.chat_ui.phases.get(&card),
        Some(Phase::Returning {
            close_requested: true,
            ..
        })
    ));
    let cards = app.durable.cards.len();
    let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
    let _ = app.workspace.cancel_close(pane);
    let _ = app.update(Message::DismissStatus);
    assert!(
        app.chat_close_owned(card),
        "its close runs on its turn: {:?}",
        errors(&app)
    );
    assert_eq!(app.durable.cards.len(), cards, "no terminal took its place");
}

/// The workspace removes one pane at a time, so a second pane that isn't a
/// terminal waits for its turn rather than being refused.
#[test]
fn panes_closed_together_each_get_their_turn() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let first = failed_chat(&mut app);
    let second = failed_chat(&mut app);
    app.chrome_ui.closing_group = vec![first, second];
    drop(app.close_pane_group());
    let _ = app.update(Message::DismissStatus);
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
    assert!(app.terminal_close_in_progress(second));

    let _ = app.update(Message::CommitClose(first));
    let target = app.chat_ui.phases[&first].target();
    let saved = app.chat_return_session(first).unwrap();
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        target,
        saved,
        Ok(()),
    )));
    assert!(!app.card_state(first).unwrap().existence.is_open());

    let target = app.chat_ui.phases[&second].target();
    let saved = app.chat_return_session(second).unwrap();
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        target,
        saved,
        Ok(()),
    )));
    assert!(!app.card_state(second).unwrap().existence.is_open());
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
}

/// Commit can find the pane has become something it mustn't remove. The
/// removal it was holding has to go back, or no other pane can ever close.
#[test]
fn a_chat_close_turned_away_at_commit_gives_up_its_removal() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let other = saved_terminal_pane(&mut app);
    app.chrome_ui.closing_group = vec![card];
    drop(app.close_pane_group());
    assert!(app.workspace.pending_close().is_some());
    app.mark_pane_moving(card);
    let _ = app.update(Message::CommitClose(card));
    assert_eq!(app.workspace.pending_close(), None);
    assert!(app.card_state(card).unwrap().existence.is_open());

    let _ = app.update(Message::CloseCard(other));
    settle_proven_gone(&mut app, other);
}

#[test]
fn repeated_removal_request_reuses_its_reservation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let _ = app.request_card_removal(card);
    let reservation = app.workspace.pending_close();
    let _ = app.request_card_removal(card);
    assert_eq!(app.workspace.pending_close(), reservation);
    assert!(app.status.is_none());
    assert!(app.card_state(card).unwrap().existence.is_open());
    close_chat(&mut app, card);
}

#[test]
fn late_handoff_reply_cannot_consume_a_restart_close() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = unresumable_chat(&mut app);
    let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
    let closing = fresh_close_request(&app, card);
    let _ = app.update(Message::ChatUi(Action::HandoffFinished(
        target,
        Err("old handoff".into()),
    )));
    assert!(matches!(
        app.chat_ui.phases.get(&card),
        Some(Phase::Closing { .. })
    ));
    let _ = app.update(Message::ChatUi(fresh_close_answer(closing, Ok(()))));
    assert!(app.chat_ui.attempt(card).is_some());
}

#[test]
fn local_return_places_terminal_before_a_scope_switch_can_interleave() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let _left = failed_chat(&mut app);
    let card = failed_chat(&mut app);
    let _right = failed_chat(&mut app);
    let before = app.workspace.order().collect::<Vec<_>>();
    let slot = before.iter().position(|id| *id == card).unwrap();
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    let target = local_return(&app).0;
    let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
    let after = app.workspace.order().collect::<Vec<_>>();
    let terminal = terminal_cards(&app)[0];
    let mut expected = before;
    expected[slot] = terminal;
    assert_eq!(after, expected);
    assert_eq!(app.workspace.pending_close(), None);
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert!(app.card_state(terminal).unwrap().existence.is_open());
    app.workspace =
        crate::workspace::Workspace::new(crate::workspace::WorkspaceConfig::default()).unwrap();
    let _ = app.update(Message::CommitClose(card));
    assert!(app.card_state(terminal).unwrap().existence.is_open());
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
}

#[test]
fn deletion_started_during_local_return_keeps_the_source_card() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let source = app.card_state(card).unwrap().clone();
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    let target = local_return(&app).0;
    app.project_ui
        .begin_deleting(PathBuf::from(FIXTURE_DIRECTORY));
    let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
    assert_eq!(app.card_state(card).unwrap().kind, source.kind);
    assert!(app.card_state(card).unwrap().existence.is_open());
    assert!(app.workspace.order().any(|id| id == card));
    assert!(terminal_cards(&app).is_empty());
    assert_eq!(app.workspace.pending_close(), None);
    assert!(!app.chat_ui.transition_pending());
}

/// Retry between a chat's close and its commit starts nothing, and a start
/// still in flight when a card is discarded is given up when it answers
/// rather than leaving a daemon actor no card can reach.
#[test]
fn a_discarded_card_never_strands_a_start() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = failed_chat(&mut app);
    let _ = app.update(Message::CloseCard(card));
    let closing = app.chat_ui.phases[&card].target();
    let saved = app.chat_return_session(card).unwrap();
    let _ = app.update(Message::RetryCard(card));
    assert!(
        matches!(app.chat_ui.phases.get(&card), Some(Phase::Closing { target, restart: None }) if *target == closing),
        "retry cannot replace a pending exact close with a new provider start"
    );
    assert_eq!(app.chat_return_session(card), Some(saved.clone()));
    let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
        closing,
        saved,
        Ok(()),
    )));
    let _ = app.update(Message::RetryCard(card));
    assert!(!app.chat_start_pending(card));
    assert!(!app.card_state(card).unwrap().existence.is_open());

    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    app.chat_discard(card);
    assert!(
        app.chat_ui.transition_pending(),
        "the start is still owed an answer"
    );
    answer_start(&mut app, target, Ok(handle));
    let retry = retired(&mut app, card);
    let _ = app.update(Message::ChatUi(Action::Retired(retry, Ok(()))));
    assert!(!app.chat_ui.transition_pending());
}

/// A close pressed while a live Codex chat returns to its CLI lands on a
/// card that is the CLI with the chat parked behind it. The chat must be
/// closed before the CLI, as a close of that card always does; closing the
/// CLI first reserved the card's removal twice and left it stuck.
#[test]
fn a_codex_chat_closed_during_its_daemon_return_closes_its_cli_too() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    let target = app
        .chat_ui
        .phases
        .values()
        .find_map(|phase| match phase {
            Phase::Returning { target, .. } => Some(*target),
            _ => None,
        })
        .unwrap();
    let _ = app.update(Message::CloseCard(card));
    assert!(matches!(
        app.chat_ui.phases.get(&card),
        Some(Phase::Returning {
            close_requested: true,
            ..
        })
    ));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    let mut session = runtime.session.clone();
    session.launch.session_id = Some(KNOWN_THREAD.into());
    let proof = runtime.handle().unwrap().proof().unwrap();
    let mut cli = return_terminal(&session).unwrap();
    cli.continuity = TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    };
    let _ = app.update(Message::ChatUi(Action::HandoffFinished(target, Ok(cli))));
    // Drive every daemon request to success until nothing is left to do.
    for _ in 0..8 {
        if let Some(intent) = app.terminal_close.clone() {
            assert!(intent.detail.is_none(), "{:?}", intent.detail);
            let _ = app.update(Message::TerminalCloseFinished {
                card: intent.card,
                terminal: intent.terminal,
                proof: intent.proof,
                generation: intent.generation,
                outcome: crate::app::terminal_close::CloseOutcome::Closed,
            });
        } else if app
            .workspace
            .pending_close()
            .is_some_and(|handoff| handoff.unmounting == card)
        {
            let _ = app.update(Message::CommitClose(card));
        } else if let Some(closing) = app.chat_ui.phases.values().find_map(|phase| match phase {
            Phase::Closing { target, .. } => Some(*target),
            _ => None,
        }) {
            let _ = app.update(Message::ChatUi(Action::CloseFinished(closing, Ok(()))));
        } else {
            break;
        }
    }
    assert!(errors(&app).is_empty(), "{:?}", errors(&app));
    assert!(!app.workspace.order().any(|id| id == card));
    assert!(!app.card_state(card).unwrap().existence.is_open());
    assert_eq!(app.workspace.pending_close(), None);
    assert!(app.terminal_close.is_none());
    assert!(!app.chat_has_parked_runtime(card));
    assert!(!app.chat_ui.transition_pending());
}

/// A chat that switches to a shell leaves its channel only for the shell
/// that takes its place; the channel is not empty, so it stays.
#[test]
fn a_chat_replaced_by_its_provider_cli_keeps_its_channel() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let channel = crate::model::ChannelId::new();
    let durable = Arc::make_mut(&mut app.durable);
    durable.channels.push(crate::model::ProjectChannel {
        title_source: Default::default(),
        id: channel,
        project_id: crate::model::ProjectId::new(),
        title: String::new(),
        ordinal: 0,
        shared: false,
        cards: Vec::new(),
        focused: None,
    });
    durable.active_channel = Some(channel);
    app.active_workspace = Some(PathBuf::from(FIXTURE_DIRECTORY));
    let card = failed_chat(&mut app);
    let members = |app: &Cagents| {
        app.durable
            .channels
            .iter()
            .find(|candidate| candidate.id == channel)
            .map(|channel| channel.cards.clone())
    };
    assert_eq!(members(&app), Some(vec![card]), "the chat's only pane");
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    let (target, plan) = local_return(&app);
    let cli = plan.launch.clone();
    assert_eq!(cli.program, "codex");
    let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
    assert_replaced_by_terminal(&mut app, card, &cli);
    let terminal = terminal_cards(&app)[0];
    assert!(
        !app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel),
        "the channel still holds the shell"
    );
    assert_eq!(members(&app), Some(vec![terminal]));
    assert!(app.card_state(terminal).unwrap().existence.is_open());
}

/// A draft whose first message could not start it stays a draft, with
/// the message back in the composer, gives up whatever the start left,
/// and can move to another provider and try again under a new identity.
#[test]
fn a_draft_whose_start_failed_keeps_its_message_and_can_move() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let failed = app.chat_ui.attempt(card).unwrap().session.id;
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure::retry("fixture: executable unavailable")),
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing(), "the card is a draft again");
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert_eq!(
        runtime.error.as_deref(),
        Some("fixture: executable unavailable")
    );
    let retirement = retired(&mut app, card);
    let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));

    let draft = chat_runtime(&app, card).target;
    let _ = app.update(Message::ChatUi(Action::DraftModel(
        draft,
        Provider::Claude,
        None,
    )));
    assert!(!app.chat_start_pending(card));
    submit(&mut app, card);
    let attempt = app.chat_ui.attempt(card).unwrap();
    assert_ne!(attempt.session.id, failed);
    assert_eq!(attempt.session.launch.provider, Provider::Claude);
    assert_eq!(
        attempt.session.launch.recipe.working_directory,
        PathBuf::from(FIXTURE_DIRECTORY)
    );
    assert_eq!(app.workspace.focused(), Some(card));
}

fn fixture_model(id: &str, reasoning: &[&str]) -> chat::Model {
    chat::Model {
        id: id.into(),
        label: id.into(),
        reasoning: reasoning.iter().map(|&effort| effort.into()).collect(),
    }
}

fn reasoning_action_draft(app: &mut Cagents, provider: Provider) -> CardId {
    drop(app.observe_listed_models(
        provider,
        vec![
            fixture_model("fixture-model", &["low", "high"]),
            fixture_model("other-model", &["low", "high"]),
        ],
        None,
    ));
    let card = draft_chat_for_provider(app, provider);
    let target = chat_runtime(app, card).target;
    drop(app.update(Message::ChatUi(Action::DraftModel(
        target,
        provider,
        Some("fixture-model".into()),
    ))));
    drop(app.update(Message::ChatUi(Action::Reasoning(
        target,
        "fixture-model".into(),
        Some("high".into()),
    ))));
    assert_eq!(
        chat_runtime(app, card).snapshot.reasoning.as_deref(),
        Some("high")
    );
    card
}

#[test]
fn reasoning_action_default_clears_the_draft_and_its_next_draft_preference() {
    for provider in [Provider::Claude, Provider::Codex] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = reasoning_action_draft(&mut app, provider);
        let target = chat_runtime(&app, card).target;
        drop(app.update(Message::ChatUi(Action::Reasoning(
            target,
            "fixture-model".into(),
            None,
        ))));
        assert!(chat_runtime(&app, card).snapshot.reasoning.is_none());
        assert_eq!(
            app.chat_ui.last_models.get(&provider),
            Some(&("fixture-model".into(), None))
        );
        drop(app.update(Message::ChatUi(Action::DraftModel(
            target,
            provider,
            Some("other-model".into()),
        ))));
        assert!(chat_runtime(&app, card).snapshot.reasoning.is_none());
        let next = app.chat_draft_snapshot(provider);
        assert_eq!(next.selected_model.as_deref(), Some("other-model"));
        assert!(next.reasoning.is_none());
    }
}

#[test]
fn reasoning_action_implicit_default_alias_preserves_high_and_reset_for_first_submit() {
    for provider in [Provider::Claude, Provider::Codex] {
        for clear in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            drop(app.observe_listed_models(
                provider,
                vec![
                    fixture_model("other-model", &["low"]),
                    fixture_model("default", &["low", "high"]),
                ],
                None,
            ));
            let card = draft_chat_for_provider(&mut app, provider);
            let runtime = chat_runtime(&app, card);
            assert!(runtime.snapshot.selected_model.is_none());
            assert_eq!(
                runtime.chosen_model().model(&runtime.snapshot).unwrap().id,
                "default"
            );
            let target = runtime.target;
            drop(app.update(Message::ChatUi(Action::Reasoning(
                target,
                "other-model".into(),
                Some("low".into()),
            ))));
            assert!(chat_runtime(&app, card).snapshot.selected_model.is_none());
            drop(app.update(Message::ChatUi(Action::Reasoning(
                target,
                "default".into(),
                Some("high".into()),
            ))));
            assert_eq!(
                chat_runtime(&app, card).snapshot.reasoning.as_deref(),
                Some("high")
            );
            if clear {
                drop(app.update(Message::ChatUi(Action::Reasoning(
                    target,
                    "default".into(),
                    None,
                ))));
            }
            let effort = (!clear).then(|| "high".to_owned());
            let next = app.chat_draft_snapshot(provider);
            assert_eq!(next.selected_model.as_deref(), Some("default"));
            assert_eq!(next.reasoning, effort);
            assert_eq!(
                app.chat_ui.last_models.get(&provider),
                Some(&("default".into(), effort.clone()))
            );
            write(&mut app, card, "hello");
            submit(&mut app, card);
            assert_eq!(
                chat_runtime(&app, card).queued.as_ref().unwrap().model,
                Some(("default".into(), effort))
            );
        }
    }
}

#[test]
fn reasoning_action_default_targets_the_staged_live_model_and_is_remembered_on_ack() {
    for provider in [Provider::Claude, Provider::Codex] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat_for_provider(&mut app, provider, None);
        show_chat_snapshot_for_test(
            &mut app,
            card,
            Snapshot {
                generation: 1,
                ready: true,
                busy: true,
                models: vec![
                    fixture_model("running", &["low", "high"]),
                    fixture_model("staged", &["low", "high"]),
                ],
                selected_model: Some("staged".into()),
                reasoning: Some("high".into()),
                ..Snapshot::default()
            },
        );
        let target = chat_runtime(&app, card).target;
        // Full snapshots preserve the separately published running model.
        // Seed that status independently of the staged model fixture.
        Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).active_model =
            Some(chat::ActiveModel {
                id: "running".into(),
                label: "Running".into(),
                reasoning: Some("low".into()),
                default_reasoning: Some("low".into()),
                aliases: Vec::new(),
            });
        app.remember_chat_model(provider, Some("staged".into()), Some("high".into()));
        drop(app.update(Message::ChatUi(Action::Reasoning(
            target,
            "staged".into(),
            None,
        ))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(
            runtime
                .commands
                .in_flight()
                .and_then(|pending| pending.choice.as_ref()),
            Some(&commands::Choice::Model {
                model: "staged".into(),
                reasoning: None,
            })
        );
        assert_eq!(
            runtime.snapshot.active_model.as_ref().unwrap().id,
            "running"
        );
        assert_eq!(
            app.chat_ui.last_models.get(&provider),
            Some(&("staged".into(), Some("high".into())))
        );
        settle(&mut app, card, Ok(()));
        assert_eq!(
            app.chat_ui.last_models.get(&provider),
            Some(&("staged".into(), None))
        );
    }
}

#[test]
fn reasoning_action_rejects_stale_models_and_unlisted_efforts() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = reasoning_action_draft(&mut app, Provider::Codex);
    let target = chat_runtime(&app, card).target;
    for (model, effort) in [("other-model", None), ("fixture-model", Some("unlisted"))] {
        drop(app.update(Message::ChatUi(Action::Reasoning(
            target,
            model.into(),
            effort.map(str::to_owned),
        ))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(
            runtime.snapshot.selected_model.as_deref(),
            Some("fixture-model")
        );
        assert_eq!(runtime.snapshot.reasoning.as_deref(), Some("high"));
        assert!(runtime.commands.is_idle());
        assert_eq!(
            app.chat_ui.last_models.get(&Provider::Codex),
            Some(&("fixture-model".into(), Some("high".into())))
        );
    }
}

#[test]
fn reasoning_action_draft_default_is_sent_when_startup_reports_an_override() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = reasoning_action_draft(&mut app, Provider::Codex);
    let target = chat_runtime(&app, card).target;
    drop(app.update(Message::ChatUi(Action::Reasoning(
        target,
        "fixture-model".into(),
        None,
    ))));
    write(&mut app, card, "hello");
    submit(&mut app, card);
    let target = chat_runtime_attempt(&app, card);
    let handle = fixture_handle(&app, card, None);
    handle.replace_snapshot_for_test(Snapshot {
        models: vec![fixture_model("fixture-model", &["low", "high"])],
        selected_model: Some("fixture-model".into()),
        reasoning: Some("high".into()),
        ready: true,
        ..Snapshot::default()
    });
    answer_start(&mut app, target, Ok(handle));
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Model));
    assert_eq!(
        chat_runtime(&app, card)
            .commands
            .in_flight()
            .unwrap()
            .choice,
        Some(commands::Choice::Model {
            model: "fixture-model".into(),
            reasoning: None,
        })
    );
    settle(&mut app, card, Ok(()));
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
    settle(&mut app, card, Ok(()));
    assert!(chat_runtime(&app, card).queued.is_none());
}

/// A draft with "hello" written on `model` of the Codex catalog, which
/// lists `gpt-a` and `gpt-b`, sent and started on a conversation that
/// lists `listed`; returns the card once the conversation is ready.
fn queued_on(app: &mut Cagents, model: &str, listed: Vec<chat::Model>) -> CardId {
    let _ = app.observe_listed_models(
        Provider::Codex,
        vec![
            fixture_model("gpt-a", &[]),
            fixture_model("gpt-b", &["low", "high"]),
        ],
        None,
    );
    let card = draft_chat(app);
    let target = chat_runtime(app, card).target;
    let _ = app.update(Message::ChatUi(Action::DraftModel(
        target,
        Provider::Codex,
        Some(model.into()),
    )));
    write(app, card, "hello");
    submit(app, card);
    let runtime = chat_runtime(app, card);
    assert_eq!(
        runtime
            .queued
            .as_ref()
            .and_then(|queued| queued.model.clone()),
        Some((model.to_owned(), None))
    );
    assert!(runtime.composer.text().trim().is_empty());
    let target = chat_runtime_attempt(app, card);
    let handle = fixture_handle(app, card, None);
    handle.replace_snapshot_for_test(Snapshot {
        models: listed,
        ready: true,
        ..Snapshot::default()
    });
    answer_start(app, target, Ok(handle));
    card
}

fn pending_step(app: &Cagents, card: CardId) -> Option<QueuedStep> {
    chat_runtime(app, card)
        .commands
        .in_flight()
        .and_then(|pending| pending.queued)
}

/// A first message chosen for a model goes out once the conversation has
/// switched to it, and not before.
#[test]
fn a_first_message_goes_out_on_the_model_chosen_for_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let listed = vec![fixture_model("gpt-a", &[]), fixture_model("gpt-b", &[])];
    let card = queued_on(&mut app, "gpt-b", listed);
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Model));
    settle(&mut app, card, Ok(()));
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
    settle(&mut app, card, Ok(()));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.queued.is_none());
    assert!(runtime.commands.is_idle());
    assert!(runtime.failures.is_empty(), "{:?}", runtime.failures);
}

/// A model the conversation turns out not to offer is not waited for:
/// the message goes to its default, and the transcript says so.
#[test]
fn a_first_message_on_a_model_the_conversation_lacks_goes_to_its_default() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = queued_on(&mut app, "gpt-b", vec![fixture_model("gpt-a", &[])]);
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
    let runtime = chat_runtime(&app, card);
    assert!(
        runtime
            .failures
            .iter()
            .any(|note| note.text
                == "This conversation does not offer gpt-b, so this message will use its default model."),
        "{:?}",
        runtime.failures
    );
}

/// A model switch that Claude did not take, or has not confirmed, says
/// so, and the model stays remembered: delivery failed, not the model.
#[test]
fn an_undelivered_model_switch_is_reported_and_the_model_kept() {
    for (outcome, said) in [
        (
            chat::failure::delivery::UNACKNOWLEDGED_INPUT,
            "hasn't confirmed the switch to gpt-b yet, so this message might not use it",
        ),
        (
            chat::failure::delivery::UNCONFIRMED_INPUT,
            "didn't take the switch to gpt-b, so this message won't use it",
        ),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let listed = vec![fixture_model("gpt-a", &[]), fixture_model("gpt-b", &[])];
        let card = queued_on(&mut app, "gpt-b", listed);
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Model));
        let remembered = app.chat_ui.last_models.get(&Provider::Codex).cloned();
        assert!(remembered.is_some());
        settle(&mut app, card, Err(outcome.into()));
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
        let notes = &chat_runtime(&app, card).failures;
        assert!(
            notes.iter().any(|note| note.text.contains(said)),
            "{notes:?}"
        );
        assert!(
            notes.iter().all(|note| !note.text.contains(outcome)),
            "{notes:?}"
        );
        assert_eq!(
            app.chat_ui.last_models.get(&Provider::Codex).cloned(),
            remembered
        );
    }
    // Any other refusal gives the model up, and its reason goes to the
    // log rather than the transcript.
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let listed = vec![fixture_model("gpt-a", &[]), fixture_model("gpt-b", &[])];
    let card = queued_on(&mut app, "gpt-b", listed);
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Model));
    settle(
        &mut app,
        card,
        Err("model gpt-b requires another plan".into()),
    );
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
    let notes = &chat_runtime(&app, card).failures;
    assert!(
        notes
            .iter()
            .any(|note| note.text == "gpt-b couldn't be chosen, so this message won't use it."),
        "{notes:?}"
    );
    assert!(
        notes.iter().all(|note| !note.text.contains("another plan")),
        "{notes:?}"
    );
    assert!(!app.chat_ui.last_models.contains_key(&Provider::Codex));
}

/// A conversation that has not listed its models yet is given time to,
/// rather than losing the reader's choice at once.
#[test]
fn a_first_message_waits_for_the_conversation_to_list_its_models() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = queued_on(&mut app, "gpt-b", Vec::new());
    assert_eq!(pending_step(&app, card), None);
    let runtime = chat_runtime(&app, card);
    let queued = runtime.queued.as_ref().unwrap();
    assert!(queued.rechecking, "it looks again once the wait is over");
    assert!(queued.model.is_some());

    let handle = runtime.handle().unwrap().clone();
    let target = runtime.target;
    handle.replace_snapshot_for_test(Snapshot {
        models: vec![fixture_model("gpt-b", &[])],
        ready: true,
        ..Snapshot::default()
    });
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Model));
}

/// Nothing is sent before the provider takes commands, and the snapshot
/// that says it does sends the message; the hidden card of a message
/// still waiting keeps that snapshot coming.
#[test]
fn a_first_message_waits_for_its_provider_to_be_ready() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    handle.replace_snapshot_for_test(Snapshot::default());
    answer_start(&mut app, target, Ok(handle.clone()));
    assert_eq!(pending_step(&app, card), None);
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert!(runtime.queued.is_some());
    let _ = runtime.set_visible(false);
    assert!(
        handle.is_visible_for_test(),
        "a hidden card waiting to send still hears from its provider"
    );

    handle.replace_snapshot_for_test(Snapshot {
        ready: true,
        items: vec![
            chat::Item {
                id: "provider-introduction".into(),
                role: chat::Role::Assistant,
                content: text(&"hidden body".repeat(64 * 1024)),
                started_at: None,
                at: None,
            }
            .into(),
        ],
        ..Snapshot::default()
    });
    assert_eq!(
        handle.snapshot().transcript_bytes(),
        0,
        "control polling does not retain hidden history"
    );
    let target = chat_runtime(&app, card).target;
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
    assert!(
        chat_runtime(&app, card).snapshot.ready,
        "hidden runtime receives ready metadata"
    );
    assert_eq!(chat_runtime(&app, card).snapshot.transcript_bytes(), 0);
    settle(&mut app, card, Ok(()));
    assert!(
        !handle.is_visible_for_test(),
        "once sent, the hidden card goes quiet again"
    );
}

/// A ready provider's refusal of the first message is its answer, and
/// is never met by sending the message again: the card goes back to its
/// draft, with the message where it was written, and the conversation
/// that holds nothing is given up.
#[test]
fn a_refused_first_message_returns_the_card_to_its_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let started = app.chat_ui.attempt(card).unwrap().session.clone();
    let handle = fixture_handle(&app, card, None);
    answer_start(&mut app, target, Ok(handle));
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
    settle(&mut app, card, Err("fixture: refused".into()));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing(), "the card is a draft again");
    assert!(runtime.commands.is_idle());
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert_eq!(runtime.error.as_deref(), Some("fixture: refused"));
    assert_eq!(
        runtime.session, started,
        "the draft its start was asked for"
    );
    assert!(runtime.session.is_unstarted());
    retired(&mut app, card);
    let CardKind::Chat(saved) = &app.card_state(card).unwrap().kind else {
        panic!("the card is a chat");
    };
    assert_eq!(saved, &started, "a restore reopens it as a draft");
}

/// A message the transcript shows arriving was taken, whatever its
/// sending reported, so it is neither sent again nor handed back. A
/// conversation that is merely busy does not show that.
#[test]
fn a_first_message_that_arrived_is_not_handed_back() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    answer_start(&mut app, target, Ok(handle.clone()));
    handle.replace_snapshot_for_test(Snapshot {
        ready: true,
        busy: true,
        ..Snapshot::default()
    });
    settle(
        &mut app,
        card,
        Err("fixture: acknowledgement timed out".into()),
    );
    assert!(chat_runtime(&app, card).choosing(), "busy is not arrival");

    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    answer_start(&mut app, target, Ok(handle.clone()));
    let _ = app.chat_runtime_mut(target).unwrap().set_visible(false);
    handle.replace_snapshot_for_test(Snapshot {
        ready: true,
        items: vec![
            chat::Item {
                id: "prompt".into(),
                role: chat::Role::User,
                content: text("hello"),
                started_at: None,
                at: None,
            }
            .into(),
        ],
        ..Snapshot::default()
    });
    assert!(
        handle.snapshot().items.is_empty(),
        "the hidden body was actually evicted"
    );
    assert!(
        handle.has_observed_user_item(),
        "receipt evidence retains no prompt text"
    );
    settle(
        &mut app,
        card,
        Err("fixture: acknowledgement timed out".into()),
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.queued.is_none());
    assert!(!runtime.is_draft());
    assert!(runtime.composer.text().trim().is_empty());
    assert!(
        runtime
            .failures
            .iter()
            .any(|note| note.text.contains("acknowledgement timed out")),
        "{:?}",
        runtime.failures
    );
}

/// A provider that cannot start ends its conversation, and the card goes
/// back to its draft saying why; a warning it carries on past does not.
#[test]
fn a_provider_that_cannot_start_returns_the_card_to_its_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    handle.replace_snapshot_for_test(Snapshot {
        error: Some("fixture: deprecated setting".into()),
        ..Snapshot::default()
    });
    answer_start(&mut app, target, Ok(handle.clone()));
    let target = chat_runtime(&app, card).target;
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
    assert!(
        chat_runtime(&app, card).queued.is_some(),
        "a warning is waited past"
    );

    handle.replace_snapshot_for_test(Snapshot {
        error: Some("fixture: no state".into()),
        exited: true,
        ..Snapshot::default()
    });
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing());
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert_eq!(
        runtime.error.as_deref(),
        Some(
            "The conversation stopped before it took the first message. Its last report: fixture: no state"
        )
    );
    retired(&mut app, card);
}

/// A card that goes back to its draft keeps nothing of the conversation
/// it gave up, not even the notes its first message's sending left.
#[test]
fn a_card_back_to_its_draft_keeps_nothing_of_its_conversation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = queued_on(&mut app, "gpt-b", vec![fixture_model("gpt-a", &[])]);
    assert!(!chat_runtime(&app, card).failures.is_empty());
    settle(&mut app, card, Err("fixture: refused".into()));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing());
    assert!(runtime.failures.is_empty(), "{:?}", runtime.failures);
    assert!(runtime.turns.is_empty());
}

/// A Codex conversation can be handed to its terminal mid-command, but
/// not while its first message waits: the terminal would never send it.
#[test]
fn a_first_message_waiting_keeps_its_codex_chat_from_its_terminal() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, Some("thread"));
    handle.replace_snapshot_for_test(Snapshot {
        session_id: Some("thread".into()),
        ..Snapshot::default()
    });
    answer_start(&mut app, target, Ok(handle));
    assert_eq!(
        chat_runtime(&app, card).session.launch.provider,
        Provider::Codex
    );
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    assert!(
        !app.chat_ui
            .phases
            .values()
            .any(|phase| matches!(phase, Phase::Returning { .. })),
        "no handoff was asked for"
    );
    assert!(chat_runtime(&app, card).queued.is_some());
    assert_eq!(errors(&app), [QUEUED_BLOCKER]);
}

/// A conversation that goes away before it takes the first message
/// leaves the card a draft, not a dead conversation with nothing in it.
#[test]
fn a_conversation_lost_before_its_first_message_leaves_a_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    handle.replace_snapshot_for_test(Snapshot::default());
    answer_start(&mut app, target, Ok(handle));
    let target = chat_runtime(&app, card).target;
    let _ = app.update(Message::ChatUi(Action::Wake(
        target,
        Wake::Disconnected("fixture: gone".into()),
    )));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing());
    assert!(!runtime.disconnected);
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert_eq!(runtime.error.as_deref(), Some("fixture: gone"));
    retired(&mut app, card);
}

/// A first message can be taken back until it is on its way: while its
/// conversation starts, and after, while it waits on the provider.
#[test]
fn a_first_message_can_be_taken_back_until_it_is_on_its_way() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, started) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    let target = chat_runtime(&app, card).target;
    let _ = app.update(Message::ChatUi(Action::CancelQueued(target)));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing());
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert!(!app.chat_start_pending(card));
    // What the abandoned start produces is given up when it lands.
    answer_start(&mut app, started, Ok(handle));
    retired(&mut app, card);
    assert!(chat_runtime(&app, card).is_draft());

    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    handle.replace_snapshot_for_test(Snapshot::default());
    answer_start(&mut app, target, Ok(handle));
    let target = chat_runtime(&app, card).target;
    let _ = app.update(Message::ChatUi(Action::CancelQueued(target)));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing());
    assert_eq!(runtime.composer.text().trim(), "hello");
    retired(&mut app, card);

    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let handle = fixture_handle(&app, card, None);
    answer_start(&mut app, target, Ok(handle));
    assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
    let target = chat_runtime(&app, card).target;
    let _ = app.update(Message::ChatUi(Action::CancelQueued(target)));
    assert_eq!(
        pending_step(&app, card),
        Some(QueuedStep::Prompt),
        "a message on its way is not taken back"
    );
}

/// A model is remembered for new drafts once its provider takes it, not
/// when it is asked for.
#[test]
fn a_model_is_remembered_once_its_conversation_takes_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, None);
    show_chat_snapshot_for_test(
        &mut app,
        card,
        Snapshot {
            generation: 1,
            models: vec![fixture_model("gpt-b", &[])],
            ready: true,
            ..Snapshot::default()
        },
    );
    let target = chat_runtime(&app, card).target;
    let _ = app.update(Message::ChatUi(Action::Model(target, "gpt-b".into())));
    assert!(!app.chat_ui.last_models.contains_key(&Provider::Codex));
    settle(&mut app, card, Err("fixture: refused".into()));
    assert!(!app.chat_ui.last_models.contains_key(&Provider::Codex));
    let _ = app.update(Message::ChatUi(Action::Model(target, "gpt-b".into())));
    settle(&mut app, card, Ok(()));
    assert_eq!(
        app.chat_ui.last_models.get(&Provider::Codex),
        Some(&("gpt-b".to_owned(), None))
    );
}

#[test]
fn removed_account_without_available_alternatives_still_offers_accounts() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let session = chat_runtime(&app, card).session.clone();
    assert!(
        app.chat_accounts(session.launch.provider, &session.launch.recipe)
            .is_none()
    );
    app.chat_ui.removed_accounts.insert(card);
    assert!(app.account_recovery_controls(card).is_some());
    let _ = app.update(Message::AccountUi(account_ui::Action::OpenPane));
    assert!(app.utility_card(&CardKind::Accounts).is_some());
    assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(session));
}

#[test]
fn a_chat_view_reads_its_account_choices_without_resolving_account_homes() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let work = app.account_ui.add_managed_fixture(Provider::Codex, 106);
    // An update ends with the choices its frame reads.
    let _ = app.update(Message::AccountUi(account_ui::Action::Tick));
    let launch = chat_runtime(&app, card).session.launch.clone();
    let offered = |app: &Cagents| -> Vec<crate::account::AccountId> {
        app.chat_accounts(launch.provider, &launch.recipe)
            .expect("two accounts to choose between")
            .choices
            .iter()
            .map(|choice| choice.id.clone())
            .collect()
    };
    let resolved = account_ui::home_resolutions();
    for _ in 0..3 {
        drop(app.chat_view(card));
    }
    assert_eq!(
        offered(&app),
        [Account::system(Provider::Codex).id(), work.clone()]
    );
    assert_eq!(
        account_ui::home_resolutions(),
        resolved,
        "a frame resolves no account's home"
    );

    // Another account is offered as soon as it is known, with no frame
    // between the change and the read.
    let personal = app.account_ui.add_managed_fixture(Provider::Codex, 107);
    assert_eq!(
        offered(&app),
        [Account::system(Provider::Codex).id(), work, personal]
    );
}

#[test]
fn a_conversation_whose_account_list_lost_its_kept_home_offers_to_restore_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let directory = tempfile::tempdir().unwrap();
    let homes = crate::account::home::Homes::at(directory.path().join("accounts"));
    let (key, home) = homes.create(Provider::Codex, None).unwrap();
    app.account_ui.use_homes(homes.clone());
    let refuse = |app: &mut Cagents, source: &chat::Session| {
        app.install_chat_session(card, source.clone());
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
        let _ = app.open_chat(card, source.clone(), chat::Intent::Restore);
        let target = chat_runtime_attempt(app, card);
        answer_start(
            app,
            target,
            Err(chat::StartFailure {
                message: "This Codex account isn't in the account list".into(),
                recovery: chat::Recovery::Account,
            }),
        );
    };

    // A home whose files are gone is plainly removed: no restore offer.
    let mut gone = chat_runtime(&app, card).session.clone();
    gone.launch.recipe.environment.insert(
        "CODEX_HOME".into(),
        homes
            .path(Provider::Codex, uuid::Uuid::new_v4())
            .to_str()
            .unwrap()
            .into(),
    );
    refuse(&mut app, &gone);
    assert!(app.chat_ui.removed_accounts.contains(&card));
    assert!(!app.chat_ui.restorable_accounts.contains_key(&card));

    let restored = || {
        let mut registry = crate::account::registry::Registry::default();
        assert!(
            registry
                .restore(
                    crate::account::registry::Registered {
                        key,
                        provider: Provider::Codex,
                        linked_home: None,
                        label: None,
                        email: Some("plan@example.com".into()),
                        organization: None,
                        added_at: 1,
                    },
                    &[],
                    &homes,
                )
                .is_none()
        );
        registry
    };
    // This window's copy of the list still names the account: the list
    // was reset under it. The daemon's refusal is the current answer.
    app.account_ui.set_registry(restored());
    let mut source = gone.clone();
    source
        .launch
        .recipe
        .environment
        .insert("CODEX_HOME".into(), home.to_str().unwrap().into());
    refuse(&mut app, &source);
    assert_eq!(
        app.chat_ui.restorable_accounts.get(&card).map(|offer| (
            offer.provider,
            offer.key,
            offer.restoring
        )),
        Some((Provider::Codex, key, true)),
        "the first refusal asks the device to restore the account at once"
    );
    assert!(app.account_recovery_controls(card).is_some());
    let _ = app.update(Message::ChatUi(Action::RestoreAccount(card)));
    // The device refuses - its provider reports no sign-in, say. The
    // offer stays, with what refused it.
    let _ = app.update(Message::ChatUi(Action::AccountRestored(
        card,
        Err("Codex reports no sign-in for this account, so it was not restored.".into()),
    )));
    assert_eq!(
        app.chat_ui.restorable_accounts.get(&card).map(|offer| (
            offer.provider,
            offer.key,
            offer.restoring
        )),
        Some((Provider::Codex, key, false))
    );
    assert_eq!(
        app.chat_ui.restorable_accounts[&card].failure.as_deref(),
        Some("Codex reports no sign-in for this account, so it was not restored."),
        "the card says why, in place of the offer"
    );
    assert!(app.chat_ui.removed_accounts.contains(&card));
    assert!(
        app.chat_ui.attempt(card).is_none(),
        "nothing starts on a refusal"
    );

    let _ = app.update(Message::ChatUi(Action::RestoreAccount(card)));
    assert_eq!(
        app.chat_ui.restorable_accounts.get(&card).map(|offer| (
            offer.provider,
            offer.key,
            offer.restoring
        )),
        Some((Provider::Codex, key, true)),
        "asking again shows the check under way"
    );
    let _ = app.update(Message::ChatUi(Action::AccountRestored(
        card,
        Ok(crate::account::remote::RestoredHome {
            registry: restored(),
            restored: crate::account::registry::Restored::Listed,
        }),
    )));
    assert!(!app.chat_ui.restorable_accounts.contains_key(&card));
    assert!(!app.chat_ui.removed_accounts.contains(&card));
    assert_eq!(
        app.chat_ui
            .attempt(card)
            .unwrap()
            .session
            .launch
            .recipe
            .environment
            .get("CODEX_HOME"),
        Some(&home.to_str().unwrap().to_owned()),
        "the conversation starts again on its own account"
    );
    assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(source));
}

#[test]
fn a_draft_whose_account_list_lost_its_kept_home_restores_it_and_sends() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let directory = tempfile::tempdir().unwrap();
    let homes = crate::account::home::Homes::at(directory.path().join("accounts"));
    let (key, home) = homes.create(Provider::Codex, None).unwrap();
    app.account_ui.use_homes(homes);
    let (card, target) = starting_chat(&mut app);
    let Some(Phase::Starting(attempt)) = app.chat_ui.phases.get_mut(&card) else {
        panic!("the draft is starting");
    };
    let failed = attempt.session.id;
    // The refusal reads its account from the attempt, so naming the kept
    // home there stands for the draft having chosen it.
    attempt
        .session
        .launch
        .recipe
        .environment
        .insert("CODEX_HOME".into(), home.to_str().unwrap().into());
    let refuse = |app: &mut Cagents, target| {
        answer_start(
            app,
            target,
            Err(chat::StartFailure {
                message: "This Codex account isn't in the account list".into(),
                recovery: chat::Recovery::Account,
            }),
        );
    };
    refuse(&mut app, target);
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing(), "the card is a draft again");
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert_eq!(
        app.chat_ui.restorable_accounts.get(&card).map(|offer| (
            offer.provider,
            offer.key,
            offer.restoring
        )),
        Some((Provider::Codex, key, true)),
        "the refusal asks the device to restore the account at once"
    );
    let retirement = retired(&mut app, card);
    let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
    let reason = "Codex reports no sign-in for this account, so it was not restored.";
    let _ = app.update(Message::ChatUi(Action::AccountRestored(
        card,
        Err(reason.into()),
    )));
    assert!(
        chat_runtime(&app, card).error.as_deref().is_some_and(
            |error| error.starts_with(reason) && error.contains("Choose another account")
        ),
        "the draft says why it was not restored, and what to do instead"
    );
    assert!(!app.chat_start_pending(card), "and sends nothing");
    let _ = app.update(Message::ChatUi(Action::RestoreAccount(card)));
    // Sending saves the draft first; give it somewhere to.
    let storage = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        storage.path().join("restored-draft.sqlite3"),
        crate::model::BootId::new("restored-draft-test"),
    )
    .unwrap();
    app.session = Some(writer);
    let task = app.update(Message::ChatUi(Action::AccountRestored(
        card,
        Ok(crate::account::remote::RestoredHome {
            registry: crate::account::registry::Registry::default(),
            restored: crate::account::registry::Restored::AlreadyRegistered,
        }),
    )));
    settle_first_start_checkpoint(&mut app, card, task);
    assert!(!app.chat_ui.restorable_accounts.contains_key(&card));
    assert_eq!(
        chat_runtime(&app, card).error,
        None,
        "the refusal no longer applies"
    );
    let attempt = app
        .chat_ui
        .attempt(card)
        .expect("the message the refusal answered is sent again");
    assert_ne!(attempt.session.id, failed, "under an identity of its own");
    assert_eq!(
        attempt.session.launch.recipe.environment,
        chat_runtime(&app, card).session.launch.recipe.environment,
        "on the account the draft names"
    );
    drop(app.session.take());
}

#[test]
fn a_draft_restored_before_its_refused_start_retires_still_sends_once() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let directory = tempfile::tempdir().unwrap();
    let homes = crate::account::home::Homes::at(directory.path().join("accounts"));
    let (_, home) = homes.create(Provider::Codex, None).unwrap();
    app.account_ui.use_homes(homes);
    let (card, target) = starting_chat(&mut app);
    let Some(Phase::Starting(attempt)) = app.chat_ui.phases.get_mut(&card) else {
        panic!("the draft is starting");
    };
    let failed = attempt.session.id;
    attempt
        .session
        .launch
        .recipe
        .environment
        .insert("CODEX_HOME".into(), home.to_str().unwrap().into());
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure {
            message: "This Codex account isn't in the account list".into(),
            recovery: chat::Recovery::Account,
        }),
    );
    let retirement = retired(&mut app, card);
    let storage = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        storage.path().join("restored-draft.sqlite3"),
        crate::model::BootId::new("restored-draft-order-test"),
    )
    .unwrap();
    app.session = Some(writer);
    // The device answers the restore before the refused start's cleanup.
    let task = app.update(Message::ChatUi(Action::AccountRestored(
        card,
        Ok(crate::account::remote::RestoredHome {
            registry: crate::account::registry::Registry::default(),
            restored: crate::account::registry::Restored::Listed,
        }),
    )));
    settle_first_start_checkpoint(&mut app, card, task);
    let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
    let attempt = app
        .chat_ui
        .attempt(card)
        .expect("the message is sent once the account is back");
    assert_ne!(attempt.session.id, failed, "under an identity of its own");
    assert!(chat_runtime(&app, card).composer.text().trim().is_empty());
    drop(app.session.take());
}

#[test]
fn a_refused_draft_the_reader_changed_is_not_sent_when_its_account_is_restored() {
    type Change = fn(&mut Runtime, &std::path::Path);
    let changes: [(&str, Change); 2] = [
        ("edited message", |runtime, _| {
            runtime.replace_draft(text_editor::Content::with_text("hello, but different"));
        }),
        ("other account", |runtime, other| {
            runtime
                .session
                .launch
                .recipe
                .environment
                .insert("CODEX_HOME".into(), other.to_str().unwrap().into());
        }),
    ];
    for (change, apply) in changes {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let homes = crate::account::home::Homes::at(directory.path().join("accounts"));
        let (_, home) = homes.create(Provider::Codex, None).unwrap();
        let (_, other) = homes.create(Provider::Codex, None).unwrap();
        app.account_ui.use_homes(homes);
        let (card, target) = starting_chat(&mut app);
        let Some(Phase::Starting(attempt)) = app.chat_ui.phases.get_mut(&card) else {
            panic!("the draft is starting");
        };
        attempt
            .session
            .launch
            .recipe
            .environment
            .insert("CODEX_HOME".into(), home.to_str().unwrap().into());
        answer_start(
            &mut app,
            target,
            Err(chat::StartFailure {
                message: "This Codex account isn't in the account list".into(),
                recovery: chat::Recovery::Account,
            }),
        );
        assert!(app.chat_ui.restorable_accounts[&card].resend.is_some());
        let retirement = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("the card is a draft");
        };
        apply(runtime, &other);
        let _ = app.update(Message::ChatUi(Action::AccountRestored(
            card,
            Ok(crate::account::remote::RestoredHome {
                registry: crate::account::registry::Registry::default(),
                restored: crate::account::registry::Restored::Listed,
            }),
        )));
        assert!(!app.chat_ui.restorable_accounts.contains_key(&card));
        assert!(
            !app.chat_start_pending(card),
            "{change}: nothing is sent the reader did not ask for"
        );
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing() && runtime.error.is_none(), "{change}");
    }
}

#[test]
fn upgraded_identity_is_installed_only_after_its_exact_durable_receipt() {
    for (
        stale_reply,
        codex_resume,
        legacy_paired,
        learns_identity,
        paired_recipe,
        writer_refused,
    ) in [
        (false, false, false, false, 0, false),
        (true, false, false, false, 0, false),
        (false, true, false, false, 0, false),
        (true, true, false, false, 0, false),
        (false, false, true, false, 0, false),
        (true, false, true, false, 0, false),
        (false, false, true, true, 0, false),
        (true, false, true, true, 0, false),
        (false, false, true, false, 1, false),
        (true, false, true, false, 1, false),
        (false, false, true, false, 2, false),
        (true, false, true, false, 2, false),
        (false, false, true, false, 1, true),
        (false, false, true, false, 2, true),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        // This assertion observes the presented history. Hidden panes
        // intentionally keep no rendered snapshot until they are shown.
        app.workspace.resize_viewport(1_600.0);
        assert_eq!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        let mut source = chat_runtime(&app, card).session.clone();
        if codex_resume || legacy_paired {
            source.launch.provider = Provider::Codex;
            source.launch.recipe.program = "codex".into();
            source.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
            source.launch.session_id = None;
            let allocator = source.proof.clone().unwrap();
            assert!(
                source.claim_owned_codex_listener(
                    std::path::Path::new("/run/test-native"),
                    &allocator
                )
            );
            source.launch.session_id = Some(KNOWN_THREAD.into());
            if !legacy_paired {
                source.native_continuity = chat::NativeContinuity::Durable {
                    provider_version: "0.100.0".into(),
                };
            } else {
                let proof = source.proof.clone().unwrap();
                source.terminal = Some(Box::new(TerminalSession {
                    id: source.id,
                    launch: source.launch.terminal_recipe().unwrap(),
                    continuity: TerminalContinuity::Live {
                        host_session_id: proof.host_session_id,
                        boot_id: proof.boot_id,
                        host_incarnation: proof.host_incarnation,
                    },
                    transcript_path: None,
                    resume_hint: None,
                    owned_codex_listener: None,
                    recovery_origin: None,
                    predecessor: None,
                }));
            }
            if learns_identity {
                source.launch.session_id = None;
            }
            if paired_recipe != 0 {
                // An old converted chat keeps its fresh issued recipe,
                // while its terminal can have been a fresh CLI or shell.
                // The daemon owns no private listener for this pairing.
                source.reset_owned_codex_listener();
                source.launch.recipe.arguments = vec![
                    "--remote".into(),
                    "unix:///run/test-paired-codex.sock".into(),
                ];
                source
                    .launch
                    .recipe
                    .environment
                    .insert("CODEX_HOME".into(), "/private/test-codex-home".into());
                let mut issued = source.launch.recipe.clone();
                if paired_recipe == 2 {
                    issued.program = "/bin/bash".into();
                    issued.arguments = vec!["--login".into()];
                }
                let terminal = source.terminal.as_mut().unwrap();
                terminal.launch = issued;
                terminal.transcript_path = Some("/private/test-paired-transcript".into());
            }
            app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
        }
        app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot-test"),
        )
        .unwrap();
        app.session = Some(writer);
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(10))
            .unwrap();
        if writer_refused {
            app.session
                .as_ref()
                .unwrap()
                .operation_store()
                .shutdown()
                .unwrap();
        }
        let mut replacement = source.clone();
        if learns_identity {
            replacement.launch.session_id = Some(KNOWN_THREAD.into());
        }
        replacement.proof.as_mut().unwrap().host_incarnation = "upgraded-daemon".into();
        if codex_resume {
            replacement
                .launch
                .recipe
                .arguments
                .splice(0..0, ["resume".into(), KNOWN_THREAD.into()]);
        } else if legacy_paired {
            let proof = replacement.proof.clone().unwrap();
            replacement.terminal.as_mut().unwrap().continuity = TerminalContinuity::Live {
                host_session_id: proof.host_session_id,
                boot_id: proof.boot_id,
                host_incarnation: proof.host_incarnation,
            };
        }
        let history = vec![Arc::new(chat::Item {
            id: "paired-upgrade-existing-answer".into(),
            role: chat::Role::Assistant,
            content: text("Existing paired answer survives upgrade"),
            started_at: None,
            at: None,
        })];
        let snapshot = Snapshot {
            session_id: replacement.launch.session_id.clone(),
            ready: true,
            items: history.clone(),
            ..Snapshot::default()
        };
        let handle = Arc::new(if legacy_paired {
            ClientHandle::fixture_upgrade_handshake(source.clone(), replacement.clone(), snapshot)
                .unwrap()
        } else {
            ClientHandle::fixture_upgrade(source.clone(), replacement.clone(), snapshot)
        });
        let target = app.chat_ui.next_target(card);
        app.chat_ui.phases.insert(
            card,
            Phase::Starting(Box::new(Attempt {
                target,
                intent: chat::Intent::Restore,
                session: source.clone(),
                recovery: None,
                upgrade_committed: false,
                resume_committed: false,
                upgrade_admission: None,
            })),
        );
        app.cards.remove(&card);
        let task = app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        )));
        assert!(app.chat_start_pending(card));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        assert!(!app.cards.contains_key(&card));
        let receipt = account_commit_reply(task);
        if writer_refused {
            assert!(matches!(&receipt,
                Message::ChatUi(Action::UpgradeCommitted(_, _, Err(reason)))
                    if reason == &crate::persistence::StoreError::Closed.to_string()));
            let _ = app.update(receipt);
            assert!(!app.chat_start_pending(card));
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
            let database = rusqlite::Connection::open_with_flags(
                directory.path().join("state.sqlite3"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let stored: String = database
                .query_row(
                    "SELECT content_json FROM cards WHERE card_id=?1",
                    [card.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<chat::Session>(&stored).unwrap(),
                source
            );
            continue;
        }
        assert!(matches!(
            receipt,
            Message::ChatUi(Action::UpgradeCommitted(_, _, Ok(())))
        ));
        if stale_reply {
            let next = app.chat_ui.next_target(card);
            app.chat_ui.phases.insert(
                card,
                Phase::Starting(Box::new(Attempt {
                    target: next,
                    intent: chat::Intent::Restore,
                    session: source.clone(),
                    recovery: None,
                    upgrade_committed: false,
                    resume_committed: false,
                    upgrade_admission: None,
                })),
            );
            let _ = app.update(receipt);
            assert_eq!(app.chat_ui.attempt(card).unwrap().target, next);
            assert!(!app.chat_ui.attempt(card).unwrap().upgrade_committed);
            assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(source));
            assert!(!app.cards.contains_key(&card));
        } else {
            let _ = app.update(receipt);
            assert!(!app.chat_start_pending(card));
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(replacement.clone())
            );
            assert_eq!(chat_runtime(&app, card).session, replacement.clone());
            assert_eq!(chat_runtime(&app, card).snapshot.items, history);
            app.persist();
            let writer = app.session.as_ref().unwrap();
            writer.flush(Duration::from_secs(10)).unwrap();
            assert_eq!(writer.last_error(), None);
            assert_eq!(
                writer
                    .operation_store()
                    .load_card(card, None)
                    .unwrap()
                    .unwrap()
                    .kind,
                CardKind::Chat(replacement)
            );
        }
    }
}

#[test]
fn account_recovery_keeps_the_saved_identity_until_exact_confirmation() {
    for close_before_ack in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let mut source = chat_runtime(&app, card).session.clone();
        source
            .launch
            .recipe
            .environment
            .insert("CODEX_HOME".into(), "/removed-account".into());
        app.install_chat_session(card, source.clone());
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot-test"),
        )
        .unwrap();
        app.session = Some(writer);
        app.persist();
        let saved = |app: &Cagents| {
            let writer = app.session.as_ref().unwrap();
            writer.flush(Duration::from_secs(10)).unwrap();
            assert_eq!(writer.last_error(), None);
            writer
                .operation_store()
                .load_card(card, None)
                .unwrap()
                .unwrap()
                .kind
        };
        assert_eq!(saved(&app), CardKind::Chat(source.clone()));
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
        let _ = app.open_chat(card, source.clone(), chat::Intent::Restore);
        let target = chat_runtime_attempt(&app, card);
        answer_start(
            &mut app,
            target,
            Err(chat::StartFailure {
                message: "The saved account was removed".into(),
                recovery: chat::Recovery::Account,
            }),
        );
        assert!(app.chat_ui.removed_accounts.contains(&card));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        let system = Account::system(Provider::Codex).id();
        let _ = app.update(Message::ChatUi(Action::RecoverAccount(
            card,
            system.clone(),
        )));
        let target = chat_runtime_attempt(&app, card);
        assert_ne!(app.chat_ui.attempt(card).unwrap().session.id, source.id);
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        let _ = app.update(Message::ChatUi(Action::RecoverAccount(
            card,
            system.clone(),
        )));
        assert_eq!(
            chat_runtime_attempt(&app, card),
            target,
            "double click does not create another candidate"
        );
        answer_start(
            &mut app,
            target,
            Err(chat::StartFailure::retry(
                "Conversation unavailable on this account",
            )),
        );
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        assert!(app.chat_ui.removed_accounts.contains(&card));
        let retirement = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
        assert_eq!(saved(&app), CardKind::Chat(source.clone()));
        let _ = app.update(Message::ChatUi(Action::RecoverAccount(
            card,
            system.clone(),
        )));
        let target = chat_runtime_attempt(&app, card);
        let handle = fixture_handle(&app, card, Some("different-conversation"));
        answer_start(&mut app, target, Ok(handle));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        let retirement = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
        let _ = app.update(Message::ChatUi(Action::RecoverAccount(
            card,
            system.clone(),
        )));
        let target = chat_runtime_attempt(&app, card);
        let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
        let mut competing = handle.session();
        competing.reset_owned_codex_listener();
        competing.id = TerminalId::new();
        // A different durable owner wins before this candidate can commit.
        let store = app.session.as_ref().unwrap().operation_store();
        store
            .rebind_chat_account(card, &source, &competing)
            .unwrap();
        let task = app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        )));
        assert!(app.chat_start_pending(card));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        let reply = account_commit_reply(task);
        assert!(matches!(
            &reply,
            Message::ChatUi(Action::AccountCommitted(_, _, Err(_)))
        ));
        let _ = app.update(reply);
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().kind,
            CardKind::Chat(competing.clone())
        );
        assert!(app.chat_ui.removed_accounts.contains(&card));
        let retirement = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
        store
            .rebind_chat_account(card, &competing, &source)
            .unwrap();

        let _ = app.update(Message::ChatUi(Action::RecoverAccount(card, system)));
        let target = chat_runtime_attempt(&app, card);
        let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
        let task = app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        )));
        assert!(app.chat_start_pending(card));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        // An unrelated save while acknowledgment is pending still contains
        // the old descriptor; only that descriptor is translated by the lane.
        app.card_state_mut(card).unwrap().title = "Title edited while committing".into();
        app.persist();
        if close_before_ack {
            app.notices.entries.clear();
            close_chat(&mut app, card);
            let reply = account_commit_reply(task);
            let _ = app.update(reply);
            app.session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(10))
                .unwrap();
            let stored = store.load_card(card, None).unwrap().unwrap();
            assert!(
                !stored.existence.is_open(),
                "commit acknowledgment cannot resurrect a closed card"
            );
            assert!(!app.cards.contains_key(&card));
            retired(&mut app, card);
            continue;
        }
        let reply = account_commit_reply(task);
        let _ = app.update(reply);
        let CardKind::Chat(recovered) = &app.card_state(card).unwrap().kind else {
            panic!("chat retained");
        };
        assert_ne!(recovered.id, source.id);
        assert_eq!(recovered.launch.session_id, source.launch.session_id);
        assert_eq!(saved(&app), CardKind::Chat(recovered.clone()));
        assert_eq!(
            store.load_card(card, None).unwrap().unwrap().title,
            "Title edited while committing"
        );
        assert!(!app.chat_ui.removed_accounts.contains(&card));
    }
}

#[test]
fn confirmed_account_replacement_retires_only_predecessor_attention_without_acknowledging_it() {
    use crate::agent::ActivityState;

    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    app.refresh_ui.focused = false;
    let old_target = chat_runtime(&app, card).target;
    let old_handle = chat_runtime(&app, card).handle().unwrap().clone();
    let mut source = chat_runtime(&app, card).session.clone();
    source
        .launch
        .recipe
        .environment
        .insert("CODEX_HOME".into(), "/removed-account".into());
    app.install_chat_session(card, source.clone());
    let other = crate::app::tests::terminal_card_state();
    let unrelated = other.id;
    Arc::make_mut(&mut app.durable).cards.push(other);
    app.sidebar_state.observe_card_update(unrelated);
    let old_notice = uuid::Uuid::new_v4();
    let journal = |id, session| crate::notifications::Snapshot {
        diagnostic: None,
        notices: vec![crate::notifications::Notice {
            id,
            delivered: true,
            acknowledged: false,
            window: None,
            event: crate::notifications::Event {
                content: None,
                target: crate::notifications::Target::Session(session),
                source_session: Some(session),
                provider: Some(Provider::Codex),
                directory: Some(source.launch.recipe.working_directory.clone()),
                destination: None,
                evidence: crate::notifications::Evidence {
                    key: "account-replacement-unread".into(),
                    kind: crate::notifications::Kind::Succeeded,
                },
            },
        }],
    };
    let old_journal = journal(old_notice, source.id);
    drop(
        app.update(Message::NotificationUi(notification_ui::Action::Snapshot(
            old_journal.clone(),
        ))),
    );
    assert!(old_handle.observe_activity_for_test(ActivityState::Waiting));
    drop(app.update(Message::ChatUi(Action::Wake(old_target, Wake::Activity))));
    let waiting = Activity {
        provider: Provider::Codex,
        state: ActivityState::Waiting,
    };
    assert_eq!(app.sidebar_state.card_activity(card), Some(waiting));
    assert!(app.sidebar_state.has_card_update(card));
    // Metadata on the same UUID must not spend that actor's attention.
    let mut metadata = source.clone();
    metadata.proof = None;
    app.install_chat_session(card, metadata);
    assert_eq!(app.sidebar_state.card_activity(card), Some(waiting));
    assert!(app.sidebar_state.has_card_update(card));
    app.install_chat_session(card, source.clone());

    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("account-attention"),
    )
    .unwrap();
    app.session = Some(writer);
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(10))
        .unwrap();
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    drop(app.open_chat(card, source.clone(), chat::Intent::Restore));
    let target = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure {
            message: "The saved account was removed".into(),
            recovery: chat::Recovery::Account,
        }),
    );
    assert!(app.chat_ui.removed_accounts.contains(&card));
    assert!(app.sidebar_state.has_card_update(card));
    let activity_before = app.sidebar_state.card_activity(card);
    let system = Account::system(Provider::Codex).id();
    // Failed and wrong-conversation candidates never install a new source.
    for wrong_thread in [false, true] {
        drop(app.update(Message::ChatUi(Action::RecoverAccount(
            card,
            system.clone(),
        ))));
        let target = chat_runtime_attempt(&app, card);
        assert_ne!(app.chat_ui.attempt(card).unwrap().session.id, source.id);
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        assert_eq!(app.sidebar_state.card_activity(card), activity_before);
        assert!(app.sidebar_state.has_card_update(card));
        if wrong_thread {
            let handle = fixture_handle(&app, card, Some("different-conversation"));
            answer_start(&mut app, target, Ok(handle));
        } else {
            answer_start(
                &mut app,
                target,
                Err(chat::StartFailure::retry("fixture refusal")),
            );
        }
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        assert_eq!(app.sidebar_state.card_activity(card), activity_before);
        assert!(app.sidebar_state.has_card_update(card));
        assert!(!app.notification_ui.acknowledged_for_test(old_notice));
        let retirement = retired(&mut app, card);
        drop(app.update(Message::ChatUi(Action::Retired(retirement, Ok(())))));
    }
    let focused = app.workspace.focused();
    let durable_focused = app.durable.focused;
    drop(app.update(Message::ChatUi(Action::RecoverAccount(card, system))));
    let target = chat_runtime_attempt(&app, card);
    // The existing handle fixture supplies the confirmed session; the
    // ordered writer and AccountCommitted path are real, not a forged ACK.
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    let replacement = handle.session();
    assert_ne!(replacement.id, source.id);
    let task = app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    )));
    assert!(app.chat_start_pending(card));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(source.clone())
    );
    assert_eq!(app.sidebar_state.card_activity(card), activity_before);
    assert!(app.sidebar_state.has_card_update(card));
    let reply = account_commit_reply(task);
    assert!(matches!(
        &reply,
        Message::ChatUi(Action::AccountCommitted(_, _, Ok(())))
    ));
    let store = app.session.as_ref().unwrap().operation_store();
    assert_eq!(
        store.load_card(card, None).unwrap().unwrap().kind,
        CardKind::Chat(replacement.clone())
    );
    // Even a genuinely committed receipt has not installed the UI identity
    // until the normal reducer accepts that exact completion.
    assert!(app.sidebar_state.has_card_update(card));
    drop(app.update(reply));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(replacement.clone())
    );
    assert_eq!(replacement.launch.session_id, source.launch.session_id);
    assert!(!app.chat_start_pending(card));
    assert!(!app.sidebar_state.has_card_update(card));
    assert_eq!(
        app.sidebar_state.card_activity(card),
        Some(Activity {
            provider: Provider::Codex,
            state: ActivityState::Idle,
        })
    );
    assert!(app.sidebar_state.has_card_update(unrelated));
    assert_eq!(app.workspace.focused(), focused);
    assert_eq!(app.durable.focused, durable_focused);
    assert!(!app.notification_ui.acknowledged_for_test(old_notice));
    let current_target = chat_runtime(&app, card).target;
    drop(app.update(Message::ChatUi(Action::Wake(old_target, Wake::Activity))));
    assert_eq!(chat_runtime(&app, card).target, current_target);
    drop(
        app.update(Message::NotificationUi(notification_ui::Action::Snapshot(
            old_journal,
        ))),
    );
    assert!(
        !app.sidebar_state.has_card_update(card),
        "late predecessor news cannot rebind"
    );
    assert!(!app.notification_ui.acknowledged_for_test(old_notice));
    let fresh_notice = uuid::Uuid::new_v4();
    drop(
        app.update(Message::NotificationUi(notification_ui::Action::Snapshot(
            journal(fresh_notice, replacement.id),
        ))),
    );
    assert!(
        app.sidebar_state.has_card_update(card),
        "current actor news remains usable"
    );
    assert!(app.sidebar_state.has_card_update(unrelated));
    assert!(!app.notification_ui.acknowledged_for_test(fresh_notice));
}

/// A conversation an earlier version started for a chat nobody wrote in
/// was never named by its provider, so once its daemon is gone there is
/// nothing to resume: it reopens as a draft rather than as a failure.
#[test]
fn a_never_named_conversation_whose_daemon_is_gone_reopens_as_a_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, None);
    let session = chat_runtime(&app, card).session.clone();
    assert!(!session.is_unstarted());
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    let _ = app.open_chat(card, session.clone(), chat::Intent::Restore);
    let target = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure::fresh("fixture: daemon gone")),
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing());
    assert_eq!(runtime.error, None, "nothing was lost, so nothing is said");
    assert!(runtime.session.is_unstarted());
    assert_eq!(
        runtime.session.launch.recipe.arguments,
        ["--remote", "unix://"]
    );
    assert!(!app.chat_offers_fresh_conversation(card));
    retired(&mut app, card);
    let CardKind::Chat(saved) = &app.card_state(card).unwrap().kind else {
        panic!("the card is a chat");
    };
    assert!(saved.is_unstarted(), "the next launch opens it as a draft");
    assert_eq!(saved.id, session.id);

    // One converted from a CLI terminal may hold a conversation there,
    // so it asks, as before.
    let card = live_chat(&mut app, None);
    let mut session = chat_runtime(&app, card).session.clone();
    session.terminal = Some(Box::new(crate::app::fresh_terminal_session(LaunchRecipe {
        unset_environment: Default::default(),
        program: "codex".into(),
        arguments: Vec::new(),
        working_directory: session.launch.recipe.working_directory.clone(),
        environment: Default::default(),
    })));
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    let _ = app.open_chat(card, session, chat::Intent::Restore);
    let target = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure::fresh("fixture: daemon gone")),
    );
    assert!(app.chat_offers_fresh_conversation(card));
}

/// An agent's chat card whose Codex conversation was never named, after its
/// restore found the daemon gone. `kept_runtime` keeps the card's chat
/// runtime, as a window open across the daemon's restart does.
fn unreachable_unnamed_agent(kept_runtime: bool) -> (Cagents, CardId, chat::Session) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, None);
    let session = chat_runtime(&app, card).session.clone();
    assert!(!session.is_unstarted());
    assert!(session.launch.session_id.is_none());
    app.workspace_model.agents = vec![crate::app::channel_ui::close_tests::agent(&session)];
    if !kept_runtime {
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
    }
    let _ = app.open_chat(card, session.clone(), chat::Intent::Restore);
    let target = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure::fresh("fixture: daemon gone")),
    );
    (app, card, session)
}

/// #3095: an agent's Codex conversation whose thread ID was never saved
/// came back from an abandoned update as a draft, and giving up the old
/// conversation retired the agent. The card keeps the conversation and says
/// why it can't reopen; the agent stays open.
#[test]
fn an_agents_never_named_conversation_stays_its_card_when_its_daemon_is_gone() {
    // Whether the card still had its chat runtime when the restore failed, as
    // a window that stays open across the daemon's restart does.
    for kept_runtime in [true, false] {
        let (mut app, card, session) = unreachable_unnamed_agent(kept_runtime);
        assert!(
            app.chat_ui
                .retiring
                .iter()
                .all(|retiring| retiring.card != card),
            "nothing gives up the agent's conversation"
        );
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(session.clone()),
            "the card still names the conversation the agent ran"
        );
        let why = super::unnamed_agent_conversation(&session);
        // Starting over would retire the agent's pane, and reconnecting finds
        // nothing, so Close is its only recovery.
        assert!(!app.chat_offers_fresh_conversation(card));
        // Nor does it open in a terminal, which would give the card up and
        // leave the agent without one.
        assert_eq!(
            app.chat_switch_blocker(card).as_deref(),
            Some(blocked_recovery::UNNAMED_AGENT_TERMINAL)
        );
        // One centred state replaces the pane, composer included.
        assert!(app.unnamed_agent(card).is_some());
        assert!(app.unnamed_agent_view(card).is_some());
        assert_eq!(app.blocked_details_text(card), Some(why.as_str()));
        assert!(!why.contains("daemon"));
        // Its open record doesn't make the agent run: until the daemon proves
        // its processes still run, the card claims nothing, and Close is only
        // Close.
        let stopped_copy = blocked_recovery::UnnamedAgentCopy {
            body: "cagents lost track of this conversation when its background service stopped. Nothing new was started.",
            close: "Close",
            close_name: "Close this tab",
        };
        assert_eq!(app.unnamed_agent_copy(card), Some(stopped_copy));
        // An answer about another conversation changes nothing.
        let survivor = crate::terminal::ipc::ProcessStart {
            pid: 4242,
            start: 7,
        };
        let mut other = session.clone();
        other.id = TerminalId::new();
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentRunning(
            card,
            other.clone(),
            vec![survivor],
        )));
        assert_eq!(app.unnamed_agent_copy(card), Some(stopped_copy));
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentRunning(
            card,
            session.clone(),
            vec![survivor],
        )));
        assert_eq!(
            app.unnamed_agent_copy(card),
            Some(blocked_recovery::UnnamedAgentCopy {
                body: "cagents lost track of this conversation when its background service stopped. The agent is still running. Nothing new was started.",
                close: "Close and end agent",
                close_name: "Close this tab and end its agent",
            })
        );
        assert!(app.unnamed_agent_view(card).is_some());
        // Close and end agent ends exactly what the card said runs before it
        // closes anything; an answer arriving meanwhile doesn't change that.
        let liveness = |app: &Cagents| app.unnamed_agent(card).unwrap().liveness.clone();
        let _ = app.update(Message::CloseCard(card));
        assert!(liveness(&app).ending);
        assert!(app.card_state(card).unwrap().existence.is_open());
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentRunning(
            card,
            session.clone(),
            Vec::new(),
        )));
        assert_eq!(liveness(&app).running, vec![survivor]);
        // A failure to end it keeps the card, says why, and lets Close try
        // again; so does an agent still running afterwards.
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentEnded(
            card,
            session.clone(),
            Err("fixture: cannot end".into()),
        )));
        assert!(!liveness(&app).ending);
        assert_eq!(
            liveness(&app).end_error.as_deref(),
            Some("fixture: cannot end")
        );
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(session.clone())
        );
        let _ = app.update(Message::CloseCard(card));
        assert!(liveness(&app).ending);
        assert_eq!(liveness(&app).end_error, None);
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentEnded(
            card,
            session.clone(),
            Ok(vec![crate::terminal::ipc::LostProcess {
                process: survivor,
                name: "codex".into(),
                command: "codex app-server".into(),
                proved: true,
            }]),
        )));
        assert_eq!(
            liveness(&app).end_error.as_deref(),
            Some(blocked_recovery::UNNAMED_AGENT_STILL_RUNS)
        );
        assert_eq!(liveness(&app).running, vec![survivor]);
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentRunning(
            card,
            session.clone(),
            Vec::new(),
        )));
        assert_eq!(app.unnamed_agent_copy(card), Some(stopped_copy));
        if kept_runtime {
            let runtime = chat_runtime(&app, card);
            assert!(!runtime.is_draft());
            assert_eq!(runtime.error.as_ref(), Some(&why));
            assert_eq!(
                runtime.start_blocked,
                Some(blocked_recovery::Reopen::UnnamedAgent)
            );
            let stopped = runtime.stopped(false, false);
            assert_eq!(
                stopped.primary.map(|(label, press, _)| (label, press)),
                Some(("Close", stopped::Press::Close))
            );
            assert_eq!(stopped.secondary, None);
        } else {
            assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
            assert_eq!(attach_failure_details(&app, card), Some(why));
            assert_eq!(
                app.blocked_failure(card).map(|blocked| blocked.reopen),
                Some(blocked_recovery::Reopen::UnnamedAgent)
            );
        }
        // The refusal belongs to the conversation it refused, not the card:
        // once the card names another one, neither its state nor its
        // terminal blocker applies, and its Details reveal nothing.
        let mut replacement = session.clone();
        replacement.id = TerminalId::new();
        app.replace_chat_conversation(card, replacement);
        assert!(app.unnamed_agent(card).is_none());
        assert!(app.unnamed_agent_view(card).is_none());
        assert_eq!(app.unnamed_agent_copy(card), None);
        assert_ne!(
            app.chat_switch_blocker(card).as_deref(),
            Some(blocked_recovery::UNNAMED_AGENT_TERMINAL)
        );
        assert_eq!(app.blocked_details_text(card), None);
    }
}

/// #3095: Close and end agent ends the agent only once the close is settled:
/// an unsent message is asked about first, and the card closes once nothing
/// it named still runs.
#[test]
fn closing_an_unreachable_unnamed_agent_ends_it_after_asking_then_closes() {
    for kept_runtime in [true, false] {
        let (mut app, card, session) = unreachable_unnamed_agent(kept_runtime);
        let survivor = crate::terminal::ipc::ProcessStart {
            pid: 4242,
            start: 7,
        };
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentRunning(
            card,
            session.clone(),
            vec![survivor],
        )));
        let liveness = |app: &Cagents| app.unnamed_agent(card).unwrap().liveness.clone();
        // A retained conversation closes through the daemon, so its card
        // goes once that answers; any other card goes at once.
        let closing = |app: &Cagents| {
            app.chat_close_owned(card) || !app.card_state(card).unwrap().existence.is_open()
        };
        if kept_runtime {
            let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                unreachable!()
            };
            runtime.replace_draft(text_editor::Content::with_text("Keep this unsent note."));
            let _ = app.update(Message::CloseCard(card));
            assert!(app.chat_close_dialog_visible());
            assert!(!liveness(&app).ending, "nothing ends before the answer");
            let _ = app.update(Message::CancelChatClose);
            assert!(!liveness(&app).ending);
            assert!(!closing(&app));
            let _ = app.update(Message::CloseCard(card));
            let _ = app.update(Message::ConfirmChatClose);
        } else {
            let _ = app.update(Message::CloseCard(card));
        }
        assert!(liveness(&app).ending);
        assert!(!closing(&app));
        // Another conversation's answer settles nothing.
        let mut other = session.clone();
        other.id = TerminalId::new();
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentEnded(
            card,
            other,
            Ok(Vec::new()),
        )));
        assert!(liveness(&app).ending);
        assert!(!closing(&app));
        let _ = app.update(Message::ChatUi(Action::UnnamedAgentEnded(
            card,
            session.clone(),
            Ok(Vec::new()),
        )));
        assert!(closing(&app));
    }
}

#[test]
fn attention_session_click_reveals_the_parent_before_reading_its_notice() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    app.refresh_ui.focused = true;
    let target = chat_runtime(&app, card).target;
    let source = chat_runtime(&app, card).session.clone();
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("chat fixture");
    };
    runtime.selected_subagent = Some("currently-viewed-child".into());
    let id = uuid::Uuid::new_v4();
    drop(app.update(Message::NotificationUi(
        crate::app::notification_ui::Action::Snapshot(crate::notifications::Snapshot {
            notices: vec![crate::notifications::Notice {
                id,
                event: crate::notifications::Event {
                    content: None,
                    target: crate::notifications::Target::Session(source.id),
                    source_session: Some(source.id),
                    provider: Some(source.launch.provider),
                    directory: Some(source.launch.recipe.working_directory.clone()),
                    destination: None,
                    evidence: crate::notifications::Evidence {
                        key: "parent-result-with-child-visible".into(),
                        kind: crate::notifications::Kind::Succeeded,
                    },
                },
                delivered: true,
                acknowledged: false,
                window: None,
            }],
            diagnostic: None,
        }),
    )));
    assert!(app.sidebar_state.has_card_update(card));
    assert!(!app.notification_ui.acknowledged_for_test(id));
    drop(app.update(Message::NotificationUi(
        crate::app::notification_ui::Action::Activated(crate::notifications::Target::Session(
            source.id,
        )),
    )));
    assert!(chat_runtime(&app, card).parent_transcript_selected());
    assert!(!app.sidebar_state.has_card_update(card));
    assert!(app.notification_ui.acknowledged_for_test(id));
    assert_eq!(chat_runtime(&app, card).target, target);
    assert_eq!(chat_runtime(&app, card).session, source);
    assert!(Arc::ptr_eq(
        chat_runtime(&app, card).handle().unwrap(),
        &handle
    ));
}

#[test]
fn chat_descriptor_unchanged_observed_wakes_keep_the_retained_workspace_allocation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let source = chat_runtime(&app, card).session.clone();
    assert_eq!(*app.residency(card), Residency::Hot);
    app.lifecycle
        .register(card, Exposure::Offscreen, Residency::Hot, Instant::now());
    // The writer and cached projections can retain a durable snapshot.
    // An unchanged observation must not copy that whole workspace.
    let retained = app.durable.clone();
    let weak = Arc::downgrade(&retained);
    for _ in 0..16 {
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Observed)));
        assert!(Arc::ptr_eq(&retained, &app.durable));
        assert!(Arc::ptr_eq(&weak.upgrade().unwrap(), &app.durable));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
    }
    assert!(
        app.lifecycle.state(card).is_none(),
        "attachment still completes hydration"
    );
    assert_eq!(chat_runtime(&app, card).target, target);
    assert!(!app.chat_start_pending(card));
}

#[test]
fn chat_descriptor_unchanged_attachment_still_promotes_cold_residency() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let source = chat_runtime(&app, card).session.clone();
    app.set_residency(card, Residency::Cold);
    app.lifecycle
        .register(card, Exposure::Offscreen, Residency::Cold, Instant::now());
    let retained = app.durable.clone();
    assert_eq!(app.install_chat_session(card, source.clone()), None);
    // Residency is this run's presence: promoting it neither copies nor
    // changes the workspace.
    assert!(Arc::ptr_eq(&retained, &app.durable));
    assert_eq!(*app.residency(card), Residency::Hot);
    assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(source));
    assert!(app.lifecycle.state(card).is_none());
}

#[test]
fn chat_descriptor_changed_source_still_replaces_the_retained_workspace_source() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let original = chat_runtime(&app, card).session.clone();
    let mut changed = original.clone();
    changed.launch.session_id = Some("new-provider-confirmed-thread".into());
    let retained = app.durable.clone();
    assert_eq!(app.install_chat_session(card, changed.clone()), Some(false));
    assert!(!Arc::ptr_eq(&retained, &app.durable));
    assert_eq!(
        retained
            .cards
            .iter()
            .find(|state| state.id == card)
            .unwrap()
            .kind,
        CardKind::Chat(original)
    );
    assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(changed));
    assert_eq!(*app.residency(card), Residency::Hot);
}

#[test]
fn unnamed_hosted_chat_attachment_finishes_hydration_without_replacing_identity() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, None);
    let session = chat_runtime(&app, card).session.clone();
    assert!(session.launch.session_id.is_none());
    assert!(!session.is_unstarted());
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    app.set_residency(card, Residency::Cold);
    app.lifecycle
        .register(card, Exposure::Offscreen, Residency::Cold, Instant::now());
    let _ = app.open_chat(card, session.clone(), chat::Intent::Restore);
    let target = chat_runtime_attempt(&app, card);
    let handle = Arc::new(ClientHandle::fixture(
        session.clone(),
        Snapshot {
            ready: true,
            busy: true,
            ..Snapshot::default()
        },
    ));
    answer_start(&mut app, target, Ok(handle));
    let runtime = chat_runtime(&app, card);
    assert!(!runtime.is_draft());
    assert!(runtime.handle().unwrap().snapshot().busy);
    // Hidden cards attach for activity without building presentation state.
    // Revealing the adopted card must render that existing busy snapshot.
    app.chat_runtime_mut(target).unwrap().show(true);
    let runtime = chat_runtime(&app, card);
    assert!(runtime.snapshot.busy);
    assert_eq!(runtime.session, session);
    assert_eq!(*app.residency(card), Residency::Hot);
    assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(session));
    assert!(!app.chat_start_pending(card));
}

/// An unstarted saved conversation reopens as a draft without retiring
/// another window's possible execution; a started one reconnects.
#[test]
fn only_a_conversation_that_ran_reconnects_on_restore() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let session = chat_runtime(&app, card).session.clone();
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    let _ = app.open_chat(card, session, chat::Intent::Restore);
    assert!(chat_runtime(&app, card).is_draft());
    assert!(!app.chat_start_pending(card));
    assert!(app.chat_ui.retiring.is_empty());

    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let session = chat_runtime(&app, card).session.clone();
    assert!(!session.is_unstarted());
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    let _ = app.open_chat(card, session, chat::Intent::Restore);
    assert!(app.chat_start_pending(card));
}

fn capacity_recovery_labels(app: &Cagents, card: CardId) -> Vec<String> {
    recovery_labels(app.card_view(card, true, false))
}

fn recovery_labels(mut element: Element<'_, Message>) -> Vec<String> {
    use iced::advanced::{Layout, layout, widget::Tree};
    let renderer = crate::app::tests::headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 800.0)),
    );
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, iced::mouse::Cursor::Unavailable);
    accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter_map(|node| node.node().label().map(str::to_owned))
        .collect()
}

#[test]
fn nonretryable_start_refusal_keeps_the_saved_card_until_explicit_recovery() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("keep this draft"));
    let source = chat_runtime(&app, card).session.clone();
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    drop(app.open_chat(card, source.clone(), chat::Intent::Restore));
    let target = chat_runtime_attempt(&app, card);
    let reason = "Codex recipe resume ID does not match the selected thread";
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure {
            message: reason.into(),
            recovery: chat::Recovery::Blocked,
        }),
    );
    assert_eq!(attach_failure_details(&app, card).as_deref(), Some(reason));
    assert!(app.chat_retry_blocked(card));
    assert!(app.chat_offers_fresh_conversation(card));
    let labels = capacity_recovery_labels(&app, card);
    assert!(
        labels.iter().any(|label| label.contains("Close")),
        "{labels:?}"
    );
    assert!(
        labels.iter().all(|label| !label.contains("Retry")),
        "{labels:?}"
    );
    app.workspace.resize_viewport(1280.0);
    drop(app.set_pane_inspector_open(card, true));
    assert!(app.pane_inspectors.is_visible(card));
    let labels = recovery_labels(cosmic::Application::view(&app));
    assert!(
        labels.iter().all(|label| !label.contains("Retry")),
        "a pane with its inspector open must honor the same refusal: {labels:?}"
    );
    drop(app.update(Message::RetryCard(card)));
    assert!(!app.chat_start_pending(card));
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(source.clone())
    );
    assert_eq!(app.chat_ui.drafts.get(&card).unwrap(), "keep this draft");

    // Another authoritative conversation on this card does not inherit
    // the earlier conversation's refusal.
    let mut replacement = source;
    replacement.id = TerminalId::new();
    app.replace_chat_conversation(card, replacement);
    assert!(!app.chat_retry_blocked(card));
}

/// A refusal a restart lifts reaches both blocked surfaces, the failed
/// card of a restore and the stopped runtime of a resume, as that kind.
#[test]
fn a_refusal_a_restart_lifts_says_so_on_the_failed_and_stopped_card() {
    let reason = format!(
        "The saved conversation could not be reopened yet, so nothing was restarted or resent. {}",
        chat::UNPROVEN_OWNER_REFUSAL
    );
    for intent in [chat::Intent::Restore, chat::Intent::Resume] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let source = chat_runtime(&app, card).session.clone();
        if intent == chat::Intent::Restore {
            app.chat_forget(card);
            app.cards.insert(card, CardRuntime::Loading);
            drop(app.open_chat(card, source.clone(), intent));
        } else {
            drop(app.start_chat(card, source.clone(), intent, None, false));
        }
        let attempt = chat_runtime_attempt(&app, card);
        answer_start(
            &mut app,
            attempt,
            Err(chat::StartFailure {
                message: reason.clone(),
                recovery: chat::Recovery::AfterRestart,
            }),
        );
        assert!(app.chat_retry_blocked(card), "{intent:?}");
        assert!(!app.chat_start_pending(card), "{intent:?}");
        let mut labels = capacity_recovery_labels(&app, card);
        if let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) {
            // A runtime puts the headline on its failure incident.
            runtime.refresh_failure_ui();
            let incident = runtime.failure_ui.incidents.last().unwrap();
            labels.extend(recovery_labels(runtime.failure_view(incident)));
        }
        assert!(
            labels
                .iter()
                .any(|label| label == blocked_recovery::RESTART_HEADLINE),
            "{intent:?}: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|label| label.starts_with(blocked_recovery::RESTART_GUIDANCE)),
            "{intent:?}: {labels:?}"
        );
        assert!(
            labels
                .iter()
                .all(|label| !label.contains("Retry") && !label.contains("Resume conversation")),
            "{intent:?}: {labels:?}"
        );
        drop(app.update(Message::RetryCard(card)));
        assert!(!app.chat_start_pending(card), "{intent:?}");
    }
}

#[test]
fn nonretryable_failed_resume_preserves_an_explicit_fresh_start_offer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = unresumable_chat(&mut app);
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("keep this draft"));
    let source = chat_runtime(&app, card).session.clone();
    drop(app.start_chat(card, source.clone(), chat::Intent::Resume, None, false));
    let attempt = chat_runtime_attempt(&app, card);
    let reason = "The saved conversation has a different exact recipe";
    answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure {
            message: reason.into(),
            recovery: chat::Recovery::Blocked,
        }),
    );
    assert!(app.chat_retry_blocked(card));
    assert!(app.chat_offers_fresh_conversation(card));
    assert!(!app.chat_start_pending(card));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.session, source);
    assert_eq!(runtime.error.as_deref(), Some(reason));
    assert_eq!(runtime.composer.text(), "keep this draft");
    let stopped = runtime.stopped(false, false);
    assert_eq!(
        stopped.guidance,
        blocked_recovery::FRESH_EXPLANATION,
        "a permanent refusal promises no reopening step"
    );
    assert_eq!(
        stopped.primary,
        Some(("Start a new conversation", stopped::Press::StartFresh, true)),
    );
    assert_eq!(
        stopped.secondary,
        Some(("Close", stopped::Press::Close, true))
    );
    assert!(!runtime.stopped(true, false).primary.unwrap().2);
    assert!(
        matches!(stopped.primary.unwrap().1.message(runtime.target), Message::ChatUi(Action::StartFresh(fresh)) if fresh == card)
    );

    let target = runtime.target;
    drop(app.update(Message::RetryCard(card)));
    drop(app.handle_chat_ui(Action::Reconnect(target)));
    drop(app.handle_chat_ui(Action::ResumeSaved(target)));
    assert!(!app.chat_start_pending(card));
    assert_eq!(chat_runtime(&app, card).session, source);

    // Only this explicit existing action gives up the old pointer; the
    // failure and stale Retry controls cannot take that choice for us.
    drop(app.handle_chat_ui(Action::StartFresh(card)));
    let closing = fresh_close_request(&app, card);
    let Some(Phase::Closing {
        restart: Some(fresh),
        ..
    }) = app.chat_ui.phases.get(&card)
    else {
        panic!("the explicit fresh action must close the failed attachment first");
    };
    assert_eq!(fresh.launch.session_id, None);
    assert_eq!(fresh.launch.recipe.arguments, ["--remote", "unix://"]);
    assert_eq!(
        fresh.launch.recipe.environment,
        source.launch.recipe.environment
    );
    assert_eq!(chat_runtime(&app, card).session, source);
    assert_eq!(chat_runtime(&app, card).composer.text(), "keep this draft");
    drop(app.handle_chat_ui(fresh_close_answer(closing, Ok(()))));
    let fresh = app
        .chat_ui
        .attempt(card)
        .expect("the selected fresh start resumes after Close");
    assert_eq!(fresh.intent, chat::Intent::Start);
    assert_eq!(fresh.session.launch.session_id, None);
    assert_eq!(
        fresh.session.launch.recipe.arguments,
        ["--remote", "unix://"]
    );
    assert_eq!(
        fresh.session.launch.recipe.environment,
        source.launch.recipe.environment
    );
    assert_eq!(
        app.chat_ui.drafts.get(&card).map(String::as_str),
        Some("keep this draft")
    );
}

#[test]
fn an_explicit_fresh_start_drops_retained_actor_and_pair_authority() {
    let conversation = "33333333-4444-4aaa-8bbb-cccccccccccc";
    for (provider, paired) in [Provider::Claude, Provider::Codex]
        .into_iter()
        .flat_map(|provider| [false, true].map(|paired| (provider, paired)))
    {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat_for_provider(&mut app, provider, Some(conversation));
        let target = chat_runtime(&app, card).target;
        let mut source = chat_runtime(&app, card).session.clone();
        source
            .launch
            .recipe
            .arguments
            .extend(["--model".into(), "fixture-model".into()]);
        let account_home = crate::account::home_variable(provider).unwrap();
        source
            .launch
            .recipe
            .environment
            .insert(account_home.into(), "/fixture/selected-account".into());
        let mut expected_arguments = source.launch.recipe.arguments.clone();
        if provider == Provider::Codex {
            let remote = expected_arguments
                .iter()
                .position(|argument| argument == "--remote")
                .unwrap();
            expected_arguments[remote + 1] = "unix://".into();
            source
                .launch
                .recipe
                .arguments
                .splice(0..0, ["resume".into(), conversation.into()]);
        } else {
            source.launch.recipe.arguments.extend([
                "--resume".into(),
                conversation.into(),
                format!("--session-id={conversation}"),
            ]);
        }
        source.native_continuity = match provider {
            Provider::Claude => chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
            Provider::Codex => chat::NativeContinuity::Durable {
                provider_version: "0.100.0".into(),
            },
            Provider::Omp | Provider::Antigravity | Provider::Muse => unreachable!(),
        };
        if paired {
            let proof = source.proof.clone().unwrap();
            source.terminal = Some(Box::new(TerminalSession {
                id: source.id,
                launch: source.launch.recipe.clone(),
                continuity: TerminalContinuity::Live {
                    host_session_id: proof.host_session_id,
                    boot_id: proof.boot_id,
                    host_incarnation: proof.host_incarnation,
                },
                transcript_path: None,
                resume_hint: None,
                owned_codex_listener: source.owned_codex_listener.clone(),
                recovery_origin: None,
                predecessor: None,
            }));
        }
        assert!(source.has_valid_native_continuity());
        assert_eq!(source.owns_codex_listener(), provider == Provider::Codex);
        app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session = source.clone();
        runtime.start_blocked = Some(blocked_recovery::Reopen::Unproven);
        runtime.replace_draft(text_editor::Content::with_text("keep the chosen draft"));
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.exited = true;
        snapshot.session_id = None;
        assert!(app.chat_offers_fresh_conversation(card));

        drop(app.handle_chat_ui(Action::StartFresh(card)));
        let closing = fresh_close_request(&app, card);
        let Some(Phase::Closing {
            restart: Some(fresh),
            ..
        }) = app.chat_ui.phases.get(&card)
        else {
            panic!("the explicit choice must close its exact old attachment first");
        };
        assert!(
            fresh.is_unstarted(),
            "{provider:?}, paired={paired}: {fresh:?}"
        );
        assert!(fresh.has_valid_native_continuity());
        assert_eq!(
            fresh.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        assert!(fresh.terminal.is_none());
        assert!(fresh.owned_codex_listener.is_none());
        assert_eq!(fresh.launch.recipe.program, source.launch.recipe.program);
        assert_eq!(
            fresh.launch.recipe.working_directory,
            source.launch.recipe.working_directory
        );
        assert_eq!(
            fresh.launch.recipe.environment,
            source.launch.recipe.environment
        );
        assert_eq!(
            fresh.launch.recipe.unset_environment,
            source.launch.recipe.unset_environment
        );
        assert_eq!(fresh.launch.recipe.arguments, expected_arguments);
        let fresh = fresh.clone();
        assert_eq!(chat_runtime(&app, card).session, source);
        assert_eq!(
            chat_runtime(&app, card).composer.text(),
            "keep the chosen draft"
        );

        drop(app.handle_chat_ui(fresh_close_answer(
            closing.clone(),
            Err("fixture: close not proved".into()),
        )));
        assert!(app.chat_ui.attempt(card).is_none());
        assert_eq!(chat_runtime(&app, card).session, source);
        assert_eq!(
            chat_runtime(&app, card).composer.text(),
            "keep the chosen draft"
        );
        // A late success from that failed close cannot adopt a new start.
        drop(app.handle_chat_ui(fresh_close_answer(closing.clone(), Ok(()))));
        assert!(app.chat_ui.attempt(card).is_none());
        assert_eq!(chat_runtime(&app, card).session, source);
        drop(app.handle_chat_ui(Action::StartFresh(card)));
        let replacement_close = fresh_close_request(&app, card);
        assert_ne!(replacement_close.0, closing.0);
        // The previous operation cannot consume the replacement close.
        drop(app.handle_chat_ui(fresh_close_answer(closing, Ok(()))));
        assert_eq!(fresh_close_request(&app, card).0, replacement_close.0);
        drop(app.handle_chat_ui(fresh_close_answer(replacement_close, Ok(()))));
        let attempt = app
            .chat_ui
            .attempt(card)
            .expect("Close admits the selected fresh start");
        assert_eq!(attempt.intent, chat::Intent::Start);
        assert_eq!(attempt.session, fresh);
        assert!(attempt.session.is_unstarted());
        assert_eq!(
            app.chat_ui.drafts.get(&card).map(String::as_str),
            Some("keep the chosen draft")
        );
    }
}

#[test]
fn unsafe_fresh_recipe_keeps_the_source_and_draft_without_a_close() {
    for arguments in [
        vec!["--resume".into(), "a-different-conversation".into()],
        vec!["an already consumed prompt".into()],
        vec!["--model".into()],
        vec!["-c".into(), "model=\"o3\"".into()],
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = unresumable_chat(&mut app);
        let mut source = chat_runtime(&app, card).session.clone();
        source.reset_owned_codex_listener();
        source.launch.recipe.arguments = arguments;
        app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session = source.clone();
        runtime.start_blocked = Some(blocked_recovery::Reopen::Unproven);
        runtime.replace_draft(text_editor::Content::with_text("keep the draft"));
        assert!(!app.chat_offers_fresh_conversation(card));
        drop(app.handle_chat_ui(Action::StartFresh(card)));
        assert!(!app.chat_start_pending(card));
        assert_eq!(chat_runtime(&app, card).session, source);
        assert_eq!(chat_runtime(&app, card).composer.text(), "keep the draft");
        app.chat_forget(card);
        app.cards.remove(&card);
        app.chat_ui.fresh_offers.insert(card);
        assert!(!app.chat_offers_fresh_conversation(card));
        drop(app.handle_chat_ui(Action::StartFresh(card)));
        assert!(!app.chat_start_pending(card));
        assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(source));
        assert_eq!(
            app.chat_ui.drafts.get(&card).map(String::as_str),
            Some("keep the draft")
        );
    }
}

#[test]
fn nonretryable_resume_refusal_keeps_runtime_and_rejects_stale_retry_actions() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("keep this draft"));
    let source = chat_runtime(&app, card).session.clone();
    drop(app.start_chat(card, source.clone(), chat::Intent::Resume, None, false));
    let target = chat_runtime_attempt(&app, card);
    let reason = "Codex recipe resume ID does not match the selected thread";
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure {
            message: reason.into(),
            recovery: chat::Recovery::Blocked,
        }),
    );
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.error.as_deref(), Some(reason));
    assert_eq!(runtime.composer.text(), "keep this draft");
    assert_eq!(runtime.session, source);
    let stopped = runtime.stopped(false, false);
    assert_eq!(
        stopped.guidance,
        blocked_recovery::FRESH_EXPLANATION,
        "a permanent refusal promises no reopening step"
    );
    assert_eq!(
        stopped.primary,
        Some(("Start a new conversation", stopped::Press::StartFresh, true))
    );
    assert_eq!(
        stopped.secondary,
        Some(("Close", stopped::Press::Close, true))
    );
    assert!(app.chat_offers_fresh_conversation(card));
    let retry = runtime.target;
    drop(app.update(Message::RetryCard(card)));
    drop(app.handle_chat_ui(Action::Reconnect(retry)));
    drop(app.handle_chat_ui(Action::ResumeSaved(retry)));
    assert!(!app.chat_start_pending(card));
    assert_eq!(chat_runtime(&app, card).session, source);

    // A valid attachment from an independent recovery clears the refusal;
    // a delayed reply from the old attempt cannot restore it.
    drop(app.start_chat(card, source, chat::Intent::Restore, None, false));
    let recovered = chat_runtime_attempt(&app, card);
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    answer_start(&mut app, recovered, Ok(handle));
    assert!(!app.chat_retry_blocked(card));
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure {
            message: "stale refusal".into(),
            recovery: chat::Recovery::Blocked,
        }),
    );
    assert!(!app.chat_retry_blocked(card));
    assert_eq!(chat_runtime(&app, card).composer.text(), "keep this draft");
}

#[test]
fn failed_resume_retains_history_and_refusal_when_a_stale_connected_handle_cannot_hydrate() {
    for (recovery, hide_before_reply) in [chat::Recovery::Retry, chat::Recovery::Blocked]
        .into_iter()
        .flat_map(|recovery| [false, true].map(|hidden| (recovery, hidden)))
    {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.workspace.resize_viewport(1280.0);
        let card = live_chat_for_provider(&mut app, Provider::Claude, Some(KNOWN_THREAD));
        let target = chat_runtime(&app, card).target;
        let mut source = chat_runtime(&app, card).session.clone();
        source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        let item = Arc::new(chat::Item {
            id: "retained-answer".into(),
            role: chat::Role::Assistant,
            content: text("The original conversation is still readable"),
            at: None,
            started_at: None,
        });
        let handle = Arc::new(ClientHandle::fixture(
            source.clone(),
            Snapshot {
                ready: true,
                exited: true,
                session_id: source.launch.session_id.clone(),
                native_continuity: source.native_continuity.clone(),
                items: vec![item.clone()],
                ..Snapshot::default()
            },
        ));
        let mut runtime = Runtime::new(
            target,
            source.clone(),
            Connection::Live(handle.clone()),
            true,
            app.settings.editor_syntax_theme,
        );
        runtime.refresh();
        runtime.replace_draft(text_editor::Content::with_text("keep this draft"));
        assert!(runtime.resumes_in_place());
        app.replace_chat_conversation(card, source.clone());
        app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
        drop(app.start_chat(card, source, chat::Intent::Resume, None, false));
        let attempt = chat_runtime_attempt(&app, card);
        if hide_before_reply {
            handle.set_interest(false, false).unwrap();
            app.set_workspace_surfaces_visible(false);
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.refresh();
            assert_eq!(runtime.snapshot.items, [item.clone()]);
        }
        let reason =
            "Provider executable claude is not executable in the preserved launch environment";
        answer_start(
            &mut app,
            attempt,
            Err(chat::StartFailure {
                message: reason.into(),
                recovery,
            }),
        );
        let runtime = match app.cards.get_mut(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime,
            _ => panic!("the failed resume keeps its runtime"),
        };
        assert!(runtime.resume_failed);
        assert!(
            handle.is_connected(),
            "the IPC socket alone cannot hydrate a retired actor"
        );
        // A stale socket can hold metadata without a resident body. It
        // must never replace the last readable runtime publication.
        handle.set_interest(false, false).unwrap();
        assert!(handle.resident_snapshot_with_clock().is_none());
        runtime.set_visible(false).unwrap();
        runtime.refresh();
        assert_eq!(runtime.snapshot.items, [item.clone()]);
        assert!(runtime.warm_stamp().is_some());
        runtime.cool();
        assert!(runtime.cache.is_empty());
        assert_eq!(runtime.snapshot.items, [item.clone()]);
        runtime.set_visible(true).unwrap();
        preparation::drain_fixture(runtime);
        assert_eq!(runtime.snapshot.items, [item]);
        assert!(
            shown(&runtime.cache["retained-answer"])
                .contains("The original conversation is still readable")
        );
        assert_eq!(runtime.failure_ui.local_error.as_deref(), Some(reason));
        assert!(!runtime.failure_ui.incidents.is_empty());
        assert_eq!(runtime.composer.text(), "keep this draft");
        assert_eq!(runtime.error.as_deref(), Some(reason));
        let incident = runtime.failure_ui.incidents.last().unwrap();
        let labels = recovery_labels(runtime.failure_view(incident));
        assert!(
            labels.iter().any(|label| label.contains("Details")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|label| label.contains("Copy")),
            "{labels:?}"
        );
        assert_eq!(
            labels
                .iter()
                .any(|label| label.contains("before trying again")),
            recovery == chat::Recovery::Retry,
            "{labels:?}",
        );
    }
}

#[test]
fn capacity_refusal_restores_first_message_and_retry_starts_it_once() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat(&mut app);
    let message = "Native chat capacity reached: 128 resident, 0 starting; limit 128";
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure {
            message: message.into(),
            recovery: chat::Recovery::Capacity,
        }),
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.is_draft());
    assert!(runtime.capacity_refused);
    assert_eq!(runtime.error.as_deref(), Some(message));
    assert_eq!(runtime.composer.text(), "hello");
    assert!(runtime.queued.is_none());
    assert!(!app.chat_offers_fresh_conversation(card));
    let labels = capacity_recovery_labels(&app, card);
    assert!(labels.iter().any(|label| label == "Retry"), "{labels:?}");
    assert!(
        labels
            .iter()
            .any(|label| label.contains("cagents chat capacity")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .all(|label| !label.contains("daemon may still be working")),
        "{labels:?}"
    );
    let retry = runtime.target;
    submit_target(&mut app, retry);
    let attempt = chat_runtime_attempt(&app, card);
    let runtime = chat_runtime(&app, card);
    assert!(!runtime.capacity_refused);
    assert_eq!(
        runtime.queued.as_ref().unwrap().prompt.source.text(),
        "hello"
    );
    assert!(runtime.composer.text().is_empty());
    // A second press while that attempt owns the card cannot enqueue or
    // create another start, even with the old rendered target.
    submit_target(&mut app, retry);
    assert_eq!(chat_runtime_attempt(&app, card), attempt);
    assert_eq!(
        chat_runtime(&app, card)
            .queued
            .as_ref()
            .unwrap()
            .prompt
            .source
            .text(),
        "hello"
    );
}

#[test]
fn capacity_refusal_on_cold_restore_keeps_guidance_identity_and_stashed_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("saved capacity draft"));
    let source = chat_runtime(&app, card).session.clone();
    app.chat_forget(card);
    app.cards.insert(card, CardRuntime::Loading);
    drop(app.open_chat(card, source.clone(), chat::Intent::Restore));
    let target = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure {
            message: "128 resident, 0 starting; limit 128".into(),
            recovery: chat::Recovery::Capacity,
        }),
    );
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
    assert_eq!(app.chat_capacity_guidance(card), Some(CAPACITY_GUIDANCE));
    let labels = capacity_recovery_labels(&app, card);
    assert!(
        labels.iter().any(|label| label.contains("Retry")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.contains("cagents chat release ID --dry-run")),
        "{labels:?}"
    );
    assert_eq!(
        app.chat_ui.drafts.get(&card).unwrap(),
        "saved capacity draft"
    );
    assert_eq!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(source.clone())
    );
    assert!(!app.chat_offers_fresh_conversation(card));
    drop(app.update(Message::RetryCard(card)));
    let retry = chat_runtime_attempt(&app, card);
    assert_eq!(app.chat_ui.attempt(card).unwrap().session, source);
    assert_eq!(app.chat_capacity_guidance(card), None);
    drop(app.update(Message::RetryCard(card)));
    assert_eq!(chat_runtime_attempt(&app, card), retry);
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    answer_start(&mut app, retry, Ok(handle));
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "saved capacity draft"
    );
    assert!(!chat_runtime(&app, card).capacity_refused);
    assert_eq!(app.chat_capacity_guidance(card), None);
    // A delayed old failure cannot replace the recovered attachment or
    // re-offer capacity recovery for its superseded attempt.
    answer_start(
        &mut app,
        target,
        Err(chat::StartFailure {
            message: "stale capacity".into(),
            recovery: chat::Recovery::Capacity,
        }),
    );
    assert!(!chat_runtime(&app, card).capacity_refused);
    assert_eq!(app.chat_capacity_guidance(card), None);
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "saved capacity draft"
    );
}

#[test]
fn capacity_refusal_retry_preserves_saved_identity_and_unsent_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("unsent capacity draft"));
    let source = chat_runtime(&app, card).session.clone();
    drop(app.handle_chat_ui(Action::Reconnect(target)));
    let attempt = chat_runtime_attempt(&app, card);
    answer_start(
        &mut app,
        attempt,
        Err(chat::StartFailure {
            message: "Resident slots full".into(),
            recovery: chat::Recovery::Capacity,
        }),
    );
    let runtime = chat_runtime(&app, card);
    assert!(runtime.capacity_refused);
    assert!(runtime.disconnected);
    assert_eq!(runtime.session, source);
    assert_eq!(runtime.composer.text(), "unsent capacity draft");
    assert!(!app.chat_offers_fresh_conversation(card));
    let labels = capacity_recovery_labels(&app, card);
    assert!(labels.iter().any(|label| label == "Retry"), "{labels:?}");
    assert!(
        labels
            .iter()
            .any(|label| label.contains("cagents chat capacity")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .all(|label| !label.contains("daemon may still be working")),
        "{labels:?}"
    );
    let retry = runtime.target;
    drop(app.handle_chat_ui(Action::Reconnect(retry)));
    assert_eq!(app.chat_ui.attempt(card).unwrap().session, source);
    assert!(!chat_runtime(&app, card).capacity_refused);
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "unsent capacity draft"
    );
}

#[test]
fn a_panic_while_starting_native_chat_is_a_failed_start() {
    let caught = std::panic::catch_unwind(|| -> Result<Arc<ClientHandle>, chat::StartFailure> {
        panic!("attach exploded");
    });
    let Err(failure) = Cagents::started_chat(caught) else {
        panic!("a panic is a start failure");
    };
    assert!(
        failure.message.contains("attach exploded"),
        "the panic payload has to reach the card: {}",
        failure.message
    );
    assert_eq!(failure.recovery, chat::Recovery::Retry);
}

#[test]
fn codex_keeps_its_source_pty_even_when_the_daemon_record_is_not_marked_live() {
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    let mut card = crate::app::tests::terminal_card_state();
    let CardKind::Terminal(terminal) = &mut card.kind else {
        unreachable!();
    };
    terminal.continuity = TerminalContinuity::Live {
        host_session_id: HostSessionId::new("host"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    };
    terminal.launch.program = "claude".into();
    let claude = conversion(terminal).expect("claude converts");
    assert!(!retains_source_pty(&claude));

    // Attach copies the daemon's stored terminal. That record keeps the
    // spawn continuity, which is not Live, while the child is still the
    // native writer's process.
    terminal.launch.program = "codex".into();
    terminal.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: terminal.launch.clone(),
        last_exit: None,
    };
    let attached = conversion(terminal).expect("codex converts");
    assert!(
        retains_source_pty(&attached),
        "a Codex attach must not ShutdownTerminal just because the stored continuity is not Live"
    );
}

/// A message naming several skills goes out as one message, each skill
/// in the provider's own spelling; only a draft that is a command as a
/// whole is dispatched as one.
#[test]
fn a_message_naming_several_skills_is_sent_as_one_message() {
    let skill = |name: &str, sigil: char| chat::Skill {
        name: name.into(),
        description: String::new(),
        invocation: format!("{sigil}{name}"),
    };
    let prompt = |command: Command| match command {
        Command::Prompt { text, .. } => text,
        other => panic!("sent as {other:?}, not a message"),
    };
    let codex = Snapshot {
        skills: vec![skill("commits", '$'), skill("pr-review", '$')],
        capabilities: chat::Capabilities {
            compact: true,
            ..chat::Capabilities::default()
        },
        ..Snapshot::default()
    };
    assert_eq!(
        prompt(
            submission(
                "/commits the fix, then /pr-review it",
                &codex,
                &[],
                PromptMode::Send
            )
            .unwrap()
        ),
        "$commits the fix, then $pr-review it"
    );
    assert!(matches!(
        submission("/compact", &codex, &[], PromptMode::Send),
        Ok(Command::Compact)
    ));

    let claude = Snapshot {
        commands: vec![chat::SlashCommand {
            name: "clear".into(),
            description: String::new(),
        }],
        skills: vec![skill("alpha", '/'), skill("beta", '/')],
        ..Snapshot::default()
    };
    let attachment = [Attachment {
        path: "shot.png".into(),
        reference: None,
    }];
    assert_eq!(
        prompt(
            submission(
                "/alpha this, and /beta that",
                &claude,
                &attachment,
                PromptMode::Send
            )
            .unwrap()
        ),
        "/alpha this, and /beta that"
    );
    // A skill alone is a message Claude Code expands, not a command.
    assert_eq!(
        prompt(submission("/alpha", &claude, &[], PromptMode::Send).unwrap()),
        "/alpha"
    );
    // Claude Code expands a skill only from the message's first
    // character, so blank lines ahead of one are not sent; a message
    // that opens with anything else keeps its indentation.
    assert_eq!(
        prompt(submission("\n  /alpha this", &claude, &[], PromptMode::Send).unwrap()),
        "/alpha this"
    );
    assert_eq!(
        prompt(submission("    let x = 1;\n/beta", &claude, &[], PromptMode::Send).unwrap()),
        "    let x = 1;\n/beta"
    );
    assert!(matches!(
        submission("/clear", &claude, &[], PromptMode::Send),
        Ok(Command::Slash { command }) if command == "/clear"
    ));
    assert!(submission("/clear", &claude, &attachment, PromptMode::Send).is_err());
    // Steering carries words to a running turn, never a command.
    assert_eq!(
        prompt(submission("/clear", &claude, &[], PromptMode::Steer).unwrap()),
        "/clear"
    );
}

#[test]
fn switching_a_terminal_never_opens_a_conversation_chooser() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let mut card = crate::app::tests::terminal_card_state();
    let id = card.id;
    let CardKind::Terminal(terminal) = &mut card.kind else {
        unreachable!()
    };
    terminal.launch.program = "codex".into();
    terminal.continuity = TerminalContinuity::Live {
        host_session_id: crate::model::HostSessionId::new("host"),
        boot_id: crate::model::BootId::new("boot"),
        host_incarnation: crate::model::HostIncarnation::new("incarnation"),
    };
    app.durable = Arc::new(WorkspaceState {
        focused: Some(id),
        cards: vec![card].into(),
        ..WorkspaceState::default()
    });
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    assert!(app.chat_switch_ready(id));
    let _ = app.update(Message::ChatUi(Action::Switch(id)));
    assert!(!app.chat_switch_ready(id));
    assert!(app.chat_switch_blocker(id).unwrap().contains("Connecting"));
    assert!(
        app.notices.entries.is_empty(),
        "progress is shown in the pane, without a warning"
    );
    let target = app.chat_ui.phases[&id].target();
    let _ = app.update(Message::ChatUi(Action::Switch(id)));
    assert_eq!(app.chat_ui.phases[&id].target(), target);
    assert!(app.notices.entries.iter().any(|entry| entry.notice.message
        == "Connecting to native chat. Wait for the connection to finish."));
    let before = app.card_state(id).unwrap().clone();
    let reason = "This Codex terminal does not expose its conversation for native attachment.";
    let task = app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
            reason,
        )))))),
    )));
    assert_eq!(
        app.card_state(id).unwrap(),
        &before,
        "a failed switch preserves the terminal"
    );
    assert!(
        app.chat_switch_ready(id),
        "a failed attachment can be retried"
    );
    // Named as its tab names it.
    let tab = app
        .repository_chrome(app.card_state(id).unwrap())
        .into_owned();
    assert_eq!(
        errors(&app),
        [format!("Could not switch “{tab}” to native chat.")]
    );
    assert_eq!(
        attach_failure_details(&app, id).as_deref(),
        Some(reason),
        "the reason stays readable beneath it"
    );
    assert!(
        matches!(
            crate::app::tests::task_app_message(task),
            Message::ChromeUi(chrome_ui::Action::ShowNoticesFor(card)) if card == id
        ),
        "the failure opens its explanation"
    );
}

#[test]
fn a_chat_switch_names_the_unsent_attachment_blocker_before_clicking() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        unreachable!()
    };
    // An image whose receipt is malformed cannot be saved with the draft.
    runtime
        .attachments
        .push(malformed_image("/tmp/unsent-attachment.png"));
    let reason = "Send or remove the attached images before leaving the chat.";
    assert_eq!(app.chat_switch_blocker(card).as_deref(), Some(reason));
    assert!(!app.chat_switch_ready(card));
    let _ = app.update(Message::ChatUi(Action::Switch(card)));
    assert!(!app.chat_start_pending(card));
    assert!(matches!(
        app.card_state(card).unwrap().kind,
        CardKind::Chat(_)
    ));
    assert!(
        app.notices
            .entries
            .iter()
            .any(|entry| entry.notice.message == reason)
    );
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        unreachable!()
    };
    runtime.attachments.clear();
    assert!(app.chat_switch_ready(card));
}

#[test]
fn retheme_preserves_completed_and_hidden_continuation_release() {
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    let next = crate::settings::EditorSyntaxTheme::Ocean;
    let body = "```rust\nlet value = 1;\n```";
    let code = "let value = 1;\n";
    let mut finished = CachedItem::new(&text(body), theme);
    let mut hidden = CachedItem::updated(&text(body), theme, None, false, true, None);
    hidden.suspend_continuations();
    let item = chat::Item {
        id: "coordinated-continuation".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Agent".into(),
            input: serde_json::json!({"prompt": body, "subagent_type": "Explore"}).to_string(),
            output: String::new(),
            status: chat::ToolStatus::Succeeded,
        },
        started_at: None,
        at: None,
    };
    let coordinated = chat::coordination::recognize(&item, Provider::Claude).unwrap();
    let mut coordinated = CachedItem::updated(
        &item.content,
        theme,
        None,
        false,
        false,
        Some(coordination::Prepared::new(coordinated)),
    );
    for cached in [&mut finished, &mut hidden, &mut coordinated] {
        assert!(!cached.blocks.is_empty());
        assert!(cached.open.is_none());
        cached.rehighlight(next);
        assert!(cached.open.is_none());
        for block in cached.blocks.values_mut() {
            assert!(!block.extend(Some("rust"), code));
        }
    }
    // A palette change does not finish a stream that is still visible.
    let mut live = CachedItem::updated(&text(body), theme, None, false, true, None);
    assert!(live.open.is_some());
    live.rehighlight(next);
    assert!(live.open.is_some());
    assert!(
        live.blocks
            .values_mut()
            .next()
            .unwrap()
            .extend(Some("rust"), code)
    );
}

#[test]
fn hidden_selected_child_resumes_actual_appended_body_after_scratch_release() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("continuation-child"));
    let target = chat_runtime(&app, card).target;
    let handle = chat_runtime(&app, card).handle().unwrap().clone();
    let parent = Arc::new(chat::Item {
        id: "continuation-parent".into(),
        role: chat::Role::Assistant,
        content: text(&"p".repeat(256 * 1024)),
        started_at: None,
        at: None,
    });
    let child = Arc::new(chat::Item {
        id: "continuation-child-item".into(),
        role: chat::Role::Assistant,
        content: text(&"c".repeat(256 * 1024)),
        started_at: None,
        at: None,
    });
    let mut snapshot = Snapshot {
        generation: 10,
        ready: true,
        busy: true,
        items: vec![parent.clone()],
        subagents: vec![chat::Subagent {
            model: None,
            waiting: false,
            detail: String::new(),
            id: "actual-child".into(),
            kind: "task".into(),
            description: String::new(),
            status: chat::ToolStatus::Running,
            background: true,
            items: vec![child.clone()],
        }],
        ..Snapshot::default()
    };
    app.chat_runtime_mut(target).unwrap().show(true);
    handle.publish_snapshot_for_test(snapshot.clone());
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    preparation::drain_fixture(runtime);
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("actual-child".into()))));
    let runtime = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(runtime);
    assert!(runtime.cache[&parent.id].source.retained_tail_capacity() >= 256 * 1024);
    assert!(
        runtime.child_cache[&child.id]
            .source
            .retained_tail_capacity()
            >= 256 * 1024
    );
    runtime.replace_draft(text_editor::Content::with_text("Unsent child draft"));
    let (catalog, bookmark) = runtime.selection_document();
    let (anchor, end) = catalog.endpoints().unwrap();
    *bookmark.borrow_mut() = selection::Bookmark {
        anchor: Some(anchor),
        end: Some(end),
        anchor_prefix: catalog.fingerprint(anchor),
        end_prefix: catalog.fingerprint(end),
    };
    drop(catalog);
    runtime.set_visible(false).unwrap();
    runtime.cool();
    preparation::retirement_barrier();
    assert!(runtime.cache.is_empty());
    assert!(runtime.child_cache.is_empty());
    assert!(runtime.child_cache_context.is_none());
    assert_eq!(runtime.snapshot.transcript_bytes(), 0);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert_eq!(runtime.selected_subagent.as_deref(), Some("actual-child"));
    assert_eq!(runtime.composer.text().trim_end(), "Unsent child draft");
    assert_eq!(bookmark.borrow().anchor, Some(anchor));
    assert_eq!(bookmark.borrow().end, Some(end));
    let appended = format!("{}\nnew child tail", "c".repeat(256 * 1024));
    snapshot.generation += 1;
    snapshot.subagents[0].items[0] = Arc::new(chat::Item {
        content: text(&appended),
        ..(*child).clone()
    });
    // Hidden publications carry metadata only. Reveal first, then install
    // the authoritative complete resnapshot through the client fixture.
    handle.publish_snapshot_for_test(snapshot.clone());
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.set_visible(true).unwrap();
    handle.replace_snapshot_for_test(snapshot.clone());
    runtime.refresh();
    preparation::drain_fixture(runtime);
    let fresh = CachedItem::new(&text(&appended), runtime.syntax_theme);
    assert_eq!(shown(&runtime.child_cache[&child.id]), shown(&fresh));
    assert!(
        runtime.cache.is_empty(),
        "unselected parent stays lazy after rehydration"
    );
    assert_eq!(runtime.selected_subagent.as_deref(), Some("actual-child"));
    assert_eq!(runtime.composer.text().trim_end(), "Unsent child draft");
    assert_eq!(bookmark.borrow().anchor, Some(anchor));
    assert_eq!(bookmark.borrow().end, Some(end));
    let (catalog, _) = runtime.selection_document();
    assert_eq!(catalog.fingerprint(anchor), bookmark.borrow().anchor_prefix);
    assert_eq!(catalog.fingerprint(end), bookmark.borrow().end_prefix);
    snapshot.generation += 1;
    snapshot.subagents[0].status = chat::ToolStatus::Succeeded;
    handle.publish_snapshot_for_test(snapshot);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    preparation::drain_fixture(runtime);
    assert_eq!(
        runtime.child_cache[&child.id]
            .source
            .retained_tail_capacity(),
        0
    );
    assert_eq!(shown(&runtime.child_cache[&child.id]), shown(&fresh));
}

#[test]
fn finished_prose_releases_rewriter_tail_without_reparsing_unchanged_text() {
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    let content = text(&"x".repeat(512 * 1024));
    let mut cached = CachedItem::new(&content, theme);
    assert_eq!(cached.source.retained_tail_capacity(), 0);
    let revision = cached.revision;
    let before = shown(&cached);
    cached.settle(&content, theme);
    cached.settle(&content, theme);
    assert_eq!(cached.revision, revision);
    assert_eq!(shown(&cached), before);
}

#[test]
fn suspended_prose_resumes_to_the_same_full_rendering_and_fences() {
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    let start = "Intro\n\n```rust\nlet value = 1;\n";
    let body = format!("{start}let next = 2;\n```\nUse Vec<Option<α>>");
    let mut cached = CachedItem::updated(&text(start), theme, None, false, true, None);
    let displayed = shown(&cached);
    let revision = cached.revision;
    let spans: Vec<_> = cached
        .blocks
        .iter()
        .map(|(key, block)| {
            (
                *key,
                block.highlight(false).clone(),
                block.highlight(true).clone(),
            )
        })
        .collect();
    cached.suspend_continuations();
    assert_eq!(cached.revision, revision);
    assert_eq!(shown(&cached), displayed);
    for (key, light, dark) in spans {
        assert!(light.ptr_eq(cached.blocks[&key].highlight(false)));
        assert!(dark.ptr_eq(cached.blocks[&key].highlight(true)));
    }
    cached.extend(&body, false, theme);
    let fresh = CachedItem::new(&text(&body), theme);
    assert_eq!(shown(&cached), shown(&fresh));
    assert_eq!(parsed_codes(&cached), parsed_codes(&fresh));
    assert_eq!(cached.source.retained_tail_capacity(), 0);
}

#[test]
fn hidden_prose_releases_graph_and_scratch_and_rehydrates_reader_state() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let snapshot = || Snapshot {
        generation: 17,
        ready: true,
        session_id: Some(KNOWN_THREAD.into()),
        subagents: vec![presentation_child(
            "selected child",
            presentation_text("scratch-owner", &"x".repeat(512 * 1024)),
        )],
        ..Snapshot::default()
    };
    show_chat_snapshot_for_test(&mut app, card, snapshot());
    drop(app.handle_chat_ui(Action::SelectSubagent(
        target,
        Some("selected child".into()),
    )));
    let runtime = app.chat_runtime_mut(target).unwrap();
    preparation::drain_fixture(runtime);
    assert!(
        runtime.child_cache["scratch-owner"]
            .source
            .retained_tail_capacity()
            >= 512 * 1024
    );
    let old_item = Arc::downgrade(&runtime.snapshot.subagents[0].items[0]);
    let client_retired = handle.observe_snapshot_retirement_for_test();
    runtime.replace_draft(text_editor::Content::with_text("unsent reader draft"));
    runtime.find = Some("kept query".into());
    runtime.expanded.edit().insert("scratch-owner".into());
    runtime.opened.edit().insert("kept turn".into(), true);
    runtime.set_visible(false).unwrap();
    runtime.set_visible(false).unwrap();
    runtime.cool();
    preparation::retirement_barrier();
    // A live hidden presentation releases the graph and its parser scratch;
    // a draft has no provider transcript to suspend.
    assert!(runtime.cache.is_empty() && runtime.child_cache.is_empty());
    // The client's publication still holds the old body until it retires
    // on its own worker, which the presentation barrier does not cover.
    client_retired
        .recv_timeout(Duration::from_secs(10))
        .expect("the hidden client's exact publication must retire");
    assert!(old_item.upgrade().is_none());
    assert_eq!(runtime.snapshot.transcript_bytes(), 0);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert_eq!(runtime.unsent_text(), "unsent reader draft");
    assert_eq!(runtime.find.as_deref(), Some("kept query"));
    assert_eq!(runtime.selected_subagent.as_deref(), Some("selected child"));
    assert!(runtime.expanded.contains("scratch-owner"));
    assert_eq!(runtime.opened.get("kept turn"), Some(&true));
    runtime.set_visible(true).unwrap();
    handle.replace_snapshot_for_test(snapshot());
    runtime.refresh();
    preparation::drain_fixture(runtime);
    let cached = &runtime.child_cache["scratch-owner"];
    assert!(!cached.placeholder);
    assert_eq!(cached.markdown.items().len(), 1);
    assert_eq!(
        cached.item.as_ref().unwrap().content,
        text(&"x".repeat(512 * 1024))
    );
    assert_eq!(runtime.unsent_text(), "unsent reader draft");
    assert_eq!(runtime.find.as_deref(), Some("kept query"));
    assert!(runtime.expanded.contains("scratch-owner"));
    assert_eq!(runtime.opened.get("kept turn"), Some(&true));
}

/// A message that arrives byte by byte must present exactly as one that
/// arrived whole. The incremental fence scan is the only difference, and a
/// mistake in it would show the wrong code behind a copy control rather
/// than fail loudly.
#[test]
fn streamed_body_finds_the_same_code_blocks_as_a_whole_one() {
    const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
    const BODY: &str =
        "Intro\n\n```rust\nfn main() {}\nlet total = 1;\n```\n\nAfter\n\n~~~\nplain\n~~~\n";
    let whole = CachedItem::new(&text(BODY), THEME);
    assert_eq!(
        parsed_codes(&whole),
        vec![
            "fn main() {}\nlet total = 1;\n".to_owned(),
            "plain\n".to_owned()
        ]
    );

    let mut streamed = CachedItem::new(&text(""), THEME);
    for end in 1..=BODY.len() {
        if !BODY.is_char_boundary(end) {
            continue;
        }
        let content = text(&BODY[..end]);
        let Some(body) = streamed
            .token
            .appended(&BODY[..streamed.token.body], &content)
        else {
            panic!("a longer prefix of the same body must read as an append");
        };
        streamed.extend(body, true, THEME);
        streamed.token = ChangeToken::of(&content);
    }
    streamed.settle(&text(BODY), THEME);
    assert_eq!(parsed_codes(&streamed), parsed_codes(&whole));
    assert_eq!(streamed.token, whole.token);
    assert_eq!(
        streamed.blocks.keys().collect::<HashSet<_>>(),
        whole.blocks.keys().collect::<HashSet<_>>(),
        "the streamed fences are highlighted under the same identities"
    );
}

/// The fence a stream is still writing is continued from its last whole
/// line on every refresh. However the stream is cut - through a line, a
/// comment that spans several, or the fence that closes the block - each
/// block must end up drawing exactly what the whole message's does, in
/// both palettes.
#[test]
fn streamed_fences_draw_what_a_whole_message_does() {
    const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
    const BODY: &str = "Intro\n\n```rust\n/* a comment\n   over lines */ fn main() {\n    \
             let text = \"a string\n        over two\";\n}\n```\n\nBetween\n\n\
             - a list\n\n  ```sh\n  echo \"one\"\r\n  ls # caf\u{e9}\n  ```\n\n> ```rust\n> let quoted = 1;\n> ```\n\n\
             ```rust\nfn main() {}\n```\n\n```rust\nfn main() {}\n```\n\n~~~\nplain\n";
    let whole = CachedItem::new(&text(BODY), THEME);
    assert_eq!(parsed_codes(&whole).len(), 6);
    assert_eq!(whole.blocks.len(), 5, "two of the fences are the same");
    let boundaries: Vec<usize> = (1..=BODY.len())
        .filter(|end| BODY.is_char_boundary(*end))
        .collect();
    for step in [1, 2, 3, 5, 7, 11, 48, 97] {
        let mut streamed = CachedItem::new(&text(""), THEME);
        for end in boundaries
            .iter()
            .copied()
            .filter(|end| end % step == 0 || *end == BODY.len())
        {
            let content = text(&BODY[..end]);
            let body = streamed
                .token
                .appended(&BODY[..streamed.token.body], &content)
                .expect("an append");
            streamed.extend(body, true, THEME);
            streamed.token = ChangeToken::of(&content);
        }
        streamed.settle(&text(BODY), THEME);
        assert_eq!(parsed_codes(&streamed), parsed_codes(&whole));
        assert_eq!(streamed.blocks.len(), whole.blocks.len());
        for (key, block) in &whole.blocks {
            let streamed = streamed.blocks.get(key).expect("the same fence");
            for dark in [false, true] {
                assert_eq!(
                    streamed.highlight(dark),
                    block.highlight(dark),
                    "in pieces of {step}"
                );
            }
        }
    }
}

/// A fence that names its language is highlighted in both of the
/// editor's palettes the moment it is cached, as one paragraph whose lines
/// are joined the way the fence wrote them, and looked up by exactly what
/// the Markdown hands the renderer; a fence without one is cached to draw
/// plain rather than guessed at.
#[test]
fn a_named_fence_is_highlighted_in_both_palettes() {
    const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
    let cached = CachedItem::new(
        &text("```rust\nfn main() {}\nlet x = 1;\n```\n\n```\nplain\n```\n"),
        THEME,
    );
    let text_of = |spans: &[iced::widget::text::Span<'static, markdown::Uri>]| {
        spans
            .iter()
            .map(|span| span.text.as_ref())
            .collect::<String>()
    };
    let code = "fn main() {}\nlet x = 1;\n";
    let block = cached
        .block(Some("rust"), code)
        .expect("the Rust fence is cached");
    for dark in [false, true] {
        let spans = block.highlight(dark).all(code);
        assert_eq!(text_of(&spans), code);
        assert!(
            spans.iter().any(|span| span.color.is_some()),
            "a keyword takes a color"
        );
    }
    let plain = cached
        .block(None, "plain\n")
        .expect("an unnamed fence is cached too");
    let spans = plain.highlight(true).all("plain\n");
    assert_eq!(text_of(&spans), "plain\n");
    assert!(spans.iter().all(|span| span.color.is_none()));
    assert!(cached.block(Some("rust"), "fn other() {}\n").is_none());
}

/// Everything an item's Markdown would put on screen, block by block, as
/// the reader would read it.
fn shown(cached: &CachedItem) -> String {
    fn walk(items: &[markdown::Item], style: markdown::Style, out: &mut String) {
        let line = |text: &markdown::Text, out: &mut String| {
            for span in text.spans(style).iter() {
                out.push_str(&span.text);
            }
            out.push('\n');
        };
        for item in items {
            match item {
                markdown::Item::Heading(_, text) | markdown::Item::Paragraph(text) => {
                    line(text, out)
                }
                markdown::Item::CodeBlock { language, code, .. } => {
                    out.push_str(&format!("[{}]\n{code}", language.as_deref().unwrap_or("")));
                }
                markdown::Item::List { bullets, .. } => {
                    for bullet in bullets {
                        let (markdown::Bullet::Point { items }
                        | markdown::Bullet::Task { items, .. }) = bullet;
                        out.push_str("* ");
                        walk(items, style, out);
                    }
                }
                markdown::Item::Quote(items) => walk(items, style, out),
                markdown::Item::Rule => out.push_str("----\n"),
                markdown::Item::Table { columns, rows } => {
                    for column in columns {
                        walk(&column.header, style, out);
                    }
                    for cell in rows.iter().flat_map(|row| &row.cells) {
                        walk(cell, style, out);
                    }
                }
                markdown::Item::Image { alt, .. } => line(alt, out),
            }
        }
    }
    let mut out = String::new();
    walk(
        cached.markdown.items(),
        markdown::Style::from(iced::Theme::Dark),
        &mut out,
    );
    out
}

/// The items a parse produced, in full but for the per-line highlighting
/// iced keeps on a code block, which the transcript never draws and
/// which depends on how the block was streamed.
fn outline(items: &[markdown::Item]) -> String {
    items
        .iter()
        .map(|item| match item {
            markdown::Item::CodeBlock { language, code, .. } => {
                format!("CodeBlock({language:?}, {code:?})")
            }
            markdown::Item::List { start, bullets } => {
                let bullets = bullets
                    .iter()
                    .map(|bullet| match bullet {
                        markdown::Bullet::Point { items } => outline(items),
                        markdown::Bullet::Task { items, done } => {
                            format!("{done}:{}", outline(items))
                        }
                    })
                    .collect::<Vec<_>>();
                format!("List({start:?}, {bullets:?})")
            }
            markdown::Item::Quote(items) => format!("Quote({})", outline(items)),
            markdown::Item::Paragraph(text) => {
                format!("Paragraph({:?})", text.semantic_spans())
            }
            markdown::Item::Heading(level, text) => {
                format!("Heading({level:?}, {:?})", text.semantic_spans())
            }
            markdown::Item::Image {
                url,
                title,
                title_text,
                alt,
            } => format!(
                "Image({url:?}, {title:?}, {:?}, {:?})",
                title_text.semantic_spans(),
                alt.semantic_spans()
            ),
            markdown::Item::Rule => "Rule".into(),
            markdown::Item::Table { columns, rows } => format!(
                "Table({:?}, {:?})",
                columns
                    .iter()
                    .map(|column| (column.alignment, outline(&column.header)))
                    .collect::<Vec<_>>(),
                rows.iter()
                    .map(|row| row
                        .cells
                        .iter()
                        .map(|cell| outline(cell))
                        .collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What an agent writes reaches the reader: a separator does not swallow
/// the section under it, a generic keeps its parameters, and an indented
/// block is a code block with its code in it.
#[test]
fn an_answer_keeps_what_the_parser_would_drop() {
    const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
    for (body, expected) in [
        (
            "Intro\n\n---\nSecond part\nstill second\n\n---\nThird part\n",
            "Intro\n----\nSecond part still second\n----\nThird part\n",
        ),
        ("+++\nkept\n+++\nafter\n", "+++ kept +++ after\n"),
        (
            "Returns Vec<Option<String>> or HashMap<K, V>.\n",
            "Returns Vec<Option<String>> or HashMap<K, V>.\n",
        ),
        (
            "<details>\n<summary>More</summary>\n\nbody\n</details>\n",
            "<details> <summary>More</summary>\nbody </details>\n",
        ),
        ("Press <kbd>Ctrl</kbd> now\n", "Press <kbd>Ctrl</kbd> now\n"),
        ("one<br>two<br/>three\n", "one\ntwo\nthree\n"),
        (
            "See <https://example.com/a> and `Vec<T>`.\n",
            "See https://example.com/a and Vec<T>.\n",
        ),
        (
            "Run:\n\n    cargo test\n    cargo fmt\n\nThen stop.\n",
            "Run:\n[]\ncargo test\ncargo fmt\nThen stop.\n",
        ),
        (
            "```rust\nlet x: Vec<T> = a < b;\n---\n```\n",
            "[rust]\nlet x: Vec<T> = a < b;\n---\n",
        ),
        (
            "| a | b |\n|---|---|\n| one<br>two | Option<T> |\n",
            "a\nb\none\ntwo\nOption<T>\n",
        ),
        // A path in a generic is not a link to its first segment.
        (
            "Takes Vec<std::string> and Option<std::path::PathBuf>.\n",
            "Takes Vec<std::string> and Option<std::path::PathBuf>.\n",
        ),
        // A code span that wraps is code on both lines.
        (
            "Call `get::<T>\n(key)` on Map<K>\n",
            "Call get::<T> (key) on Map<K>\n",
        ),
        // A quoted fence inside a fence is code, not the fence's end.
        (
            "```\n> ```\nVec<T>\n---\n```\nAfter <a>\n",
            "[]\n> ```\nVec<T>\n---\nAfter <a>\n",
        ),
        // A fence ends with the list item it is in, and one at the
        // margin after it is a fence of its own.
        ("- ```\n  in <a>\nout <b>\n", "* []\nin <a>\nout <b>\n"),
        (
            "- ```\n  in <a>\n```\nnext <b>\n```\nlast <c>\n",
            "* []\nin <a>\n[]\nnext <b>\nlast <c>\n",
        ),
    ] {
        assert_eq!(
            shown(&CachedItem::new(&text(body), THEME)),
            expected,
            "{body:?}"
        );
    }
    let linked = CachedItem::new(&text("See <https://example.com/a>."), THEME);
    assert!(
        format!("{:?}", linked.markdown.items()).contains("link: Some(\"https://example.com/a\")"),
        "an autolink is still a link"
    );
}

/// However an answer is cut into deltas, it parses into the items the
/// whole answer parses into - including where the end of a line was kept
/// back until a late `>` made it an autolink or a line break.
///
/// The bodies hold no list whose items are separated by blank lines:
/// iced's incremental parse starts a second list at such an item, with or
/// without the rewrite, and that is not what this is about.
#[test]
fn a_streamed_answer_parses_as_the_whole_one_does() {
    const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
    for body in [
        "Intro\n\n---\nNext <part>\n\n---\n\nUse Vec<Option<String>> and `Vec<T>`.\n",
        "See <https://example.com/a> or <me@example.com>.\n\n| a | b |\n|---|---|\n| one<br>two | Option<T> |\n",
        "Before\n\n```rust,ignore\nfn a<T>() {}\n---\n```\n\n~~~\n<b>\n~~~\n\nAfter <c>\n",
        "Run:\n\n    cargo test\n\n    cargo fmt\n\nThen:\n\n1. One\n\n   ```sh\n   echo <x>\n   ```\n\nDone <kbd>k</kbd>\n",
        "<details>\n<summary>S</summary>\n\nλ body\n</details>\n\n+++\nlast\n",
    ] {
        let whole = CachedItem::new(&text(body), THEME);
        let boundaries: Vec<usize> = (1..=body.len())
            .filter(|end| body.is_char_boundary(*end))
            .collect();
        // A character at a time, then in deltas of a few uneven sizes.
        let mut cuts = vec![boundaries.clone()];
        for stride in [2, 3, 5, 11] {
            let mut cut: Vec<usize> = boundaries.iter().copied().step_by(stride).collect();
            cut.push(body.len());
            cuts.push(cut);
        }
        for cut in cuts {
            let mut streamed = CachedItem::new(&text(""), THEME);
            for end in cut {
                let content = text(&body[..end]);
                if streamed.token == ChangeToken::of(&content) {
                    continue;
                }
                let grown = streamed
                    .token
                    .appended(&body[..streamed.token.body], &content)
                    .expect("a longer prefix of the same body reads as an append");
                streamed.extend(grown, true, THEME);
                streamed.token = ChangeToken::of(&content);
            }
            streamed.settle(&text(body), THEME);
            assert_eq!(
                outline(streamed.markdown.items()),
                outline(whole.markdown.items()),
                "{body:?}"
            );
            assert_eq!(
                streamed.blocks.keys().collect::<HashSet<_>>(),
                whole.blocks.keys().collect::<HashSet<_>>()
            );
        }
    }
}

/// A fence's info string names its language first and says other things
/// after; the grammar and the caption both go by the language.
#[test]
fn a_fence_is_highlighted_by_the_first_word_of_its_info_string() {
    const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
    for (info, language) in [
        ("rust", Some("rust")),
        ("rust,ignore", Some("rust")),
        ("ts title=\"x.ts\"", Some("ts")),
        ("  python  {1,3}", Some("python")),
        (",", None),
        ("", None),
    ] {
        assert_eq!(prose::fence_language(info), language, "{info:?}");
    }
    let cached = CachedItem::new(&text("```rust,ignore\nfn main() {}\n```\n"), THEME);
    let block = cached
        .block(Some("rust,ignore"), "fn main() {}\n")
        .expect("the fence is cached under the info string the parser hands over");
    assert!(
        block
            .highlight(true)
            .all("fn main() {}\n")
            .iter()
            .any(|span| span.color.is_some()),
        "a keyword takes a color"
    );
}

/// Parser continuation requires the complete previous prefix and matching
/// presentation flags, rather than a bounded digest of that prefix.
#[test]
fn change_token_refuses_a_rewritten_body() {
    const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
    let cached = CachedItem::new(&text("hello world"), THEME);
    assert_eq!(
        cached
            .token
            .appended("hello world", &text("hello world and more")),
        Some("hello world and more")
    );
    assert!(
        cached
            .token
            .appended("hello world", &text("HELLO WORLD and more"))
            .is_none()
    );
    assert!(
        cached
            .token
            .appended("hello world", &text("hello world"))
            .is_none()
    );
    assert!(
        cached
            .token
            .appended("hello world", &text("hello"))
            .is_none()
    );
    assert!(
        cached
            .token
            .appended(
                "hello world",
                &ItemContent::Text {
                    text: "hello world and more".to_owned(),
                    thinking: true,
                    phase: chat::Phase::Final,
                }
            )
            .is_none(),
        "thinking text is collapsed differently and is not the same item"
    );
    assert!(
        cached
            .token
            .appended(
                "hello world",
                &ItemContent::Text {
                    text: "hello world and more".to_owned(),
                    thinking: false,
                    phase: chat::Phase::Commentary,
                }
            )
            .is_none(),
        "narration and an answer are typeset differently, so a phase change \
             is not an append"
    );
}

/// Same-length edits are what a length-only token would miss.
#[test]
fn change_token_separates_same_length_tool_output() {
    let tool = |output: &str| ItemContent::Tool {
        name: "shell".to_owned(),
        input: "ls".to_owned(),
        output: output.to_owned(),
        status: chat::ToolStatus::Running,
    };
    assert_ne!(
        ChangeToken::of(&tool("aaaa")),
        ChangeToken::of(&tool("aaab"))
    );
    assert_eq!(
        ChangeToken::of(&tool("aaaa")),
        ChangeToken::of(&tool("aaaa"))
    );
}

/// A copy control that resolves to nothing is indistinguishable from a
/// broken one, and a command's output has no other way out of the
/// transcript.
#[test]
fn every_step_a_copy_control_is_offered_on_resolves_to_text() {
    let command = ItemContent::Command {
        label: "Ran tests".to_owned(),
        command: "cargo test".to_owned(),
        working_directory: "/tmp".to_owned(),
        action: chat::CommandAction::Run,
        status: chat::ToolStatus::Succeeded,
        exit_code: Some(0),
        duration_ms: Some(12),
        output: "ok. 3 passed".to_owned(),
    };
    assert_eq!(
        copy_text(CopyPart::Text, &command).as_deref(),
        Some("cargo test\n\nok. 3 passed")
    );
    assert_eq!(
        copy_text(CopyPart::Input, &command).as_deref(),
        Some("cargo test")
    );
    assert_eq!(
        copy_text(CopyPart::Output, &command).as_deref(),
        Some("ok. 3 passed")
    );
    let changes = ItemContent::Changes {
        changes: vec![chat::FileChange {
            path: "src/app.rs".to_owned(),
            diff: "@@\n-old\n+new\n".to_owned(),
            kind: chat::ChangeKind::Updated,
        }],
        status: chat::ToolStatus::Succeeded,
    };
    assert_eq!(
        copy_text(CopyPart::Text, &changes).as_deref(),
        Some("@@\n-old\n+new\n")
    );
    let tool = ItemContent::Tool {
        name: "write".to_owned(),
        input: "{}".to_owned(),
        output: "done".to_owned(),
        status: chat::ToolStatus::Succeeded,
    };
    assert_eq!(
        copy_text(CopyPart::Text, &tool).as_deref(),
        Some("{}\n\ndone")
    );
    assert_eq!(
        copy_text(
            CopyPart::Text,
            &ItemContent::WebSearch {
                query: "iced scrollable".to_owned(),
                results: vec![chat::SearchResult {
                    title: "Scrollable".to_owned(),
                    url: "https://docs.rs/iced".to_owned(),
                    snippet: String::new(),
                }],
            }
        )
        .as_deref(),
        Some("iced scrollable\n\nScrollable\nhttps://docs.rs/iced")
    );
}

/// The detail prints a rename's source path, so a token that ignores it
/// would leave the stale path on screen after an in-place patch update.
#[test]
fn change_token_separates_renames_that_differ_only_in_their_source() {
    let renamed = |from: &str| ItemContent::Changes {
        changes: vec![chat::FileChange {
            path: "src/new.rs".to_owned(),
            diff: String::new(),
            kind: chat::ChangeKind::Renamed {
                from: from.to_owned(),
            },
        }],
        status: chat::ToolStatus::Succeeded,
    };
    assert_ne!(
        ChangeToken::of(&renamed("src/old.rs")),
        ChangeToken::of(&renamed("src/older.rs"))
    );
    assert_eq!(
        ChangeToken::of(&renamed("src/old.rs")),
        ChangeToken::of(&renamed("src/old.rs"))
    );
}
#[tokio::test]
async fn reported_file_action_opens_the_filesystem_target_through_symlink_parent_components() {
    use cosmic::iced::futures::StreamExt;
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    let external = directory.path().join("external");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(external.join("child")).unwrap();
    std::os::unix::fs::symlink(external.join("child"), project.join("link")).unwrap();
    std::fs::write(project.join("target.txt"), "wrong lexical target").unwrap();
    std::fs::write(external.join("target.txt"), "reported filesystem target").unwrap();
    let reported = project.join("link/../target.txt");
    assert_eq!(
        std::fs::read_to_string(&reported).unwrap(),
        "reported filesystem target"
    );
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.settings.editor_auto_save = false;
    app.workspace.resize_viewport(1200.0);
    let card = live_chat(&mut app, Some("reported-file-route"));
    let target = chat_runtime(&app, card).target;
    let task = app.handle_chat_ui(Action::OpenChangedFile(target, reported.clone()));
    let mut pending = vec![task];
    while let Some(task) = pending.pop() {
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(
                    message @ (Message::Repository(_)
                    | Message::ChatUi(Action::ChangedFileResolved(..))),
                )) = action
                {
                    pending.push(app.update(message));
                }
            }
        }
    }
    let editor = app.workspace.focused().unwrap();
    assert_ne!(editor, card);
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&editor) else {
        panic!("reported file must open in the native editor");
    };
    assert_eq!(
        runtime.editor_text().as_deref(),
        Some("reported filesystem target")
    );
    assert_eq!(
        runtime
            .descriptor
            .worktree
            .join(runtime.selected_path().unwrap()),
        reported.canonicalize().unwrap()
    );
    assert!(
        runtime.inspector_context().file_error.is_none(),
        "the canonical file pane must finish listing without a symlink-root error"
    );
    let before = app.workspace.cards().len();
    drop(app.handle_chat_ui(Action::OpenChangedFile(
        target,
        PathBuf::from("/invalid\0path"),
    )));
    assert_eq!(app.workspace.cards().len(), before);
    assert_eq!(app.workspace.focused(), Some(editor));
}
#[tokio::test]
async fn retired_chat_cannot_open_a_file_when_its_filesystem_resolution_returns() {
    use cosmic::iced::futures::StreamExt;
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file.txt");
    std::fs::write(&file, "file").unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("retired-file-open"));
    let target = chat_runtime(&app, card).target;
    let before = app.workspace.cards().len();
    let task = app.handle_chat_ui(Action::OpenChangedFile(target, file));
    app.chat_runtime_mut(target).unwrap().target.generation += 1;
    if let Some(mut stream) = iced::runtime::task::into_stream(task) {
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                drop(app.update(message));
            }
        }
    }
    assert_eq!(app.workspace.cards().len(), before);
    assert_eq!(app.workspace.focused(), Some(card));
}

#[test]
fn file_review_replies_finish_while_modal_input_is_blocked() {
    let target = Target {
        card: CardId::new(),
        generation: 1,
    };
    assert!(Action::ChangesLoaded(target, 1, Err("reply".into())).allowed_while_modal());
    assert!(
        Action::ChangedFileResolved(target, 1, "/reported".into(), Err("reply".into()))
            .allowed_while_modal()
    );
    assert!(!Action::OpenChangedFile(target, "/reported".into()).allowed_while_modal());
}
#[test]
fn composer_actions_navigate_dismiss_and_ignore_stale_targets() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("composer-actions"));
    app.workspace.focus(card).unwrap();
    let target = chat_runtime(&app, card).target;
    let snapshot = Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot);
    snapshot.ready = true;
    snapshot.capabilities.branch = true;
    snapshot.capabilities.compact = true;
    // The menu lists its rows in a fixed order; opening it focuses the
    // first one that can be pressed.
    let rows = |app: &Cagents| chat_runtime(app, card).composer_action_rows(false);
    assert_eq!(
        rows(&app),
        [(ComposerRow::Fork, true), (ComposerRow::Compact, true)]
    );
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot)
        .capabilities
        .interrupt_modes = vec!["immediate".into(), "wait".into()];
    Arc::make_mut(&mut runtime.snapshot).busy = true;
    // While a turn runs only steering can be pressed, so focus skips the
    // disabled fork and compact rows rather than landing where nothing
    // shows it.
    assert_eq!(
        rows(&app),
        [
            (ComposerRow::Steering, true),
            (ComposerRow::Fork, false),
            (ComposerRow::Compact, false),
        ]
    );
    assert!(
        chat_runtime(&app, card)
            .composer_action_rows(true)
            .iter()
            .all(|(_, enabled)| !enabled)
    );
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).busy = false;

    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    assert!(chat_runtime(&app, card).composer_actions);
    let stale = Target {
        generation: target.generation + 1,
        ..target
    };
    drop(app.handle_chat_ui(Action::ComposerActions(stale, true)));
    assert!(chat_runtime(&app, card).composer_actions);

    drop(app.handle_chat_ui(Action::BranchPicker(target, true)));
    assert!(chat_runtime(&app, card).branch_picker);
    drop(app.handle_chat_ui(Action::BranchPicker(target, false)));
    assert!(chat_runtime(&app, card).composer_actions);
    assert!(!chat_runtime(&app, card).branch_picker);
    assert!(app.dismiss_chat_picker());
    assert!(!chat_runtime(&app, card).composer_actions);
    assert!(!app.dismiss_chat_picker());

    // Busy chats can inspect the menu, but cannot enter the fork picker.
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).busy = true;
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    drop(app.handle_chat_ui(Action::BranchPicker(target, true)));
    assert!(!chat_runtime(&app, card).branch_picker);
    let key = keyboard::Key::Named(keyboard::key::Named::Escape);
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        },
        captured: false,
    }));
    assert!(!chat_runtime(&app, card).composer_actions);
    assert!(chat_runtime(&app, card).snapshot.busy);

    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).busy = false;
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    drop(app.handle_chat_ui(Action::Command(target, Command::Compact)));
    assert!(!chat_runtime(&app, card).composer_actions);
    assert!(!chat_runtime(&app, card).branch_picker);
}

#[test]
fn composer_menu_names_commands_and_skills_into_the_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("composer-menu"));
    app.workspace.focus(card).unwrap();
    let target = chat_runtime(&app, card).target;
    let snapshot = Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot);
    snapshot.ready = true;
    snapshot.capabilities.compact = true;
    snapshot.commands = vec![
        chat::SlashCommand {
            name: "/review".into(),
            description: "Review the working tree".into(),
        },
        chat::SlashCommand {
            name: "/compact".into(),
            description: "Provider compaction".into(),
        },
    ];
    snapshot.skills = vec![chat::Skill {
        name: "release-notes".into(),
        description: "Draft release notes".into(),
        invocation: "/release-notes".into(),
    }];
    let entries = |app: &Cagents| {
        chat_runtime(app, card)
            .composer_menu(false)
            .into_iter()
            .map(|(entry, enabled)| {
                let label = match entry {
                    MenuEntry::Row(row) => row.id().to_owned(),
                    MenuEntry::Named(name) => name.insertion.into_owned(),
                };
                (label, enabled)
            })
            .collect::<Vec<_>>()
    };
    // The menu's own rows come first; then what slash completion offers
    // an empty draft, without a second compaction entry beside the row.
    assert_eq!(
        entries(&app),
        [
            (ComposerRow::Compact.id().to_owned(), true),
            ("/review".to_owned(), true),
            ("/resume".to_owned(), true),
            ("/release-notes".to_owned(), true),
        ]
    );
    assert!(chat_runtime(&app, card).composer_menu_searchable());

    // The search narrows rows by what they do and names by their name.
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    drop(app.handle_chat_ui(Action::ComposerActionsQuery(target, " SUMMARIZE ".into())));
    assert_eq!(
        entries(&app),
        [(ComposerRow::Compact.id().to_owned(), true)]
    );
    drop(app.handle_chat_ui(Action::ComposerActionsQuery(target, "re".into())));
    assert_eq!(entries(&app)[0].0, "/review");
    // A slash typed out of habit is not part of the name searched for.
    drop(app.handle_chat_ui(Action::ComposerActionsQuery(target, "/Rev".into())));
    assert_eq!(entries(&app), [("/review".to_owned(), true)]);
    // Reopening starts a new search.
    drop(app.handle_chat_ui(Action::ComposerActions(target, false)));
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    assert_eq!(chat_runtime(&app, card).composer_actions_query, "");

    // Enter in the search chooses the first entry it lists: an empty
    // draft takes the command at its head.
    drop(app.handle_chat_ui(Action::ComposerActionsQuery(target, "rev".into())));
    drop(app.handle_chat_ui(Action::ComposerActionsSubmit(target)));
    assert!(!chat_runtime(&app, card).composer_actions);
    assert_eq!(chat_runtime(&app, card).composer.text(), "/review ");

    // Once the draft has words, a command would be sent as words, so the
    // menu offers only skills, named after the caret's word with a space.
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text("summarize the change"));
    runtime.composer_action(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    assert_eq!(
        entries(&app),
        [
            (ComposerRow::Compact.id().to_owned(), true),
            ("/release-notes".to_owned(), true),
        ]
    );
    assert!(chat_runtime(&app, card).composer_menu_searchable());
    drop(app.handle_chat_ui(Action::ComposerName(target, "/review".into())));
    assert!(chat_runtime(&app, card).composer_actions);
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "summarize the change"
    );
    drop(app.handle_chat_ui(Action::ComposerName(target, "/release-notes".into())));
    assert!(!chat_runtime(&app, card).composer_actions);
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "summarize the change /release-notes "
    );

    // A provider that loads only the skill a message opens with gets it
    // at the head instead, and the caret stays where the writing was.
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot)
        .capabilities
        .skill_reach = chat::SkillReach::Leading;
    runtime.replace_draft(text_editor::Content::with_text("summarize\nthe change"));
    runtime.composer_action(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    drop(app.handle_chat_ui(Action::ComposerName(target, "/release-notes".into())));
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert_eq!(
        runtime.composer.text(),
        "/release-notes summarize\nthe change"
    );
    runtime.composer_action(text_editor::Action::Edit(text_editor::Edit::Insert('.')));
    assert_eq!(
        runtime.composer.text(),
        "/release-notes summarize\nthe change."
    );
    // A message opening with a command stays one: the skill goes at the
    // caret, where the hint says it is sent as words.
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text("/review the change"));
    runtime.composer_action(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    drop(app.handle_chat_ui(Action::ComposerName(target, "/release-notes".into())));
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "/review the change /release-notes "
    );
    // Punctuation the draft opens with is set apart from the name.
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text("(quick) notes"));
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    drop(app.handle_chat_ui(Action::ComposerName(target, "/release-notes".into())));
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "/release-notes (quick) notes"
    );
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot)
        .capabilities
        .skill_reach = chat::SkillReach::Anywhere;
    runtime.replace_draft(text_editor::Content::with_text(
        "summarize the change /release-notes ",
    ));

    // Naming changes only the draft, so it stays open during a turn;
    // a stale chat names nothing.
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).busy = true;
    assert!(entries(&app).contains(&("/release-notes".to_owned(), true)));
    let stale = Target {
        generation: target.generation + 1,
        ..target
    };
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    drop(app.handle_chat_ui(Action::ComposerName(stale, "/release-notes".into())));
    drop(app.handle_chat_ui(Action::ComposerActionsSubmit(stale)));
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "summarize the change /release-notes "
    );
}

#[test]
fn composer_menu_plan_row_enters_and_leaves_plan_mode() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat_for_provider(&mut app, Provider::Claude, Some("composer-plan"));
    let target = chat_runtime(&app, card).target;
    let rows = |app: &Cagents| chat_runtime(app, card).composer_action_rows(false);
    // Providers without a plan mode have no row for it.
    assert!(!rows(&app).iter().any(|(row, _)| *row == ComposerRow::Plan));

    let snapshot = Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot);
    snapshot.ready = true;
    snapshot.capabilities.permission_modes = ["default", "acceptEdits", "plan"]
        .map(String::from)
        .to_vec();
    snapshot.permission_mode = Some("acceptEdits".into());
    assert!(rows(&app).contains(&(ComposerRow::Plan, true)));
    let toggle = |app: &Cagents| chat_runtime(app, card).plan_toggle().map(str::to_owned);
    assert_eq!(toggle(&app).as_deref(), Some(PLAN_MODE));

    // Choosing the row closes the menu and asks for plan mode through
    // the footer's own action; leaving returns to the replaced mode.
    drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
    drop(app.handle_chat_ui(Action::ComposerActionsQuery(target, "plan".into())));
    drop(app.handle_chat_ui(Action::ComposerActionsSubmit(target)));
    assert!(!chat_runtime(&app, card).composer_actions);
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).permission_mode =
        Some(PLAN_MODE.into());
    assert_eq!(toggle(&app).as_deref(), Some("acceptEdits"));
    // Choosing plan again while in it keeps the mode to return to.
    drop(app.handle_chat_ui(Action::PermissionMode(target, PLAN_MODE.into())));
    assert_eq!(toggle(&app).as_deref(), Some("acceptEdits"));

    // A mode the provider no longer offers falls back to its default.
    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot)
        .capabilities
        .permission_modes = ["default", "plan"].map(String::from).to_vec();
    assert_eq!(toggle(&app).as_deref(), Some("default"));
}

#[test]
fn inline_fork_and_file_review_actions_never_leave_both_panels_open() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("review-picker-exclusion"));
    let target = chat_runtime(&app, card).target;
    let snapshot = Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot);
    snapshot.ready = true;
    snapshot.capabilities.branch = true;
    app.chat_runtime_mut(target).unwrap().branch_picker = true;
    drop(app.handle_chat_ui(Action::ChangesPicker(target, true)));
    assert!(chat_runtime(&app, card).changes.open);
    assert!(!chat_runtime(&app, card).branch_picker);
    drop(app.handle_chat_ui(Action::BranchPicker(target, true)));
    assert!(!chat_runtime(&app, card).changes.open);
    assert!(chat_runtime(&app, card).branch_picker);
}
#[test]
fn auth_recovery_rejects_stale_wrong_account_and_unknown_home_actions() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.error = Some("Codex RPC error: Not logged in".into());
    runtime.replace_draft(text_editor::Content::with_text("unsent recovery draft"));
    let launch = runtime.session.launch.clone();
    let wrong = app.account_ui.add_managed_fixture(Provider::Codex, 9901);
    let wrong = app.known_account(&wrong).unwrap();
    assert!(app.chat_auth_recovery_enabled(target));
    drop(app.handle_chat_ui(Action::SignIn(target, wrong)));
    let stale = Target {
        generation: target.generation + 1,
        ..target
    };
    drop(app.handle_chat_ui(Action::SignIn(stale, Account::system(Provider::Codex))));
    drop(app.handle_chat_ui(Action::ResumeAfterSignIn(stale)));
    assert_eq!(chat_runtime(&app, card).session.launch, launch);
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "unsent recovery draft"
    );
    assert!(app.chat_ui.attempt(card).is_none());
    app.chat_runtime_mut(target)
        .unwrap()
        .session
        .launch
        .recipe
        .environment
        .insert(
            "CODEX_HOME".into(),
            "/unregistered/auth-recovery-home".into(),
        );
    assert!(!app.chat_auth_recovery_enabled(target));
    assert!(app.chat_auth_recovery(chat_runtime(&app, card)).is_none());
    drop(app.handle_chat_ui(Action::SignIn(target, Account::system(Provider::Codex))));
    drop(app.handle_chat_ui(Action::ResumeAfterSignIn(target)));
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        "unsent recovery draft"
    );
    assert!(app.chat_ui.attempt(card).is_none());
}
/// An ACP provider reports a missing sign-in when its conversation starts,
/// not during a turn, so the card goes back to its draft. That draft offers
/// the sign-in a started conversation gets, on the system account a provider
/// with no home of its own runs on, and sends its message again after.
#[test]
fn a_draft_whose_start_needs_sign_in_offers_sign_in_and_sends_again() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat_for_provider(&mut app, Provider::Muse);
    let handle = fixture_handle(&app, card, None);
    // The adapter has not yet said it takes commands.
    handle.replace_snapshot_for_test(Snapshot::default());
    answer_start(&mut app, target, Ok(handle.clone()));
    let target = chat_runtime(&app, card).target;
    let failure = format!(
        "{}: sign in to Muse with `muse login`, then try again. (Authentication required)",
        chat::ACP_SIGN_IN_PREFIX
    );
    handle.replace_snapshot_for_test(Snapshot {
        error: Some(failure.clone()),
        exited: true,
        ..Snapshot::default()
    });
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
    retired(&mut app, card);
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing());
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert_eq!(runtime.error.as_deref(), Some(failure.as_str()));
    let target = runtime.target;
    let system = Account::system(Provider::Muse);
    let recovery = app
        .chat_auth_recovery(chat_runtime(&app, card))
        .expect("the draft offers sign-in");
    assert_eq!(recovery.account, system);
    assert!(recovery.draft);
    assert!(app.chat_auth_recovery_enabled(target));
    assert!(app.chat_auth_resume_enabled(target));

    // Sending again is the draft's own first message, not a resume of a
    // conversation it never had.
    start_with(&mut app, target, Action::ResumeAfterSignIn(target));
    let attempt = app
        .chat_ui
        .attempt(card)
        .expect("sending again starts the conversation anew");
    assert_ne!(attempt.target, target);
    assert!(chat_runtime(&app, card).queued.is_some());
}

/// Recovery needs the draft's message: with nothing left to send, signing in
/// is still offered but sending again is not.
#[test]
fn a_signed_out_draft_with_nothing_to_send_offers_only_sign_in() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat_in(&mut app, Provider::Muse, Path::new(FIXTURE_DIRECTORY));
    let target = chat_runtime(&app, card).target;
    app.chat_runtime_mut(target).unwrap().error = Some(format!(
        "{}: sign in to Muse with `muse login`, then try again. (log in first)",
        chat::ACP_SIGN_IN_PREFIX
    ));
    assert!(app.chat_auth_recovery_enabled(target));
    assert!(!app.chat_auth_resume_enabled(target));
    drop(app.handle_chat_ui(Action::ResumeAfterSignIn(target)));
    assert!(app.chat_ui.attempt(card).is_none());
    // Overriding HOME still hides which sign-in the provider would read.
    app.chat_runtime_mut(target)
        .unwrap()
        .session
        .launch
        .recipe
        .environment
        .insert("HOME".into(), "/unknown/muse-recovery-home".into());
    assert!(app.chat_auth_recovery(chat_runtime(&app, card)).is_none());
}

/// A start that stops for any other reason keeps its plain report.
#[test]
fn a_draft_whose_start_fails_otherwise_offers_no_sign_in() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target) = starting_chat_for_provider(&mut app, Provider::Muse);
    let handle = fixture_handle(&app, card, None);
    // The adapter has not yet said it takes commands.
    handle.replace_snapshot_for_test(Snapshot::default());
    answer_start(&mut app, target, Ok(handle.clone()));
    let target = chat_runtime(&app, card).target;
    handle.replace_snapshot_for_test(Snapshot {
        error: Some("fixture: adapter crashed".into()),
        exited: true,
        ..Snapshot::default()
    });
    let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.choosing());
    assert_eq!(runtime.composer.text().trim(), "hello");
    assert_eq!(
        runtime.error.as_deref(),
        Some(
            "The conversation stopped before it took the first message. Its last report: fixture: adapter crashed"
        )
    );
    assert!(runtime.auth_error().is_none());
    assert!(app.chat_auth_recovery(runtime).is_none());
    assert!(!app.chat_auth_recovery_enabled(runtime.target));
}
#[test]
fn auth_recovery_reconnect_preserves_session_and_draft_and_rejects_resolved_error() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text("keep this unsent"));
    let launch = runtime.session.launch.clone();
    drop(app.handle_chat_ui(Action::ResumeAfterSignIn(target)));
    assert!(app.chat_ui.attempt(card).is_none());
    app.chat_runtime_mut(target).unwrap().disconnected = true;
    app.chat_runtime_mut(target).unwrap().error = Some("Codex RPC error: Not logged in".into());
    drop(app.handle_chat_ui(Action::ResumeAfterSignIn(target)));
    assert!(app.chat_ui.attempt(card).is_some());
    assert_eq!(chat_runtime(&app, card).session.launch, launch);
    assert_eq!(chat_runtime(&app, card).composer.text(), "keep this unsent");
    assert!(chat_runtime(&app, card).queued.is_none());
    let reconnect = app.chat_ui.attempt(card).unwrap().target;
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    answer_start(&mut app, reconnect, Ok(handle));
    assert_eq!(chat_runtime(&app, card).session.launch, launch);
    assert_eq!(chat_runtime(&app, card).composer.text(), "keep this unsent");
    assert!(chat_runtime(&app, card).queued.is_none());
}
#[test]
fn auth_recovery_notice_dedup_is_scoped_to_its_turn() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (_, target, _) = history_fixture(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let message = "Codex RPC error: Not logged in".to_owned();
    runtime.note(message.clone());
    runtime.note(message.clone());
    assert_eq!(runtime.failures.len(), 1);
    runtime.error = Some(message.clone());
    assert!(runtime.active_auth_notice(&runtime.failures[0].message, &message));
    // A later exchange must be able to report the same failure again.
    let next = chat::Item {
        id: "next-auth-exchange".into(),
        role: chat::Role::User,
        content: ItemContent::Text {
            text: "next prompt".into(),
            thinking: false,
            phase: chat::Phase::default(),
        },
        at: None,
        started_at: None,
    };
    let mut snapshot = (*runtime.snapshot).clone();
    snapshot.generation += 1;
    snapshot.items = vec![Arc::new(next)];
    runtime.install_snapshot(Arc::new(snapshot), None);
    runtime.note(message.clone());
    assert_eq!(runtime.failures.len(), 2);
    assert!(!runtime.active_auth_notice(&runtime.failures[0].message, &message));
    assert!(runtime.active_auth_notice(&runtime.failures[1].message, &message));
    runtime.error = None;
    assert!(!runtime.active_auth_notice(&runtime.failures[1].message, &message));
}
#[test]
fn auth_recovery_requires_the_recipe_to_bind_a_known_account_home() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    let system = Account::system(Provider::Codex);
    let home = system.resolved_home().unwrap();
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.error = Some("Codex RPC error: Not logged in".into());
    runtime.replace_draft(text_editor::Content::with_text("preserved account draft"));
    let mut recipe = runtime.session.launch.recipe.clone();
    recipe
        .environment
        .insert("CODEX_HOME".into(), home.display().to_string());
    runtime.session.launch.recipe = recipe.clone();
    assert!(app.chat_auth_recovery_enabled(target));
    for (variable, value, unset) in [
        ("CODEX_HOME", None, true),
        ("CODEX_HOME", Some(""), false),
        ("HOME", Some("/unknown/auth-recovery-home"), false),
        ("HOME", None, true),
    ] {
        let mut changed = recipe.clone();
        changed.environment.remove("CODEX_HOME");
        if let Some(value) = value {
            changed.environment.insert(variable.into(), value.into());
        }
        if unset {
            changed.unset_environment.insert(variable.into());
        }
        app.chat_runtime_mut(target).unwrap().session.launch.recipe = changed;
        assert!(app.chat_auth_recovery(chat_runtime(&app, card)).is_none());
        drop(app.handle_chat_ui(Action::SignIn(target, system.clone())));
        drop(app.handle_chat_ui(Action::ResumeAfterSignIn(target)));
        assert!(app.chat_ui.attempt(card).is_none());
        assert_eq!(
            chat_runtime(&app, card).composer.text(),
            "preserved account draft"
        );
    }
    recipe.environment.insert(
        "HOME".into(),
        "/different/home/with/explicit/provider-binding".into(),
    );
    app.chat_runtime_mut(target).unwrap().session.launch.recipe = recipe;
    assert!(app.chat_auth_recovery_enabled(target));
    // Contradictory explicit+unset provider binding is removed by ProcessSpec.
    app.chat_runtime_mut(target)
        .unwrap()
        .session
        .launch
        .recipe
        .unset_environment
        .insert("CODEX_HOME".into());
    assert!(app.chat_auth_recovery(chat_runtime(&app, card)).is_none());
}
#[test]
fn auth_recovery_changed_identity_keeps_sign_in_but_fences_resume() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = history_fixture(&mut app);
    let id = app.account_ui.add_managed_fixture(Provider::Codex, 9902);
    let account = app.known_account(&id).unwrap();
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.session.launch.recipe.environment.insert(
        "CODEX_HOME".into(),
        account.resolved_home().unwrap().display().to_string(),
    );
    runtime.error = Some("Codex RPC error: Not logged in".into());
    runtime.replace_draft(text_editor::Content::with_text("keep expected login draft"));
    let now = crate::chat::now_ms().unwrap();
    for (email, changed) in [
        ("other-dummy@example.test".to_owned(), true),
        (
            format!("{}@example.com", uuid::Uuid::from_u128(9902)),
            false,
        ),
    ] {
        let (owner, ticket) =
            app.account_ui
                .begin_probe_for_test(&id, crate::account::schedule::Trigger::Focus, now);
        drop(app.update(Message::AccountUi(account_ui::Action::Probed {
            owner,
            ticket,
            id: id.clone(),
            outcome: account::probe::Outcome::Answered {
                identity: Some(account::Identity {
                    email: Some(email),
                    organization: None,
                    plan: None,
                    disputed_by: None,
                }),
                catalog: None,
                snapshot: Box::new(account::UsageSnapshot::state(
                    account::UsageStatus::Ok,
                    "",
                    now,
                )),
            },
        })));
        assert!(
            app.chat_auth_recovery_enabled(target),
            "Sign in may recover the expected login"
        );
        assert_eq!(app.chat_auth_resume_enabled(target), !changed);
        if changed {
            drop(app.handle_chat_ui(Action::ResumeAfterSignIn(target)));
            assert!(app.chat_ui.attempt(card).is_none());
            assert_eq!(
                chat_runtime(&app, card).composer.text(),
                "keep expected login draft"
            );
        }
    }
}
#[test]
fn auth_recovery_live_resume_dispatches_verification_without_reconnecting_or_replaying() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let failure = "Codex RPC error: Not logged in";
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.error = Some(failure.into());
    snapshot.busy = false;
    snapshot.exited = false;
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    runtime.replace_draft(text_editor::Content::with_text("unsent recovery draft"));
    let launch = runtime.session.launch.clone();
    drop(app.handle_chat_ui(Action::ResumeAfterSignIn(target)));
    assert!(app.chat_ui.attempt(card).is_none());
    let runtime = chat_runtime(&app, card);
    assert!(runtime.commands.in_flight().is_some());
    assert_eq!(runtime.snapshot.error.as_deref(), Some(failure));
    assert_eq!(runtime.session.launch, launch);
    assert_eq!(runtime.composer.text(), "unsent recovery draft");
    assert!(runtime.queued.is_none());
}
#[test]
fn auth_recovery_panel_reads_latest_verification_detail_from_current_snapshot() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let original = "Authentication required. Please sign in again.";
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.error = Some(chat::sign_in_verification_failure(
        original,
        "Different login: open Accounts to resolve the identity",
    ));
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    runtime.error = Some(original.into());
    assert_eq!(
        runtime.auth_error(),
        runtime.snapshot.error.as_deref(),
        "Resume must capture the authoritative latest failure, not a stale local copy"
    );
    assert_eq!(
        runtime.auth_verification_detail(),
        Some("Different login: open Accounts to resolve the identity")
    );
    assert!(app.chat_auth_recovery(chat_runtime(&app, card)).is_some());
    app.chat_runtime_mut(target).unwrap().error = Some(chat::sign_in_verification_failure(
        original,
        "older verification detail",
    ));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.error = Some(original.into());
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    assert_eq!(runtime.auth_error(), Some(original));
    assert_eq!(
        runtime.auth_verification_detail(),
        None,
        "a new plain failure suppresses older local verification detail"
    );
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.error = None;
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    runtime.error = None;
    assert_eq!(runtime.auth_verification_detail(), None);
}
#[test]
fn verified_idle_auth_recovery_removes_only_current_exchange_warning() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let failure = "Authentication required. Please sign in again.";
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.ready = true;
    snapshot.busy = false;
    snapshot.exited = false;
    snapshot.error = Some(failure.into());
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot.clone());
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    let current = runtime
        .notice_anchor(&runtime.snapshot, &runtime.turns)
        .map(str::to_owned);
    let older = failure::Note {
        message: Some("older-exchange".into()),
        after: Some("older-exchange".into()),
        text: failure.into(),
    };
    runtime.failures.insert(0, older.clone());
    runtime.note("Tool deployment failed with HTTP 401".into());
    runtime.note(chat::sign_in_verification_failure(
        failure,
        "first failed verification",
    ));
    runtime.note(chat::sign_in_verification_failure(
        failure,
        "latest failed verification",
    ));
    assert_eq!(
        runtime
            .failures
            .iter()
            .filter(|note| note.message == current
                && chat::requires_sign_in(Provider::Codex, &note.text))
            .count(),
        1
    );
    let transcript = runtime.snapshot.items.clone();
    snapshot.error = None;
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.snapshot.items, transcript);
    assert_eq!(runtime.failures.len(), 2);
    assert_eq!(runtime.failures[0], older);
    assert!(
        runtime
            .failures
            .iter()
            .any(|note| note.message == current
                && note.text == "Tool deployment failed with HTTP 401")
    );
}

#[test]
fn auth_warning_remains_when_new_work_or_error_prevents_idle_recovery() {
    for transition in 0..3 {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        let failure = "Authentication required. Please sign in again.";
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.ready = true;
        snapshot.busy = false;
        snapshot.error = Some(failure.into());
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot.clone());
        app.chat_runtime_mut(target).unwrap().refresh();
        let recorded = chat_runtime(&app, card).failures.clone();
        snapshot.generation += 1;
        snapshot.error = None;
        match transition {
            0 => snapshot.busy = true,
            1 => {
                snapshot.turn_generation += 1;
                snapshot.items.push(Arc::new(chat::Item {
                    id: "new-auth-turn".into(),
                    role: chat::Role::User,
                    content: text("new work"),
                    at: None,
                    started_at: None,
                }));
            }
            _ => snapshot.error = Some(failure.into()),
        }
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert_eq!(chat_runtime(&app, card).failures, recorded);
    }
}
/// The actual App entry batches even an empty action. Observe its effects
/// rather than assuming that a no-op task has no stream representation.
fn assert_chat_copy_task_has_no_effects(task: Task<Message>, reason: &str) {
    use iced::futures::StreamExt;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), async {
            match iced::runtime::task::into_stream(task) {
                Some(stream) => stream.collect::<Vec<_>>().await,
                None => Vec::new(),
            }
        })
        .await
    });
    // A regression may have started blocking copy work. Bound teardown
    // before asserting: timeout does not cancel an already running worker.
    // The owning process supervisor retains its separate hard deadline.
    runtime.shutdown_timeout(Duration::from_secs(1));
    let effects = result.expect("a rejected copy must settle without asynchronous work");
    assert!(effects.is_empty(), "{reason}");
}

fn assert_chat_copy_task_writes(task: Task<Message>, expected: &str) {
    use iced::futures::StreamExt;
    let copied = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            let mut effects = iced::runtime::task::into_stream(task)
                .expect("valid completion must produce clipboard work");
            tokio::time::timeout(Duration::from_secs(1), async {
                while let Some(effect) = effects.next().await {
                    if let iced::runtime::Action::Clipboard(
                        iced::runtime::clipboard::Action::Write { contents, .. },
                    ) = effect
                    {
                        return contents;
                    }
                }
                panic!("valid completion must emit a real clipboard write");
            })
            .await
            .expect("valid completion clipboard deadline")
        });
    assert_eq!(copied, expected);
}

fn assert_chat_copy_task_has_no_clipboard_write(task: Task<Message>, reason: &str) {
    use iced::futures::StreamExt;
    let effects = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                match iced::runtime::task::into_stream(task) {
                    Some(stream) => stream.collect::<Vec<_>>().await,
                    None => Vec::new(),
                }
            })
            .await
            .expect("deferred copy and its acknowledgement timer must settle")
        });
    assert!(
        effects.iter().all(|effect| !matches!(
            effect,
            iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write { .. })
        )),
        "{reason}"
    );
}

#[test]
fn logical_message_copy_uses_aligned_scope_and_never_loaded_fragment_fallback() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let parent = presentation_text("same:opaque:id", "loaded final 45 KiB fragment");
    let child = presentation_text("same:opaque:id", "child loaded fragment");
    let source = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: chat::TextCopyScope::Conversation,
        token: 7,
        revision: 9,
        bytes: 300 * 1024,
    };
    let mut child_source = source.clone();
    child_source.scope = chat::TextCopyScope::Subagent { id: "child".into() };
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.items = vec![parent];
    snapshot.subagents = vec![presentation_child("child", child)];
    snapshot.text_copy_sources.conversation = vec![Some(source.clone())];
    snapshot
        .text_copy_sources
        .children
        .insert("child".into(), vec![Some(child_source.clone())]);
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert_eq!(
        chat_runtime(&app, card).message_copy_source(
            chat_runtime(&app, card)
                .active_item("same:opaque:id")
                .unwrap()
        ),
        Some(source)
    );
    drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
    assert_eq!(
        chat_runtime(&app, card).message_copy_source(
            chat_runtime(&app, card)
                .active_item("same:opaque:id")
                .unwrap()
        ),
        Some(child_source)
    );
    let task = app.handle_chat_ui(Action::CopyMessage(target, "same:opaque:id".into(), None));
    assert_chat_copy_task_has_no_effects(
        task,
        "missing source must never copy the loaded fragment",
    );
    assert!(
        chat_runtime(&app, card)
            .error
            .as_deref()
            .unwrap()
            .contains("unavailable")
    );
    assert!(chat_runtime(&app, card).copied.is_none());
}

/// A long output's row shows only its start and end. Its Copy reads the
/// whole output from its source, and the step's copy puts the command ahead
/// of it, while a row showing all of its output copies what it shows.
#[test]
fn an_excerpted_output_copies_whole_from_its_source() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    app.workspace.focus(card).unwrap();
    let whole = "line\n".repeat(200_000);
    let step = |id: &str, output: &str| {
        Arc::new(chat::Item {
            id: id.into(),
            role: chat::Role::Assistant,
            content: ItemContent::Command {
                label: String::new(),
                command: "cargo build".into(),
                working_directory: String::new(),
                action: chat::CommandAction::Run,
                status: chat::ToolStatus::Succeeded,
                exit_code: Some(0),
                duration_ms: None,
                output: output.into(),
            },
            at: None,
            started_at: None,
        })
    };
    let source = |token, bytes: usize| chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: chat::TextCopyScope::Conversation,
        token,
        revision: 1,
        bytes: bytes as u64,
    };
    let excerpt = &format!("line\n{}line\n", chat::excerpt_gap(1_000_000));
    let excerpted = source(1, whole.len());
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.items = vec![
        step("long", excerpt),
        step("short", "done\n"),
        step("huge", excerpt),
    ];
    snapshot.text_copy_sources.conversation = vec![
        Some(excerpted.clone()),
        Some(source(2, 5)),
        Some(source(3, chat::TEXT_COPY_LIMIT_BYTES + 1)),
    ];
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.excerpted_output_source(runtime.active_item("long").unwrap()),
        Some(excerpted.clone())
    );
    assert_eq!(
        runtime.excerpted_output_source(runtime.active_item("short").unwrap()),
        None,
        "a row showing all of its output copies what it shows"
    );
    assert_eq!(
        runtime.excerpted_output_source(runtime.active_item("huge").unwrap()),
        None,
        "an output past the clipboard limit copies the excerpt it shows, as its note says"
    );
    assert_eq!(
        step::excerpt_note(&CopyPart::Output, true),
        "Only the start and end of this output are shown. Copy includes all of it."
    );
    assert!(chat::is_excerpt(excerpt) && !chat::is_excerpt("done\n"));
    // The step's own Copy starts with its command, as a step shown whole
    // does; its output's Copy is the output alone.
    let long = runtime.active_item("long").unwrap().content.clone();
    assert_eq!(
        super::step_copy_prefix(&CopyPart::Text, &long).as_deref(),
        Some("cargo build")
    );
    assert_eq!(
        super::step_copy_prefix(&CopyPart::Output, &long).as_deref(),
        Some("")
    );
    let tool = ItemContent::Tool {
        name: "shell".into(),
        input: "{\"cmd\":\"ls\"}".into(),
        output: excerpt.into(),
        status: chat::ToolStatus::Succeeded,
    };
    assert_eq!(
        super::step_copy_prefix(&CopyPart::Text, &tool).as_deref(),
        Some("{\"cmd\":\"ls\"}")
    );
    assert_eq!(super::step_copy_prefix(&CopyPart::Input, &tool), None);
    let request = text_copy::Request::for_test(handle.clone(), excerpted.clone());
    let task = app.handle_chat_ui(Action::MessageCopied(
        target,
        "long".into(),
        request,
        Ok(whole.clone()),
    ));
    assert_chat_copy_task_writes(task, &whole);
    let request = text_copy::Request::for_test(handle, excerpted).with_prefix("cargo build");
    let task = app.handle_chat_ui(Action::MessageCopied(
        target,
        "long".into(),
        request,
        Ok(whole.clone()),
    ));
    assert_chat_copy_task_writes(task, &format!("cargo build\n\n{whole}"));
}

/// Establish every completion guard, and prove this exact authority can
/// emit a clipboard effect before a later copy supersedes its intent.
fn valid_message_copy_completion(
    app: &mut Cagents,
) -> (CardId, Target, text_copy::Request, String) {
    let (card, target, handle) = history_fixture(app);
    app.workspace.focus(card).unwrap();
    let body = "x".repeat(300 * 1024);
    let source = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: chat::TextCopyScope::Conversation,
        token: 7,
        revision: 9,
        bytes: body.len() as u64,
    };
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.items = vec![presentation_text("last-part", "only the last loaded part")];
    snapshot.text_copy_sources.conversation = vec![Some(source.clone())];
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let request = text_copy::Request::for_test(handle, source);
    let control = app.handle_chat_ui(Action::MessageCopied(
        target,
        "last-part".into(),
        request.clone(),
        Ok(body.clone()),
    ));
    assert_chat_copy_task_writes(control, &body);
    assert_eq!(app.workspace.focused(), Some(card));
    app.chat_runtime_mut(target).unwrap().copied = None;
    (card, target, request, body)
}

fn valid_child_failure_copy_completion(
    app: &mut Cagents,
) -> (Target, String, text_copy::FailureRequest, String) {
    let (card, target, handle) = history_fixture(app);
    app.workspace.focus(card).unwrap();
    let raw = format!(
        "You've hit your session limit · resets 3:40pm (America/Vancouver)\n{}\nTAIL original λ",
        "猫 exact diagnostics\n".repeat(20_000)
    );
    let mut preview = (*presentation_text(
        "child-failure",
        &format!(
            "You've hit your session limit · resets 3:40pm (America/Vancouver){}",
            chat::failure::PREVIEW_OMITTED
        ),
    ))
    .clone();
    assert!(chat::failure::mark_provider_error(
        Provider::Codex,
        &mut preview
    ));
    let source = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: chat::TextCopyScope::Subagent { id: "child".into() },
        token: 7,
        revision: 3,
        bytes: raw.len() as u64,
    };
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.subagents = vec![presentation_child("child", Arc::new(preview))];
    snapshot
        .text_copy_sources
        .children
        .insert("child".into(), vec![Some(source)]);
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.selected_subagent = Some("child".into());
    runtime.refresh();
    let key = runtime
        .failure_ui
        .incidents
        .iter()
        .find(|incident| incident.child_scope.as_deref() == Some("child"))
        .unwrap()
        .key
        .clone();
    let plan = runtime.failure_copy_plan(&key).unwrap().unwrap();
    let request =
        text_copy::FailureRequest::for_test(handle, runtime.selected_subagent.clone(), plan);
    (target, key, request, raw)
}

#[test]
fn child_failure_copy_completion_writes_exact_original_and_rechecks_deferred_authority() {
    let _context = text_copy::isolated_context();
    // This UI oracle begins at the completed worker boundary. The HOST
    // actor oracle separately exercises actual owned-source page reads.
    for change in ["none", "source", "remove", "new-copy", "selection"] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (target, key, request, raw) = valid_child_failure_copy_completion(&mut app);
        let handle = app
            .chat_runtime_mut(target)
            .unwrap()
            .handle()
            .unwrap()
            .clone();
        let task = app.handle_chat_ui(Action::FailureCopied(target, key, request, Ok(raw.clone())));
        match change {
            "source" | "remove" => {
                let mut snapshot = (*handle.snapshot()).clone();
                if change == "source" {
                    snapshot
                        .text_copy_sources
                        .children
                        .get_mut("child")
                        .unwrap()[0]
                        .as_mut()
                        .unwrap()
                        .revision += 1;
                } else {
                    snapshot.subagents.clear();
                    snapshot.text_copy_sources.children.clear();
                }
                snapshot.generation += 1;
                handle.publish_snapshot_for_test(snapshot);
            }
            "new-copy" => {
                drop(app.handle_chat_ui(Action::CopyLiteral("newer intent".into())));
            }
            "selection" => {
                drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
            }
            _ => {}
        }
        if change == "none" {
            assert_chat_copy_task_writes(task, &raw);
        } else {
            assert_chat_copy_task_has_no_clipboard_write(task, change);
        }
    }
}

#[test]
fn child_failure_copy_refuses_unavailable_original_instead_of_copying_preview() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (target, key, _, _) = valid_child_failure_copy_completion(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let handle = runtime.handle().unwrap().clone();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot
        .text_copy_sources
        .children
        .insert("child".into(), vec![None]);
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    assert_eq!(runtime.failure_originals_retained(&key), Some(false));
    let task = app.handle_chat_ui(Action::CopyFailure(target, key));
    assert_chat_copy_task_has_no_clipboard_write(
        task,
        "an unavailable original must never copy the resident excerpt",
    );
    assert!(
        app.chat_runtime_mut(target)
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("Original diagnostic unavailable")
    );
}

#[test]
fn child_failure_read_error_reports_withdrawal_but_suppresses_replacement_feedback() {
    let _context = text_copy::isolated_context();
    for change in ["withdrawn", "replacement"] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (target, key, request, _) = valid_child_failure_copy_completion(&mut app);
        let handle = app
            .chat_runtime_mut(target)
            .unwrap()
            .handle()
            .unwrap()
            .clone();
        let mut snapshot = (*handle.snapshot()).clone();
        let source = &mut snapshot
            .text_copy_sources
            .children
            .get_mut("child")
            .unwrap()[0];
        if change == "withdrawn" {
            *source = None;
        } else {
            source.as_mut().unwrap().cache = uuid::Uuid::new_v4();
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        let task = app.handle_chat_ui(Action::FailureCopied(
            target,
            key,
            request,
            Err("owned backing read failed".into()),
        ));
        assert_chat_copy_task_has_no_clipboard_write(task, "read errors cannot copy a preview");
        let error = app.chat_runtime_mut(target).unwrap().error.as_deref();
        assert_eq!(
            error.is_some_and(|error| error.contains("Original diagnostic unavailable")),
            change == "withdrawn"
        );
    }
}

#[test]
fn logical_message_copy_completion_survives_parent_window_eviction() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (_, target, request, body) = valid_message_copy_completion(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let handle = runtime.handle().unwrap().clone();
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.items = vec![presentation_text("another-page", "another resident body")];
    snapshot.text_copy_sources.conversation = vec![None];
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    // The worker has verified canonical pages; the loaded presentation no
    // longer contains the clicked part when its completion reaches App.
    let task = app.update(Message::ChatUi(Action::MessageCopied(
        target,
        "last-part".into(),
        request,
        Ok(body.clone()),
    )));
    assert_chat_copy_task_writes(task, &body);
    assert!(app.chat_runtime_mut(target).unwrap().copied.is_some());
}

#[test]
fn provider_queued_copy_survives_unrelated_streaming_but_refuses_rewrite_removal_and_new_intent() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let body = format!("{}meaningful queued task λ\r\n", " ".repeat(4096));
    let (card, target, handle) =
        provider_queue(&mut app, vec![provider_message("queued-source", &body)]);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.show(true);
    let source = runtime
        .provider_queue
        .borrow_mut()
        .get(&runtime.snapshot, "queued-source")
        .unwrap();
    assert_eq!(&*source.preview, "meaningful queued task λ");
    let copied = QueueCopy::Provider(Arc::downgrade(&source), "queued-source".into());
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.busy = !snapshot.busy;
    handle.publish_delta_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert_chat_copy_task_writes(
        app.handle_chat_ui(Action::CopyQueued(target, copied.clone())),
        &body,
    );
    let task = app.handle_chat_ui(Action::CopyQueued(target, copied.clone()));
    drop(app.handle_chat_ui(Action::CopyLiteral(
        "newer explicit clipboard intent".into(),
    )));
    assert_chat_copy_task_has_no_clipboard_write(
        task,
        "newer intent still supersedes unchanged provider queue Copy",
    );
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.queued_messages[0].text = "same id, changed queue body".into();
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert_chat_copy_task_has_no_effects(
        app.handle_chat_ui(Action::CopyQueued(target, copied.clone())),
        "same queue ID cannot authorize old rendered source after rewrite",
    );
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.queued_messages.clear();
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    assert_chat_copy_task_has_no_effects(
        app.handle_chat_ui(Action::CopyQueued(target, copied)),
        "removed provider queue source cannot copy",
    );
    drop(source);
    drop(app);
    preparation::retirement_barrier();
    let _ = card;
}

#[test]
fn queued_literal_actual_reply_copy_and_takeback_preserve_source_and_reject_late_ownership() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let body = format!(
        "  **literal queued**\r\n{}END_QUEUED_λ\r\n",
        "raw `code` [no link](invalid:source) ".repeat(16_384)
    );
    write(&mut app, card, &body);
    submit(&mut app, card); // startup task is dropped; no provider runs
    let target = chat_runtime_attempt(&app, card);
    let handle = fixture_handle(&app, card, None);
    handle.replace_snapshot_for_test(Snapshot::default());
    answer_start(&mut app, target, Ok(handle));
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.show(true);
    let source = runtime.queued.as_ref().unwrap().prompt.source.clone();
    assert_eq!(
        source.text(),
        body,
        "submission moves the exact canonical local bytes"
    );
    assert!(
        runtime
            .queued
            .as_ref()
            .unwrap()
            .prompt
            .presentation
            .prepared
            .is_none()
    );
    let materialized = markdown::materialized_bytes();
    let cold = runtime.selection_document().0;
    assert!(
        cold.text(selection::logical_identity("queued-prompt", &[2]))
            .is_none(),
        "pending local preview is not full catalog authority"
    );
    assert_eq!(markdown::materialized_bytes(), materialized);
    // Native cold layout may display only a bounded preview; it cannot
    // clone/style/materialize the queued full source to build a widget.
    {
        use iced::advanced::{layout, widget::Tree};
        let mut element = app.chat_view(card);
        let mut tree = Tree::new(element.as_widget());
        let _ = element.as_widget_mut().layout(
            &mut tree,
            &crate::app::tests::headless_renderer(),
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(960.0, 720.0)),
        );
    }
    assert_eq!(
        markdown::materialized_bytes(),
        materialized,
        "cold native queue layout cannot materialize the large literal body"
    );
    let (run, ready) = preparation::local_reply_for_test(app.chat_runtime_mut(target).unwrap());
    drop(app.update(Message::ChatUi(Action::LocalPromptPrepared(
        target, run, ready,
    ))));
    let runtime = app.chat_runtime_mut(target).unwrap();
    let prepared = runtime
        .queued
        .as_ref()
        .unwrap()
        .prompt
        .presentation
        .prepared
        .as_ref()
        .expect("actual local action accepts exact ready source");
    assert_eq!(&*prepared.full.plain(), body.trim_end());
    assert_ne!(prepared.created_on, std::thread::current().id());
    assert_eq!(
        &*runtime
            .selection_document()
            .0
            .text(selection::logical_identity("queued-prompt", &[2]))
            .unwrap(),
        body.trim_end()
    );
    let copied = QueueCopy::Initial(Arc::downgrade(&source));
    let task = app.handle_chat_ui(Action::CopyQueued(target, copied.clone()));
    assert_chat_copy_task_writes(task, &body);
    let task = app.handle_chat_ui(Action::CopyQueued(target, copied.clone()));
    drop(app.handle_chat_ui(Action::CopyLiteral("newer explicit selection".into())));
    assert_chat_copy_task_has_no_clipboard_write(
        task,
        "newer clipboard intent supersedes queued full-source copy",
    );
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.queued.as_mut().unwrap().prompt.presentation = prompt::LocalPresentation::default();
    let (late_run, late_ready) = preparation::local_reply_for_test(runtime);
    drop(app.update(Message::ChatUi(Action::CancelQueued(target))));
    assert!(chat_runtime(&app, card).queued.is_none());
    assert_eq!(
        chat_runtime(&app, card).composer.text(),
        body,
        "takeback restores all raw bytes and endings"
    );
    drop(app.update(Message::ChatUi(Action::LocalPromptPrepared(
        target, late_run, late_ready,
    ))));
    assert!(
        chat_runtime(&app, card).queued.is_none(),
        "late local ready cannot resurrect canceled input"
    );
    assert_chat_copy_task_has_no_effects(
        app.handle_chat_ui(Action::CopyQueued(target, copied)),
        "taken-back source is no longer queued copy authority",
    );
    preparation::retirement_barrier();
}

#[test]
fn shared_code_copy_keeps_exact_bytes_typed_target_and_clipboard_intent_until_effect() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let raw: Arc<str> = "  literal *code* λ é\r\ntrailing\n".repeat(3000).into();
    let task = app.handle_chat_ui(Action::CopySharedCode(target, raw.clone()));
    assert_chat_copy_task_writes(task, &raw);
    let stale = Target {
        generation: target.generation.wrapping_add(1),
        ..target
    };
    assert_chat_copy_task_has_no_effects(
        app.handle_chat_ui(Action::CopySharedCode(stale, raw.clone())),
        "a stale retained block target cannot copy into another UI generation",
    );
    let task = app.handle_chat_ui(Action::CopySharedCode(target, raw));
    drop(app.handle_chat_ui(Action::CopyLiteral("newer selection".into())));
    assert_chat_copy_task_has_no_clipboard_write(
        task,
        "newer clipboard intent wins before the deferred shared-source write",
    );
}

#[test]
fn logical_message_copy_completion_refuses_destructive_changes_after_last_page() {
    let _context = text_copy::isolated_context();
    for change in ["rewrite", "remove", "reset"] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (_, target, request, body) = valid_message_copy_completion(&mut app);
        let runtime = app.chat_runtime_mut(target).unwrap();
        let handle = runtime.handle().unwrap().clone();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.text_copy_sources.conversation_revision += 1;
        if change == "rewrite" {
            snapshot.items = vec![presentation_text("last-part", "replacement")];
            snapshot.text_copy_sources.conversation[0]
                .as_mut()
                .unwrap()
                .revision += 1;
        } else {
            snapshot.items.clear();
            snapshot.text_copy_sources.conversation.clear();
        }
        if change == "reset" {
            snapshot.session_id = None;
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        // Completion must consult the latest client publication even when
        // the UI's ordinary refresh has not consumed it yet.
        let task = app.update(Message::ChatUi(Action::MessageCopied(
            target,
            "last-part".into(),
            request,
            Ok(body),
        )));
        assert_chat_copy_task_has_no_effects(task, change);
        assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
    }
}

#[test]
fn logical_message_copy_deferred_write_refuses_destructive_publication_before_poll() {
    let _context = text_copy::isolated_context();
    for change in ["rewrite", "remove", "reset"] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (_, target, request, body) = valid_message_copy_completion(&mut app);
        // Finish while the source is valid, but retain its unpolled task.
        let task = app.update(Message::ChatUi(Action::MessageCopied(
            target,
            "last-part".into(),
            request,
            Ok(body),
        )));
        let handle = app
            .chat_runtime_mut(target)
            .unwrap()
            .handle()
            .unwrap()
            .clone();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.text_copy_sources.conversation_revision += 1;
        if change == "rewrite" {
            snapshot.text_copy_sources.conversation[0]
                .as_mut()
                .unwrap()
                .revision += 1;
        } else {
            snapshot.items.clear();
            snapshot.text_copy_sources.conversation.clear();
        }
        if change == "reset" {
            snapshot.session_id = None;
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        // The ordinary UI refresh has not consumed this publication.
        assert_chat_copy_task_has_no_clipboard_write(task, change);
    }
}

#[test]
fn logical_message_copy_deferred_write_preserves_paging_and_append_authority() {
    let _context = text_copy::isolated_context();
    for change in ["paging", "append"] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (_, target, request, body) = valid_message_copy_completion(&mut app);
        let task = app.update(Message::ChatUi(Action::MessageCopied(
            target,
            "last-part".into(),
            request,
            Ok(body.clone()),
        )));
        let handle = app
            .chat_runtime_mut(target)
            .unwrap()
            .handle()
            .unwrap()
            .clone();
        let mut snapshot = (*handle.snapshot()).clone();
        if change == "paging" {
            snapshot.items = vec![presentation_text("another-page", "another body")];
            snapshot.text_copy_sources.conversation = vec![None];
        } else {
            snapshot.text_copy_sources.conversation[0]
                .as_mut()
                .unwrap()
                .bytes += 10;
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        assert_chat_copy_task_writes(task, &body);
    }
}

#[test]
fn logical_message_copy_deferred_child_write_rechecks_resident_authority() {
    let _context = text_copy::isolated_context();
    for change in ["rewrite", "trim", "append"] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        app.workspace.focus(card).unwrap();
        let body = "complete child 🦀";
        let source = chat::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: chat::TextCopyScope::Subagent { id: "child".into() },
            token: 1,
            revision: 1,
            bytes: body.len() as u64,
        };
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.subagents = vec![presentation_child(
            "child",
            presentation_text("child-message", body),
        )];
        snapshot
            .text_copy_sources
            .children
            .insert("child".into(), vec![Some(source.clone())]);
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.selected_subagent = Some("child".into());
        runtime.refresh();
        let request = text_copy::Request::for_test(handle.clone(), source);
        let task = app.update(Message::ChatUi(Action::MessageCopied(
            target,
            "child-message".into(),
            request,
            Ok(body.into()),
        )));
        let mut snapshot = (*handle.snapshot()).clone();
        let sources = snapshot
            .text_copy_sources
            .children
            .get_mut("child")
            .unwrap();
        match change {
            "rewrite" => sources[0].as_mut().unwrap().cache = uuid::Uuid::new_v4(),
            "trim" => sources.clear(),
            _ => sources[0].as_mut().unwrap().bytes += 10,
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        if change == "append" {
            assert_chat_copy_task_writes(task, body);
        } else {
            assert_chat_copy_task_has_no_clipboard_write(task, change);
        }
    }
}

#[test]
fn logical_message_copy_completion_refuses_scope_handle_focus_and_failed_canonical_read() {
    let _context = text_copy::isolated_context();
    for change in ["scope", "handle", "focus", "canonical read"] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (_, target, request, body) = valid_message_copy_completion(&mut app);
        match change {
            "scope" => {
                app.chat_runtime_mut(target).unwrap().selected_subagent = Some("other-child".into())
            }
            "handle" => app.chat_runtime_mut(target).unwrap().connection = Connection::Connecting,
            "focus" => {
                let other = live_chat(&mut app, Some(KNOWN_THREAD));
                app.workspace.focus(other).unwrap();
            }
            _ => {}
        }
        let result = if change == "canonical read" {
            Err("Message changed; copy its current text again".into())
        } else {
            Ok(body)
        };
        let task = app.update(Message::ChatUi(Action::MessageCopied(
            target,
            "last-part".into(),
            request,
            result,
        )));
        assert_chat_copy_task_has_no_effects(task, change);
        assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
        if change == "canonical read" {
            assert!(
                app.chat_runtime_mut(target)
                    .unwrap()
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("Message changed")
            );
        }
    }
}

#[test]
fn newer_chat_copy_blocks_delayed_full_message_clipboard_write() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, request, body) = valid_message_copy_completion(&mut app);
    drop(app.handle_chat_ui(Action::CopyLiteral("newer code selection".into())));
    assert_eq!(app.workspace.focused(), Some(card));
    let task = app.handle_chat_ui(Action::MessageCopied(
        target,
        "last-part".into(),
        request,
        Ok(body),
    ));
    assert_chat_copy_task_has_no_effects(
        task,
        "a delayed read must not overwrite the newer clipboard intent",
    );
    assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
}

#[test]
fn captured_builtin_copy_event_on_same_card_supersedes_async_message_copy() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, request, body) = valid_message_copy_completion(&mut app);
    drop(app.update(Message::KeyEvent {
        captured: true,
        event: keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("c".into()),
            modified_key: keyboard::Key::Character("c".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyC),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::COMMAND,
            text: None,
            repeat: false,
        },
    }));
    assert_eq!(app.workspace.focused(), Some(card));
    let task = app.handle_chat_ui(Action::MessageCopied(
        target,
        "last-part".into(),
        request,
        Ok(body),
    ));
    assert_chat_copy_task_has_no_effects(
        task,
        "a builtin copy captured by the same card must supersede the old read",
    );
    assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
}

#[test]
fn logical_copy_footer_uses_item_identity_when_display_ids_collide() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let first = presentation_text("opaque:native-part:1", "first logical message part");
    let second = presentation_text("opaque:native-part:1", "different opaque provider message");
    let source = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: chat::TextCopyScope::Conversation,
        token: 7,
        revision: 9,
        bytes: 300 * 1024,
    };
    let mut second_source = source.clone();
    second_source.token += 1;
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.items = vec![first.clone(), second.clone()];
    snapshot.text_copy_sources.conversation =
        vec![Some(source.clone()), Some(second_source.clone())];
    snapshot.generation += 1;
    handle.publish_snapshot_for_test(snapshot);
    app.chat_runtime_mut(target).unwrap().refresh();
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.message_copy_source(first.as_ref()), Some(source));
    assert_eq!(
        runtime.message_copy_source(second.as_ref()),
        Some(second_source)
    );
    assert!(
        runtime
            .message_copy_source(
                presentation_text("opaque:native-part:1", "detached same id").as_ref()
            )
            .is_none()
    );
}
#[test]
fn a_long_answer_is_presented_and_copied_as_one_logical_message() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let part = chat::TRANSCRIPT_PART_BYTES;
    let mut message = "Intro paragraph.\n\n```rust\n".to_owned();
    message.push_str(&"let crab = \"🦀\";\n".repeat(part / 12));
    message.push_str("```\n\nAfter the fence.\n");
    let source = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: chat::TextCopyScope::Conversation,
        token: 7,
        revision: 1,
        bytes: message.len() as u64,
    };
    let publish = |window: std::ops::Range<usize>| {
        let mut snapshot = (*handle.snapshot()).clone();
        let items = logical_prose::parts("answer", &message, window);
        snapshot.text_copy_sources.conversation = vec![Some(source.clone()); items.len()];
        snapshot.items = items;
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
    };
    // The window holds every part: they are joined with no read.
    publish(0..usize::MAX);
    app.chat_runtime_mut(target).unwrap().refresh();
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.snapshot.items.len(), 1);
    let joined = runtime.snapshot.items[0].clone();
    assert_eq!(joined.id, "answer");
    assert!(matches!(&joined.content, ItemContent::Text { text, .. } if *text == message));
    assert_eq!(runtime.message_copy_source(&joined), Some(source.clone()));
    assert!(runtime.logical_prose.request().is_none());
    // The window holds only the tail: the whole message is read, then shown.
    publish(1..usize::MAX);
    app.chat_runtime_mut(target).unwrap().refresh();
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime
            .logical_prose
            .request()
            .map(|(read, _)| read.clone()),
        Some(source.clone())
    );
    drop(app.handle_chat_ui(Action::LogicalProseRead(
        target,
        source.clone(),
        Ok(message.clone()),
    )));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.snapshot.items.len(), 1);
    assert!(matches!(
        &runtime.snapshot.items[0].content,
        ItemContent::Text { text, .. } if *text == message
    ));
    assert!(runtime.logical_prose.request().is_none());
    // Hiding the pane releases the complete text with the presentation.
    app.chat_runtime_mut(target).unwrap().show(false);
    app.chat_runtime_mut(target).unwrap().cool();
    app.chat_runtime_mut(target).unwrap().refresh();
    assert!(chat_runtime(&app, card).logical_prose.raw().is_none());
}
#[test]
fn a_long_answer_keeps_its_prepared_text_while_parts_are_appended() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (_card, target, handle) = history_fixture(&mut app);
    let part = chat::TRANSCRIPT_PART_BYTES;
    let message = "- list item that streams in\n".repeat(4 * part / 28);
    let source = |bytes: usize, revision: u64| chat::TextCopySource {
        cache: uuid::Uuid::from_u128(766),
        scope: chat::TextCopyScope::Conversation,
        token: 7,
        revision,
        bytes: bytes as u64,
    };
    let publish = |end: usize, revision: u64| {
        let body = &message[..end];
        let mut snapshot = (*handle.snapshot()).clone();
        let items = logical_prose::parts("answer", body, 0..usize::MAX);
        snapshot.text_copy_sources.conversation =
            vec![Some(source(body.len(), revision)); items.len()];
        snapshot.items = items;
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
    };
    publish(part / 2, 1);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    preparation::drain_fixture(runtime);
    assert!(!runtime.cache["answer"].placeholder);
    // Each append crosses into another part, so every publication joins
    // into a new item; the prepared text stays shown until the next is ready.
    for end in [part + part / 2, 2 * part + part / 2, message.len()] {
        publish(end, 1);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.refresh();
        assert!(
            !runtime.cache["answer"].placeholder,
            "an append to {end} bytes blanked the answer"
        );
        preparation::drain_fixture(runtime);
        assert!(!runtime.cache["answer"].placeholder);
    }
    // A rewrite is not an append and waits for its own preparation.
    publish(message.len() - 1, 2);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    assert!(runtime.cache["answer"].placeholder);
}
#[test]
fn a_long_answer_streaming_past_its_window_never_blanks() {
    let _context = text_copy::isolated_context();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, handle) = history_fixture(&mut app);
    let part = chat::TRANSCRIPT_PART_BYTES;
    let message = "- list item that streams in\n".repeat(12 * part / 28);
    let source = |bytes: usize| chat::TextCopySource {
        cache: uuid::Uuid::from_u128(767),
        scope: chat::TextCopyScope::Conversation,
        token: 7,
        revision: 1,
        bytes: bytes as u64,
    };
    // The store's window holds the last three parts of what has arrived.
    let publish = |end: usize| {
        let body = &message[..end];
        let count = body.len().div_ceil(part);
        let mut snapshot = (*handle.snapshot()).clone();
        let items = logical_prose::parts("answer", body, count.saturating_sub(3)..count);
        snapshot.text_copy_sources.conversation = vec![Some(source(end)); items.len()];
        snapshot.items = items;
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
    };
    let deliver = |app: &mut Cagents| {
        let read = chat_runtime(app, card)
            .logical_prose
            .request()
            .map(|(read, _)| read.clone());
        if let Some(read) = read {
            let text = message[..read.bytes as usize].to_owned();
            drop(app.handle_chat_ui(Action::LogicalProseRead(target, read, Ok(text))));
        }
    };
    let shown = |app: &Cagents| {
        let runtime = chat_runtime(app, card);
        assert_eq!(runtime.snapshot.items.len(), 1);
        match &runtime.snapshot.items[0].content {
            ItemContent::Text { text, .. } => text.clone(),
            _ => unreachable!(),
        }
    };
    publish(4 * part);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.refresh();
    preparation::drain_fixture(runtime);
    // Steps of half a part. Reads arrive promptly at first, then stall for
    // long enough that the window moves past the last one.
    let mut end = 4 * part;
    let mut step = 0;
    while end < message.len() {
        end = (end + part / 2).min(message.len());
        step += 1;
        publish(end);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.refresh();
        assert!(
            !runtime.cache["answer"].placeholder,
            "growth to {end} bytes blanked the answer"
        );
        if !(8..14).contains(&step) {
            deliver(&mut app);
            assert!(
                !chat_runtime(&app, card).cache["answer"].placeholder,
                "the complete read at {end} bytes blanked the answer"
            );
        }
        preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
        assert!(!chat_runtime(&app, card).cache["answer"].placeholder);
        if step < 8 {
            // Reads made before the window outran them show the whole answer.
            assert_eq!(shown(&app), message[..end]);
        }
    }
    deliver(&mut app);
    preparation::drain_fixture(app.chat_runtime_mut(target).unwrap());
    assert_eq!(shown(&app), message);
}
struct LiveSharedRecovery {
    app: Cagents,
    _directory: tempfile::TempDir,
    card: CardId,
    target: Target,
    source: chat::Session,
    canonical: chat::Session,
    model: crate::workspace_model::WorkspaceModel,
    allocation: usize,
    draft: String,
    cursor: text_editor::Cursor,
    selection: Option<String>,
}

fn live_shared_recovery_fixture() -> LiveSharedRecovery {
    live_shared_recovery_fixture_for_identity(Some(KNOWN_THREAD))
}

fn live_shared_recovery_fixture_for_identity(session_id: Option<&str>) -> LiveSharedRecovery {
    use crate::model::{ChannelId, ProjectChannel, ProjectId, ProjectKind, ProjectState};
    use crate::workspace_model::{
        ModelChannel, ModelProject, WorkspaceModel,
        views::{Pane, Scope, View},
    };
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let mut source = chat_runtime(&app, card).session.clone();
    source.launch.provider = Provider::Claude;
    source.launch.session_id = session_id.map(str::to_owned);
    source.launch.recipe.program = "claude".into();
    source.launch.recipe.arguments.clear();
    source.launch.recipe.environment.clear();
    source.launch.recipe.environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        "/private/retained-recovery-account".into(),
    );
    source.owned_codex_listener = None;
    source.terminal = None;
    source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
    assert!(source.has_valid_native_continuity());
    app.install_chat_session(card, source.clone());
    let before = Arc::new(chat::Item {
        id: "native-before-recovery".into(),
        role: chat::Role::Assistant,
        content: text("BEFORE_RECOVERY"),
        started_at: None,
        at: None,
    });
    {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session = source.clone();
        runtime.connection = Connection::Live(Arc::new(ClientHandle::fixture(
            source.clone(),
            Snapshot {
                session_id: source.launch.session_id.clone(),
                ready: true,
                items: vec![before],
                ..Snapshot::default()
            },
        )));
        runtime.show(true);
    }
    let channel = ChannelId::new();
    let project = ProjectId::new();
    let identity = uuid::Uuid::new_v4();
    let repository = PathBuf::from(FIXTURE_DIRECTORY);
    app.sidebar_model.projects.push(
        crate::sidebar::SidebarProject::new(
            project,
            crate::repository::RepositoryId::new(),
            "Recovery fixture".into(),
            repository.clone(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    {
        let durable = Arc::make_mut(&mut app.durable);
        durable.workspace_model_identity = Some(identity);
        durable.active_channel = Some(channel);
        durable.channels.push(ProjectChannel {
            id: channel,
            project_id: project,
            title: "Recovery fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
    }
    app.shared_views = super::super::shared_views::State::for_workspace(&app.durable);
    let mut view = View::empty(Scope::Channel(channel));
    view.panes
        .push(Pane::from_card(app.card_state(card).unwrap(), Some(repository.clone())).unwrap());
    let mut model = WorkspaceModel {
        identity,
        sequence: 1,
        projects: vec![ModelProject {
            repository: repository.clone(),
            name: "Recovery fixture".into(),
            kind: ProjectKind::Folder,
            ordinal: 1,
        }],
        channels: vec![ModelChannel {
            id: channel,
            repository,
            title: "Recovery fixture".into(),
            title_source: Default::default(),
            ordinal: 1,
        }],
        views: vec![view],
        ..WorkspaceModel::default()
    };
    drop(app.update(Message::WorkspaceModelObserved(
        super::super::workspace_model_sync::ModelUpdate::ViewApplied {
            scope: Scope::Channel(channel),
            operation: None,
            model: model.clone(),
        },
    )));
    assert!(!app.chat_start_pending(card));
    assert_eq!(chat_runtime(&app, card).session, source);
    // The assertions inspect the presented body. Hidden panes deliberately
    // defer snapshot presentation even after their connection is admitted.
    app.workspace.resize_viewport(1_600.0);
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    let draft = "Keep this unsent thought\n  with λ and spacing".to_owned();
    write(&mut app, card, &draft);
    let (allocation, cursor, selection) = {
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.composer.perform(text_editor::Action::Move(
            text_editor::Motion::DocumentStart,
        ));
        runtime
            .composer
            .perform(text_editor::Action::Select(text_editor::Motion::Right));
        (
            runtime as *const Runtime as usize,
            runtime.composer.cursor(),
            runtime.composer.selection(),
        )
    };
    assert_eq!(selection.as_deref(), Some("K"));
    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = crate::session::SessionRuntime::open(
        directory.path().join("state.sqlite3"),
        crate::model::BootId::new("boot-test"),
    )
    .unwrap();
    writer.save_project(ProjectState {
        id: project,
        generation: 0,
        name: "Recovery fixture".into(),
        repository: PathBuf::from(FIXTURE_DIRECTORY),
        kind: ProjectKind::Folder,
        existence: Existence::Open,
    });
    app.session = Some(writer);
    app.persist();
    app.session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(10))
        .unwrap();
    drop(app.update(Message::ChatUi(Action::Wake(
        target,
        Wake::Disconnected("fixture native HOST lost".into()),
    ))));
    assert!(chat_runtime(&app, card).disconnected);
    let mut canonical = source.clone();
    canonical.proof.as_mut().unwrap().host_incarnation = "recovered-daemon".into();
    assert!(chat::is_upgrade_recovery(&source, &canonical));
    model.sequence += 1;
    model.views[0].revision += 1;
    model.views[0].panes[0].kind = CardKind::Chat(canonical.clone());
    LiveSharedRecovery {
        app,
        _directory: directory,
        card,
        target,
        source,
        canonical,
        model,
        allocation,
        draft,
        cursor,
        selection,
    }
}

fn assert_live_recovery_content(fixture: &LiveSharedRecovery) {
    let runtime = chat_runtime(&fixture.app, fixture.card);
    assert_eq!(
        runtime as *const Runtime as usize, fixture.allocation,
        "recovery must retain the original live Runtime/Content owner"
    );
    assert_eq!(runtime.composer.text(), fixture.draft);
    assert_eq!(runtime.composer.cursor(), fixture.cursor);
    assert_eq!(runtime.composer.selection(), fixture.selection);
    assert!(
        runtime.queued.is_none(),
        "an unsent draft is never a provider replay"
    );
    assert!(runtime.followups.is_empty());
}

fn exercise_live_shared_recovery(reconnect_first: bool) {
    exercise_live_shared_recovery_fixture(live_shared_recovery_fixture(), reconnect_first);
}

fn exercise_live_shared_recovery_fixture(mut f: LiveSharedRecovery, reconnect_first: bool) {
    let old_attempt = reconnect_first.then(|| {
        drop(f.app.update(Message::ChatUi(Action::Reconnect(f.target))));
        f.app.chat_ui.attempt(f.card).unwrap().target
    });
    // The actual watcher message projects a same-ID authoritative descriptor,
    // independently of the old live connection's unsent Content.
    drop(f.app.update(Message::WorkspaceModelObserved(
        super::super::workspace_model_sync::ModelUpdate::Read(f.model.clone()),
    )));
    assert_eq!(
        f.app.card_state(f.card).unwrap().kind,
        CardKind::Chat(f.canonical.clone())
    );
    assert_live_recovery_content(&f);
    assert_eq!(
        chat_runtime(&f.app, f.card).snapshot.items[0].content,
        text("BEFORE_RECOVERY")
    );
    let saved = f.app.persistent_workspace();
    assert_eq!(
        saved.cards.iter().find(|c| c.id == f.card).unwrap().kind,
        CardKind::Chat(f.source.clone()),
        "ModelUpdate cannot autosave canonical ownership before authenticated lineage and durable ACK"
    );
    assert_eq!(saved.chat_drafts.get(&f.card), Some(&f.draft));
    f.app.persist();
    f.app
        .session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        f.app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_card(f.card, None)
            .unwrap()
            .unwrap()
            .kind,
        CardKind::Chat(f.source.clone())
    );
    let admitted = f
        .app
        .chat_ui
        .attempt(f.card)
        .expect("canonical projection must enter strict upgrade admission")
        .target;
    assert_eq!(
        f.app
            .chat_ui
            .attempt(f.card)
            .unwrap()
            .upgrade_admission
            .as_ref(),
        Some(&f.canonical)
    );
    if let Some(old) = old_attempt {
        assert_ne!(
            old, admitted,
            "canonical admission supersedes the ordinary source attachment"
        );
    }
    // Retry from the still-rendered disconnected runtime cannot replace the
    // already admitted strict attachment with an ordinary restore.
    drop(f.app.update(Message::ChatUi(Action::Reconnect(f.target))));
    assert_eq!(f.app.chat_ui.attempt(f.card).unwrap().target, admitted);
    let snapshot = Snapshot {
        session_id: f.canonical.launch.session_id.clone(),
        ready: true,
        items: vec![Arc::new(chat::Item {
            id: "native-after-recovery".into(),
            role: chat::Role::Assistant,
            content: text("BEFORE_RECOVERY\nAFTER_RECOVERY_EXACT_ONCE"),
            started_at: None,
            at: None,
        })],
        ..Snapshot::default()
    };
    if let Some(old) = old_attempt {
        let stale = Arc::new(
            ClientHandle::fixture_upgrade_handshake(
                f.source.clone(),
                f.canonical.clone(),
                snapshot.clone(),
            )
            .unwrap(),
        );
        answer_start(&mut f.app, old, Ok(stale));
        assert_eq!(f.app.chat_ui.attempt(f.card).unwrap().target, admitted);
        assert!(!f.app.chat_ui.attempt(f.card).unwrap().upgrade_committed);
    }
    let handle = Arc::new(
        ClientHandle::fixture_upgrade_handshake(
            f.source.clone(),
            f.canonical.clone(),
            snapshot.clone(),
        )
        .unwrap(),
    );
    let witness = handle.authenticated_upgrade_recovery().unwrap();
    assert!(witness.validate());
    assert_eq!(witness.source(), &f.source);
    assert_eq!(witness.replacement(), &f.canonical);
    assert_eq!(
        f.app.workspace.visibility(f.card),
        Some(VisibilityClass::Visible)
    );
    let checkpoint = f.app.update(Message::ChatUi(Action::Ready(
        admitted,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    )));
    assert_live_recovery_content(&f);
    assert_eq!(
        chat_runtime(&f.app, f.card).session,
        f.source,
        "a connected handle is not the durable writer ACK"
    );
    let receipt = account_commit_reply(checkpoint);
    assert!(matches!(
        &receipt,
        Message::ChatUi(Action::UpgradeCommitted(_, _, Ok(())))
    ));
    drop(f.app.update(receipt));
    assert!(!f.app.chat_start_pending(f.card));
    assert_live_recovery_content(&f);
    let runtime = chat_runtime(&f.app, f.card);
    assert_eq!(runtime.session, f.canonical);
    assert!(!runtime.disconnected);
    assert!(runtime.connected());
    assert_eq!(runtime.snapshot.items, snapshot.items);
    assert!(
        f.app
            .persistent_workspace()
            .pending_chat_admissions
            .is_empty()
    );
    f.app.persist();
    let writer = f.app.session.as_ref().unwrap();
    writer.flush(Duration::from_secs(10)).unwrap();
    assert_eq!(writer.last_error(), None);
    assert_eq!(
        writer
            .operation_store()
            .load_card(f.card, None)
            .unwrap()
            .unwrap()
            .kind,
        CardKind::Chat(f.canonical)
    );
}

#[test]
fn retained_live_model_upgrade_reconnect_holds_source_through_real_writer_ack() {
    exercise_live_shared_recovery(false);
}

#[test]
fn retained_live_model_upgrade_supersedes_inflight_ordinary_reconnect_without_replaying_draft() {
    exercise_live_shared_recovery(true);
}

fn assert_no_live_upgrade_checkpoint(task: Task<Message>) {
    use cosmic::iced::futures::StreamExt;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) else {
                return;
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) =
                        action
                    {
                        assert!(
                            !matches!(
                                message,
                                Message::ChatUi(
                                    Action::UpgradeCommitted(..)
                                        | Action::AccountCommitted(..)
                                        | Action::Ready(..)
                                        | Action::Retired(..)
                                )
                            ),
                            "refused or stale Ready cannot checkpoint, activate or retire an actor"
                        );
                    }
                }
            })
            .await
            .expect("ancillary UI tasks must settle without an upgrade checkpoint");
        });
}

fn live_shared_recovery_handle(f: &LiveSharedRecovery) -> Arc<ClientHandle> {
    Arc::new(
        ClientHandle::fixture_upgrade_handshake(
            f.source.clone(),
            f.canonical.clone(),
            Snapshot {
                session_id: f.canonical.launch.session_id.clone(),
                ready: true,
                ..Snapshot::default()
            },
        )
        .unwrap(),
    )
}

fn project_live_shared_recovery(f: &mut LiveSharedRecovery) -> Target {
    drop(f.app.update(Message::WorkspaceModelObserved(
        super::super::workspace_model_sync::ModelUpdate::Read(f.model.clone()),
    )));
    let attempt = f
        .app
        .chat_ui
        .attempt(f.card)
        .expect("strict canonical admission");
    assert_eq!(attempt.upgrade_admission.as_ref(), Some(&f.canonical));
    attempt.target
}

fn assert_inert_live_recovery_copy(f: &LiveSharedRecovery) {
    let (copy, runtime) = f
        .app
        .cards
        .iter()
        .find_map(|(card, value)| match value {
            CardRuntime::Chat(runtime)
                if (&**runtime as *const Runtime as usize) == f.allocation =>
            {
                Some((*card, &**runtime))
            }
            _ => None,
        })
        .expect("the original Content owner must remain reachable");
    assert_ne!(copy, f.card);
    assert_eq!(runtime.session, f.source);
    assert!(runtime.awaiting_admission());
    assert_eq!(runtime.composer.text(), f.draft);
    assert_eq!(runtime.composer.cursor(), f.cursor);
    assert_eq!(runtime.composer.selection(), f.selection);
    assert!(runtime.queued.is_none() && runtime.followups.is_empty());
    assert_eq!(
        f.app.card_state(copy).unwrap().kind,
        CardKind::Chat(f.source.clone())
    );
    assert!(f.app.durable.local_only_cards.contains(&copy));
}

#[test]
fn retained_live_model_upgrade_first_identity_and_bound_reset_require_the_same_exact_checkpoint() {
    for first_identity in [true, false] {
        let mut f =
            live_shared_recovery_fixture_for_identity((!first_identity).then_some(KNOWN_THREAD));
        f.canonical.launch.session_id = Some("00000000-0000-4000-8000-000000000002".into());
        f.model.views[0].panes[0].kind = CardKind::Chat(f.canonical.clone());
        assert!(chat::client::UpgradeRecovery::is_candidate(
            &f.source,
            &f.canonical
        ));
        assert!(!chat::is_upgrade_recovery(&f.source, &f.canonical));
        if !first_identity {
            assert!(!chat::is_upgrade_identity_learning(&f.source, &f.canonical));
        }
        assert!(
            f.app
                .session
                .as_ref()
                .unwrap()
                .operation_store()
                .rebind_chat_upgrade(f.card, &f.source, &f.canonical)
                .is_err(),
            "raw descriptors never authorize a retained transition"
        );
        exercise_live_shared_recovery_fixture(f, false);
    }
}

#[test]
fn retained_live_model_upgrade_cannot_activate_unproved_refused_or_superseded_handles() {
    for case in [
        "unproved",
        "writer refusal",
        "newer canonical before Ready",
        "newer canonical before writer ACK",
    ] {
        let mut f = live_shared_recovery_fixture();
        let target = project_live_shared_recovery(&mut f);
        let fault = rusqlite::Connection::open(f._directory.path().join("state.sqlite3")).unwrap();
        if case == "writer refusal" {
            fault.execute_batch("CREATE TRIGGER refuse_recovery BEFORE UPDATE OF content_json ON cards WHEN NEW.content_json != OLD.content_json BEGIN SELECT RAISE(ABORT, 'fixture refused recovery'); END;").unwrap();
        }
        let handle = if case == "unproved" {
            Arc::new(ClientHandle::fixture(
                f.canonical.clone(),
                Snapshot {
                    session_id: f.canonical.launch.session_id.clone(),
                    ready: true,
                    ..Snapshot::default()
                },
            ))
        } else {
            live_shared_recovery_handle(&f)
        };
        let mut newer = f.model.clone();
        newer.sequence += 1;
        newer.views[0].revision += 1;
        let CardKind::Chat(next) = &mut newer.views[0].panes[0].kind else {
            unreachable!()
        };
        next.proof.as_mut().unwrap().host_incarnation = "still-newer-daemon".into();
        if case == "newer canonical before Ready" {
            drop(f.app.update(Message::WorkspaceModelObserved(
                super::super::workspace_model_sync::ModelUpdate::Read(newer.clone()),
            )));
        }

        let checkpoint = f.app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        )));
        if case == "unproved" {
            assert_no_live_upgrade_checkpoint(checkpoint);
            assert_live_recovery_content(&f);
            assert_eq!(chat_runtime(&f.app, f.card).session, f.source);
            assert!(!chat_runtime(&f.app, f.card).connected());
        } else if case == "newer canonical before Ready" {
            assert_no_live_upgrade_checkpoint(checkpoint);
            assert_inert_live_recovery_copy(&f);
        } else {
            let receipt = account_commit_reply(checkpoint);
            if case == "writer refusal" {
                assert!(
                    matches!(&receipt, Message::ChatUi(Action::UpgradeCommitted(_, _, Err(error))) if error.contains("fixture refused recovery"))
                );
                fault
                    .execute_batch("DROP TRIGGER refuse_recovery;")
                    .unwrap();
            } else {
                assert!(matches!(
                    &receipt,
                    Message::ChatUi(Action::UpgradeCommitted(_, _, Ok(())))
                ));
                drop(f.app.update(Message::WorkspaceModelObserved(
                    super::super::workspace_model_sync::ModelUpdate::Read(newer),
                )));
            }
            drop(f.app.update(receipt));
            assert_inert_live_recovery_copy(&f);
        }
        assert!(!f.app.chat_start_pending(f.card));
        assert!(
            f.app.chat_ui.retiring.is_empty() && f.app.chat_ui.abandoned.is_empty(),
            "failed admission only detaches; shared actors are never retired/restarted"
        );
    }
}

#[test]
fn retained_live_model_upgrade_does_not_supersede_account_return_or_close_owners() {
    for owner in ["account", "return", "close", "other source"] {
        let mut f = live_shared_recovery_fixture();
        let target = f.app.chat_ui.next_target(f.card);
        let phase = match owner {
            "account" => Phase::Starting(Box::new(Attempt {
                target,
                intent: chat::Intent::Restore,
                session: f.source.clone(),
                recovery: Some(AccountRecovery {
                    source: f.source.clone(),
                    committed: false,
                    submit: None,
                }),
                upgrade_committed: false,
                resume_committed: false,
                upgrade_admission: None,
            })),
            "return" => Phase::Returning {
                target,
                local: None,
                close_requested: false,
            },
            "other source" => {
                let mut other = f.source.clone();
                other.id = TerminalId::new();
                other.launch.recipe.arguments.push("--other-start".into());
                Phase::Starting(Box::new(Attempt {
                    target,
                    intent: chat::Intent::Restore,
                    session: other,
                    recovery: None,
                    upgrade_committed: false,
                    resume_committed: false,
                    upgrade_admission: None,
                }))
            }
            _ => Phase::Closing {
                target,
                restart: None,
            },
        };
        f.app.chat_ui.phases.insert(f.card, phase);
        drop(f.app.update(Message::WorkspaceModelObserved(
            super::super::workspace_model_sync::ModelUpdate::Read(f.model.clone()),
        )));
        assert_eq!(
            f.app.chat_ui.phases[&f.card].target(),
            target,
            "{owner} owns its generation"
        );
        match (&f.app.chat_ui.phases[&f.card], owner) {
            (Phase::Starting(attempt), "account") => {
                assert!(attempt.recovery.is_some() && attempt.upgrade_admission.is_none())
            }
            (Phase::Starting(attempt), "other source") => {
                assert_ne!(attempt.session, f.source);
                assert!(attempt.recovery.is_none() && attempt.upgrade_admission.is_none());
            }
            (Phase::Returning { .. }, "return") | (Phase::Closing { .. }, "close") => {}
            _ => panic!("{owner} phase changed"),
        }
        assert_live_recovery_content(&f);
    }
}

#[test]
fn retained_live_model_upgrade_candidates_keep_account_recipe_and_proof_guards() {
    for mutation in [
        "account",
        "program",
        "arguments",
        "directory",
        "boot",
        "proof",
    ] {
        let mut f = live_shared_recovery_fixture();
        match mutation {
            "account" => {
                f.canonical
                    .launch
                    .recipe
                    .environment
                    .insert("CLAUDE_CONFIG_DIR".into(), "/other-account".into());
            }
            "program" => f.canonical.launch.recipe.program = "other-provider".into(),
            "arguments" => f
                .canonical
                .launch
                .recipe
                .arguments
                .push("--other-model".into()),
            "directory" => f.canonical.launch.recipe.working_directory = "/other-project".into(),
            "boot" => f.canonical.proof.as_mut().unwrap().boot_id = "other-boot".into(),
            "proof" => f.canonical.proof.as_mut().unwrap().host_incarnation = "".into(),
            other => unreachable!("{other}"),
        }
        assert!(
            !chat::client::UpgradeRecovery::is_candidate(&f.source, &f.canonical),
            "{mutation}"
        );
        f.model.views[0].panes[0].kind = CardKind::Chat(f.canonical.clone());
        drop(f.app.update(Message::WorkspaceModelObserved(
            super::super::workspace_model_sync::ModelUpdate::Read(f.model.clone()),
        )));
        assert!(
            !f.app.chat_start_pending(f.card),
            "{mutation} cannot dispatch a strict or ordinary provider attachment"
        );
        assert_live_recovery_content(&f);
        assert_eq!(chat_runtime(&f.app, f.card).session, f.source);
    }
}

/// A draft has no lineage for the strict upgrade to prove. Another window
/// started its conversation, so this one attaches as a late window does,
/// with what it wrote still unsent.
#[test]
fn retained_live_model_upgrade_leaves_an_unstarted_source_to_an_ordinary_attachment() {
    let mut f = live_shared_recovery_fixture();
    f.source.proof = None;
    f.source.launch.session_id = None;
    f.source.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let runtime = f.app.chat_runtime_mut(f.target).unwrap();
    runtime.session = f.source.clone();
    runtime.connection = Connection::Draft;
    f.app.install_chat_session(f.card, f.source.clone());
    assert!(!chat::client::UpgradeRecovery::is_candidate(
        &f.source,
        &f.canonical
    ));
    f.model.views[0].panes[0].kind = CardKind::Chat(f.canonical.clone());
    drop(f.app.update(Message::WorkspaceModelObserved(
        super::super::workspace_model_sync::ModelUpdate::Read(f.model.clone()),
    )));
    assert!(matches!(
        f.app.chat_ui.phases.get(&f.card),
        Some(Phase::Starting(attempt))
            if attempt.session == f.canonical
                && attempt.intent == chat::Intent::Restore
                && attempt.upgrade_admission.is_none()
    ));
    assert_eq!(f.app.chat_ui.drafts.get(&f.card), Some(&f.draft));
    assert!(f.app.chat_ui.retiring.is_empty());
}

#[test]
fn retained_live_model_upgrade_refuses_future_unnamed_alias_over_a_known_checkpoint() {
    let mut f = live_shared_recovery_fixture_for_identity(None);
    f.canonical.launch.session_id = Some("00000000-0000-4000-8000-000000000002".into());
    f.model.views[0].panes[0].kind = CardKind::Chat(f.canonical.clone());
    let target = project_live_shared_recovery(&mut f);
    f.app.persist();
    f.app
        .session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(10))
        .unwrap();
    // A separate already-known checkpoint wins over this older unnamed
    // alias, even though its future B attachment has authentic lineage.
    let mut checkpoint = (*f.app.persistent_workspace()).clone();
    let mut known = f.source.clone();
    known.launch.session_id = Some(KNOWN_THREAD.into());
    checkpoint
        .cards
        .iter_mut()
        .find(|c| c.id == f.card)
        .unwrap()
        .kind = CardKind::Chat(known.clone());
    checkpoint.pending_chat_admissions.remove(&f.card);
    f.app
        .session
        .as_ref()
        .unwrap()
        .snapshot(Arc::new(checkpoint));
    f.app
        .session
        .as_ref()
        .unwrap()
        .flush(Duration::from_secs(10))
        .unwrap();
    let handle = live_shared_recovery_handle(&f);
    assert!(handle.authenticated_upgrade_recovery().unwrap().validate());
    let task = f.app.update(Message::ChatUi(Action::Ready(
        target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    )));
    let receipt = account_commit_reply(task);
    assert!(
        matches!(&receipt, Message::ChatUi(Action::UpgradeCommitted(_, _, Err(error))) if error.contains("saved conversation changed"))
    );
    assert_eq!(
        f.app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_card(f.card, None)
            .unwrap()
            .unwrap()
            .kind,
        CardKind::Chat(known.clone())
    );
    drop(f.app.update(receipt));
    assert_inert_live_recovery_copy(&f);
    assert!(!f.app.chat_start_pending(f.card));
    assert_eq!(
        f.app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .load_card(f.card, None)
            .unwrap()
            .unwrap()
            .kind,
        CardKind::Chat(known)
    );
}

#[test]
fn retained_claude_identity_is_installed_only_after_its_exact_durable_receipt() {
    // pre-ID learning, Codex normalization, and grant-bound retained reset.
    for (stale_reply, mode, refused) in [
        (false, 0, false),
        (true, 0, false),
        (false, 1, false),
        (true, 1, false),
        (false, 2, false),
        (true, 2, false),
        (false, 2, true),
    ] {
        let codex_resume = mode == 1;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let mut source = chat_runtime(&app, card).session.clone();
        if codex_resume {
            source.launch.provider = Provider::Codex;
            source.launch.recipe.program = "codex".into();
            source.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
            source.launch.session_id = None;
            let allocator = source.proof.clone().unwrap();
            assert!(
                source.claim_owned_codex_listener(
                    std::path::Path::new("/run/test-native"),
                    &allocator
                )
            );
            source.launch.session_id = Some(KNOWN_THREAD.into());
            source.native_continuity = chat::NativeContinuity::Durable {
                provider_version: "0.100.0".into(),
            };
        } else {
            source.launch.provider = Provider::Claude;
            source.launch.recipe.program = "claude".into();
            source.launch.recipe.arguments.clear();
            source.owned_codex_listener = None;
            source.launch.session_id = (mode == 2).then(|| KNOWN_THREAD.into());
            source.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        }
        assert!(
            source.has_valid_native_continuity(),
            "invalid writer fixture source: mode={mode}, stale_reply={stale_reply}, refused={refused}"
        );
        assert_eq!(
            source.owned_codex_listener.is_some(),
            codex_resume,
            "unexpected fixture allocation: mode={mode}, stale_reply={stale_reply}, refused={refused}"
        );
        assert_eq!(
            source.owns_codex_listener(),
            codex_resume,
            "invalid fixture allocation: mode={mode}, stale_reply={stale_reply}, refused={refused}"
        );
        app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
        let draft = "Keep this unsent thought\n  with λ and spacing";
        write(&mut app, card, draft);
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot-test"),
        )
        .unwrap();
        app.session = Some(writer);
        app.persist();
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(10))
            .unwrap();
        let fault = rusqlite::Connection::open(directory.path().join("state.sqlite3")).unwrap();
        if refused {
            // Refuse the identity CAS while allowing the unchanged old-source title autosave.
            fault.execute_batch("CREATE TRIGGER refuse_upgrade BEFORE UPDATE OF content_json ON cards WHEN NEW.content_json != OLD.content_json BEGIN SELECT RAISE(ABORT, 'fixture refused upgraded identity'); END;").unwrap();
        }
        let mut replacement = source.clone();
        replacement.proof.as_mut().unwrap().host_incarnation = "upgraded-daemon".into();
        if codex_resume {
            replacement
                .launch
                .recipe
                .arguments
                .splice(0..0, ["resume".into(), KNOWN_THREAD.into()]);
        } else {
            replacement.launch.session_id = Some("00000000-0000-4000-8000-000000000002".into());
        }
        if mode == 2 {
            assert!(
                app.session
                    .as_ref()
                    .unwrap()
                    .operation_store()
                    .rebind_chat_upgrade(card, &source, &replacement)
                    .is_err(),
                "raw Session values cannot authorize a retained reset"
            );
        }
        let handle = Arc::new(
            ClientHandle::fixture_upgrade_handshake(
                source.clone(),
                replacement.clone(),
                Snapshot {
                    session_id: replacement.launch.session_id.clone(),
                    ready: true,
                    ..Snapshot::default()
                },
            )
            .unwrap(),
        );
        app.chat_forget(card);
        let target = app.chat_ui.next_target(card);
        app.chat_ui.phases.insert(
            card,
            Phase::Starting(Box::new(Attempt {
                target,
                intent: chat::Intent::Restore,
                session: source.clone(),
                recovery: None,
                upgrade_committed: false,
                resume_committed: false,
                upgrade_admission: None,
            })),
        );
        app.cards.remove(&card);
        let task = app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        )));
        assert!(app.chat_start_pending(card));
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(source.clone())
        );
        assert!(!app.cards.contains_key(&card));
        assert_eq!(app.chat_ui.drafts.get(&card).unwrap(), draft);
        // This changed old-source autosave must translate only after the ordered CAS succeeds.
        app.card_state_mut(card).unwrap().title = "Edited while recovery waits".into();
        app.persist();
        let receipt = account_commit_reply(task);
        if refused {
            assert!(matches!(
                &receipt,
                Message::ChatUi(Action::UpgradeCommitted(_, _, Err(_)))
            ));
            let _ = app.update(receipt);
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            assert!(!app.chat_start_pending(card));
            assert!(!matches!(app.cards.get(&card), Some(CardRuntime::Chat(_))));
            assert_eq!(app.persistent_workspace().chat_drafts[&card], draft);
            fault.execute_batch("DROP TRIGGER refuse_upgrade;").unwrap();
            let writer = app.session.as_ref().unwrap();
            writer.flush(Duration::from_secs(10)).unwrap();
            assert_eq!(
                writer
                    .operation_store()
                    .load_card(card, None)
                    .unwrap()
                    .unwrap()
                    .kind,
                CardKind::Chat(source.clone())
            );
            drop(app.session.take());
            let reopened = crate::persistence::StateStore::open(
                directory.path().join("state.sqlite3"),
                crate::model::BootId::new("boot-test"),
            )
            .unwrap();
            assert_eq!(
                reopened.load_card(card, None).unwrap().unwrap().kind,
                CardKind::Chat(source)
            );
            assert_eq!(
                reopened
                    .load_workspace(app.durable.id, None)
                    .unwrap()
                    .unwrap()
                    .chat_drafts[&card],
                draft
            );
            continue;
        }
        assert!(matches!(
            receipt,
            Message::ChatUi(Action::UpgradeCommitted(_, _, Ok(())))
        ));
        if stale_reply {
            let next = app.chat_ui.next_target(card);
            app.chat_ui.phases.insert(
                card,
                Phase::Starting(Box::new(Attempt {
                    target: next,
                    intent: chat::Intent::Restore,
                    session: source.clone(),
                    recovery: None,
                    upgrade_committed: false,
                    resume_committed: false,
                    upgrade_admission: None,
                })),
            );
            let _ = app.update(receipt);
            assert_eq!(app.chat_ui.attempt(card).unwrap().target, next);
            assert!(!app.chat_ui.attempt(card).unwrap().upgrade_committed);
            assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(source));
            assert!(!app.cards.contains_key(&card));
        } else {
            let _ = app.update(receipt);
            assert!(!app.chat_start_pending(card));
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(replacement.clone())
            );
            assert_eq!(chat_runtime(&app, card).session, replacement.clone());
            assert_eq!(chat_runtime(&app, card).composer.text(), draft);
            app.persist();
            let writer = app.session.as_ref().unwrap();
            writer.flush(Duration::from_secs(10)).unwrap();
            assert_eq!(writer.last_error(), None);
            assert_eq!(
                writer
                    .operation_store()
                    .load_card(card, None)
                    .unwrap()
                    .unwrap()
                    .kind,
                CardKind::Chat(replacement.clone())
            );
            assert_eq!(
                writer
                    .operation_store()
                    .load_workspace(app.durable.id, None)
                    .unwrap()
                    .unwrap()
                    .chat_drafts[&card],
                draft
            );
            drop(app.session.take());
            let reopened = crate::persistence::StateStore::open(
                directory.path().join("state.sqlite3"),
                crate::model::BootId::new("boot-test"),
            )
            .unwrap();
            assert_eq!(
                reopened.load_card(card, None).unwrap().unwrap().kind,
                CardKind::Chat(replacement)
            );
            assert_eq!(
                reopened
                    .load_workspace(app.durable.id, None)
                    .unwrap()
                    .unwrap()
                    .chat_drafts[&card],
                draft
            );
        }
    }
}

#[test]
fn flow_dispatch_prepares_cached_tools_and_recreated_temporary_contexts() {
    use iced::advanced::{
        Layout, Shell, layout, mouse,
        widget::{Operation, Tree},
    };
    use iced::futures::StreamExt;
    #[derive(Default)]
    struct Read(Vec<Arc<str>>);
    impl Operation for Read {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn custom(
            &mut self,
            _: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn std::any::Any,
        ) {
            if let Some(state) = state.downcast_ref::<selectable_prose::State>() {
                self.0.push(state.plain.clone());
            }
        }
    }
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let theme = crate::settings::EditorSyntaxTheme::Auto;
    let source = "x".repeat(6000);
    let renderer = crate::app::tests::headless_renderer();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 200.0));
    for temporary in [false, true] {
        if !temporary {
            let mut cached = CachedItem::updated(&text("tool"), theme, None, false, false, None);
            cached.tool_output = Some(tool_text::ToolText::update(
                None,
                &source,
                tool_text::Language::Plain,
            ));
            app.chat_runtime_mut(target)
                .unwrap()
                .cache
                .insert("tool".into(), cached);
        }
        let make = |app: &Cagents| {
            if temporary {
                // Approval/reader contexts construct a temporary ToolText;
                // the actual native Tree is its surviving flow owner.
                tool_text::ToolText::update(None, &source, tool_text::Language::Plain)
                    .view(theme, None, target, None)
            } else {
                chat_runtime(app, card).cache["tool"]
                    .tool_output
                    .as_ref()
                    .unwrap()
                    .view_logical_with_reader(
                        theme,
                        None,
                        target,
                        None,
                        selectable_prose::Logical {
                            identity: 42,
                            start: 0,
                        },
                        scroll::Memory::default(),
                        None,
                    )
            }
        };
        let mut element = make(&app);
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &iced::Rectangle::with_size(iced::Size::new(400.0, 200.0)),
        );
        let action = messages
            .into_iter()
            .find_map(|message| match message {
                Message::ChatUi(action @ Action::FlowRequest(..)) => Some(action),
                _ => None,
            })
            .expect("visible native flow publishes its request");
        let before = app
            .chat_runtime_mut(target)
            .unwrap()
            .cache
            .get("tool")
            .map(|cached| cached.revision);
        // The previous iteration's shaping slot is released on the
        // retirement worker; a dispatch that overtakes it is only deferred.
        preparation::retirement_barrier();
        let reply = executor.block_on(async {
            let task = app.handle_chat_action(action);
            let mut outputs =
                iced::runtime::task::into_stream(task).expect("actual dispatch starts shaping");
            tokio::time::timeout(Duration::from_secs(20), async {
                while let Some(output) = outputs.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                        action @ Action::FlowReady(..),
                    ))) = output
                    {
                        return action;
                    }
                }
                panic!("flow task produced no completion");
            })
            .await
            .expect("bounded fixture completion")
        });
        drop(app.handle_chat_action(reply));
        if !temporary {
            assert_ne!(
                app.chat_runtime_mut(target).unwrap().cache["tool"].revision,
                before.unwrap()
            );
        }
        let mut refreshed = make(&app);
        refreshed.as_widget_mut().diff(&mut tree);
        let node = refreshed
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut read = Read::default();
        refreshed
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut read);
        assert!(
            read.0.iter().any(|plain| plain.as_ref() == source),
            "completed glyph owner survives context recreation"
        );
    }
}

#[test]
fn collapsed_tool_bookmarks_survive_runtime_updates_and_reopen_only_after_worker_verification() {
    use iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    struct Move;
    impl Operation for Move {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            _: Option<&widget::Id>,
            _: iced::Rectangle,
            _: iced::Rectangle,
            _: iced::Vector,
            state: &mut dyn operation::Scrollable,
        ) {
            state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                x: Some(100.0),
                y: Some(300.0),
            });
        }
    }
    #[derive(Default)]
    struct Read(Vec<iced::Vector>);
    impl Operation for Read {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            _: Option<&widget::Id>,
            _: iced::Rectangle,
            _: iced::Rectangle,
            translation: iced::Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            self.0.push(translation);
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(400.0, 200.0));
    for child in [false, true] {
        for hidden in [false, true] {
            for rewrite in [false, true] {
                let mut app = crate::app::tests::context_inspector_test_app(false);
                let card = live_chat(&mut app, Some(KNOWN_THREAD));
                let target = chat_runtime(&app, card).target;
                let original = format!("{}old-tail", "x".repeat(4096));
                let item = Arc::new(chat::Item {
                    id: "bookmarked-tool".into(),
                    role: chat::Role::Assistant,
                    content: ItemContent::Tool {
                        name: "FixtureInspect".into(),
                        input: original.clone(),
                        output: original.clone(),
                        status: chat::ToolStatus::Running,
                    },
                    started_at: None,
                    at: None,
                });
                let mut snapshot = Snapshot {
                    generation: 5,
                    session_id: Some(KNOWN_THREAD.into()),
                    ready: true,
                    items: vec![item.clone()],
                    ..Snapshot::default()
                };
                if child {
                    snapshot.subagents.push(chat::Subagent {
                        id: "selected-child".into(),
                        kind: "fixture".into(),
                        description: "Child".into(),
                        status: chat::ToolStatus::Running,
                        model: None,
                        waiting: false,
                        detail: String::new(),
                        background: true,
                        items: vec![item.clone()],
                    });
                }
                show_chat_snapshot_for_test(&mut app, card, snapshot.clone());
                if child {
                    drop(app.handle_chat_ui(Action::SelectSubagent(
                        target,
                        Some("selected-child".into()),
                    )));
                }
                drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
                let runtime = app.chat_runtime_mut(target).unwrap();
                let memory = runtime.scroll_memory.clone();
                let context = runtime.selected_subagent.clone();
                // Mount native reader geometry with the actual cached
                // ToolText source authority, then destroy every native tree.
                let mount = |source: reader_source::Source, part, axis, narrow: bool| {
                    let logical = selection::logical_identity(&item.id, &[part]);
                    let key = scroll::reader_key(logical, axis);
                    let remembered = memory.reader_width_for(context.as_deref(), key, &source);
                    scroll::block_source(
                        widget::scrollable(scroll::minimum_width(
                            widget::container(
                                widget::Space::new()
                                    .width(if narrow { 100.0 } else { 2000.0 })
                                    .height(2000),
                            ),
                            remembered,
                        ))
                        .direction(iced::widget::scrollable::Direction::Both {
                            vertical: design::scrollbar(),
                            horizontal: design::scrollbar(),
                        })
                        .width(Length::Fill)
                        .height(Length::Fill),
                        memory.clone(),
                        context.clone(),
                        key,
                        false,
                        Some(source),
                    )
                };
                for part in [4, 5] {
                    let cached = &runtime.presentation_cache()[&item.id];
                    let source = if part == 4 {
                        &cached.tool_input
                    } else {
                        &cached.tool_output
                    }
                    .as_ref()
                    .unwrap()
                    .reader_source
                    .clone();
                    for axis in [scroll::ReaderAxis::Horizontal, scroll::ReaderAxis::Vertical] {
                        let mut element = mount(source.clone(), part, axis, false);
                        let mut tree = Tree::new(&element);
                        let node = element
                            .as_widget_mut()
                            .layout(&mut tree, &renderer, &limits);
                        element.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &mut Move,
                        );
                        element
                            .as_widget_mut()
                            .layout(&mut tree, &renderer, &limits);
                    }
                }
                let original_proofs = memory.reader_proofs(context.as_deref());
                assert_eq!(original_proofs.len(), 4);
                drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
                assert!(
                    !app.chat_runtime_mut(target)
                        .unwrap()
                        .item_expanded(&item.id)
                );
                let handle = app
                    .chat_runtime_mut(target)
                    .unwrap()
                    .handle()
                    .unwrap()
                    .clone();
                if hidden {
                    app.chat_runtime_mut(target)
                        .unwrap()
                        .set_visible(false)
                        .unwrap();
                    app.chat_runtime_mut(target).unwrap().cool();
                    preparation::retirement_barrier();
                    assert!(
                        app.chat_runtime_mut(target)
                            .unwrap()
                            .presentation_cache()
                            .is_empty()
                    );
                    assert_eq!(handle.snapshot().transcript_bytes(), 0);
                    app.chat_runtime_mut(target)
                        .unwrap()
                        .set_visible(true)
                        .unwrap();
                }
                let changed = if rewrite {
                    format!("{}new-tail", "x".repeat(4096))
                } else {
                    format!("{original} appended while collapsed")
                };
                let next = Arc::new(chat::Item {
                    content: ItemContent::Tool {
                        name: "FixtureInspect".into(),
                        input: changed.clone(),
                        output: changed.clone(),
                        status: chat::ToolStatus::Running,
                    },
                    ..(*item).clone()
                });
                snapshot.generation += 1;
                snapshot.items[0] = next.clone();
                if child {
                    snapshot.subagents[0].items[0] = next.clone();
                }
                handle.replace_snapshot_for_test(snapshot);
                let runtime = app.chat_runtime_mut(target).unwrap();
                runtime.refresh();
                assert!(
                    runtime.preparation.contains(&item.id),
                    "collapsed bookmarked items use actual worker admission"
                );
                assert_eq!(
                    *memory.reader_proofs(context.as_deref()),
                    *original_proofs,
                    "prefix evidence changes only after worker completion"
                );
                // Reopen before output arrives: Toggle must preserve the
                // actual admitted owner rather than construct a cold reader.
                drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
                let runtime = app.chat_runtime_mut(target).unwrap();
                assert!(runtime.item_expanded(&item.id));
                assert!(runtime.presentation_cache()[&item.id].preparing);
                preparation::drain_fixture(runtime);
                let cached = &runtime.presentation_cache()[&item.id];
                assert!(!cached.preparing);
                for part in [4, 5] {
                    let reader = if part == 4 {
                        &cached.tool_input
                    } else {
                        &cached.tool_output
                    }
                    .as_ref()
                    .unwrap();
                    assert_eq!(reader.text.as_ref(), changed);
                    // Exact precomputed source/Arc descriptors survive a
                    // later disclosure rebuild without rehashing or copying.
                    let shared = reader.text.clone();
                    for axis in [scroll::ReaderAxis::Horizontal, scroll::ReaderAxis::Vertical] {
                        let key = scroll::reader_key(
                            selection::logical_identity(&item.id, &[part]),
                            axis,
                        );
                        let remembered =
                            memory.reader_width_for(context.as_deref(), key, &reader.reader_source);
                        assert_eq!(remembered >= 2000.0, !rewrite);
                        let mut element = mount(reader.reader_source.clone(), part, axis, true);
                        let mut tree = Tree::new(&element);
                        let node = element
                            .as_widget_mut()
                            .layout(&mut tree, &renderer, &limits);
                        let mut read = Read::default();
                        element.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &mut read,
                        );
                        assert_eq!(
                            read.0,
                            [if rewrite {
                                iced::Vector::ZERO
                            } else {
                                iced::Vector::new(100.0, 300.0)
                            }]
                        );
                    }
                    assert!(Arc::ptr_eq(&shared, &reader.text));
                }
                let source_owner = cached.tool_output.as_ref().unwrap().text.clone();
                drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
                drop(app.handle_chat_ui(Action::Toggle(target, item.id.clone())));
                assert!(
                    Arc::ptr_eq(
                        &source_owner,
                        &app.chat_runtime_mut(target).unwrap().presentation_cache()[&item.id]
                            .tool_output
                            .as_ref()
                            .unwrap()
                            .text
                    ),
                    "reopening shares worker-prepared immutable body"
                );
            }
        }
    }
}

#[test]
fn hidden_chat_releases_all_transcript_owners_and_rehydrates_reader_state() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let target = chat_runtime(&app, card).target;
    let snapshot = || Snapshot {
        generation: 17,
        session_id: Some(KNOWN_THREAD.into()),
        ready: true,
        items: vec![Arc::new(chat::Item {
            id: "parent".into(),
            role: chat::Role::Assistant,
            content: text(&format!(
                "```rust\n{}\n```",
                "let value = 123;\n".repeat(4096)
            )),
            started_at: None,
            at: None,
        })],
        subagents: vec![chat::Subagent {
            id: "child".into(),
            kind: "fixture".into(),
            description: "Child".into(),
            status: chat::ToolStatus::Succeeded,
            model: None,
            waiting: false,
            detail: String::new(),
            background: false,
            items: vec![Arc::new(chat::Item {
                id: "child-item".into(),
                role: chat::Role::Assistant,
                content: text(&"child words\n".repeat(4096)),
                started_at: None,
                at: None,
            })],
        }],
        ..Snapshot::default()
    };
    let first = snapshot();
    let parent = Arc::downgrade(&first.items[0]);
    let child = Arc::downgrade(&first.subagents[0].items[0]);
    show_chat_snapshot_for_test(&mut app, card, first);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let handle = runtime.handle().unwrap().clone();
    let client_retired = handle.observe_snapshot_retirement_for_test();
    runtime.replace_draft(text_editor::Content::with_text("Unsent draft"));
    runtime.selected_subagent = Some("child".into());
    let plain: Arc<str> = Arc::from("Rendered selected child words\n".repeat(4096));
    let weak_plain = Arc::downgrade(&plain);
    let mut document = selection::Catalog::builder();
    document.plain("child-item", &[0], plain);
    let document = document.finish();
    let weak_document = Arc::downgrade(&document);
    let (anchor, end) = document.endpoints().unwrap();
    let bookmark = std::rc::Rc::new(std::cell::RefCell::new(selection::Bookmark {
        anchor: Some(anchor),
        end: Some(end),
        anchor_prefix: document.fingerprint(anchor),
        end_prefix: document.fingerprint(end),
    }));
    let key = runtime.selection_key();
    runtime
        .selection_document
        .get_mut()
        .replace((key, document));
    runtime
        .selection_bookmarks
        .get_mut()
        .insert(Some("child".into()), bookmark.clone());
    runtime.scroll = view::Scroll::new(200.0, 1000.0, 400.0);
    runtime
        .subagent_scroll
        .insert("child".into(), view::Scroll::new(75.0, 900.0, 400.0));
    runtime.expanded.edit().insert("parent".into());
    let scroll = runtime.scroll;
    let child_scroll = runtime.subagent_scroll.clone();
    let turns = runtime
        .turns
        .iter()
        .map(|turn| turn.key.clone())
        .collect::<Vec<_>>();
    runtime.unseen_activity = false;
    assert!(!runtime.cache.is_empty());
    runtime.set_visible(false).unwrap();
    runtime.cool();
    preparation::retirement_barrier();
    // Client publications retire on a separate worker. Waiting for UI
    // presentation disposal alone does not establish body reclamation.
    client_retired
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the hidden client's exact publication must retire");
    assert!(runtime.cache.is_empty());
    assert!(runtime.selection_document.borrow().is_none());
    assert!(
        weak_document.upgrade().is_none(),
        "the logical selection catalog must be released"
    );
    assert!(
        weak_plain.upgrade().is_none(),
        "selection must not retain hidden rendered text"
    );
    assert!(std::rc::Rc::ptr_eq(
        &runtime.selection_bookmarks.borrow()[&Some("child".into())],
        &bookmark
    ));
    assert_eq!(bookmark.borrow().anchor, Some(anchor));
    assert_eq!(bookmark.borrow().end, Some(end));
    assert_eq!(runtime.snapshot.transcript_bytes(), 0);
    assert_eq!(handle.snapshot().transcript_bytes(), 0);
    assert!(handle.resident_snapshot_with_clock().is_none());
    assert!(
        parent.upgrade().is_none(),
        "client, runtime and presentation must all release the parent body"
    );
    assert!(
        child.upgrade().is_none(),
        "unselected child bodies must be released too"
    );
    assert_eq!(runtime.composer.text().trim_end(), "Unsent draft");
    assert_eq!(runtime.scroll, scroll);
    assert_eq!(runtime.subagent_scroll, child_scroll);
    assert_eq!(runtime.selected_subagent.as_deref(), Some("child"));
    runtime.set_visible(true).unwrap();
    assert!(
        runtime.cache.is_empty(),
        "reveal waits for an authoritative body"
    );
    // The daemon answers the unconditional read at the same generation.
    handle.replace_snapshot_for_test(snapshot());
    runtime.refresh();
    preparation::drain_fixture(runtime);
    assert!(runtime.snapshot.transcript_bytes() > 0);
    assert!(!runtime.presentation_cache().is_empty());
    assert!(!runtime.child_cache["child-item"].placeholder);
    assert_eq!(runtime.composer.text().trim_end(), "Unsent draft");
    assert_eq!(runtime.scroll, scroll);
    assert_eq!(runtime.subagent_scroll, child_scroll);
    assert_eq!(runtime.selected_subagent.as_deref(), Some("child"));
    assert!(runtime.expanded.contains("parent"));
    assert_eq!(
        runtime
            .turns
            .iter()
            .map(|turn| turn.key.clone())
            .collect::<Vec<_>>(),
        turns
    );
    assert!(
        !runtime.unseen_activity,
        "unchanged rehydration must not invent new activity"
    );
    let mut changed = snapshot();
    changed.generation += 1;
    Arc::make_mut(&mut changed.items[0]).content = text("new live tail");
    handle.replace_snapshot_for_test(changed);
    runtime.refresh();
    assert!(
        runtime.unseen_activity,
        "new tail content still signals unread activity"
    );
}

#[test]
fn late_connection_callbacks_cannot_change_a_reconnected_card() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some(KNOWN_THREAD));
    let old = chat_runtime(&app, card).target;
    // This is exactly the closure shape installed by start_chat: a late
    // worker callback carries the connection Target, never the current card.
    let sender = app.chat_ui.sender.clone();
    let late_callback = move |wake| sender.try_send((old, wake)).unwrap();
    drop(app.handle_chat_ui(Action::Reconnect(old)));
    let attempt = app.chat_ui.attempt(card).unwrap().target;
    let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
    answer_start(&mut app, attempt, Ok(handle.clone()));
    let target = chat_runtime(&app, card).target;
    assert_ne!(old, target);
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("new connection draft"));
    let snapshot = chat_runtime(&app, card).snapshot.clone();
    let session = chat_runtime(&app, card).session.clone();
    let account_report = crate::account::LiveReport::CodexLimits(
        crate::account::codex::parse_update(&serde_json::json!({"rateLimits": {
            "primary": {"usedPercent": 31, "windowDurationMins": 300},
        }}))
        .unwrap(),
    );
    assert!(handle.push_account_usage_for_test(account_report, crate::chat::now_ms().unwrap()));
    assert!(app.chat_ui.receiver.is_empty());
    for wake in [
        Wake::Changed,
        Wake::Observed,
        Wake::Activity,
        Wake::AccountUsage,
        Wake::Limit,
        Wake::AgentAddresses,
        Wake::Disconnected("old connection retired".into()),
    ] {
        late_callback(wake);
        let (tag, wake) = app.chat_ui.receiver.try_recv().unwrap();
        assert_eq!(tag, old);
        drop(app.handle_chat_ui(Action::Wake(tag, wake)));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.target, target);
        assert!(!runtime.disconnected);
        assert!(runtime.error.is_none());
        assert_eq!(runtime.snapshot, snapshot);
        assert_eq!(runtime.session, session);
        assert_eq!(runtime.composer.text().trim(), "new connection draft");
    }
    assert_eq!(
        handle.take_account_usage().len(),
        1,
        "an old account wake must not drain the replacement connection's report queue"
    );
}
