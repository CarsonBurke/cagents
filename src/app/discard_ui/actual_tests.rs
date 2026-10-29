//! Exercise the real App confirmation/tasks/receipt consumer in an isolated
//! process. Desktop Trash and runtime are private to this exact child.
use super::*;
use crate::process::{CommandSpec, ProcessExecutor};
use cosmic::Application;
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn isolated(name: &str) -> bool {
    if std::env::var("CAGENTS_DISCARD_CASE").as_deref() == Ok(name) {
        return false;
    }
    let owner = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let runtime = owner.path().join("runtime");
    let data = owner.path().join("data");
    let root = owner.path().join("checkout");
    for dir in [&runtime, &data, &root] {
        fs::create_dir(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let stdout = owner.path().join("stdout");
    let stderr = owner.path().join("stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    command
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env("CAGENTS_DISCARD_CASE", name)
        .env("CAGENTS_DISCARD_ROOT", &root)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_DATA_HOME", data)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "discard child timed out");
        std::thread::sleep(Duration::from_millis(10));
    };
    fn read(path: &Path) -> String {
        let mut bytes = Vec::new();
        fs::File::open(path)
            .unwrap()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= 1024 * 1024);
        String::from_utf8_lossy(&bytes).into_owned()
    }
    let out = read(&stdout);
    let err = read(&stderr);
    assert!(status.success(), "{name}: {status}\n{out}\n{err}");
    assert!(
        out.contains(&format!("test {name} ... ok")),
        "missing exact positive test: {out}\n{err}"
    );
    assert!(
        out.contains("test result: ok. 1 passed; 0 failed;"),
        "zero/incorrect selector: {out}"
    );
    true
}

fn git(root: &Path, arguments: &[&str]) {
    let command = CommandSpec::new("git")
        .cwd(root)
        .args(arguments.iter().copied())
        .timeout(Duration::from_secs(10));
    SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap()
        .require_success(&command.program)
        .unwrap();
}

/// A batch of no-op tasks can still own an empty stream. Prove there is no
/// work before polling, then require synchronous exhaustion with no action.
fn assert_no_app_work(task: Task<Message>) {
    use cosmic::iced::futures::{FutureExt, StreamExt};

    assert_eq!(task.units(), 0, "bookkeeping must schedule no App work");
    if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
        assert!(
            matches!(stream.next().now_or_never(), Some(None)),
            "bookkeeping must finish immediately without any App action"
        );
    }
}

#[test]
fn discard_app_accessibility_click_uses_current_rendered_source_and_opens_confirmation() {
    const NAME: &str = "app::discard_ui::actual_tests::discard_app_accessibility_click_uses_current_rendered_source_and_opens_confirmation";
    if isolated(NAME) {
        return;
    }
    use cosmic::iced::{
        Event, Size,
        advanced::{clipboard, mouse},
        runtime::user_interface::{Cache, UserInterface},
    };
    use iced_accessibility::accesskit::{Action, ActionRequest, NodeId, Role, TreeId};

    const DRAFT: &str = "Unsent Codex draft must survive the inspector action";

    // Use the complete App view, mounted focused chat and its real UI cache,
    // with no pointer hover. No provider/daemon task is polled in this fixture.
    // The event uses Winit's numeric conversion of the CURRENT emitted node;
    // an AT-SPI global object-path number is not a local AccessKit node ID.
    fn frame(
        app: &Cagents,
        cache: Cache,
        click: bool,
        chat: CardId,
        original_draft: &str,
    ) -> (Cache, NodeId, Vec<Message>) {
        use cosmic::iced::advanced::widget::{Operation, operation};

        struct ComposerFocus {
            id: widget::Id,
            found: usize,
            focused: bool,
        }
        impl Operation for ComposerFocus {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _bounds: cosmic::iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if id == Some(&self.id) {
                    self.found += 1;
                    self.focused = state.is_focused();
                }
            }
        }

        assert_eq!(app.workspace.focused(), Some(chat));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&chat) else {
            panic!("the focused center pane must be a mounted chat");
        };
        assert_eq!(
            runtime.session.launch.provider,
            crate::agent::Provider::Codex
        );
        assert_eq!(runtime.unsent_text(), original_draft);
        assert!(!app.chat_start_pending(chat));
        let mut renderer = crate::app::tests::headless_renderer();
        let mut ui =
            UserInterface::build(app.view(), Size::new(1280.0, 900.0), cache, &mut renderer);
        let composer_id = card_control_id(chat, "chat-composer");
        if !click {
            // Install native widget focus once; later cached rebuilds must
            // preserve it without re-focusing or bypassing the mounted pane.
            ui.operate(
                &renderer,
                &mut operation::focusable::focus::<()>(composer_id.clone()),
            );
        }
        let mut focus = ComposerFocus {
            id: composer_id.clone(),
            found: 0,
            focused: false,
        };
        ui.operate(&renderer, &mut focus);
        assert_eq!(focus.found, 1, "one real mounted composer focus owner");
        assert!(focus.focused, "the real composer owns keyboard focus");
        let accessible = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let composer_ax_id = iced_accessibility::A11yId::from(composer_id);
        let composers = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter(|node| {
                node.node().role() == Role::MultilineTextInput
                    && iced_accessibility::IdEq::eq(node.id(), &composer_ax_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            composers.len(),
            1,
            "the current chat composer is exported exactly once by App.view"
        );
        assert!(!composers[0].node().is_disabled());
        assert!(!composers[0].node().is_hidden());
        let composer_bounds = composers[0]
            .node()
            .bounds()
            .expect("mounted composer geometry");
        assert!(composer_bounds.x0 >= 0.0 && composer_bounds.x1 <= 1280.0);
        assert!(composer_bounds.y0 >= 0.0 && composer_bounds.y1 <= 900.0);
        assert!(
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| {
                    node.node().role() == Role::TextRun
                        && node
                            .node()
                            .value()
                            .is_some_and(|value| value.trim_end() == DRAFT)
                }),
            "the actual composer exports its unchanged nonempty draft"
        );
        let buttons = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter(|node| {
                node.node().role() == Role::Button
                    && node.node().label() == Some("Discard all Unstaged")
            })
            .collect::<Vec<_>>();
        assert_eq!(buttons.len(), 1, "one actual current discard control");
        assert!(!buttons[0].node().is_disabled());
        let bounds = buttons[0].node().bounds().expect("real button geometry");
        assert!(bounds.x0 >= 0.0 && bounds.x1 <= 1280.0);
        assert!(bounds.y0 >= 0.0 && bounds.y1 <= 900.0);
        let target_node = NodeId::from(buttons[0].id().clone());
        assert_ne!(target_node, NodeId::from(composers[0].id().clone()));
        assert!(
            composer_bounds.x1 <= bounds.x0,
            "the focused composer is left of the inspector control"
        );
        let mut emitted = Vec::new();
        if click {
            let event = Event::A11y(
                widget::Id::from(u128::from(target_node.0) as u64),
                ActionRequest {
                    action: Action::Click,
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
                &mut emitted,
            );
            assert_eq!(statuses, vec![cosmic::iced::event::Status::Captured]);
            focus.found = 0;
            focus.focused = false;
            ui.operate(&renderer, &mut focus);
            assert_eq!(focus.found, 1);
            assert!(
                focus.focused,
                "a foreign AX Click must not take composer focus"
            );
        }
        (ui.into_cache(), target_node, emitted)
    }

    fn discard_message(
        app: &mut Cagents,
        messages: Vec<Message>,
        original: uuid::Uuid,
        key: &ActiveWorktreeKey,
    ) -> Message {
        let mut discard = None;
        let mut selection_clears = [false; 2];
        for message in messages {
            match &message {
                Message::SidebarSnapshot { source, action }
                    if *source == original
                        && matches!(&**action,
                        SidebarAction::ChangeGroup { key: observed,
                            area: crate::repository::ChangeArea::Unstaged,
                            command: sidebar::ChangeGroupCommand::Discard } if observed == key) =>
                {
                    assert!(
                        discard.replace(message).is_none(),
                        "one Click emits one discard intent"
                    );
                }
                Message::SidebarSnapshot { source, action }
                    if *source == original
                        && matches!(&**action,
                        SidebarAction::ChangeViewport { key: observed, scope: sidebar::ChangeTreeScope::Area(
                            crate::repository::ChangeArea::Unstaged | crate::repository::ChangeArea::Untracked), first_row: 0, .. }
                            if observed == key) =>
                {
                    // Reduce only the real mounted lists' layout callbacks.
                    // No emitted work or unrelated input is discarded.
                    assert_no_app_work(app.update(message));
                    assert_eq!(app.discard_ui.source().unwrap().id, original);
                    assert!(!app.modal_is_open());
                }
                Message::SidebarSnapshot { source, action }
                    if *source == original
                        && matches!(&**action,
                            SidebarAction::CommitSubjectEditorAction { key: observed,
                                action: cosmic::iced::widget::text_editor::Action::ClearSelection }
                            | SidebarAction::CommitEditorAction { key: observed,
                                action: cosmic::iced::widget::text_editor::Action::ClearSelection }
                            if observed == key) =>
                {
                    // Focusing the real composer unfocuses both Changes
                    // editors. Each publishes its deferred selection clear on
                    // the next event; reduce that exact current bookkeeping.
                    let editor = match &**action {
                        SidebarAction::CommitSubjectEditorAction { .. } => 0,
                        SidebarAction::CommitEditorAction { .. } => 1,
                        _ => unreachable!(),
                    };
                    assert!(!selection_clears[editor], "one clear per mounted editor");
                    selection_clears[editor] = true;
                    let commit = app.sidebar_state.commit_message().to_owned();
                    let revision = app
                        .sidebar_state
                        .submitted_commit_revision(key, &commit, false)
                        .expect("the current fixture commit draft is bound");
                    let focused = app.workspace.focused().expect("mounted chat focus");
                    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&focused) else {
                        panic!("selection bookkeeping must retain the focused chat");
                    };
                    let target = runtime.target;
                    assert_eq!(runtime.unsent_text(), DRAFT);
                    assert_no_app_work(app.update(message));
                    assert_eq!(app.sidebar_state.commit_message(), commit);
                    assert_eq!(
                        app.sidebar_state
                            .submitted_commit_revision(key, &commit, false),
                        Some(revision)
                    );
                    assert_eq!(app.workspace.focused(), Some(focused));
                    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&focused) else {
                        panic!("selection bookkeeping must retain the mounted chat");
                    };
                    assert_eq!(runtime.target, target);
                    assert_eq!(runtime.unsent_text(), DRAFT);
                    assert_eq!(app.discard_ui.source().unwrap().id, original);
                    assert!(!app.modal_is_open());
                }
                Message::ChatUi(chat_ui::Action::Scrolled(target, None, _, false))
                    if app.workspace.focused() == Some(target.card)
                        && matches!(app.cards.get(&target.card),
                            Some(CardRuntime::Chat(runtime)) if runtime.target == *target) =>
                {
                    // The mounted transcript reports its initial geometry on
                    // the first event, including a foreign inspector AX Click.
                    // Reduce only this exact current, non-user layout report.
                    let target = *target;
                    assert_no_app_work(app.update(message));
                    assert_eq!(app.workspace.focused(), Some(target.card));
                    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&target.card) else {
                        panic!("layout bookkeeping must retain the mounted chat");
                    };
                    assert_eq!(runtime.target, target);
                    assert_eq!(runtime.unsent_text(), DRAFT);
                    assert_eq!(app.discard_ui.source().unwrap().id, original);
                    assert!(!app.modal_is_open());
                }
                _ => panic!("unexpected actual A11y message: {message:?}"),
            }
        }
        discard.expect("the actual current UI consumes Click into a discard intent")
    }

    let root = PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap());
    git(&root, &["init", "-q"]);
    fs::write(root.join("modified"), b"index modified\n").unwrap();
    fs::write(root.join("deleted"), b"index deleted\n").unwrap();
    git(&root, &["add", "--all"]);
    git(&root, &["commit", "-qm", "base"]);
    fs::write(root.join("modified"), b"precious modified\n").unwrap();
    fs::remove_file(root.join("deleted")).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    let untracked = root.join("nested/original file");
    fs::write(&untracked, b"precious untracked\n").unwrap();
    let untracked_identity = fs::metadata(&untracked).unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.window_width = 1280.0;
    app.window_height = 900.0;
    app.active_workspace = Some(root.clone());
    let key = ActiveWorktreeKey {
        project_id: crate::model::ProjectId::new(),
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.clone(),
    };
    app.sidebar_model = sidebar::SidebarModel::ready(
        sidebar::SidebarMode::Expanded,
        vec![sidebar::SidebarProject::new(
            key.project_id,
            crate::repository::RepositoryId::new(),
            "A11y discard fixture".into(),
            root.clone(),
            vec![sidebar::SidebarWorktree::from_durable(
                key.worktree_id,
                root.clone(),
                None,
            )],
            Vec::new(),
        )],
    )
    .with_source_control(Some(SourceControlModel::loading(key.clone())));
    assert!(app.sidebar_state.sync_active_worktree(
        &app.sidebar_model,
        &sidebar::SidebarFocus::Repository {
            worktree: root.clone()
        }
    ));
    // Model the native failure's nonempty center pane through the existing
    // public App insertion/Ready path. The asynchronous start task is dropped,
    // and Ready uses the existing in-process ClientHandle socket fixture.
    app.workspace.resize_viewport(540.0);
    let session = crate::chat::Session {
        id: crate::model::TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Codex,
            recipe: crate::model::LaunchRecipe {
                program: "codex".into(),
                arguments: Vec::new(),
                working_directory: root.clone(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: Some("a11y-mounted-codex-thread".into()),
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
    };
    drop(app.insert_chat_card(session, None, None));
    let chat = app.workspace.focused().expect("inserted chat is focused");
    assert!(app.chat_start_pending(chat));
    let ready = crate::app::chat_ui::ready_message_for_pending_switch(
        &app,
        chat,
        crate::chat::Snapshot {
            ready: true,
            session_id: Some("a11y-mounted-codex-thread".into()),
            ..crate::chat::Snapshot::default()
        },
    );
    drop(app.update(ready));
    assert!(!app.chat_start_pending(chat));
    let target = match app.cards.get(&chat).expect("ready chat runtime") {
        CardRuntime::Chat(runtime) => runtime.target,
        _ => panic!("Ready must install the actual chat runtime"),
    };
    drop(
        app.update(Message::ChatUi(crate::app::chat_ui::Action::Composer(
            target,
            cosmic::widget::text_editor::Action::Edit(cosmic::widget::text_editor::Edit::Paste(
                Arc::new(DRAFT.to_owned()),
            )),
        ))),
    );
    let original_draft = match app.cards.get(&chat).expect("typed chat runtime") {
        CardRuntime::Chat(runtime) => runtime.unsent_text(),
        _ => unreachable!(),
    };
    assert_eq!(original_draft.trim_end(), DRAFT);
    app.context_tab = ContextTab::Changes;
    let loaded = crate::app::tests::task_app_message(app.source_control_snapshot_task(key.clone()));
    assert!(matches!(
        &loaded,
        Message::SourceControlLoaded { result: Ok(_), .. }
    ));
    drop(app.update(loaded));
    let original = app.discard_ui.source().unwrap().id;
    let source = sidebar::bound_source_control(&app.sidebar_model, &app.sidebar_state).unwrap();
    assert_eq!(source.data_state, sidebar::SidebarDataState::Ready);
    assert_eq!(
        source
            .group(crate::repository::ChangeArea::Unstaged)
            .unwrap()
            .rows()
            .len(),
        2
    );
    assert_eq!(
        source
            .group(crate::repository::ChangeArea::Untracked)
            .unwrap()
            .rows()
            .len(),
        1
    );
    let (cache, first_id, empty) = frame(&app, Cache::new(), false, chat, &original_draft);
    assert!(empty.is_empty());
    // Periodic Git refresh uses this same real worker/completion boundary.
    // A refresh of the same binding must not retire the rendered actor.
    let loaded = crate::app::tests::task_app_message(app.source_control_snapshot_task(key.clone()));
    assert!(
        matches!(&loaded, Message::SourceControlLoaded { source, result: Ok(_), .. }
        if source.id == original)
    );
    drop(app.update(loaded));
    assert_eq!(app.discard_ui.source().unwrap().id, original);
    let (cache, rebuilt_id, emitted) = frame(&app, cache, true, chat, &original_draft);
    assert_eq!(
        rebuilt_id, first_id,
        "the cached current button retains its native target"
    );
    let queued = discard_message(&mut app, emitted, original, &key);

    // Retire only the original source actor after actual widget dispatch.
    // The old queued action cannot borrow the replacement's authority, even
    // when the replacement has the same logical worktree and current rows.
    app.discard_ui.retire_source();
    let loaded = crate::app::tests::task_app_message(app.source_control_snapshot_task(key.clone()));
    assert!(matches!(
        &loaded,
        Message::SourceControlLoaded { result: Ok(_), .. }
    ));
    drop(app.update(loaded));
    let current = app.discard_ui.source().unwrap().id;
    assert_ne!(current, original);
    assert_no_app_work(app.update(queued));
    assert!(!app.modal_is_open());
    assert!(app.sidebar_state.pending_discard().is_none());
    assert_eq!(app.discard_ui.source().unwrap().id, current);

    let (_, _, emitted) = frame(&app, cache, true, chat, &original_draft);
    let current_message = discard_message(&mut app, emitted, current, &key);
    drop(app.update(current_message));
    assert!(app.modal_is_open());
    assert_eq!(app.discard_ui.intent_id(), Some(current));
    let pending = app
        .sidebar_state
        .pending_discard()
        .expect("actual App confirmation");
    assert_eq!(pending.key, key);
    assert_eq!(pending.request.worktree, root);
    assert_eq!(pending.request.tracked.len(), 2);
    let mut tracked = pending
        .request
        .tracked
        .iter()
        .map(|selection| selection.path.as_path())
        .collect::<Vec<_>>();
    tracked.sort();
    assert_eq!(tracked, vec![Path::new("deleted"), Path::new("modified")]);
    assert!(
        pending
            .request
            .tracked
            .iter()
            .all(|selection| selection.old_path.is_none())
    );
    assert!(
        pending.request.untracked.is_empty(),
        "this control confirms only the tracked area"
    );
    assert!(!pending.request.confirmed);
    let mut renderer = crate::app::tests::headless_renderer();
    let dialog = UserInterface::build(
        app.dialog().expect("actual confirmation view"),
        Size::new(1280.0, 900.0),
        Cache::new(),
        &mut renderer,
    );
    let accessible = dialog.a11y_nodes(mouse::Cursor::Unavailable);
    let labels = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .filter_map(|node| node.node().label())
        .collect::<Vec<_>>();
    assert!(labels.contains(&"Discard changes?"));
    assert!(labels.contains(&"2 tracked paths will be restored from the index"));
    assert_eq!(
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter(|node| {
                node.node().role() == Role::Button
                    && node.node().label() == Some("Discard")
                    && !node.node().is_disabled()
            })
            .count(),
        1
    );
    assert_eq!(
        app.sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .group(crate::repository::ChangeArea::Untracked)
            .unwrap()
            .rows()
            .len(),
        1
    );
    assert!(!app.discard_ui.has_inflight());
    assert!(app.discard_ui.slots.is_empty());
    assert_eq!(
        fs::read(root.join("modified")).unwrap(),
        b"precious modified\n"
    );
    assert!(!root.join("deleted").exists());
    assert_eq!(fs::read(&untracked).unwrap(), b"precious untracked\n");
    let after = fs::metadata(&untracked).unwrap();
    assert_eq!(
        (after.dev(), after.ino()),
        (untracked_identity.dev(), untracked_identity.ino())
    );
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(app.workspace.focused(), Some(chat));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&chat) else {
        panic!("confirmation must retain the same center chat");
    };
    assert_eq!(runtime.target, target);
    assert_eq!(runtime.unsent_text(), original_draft);
}

#[test]
fn discard_app_pointer_confirmation_rebuild_keeps_original_receipt_through_undo() {
    const NAME: &str = "app::discard_ui::actual_tests::discard_app_pointer_confirmation_rebuild_keeps_original_receipt_through_undo";
    if isolated(NAME) {
        return;
    }
    use cosmic::iced::{
        Event, Point, Rectangle, Size,
        advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree},
    };

    // Rebuild between every input, as the native runtime does. Derive the
    // target from the actual laid-out accessible button, not a saved pixel.
    fn click(
        app: &mut Cagents,
        confirmation: bool,
        original: uuid::Uuid,
        key: &ActiveWorktreeKey,
        label: &str,
        size: Size,
        renderer: &cosmic::Renderer,
    ) -> Vec<Message> {
        fn has_id(tree: &Tree, wanted: &widget::Id) -> bool {
            tree.id
                .as_ref()
                .is_some_and(|id| iced_accessibility::IdEq::eq(id, wanted))
                || tree.children.iter().any(|child| has_id(child, wanted))
        }
        let mut tree = Tree::empty();
        let viewport = Rectangle::with_size(size);
        let mut point = None;
        let mut messages = Vec::new();
        let commit_message = app.sidebar_state.commit_message().to_owned();
        for phase in 0..3 {
            let mut element = if confirmation {
                app.dialog().expect("actual confirmation is mounted")
            } else {
                app.context_panel(None, size.width)
            };
            tree.diff(element.as_widget_mut());
            let node = element.as_widget_mut().layout(
                &mut tree,
                renderer,
                &layout::Limits::new(Size::ZERO, size),
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                mouse::Cursor::Unavailable,
            );
            let buttons = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .filter(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some(label)
                })
                .collect::<Vec<_>>();
            assert_eq!(buttons.len(), 1, "one current actual {label} control");
            let bounds = buttons[0].node().bounds().expect("button geometry");
            let current = Point::new(
                ((bounds.x0 + bounds.x1) / 2.0) as f32,
                ((bounds.y0 + bounds.y1) / 2.0) as f32,
            );
            assert!(viewport.contains(current), "{label} is within the viewport");
            let at = *point.get_or_insert(current);
            assert_eq!(current, at, "unchanged source keeps the press target");
            let event = match phase {
                0 => mouse::Event::CursorMoved { position: at },
                1 => mouse::Event::ButtonPressed(mouse::Button::Left),
                _ => mouse::Event::ButtonReleased(mouse::Button::Left),
            };
            let mut emitted = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                mouse::Cursor::Available(at),
                renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut emitted),
                &viewport,
            );
            drop(element);
            for message in emitted {
                if let Message::SidebarSnapshot { source, action } = &message
                    && let (SidebarAction::CommitSubjectEditorAction {
                        key: observed,
                        action: widget::text_editor::Action::ClearSelection,
                    }
                    | SidebarAction::CommitEditorAction {
                        key: observed,
                        action: widget::text_editor::Action::ClearSelection,
                    }) = &**action
                {
                    // The real commit editors clear their selection on an
                    // outside press. Apply that exact native callback before
                    // rebuilding; it is neither an edit nor a discard.
                    assert!(!confirmation, "no commit editor is mounted in the dialog");
                    assert_eq!(phase, 1, "only the outside press clears selection");
                    assert_eq!(*source, original, "phase {phase}: {message:?}");
                    assert_eq!(observed, key, "phase {phase}: {message:?}");
                    let task = app.update(message);
                    assert_no_app_work(task);
                    assert_eq!(app.sidebar_state.commit_message(), commit_message);
                    assert!(!app.modal_is_open());
                    assert_eq!(app.discard_ui.source().unwrap().id, original);
                    continue;
                }
                if let Message::SidebarSnapshot { source, action } = &message
                    && let SidebarAction::ChangeViewport {
                        key: observed,
                        scope,
                        first_row,
                        epoch,
                    } = &**action
                {
                    // Native layout can notify a viewport before a button is
                    // released. Accept only this actual mounted list's callback
                    // and reduce it through App before rebuilding the next frame.
                    assert!(!confirmation, "no change list is mounted in the dialog");
                    assert_eq!(*source, original, "phase {phase}: {message:?}");
                    assert_eq!(observed, key, "phase {phase}: {message:?}");
                    assert_eq!(
                        *scope,
                        sidebar::ChangeTreeScope::Area(crate::repository::ChangeArea::Unstaged),
                        "phase {phase}: {message:?}"
                    );
                    assert_eq!(*first_row, 0, "phase {phase}: {message:?}");
                    assert!(
                        has_id(
                            &tree,
                            &widget::Id::new(format!(
                                "sidebar.changes.scroll.{}.{scope:?}.{epoch}",
                                key.worktree_id
                            ))
                        ),
                        "phase {phase}: callback must name the currently mounted epoch: {message:?}"
                    );
                    let task = app.update(message);
                    assert_no_app_work(task);
                    assert!(!app.modal_is_open());
                    assert_eq!(app.discard_ui.source().unwrap().id, original);
                    continue;
                }
                let target = matches!(&message,
                    Message::SidebarSnapshot { source, action } if *source == original &&
                    if confirmation {
                        matches!(&**action, SidebarAction::ConfirmDiscard { key: observed, .. } if observed == key)
                    } else {
                        matches!(&**action, SidebarAction::ChangeGroup { key: observed,
                            command: sidebar::ChangeGroupCommand::Discard, .. } if observed == key)
                    }
                );
                assert!(
                    target,
                    "phase {phase}: unexpected actual message: {message:?}"
                );
                assert_eq!(phase, 2, "Discard is a release action: {message:?}");
                messages.push(message);
            }
            if phase < 2 {
                assert!(messages.is_empty(), "Discard is a release action");
            }
        }
        messages
    }

    let root = PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap());
    git(&root, &["init", "-q"]);
    fs::write(root.join("changed"), b"index baseline\n").unwrap();
    git(&root, &["add", "--all"]);
    git(&root, &["commit", "-qm", "base"]);
    fs::write(root.join("changed"), b"original precious work\n").unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let key = ActiveWorktreeKey {
        project_id: crate::model::ProjectId::new(),
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.clone(),
    };
    // The actual inspector binds its source through the selected project and
    // worktree, not through the snapshot model alone. Populate the same model
    // identity and synchronize that selection before the real snapshot read.
    app.sidebar_model = sidebar::SidebarModel::ready(
        sidebar::SidebarMode::Expanded,
        vec![sidebar::SidebarProject::new(
            key.project_id,
            crate::repository::RepositoryId::new(),
            "Discard fixture".into(),
            root.clone(),
            vec![sidebar::SidebarWorktree::from_durable(
                key.worktree_id,
                root.clone(),
                None,
            )],
            Vec::new(),
        )],
    )
    .with_source_control(Some(SourceControlModel::loading(key.clone())));
    assert!(app.sidebar_state.sync_active_worktree(
        &app.sidebar_model,
        &sidebar::SidebarFocus::Repository {
            worktree: root.clone(),
        },
    ));
    assert_eq!(app.sidebar_state.active_worktree(), Some(&key));
    let loaded = crate::app::tests::task_app_message(app.source_control_snapshot_task(key.clone()));
    assert!(matches!(
        &loaded,
        Message::SourceControlLoaded { result: Ok(_), .. }
    ));
    drop(app.update(loaded));
    let source = sidebar::bound_source_control(&app.sidebar_model, &app.sidebar_state)
        .expect("the selected original checkout owns the inspector snapshot");
    assert_eq!(source.data_state, sidebar::SidebarDataState::Ready);
    assert_eq!(
        source
            .group(crate::repository::ChangeArea::Unstaged)
            .unwrap()
            .rows()
            .len(),
        1,
        "the real Git snapshot exposes the one changed original file"
    );
    app.context_tab = ContextTab::Changes;
    let original = app.discard_ui.source().unwrap().id;
    let renderer = crate::app::tests::headless_renderer();
    let mut requested = click(
        &mut app,
        false,
        original,
        &key,
        "Discard all Unstaged",
        Size::new(380.0, 900.0),
        &renderer,
    );
    assert_eq!(requested.len(), 1);
    assert!(
        matches!(&requested[0], Message::SidebarSnapshot { source, action }
        if *source == original && matches!(&**action, SidebarAction::ChangeGroup {
            command: sidebar::ChangeGroupCommand::Discard, ..
        }))
    );
    drop(app.update(requested.pop().unwrap()));
    assert!(app.modal_is_open());
    assert_eq!(app.discard_ui.intent_id(), Some(original));
    assert_eq!(
        fs::read(root.join("changed")).unwrap(),
        b"original precious work\n"
    );

    let mut confirmed = click(
        &mut app,
        true,
        original,
        &key,
        "Discard",
        Size::new(1280.0, 900.0),
        &renderer,
    );
    assert_eq!(confirmed.len(), 1);
    assert!(
        matches!(&confirmed[0], Message::SidebarSnapshot { source, action }
        if *source == original && matches!(&**action, SidebarAction::ConfirmDiscard { request, .. }
            if request.confirmed && request.tracked.len() == 1 && request.untracked.is_empty()))
    );
    let task = app.update(confirmed.pop().unwrap());
    assert!(!app.modal_is_open());
    assert!(app.discard_ui.has_inflight());
    let slot = app.discard_ui.slots[0].clone();
    let finished = crate::app::tests::task_app_message(task);
    drop(app.update(finished));
    assert_eq!(fs::read(root.join("changed")).unwrap(), b"index baseline\n");
    assert_eq!(slot.source_id(), original);
    let receipt = slot.0.lock().unwrap().receipt.clone().unwrap();
    assert_eq!(receipt.remaining_paths(), 1);
    // A replaced source must refuse Undo without consuming the saved receipt
    // or changing the replacement. Returning the exact original inode permits
    // retry; copying baseline bytes into a new inode would not authorize it.
    let parked = root.with_extension("original-source");
    fs::rename(&root, &parked).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("changed"), b"replacement must survive\n").unwrap();
    let task = app.update(Message::DiscardUndo {
        id: slot.id(),
        resolve: false,
    });
    let finished = crate::app::tests::task_app_message(task);
    assert!(matches!(
        &finished,
        Message::DiscardFinished {
            report: Some(_),
            ..
        }
    ));
    drop(app.update(finished));
    assert!(!app.discard_ui.has_inflight());
    assert_eq!(receipt.remaining_paths(), 1);
    assert!(receipt.can_undo());
    assert!(app.discard_ui.slot(slot.id()).is_some());
    assert_eq!(
        fs::read(root.join("changed")).unwrap(),
        b"replacement must survive\n"
    );
    assert_eq!(
        fs::read(parked.join("changed")).unwrap(),
        b"index baseline\n"
    );
    let foreign = root.with_extension("preserved-replacement");
    fs::rename(&root, &foreign).unwrap();
    fs::rename(&parked, &root).unwrap();
    let other = ActiveWorktreeKey {
        project_id: crate::model::ProjectId::new(),
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.with_file_name("other"),
    };
    app.discard_ui.select(Some(other.clone()));
    app.sidebar_model.source_control = Some(SourceControlModel::loading(other.clone()));
    let task = app.update(Message::DiscardUndo {
        id: slot.id(),
        resolve: false,
    });
    assert!(app.discard_ui.has_inflight());
    let finished = crate::app::tests::task_app_message(task);
    assert!(matches!(
        &finished,
        Message::DiscardFinished { report: None, .. }
    ));
    drop(app.update(finished));
    assert_eq!(
        app.sidebar_model.source_control.as_ref().unwrap().key,
        other
    );
    assert_eq!(
        fs::read(foreign.join("changed")).unwrap(),
        b"replacement must survive\n"
    );
    assert_eq!(
        fs::read(root.join("changed")).unwrap(),
        b"original precious work\n"
    );
    assert_eq!(receipt.remaining_paths(), 0);
    assert!(app.discard_ui.slot(slot.id()).is_none());
    assert!(app.discard_ui.close_block_reason().is_none());
}

#[test]
fn discard_app_actual_tasks_keep_original_receipt_across_navigation_and_restore_native_trash() {
    const NAME: &str = "app::discard_ui::actual_tests::discard_app_actual_tasks_keep_original_receipt_across_navigation_and_restore_native_trash";
    if isolated(NAME) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap());
    git(&root, &["init", "-q"]);
    fs::write(root.join("modified"), b"base modified\n").unwrap();
    fs::write(root.join("deleted"), b"base deleted\n").unwrap();
    fs::write(root.join("executable"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(root.join("executable"), fs::Permissions::from_mode(0o755)).unwrap();
    git(&root, &["add", "--all"]);
    git(&root, &["commit", "-qm", "base"]);
    fs::write(root.join("modified"), b"precious modified\n").unwrap();
    fs::remove_file(root.join("deleted")).unwrap();
    fs::write(root.join("executable"), b"#!/bin/sh\nexit 7\n").unwrap();
    fs::create_dir(root.join("empty-after-discard")).unwrap();
    let untracked = root.join("empty-after-discard/original file");
    fs::write(&untracked, b"untracked original\n").unwrap();
    fs::set_permissions(&untracked, fs::Permissions::from_mode(0o640)).unwrap();
    let original = fs::metadata(&untracked).unwrap();
    symlink("literal missing target", root.join("original-link")).unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let key = ActiveWorktreeKey {
        project_id: crate::model::ProjectId::new(),
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.clone(),
    };
    app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
    // Poll the ACTUAL snapshot worker, then its actual reducer, so the source
    // used by the later confirmation is the authority that produced the rows.
    let task = app.source_control_snapshot_task(key.clone());
    let loaded = crate::app::tests::task_app_message(task);
    assert!(matches!(
        &loaded,
        Message::SourceControlLoaded { result: Ok(_), .. }
    ));
    drop(app.update(loaded));
    let source = app.discard_ui.source().unwrap().id;
    let request = DiscardRequest {
        worktree: root.clone(),
        tracked: ["modified", "deleted", "executable"]
            .into_iter()
            .map(|name| crate::repository::PathSelection::new(RepoPath::parse(name).unwrap()))
            .collect(),
        untracked: ["empty-after-discard/original file", "original-link"]
            .into_iter()
            .map(|name| RepoPath::parse(name).unwrap())
            .collect(),
        confirmed: false,
    };
    drop(app.update(Message::SidebarSnapshot {
        source,
        action: Box::new(SidebarAction::RequestDiscard {
            key: key.clone(),
            request: request.clone(),
        }),
    }));
    assert!(app.modal_is_open());
    let task = app.update(Message::SidebarSnapshot {
        source,
        action: Box::new(SidebarAction::ConfirmDiscard {
            key: key.clone(),
            request: DiscardRequest {
                confirmed: true,
                ..request
            },
        }),
    });
    assert!(app.discard_ui.has_inflight());
    assert!(app.discard_ui.close_block_reason().is_some());
    let slot = app.discard_ui.slots[0].clone();
    // Navigate before polling the actual mutation worker. The task and already
    // installed receipt slot retain A; no completion may decorate the new B.
    let other = ActiveWorktreeKey {
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.with_file_name("other"),
        ..key
    };
    app.discard_ui.select(Some(other.clone()));
    app.sidebar_model.source_control = Some(SourceControlModel::loading(other.clone()));
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
        action: instance::UiUpdateAction::Prepare,
        response,
    })));
    assert!(receiver.try_recv().unwrap().starts_with("error:"));
    assert!(
        !app.update_prepared,
        "navigation cannot hide A's worker from updater Prepare"
    );
    // Also exercise the actual commit/flush boundaries if a background
    // operation appears after the initial preflight of an update.
    app.update_prepared = true;
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
        action: instance::UiUpdateAction::Commit,
        response,
    })));
    assert!(receiver.try_recv().unwrap().starts_with("error:"));
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    drop(app.update(Message::UiUpdateFlushed {
        generation: app.update_generation,
        committing: true,
        response,
        saved: Err("No settings save in this boundary control".into()),
        result: Ok(()),
    }));
    assert!(receiver.try_recv().unwrap().starts_with("error:"));
    assert!(!app.update_prepared);
    assert!(!app.update_committed);
    assert!(app.exit.is_none());
    let finished = crate::app::tests::task_app_message(task);
    assert!(matches!(&finished, Message::DiscardFinished { .. }));
    assert!(!app.discard_ui.has_inflight());
    let receipt = slot
        .0
        .lock()
        .unwrap()
        .receipt
        .clone()
        .expect("worker installs receipt before completion");
    assert_eq!(receipt.remaining_paths(), 5);
    assert!(!untracked.exists());
    assert!(!root.join("empty-after-discard").exists());
    assert_eq!(fs::read(root.join("modified")).unwrap(), b"base modified\n");
    let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap());
    let trash = fs::read_dir(data.join("Trash/files"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(trash.len(), 2);
    assert!(
        trash
            .iter()
            .any(|path| fs::symlink_metadata(path).is_ok_and(|metadata| (
                metadata.dev(),
                metadata.ino()
            ) == (
                original.dev(),
                original.ino()
            )))
    );
    assert_eq!(fs::read_dir(data.join("Trash/info")).unwrap().count(), 2);
    drop(app.update(finished));
    assert_eq!(
        app.sidebar_model.source_control.as_ref().unwrap().key,
        other
    );
    assert!(app.discard_ui.slot(slot.id()).is_some());
    app.update_prepared = true;
    drop(app.update(Message::DiscardUndo {
        id: slot.id(),
        resolve: false,
    }));
    drop(app.update(Message::DiscardUndo {
        id: slot.id(),
        resolve: true,
    }));
    assert!(
        !app.discard_ui.has_inflight(),
        "prepared update rejects both Undo and Resolve admission"
    );
    assert_eq!(receipt.remaining_paths(), 5);
    assert!(!untracked.exists());
    app.update_prepared = false;
    let undo = app.update(Message::DiscardUndo {
        id: slot.id(),
        resolve: false,
    });
    assert!(app.discard_ui.has_inflight());
    let undone = crate::app::tests::task_app_message(undo);
    drop(app.update(undone));
    assert_eq!(receipt.remaining_paths(), 0);
    assert!(!receipt.live_recovery.needs_resolution());
    assert!(!app.discard_ui.has_inflight());
    assert!(app.discard_ui.slot(slot.id()).is_none());
    assert_eq!(
        fs::read(root.join("modified")).unwrap(),
        b"precious modified\n"
    );
    assert!(!root.join("deleted").exists());
    assert_eq!(
        fs::read(root.join("executable")).unwrap(),
        b"#!/bin/sh\nexit 7\n"
    );
    assert_eq!(
        fs::metadata(root.join("executable")).unwrap().mode() & 0o7777,
        0o755
    );
    assert_eq!(fs::read(&untracked).unwrap(), b"untracked original\n");
    let restored = fs::metadata(&untracked).unwrap();
    assert_eq!(
        (restored.dev(), restored.ino(), restored.mode() & 0o7777),
        (original.dev(), original.ino(), 0o640)
    );
    assert_eq!(
        fs::read_link(root.join("original-link")).unwrap(),
        PathBuf::from("literal missing target")
    );
    assert_eq!(fs::read_dir(data.join("Trash/files")).unwrap().count(), 0);
    assert_eq!(fs::read_dir(data.join("Trash/info")).unwrap().count(), 0);
    assert!(app.discard_ui.close_block_reason().is_none());
}

#[test]
fn discard_app_partial_undo_preserves_newer_file_and_keeps_remaining_receipt() {
    const NAME: &str = "app::discard_ui::actual_tests::discard_app_partial_undo_preserves_newer_file_and_keeps_remaining_receipt";
    if isolated(NAME) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap());
    git(&root, &["init", "-q"]);
    fs::write(root.join("changed"), b"base\n").unwrap();
    git(&root, &["add", "--all"]);
    git(&root, &["commit", "-qm", "base"]);
    fs::write(root.join("changed"), b"discarded precious work\n").unwrap();
    fs::write(root.join("new file"), b"new file original\n").unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let key = ActiveWorktreeKey {
        project_id: crate::model::ProjectId::new(),
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.clone(),
    };
    app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
    let task = app.source_control_snapshot_task(key.clone());
    let loaded = crate::app::tests::task_app_message(task);
    assert!(matches!(
        &loaded,
        Message::SourceControlLoaded { result: Ok(_), .. }
    ));
    drop(app.update(loaded));
    let source = app.discard_ui.source().unwrap().id;
    let request = DiscardRequest {
        worktree: root.clone(),
        tracked: vec![crate::repository::PathSelection::new(
            RepoPath::parse("changed").unwrap(),
        )],
        untracked: vec![RepoPath::parse("new file").unwrap()],
        confirmed: false,
    };
    drop(app.update(Message::SidebarSnapshot {
        source,
        action: Box::new(SidebarAction::RequestDiscard {
            key: key.clone(),
            request: request.clone(),
        }),
    }));
    let task = app.update(Message::SidebarSnapshot {
        source,
        action: Box::new(SidebarAction::ConfirmDiscard {
            key,
            request: DiscardRequest {
                confirmed: true,
                ..request
            },
        }),
    });
    let slot = app.discard_ui.slots[0].clone();
    let finished = crate::app::tests::task_app_message(task);
    drop(app.update(finished));
    let receipt = slot.0.lock().unwrap().receipt.clone().unwrap();
    assert_eq!(receipt.remaining_paths(), 2);
    assert_eq!(fs::read(root.join("changed")).unwrap(), b"base\n");
    assert!(!root.join("new file").exists());
    fs::write(root.join("changed"), b"user's newer work\n").unwrap();
    let task = app.update(Message::DiscardUndo {
        id: slot.id(),
        resolve: false,
    });
    let finished = crate::app::tests::task_app_message(task);
    assert!(
        matches!(&finished, Message::DiscardFinished { report: Some(report), .. } if report.contains("changed"))
    );
    drop(app.update(finished));
    assert_eq!(
        fs::read(root.join("changed")).unwrap(),
        b"user's newer work\n"
    );
    assert_eq!(
        fs::read(root.join("new file")).unwrap(),
        b"new file original\n"
    );
    assert_eq!(receipt.remaining_paths(), 1);
    assert!(receipt.can_undo());
    assert!(app.discard_ui.slot(slot.id()).is_some());
    let task = app.update(Message::DiscardUndo {
        id: slot.id(),
        resolve: false,
    });
    let finished = crate::app::tests::task_app_message(task);
    assert!(
        matches!(&finished, Message::DiscardFinished { report: Some(report), .. } if report.contains("changed"))
    );
    drop(app.update(finished));
    assert_eq!(receipt.remaining_paths(), 1);
    assert_eq!(
        fs::read(root.join("changed")).unwrap(),
        b"user's newer work\n"
    );
    assert_eq!(
        fs::read(root.join("new file")).unwrap(),
        b"new file original\n"
    );
}

#[test]
fn discard_banner_cached_press_cannot_move_to_an_earlier_materialized_slot() {
    const NAME: &str = "app::discard_ui::actual_tests::discard_banner_cached_press_cannot_move_to_an_earlier_materialized_slot";
    if isolated(NAME) {
        return;
    }
    use cosmic::iced::{
        Event, Point, Size,
        advanced::{clipboard, mouse},
        runtime::user_interface::{Cache, UserInterface},
    };
    use iced_accessibility::accesskit::Role;

    struct Frames {
        cache: Cache,
        renderer: cosmic::Renderer,
    }
    impl Frames {
        fn frame(
            &mut self,
            state: &State,
            label: &str,
            count: usize,
            disabled: bool,
            at: Option<Point>,
            event: Option<mouse::Event>,
        ) -> (Point, Vec<Message>) {
            let mut ui = UserInterface::build(
                state.banner(disabled),
                Size::new(1280.0, 900.0),
                std::mem::replace(&mut self.cache, Cache::new()),
                &mut self.renderer,
            );
            let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
            let mut buttons = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter(|node| {
                    node.node().role() == Role::Button && node.node().label() == Some(label)
                })
                .map(|node| {
                    assert_eq!(node.node().is_disabled(), disabled);
                    node.node().bounds().expect("actual banner button geometry")
                })
                .collect::<Vec<_>>();
            assert_eq!(buttons.len(), count);
            buttons.sort_by(|a, b| a.y0.total_cmp(&b.y0));
            let first = buttons[0];
            let at = at.unwrap_or_else(|| {
                Point::new(
                    ((first.x0 + first.x1) / 2.0) as f32,
                    ((first.y0 + first.y1) / 2.0) as f32,
                )
            });
            // After materialization this SAME coordinate must hit the earlier
            // slot's actionable button. An empty release outside all controls
            // would not establish ownership of the native held press.
            assert!(
                (first.x0..first.x1).contains(&f64::from(at.x))
                    && (first.y0..first.y1).contains(&f64::from(at.y))
                    && (0.0..180.0).contains(&at.y),
                "the release remains over the first actual banner action"
            );
            let mut messages = Vec::new();
            let events = event.map(Event::Mouse).into_iter().collect::<Vec<_>>();
            let _ = ui.update(
                &events,
                mouse::Cursor::Available(at),
                &mut self.renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            self.cache = ui.into_cache();
            (at, messages)
        }
    }
    fn assert_action(messages: Vec<Message>, id: uuid::Uuid, failure: bool) {
        assert_eq!(messages.len(), 1, "one exact native action");
        match &messages[0] {
            Message::DiscardCopyFailureDetails { id: actual } if failure => {
                assert_eq!(*actual, id)
            }
            Message::DiscardUndo {
                id: actual,
                resolve: false,
            } if !failure => {
                assert_eq!(*actual, id)
            }
            _ => panic!("the native action changed operation or action kind"),
        }
    }

    let root = PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap());
    git(&root, &["init", "-q"]);
    for path in ["earlier", "later"] {
        fs::write(root.join(path), b"indexed baseline\n").unwrap();
    }
    git(&root, &["add", "--all"]);
    let index = fs::read(root.join(".git/index")).unwrap();
    // Cover both identical native row shapes: a completed Undo receipt and
    // the new unconfirmed-operation diagnostic. Callbacks are inspected only;
    // neither the Undo nor clipboard App action is executed by these frames.
    for failure in [false, true] {
        for path in ["earlier", "later"] {
            fs::write(root.join(path), b"precious original\n").unwrap();
        }
        let source = ReadSource::new(ActiveWorktreeKey {
            project_id: crate::model::ProjectId::new(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: root.clone(),
        });
        source.read_service().unwrap();
        let mut state = State {
            active: Some(source.clone()),
            ..State::default()
        };
        let mut pending = Vec::new();
        for path in ["earlier", "later"] {
            let mut request = DiscardRequest {
                worktree: root.clone(),
                tracked: vec![crate::repository::PathSelection::new(
                    RepoPath::parse(path).unwrap(),
                )],
                untracked: vec![],
                confirmed: false,
            };
            assert!(state.confirm_intent(source.id, &request));
            request.confirmed = true;
            let slot = state.reserve_confirmed(source.id, &request).unwrap();
            pending.push((slot, request));
        }
        let [(earlier, earlier_request), (later, later_request)] = pending.as_slice() else {
            unreachable!()
        };
        if failure {
            later.worker_failed("Discard", "fixture unknown completion");
        } else {
            assert!(
                later
                    .finish_discard(later_request.clone(), vec![root.clone()])
                    .is_none()
            );
        }
        let label = if failure {
            "Copy failure details"
        } else {
            "Undo discard"
        };
        let mut frames = Frames {
            cache: Cache::new(),
            renderer: crate::app::tests::headless_renderer(),
        };
        let press = Some(mouse::Event::ButtonPressed(mouse::Button::Left));
        let release = Some(mouse::Event::ButtonReleased(mouse::Button::Left));
        let (at, messages) = frames.frame(&state, label, 1, false, None, press);
        assert!(messages.is_empty());
        assert!(
            frames
                .frame(&state, label, 1, false, Some(at), None)
                .1
                .is_empty()
        );
        assert_action(
            frames.frame(&state, label, 1, false, Some(at), release).1,
            later.id(),
            failure,
        );

        assert!(
            frames
                .frame(&state, label, 1, false, Some(at), press)
                .1
                .is_empty()
        );
        if failure {
            earlier.worker_failed("Discard", "fixture earlier unknown completion");
        } else {
            assert!(
                earlier
                    .finish_discard(earlier_request.clone(), vec![root.clone()])
                    .is_none()
            );
        }
        assert!(
            frames
                .frame(&state, label, 2, false, Some(at), release)
                .1
                .is_empty()
        );
        assert!(
            frames
                .frame(&state, label, 2, false, Some(at), press)
                .1
                .is_empty()
        );
        assert_action(
            frames.frame(&state, label, 2, false, Some(at), release).1,
            earlier.id(),
            failure,
        );

        // Even the same slot/action must acquire a fresh press after a modal
        // disables it and then returns. Enabled state is part of its owner.
        assert!(
            frames
                .frame(&state, label, 2, false, Some(at), press)
                .1
                .is_empty()
        );
        assert!(
            frames
                .frame(&state, label, 2, true, Some(at), None)
                .1
                .is_empty()
        );
        assert!(
            frames
                .frame(&state, label, 2, false, Some(at), release)
                .1
                .is_empty()
        );
        assert!(
            frames
                .frame(&state, label, 2, false, Some(at), press)
                .1
                .is_empty()
        );
        assert_action(
            frames.frame(&state, label, 2, false, Some(at), release).1,
            earlier.id(),
            failure,
        );
        let expected = if failure {
            b"precious original\n".as_slice()
        } else {
            b"indexed baseline\n".as_slice()
        };
        for path in ["earlier", "later"] {
            assert_eq!(fs::read(root.join(path)).unwrap(), expected);
        }
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    }
}

#[test]
fn discard_app_post_restore_panic_retains_unconfirmed_owner_without_awaiter() {
    const NAME: &str = "app::discard_ui::actual_tests::discard_app_post_restore_panic_retains_unconfirmed_owner_without_awaiter";
    if isolated(NAME) {
        return;
    }
    use crate::process::{ProcessFailure, ProcessOutput};
    use cosmic::iced::{
        Size,
        advanced::mouse,
        futures::FutureExt,
        runtime::user_interface::{Cache, UserInterface},
    };
    struct PanicAfterRestore {
        restored: std::sync::mpsc::SyncSender<()>,
        proceed: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl ProcessExecutor for PanicAfterRestore {
        fn execute(
            &self,
            command: &CommandSpec,
            cancellation: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            let output = SystemProcessExecutor.execute(command, cancellation)?;
            if output.success && command.arguments.iter().any(|arg| arg == "restore") {
                self.restored.send(()).unwrap();
                self.proceed
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap();
                panic!("discard panic after actual Git restore");
            }
            Ok(output)
        }
    }
    let root = PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap());
    git(&root, &["init", "-q"]);
    for dropped_awaiter in [false, true] {
        let path = if dropped_awaiter {
            "detached"
        } else {
            "awaited"
        };
        fs::write(root.join(path), b"indexed baseline\n").unwrap();
        let baseline_mode = fs::metadata(root.join(path)).unwrap().mode() & 0o7777;
        git(&root, &["add", "--", path]);
        let index = fs::read(root.join(".git/index")).unwrap();
        fs::write(root.join(path), b"precious original\n").unwrap();
        fs::set_permissions(root.join(path), fs::Permissions::from_mode(0o640)).unwrap();
        let key = ActiveWorktreeKey {
            project_id: crate::model::ProjectId::new(),
            worktree_id: crate::model::WorktreeId::new(),
            canonical_path: root.clone(),
        };
        let source = ReadSource::new(key.clone());
        source.read_service().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.discard_ui.active = Some(source.clone());
        let mut request = DiscardRequest {
            worktree: root.clone(),
            tracked: vec![crate::repository::PathSelection::new(
                RepoPath::parse(path).unwrap(),
            )],
            untracked: vec![],
            confirmed: false,
        };
        assert!(app.discard_ui.confirm_intent(source.id, &request));
        request.confirmed = true;
        let slot = app
            .discard_ui
            .reserve_confirmed(source.id, &request)
            .unwrap();
        let (restored, observed) = std::sync::mpsc::sync_channel(1);
        let (release, proceed) = std::sync::mpsc::sync_channel(1);
        let service = RepositoryService::new(
            "git",
            Arc::new(PanicAfterRestore {
                restored,
                proceed: Mutex::new(proceed),
            }),
        )
        .with_worktree_authority(source.authority.get().unwrap().clone().unwrap())
        .unwrap();
        let worker_request = request.clone();
        let known = vec![root.clone()];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut worker = Box::pin(slot.run_worker("Discard", move |_| {
            let _ = service.discard_recoverable(&worker_request, &CancellationToken::new(), &known);
            panic!("expected the actual post-restore injection");
        }));
        assert!(
            runtime
                .block_on(async { worker.as_mut().now_or_never() })
                .is_none()
        );
        observed.recv_timeout(Duration::from_secs(15)).unwrap();
        // The real restore completed, but the backend has not returned any
        // RecoverableDiscard. These are effects, not a pre-effect panic fixture.
        assert_eq!(fs::read(root.join(path)).unwrap(), b"indexed baseline\n");
        assert_eq!(
            fs::metadata(root.join(path)).unwrap().mode() & 0o7777,
            baseline_mode
        );
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
        if dropped_awaiter {
            drop(worker);
            release.send(()).unwrap();
        } else {
            release.send(()).unwrap();
            let report = runtime.block_on(worker).unwrap();
            assert!(report.contains("panicked"));
        }
        runtime.shutdown_timeout(Duration::from_secs(15));
        assert!(!app.discard_ui.has_inflight());
        {
            let entry = slot.0.lock().unwrap();
            assert!(entry.receipt.is_none());
            assert_eq!(entry.source.id, source.id);
            let failure = entry.unconfirmed.as_ref().unwrap();
            assert_eq!(failure.request.as_ref(), Some(&request));
            assert_eq!(Some(&failure.error), entry.error.as_ref());
            assert!(failure.error.contains("completion is unconfirmed"));
        }
        app.discard_ui.select(None);
        app.discard_ui.prune();
        assert!(app.discard_ui.slot(slot.id()).is_some());
        assert!(
            app.discard_ui
                .close_block_reason()
                .unwrap()
                .contains("unknown")
        );
        assert!(app.window_update_blocker().unwrap().contains("unknown"));
        assert!(app.discard_ui.begin_undo(slot.id(), false).is_none());
        let details = app.discard_ui.failure_details(slot.id()).unwrap();
        assert!(details.contains("No complete Undo receipt was returned"));
        assert!(details.contains(&root.display().to_string()));
        assert!(details.contains(path));
        let mut renderer = crate::app::tests::headless_renderer();
        let ui = UserInterface::build(
            app.discard_ui.banner(false),
            Size::new(1280.0, 900.0),
            Cache::default(),
            &mut renderer,
        );
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        assert!(
            nodes
                .root()
                .iter()
                .chain(nodes.children())
                .any(|node| node.node().label() == Some("Copy failure details")
                    && !node.node().is_disabled())
        );
        assert_eq!(fs::read(root.join(path)).unwrap(), b"indexed baseline\n");
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    }
}

#[test]
fn discard_app_undo_worker_panic_keeps_receipt_for_retry() {
    const NAME: &str =
        "app::discard_ui::actual_tests::discard_app_undo_worker_panic_keeps_receipt_for_retry";
    if isolated(NAME) {
        return;
    }
    let root = PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap());
    git(&root, &["init", "-q"]);
    fs::write(root.join("file"), b"index baseline\n").unwrap();
    git(&root, &["add", "--all"]);
    fs::write(root.join("file"), b"precious original\n").unwrap();
    fs::set_permissions(root.join("file"), fs::Permissions::from_mode(0o640)).unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let key = ActiveWorktreeKey {
        project_id: crate::model::ProjectId::new(),
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.clone(),
    };
    app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
    let loaded = crate::app::tests::task_app_message(app.source_control_snapshot_task(key.clone()));
    assert!(matches!(
        &loaded,
        Message::SourceControlLoaded { result: Ok(_), .. }
    ));
    drop(app.update(loaded));
    let source = app.discard_ui.source().unwrap().clone();
    let discarded = source
        .mutation_service()
        .unwrap()
        .discard_recoverable(
            &DiscardRequest {
                worktree: root.clone(),
                tracked: vec![crate::repository::PathSelection::new(
                    RepoPath::parse("file").unwrap(),
                )],
                untracked: vec![],
                confirmed: true,
            },
            &CancellationToken::new(),
            &[root.clone()],
        )
        .unwrap();
    assert!(matches!(
        discarded.outcome,
        crate::repository::MutationOutcome::Applied { .. }
    ));
    let receipt = discarded.recovery;
    let index = fs::read(root.join(".git/index")).unwrap();
    let slot = Slot(Arc::new(Mutex::new(Entry {
        id: uuid::Uuid::new_v4(),
        source: source.clone(),
        original_request: None,
        receipt: Some(receipt.clone()),
        unconfirmed: None,
        running: false,
        error: None,
    })));
    app.discard_ui.slots.push(slot.clone());
    assert!(app.discard_ui.begin_undo(slot.id(), false).is_some());
    let report = super::tests::worker_panic(&slot, "Undo");
    assert!(report.as_ref().unwrap().contains("Undo worker failed"));
    assert!(report.as_ref().unwrap().contains("panicked"));
    assert!(!app.discard_ui.has_inflight());
    {
        let entry = slot.0.lock().unwrap();
        assert_eq!(entry.error, report);
        assert_eq!(entry.receipt.as_ref(), Some(&receipt));
        assert_eq!(entry.source.id, source.id);
    }
    assert_eq!(receipt.remaining_paths(), 1);
    assert!(receipt.can_undo());
    assert!(!receipt.live_recovery.needs_resolution());
    assert!(app.discard_ui.close_block_reason().is_none());
    assert_eq!(fs::read(root.join("file")).unwrap(), b"index baseline\n");
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);

    let task = app.update(Message::DiscardUndo {
        id: slot.id(),
        resolve: false,
    });
    assert!(app.discard_ui.has_inflight());
    let finished = crate::app::tests::task_app_message(task);
    assert!(matches!(
        &finished,
        Message::DiscardFinished { report: None, .. }
    ));
    drop(app.update(finished));
    assert!(!app.discard_ui.has_inflight());
    assert!(app.discard_ui.slot(slot.id()).is_none());
    assert!(app.discard_ui.close_block_reason().is_none());
    assert_eq!(receipt.remaining_paths(), 0);
    assert_eq!(fs::read(root.join("file")).unwrap(), b"precious original\n");
    assert_eq!(
        fs::metadata(root.join("file")).unwrap().mode() & 0o7777,
        0o640
    );
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
}

#[test]
fn discard_app_recover_original_uses_nonce_and_original_slot_across_navigation() {
    const NAME: &str = "app::discard_ui::actual_tests::discard_app_recover_original_uses_nonce_and_original_slot_across_navigation";
    if isolated(NAME) {
        return;
    }
    use crate::process::{ProcessFailure, ProcessOutput};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct MoveParent {
        root: PathBuf,
        parked: PathBuf,
        armed: AtomicBool,
    }
    impl ProcessExecutor for MoveParent {
        fn execute(
            &self,
            command: &CommandSpec,
            cancel: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            if command.arguments.iter().any(|arg| arg == "cat-file")
                && self.armed.swap(false, Ordering::SeqCst)
            {
                fs::rename(self.root.join("nested"), &self.parked).unwrap();
                fs::create_dir(self.root.join("nested")).unwrap();
                fs::write(self.root.join("nested/file"), b"foreign preserved\n").unwrap();
            }
            SystemProcessExecutor.execute(command, cancel)
        }
    }
    let root = PathBuf::from(std::env::var_os("CAGENTS_DISCARD_ROOT").unwrap());
    git(&root, &["init", "-q"]);
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/file"), b"index baseline\n").unwrap();
    git(&root, &["add", "--all"]);
    git(&root, &["commit", "-qm", "base"]);
    fs::write(root.join("nested/file"), b"precious original\n").unwrap();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let key = ActiveWorktreeKey {
        project_id: crate::model::ProjectId::new(),
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.clone(),
    };
    app.sidebar_model.source_control = Some(SourceControlModel::loading(key.clone()));
    let task = app.source_control_snapshot_task(key.clone());
    let loaded = crate::app::tests::task_app_message(task);
    assert!(matches!(
        &loaded,
        Message::SourceControlLoaded { result: Ok(_), .. }
    ));
    drop(app.update(loaded));
    let source = app.discard_ui.source().unwrap().clone();
    let parked = root.with_extension("original-parent");
    let executor = Arc::new(MoveParent {
        root: root.clone(),
        parked: parked.clone(),
        armed: AtomicBool::new(false),
    });
    let service = RepositoryService::new("git", executor.clone())
        .with_worktree_authority(source.authority.get().unwrap().clone().unwrap())
        .unwrap();
    let request = DiscardRequest {
        worktree: root.clone(),
        tracked: vec![crate::repository::PathSelection::new(
            RepoPath::parse("nested/file").unwrap(),
        )],
        untracked: vec![],
        confirmed: true,
    };
    let discarded = service
        .discard_recoverable(&request, &CancellationToken::new(), &[root.clone()])
        .unwrap();
    assert!(matches!(
        discarded.outcome,
        crate::repository::MutationOutcome::Applied { .. }
    ));
    executor.armed.store(true, Ordering::SeqCst);
    let outcome = service
        .undo_discard(&discarded.recovery, &CancellationToken::new())
        .unwrap();
    assert!(matches!(
        outcome,
        crate::repository::MutationOutcome::NotApplied { .. }
    ));
    let receipt = discarded.recovery;
    let choice = receipt.live_recovery.recoverable_originals().pop().unwrap();
    let payload = fs::metadata(&choice.retained_path).unwrap();
    // Install the genuine backend failed-Undo receipt as the App fixture's
    // original slot. All recovery actions below use the actual App reducer,
    // mutation envelope, worker and completion; no synthetic recovery plan.
    let slot = Slot(Arc::new(Mutex::new(Entry {
        id: uuid::Uuid::new_v4(),
        source,
        original_request: None,
        receipt: Some(receipt.clone()),
        unconfirmed: None,
        running: false,
        error: Some("Original parent changed".into()),
    })));
    app.discard_ui.slots.push(slot.clone());
    assert!(app.discard_ui.close_block_reason().is_some());
    assert!(
        app.discard_ui
            .begin_recover_original(slot.id(), choice.nonce)
            .is_some()
    );
    let report = super::tests::worker_panic(&slot, "Undo");
    assert!(report.as_ref().unwrap().contains("Undo worker failed"));
    assert!(!app.discard_ui.has_inflight());
    assert_eq!(slot.0.lock().unwrap().error, report);
    assert_eq!(slot.0.lock().unwrap().receipt.as_ref(), Some(&receipt));
    assert_eq!(
        receipt.live_recovery.recoverable_originals()[0].nonce,
        choice.nonce
    );
    assert!(receipt.live_recovery.needs_resolution());
    assert_eq!(
        app.discard_ui.close_block_reason(),
        Some("Resolve the pending discard recovery before closing.")
    );
    drop(app.update(Message::DiscardRecoverOriginal {
        id: slot.id(),
        nonce: uuid::Uuid::new_v4(),
    }));
    assert!(!app.discard_ui.has_inflight());
    assert!(!app.source_control_mutations.contains_key(&root));
    assert!(choice.retained_path.exists());
    let other = ActiveWorktreeKey {
        worktree_id: crate::model::WorktreeId::new(),
        canonical_path: root.with_file_name("other"),
        ..key
    };
    app.discard_ui.select(Some(other.clone()));
    app.sidebar_model.source_control = Some(SourceControlModel::loading(other.clone()));
    // Genuine manual preparation preserves both foreign data and baseline.
    fs::rename(root.join("nested"), root.join("foreign-preserved")).unwrap();
    fs::rename(&parked, root.join("nested")).unwrap();
    fs::rename(
        root.join("nested/file"),
        root.join("nested/baseline-preserved"),
    )
    .unwrap();
    let task = app.update(Message::DiscardRecoverOriginal {
        id: slot.id(),
        nonce: choice.nonce,
    });
    assert!(app.discard_ui.has_inflight());
    assert!(app.source_control_mutations.contains_key(&root));
    assert!(app.discard_ui.close_block_reason().is_some());
    let (response, receiver) = std::sync::mpsc::sync_channel(1);
    drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
        action: instance::UiUpdateAction::Prepare,
        response,
    })));
    assert!(receiver.try_recv().unwrap().starts_with("error:"));
    let finished = crate::app::tests::task_app_message(task);
    assert!(matches!(
        &finished,
        Message::DiscardFinished { report: None, .. }
    ));
    drop(app.update(finished));
    assert!(!app.discard_ui.has_inflight());
    assert!(!app.source_control_mutations.contains_key(&root));
    assert_eq!(
        app.sidebar_model.source_control.as_ref().unwrap().key,
        other
    );
    assert!(app.discard_ui.close_block_reason().is_none());
    assert_eq!(receipt.remaining_paths(), 0);
    let recovered = fs::metadata(root.join("nested/file")).unwrap();
    assert_eq!(
        (recovered.dev(), recovered.ino()),
        (payload.dev(), payload.ino())
    );
    assert_eq!(
        fs::read(root.join("nested/file")).unwrap(),
        b"precious original\n"
    );
    assert_eq!(
        fs::read(root.join("nested/baseline-preserved")).unwrap(),
        b"index baseline\n"
    );
    assert_eq!(
        fs::read(root.join("foreign-preserved/file")).unwrap(),
        b"foreign preserved\n"
    );
    drop(app.update(Message::DiscardRecoverOriginal {
        id: slot.id(),
        nonce: choice.nonce,
    }));
    assert!(
        !app.discard_ui.has_inflight(),
        "a stale rendered action cannot rearm the old slot"
    );
}
