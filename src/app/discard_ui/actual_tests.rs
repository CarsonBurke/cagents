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
                    assert!(
                        iced::runtime::task::into_stream(task).is_none(),
                        "viewport bookkeeping must not discard asynchronous App work"
                    );
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
        receipt: Some(receipt.clone()),
        running: false,
        error: Some("Original parent changed".into()),
    })));
    app.discard_ui.slots.push(slot.clone());
    assert!(app.discard_ui.close_block_reason().is_some());
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
