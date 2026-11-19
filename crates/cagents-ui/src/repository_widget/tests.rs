use super::retained_save_tests::Checkout;
/// Only Save-owning tests use this physical read fixture. The returned
/// Checkout owns the original checkout and its recovery parent throughout every lazy Task lifetime.
fn save_fixture(
    card: CardId,
    name: &str,
    text: &str,
) -> (Checkout, RepositoryRuntime, AsyncIdentity) {
    save_fixture_with_source(card, name, name, text)
}

fn save_fixture_with_source(
    card: CardId,
    name: &str,
    source: &str,
    text: &str,
) -> (Checkout, RepositoryRuntime, AsyncIdentity) {
    let directory = Checkout::new();
    std::fs::write(directory.path().join(source), text).unwrap();
    if name != source {
        std::os::unix::fs::symlink(source, directory.path().join(name)).unwrap();
    }
    let mut runtime = RepositoryRuntime::new(RepositoryView {
        project: ProjectId::new(),
        repository: directory.path().to_owned(),
        worktree: directory.path().to_owned(),
        selected_file: None,
    });
    runtime.files = FileListState::Ready(vec![RepositoryFile {
        path: name.into(),
        unstaged: true,
        status: Some(" M".into()),
        ..RepositoryFile::default()
    }]);
    drop(runtime.open_file(card, name.into()));
    let identity = runtime.current_identity(card, name.into());
    let read = crate::editor::read_file_at(
        directory.path(),
        Path::new(name),
        crate::editor::PdfTone::Original,
        None,
    )
    .unwrap();
    drop(runtime.update(RepositoryAction::FileLoadedAt {
        identity: identity.clone(),
        result: Ok(read),
    }));
    assert_eq!(runtime.editor_text().as_deref(), Some(text));
    assert!(runtime.configure_save_recovery(Default::default(), vec![directory.path().to_owned()]));
    (directory, runtime, identity)
}

fn save_warning_fixture(card: CardId) -> (Checkout, RepositoryRuntime) {
    let (directory, mut runtime, identity) = save_fixture(card, "notes.txt", "A\n");
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!()
    };
    document.revision = 2;
    document.active_save = Some(SaveTicket {
        id: 1,
        identity: identity.clone(),
        revision: 1,
        history_revision: u64::MAX,
        snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("B\n").text_snapshot(),
        snapshot: "B\n".into(),
    });
    drop(runtime.update(RepositoryAction::SaveFinished {
        identity,
        ticket: 1,
        revision: 1,
        saved_text: "B\n".into(),
        result: Ok(SaveOutcome::Saved {
            content_hash: blake3::hash(b"B\n"),
            durability: SaveDurability::Unconfirmed {
                error: "injected parent sync failure".into(),
            },
        }),
    }));
    (directory, runtime)
}

async fn finish_navigation_task(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
    drop(super::retained_save_tests::finish(runtime, task).await);
}

#[tokio::test]
async fn folder_editor_navigation_survives_saved_file_and_sqlite_restart() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::folder_editor_navigation_survives_saved_file_and_sqlite_restart",
    ) {
        return;
    }
    use crate::model::{CardKind, CardState, WorkspaceState};
    use crate::persistence::{SnapshotBase, StateStore};
    let directory = Checkout::new();
    let root = directory.path().canonicalize().unwrap();
    std::fs::write(root.join("notes.txt"), "first\nsecond\nthird\n").unwrap();
    let card = CardId::new();
    let view = RepositoryView {
        project: ProjectId::new(),
        repository: root.clone(),
        worktree: root.clone(),
        selected_file: None,
    };
    let mut runtime = RepositoryRuntime::new_directory(view.clone());
    let task = runtime.update(RepositoryAction::OpenFile {
        card,
        path: "notes.txt".into(),
    });
    finish_navigation_task(&mut runtime, task).await;
    let task = runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
    });
    drop(task);
    assert!(runtime.configure_save_recovery(Default::default(), vec![root.clone()]));
    let task = runtime.request_save(card, false);
    finish_navigation_task(&mut runtime, task).await;
    assert!(!runtime.is_dirty());
    let DocumentState::Editable(document) = &mut runtime.document else {
        panic!("editable file");
    };
    document.content.set_cursor(text_editor::Cursor {
        position: text_editor::Position { line: 2, column: 3 },
        selection: Some(text_editor::Position { line: 1, column: 2 }),
    });
    document
        .content
        .restore_scroll_position(text_editor::ScrollPosition {
            line: 1,
            vertical: 2.5,
            horizontal: 7.0,
        });
    let navigation = runtime.navigation().unwrap();
    assert!(
        runtime.descriptor.selected_file.is_none(),
        "folder mode remains a browser"
    );
    let mut workspace = WorkspaceState::default();
    workspace.cards.push(CardState {
        id: card,
        generation: 0,
        title: "Folder".into(),
        width_fraction: 1.0,
        existence: crate::model::Existence::Open,
        kind: CardKind::Repository(view.clone()),
    });
    workspace
        .repository_navigation
        .insert(card, navigation.clone());
    let store = StateStore::open(
        root.join("navigation.sqlite3"),
        crate::model::BootId::new("editor-navigation-test"),
    )
    .unwrap();
    store
        .save_workspace(
            &workspace,
            SnapshotBase {
                workspace: 0,
                cards: &std::collections::BTreeMap::new(),
            },
        )
        .unwrap();
    let restored = store.load_workspace(workspace.id, None).unwrap().unwrap();
    assert_eq!(restored.repository_navigation[&card], navigation);
    let mut reopened = RepositoryRuntime::new_directory(view);
    reopened.restore_navigation(&restored.repository_navigation[&card]);
    let task = reopened.initial_load(card);
    assert_eq!(
        reopened.navigation(),
        Some(navigation.clone()),
        "a snapshot during hydration retains the pending caret"
    );
    finish_navigation_task(&mut reopened, task).await;
    assert_eq!(reopened.selected_path(), Some(Path::new("notes.txt")));
    assert_eq!(reopened.navigation(), Some(navigation));
    let DocumentState::Editable(document) = &reopened.document else {
        panic!("restored editor");
    };
    assert_eq!(
        document.content.text(),
        std::fs::read_to_string(root.join("notes.txt")).unwrap()
    );
    assert!(!reopened.is_dirty());
    assert_eq!(
        reopened.take_restored_editor_focus(card),
        Some(editor_focus_id(card))
    );
    assert!(reopened.take_restored_editor_focus(card).is_none());
}

#[tokio::test]
async fn restored_repository_navigation_installs_initial_git_status_without_losing_caret() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    run_git(root, ["init", "-q"]);
    std::fs::write(root.join("notes.txt"), "before\n").unwrap();
    run_git(root, ["add", "notes.txt"]);
    run_git(
        root,
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    std::fs::write(root.join("notes.txt"), "after\n").unwrap();
    let view = RepositoryView {
        project: ProjectId::new(),
        repository: root.join(".git"),
        worktree: root.into(),
        selected_file: None,
    };
    let navigation = crate::model::RepositoryNavigation {
        project: view.project,
        worktree: view.worktree.clone(),
        selected_file: Some("notes.txt".into()),
        editor: Some(crate::model::EditorNavigation {
            caret: crate::model::EditorPosition { line: 0, column: 3 },
            selection: None,
            scroll_line: 0,
            scroll_vertical: 0.0,
            scroll_horizontal: 0.0,
        }),
    };
    let mut runtime = RepositoryRuntime::new(view);
    runtime.restore_navigation(&navigation);
    let task = runtime.initial_load(CardId::new());
    finish_navigation_task(&mut runtime, task).await;
    assert!(runtime.selected.as_ref().unwrap().unstaged);
    assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
    assert_eq!(
        runtime.navigation().unwrap().editor.unwrap().caret,
        navigation.editor.unwrap().caret
    );
}

#[tokio::test]
async fn dedicated_file_navigation_restore_finishes_loading_without_a_file_list() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "saved").unwrap();
    let view = RepositoryView {
        project: ProjectId::new(),
        repository: directory.path().into(),
        worktree: directory.path().into(),
        selected_file: Some("notes.txt".into()),
    };
    let navigation = crate::model::RepositoryNavigation {
        project: view.project,
        worktree: view.worktree.clone(),
        selected_file: view.selected_file.clone(),
        editor: None,
    };
    let mut runtime = RepositoryRuntime::new(view);
    runtime.restore_navigation(&navigation);
    let task = runtime.initial_load(CardId::new());
    finish_navigation_task(&mut runtime, task).await;
    assert!(matches!(runtime.files, FileListState::Ready(_)));
    assert!(matches!(runtime.document, DocumentState::Editable(_)));
    assert_eq!(runtime.selected_path(), Some(Path::new("notes.txt")));
}

#[test]
fn restored_editor_navigation_rejects_other_recipes_and_escaping_paths() {
    let card = CardId::new();
    let (runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "saved");
    let navigation = runtime.navigation().unwrap();
    for variant in 0..4 {
        let mut changed = navigation.clone();
        match variant {
            0 => changed.project = ProjectId::new(),
            1 => changed.worktree = "/other".into(),
            2 => changed.selected_file = Some("../outside".into()),
            _ => changed.selected_file = Some("/outside".into()),
        }
        let mut reopened = RepositoryRuntime::new(runtime.descriptor.clone());
        reopened.restore_navigation(&changed);
        assert!(reopened.restored_navigation.is_none());
    }
    let mut dedicated = runtime.descriptor.clone();
    dedicated.selected_file = Some("other.txt".into());
    let mut reopened = RepositoryRuntime::new(dedicated);
    reopened.restore_navigation(&navigation);
    assert!(reopened.restored_navigation.is_none());
}

#[tokio::test]
async fn restored_editor_caret_clamps_to_changed_file_and_never_applies_to_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::write(root.join("notes.txt"), "é").unwrap();
    let card = CardId::new();
    let view = RepositoryView {
        project: ProjectId::new(),
        repository: root.clone(),
        worktree: root.clone(),
        selected_file: None,
    };
    let navigation = crate::model::RepositoryNavigation {
        project: view.project,
        worktree: root,
        selected_file: Some("notes.txt".into()),
        editor: Some(crate::model::EditorNavigation {
            caret: crate::model::EditorPosition {
                line: 99,
                column: 99,
            },
            selection: None,
            scroll_line: 0,
            scroll_vertical: 0.0,
            scroll_horizontal: 0.0,
        }),
    };
    let mut runtime = RepositoryRuntime::new_directory(view.clone());
    runtime.restore_navigation(&navigation);
    let task = runtime.initial_load(card);
    finish_navigation_task(&mut runtime, task).await;
    assert_eq!(
        runtime.navigation().unwrap().editor.unwrap().caret,
        crate::model::EditorPosition { line: 0, column: 2 }
    );
    let mut runtime = RepositoryRuntime::new_directory(view);
    runtime.restore_navigation(&navigation);
    let stale = runtime.initial_load(card);
    let replacement = runtime.open_file_entry(
        card,
        RepositoryFile {
            path: "missing.txt".into(),
            ..RepositoryFile::default()
        },
    );
    finish_navigation_task(&mut runtime, stale).await;
    finish_navigation_task(&mut runtime, replacement).await;
    assert_eq!(runtime.selected_path(), Some(Path::new("missing.txt")));
    assert!(runtime.pending_editor_navigation.is_none());
}

#[test]
fn shell_toolbar_actions_stay_in_the_header_when_find_is_open() {
    use cosmic::iced::Size;
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};

    #[derive(Clone)]
    enum ShellMessage {
        Preview,
        Repository,
    }
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::dirty_fixture(card);
    runtime.find_open = true;
    let renderer = headless_renderer();
    let mut toolbar = selected_file_toolbar_mapped_with(
        card,
        &runtime,
        Some(
            icon_button(Icon::Browser, "Shell preview")
                .on_press(ShellMessage::Preview)
                .into(),
        ),
        |_| ShellMessage::Repository,
    )
    .unwrap();
    let mut tree = Tree::new(toolbar.as_widget());
    let node = toolbar.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(600.0, 160.0)),
    );
    let nodes =
        toolbar
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let preview = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| node.node().label() == Some("Shell preview"))
        .expect("shell actions retain their accessible name");
    let bounds = preview.node().bounds().unwrap();
    assert!(bounds.y1 <= f64::from(crate::design::HEADER_CONTENT_HEIGHT));
    assert!(node.children()[1].bounds().y >= crate::design::HEADER_CONTENT_HEIGHT);
}

#[test]
fn retargeted_file_panes_reject_old_reads_even_after_returning_to_the_same_path() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "old contents").unwrap();
    let card = crate::model::CardId::new();
    let view = RepositoryView {
        project: crate::model::ProjectId::new(),
        repository: directory.path().to_path_buf(),
        worktree: directory.path().to_path_buf(),
        selected_file: Some("a.txt".into()),
    };
    let mut runtime = RepositoryRuntime::new(view.clone());
    drop(runtime.initial_load(card));
    let old = RepositoryAction::FileLoaded {
        identity: runtime.current_identity(card, "a.txt".into()),
        result: crate::editor::load_file(
            directory.path(),
            Path::new("a.txt"),
            crate::editor::PdfTone::Original,
        ),
    };
    let mut other = view.clone();
    other.selected_file = Some("b.txt".into());
    drop(runtime.retarget_file(card, other));
    std::fs::write(directory.path().join("a.txt"), "new contents").unwrap();
    drop(runtime.retarget_file(card, view));
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity: runtime.current_identity(card, "a.txt".into()),
        result: crate::editor::load_file(
            directory.path(),
            Path::new("a.txt"),
            crate::editor::PdfTone::Original,
        ),
    }));
    assert_eq!(runtime.editor_text().as_deref(), Some("new contents"));
    drop(runtime.update(old));
    assert_eq!(runtime.editor_text().as_deref(), Some("new contents"));
    assert!(!runtime.inspector_context().can_back);
}

#[tokio::test]
async fn unavailable_explicit_file_protection_replaces_pixels_and_fences_old_reads() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::unavailable_explicit_file_protection_replaces_pixels_and_fences_old_reads",
    ) {
        return;
    }
    // Exercise production protection, which otherwise deliberately does
    // nothing in an uninitialized UI unit-test process.
    crate::worktree_cleanup::prepare_standalone_editor_protection();
    assert!(crate::device::selected().is_none());
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    image::RgbaImage::from_pixel(2, 2, image::Rgba([217, 0, 217, 255]))
        .save(root.join("old.png"))
        .unwrap();
    let card = CardId::new();
    let view = RepositoryView {
        project: ProjectId::new(),
        repository: root.clone(),
        worktree: root.clone(),
        selected_file: Some("old.png".into()),
    };
    let mut runtime = RepositoryRuntime::new_directory(view.clone());
    let task = runtime.initial_load(card);
    finish_navigation_task(&mut runtime, task).await;
    assert!(matches!(runtime.document, DocumentState::Image { .. }));
    assert!(runtime.original_read_attested);
    assert_eq!(
        runtime.worktree_protection.as_ref().unwrap().directory(),
        root
    );
    let old_watch = runtime.document_watch.as_ref().unwrap().id();
    let old_identity = runtime.current_identity(card, "old.png".into());
    let old_read = crate::editor::read_file_at(
        &root,
        Path::new("old.png"),
        crate::editor::PdfTone::Original,
        None,
    )
    .unwrap();
    let old_listing = AsyncIdentity {
        card,
        worktree: root.clone(),
        path: ".".into(),
        generation: runtime.file_list_generation,
    };
    assert!(runtime.accepts_selected(&old_identity));
    assert!(runtime.accepts_file_list(&old_listing));
    let missing = root.join("retired-history");
    assert_eq!(
        crate::worktree_cleanup::protect(&missing)
            .expect_err("the real guard must fail")
            .kind(),
        std::io::ErrorKind::NotFound
    );
    let target = RepositoryView {
        repository: missing.clone(),
        worktree: missing.clone(),
        selected_file: Some("image.png".into()),
        ..view.clone()
    };
    let task = runtime.retarget_file(card, target.clone());
    assert!(iced::runtime::task::into_stream(task).is_none());
    assert!(runtime.document_watch.is_none());
    let departed_wake = runtime.update(RepositoryAction::DocumentChanged {
        card,
        watch: old_watch,
    });
    // An empty batched Task can still expose an empty stream. Judge the
    // preserved document/read state rather than that container detail.
    assert!(!runtime.document_reading);
    assert!(!runtime.document_reload_pending);
    finish_navigation_task(&mut runtime, departed_wake).await;
    assert_eq!(runtime.descriptor, target);
    assert_eq!(runtime.selected_path(), Some(Path::new("image.png")));
    let DocumentState::Failed { path, message } = &runtime.document else {
        panic!("a refused file must replace the old image preview")
    };
    assert_eq!(*path, missing.join("image.png"));
    assert!(message.contains(&path.display().to_string()));
    assert!(message.contains("on this device"));
    assert!(
        message.contains(&std::io::Error::from(std::io::ErrorKind::NotFound).to_string())
            || message.contains("os error 2")
    );
    let failure = message.clone();
    // Failure retains the previous directory lease until the pane can
    // acquire a replacement; it does not pretend the new lease succeeded.
    assert_eq!(
        runtime.worktree_protection.as_ref().unwrap().directory(),
        root
    );
    assert!(!runtime.original_read_attested);
    assert!(runtime.original_editor_root.is_none());
    assert!(!runtime.accepts_selected(&old_identity));
    assert!(!runtime.accepts_file_list(&old_listing));
    drop(runtime.update(RepositoryAction::FileLoadedAt {
        identity: old_identity,
        result: Ok(old_read),
    }));
    assert_eq!(runtime.inspector_context().document_error, Some(failure));
    assert!(matches!(runtime.document, DocumentState::Failed { .. }));

    // Both a newly opened explicit pane and the folder-browser route
    // present the same concrete failure before scheduling a payload read.
    let mut fresh = RepositoryRuntime::new_directory(target.clone());
    assert!(iced::runtime::task::into_stream(fresh.initial_load(card)).is_none());
    assert!(matches!(&fresh.document, DocumentState::Failed { path, .. }
        if *path == missing.join("image.png")));
    let mut folder = RepositoryRuntime::new_directory(RepositoryView {
        selected_file: None,
        ..target.clone()
    });
    assert!(iced::runtime::task::into_stream(folder.open_file(card, "image.png".into())).is_none());
    assert!(
        matches!(&folder.document, DocumentState::Failed { path, .. }
        if *path == missing.join("image.png"))
    );

    // A normal retry acquires the actual new directory and reads its own
    // image, rather than resurrecting the previous preview or root.
    std::fs::create_dir(&missing).unwrap();
    image::RgbaImage::from_pixel(3, 1, image::Rgba([0, 90, 0, 255]))
        .save(missing.join("image.png"))
        .unwrap();
    let task = runtime.initial_load(card);
    finish_navigation_task(&mut runtime, task).await;
    assert!(matches!(
        runtime.document,
        DocumentState::Image {
            width: 3,
            height: 1,
            ..
        }
    ));
    assert_eq!(
        runtime.worktree_protection.as_ref().unwrap().directory(),
        missing
    );
    assert!(runtime.original_read_attested);

    // The same error context applies when the directory lease succeeds
    // but the requested file itself is absent.
    let absent = RepositoryView {
        selected_file: Some("absent.png".into()),
        ..target
    };
    let task = runtime.retarget_file(card, absent);
    finish_navigation_task(&mut runtime, task).await;
    assert!(
        matches!(&runtime.document, DocumentState::Failed { path, message }
        if *path == missing.join("absent.png") && message.contains("on this device")
            && message.contains("os error 2"))
    );

    // A failed new target cannot relax the existing unsaved-buffer fence.
    std::fs::write(root.join("notes.txt"), "unsaved baseline").unwrap();
    let text_view = RepositoryView {
        selected_file: Some("notes.txt".into()),
        ..view
    };
    let task = runtime.retarget_file(card, text_view.clone());
    finish_navigation_task(&mut runtime, task).await;
    assert!(matches!(runtime.document, DocumentState::Editable(_)));
    assert_eq!(runtime.selected_path(), Some(Path::new("notes.txt")));
    assert_eq!(runtime.editor_text().as_deref(), Some("unsaved baseline"));
    assert!(runtime.editable_surface_visible());
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
    }));
    assert!(runtime.is_dirty());
    let text = runtime.editor_text();
    let task = runtime.retarget_file(
        card,
        RepositoryView {
            worktree: root.join("another-missing-root"),
            selected_file: Some("image.png".into()),
            ..text_view.clone()
        },
    );
    assert!(iced::runtime::task::into_stream(task).is_none());
    assert_eq!(runtime.descriptor, text_view);
    assert_eq!(runtime.editor_text(), text);
    assert!(runtime.is_dirty());
}
use std::collections::BTreeSet;

use crate::lsp::{LspRange, LspSnippet, LspSnippetStop};
use crate::model::ProjectId;

/// A runtime whose working-tree comparison describes the live buffer.
///
/// The file is open for editing and the index baseline is known, which is
/// what keeps that comparison in step with unsaved edits and what lets the
/// editor tint the lines the author added.
fn working_diff_runtime(
    card: CardId,
    before: &str,
    after: &str,
) -> (RepositoryRuntime, AsyncIdentity) {
    let (mut runtime, identity) = RepositoryRuntime::lsp_open_fixture(card, after);
    runtime.source_surface = SourceSurface::Diff;
    let model = crate::diff_view::build_diff(before, after);
    runtime.working_diff.additions = added_source_lines(&model);
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(model)),
    );
    runtime.working_diff.applied = runtime.wanted_working_diff();
    runtime.working_diff.desired = runtime.wanted_working_diff();
    (runtime, identity)
}

/// The added-line tint only tells the truth while the comparison it came
/// from still describes the buffer, so an edit has to retire it until the
/// next working diff lands.
#[test]
fn editing_the_working_source_retires_the_added_line_tint() {
    let card = CardId::new();
    let (mut runtime, _) = working_diff_runtime(card, "removed\nkept\n", "added\nkept\n");
    // The tint belongs to the buffer the author is typing into, which is
    // the Edit surface. Diff renders the comparison instead.
    runtime.source_surface = SourceSurface::Editor;
    assert_eq!(runtime.working_diff.additions, vec![0]);
    assert!(runtime.working_annotations_current());
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Move(text_editor::Motion::DocumentStart),
    }));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Select(text_editor::Motion::End),
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("native buffer")
    };
    assert_eq!(document.content.selection().as_deref(), Some("added"));
    // IME commits and clipboard paste both take this exact action path.
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("café".into()))),
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("native buffer")
    };
    assert_eq!(document.content.text(), "café\nkept\n");
    assert_eq!(document.saved_text, "added\nkept\n");
    assert!(document.dirty);
    assert_eq!(&*runtime.diff_model().unwrap().before, "removed\nkept\n");
    assert!(
        !runtime.working_annotations_current(),
        "a tint drawn from a superseded comparison must go out with it"
    );
}

/// The comparison surface mounts no buffer, so an editor action arriving
/// while it is up is a stale message from a surface that is gone.
#[test]
fn the_comparison_surface_takes_no_editor_actions() {
    let card = CardId::new();
    let (mut runtime, _) = working_diff_runtime(card, "old\n", "working\n");
    for basis in [
        DiffBasis::IndexToWorktree,
        DiffBasis::HeadToIndex,
        DiffBasis::CommitToParent {
            oid: fixture_oid(1),
            parent: Some(fixture_oid(2)),
        },
    ] {
        runtime.diff_basis = basis.clone();
        runtime.set_diff_state(
            basis,
            DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
                "old\n",
                "historic\n",
            ))),
        );
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("native buffer")
        };
        assert_eq!(document.content.text(), "working\n");
        assert!(!document.dirty);
        assert!(runtime.editor_focus_target(card).is_none());
    }
}

#[test]
fn working_diff_worker_matches_the_native_source_and_leaves_the_editor_alone() {
    use cosmic::iced::futures::StreamExt;

    for cancelled in [false, true] {
        let card = CardId::new();
        let baseline = "xlet café = \"😀\";\nsecond\r\n\rthird\rfourth";
        let saved = "let café = \"😀\";\r\nsecond\n\rthird\rfourth\n";
        let after = "xlet café = \"😀\";\r\nsecond\n\rthird\rfourth\n";
        let (mut runtime, _) = working_diff_runtime(card, baseline, saved);
        runtime.set_lsp_document(None);
        runtime.source_surface = SourceSurface::Editor;
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        runtime.working_diff.applied = None;
        drop(runtime.working_diff_task(card));
        let ticket = runtime.working_diff.timer.unwrap();
        runtime.working_diff.deadline = Some(Instant::now());
        let editor_state = |runtime: &RepositoryRuntime| {
            let DocumentState::Editable(document) = &runtime.document else {
                panic!("native buffer")
            };
            (
                document.saved_text.clone(),
                document.saved_history_revision,
                document.dirty,
                document.revision,
                document.history.revision(),
                document.content.cursor(),
            )
        };
        let original_state = editor_state(&runtime);
        assert!(runtime.is_dirty());

        let task = runtime.start_working_diff(card, ticket);
        assert_eq!(editor_state(&runtime), original_state);
        if cancelled {
            runtime
                .working_diff
                .active
                .as_ref()
                .unwrap()
                .cancellation
                .cancel();
        }

        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let completion = executor.block_on(async {
            let mut stream = iced::runtime::task::into_stream(task).unwrap();
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(action) = action {
                    return action;
                }
            }
            panic!("the diff worker must return its completion")
        });
        let RepositoryAction::WorkingDiffLoaded { result, .. } = &completion else {
            panic!("working diff completion")
        };
        if cancelled {
            assert!(matches!(result, Ok(None)));
        } else {
            let model = result.as_ref().unwrap().as_ref().unwrap();
            let expected = crate::diff_view::build_diff(baseline, after);
            for marker in ["[LF]", "[CRLF]", "[no newline]"] {
                assert!(
                    expected
                        .inline_rows
                        .iter()
                        .any(|row| row.text.ends_with(marker))
                );
            }
            assert_eq!(model.line_counts, expected.line_counts);
            assert_eq!(model.inline_rows, expected.inline_rows);
            assert_eq!(model.split_rows, expected.split_rows);
        }
        drop(runtime.update(completion));
        if !cancelled {
            assert!(runtime.working_annotations_current());
        }
        assert_eq!(editor_state(&runtime), original_state);
    }
}

#[test]
fn working_diff_coalesces_edits_and_rejects_cancelled_revision_results() {
    let card = CardId::new();
    let (mut runtime, identity) = working_diff_runtime(card, "old\n", "first\n");
    // Edits arrive from the Edit surface; the comparison surface mounts no
    // buffer to type into.
    runtime.source_surface = SourceSurface::Editor;
    runtime.working_diff.applied = None;
    drop(runtime.working_diff_task(card));
    let timer = runtime.working_diff.timer.unwrap();
    for _ in 0..20 {
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        drop(runtime.working_diff_task(card));
        assert_eq!(runtime.working_diff.timer, Some(timer));
        assert!(runtime.working_diff.active.is_none());
    }
    runtime.working_diff.deadline = Some(Instant::now());
    drop(runtime.start_working_diff(card, timer));
    let active = runtime.working_diff.active.as_ref().unwrap();
    let revision = active.revision;
    let before = Arc::clone(&active.before);
    let cancellation = active.cancellation.clone();
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('y')),
    }));
    drop(runtime.working_diff_task(card));
    assert!(cancellation.is_cancelled());
    assert!(
        runtime.working_diff.timer.is_none(),
        "a cancelled CPU slot stays occupied until completion"
    );
    drop(runtime.finish_working_diff(
        identity.clone(),
        revision,
        timer,
        before.clone(),
        Ok(Some(crate::diff_view::build_diff(&before, "obsolete\n"))),
    ));
    assert!(runtime.working_diff.applied.is_none());
    let next = runtime.working_diff.timer.unwrap();
    runtime.working_diff.deadline = Some(Instant::now());
    drop(runtime.start_working_diff(card, next));
    let active = runtime.working_diff.active.as_ref().unwrap();
    let revision = active.revision;
    let before = Arc::clone(&active.before);
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("native buffer")
    };
    let text = document.content.text();
    let model = crate::diff_view::build_diff(&before, &text);
    drop(runtime.finish_working_diff(identity, revision, next, before, Ok(Some(model))));
    assert!(runtime.working_annotations_current());
    assert!(runtime.working_diff.timer.is_none());
    assert!(runtime.working_diff.active.is_none());
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("native buffer")
    };
    assert_eq!(document.content.text(), text);
}

#[test]
fn equal_baseline_bytes_from_a_new_basis_request_cannot_accept_an_old_worker() {
    let card = CardId::new();
    let (mut runtime, identity) = working_diff_runtime(card, "old\n", "new\n");
    runtime.working_diff.applied = None;
    drop(runtime.working_diff_task(card));
    let ticket = runtime.working_diff.timer.unwrap();
    runtime.working_diff.deadline = Some(Instant::now());
    drop(runtime.start_working_diff(card, ticket));
    let before = Arc::clone(&runtime.working_diff.active.as_ref().unwrap().before);
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
            "old\n", "new\n",
        ))),
    );
    drop(runtime.finish_working_diff(
        identity,
        0,
        ticket,
        before,
        Ok(Some(crate::diff_view::build_diff("old\n", "obsolete\n"))),
    ));
    assert!(runtime.working_diff.applied.is_none());
    assert_eq!(
        runtime
            .diff_model()
            .unwrap()
            .inline_rows
            .last()
            .unwrap()
            .text,
        "new"
    );
}

#[test]
fn unrelated_status_changes_do_not_invalidate_the_selected_diff() {
    let card = CardId::new();
    let (mut runtime, _) = working_diff_runtime(card, "old\n", "new\n");
    let baseline = Arc::clone(&runtime.diff_model().unwrap().before);
    let FileListState::Ready(files) = &runtime.files else {
        panic!("file list")
    };
    let mut files = files.clone();
    let other = files
        .iter_mut()
        .find(|file| file.path == Path::new("other.txt"))
        .unwrap();
    other.staged = true;
    other.status = Some("MM".into());
    runtime.file_list_loading = true;
    runtime.background_file_refresh = true;
    let identity = AsyncIdentity {
        card,
        path: ".".into(),
        worktree: runtime.descriptor.worktree.clone(),
        generation: runtime.file_list_generation,
    };
    drop(runtime.update(RepositoryAction::FilesLoaded {
        identity,
        result: Ok(files),
    }));
    assert!(Arc::ptr_eq(
        &baseline,
        &runtime.diff_model().unwrap().before
    ));
    assert!(runtime.file(Path::new("other.txt")).unwrap().staged);
    assert!(runtime.working_annotations_current());
}

#[test]
fn status_refresh_after_our_save_keeps_the_visible_working_diff() {
    let card = CardId::new();
    let (mut runtime, _) = working_diff_runtime(card, "old\n", "new\n");
    // SaveFinished updates the selected file before the next status read,
    // while the list can still contain its pre-save clean status.
    let selected = runtime.selected.clone().unwrap();
    let FileListState::Ready(files) = &mut runtime.files else {
        panic!("file list")
    };
    let previous = files
        .iter_mut()
        .find(|file| file.path == selected.path)
        .unwrap();
    previous.unstaged = false;
    previous.status = None;
    let mut refreshed = files.clone();
    let refreshed_file = refreshed
        .iter_mut()
        .find(|file| file.path == selected.path)
        .unwrap();
    *refreshed_file = selected;
    drop(runtime.refresh_visible(card));
    let identity = AsyncIdentity {
        card,
        path: ".".into(),
        worktree: runtime.descriptor.worktree.clone(),
        generation: runtime.file_list_generation,
    };
    drop(runtime.update(RepositoryAction::FilesLoaded {
        identity,
        result: Ok(refreshed),
    }));
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert!(runtime.working_annotations_current());
    assert!(
        matches!(
            runtime.diff_state(&DiffBasis::IndexToWorktree),
            Some(DiffState::Ready(_, _))
        ),
        "our own save must not replace the working diff with a loading screen"
    );
}

#[test]
fn branch_only_paths_open_read_only_and_keep_rename_identity_for_retry() {
    let card = CardId::new();
    let (mut runtime, identity) = working_diff_runtime(card, "old\n", "working\n");
    let basis = DiffBasis::MergeBaseToHead {
        base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        head: fixture_oid(1),
        merge_base: fixture_oid(2),
    };
    drop(runtime.update(RepositoryAction::OpenCommittedFile {
        card,
        path: RepoPath::parse("deleted-or-renamed.txt").unwrap(),
        old_path: Some(RepoPath::parse("old-name.txt").unwrap()),
        basis: basis.clone(),
    }));
    assert!(!runtime.accepts_selected(&identity));
    assert!(matches!(runtime.document, DocumentState::None));
    assert!(!runtime.can_save());
    assert!(runtime.file(Path::new("deleted-or-renamed.txt")).is_none());
    assert_eq!(
        runtime.comparison_old_path(),
        Some(PathBuf::from("old-name.txt"))
    );
    assert_eq!(runtime.diff_basis, basis);
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    runtime.set_diff_state(
        basis.clone(),
        DiffState::ready(RenderedDiff::Error("retry".into())),
    );
    drop(runtime.update(RepositoryAction::RetryDiff { card }));
    assert!(matches!(
        runtime.diff_state(&basis),
        Some(DiffState::Loading { .. })
    ));
    assert_eq!(
        runtime.comparison_old_path(),
        Some(PathBuf::from("old-name.txt"))
    );
}

#[test]
fn commit_only_paths_open_read_only_and_keep_rename_identity_for_retry() {
    let card = CardId::new();
    let (mut runtime, identity) = working_diff_runtime(card, "old\n", "working\n");
    let basis = DiffBasis::CommitToParent {
        oid: fixture_oid(1),
        parent: Some(fixture_oid(2)),
    };
    drop(runtime.update(RepositoryAction::OpenCommittedFile {
        card,
        path: RepoPath::parse("deleted-or-renamed.txt").unwrap(),
        old_path: Some(RepoPath::parse("old-name.txt").unwrap()),
        basis: basis.clone(),
    }));
    assert!(!runtime.accepts_selected(&identity));
    assert!(matches!(runtime.document, DocumentState::None));
    assert!(!runtime.can_save());
    assert!(runtime.file(Path::new("deleted-or-renamed.txt")).is_none());
    assert_eq!(
        runtime.comparison_old_path(),
        Some(PathBuf::from("old-name.txt"))
    );
    assert_eq!(runtime.diff_basis, basis);
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    runtime.set_diff_state(
        basis.clone(),
        DiffState::ready(RenderedDiff::Error("retry".into())),
    );
    drop(runtime.update(RepositoryAction::RetryDiff { card }));
    assert!(matches!(
        runtime.diff_state(&basis),
        Some(DiffState::Loading { .. })
    ));
    assert_eq!(
        runtime.comparison_old_path(),
        Some(PathBuf::from("old-name.txt"))
    );
}

/// A review surface installs a pull request comparison over a working
/// file list that knows nothing about it. The comparison is read-only,
/// keeps its rename identity, survives the status refresh a background
/// change triggers, and can be retried — exactly like the other immutable
/// bases, which is what the review surface will depend on.
#[test]
fn a_pull_request_basis_survives_status_refresh_and_retries() {
    let card = CardId::new();
    let (mut runtime, identity) = working_diff_runtime(card, "old\n", "working\n");
    let basis = DiffBasis::PullRequest {
        number: 12,
        base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        head: fixture_oid(1),
        merge_base: fixture_oid(2),
    };
    drop(runtime.update(RepositoryAction::OpenCommittedFile {
        card,
        path: RepoPath::parse("renamed.txt").unwrap(),
        old_path: Some(RepoPath::parse("old-name.txt").unwrap()),
        basis: basis.clone(),
    }));
    assert!(!runtime.accepts_selected(&identity));
    assert!(matches!(runtime.document, DocumentState::None));
    assert!(!runtime.can_save());
    assert_eq!(runtime.diff_basis, basis);
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert!(runtime.can_show_diff());
    assert_eq!(basis_label(&basis), "#12 vs main");
    assert_eq!(basis_choice_label(&basis), "Pull request");
    assert_eq!(basis_choice(&basis), None);

    // The refresh a background working-tree change triggers must not
    // replace the review with a working comparison.
    drop(runtime.refresh_selected_status(card));
    assert_eq!(runtime.diff_basis, basis);
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert_eq!(
        runtime.comparison_old_path(),
        Some(PathBuf::from("old-name.txt"))
    );

    runtime.set_diff_state(
        basis.clone(),
        DiffState::ready(RenderedDiff::Error("retry".into())),
    );
    assert!(runtime.can_retry_diff());
    drop(runtime.update(RepositoryAction::RetryDiff { card }));
    assert!(matches!(
        runtime.diff_state(&basis),
        Some(DiffState::Loading { .. })
    ));
    assert_eq!(
        runtime.comparison_old_path(),
        Some(PathBuf::from("old-name.txt"))
    );
}

#[test]
fn branch_navigation_cannot_discard_an_unsaved_or_saving_native_buffer() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::branch_navigation_cannot_discard_an_unsaved_or_saving_native_buffer",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, _) = save_fixture(card, "notes.txt", "working\n");
    // The buffer is dirtied through the Edit surface, which is the only
    // surface that mounts it.
    runtime.source_surface = SourceSurface::Editor;
    let open = RepositoryAction::OpenCommittedFile {
        card,
        path: RepoPath::parse("deleted.txt").unwrap(),
        old_path: None,
        basis: DiffBasis::MergeBaseToHead {
            base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
            head: fixture_oid(1),
            merge_base: fixture_oid(2),
        },
    };
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.update(open.clone()));
    assert_eq!(
        runtime.selected.as_ref().unwrap().path,
        Path::new("notes.txt")
    );
    assert!(runtime.is_dirty());
    drop(runtime.request_save(card, false));
    drop(runtime.update(open));
    assert_eq!(
        runtime.selected.as_ref().unwrap().path,
        Path::new("notes.txt")
    );
    assert!(runtime.is_saving());
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
}

#[test]
fn faint_diff_syntax_remains_readable_over_both_highlight_layers() {
    for theme in [cosmic::Theme::light(), cosmic::Theme::dark()] {
        for kind in [DiffRowKind::Addition, DiffRowKind::Deletion] {
            let row = diff_row_background(&theme, kind);
            let highlight = color_over(diff_highlight_color(&theme, kind, 0.18), row);
            let faint = iced::Color::from_rgba(0.6, 0.6, 0.6, 0.7);
            for background in [row, highlight] {
                let foreground = readable_diff_color(faint, background);
                assert!(foreground.relative_contrast(background) >= 4.5);
                assert_eq!(readable_diff_color(foreground, background), foreground);
            }
        }
    }
}

#[test]
fn mode_switches_keep_the_dirty_editor_and_diff_follows_its_line() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.md", "# Notes\n");
    let editor_position = text_editor::ScrollPosition {
        line: 12,
        vertical: 7.0,
        horizontal: 95.5,
    };
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.markdown = true;
        document.dirty = true;
        document.content.restore_scroll_position(editor_position);
    }
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
            "",
            &"line\n".repeat(100),
        ))),
    );
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    drop(runtime.update(RepositoryAction::DiffViewport {
        user_initiated: true,
        card,
        offset: 900.0,
        height: 300.0,
        horizontal: 40.0,
    }));
    drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
    assert_eq!(runtime.diff_viewport.offset, 900.0);
    drop(runtime.update(RepositoryAction::DiffViewport {
        user_initiated: true,
        card,
        offset: 1500.0,
        height: 300.0,
        horizontal: 80.0,
    }));
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    drop(runtime.update(RepositoryAction::PreviewViewport {
        card,
        offset: 700.0,
    }));
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: false,
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("mode changes must not replace the buffer");
    };
    assert!(document.dirty);
    assert_eq!(document.content.text(), "# Notes\n");
    assert_eq!(document.content.scroll_position(), editor_position);
    // Returning from the editor follows the line the reader was on.
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    let row = runtime.diff_typography().line_height;
    assert_eq!(
        (runtime.diff_viewport.offset, runtime.diff_horizontal),
        (
            12.0 * row + editor_position.vertical,
            editor_position.horizontal
        )
    );
    // The other layout's older place does not take the line away.
    drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: false }));
    assert_eq!(
        (runtime.diff_viewport.offset, runtime.diff_horizontal),
        (
            12.0 * row + editor_position.vertical,
            editor_position.horizontal
        )
    );
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    assert_eq!(runtime.viewports.preview, 700.0);
    // Navigation still refuses to discard this unsaved buffer.
    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: "other.txt".into(),
    }));
    assert_eq!(
        runtime.selected.as_ref().unwrap().path,
        Path::new("notes.md")
    );
}

#[test]
fn reopening_files_restores_viewports_without_accepting_stale_loads() {
    let card = CardId::new();
    let text = format!("# Notes {}\n", "long source line ".repeat(30)).repeat(100);
    let (mut runtime, stale) = RepositoryRuntime::open_fixture(card, "notes.md", &text);
    let editor_position = text_editor::ScrollPosition {
        line: 25,
        vertical: 0.0,
        horizontal: 140.5,
    };
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.markdown = true;
        document.content.restore_scroll_position(editor_position);
    }
    let renderer = headless_renderer();
    let mut tree = Tree::new(editor_surface(card, &runtime).as_widget());
    let cursor = runtime.cursor_position();
    {
        let mut element = editor_surface(card, &runtime);
        element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(260.0, 200.0)),
        );
    }
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
            "",
            &"line\n".repeat(100),
        ))),
    );
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    drop(runtime.update(RepositoryAction::DiffViewport {
        user_initiated: true,
        card,
        offset: 1200.0,
        height: 300.0,
        horizontal: 35.0,
    }));
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    drop(runtime.update(RepositoryAction::PreviewViewport {
        card,
        offset: 800.0,
    }));
    let FileListState::Ready(files) = &runtime.files else {
        panic!("the fixture has a loaded file list");
    };
    let files = files.clone();
    drop(runtime.update(RepositoryAction::BackToFiles { card }));
    let identity = AsyncIdentity {
        card,
        worktree: runtime.descriptor.worktree.clone(),
        path: ".".into(),
        generation: runtime.file_list_generation,
    };
    drop(runtime.update(RepositoryAction::FilesLoaded {
        identity,
        result: Ok(files),
    }));
    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: "other.txt".into(),
    }));
    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: "notes.md".into(),
    }));
    let loaded = || {
        Ok(LoadedFile {
            source: "notes.md".into(),
            content: FileContent::Editable {
                text: text.clone(),
                content_hash: blake3::hash(text.as_bytes()),
                markdown: true,
            },
        })
    };
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity: stale,
        result: loaded(),
    }));
    assert!(matches!(runtime.document, DocumentState::Loading { .. }));
    let identity = runtime.current_identity(card, "notes.md".into());
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: loaded(),
    }));
    {
        let mut element = editor_surface(card, &runtime);
        tree.diff(element.as_widget_mut());
        element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(260.0, 200.0)),
        );
    }
    assert_eq!(runtime.cursor_position(), cursor);
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("the current file should finish loading");
    };
    assert_eq!(document.content.scroll_position(), editor_position);
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    assert_eq!(runtime.viewports.preview, 800.0);
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    assert_eq!(
        (runtime.diff_viewport.offset, runtime.diff_horizontal),
        (1200.0, 35.0)
    );
    let request = runtime.next_diff_request;
    let identity = runtime.current_identity(card, "notes.md".into());
    drop(runtime.update(RepositoryAction::DiffLoaded {
        identity,
        basis: DiffBasis::IndexToWorktree,
        request,
        content: staged_text_diff(crate::diff_view::build_diff("", &"line\n".repeat(100))),
    }));
    assert_eq!(runtime.diff_viewport.offset, 1200.0);
    let visible = visible_rows(1000, runtime.diff_viewport, DIFF_ROW_HEIGHT);
    let restored = (1200.0 / DIFF_ROW_HEIGHT) as usize;
    assert!(visible.start <= restored && visible.end > restored);
}

#[test]
fn viewport_retention_evicts_old_paths_but_refreshes_revisited_paths() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::selection_fixture(card, "old.txt");
    runtime.set_standalone(true);
    drop(runtime.update(RepositoryAction::DiffViewport {
        user_initiated: true,
        card,
        offset: 600.0,
        height: 300.0,
        horizontal: 0.0,
    }));
    for index in 0..MAX_RETAINED_FILE_VIEWPORTS {
        drop(runtime.update(RepositoryAction::OpenFile {
            card,
            path: format!("file-{index}.txt").into(),
        }));
    }
    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: "old.txt".into(),
    }));
    assert_eq!(runtime.diff_viewport.offset, 600.0);
    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: "extra.txt".into(),
    }));
    assert!(runtime.retained_viewports.len() <= MAX_RETAINED_FILE_VIEWPORTS);
    assert!(
        !runtime
            .retained_viewports
            .iter()
            .any(|(path, _)| path == Path::new("file-0.txt"))
    );
    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: "old.txt".into(),
    }));
    assert_eq!(runtime.diff_viewport.offset, 600.0);
}

#[test]
fn text_preview_preserves_literal_extensionless_content_and_retains_its_viewport() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::selection_fixture(card, "LICENSE");
    let text = "# Literal text\n\tIndented\n<tag>& symbol\n";
    runtime.document = DocumentState::Editable(EditorDocument::unlinked(
        "LICENSE".into(),
        text.into(),
        blake3::hash(text.as_bytes()),
        false,
    ));
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert_eq!(document.preview_text, text);
    assert!(document.markdown_items.is_empty());
    assert!(!runtime.editable_surface_visible());
    drop(runtime.update(RepositoryAction::PreviewViewport {
        card,
        offset: 420.0,
    }));
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: false,
    }));
    assert!(runtime.editable_surface_visible());
    assert_eq!(runtime.viewports.preview, 420.0);
}

#[test]
fn diagram_theme_change_cancels_work_and_fences_old_tone_completions() {
    let card = CardId::new();
    let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "chart.mmd");
    runtime.document = DocumentState::Editable(EditorDocument::unlinked(
        "chart.mmd".into(),
        "flowchart LR; A-->B".into(),
        blake3::hash(b"fixture"),
        false,
    ));
    runtime.markdown_mode = MarkdownMode::Preview;
    drop(runtime.diagram_task(card));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    let old_cancel = document.diagram_cancel.as_ref().unwrap().0.clone();
    let other = if document.diagram_dark {
        cosmic::Theme::light()
    } else {
        cosmic::Theme::dark()
    };
    drop(runtime.retone_document(card, &other));
    assert!(old_cancel.is_cancelled());
    let key = blake3::hash(b"flowchart LR; A-->B");
    drop(runtime.update(RepositoryAction::DiagramsRendered {
        identity: identity.clone(),
        revision: 0,
        tone_generation: 0,
        result: vec![(key, preview::Diagram::Error("old tone".into()))],
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert!(document.diagrams.is_empty());
    assert_eq!(document.diagram_pending, Some(0));
    assert_eq!(document.diagram_tone_generation, 1);
    drop(runtime.update(RepositoryAction::DiagramsRendered {
        identity,
        revision: 0,
        tone_generation: 1,
        result: vec![(key, preview::Diagram::Error("new tone".into()))],
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert!(document.diagrams.contains_key(&key));
    assert_eq!(document.diagram_pending, None);
    drop(runtime.retone_document(card, &other));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert_eq!(document.diagram_pending, None);
    assert_eq!(document.diagram_tone_generation, 1);
}

#[test]
fn diagram_completion_is_fenced_by_document_identity_and_buffer_revision() {
    let card = CardId::new();
    let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "chart.mmd");
    runtime.document = DocumentState::Editable(EditorDocument::unlinked(
        "chart.mmd".into(),
        "flowchart LR; A-->B".into(),
        blake3::hash(b"fixture"),
        false,
    ));
    runtime.markdown_mode = MarkdownMode::Preview;
    drop(runtime.diagram_task(card));
    let DocumentState::Editable(document) = &mut runtime.document else {
        panic!()
    };
    assert_eq!(document.diagram_pending, Some(0));
    document.revision = 1;
    document.content = text_editor::Content::with_text("flowchart LR; A-->C");
    document.markdown_stale = true;
    drop(runtime.diagram_task(card)); // Coalesces onto the one outstanding worker.
    let key = blake3::hash(b"flowchart LR; A-->B");
    drop(runtime.update(RepositoryAction::DiagramsRendered {
        identity: identity.clone(),
        revision: 0,
        tone_generation: 0,
        result: vec![(key, preview::Diagram::Error("stale".into()))],
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert!(!document.diagrams.contains_key(&key));
    assert_eq!(document.diagram_pending, None);
    let ticket = runtime.markdown_debounce.timer.unwrap();
    runtime.markdown_debounce.deadline = None;
    drop(runtime.finish_markdown(card, ticket));
    let mut stale = identity.clone();
    stale.generation += 1;
    drop(runtime.update(RepositoryAction::DiagramsRendered {
        identity: stale,
        revision: 1,
        tone_generation: 0,
        result: vec![],
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert_eq!(document.diagram_pending, Some(1));
    let key = blake3::hash(b"flowchart LR; A-->C");
    drop(runtime.update(RepositoryAction::DiagramsRendered {
        identity,
        revision: 1,
        tone_generation: 0,
        result: vec![(key, preview::Diagram::Error("invalid".into()))],
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert!(document.diagrams.contains_key(&key));
    assert_eq!(document.diagram_pending, None);
}

#[test]
fn preview_actions_require_a_preview_renderer_and_preserve_the_current_surface() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.document = DocumentState::Editable(EditorDocument::unlinked(
        PathBuf::from("plain.rs"),
        "fn main() {}\n".into(),
        blake3::hash(b"fn main() {}\n"),
        false,
    ));
    assert!(runtime.can_apply_markdown_mode(true));
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
    assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: false,
    }));
    assert_eq!(runtime.source_surface, SourceSurface::Editor);

    runtime.document = DocumentState::Editable(EditorDocument::unlinked(
        PathBuf::from("README.md"),
        "# Heading\n".into(),
        blake3::hash(b"# Heading\n"),
        true,
    ));
    assert!(runtime.can_apply_markdown_mode(true));
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
    assert!(runtime.editor_focus_target(card).is_none());
}

#[test]
fn diff_actions_share_capabilities_without_blocking_committed_comparisons_or_retry() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.source_surface = SourceSurface::Editor;
    let file = runtime.selected.as_mut().unwrap();
    file.staged = false;
    file.unstaged = false;
    assert!(!runtime.can_show_diff());
    assert_eq!(
        runtime.diff_basis_refusal(&DiffBasis::HeadToIndex),
        Some(DiffBasisRefusal::NotListed)
    );
    runtime.mutation_refresh_pending = true;
    assert_eq!(
        runtime.diff_basis_refusal(&DiffBasis::HeadToIndex),
        Some(DiffBasisRefusal::Refreshing)
    );
    runtime.mutation_refresh_pending = false;
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    drop(runtime.update(RepositoryAction::ApplyDiffBasis {
        card,
        basis: DiffBasis::HeadToIndex,
    }));
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
    assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);

    let basis = DiffBasis::CommitToParent {
        oid: fixture_oid(1),
        parent: Some(fixture_oid(2)),
    };
    assert!(runtime.can_select_diff_basis(&basis));
    drop(runtime.update(RepositoryAction::ApplyDiffBasis {
        card,
        basis: basis.clone(),
    }));
    assert!(runtime.can_show_diff());
    assert!(!runtime.can_change_diff_layout());
    drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
    assert_eq!(runtime.diff_mode, DiffMode::Inline);
    runtime.set_diff_state(
        basis.clone(),
        DiffState::ready(RenderedDiff::Error("read failed".into())),
    );
    assert!(runtime.can_retry_diff());
    drop(runtime.update(RepositoryAction::RetryDiff { card }));
    assert!(!runtime.can_retry_diff());
    assert!(matches!(
        runtime.diff_state(&basis),
        Some(DiffState::Loading { .. })
    ));
    runtime.set_diff_state(basis, DiffState::ready(staged_text_diff(model_with(1))));
    assert!(runtime.can_change_diff_layout());
    drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
    assert_eq!(runtime.diff_mode, DiffMode::Split);
}

#[test]
fn editor_navigation_only_schedules_visible_diffs_and_cancels_hidden_workers() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    drop(runtime.open_file(card, identity.path));
    assert!(
        runtime.diffs.is_empty(),
        "opening text must not compute either hidden diff"
    );
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    let (request, cancelled_on_hide) = match runtime.diff_state(&DiffBasis::IndexToWorktree) {
        Some(DiffState::Loading {
            request,
            cancellation,
        }) => (*request, cancellation.clone()),
        _ => panic!("the visible comparison must start loading"),
    };
    assert!(runtime.diff_state(&DiffBasis::HeadToIndex).is_none());
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    assert!(
        matches!(
            runtime.diff_state(&DiffBasis::IndexToWorktree),
            Some(DiffState::Loading { request: current, .. }) if *current == request
        ),
        "redrawing or reselecting the same surface must not restart its worker"
    );
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: false }));
    assert!(cancelled_on_hide.is_cancelled());
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());

    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    let cancelled_on_close = match runtime.diff_state(&DiffBasis::IndexToWorktree) {
        Some(DiffState::Loading { cancellation, .. }) => cancellation.clone(),
        _ => panic!("returning to an unfinished comparison must restart it"),
    };
    drop(runtime);
    assert!(cancelled_on_close.is_cancelled());
}
use crate::repository::Oid;
use tempfile::tempdir;

use super::*;

#[test]
fn diff_scroll_range_matches_shaped_content_and_real_split_half_layout() {
    let renderer = headless_renderer();
    for source in [
        "short".to_owned(),
        "a very long ASCII line ".repeat(24),
        "\tCafe\u{301} 界 👩‍💻\tffi ".repeat(18),
        "__global__ void kernel() { const char* name = \"CUDA\"; }\t".repeat(12),
    ] {
        let RenderedDiff::Text { model, .. } = RenderedDiff::from_content(
            DiffContent::Comparison {
                old: DiffSide::Absent,
                new: DiffSide::Text {
                    text: format!("{source}\n"),
                    bytes: source.len() as u64 + 1,
                },
                old_label: "old".into(),
                new_label: "new".into(),
            },
            Path::new("kernel.cu"),
            crate::settings::EditorSyntaxTheme::Auto,
            Font::MONOSPACE,
            &CancellationToken::new(),
        ) else {
            panic!("text comparison must produce rows");
        };
        let dark = cosmic::theme::is_dark();
        let mut widest = 0.0_f32;
        for row in &model.inline_rows {
            let mut text: Element<'_, RepositoryAction> =
                iced::widget::rich_text(crate::diff_view::line_spans(
                    &row.text,
                    &row.changed,
                    model
                        .syntax
                        .line(row.new_line.is_none(), row.new_line.or(row.old_line), dark),
                    iced::Color::TRANSPARENT,
                ))
                .size(DIFF_FONT_SIZE)
                .font(Font::MONOSPACE)
                .line_height(iced::widget::text::LineHeight::Absolute(iced::Pixels(
                    DIFF_LINE_HEIGHT,
                )))
                .wrapping(iced::widget::text::Wrapping::None)
                .into();
            let mut tree = Tree::new(text.as_widget());
            let node = text.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(
                    iced::Size::ZERO,
                    iced::Size::new(f32::INFINITY, f32::INFINITY),
                ),
            );
            widest = widest.max(node.size().width);
        }
        let code_width = model.max_line_width[usize::from(dark)] as f32;
        assert!(code_width >= widest && code_width - widest < 1.0);
        // Reuse the model through narrow/wide/narrow viewports and both
        // modes: no stale range may survive a resize or mode transition.
        for width in [320.0, 1200.0, 480.0] {
            for mode in [DiffMode::Split, DiffMode::Inline, DiffMode::Split] {
                let geometry = DiffScrollGeometry::new(width, 44.0, mode);
                if mode == DiffMode::Inline {
                    let two_gutter_code = geometry.viewport_width - 2.0 * 44.0 - DIFF_MARKER_WIDTH;
                    assert!((geometry.code_viewport - two_gutter_code).abs() < 0.01);
                }
                let offset = geometry.overflow(code_width);
                if widest + 2.0 * DIFF_ROW_PADDING <= geometry.code_viewport {
                    assert_eq!(offset, 0.0, "short lines must not scroll");
                } else {
                    assert!(
                        (widest + 2.0 * DIFF_ROW_PADDING - offset - geometry.code_viewport).abs()
                            < 1.0,
                        "the last shaped pixel, not an estimated column, ends the range"
                    );
                }
                let mut row = match mode {
                    DiffMode::Inline => inline_diff_row(
                        model.inline_rows.last().unwrap(),
                        &model.syntax,
                        44.0,
                        code_width,
                        offset,
                    ),
                    DiffMode::Split => split_diff_row(
                        model.split_rows.last().unwrap(),
                        &model.syntax,
                        44.0,
                        code_width,
                        offset,
                    ),
                };
                let mut tree = Tree::new(row.as_widget());
                let node = row.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(
                        iced::Size::ZERO,
                        iced::Size::new(geometry.viewport_width, DIFF_ROW_HEIGHT),
                    ),
                );
                let root = Layout::new(&node);
                let halves = match mode {
                    DiffMode::Inline => vec![root],
                    DiffMode::Split => root.children().step_by(2).collect(),
                };
                for half in halves {
                    let content = half.children().next().unwrap();
                    let code = content.children().last().unwrap();
                    assert!((code.bounds().width - geometry.code_viewport).abs() < 0.01);
                    let padded = code.children().next().unwrap();
                    assert!((padded.bounds().x - code.bounds().x + offset).abs() < 0.01);
                    assert!(
                        (padded.bounds().width - code_width - 2.0 * DIFF_ROW_PADDING).abs() < 0.01
                    );
                    let shaped = padded.children().next().unwrap();
                    assert!(
                        (shaped.bounds().x - code.bounds().x + offset - DIFF_ROW_PADDING).abs()
                            < 0.01
                    );
                    assert!((shaped.bounds().width - code_width).abs() < 0.01);
                }
            }
        }
    }
}

/// Every scroll viewport in a laid-out surface, outermost first, paired
/// with the content node that scrolls inside it.
///
/// A scroll viewport is the one place a child is laid out larger than the
/// parent clipping it, so finding them by that shape keeps the assertions
/// below independent of how the surrounding view nests them.
fn scroll_viewports(root: &layout::Node) -> Vec<(&layout::Node, &layout::Node)> {
    let mut found = Vec::new();
    let mut pending = std::collections::VecDeque::from([root]);
    while let Some(node) = pending.pop_front() {
        if let [content] = node.children() {
            let outer = node.size();
            let inner = content.size();
            if inner.width > outer.width + 0.5 || inner.height > outer.height + 0.5 {
                found.push((node, content));
            }
        }
        pending.extend(node.children());
    }
    found
}

/// The rectangle a node's drawable children occupy, in that node's own
/// coordinates. Zero-area children are the virtualization and rail
/// spacers, which are the reservation rather than content inside it.
fn occupied(parent: &layout::Node) -> iced::Rectangle {
    parent
        .children()
        .iter()
        .map(layout::Node::bounds)
        .filter(|bounds| bounds.width > 0.0 && bounds.height > 0.0)
        .reduce(|union, bounds| union.union(&bounds))
        .expect("the surface lays out drawable content")
}

fn laid_out(
    mut element: Element<'_, RepositoryAction>,
    renderer: &cosmic::Renderer,
    size: iced::Size,
) -> layout::Node {
    let mut tree = Tree::new(element.as_widget());
    element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(iced::Size::ZERO, size),
    )
}

/// The scroll rail is a layout promise, so it is read back off laid-out
/// surfaces rather than off the constants it is built from: a two-axis
/// scrollable has to leave the corner where its two thumbs meet empty, a
/// vertical-only one has to stop its content short of the right edge, and
/// a horizontal-only one must reserve the bottom rail without also
/// reserving a right rail it has no thumb to hide under.
#[test]
fn scroll_rails_keep_content_clear_of_every_thumb_and_their_junction() {
    let renderer = headless_renderer();
    let rail = crate::design::SCROLLBAR_GUTTER;
    let card = CardId::new();
    let line = "let value = compute(argument, another_argument, third_argument);";
    let before = format!("{}kept\n", format!("{line}\n").repeat(24));
    let (runtime, _) = working_diff_runtime(card, &before, "kept\n");
    let model = runtime.diff_model().expect("a text comparison");

    // Two axes: the diff canvas, whose rails are the ones editor content
    // coordinates are derived from.
    let surface = iced::Size::new(420.0, 240.0);
    let diff = laid_out(
        diff_model_view(card, &runtime, model, "HEAD", "Worktree", None),
        &renderer,
        surface,
    );
    let (viewport, canvas) = *scroll_viewports(&diff)
        .first()
        .expect("the diff mounts a scrollable");
    assert!(
        canvas.size().width > viewport.size().width
            && canvas.size().height > viewport.size().height,
        "the fixture has to overflow both axes for either thumb to exist"
    );
    // The canvas holds one column, translated in X as the code pans.
    let column = canvas
        .children()
        .first()
        .expect("the canvas carries the row column");
    let right = viewport.size().width - (column.bounds().x + column.bounds().width);
    assert!(
        (right - rail).abs() < 0.5,
        "diff rows reach {right} from the viewport's right edge, not {rail}"
    );
    let rows = occupied(column);
    let bottom = column.bounds().height - (rows.y + rows.height);
    assert!(
        (bottom - rail).abs() < 0.5,
        "the lowest diff row leaves {bottom} below it, not {rail}"
    );

    // All toolbar targets share the full header's center, including when
    // the action lane overflows. A bottom-only rail used to lift these
    // targets above the filename and the shell's trailing Find button.
    fn check_targets(node: &layout::Node, parent_y: f32, count: &mut usize) {
        let bounds = node.bounds();
        let y = parent_y + bounds.y;
        let target = crate::design::PRIMARY_CONTROL_SIZE;
        if (bounds.width - target).abs() < 0.1 && (bounds.height - target).abs() < 0.1 {
            assert!(
                (y + bounds.height / 2.0 - crate::design::HEADER_CONTENT_HEIGHT / 2.0).abs() < 0.1,
                "toolbar target must share the header baseline"
            );
            *count += 1;
        }
        for child in node.children() {
            check_targets(child, y, count);
        }
    }
    for width in [240.0, 1200.0] {
        let toolbar = laid_out(
            selected_file_toolbar(card, &runtime).unwrap(),
            &renderer,
            iced::Size::new(width, crate::design::HEADER_HEIGHT),
        );
        let mut targets = 0;
        check_targets(&toolbar, 0.0, &mut targets);
        assert!(targets > 0);
    }

    // The compact toolbar keeps its primary actions visible and moves
    // secondary actions into a menu instead of a horizontal scroll lane.
    let toolbar = laid_out(
        selected_file_toolbar(card, &runtime).expect("a selected file has a toolbar"),
        &renderer,
        iced::Size::new(240.0, crate::design::HEADER_HEIGHT),
    );
    assert!(scroll_viewports(&toolbar).is_empty());

    // Vertical only: the Markdown preview, whose inset is `space::XL`.
    // That is wider than the rail today, and this is what notices if the
    // inset ever becomes narrower.
    let (mut preview, identity) = RepositoryRuntime::selection_fixture(card, "notes.md");
    let text = "# Notes\n\nA paragraph long enough to wrap inside the preview.\n".repeat(24);
    drop(preview.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: "notes.md".into(),
            content: crate::editor::FileContent::Editable {
                content_hash: blake3::hash(text.as_bytes()),
                text,
                markdown: true,
            },
        }),
    }));
    drop(preview.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    let DocumentState::Editable(document) = &preview.document else {
        panic!("the fixture loads an editable Markdown buffer");
    };
    let rendered = laid_out(
        editable_document_view(card, &preview, document),
        &renderer,
        surface,
    );
    let (viewport, content) = *scroll_viewports(&rendered)
        .first()
        .expect("the preview scrolls");
    assert!(
        content.size().height > viewport.size().height,
        "the fixture has to overflow for a thumb to exist"
    );
    let prose = occupied(content);
    let right = content.size().width - (prose.x + prose.width);
    assert!(
        right >= rail - 0.5,
        "preview prose ends {right} from the right edge, inside the {rail} rail"
    );
}

#[test]
fn diff_accessibility_retains_source_and_line_context_through_scrolling() {
    use iced_accessibility::accesskit::Role;

    let before = "\tlet café = old_value(); // preserve all source text";
    let after = "\tlet café = new_value(); // preserve all source text";
    let model = crate::diff_view::build_diff(
        &format!("{before}\nunchanged\n"),
        &format!("{after}\nunchanged\n"),
    );
    let renderer = headless_renderer();
    for mode in [DiffMode::Inline, DiffMode::Split] {
        let mut gutter_bounds = None;
        for horizontal in [0.0, 120.0] {
            let mut rows = vec![iced::widget::Space::new().height(30).into()];
            match mode {
                DiffMode::Inline => rows.extend(
                    model
                        .inline_rows
                        .iter()
                        .map(|row| inline_diff_row(row, &model.syntax, 44.0, 1024.0, horizontal)),
                ),
                DiffMode::Split => rows.extend(
                    model
                        .split_rows
                        .iter()
                        .map(|row| split_diff_row(row, &model.syntax, 44.0, 1024.0, horizontal)),
                ),
            }
            let mut element = Element::new(DiffHorizontalContent {
                content: widget::column::with_children(rows).width(480).into(),
                width: Length::Fixed(1504.0),
                content_width: 480.0,
                offset: -horizontal,
                accessible_text: None,
                viewport: Some(iced::Size::new(480.0, 90.0)),
            });
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1504.0, 300.0)),
            );
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node).with_virtual_offset(iced::Vector::new(horizontal, 30.0)),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            let nodes = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .map(|node| node.node())
                .filter(|node| !node.is_hidden())
                .collect::<Vec<_>>();
            for (source, line) in [(before, "Old line 1"), (after, "New line 1")] {
                let code = nodes
                    .iter()
                    .find(|node| node.label() == Some(source))
                    .expect("screen readers can read the exact old/new source");
                assert_eq!(code.role(), Role::Paragraph);
                assert_eq!(code.description(), Some(line));
                let bounds = code.bounds().expect("visible code bounds");
                assert!(bounds.x0 >= 0.0 && bounds.x1 <= 480.0);
                assert!(bounds.y0 >= 0.0 && bounds.y1 <= 90.0);
            }
            let gutters = nodes
                .iter()
                .filter(|node| node.label() == Some("1"))
                .map(|node| node.bounds().expect("visible line number bounds"))
                .collect::<Vec<_>>();
            assert_eq!(gutters.len(), 2, "both native line-number gutters survive");
            if let Some(previous) = &gutter_bounds {
                assert_eq!(
                    &gutters, previous,
                    "horizontal scrolling keeps gutters fixed"
                );
            } else {
                gutter_bounds = Some(gutters);
            }
            let context = nodes
                .iter()
                .filter(|node| node.label() == Some("unchanged"))
                .map(|node| node.description().expect("context line numbers"))
                .collect::<Vec<_>>();
            assert_eq!(
                context,
                match mode {
                    DiffMode::Inline => vec!["Old line 2, new line 2"],
                    DiffMode::Split => vec!["Old line 2", "New line 2"],
                }
            );
        }
    }
}

/// The working-tree comparison is a comparison, so its deleted lines are
/// rows in it like any other basis's.
///
/// It used to mount the live editable buffer instead. A buffer cannot show
/// a line the file no longer contains, so deletions were marked with a
/// rule and relegated to a second pane holding only the selected hunk's,
/// which is why a deletion was almost never on screen. The comparison
/// surface therefore has to lay out exactly what the diff renderer lays
/// out for the same model, on both layouts.
#[test]
fn the_working_comparison_lays_out_its_deleted_lines_as_rows() {
    let card = CardId::new();
    let (mut runtime, _) = working_diff_runtime(card, "gone\nkept\nalso gone\n", "kept\n");
    let renderer = headless_renderer();
    let size = iced::Size::new(720.0, 600.0);
    fn diff_rows(layout: Layout<'_>) -> usize {
        let mut rows = usize::from((layout.bounds().height - DIFF_ROW_HEIGHT).abs() < 0.01);
        for child in layout.children() {
            rows += diff_rows(child);
        }
        rows
    }
    for mode in [DiffMode::Inline, DiffMode::Split] {
        runtime.diff_mode = mode;
        let model = runtime.diff_model().expect("the fixture computed a diff");
        assert_eq!(
            model
                .inline_rows
                .iter()
                .filter(|row| row.kind == DiffRowKind::Deletion)
                .count(),
            2,
            "the fixture has to delete lines for this to mean anything"
        );
        let rendered = laid_out(
            diff_model_view(card, &runtime, model, "Index", "Working buffer", None),
            &renderer,
            size,
        );
        let mounted = laid_out(diff_surface(card, &runtime, None), &renderer, size);
        let rendered = diff_rows(Layout::new(&rendered));
        assert!(
            rendered >= model.row_count(mode),
            "{mode:?} carries one row per model row, got {rendered}"
        );
        assert_eq!(
            diff_rows(Layout::new(&mounted)),
            rendered,
            "{mode:?} must mount the comparison, not the buffer"
        );
    }
}

#[test]
fn parses_status_paths_without_losing_rename_boundaries() {
    let parsed =
        parse_status_paths(b" M src/main.rs\0R  renamed.md\0old.md\0?? new.txt\0").unwrap();
    assert_eq!(
        parsed
            .get(Path::new("src/main.rs"))
            .map(ParsedStatus::display_code),
        Some(" M".into())
    );
    assert_eq!(
        parsed
            .get(Path::new("renamed.md"))
            .map(ParsedStatus::display_code),
        Some("R ".into())
    );
    assert_eq!(
        parsed
            .get(Path::new("new.txt"))
            .map(ParsedStatus::display_code),
        Some("??".into())
    );
    assert_eq!(
        parsed
            .get(Path::new("renamed.md"))
            .and_then(|status| status.staged_old_path.as_ref()),
        Some(&OldPath::Renamed("old.md".into()))
    );
    assert!(!parsed.contains_key(Path::new("old.md")));
}

#[test]
fn rejects_truncated_and_traversing_git_paths() {
    assert!(parse_paths(b"src/main.rs", "fixture").is_err());
    assert!(parse_paths(b"../outside\0", "fixture").is_err());
    assert!(parse_status_paths(b"R  renamed\0").is_err());
}

#[test]
fn merges_staged_deletion_and_untracked_replacement_for_one_path() {
    let parsed = parse_status_paths(b"D  replacement.txt\0?? replacement.txt\0").unwrap();
    let status = parsed.get(Path::new("replacement.txt")).unwrap();
    assert_eq!(status.staged, Some(b'D'));
    assert_eq!(status.unstaged, Some(b'?'));
    assert_eq!(status.display_code(), "D?");

    let root = tempdir().unwrap();
    run_git(root.path(), ["init", "-q"]);
    run_git(root.path(), ["config", "user.name", "Fixture"]);
    run_git(
        root.path(),
        ["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(root.path().join("replacement.txt"), "same path\n").unwrap();
    run_git(root.path(), ["add", "replacement.txt"]);
    run_git(root.path(), ["commit", "-qm", "initial"]);
    run_git(root.path(), ["rm", "--cached", "-q", "replacement.txt"]);
    let files = load_repository_files(root.path()).unwrap();
    let replacements = files
        .iter()
        .filter(|file| file.path == Path::new("replacement.txt"))
        .collect::<Vec<_>>();
    assert_eq!(replacements.len(), 1);
    assert!(replacements[0].staged);
    assert!(replacements[0].unstaged);
    assert_eq!(replacements[0].status.as_deref(), Some("D?"));
}

#[test]
fn worktree_rename_uses_the_old_baseline_and_suppresses_the_source_row() {
    let files = assemble_repository_files(
        b"old-name.md\0",
        b"renamed.md\0",
        b" R renamed.md\0old-name.md\0",
    )
    .unwrap();
    assert_eq!(
        files,
        vec![RepositoryFile {
            path: PathBuf::from("renamed.md"),
            staged_old_path: None,
            unstaged_old_path: Some(OldPath::Renamed("old-name.md".into())),
            staged: false,
            unstaged: true,
            status: Some(" R".into()),
            directory: false,
        }]
    );
}

#[test]
fn real_git_fixture_lists_tracked_and_changed_files() {
    let root = tempdir().unwrap();
    run_git(root.path(), ["init", "-q"]);
    run_git(root.path(), ["config", "user.name", "Fixture"]);
    run_git(
        root.path(),
        ["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(root.path().join("tracked.md"), "# Before\n").unwrap();
    std::fs::write(root.path().join("old-name.md"), "unchanged\n").unwrap();
    run_git(root.path(), ["add", "tracked.md", "old-name.md"]);
    run_git(root.path(), ["commit", "-qm", "initial"]);
    run_git(root.path(), ["mv", "old-name.md", "renamed.md"]);
    std::fs::write(root.path().join("tracked.md"), "# After\n").unwrap();
    std::fs::write(root.path().join("new.txt"), "new\n").unwrap();

    let files = load_repository_files(root.path()).unwrap();
    assert_eq!(
        files,
        vec![
            RepositoryFile {
                path: PathBuf::from("new.txt"),
                staged_old_path: None,
                unstaged_old_path: None,
                staged: false,
                unstaged: true,
                status: Some("??".into()),
                directory: false,
            },
            RepositoryFile {
                path: PathBuf::from("renamed.md"),
                staged_old_path: Some(OldPath::Renamed("old-name.md".into())),
                unstaged_old_path: None,
                staged: true,
                unstaged: false,
                status: Some("R ".into()),
                directory: false,
            },
            RepositoryFile {
                path: PathBuf::from("tracked.md"),
                staged_old_path: None,
                unstaged_old_path: None,
                staged: false,
                unstaged: true,
                status: Some(" M".into()),
                directory: false,
            },
        ]
    );
}

#[test]
fn real_git_fixture_keeps_status_only_and_staged_plus_unstaged_deletions() {
    let root = tempdir().unwrap();
    run_git(root.path(), ["init", "-q"]);
    run_git(root.path(), ["config", "user.name", "Fixture"]);
    run_git(
        root.path(),
        ["config", "user.email", "fixture@example.invalid"],
    );
    for path in ["staged-delete", "unstaged-delete", "both-delete"] {
        std::fs::write(root.path().join(path), "head\n").unwrap();
    }
    run_git(root.path(), ["add", "."]);
    run_git(root.path(), ["commit", "-qm", "initial"]);
    run_git(root.path(), ["rm", "-q", "staged-delete"]);
    std::fs::remove_file(root.path().join("unstaged-delete")).unwrap();
    std::fs::write(root.path().join("both-delete"), "indexed\n").unwrap();
    run_git(root.path(), ["add", "both-delete"]);
    std::fs::remove_file(root.path().join("both-delete")).unwrap();

    let files = load_repository_files(root.path()).unwrap();
    let staged = files
        .iter()
        .find(|file| file.path == Path::new("staged-delete"))
        .expect("a staged deletion absent from the index must remain listed");
    assert_eq!((staged.staged, staged.unstaged), (true, false));
    assert_eq!(staged.status.as_deref(), Some("D "));
    let unstaged = files
        .iter()
        .find(|file| file.path == Path::new("unstaged-delete"))
        .unwrap();
    assert_eq!((unstaged.staged, unstaged.unstaged), (false, true));
    assert_eq!(unstaged.status.as_deref(), Some(" D"));
    let both = files
        .iter()
        .find(|file| file.path == Path::new("both-delete"))
        .unwrap();
    assert_eq!((both.staged, both.unstaged), (true, true));
    assert_eq!(both.status.as_deref(), Some("MD"));
}

#[test]
fn editor_dirty_state_and_markdown_preview_follow_edits() {
    let mut document = EditorDocument::unlinked(
        PathBuf::from("README.md"),
        "# Title\n".into(),
        blake3::hash(b"# Title\n"),
        true,
    );
    assert!(document.markdown_items.is_empty());
    assert!(document.preview_text.is_empty());
    assert!(document.markdown_stale);
    document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
        std::sync::Arc::new("More".into()),
    )));
    assert!(document.dirty);
    assert!(document.markdown_items.is_empty());
    document.sync_markdown();
    assert!(!document.markdown_items.is_empty());
    assert_eq!(document.preview_text, document.content.text());
    assert!(!document.markdown_stale);
    document.discard_edits();
    assert!(!document.dirty);
    assert!(document.active_save.is_none());
    assert_eq!(document.content.text(), "# Title\n");
    assert!(document.markdown_stale);
    document.sync_markdown();
    assert_eq!(document.preview_text, "# Title\n");
    assert_eq!(document.markdown_items.len(), 1);
}

#[test]
fn save_completion_keeps_edits_made_while_the_write_was_in_flight_dirty() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    let card = CardId::new();
    let path = PathBuf::from("notes.txt");
    runtime.generation = 1;
    runtime.selected = Some(RepositoryFile {
        path: path.clone(),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some(" M".into()),
        directory: false,
    });
    let mut document = EditorDocument::unlinked(
        path.clone(),
        "before\n".into(),
        blake3::hash(b"before\n"),
        false,
    );
    document.content = text_editor::Content::with_text("typed later\n");
    document.dirty = true;
    document.revision = 2;
    let identity = AsyncIdentity {
        card,
        worktree: PathBuf::from("/repo"),
        path: path.clone(),
        generation: 1,
    };
    document.active_save = Some(SaveTicket {
        id: 7,
        identity: identity.clone(),
        revision: 1,
        history_revision: if content_matches(&document.content, "submitted\n") {
            document.history.revision()
        } else {
            u64::MAX
        },
        snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("submitted\n")
            .text_snapshot(),
        snapshot: "submitted\n".into(),
    });
    runtime.document = DocumentState::Editable(document);

    let _ = runtime.update(RepositoryAction::SaveFinished {
        identity,
        ticket: 7,
        revision: 1,
        saved_text: "submitted\n".into(),
        result: Ok(SaveOutcome::Saved {
            content_hash: blake3::hash(b"submitted\n"),
            durability: SaveDurability::Durable,
        }),
    });

    let DocumentState::Editable(document) = &runtime.document else {
        panic!("document should remain editable");
    };
    assert_eq!(document.saved_text, "submitted\n");
    assert_eq!(document.content.text(), "typed later\n");
    assert!(document.dirty);
    assert!(document.active_save.is_none());
}

#[test]
fn post_persist_failure_uses_submitted_snapshot_as_baseline_and_fences_edit_back() {
    let card = CardId::new();
    let runtime = RepositoryRuntime::post_persist_warning_fixture(card);

    let DocumentState::Editable(document) = &runtime.document else {
        panic!("document should remain editable");
    };
    assert_eq!(document.saved_text, "B\n");
    assert_eq!(document.disk_hash, blake3::hash(b"B\n"));
    assert_eq!(document.content.text(), "A\n");
    assert_eq!(document.revision, 2);
    assert!(document.dirty);
    assert!(document.active_save.is_none());
    assert!(
        document
            .durability_warning
            .as_deref()
            .is_some_and(|warning| {
                warning.contains("replaced on disk")
                    && warning.contains("Reload from disk or retry")
            })
    );
    assert!(runtime.blocks_navigation());
    assert!(runtime.can_save());
    assert!(runtime.inspector_context().can_reload);
}

#[test]
fn pre_persist_failure_keeps_original_baseline_and_unsaved_buffer_dirty() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::post_persist_failure_fixture(card);
    let DocumentState::Editable(document) = &mut runtime.document else {
        panic!("document should remain editable");
    };
    document.content = text_editor::Content::with_text("B\n");
    document.dirty = true;
    document.revision = 1;
    let ticket = document.active_save.clone().expect("save is in flight");

    let _ = runtime.update(RepositoryAction::SaveFinished {
        identity: ticket.identity,
        ticket: ticket.id,
        revision: ticket.revision,
        saved_text: ticket.snapshot,
        result: Err(FileIoError::Io("injected failure before persist".into())),
    });

    let DocumentState::Editable(document) = &runtime.document else {
        panic!("document should remain editable");
    };
    assert_eq!(document.saved_text, "A\n");
    assert_eq!(document.disk_hash, blake3::hash(b"A\n"));
    assert_eq!(document.content.text(), "B\n");
    assert!(document.dirty);
    assert!(document.durability_warning().is_none());
    assert!(
        document
            .error
            .as_deref()
            .is_some_and(|error| error.contains("failure before persist"))
    );
    assert!(runtime.blocks_navigation());
}

#[test]
fn failed_retry_keeps_post_persist_warning_visible_and_navigation_fenced() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::failed_retry_keeps_post_persist_warning_visible_and_navigation_fenced",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime) = save_warning_fixture(card);
    let _retry = runtime.request_save(card, false);
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("document should remain editable");
    };
    let ticket = document.active_save.clone().expect("retry is in flight");

    let _ = runtime.update(RepositoryAction::SaveFinished {
        identity: ticket.identity,
        ticket: ticket.id,
        revision: ticket.revision,
        saved_text: ticket.snapshot,
        result: Err(FileIoError::Io(
            "injected retry failure before persist".into(),
        )),
    });

    let message = runtime.save_error().expect("combined warning is visible");
    assert!(message.contains("retry failure before persist"));
    assert!(message.contains("replaced on disk"));
    assert!(message.contains("Reload from disk or retry"));
    assert!(runtime.blocks_navigation());
    assert!(runtime.can_save());
    let context = runtime.inspector_context();
    assert_eq!(context.document, "Durability warning");
    assert!(
        context
            .document_error
            .as_deref()
            .is_some_and(|message| message.contains("retry failure before persist")
                && message.contains("replaced on disk"))
    );
    assert!(context.can_reload);
    assert!(context.can_save);
}

/// A real file read without opening any save/recovery authority.
fn reload_read_fixture(card: CardId) -> (tempfile::TempDir, RepositoryRuntime, AsyncIdentity) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("notes.txt"),
        "original line\nsecond line\n",
    )
    .unwrap();
    let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
        project: ProjectId::new(),
        repository: directory.path().to_owned(),
        worktree: directory.path().to_owned(),
        selected_file: Some("notes.txt".into()),
    });
    drop(runtime.initial_load(card));
    let identity = runtime.current_identity(card, "notes.txt".into());
    finish_reload_read(&mut runtime, identity.clone());
    assert!(runtime.original_read_attested);
    drop(runtime.take_lsp_intents());
    (directory, runtime, identity)
}

fn finish_reload_read(runtime: &mut RepositoryRuntime, identity: AsyncIdentity) {
    let result = crate::editor::read_file_at(
        &identity.worktree,
        &identity.path,
        crate::editor::PdfTone::Original,
        runtime.original_editor_root.clone(),
    );
    drop(runtime.update(RepositoryAction::FileLoadedAt { identity, result }));
}

#[test]
fn failed_or_noneditable_reload_keeps_unsaved_text_selection_and_history() {
    for replacement in ["deleted", "binary", "invalid utf8", "oversize"] {
        let card = CardId::new();
        let (directory, mut runtime, stale) = reload_read_fixture(card);
        move_to_start(&mut runtime, card);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        let edited = runtime.editor_text().unwrap();
        let DocumentState::Editable(document) = &mut runtime.document else {
            panic!()
        };
        // The conflict state exposes the ordinary Reload control.
        let conflict = replacement != "invalid utf8";
        document.conflict = conflict;
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 5 },
            selection: Some(text_editor::Position { line: 0, column: 1 }),
        });
        let cursor = document.content.cursor();
        let revision = document.revision;
        let version = document.lsp_version;
        let saved = document.saved_text.clone();
        let hash = document.disk_hash;
        assert!(document.history.can_undo());
        assert!(!document.history.can_redo());
        drop(runtime.take_lsp_intents());
        let stale_read = crate::editor::read_file_at(
            directory.path(),
            Path::new("notes.txt"),
            crate::editor::PdfTone::Original,
            runtime.original_editor_root.clone(),
        )
        .unwrap();
        let path = directory.path().join("notes.txt");
        match replacement {
            "deleted" => std::fs::remove_file(&path).unwrap(),
            "binary" => std::fs::write(&path, b"whole\0binary").unwrap(),
            "invalid utf8" => std::fs::write(&path, b"valid prefix\xff").unwrap(),
            "oversize" => std::fs::File::create(&path)
                .unwrap()
                .set_len(crate::editor::MAX_EDITABLE_FILE_BYTES + 1)
                .unwrap(),
            _ => unreachable!(),
        }
        drop(runtime.update(RepositoryAction::Reload { card }));
        let pending = runtime.current_identity(card, "notes.txt".into());
        assert_ne!(pending.generation, stale.generation);
        assert!(runtime.is_dirty() && runtime.blocks_navigation());
        assert!(!runtime.can_abandon_unsaved_changes());
        assert!(!runtime.can_save() && !runtime.can_discard_edits());
        assert!(runtime.editor_focus_target(card).is_none());
        assert_eq!(runtime.take_lsp_intents(), vec![LspIntent::Close]);

        // Repeat Reload, navigation, typing and history callbacks may not
        // replace or mutate the predecessor while its read is pending.
        for action in [
            RepositoryAction::Reload { card },
            RepositoryAction::OpenFile {
                card,
                path: "other.txt".into(),
            },
            RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Edit(text_editor::Edit::Insert('q')),
            },
            RepositoryAction::Undo { card },
            RepositoryAction::DiscardEdits { card },
        ] {
            drop(runtime.update(action));
        }
        assert_eq!(runtime.current_identity(card, "notes.txt".into()), pending);
        let DocumentState::Loading {
            predecessor: Some(document),
        } = &runtime.document
        else {
            panic!("the same unsaved buffer must remain retained")
        };
        assert_eq!(document.content.text(), edited);
        assert_eq!(document.content.cursor(), cursor);
        assert_eq!(document.revision, revision);

        // A late old-document callback cannot steal the retained buffer.
        drop(runtime.update(RepositoryAction::FileLoadedAt {
            identity: stale,
            result: Ok(stale_read),
        }));
        assert!(!runtime.original_read_attested);
        assert!(runtime.is_dirty() && runtime.blocks_navigation());
        drop(runtime.preview_fragment(card, "L2:6"));
        assert!(runtime.pending_fragment.is_some());
        finish_reload_read(&mut runtime, pending.clone());
        assert!(
            !runtime.original_read_attested,
            "rejected disk bytes cannot attest the old text: {replacement}"
        );
        assert_eq!(runtime.current_identity(card, "notes.txt".into()), pending);
        assert_eq!(runtime.selected_path(), Some(Path::new("notes.txt")));
        assert!(runtime.pending_fragment.is_none());
        assert!(runtime.pending_editor_navigation.is_none());
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("failed {replacement} reload must keep the editor")
        };
        assert_eq!(document.content.text(), edited, "{replacement}");
        assert_eq!(document.content.cursor(), cursor, "{replacement}");
        assert_eq!(
            (document.revision, document.lsp_version),
            (revision, version)
        );
        assert_eq!(document.saved_text, saved);
        assert_eq!(document.disk_hash, hash);
        assert_eq!(document.conflict, conflict);
        assert!(document.dirty);
        assert!(document.history.can_undo() && !document.history.can_redo());
        let detail = runtime.save_error().unwrap();
        assert!(detail.contains("your edits were kept"), "{detail}");
        let expected = match replacement {
            "deleted" => "on this device",
            "binary" => "binary data",
            "invalid utf8" => "not valid UTF-8",
            "oversize" => "editable files are limited",
            _ => unreachable!(),
        };
        assert!(detail.contains(expected), "{replacement}: {detail}");
        assert!(runtime.is_dirty() && runtime.blocks_navigation());
        assert!(
            matches!(runtime.take_lsp_intents().as_slice(), [LspIntent::Open { text, version: open_version, .. }] if text == &LspChangeText::from(edited.as_str()) && *open_version == version)
        );
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(
            runtime.editor_text().as_deref(),
            Some("original line\nsecond line\n")
        );
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(runtime.editor_text().as_deref(), Some(edited.as_str()));
    }
}

#[test]
fn rejected_reload_preserves_foreground_focus_until_the_editor_is_visible() {
    use cosmic::iced::advanced::widget::operation;
    use cosmic::iced::futures::StreamExt;

    struct FocusProbe(widget::Id, bool);
    impl Operation for FocusProbe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }

        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if id == Some(&self.0) {
                self.1 = state.is_focused();
            }
        }
    }

    fn apply_widgets<Message: 'static>(
        task: Task<Message>,
        element: &mut Element<'_, ()>,
        tree: &mut Tree,
        node: &layout::Node,
        renderer: &cosmic::Renderer,
    ) {
        let executor = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        executor.block_on(async {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    let iced::runtime::Action::Widget(mut operation) = action else {
                        panic!("restoring local scroll/focus must not dispatch other work");
                    };
                    loop {
                        element.as_widget_mut().operate(
                            tree,
                            Layout::new(node),
                            renderer,
                            operation.as_mut(),
                        );
                        match operation.finish() {
                            operation::Outcome::Chain(next) => operation = next,
                            operation::Outcome::None | operation::Outcome::Some(()) => break,
                        }
                    }
                }
            }
        });
    }

    let card = CardId::new();
    let (directory, mut runtime, _) = reload_read_fixture(card);
    move_to_start(&mut runtime, card);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    let edited = runtime.editor_text().unwrap();
    let cursor = runtime.cursor_position();
    std::fs::remove_file(directory.path().join("notes.txt")).unwrap();
    drop(runtime.update(RepositoryAction::Reload { card }));
    let identity = runtime.current_identity(card, "notes.txt".into());
    let result = crate::editor::read_file_at(
        &identity.worktree,
        &identity.path,
        crate::editor::PdfTone::Original,
        runtime.original_editor_root.clone(),
    );
    assert!(result.as_ref().unwrap().clone().into_parts().1.is_err());

    // The editor is not mounted while a Work form or dialog owns input.
    // Apply the real accepted-read task to that foreground native widget:
    // focusing an absent editor would unfocus this field too.
    let renderer = headless_renderer();
    let foreground = widget::Id::unique();
    let mut element: Element<'_, ()> = iced::widget::text_input("PR URL", "unsent value")
        .id(foreground.clone())
        .on_input(|_| ())
        .into();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(480.0, 120.0)),
    );
    apply_widgets(
        iced::widget::operation::focus::<()>(foreground.clone()),
        &mut element,
        &mut tree,
        &node,
        &renderer,
    );
    let mut before = FocusProbe(foreground.clone(), false);
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
    assert!(
        before.1,
        "the foreground field must actually own native focus"
    );

    let completion = runtime.update(RepositoryAction::FileLoadedAt { identity, result });
    let target = runtime.loaded_focus_target(card).unwrap();
    apply_widgets(
        Task::batch([
            completion,
            crate::workspace_widget::focus_after_loading(editor_focus_id(card), target),
        ]),
        &mut element,
        &mut tree,
        &node,
        &renderer,
    );
    let mut after = FocusProbe(foreground, false);
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
    assert!(
        after.1,
        "a hidden failed Reload must preserve foreground focus"
    );
    assert_eq!(runtime.editor_text().as_deref(), Some(edited.as_str()));
    assert_eq!(runtime.cursor_position(), cursor);

    // The shell can deliver the retained request once this editor really
    // owns input again; it is not consumed by the hidden completion task.
    let target = runtime.take_restored_editor_focus(card).unwrap();
    assert!(runtime.take_restored_editor_focus(card).is_none());
    let mut editor: Element<'_, ()> = editor_surface(card, &runtime).map(|_| ());
    let mut editor_tree = Tree::new(editor.as_widget());
    let editor_node = editor.as_widget_mut().layout(
        &mut editor_tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, iced::Size::new(480.0, 240.0)),
    );
    apply_widgets(
        crate::workspace_widget::focus_after_loading::<()>(editor_focus_id(card), target.clone()),
        &mut editor,
        &mut editor_tree,
        &editor_node,
        &renderer,
    );
    let mut visible = FocusProbe(target, false);
    editor.as_widget_mut().operate(
        &mut editor_tree,
        Layout::new(&editor_node),
        &renderer,
        &mut visible,
    );
    assert!(
        visible.1,
        "the visible editor must still accept its focus handoff"
    );
}

#[test]
fn successful_reload_still_replaces_clean_and_explicitly_reloaded_dirty_documents() {
    for dirty in [false, true] {
        let card = CardId::new();
        let (directory, mut runtime, stale) = reload_read_fixture(card);
        if dirty {
            move_to_start(&mut runtime, card);
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
            }));
        }
        std::fs::write(directory.path().join("notes.txt"), "replacement text\n").unwrap();
        drop(runtime.update(RepositoryAction::Reload { card }));
        let current = runtime.current_identity(card, "notes.txt".into());
        assert_ne!(current.generation, stale.generation);
        assert_eq!(runtime.is_dirty(), dirty);
        // A result for another selected path with this generation is refused too.
        let mut foreign = current.clone();
        foreign.path = "other.txt".into();
        drop(runtime.update(RepositoryAction::FileLoaded {
            identity: foreign,
            result: Err(FileIoError::Io("unrelated".into())),
        }));
        assert!(matches!(runtime.document, DocumentState::Loading { .. }));
        finish_reload_read(&mut runtime, current);
        assert_eq!(runtime.editor_text().as_deref(), Some("replacement text\n"));
        assert!(runtime.original_read_attested);
        assert!(!runtime.is_dirty() && !runtime.blocks_navigation());
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert!(!document.history.can_undo() && !document.history.can_redo());
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(runtime.editor_text().as_deref(), Some("replacement text\n"));
        assert_eq!(
            std::fs::read_to_string(directory.path().join("notes.txt")).unwrap(),
            "replacement text\n"
        );
    }
}

#[test]
fn failed_reload_releases_obsolete_diagram_work_without_forgetting_the_buffer() {
    let card = CardId::new();
    let (directory, mut runtime, stale) = reload_read_fixture(card);
    let DocumentState::Editable(document) = &mut runtime.document else {
        panic!()
    };
    document.markdown = true;
    document.conflict = true;
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::SelectAll,
    }));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
            "```mermaid\nflowchart LR; A-->B\n```\n".into(),
        ))),
    }));
    runtime.markdown_mode = MarkdownMode::Preview;
    drop(runtime.diagram_task(card));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    let revision = document.revision;
    let tone_generation = document.diagram_tone_generation;
    let cancel = document.diagram_cancel.as_ref().unwrap().0.clone();
    assert!(document.diagram_pending.is_some());
    std::fs::remove_file(directory.path().join("notes.txt")).unwrap();
    drop(runtime.update(RepositoryAction::Reload { card }));
    assert!(cancel.is_cancelled());
    let current = runtime.current_identity(card, "notes.txt".into());
    finish_reload_read(&mut runtime, current);
    drop(runtime.update(RepositoryAction::DiagramsRendered {
        identity: stale,
        revision,
        tone_generation,
        result: vec![(
            blake3::hash(b"flowchart LR; A-->B"),
            preview::Diagram::Error("obsolete".into()),
        )],
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert_eq!(document.diagram_pending, None);
    assert!(document.diagram_cancel.is_none() && document.diagrams.is_empty());
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!()
    };
    assert_eq!(document.diagram_pending, Some(revision));
    assert!(!document.diagram_cancel.as_ref().unwrap().0.is_cancelled());
    assert!(document.content.text().contains("flowchart LR; A-->B"));
    assert!(document.history.can_undo());
}

#[test]
fn reload_and_overwrite_resolve_post_persist_durability_warning_explicitly() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::reload_and_overwrite_resolve_post_persist_durability_warning_explicitly",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut refused) = save_warning_fixture(card);
    let DocumentState::Editable(document) = &mut refused.document else {
        panic!()
    };
    document.dirty = false;
    let text = document.content.text();
    let warning = document.durability_warning().unwrap().to_owned();
    drop(refused.update(RepositoryAction::Reload { card }));
    assert!(matches!(&refused.document,
        DocumentState::Loading { predecessor: Some(document) }
            if document.durability_warning() == Some(warning.as_str())));
    let identity = refused.current_identity(card, "notes.txt".into());
    drop(refused.update(RepositoryAction::FileLoaded {
        identity,
        result: Err(FileIoError::Io("read refused after Reload began".into())),
    }));
    assert_eq!(refused.editor_text().as_deref(), Some(text.as_str()));
    assert!(!refused.is_dirty() && refused.blocks_navigation());
    let DocumentState::Editable(document) = &refused.document else {
        panic!()
    };
    assert_eq!(document.durability_warning(), Some(warning.as_str()));
    assert!(
        document
            .error
            .as_deref()
            .unwrap()
            .contains("your edits were kept")
    );

    let mut reloaded = RepositoryRuntime::post_persist_warning_fixture(card);
    let _reload = reloaded.update(RepositoryAction::Reload { card });
    let identity = reloaded.current_identity(card, PathBuf::from("notes.txt"));
    let _ = reloaded.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: "notes.txt".into(),
            content: FileContent::Editable {
                text: "B\n".into(),
                content_hash: blake3::hash(b"B\n"),
                markdown: false,
            },
        }),
    });
    assert!(!reloaded.blocks_navigation());
    assert!(reloaded.save_error().is_none());

    let (_directory, mut overwritten) = save_warning_fixture(card);
    let _overwrite = overwritten.request_save(card, true);
    let DocumentState::Editable(document) = &overwritten.document else {
        panic!("document should remain editable");
    };
    let ticket = document.active_save.clone().expect("overwrite started");
    let _ = overwritten.update(RepositoryAction::SaveFinished {
        identity: ticket.identity,
        ticket: ticket.id,
        revision: ticket.revision,
        saved_text: ticket.snapshot,
        result: Ok(SaveOutcome::Saved {
            content_hash: blake3::hash(b"A\n"),
            durability: SaveDurability::Durable,
        }),
    });
    assert!(!overwritten.blocks_navigation());
    assert!(overwritten.save_error().is_none());
}

#[test]
fn obsolete_save_completion_cannot_change_document_or_release_navigation_fence() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    let card = CardId::new();
    let path = PathBuf::from("notes.txt");
    runtime.generation = 1;
    runtime.selected = Some(RepositoryFile {
        path: path.clone(),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some(" M".into()),
        directory: false,
    });
    let mut document = EditorDocument::unlinked(
        path.clone(),
        "disk\n".into(),
        blake3::hash(b"disk\n"),
        false,
    );
    document.content = text_editor::Content::with_text("newest\n");
    document.dirty = true;
    document.revision = 3;
    let identity = AsyncIdentity {
        card,
        worktree: PathBuf::from("/repo"),
        path: path.clone(),
        generation: 1,
    };
    document.active_save = Some(SaveTicket {
        id: 8,
        identity: identity.clone(),
        revision: 3,
        history_revision: if content_matches(&document.content, "newest\n") {
            document.history.revision()
        } else {
            u64::MAX
        },
        snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("newest\n")
            .text_snapshot(),
        snapshot: "newest\n".into(),
    });
    runtime.document = DocumentState::Editable(document);

    let _ = runtime.update(RepositoryAction::SaveFinished {
        identity,
        ticket: 7,
        revision: 2,
        saved_text: "obsolete\n".into(),
        result: Ok(SaveOutcome::Saved {
            content_hash: blake3::hash(b"obsolete\n"),
            durability: SaveDurability::Durable,
        }),
    });

    let DocumentState::Editable(document) = &runtime.document else {
        panic!("document should remain editable");
    };
    assert_eq!(document.saved_text, "disk\n");
    assert_eq!(document.content.text(), "newest\n");
    assert!(document.active_save.is_none());
    assert!(document.conflict);
    assert!(
        document
            .error
            .as_deref()
            .is_some_and(|error| error.contains("disk state is unknown"))
    );
    assert!(runtime.blocks_navigation());
}

#[test]
fn save_in_flight_blocks_navigation_even_when_text_temporarily_matches_old_disk() {
    let mut document = EditorDocument::unlinked(
        PathBuf::from("notes.txt"),
        "disk\n".into(),
        blake3::hash(b"disk\n"),
        false,
    );
    document.revision = 2;
    document.active_save = Some(SaveTicket {
        id: 1,
        identity: AsyncIdentity {
            card: CardId::new(),
            worktree: PathBuf::from("/repo"),
            path: PathBuf::from("notes.txt"),
            generation: 1,
        },
        revision: 1,
        history_revision: if content_matches(&document.content, "submitted\n") {
            document.history.revision()
        } else {
            u64::MAX
        },
        snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("submitted\n")
            .text_snapshot(),
        snapshot: "submitted\n".into(),
    });
    document.dirty = false;
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    runtime.document = DocumentState::Editable(document);
    assert!(!runtime.is_dirty());
    assert!(runtime.blocks_navigation());
}

#[test]
fn active_save_rejects_open_reload_and_discard_transitions() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    let card = CardId::new();
    let path = PathBuf::from("notes.txt");
    runtime.generation = 4;
    runtime.selected = Some(RepositoryFile {
        path: path.clone(),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some(" M".into()),
        directory: false,
    });
    let identity = AsyncIdentity {
        card,
        worktree: PathBuf::from("/repo"),
        path: path.clone(),
        generation: 4,
    };
    let mut document = EditorDocument::unlinked(
        path.clone(),
        "disk\n".into(),
        blake3::hash(b"disk\n"),
        false,
    );
    document.content = text_editor::Content::with_text("submitted\n");
    document.dirty = true;
    document.revision = 1;
    document.active_save = Some(SaveTicket {
        id: 9,
        identity,
        revision: 1,
        history_revision: if content_matches(&document.content, "submitted\n") {
            document.history.revision()
        } else {
            u64::MAX
        },
        snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("submitted\n")
            .text_snapshot(),
        snapshot: "submitted\n".into(),
    });
    runtime.document = DocumentState::Editable(document);

    for action in [
        RepositoryAction::DiscardEdits { card },
        RepositoryAction::Reload { card },
        RepositoryAction::OpenFile {
            card,
            path: PathBuf::from("other.txt"),
        },
        RepositoryAction::ApplySourceSurface { card, diff: true },
        RepositoryAction::ApplyMarkdownMode {
            card,
            preview: true,
        },
    ] {
        let _ = runtime.update(action);
    }

    assert_eq!(runtime.generation, 4);
    assert_eq!(runtime.selected.as_ref().unwrap().path, path);
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("the active save document must remain installed");
    };
    assert_eq!(document.content.text(), "submitted\n");
    assert_eq!(
        document.active_save.as_ref().map(|ticket| ticket.id),
        Some(9)
    );
    assert!(runtime.blocks_navigation());
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
    assert_eq!(runtime.markdown_mode, MarkdownMode::Edit);
}

#[test]
fn mutation_refresh_reconciles_staging_without_obsoleting_document_loads() {
    let card = CardId::new();
    let (mut runtime, identity) = RepositoryRuntime::lsp_selection_fixture(card);
    runtime.source_surface = SourceSurface::Diff;
    let old_request = runtime.next_diff_request;
    let _ = runtime.refresh_after_mutation(card);
    let mut staged = runtime.selected.clone().unwrap();
    staged.staged = true;
    staged.unstaged = false;
    staged.status = Some("M ".into());
    let _ = runtime.update(RepositoryAction::FilesLoaded {
        identity: AsyncIdentity {
            card,
            worktree: identity.worktree.clone(),
            path: PathBuf::from("."),
            generation: runtime.file_list_generation,
        },
        result: Ok(vec![staged]),
    });
    let _ = runtime.update(RepositoryAction::FileLoaded {
        identity: identity.clone(),
        result: Ok(LoadedFile {
            source: identity.path.clone(),
            content: FileContent::Editable {
                text: "loaded\n".into(),
                content_hash: blake3::hash(b"loaded\n"),
                markdown: false,
            },
        }),
    });
    let _ = runtime.update(RepositoryAction::DiffLoaded {
        identity,
        basis: DiffBasis::IndexToWorktree,
        request: old_request,
        content: RenderedDiff::Error("obsolete diff".into()),
    });
    let context = runtime.inspector_context();
    assert!(context.has_staged);
    assert!(!context.has_unstaged);
    assert_eq!(runtime.diff_basis, DiffBasis::HeadToIndex);
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
    assert!(matches!(
        runtime.diff_state(&DiffBasis::HeadToIndex),
        Some(DiffState::Loading { .. })
    ));
    assert!(matches!(
        &runtime.document,
        DocumentState::Editable(document) if document.content.text() == "loaded\n"
    ));
}

#[test]
fn mutation_refresh_keeps_dirty_buffer_save_and_pinned_commit_when_path_disappears() {
    let card = CardId::new();
    let (mut runtime, completion) = RepositoryRuntime::post_persist_failure_fixture(card);
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    document.replace_text("still editing\n");
    let commit = DiffBasis::CommitToParent {
        oid: fixture_oid(1),
        parent: Some(fixture_oid(2)),
    };
    runtime.diff_basis = commit.clone();
    runtime.source_surface = SourceSurface::Diff;
    runtime.set_diff_state(
        commit.clone(),
        DiffState::ready(staged_text_diff(model_with(4))),
    );
    let _ = runtime.refresh_after_mutation(card);
    let _ = runtime.update(RepositoryAction::FilesLoaded {
        identity: AsyncIdentity {
            card,
            worktree: runtime.descriptor.worktree.clone(),
            path: PathBuf::from("."),
            generation: runtime.file_list_generation,
        },
        result: Ok(Vec::new()),
    });
    let context = runtime.inspector_context();
    assert!(!context.has_staged && !context.has_unstaged);
    assert!(context.dirty && context.saving);
    assert_eq!(runtime.diff_basis, commit);
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert_eq!(context.diff_additions, Some(4));
    let _ = runtime.update(completion);
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("mutation refresh must retain the edited buffer");
    };
    assert_eq!(document.content.text(), "still editing\n");
    assert_eq!(document.saved_text, "B\n");
    assert!(document.dirty);
    assert!(document.active_save.is_none());
    assert!(!document.conflict);
}

#[test]
fn save_completion_supersedes_status_read_started_before_it() {
    let card = CardId::new();
    let (mut runtime, completion) = RepositoryRuntime::post_persist_failure_fixture(card);
    let _ = runtime.refresh_after_mutation(card);
    let stale_identity = AsyncIdentity {
        card,
        worktree: runtime.descriptor.worktree.clone(),
        path: PathBuf::from("."),
        generation: runtime.file_list_generation,
    };
    let _ = runtime.update(completion);
    let _ = runtime.update(RepositoryAction::FilesLoaded {
        identity: stale_identity,
        result: Ok(Vec::new()),
    });
    assert!(runtime.mutation_refresh_pending);
    let mut changed = runtime.selected.clone().unwrap();
    changed.unstaged = true;
    changed.status = Some(" M".into());
    let _ = runtime.update(RepositoryAction::FilesLoaded {
        identity: AsyncIdentity {
            card,
            worktree: runtime.descriptor.worktree.clone(),
            path: PathBuf::from("."),
            generation: runtime.file_list_generation,
        },
        result: Ok(vec![changed]),
    });
    assert!(runtime.inspector_context().has_unstaged);
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
    assert!(!runtime.is_saving());
}

#[test]
fn file_list_refresh_does_not_obsolete_a_pending_document_load() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    let card = CardId::new();
    let path = PathBuf::from("notes.md");
    runtime.generation = 4;
    runtime.selected = Some(RepositoryFile {
        path: path.clone(),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some(" M".into()),
        directory: false,
    });
    runtime.document = DocumentState::Loading { predecessor: None };

    let _refresh = runtime.update(RepositoryAction::RefreshFiles { card });
    assert_eq!(runtime.generation, 4);
    assert_eq!(runtime.file_list_generation, 1);
    let _ = runtime.update(RepositoryAction::FileLoaded {
        identity: AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 4,
        },
        result: Ok(LoadedFile {
            source: path.clone(),
            content: FileContent::Editable {
                text: "loaded\n".into(),
                content_hash: blake3::hash(b"loaded\n"),
                markdown: true,
            },
        }),
    });
    let _ = runtime.update(RepositoryAction::FilesLoaded {
        identity: AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: PathBuf::from("."),
            generation: 1,
        },
        result: Ok(vec![RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: true,
            unstaged: false,
            status: Some("M ".into()),
            directory: false,
        }]),
    });

    assert!(matches!(
        runtime.document,
        DocumentState::Editable(ref document) if document.path == path
    ));
    assert!(
        runtime
            .selected
            .as_ref()
            .is_some_and(|selected| !selected.staged && selected.unstaged)
    );
    assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
}

#[test]
fn file_list_refresh_does_not_obsolete_an_active_save() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    let card = CardId::new();
    let path = PathBuf::from("notes.txt");
    runtime.generation = 7;
    runtime.selected = Some(RepositoryFile {
        path: path.clone(),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some(" M".into()),
        directory: false,
    });
    let identity = AsyncIdentity {
        card,
        worktree: PathBuf::from("/repo"),
        path: path.clone(),
        generation: 7,
    };
    let mut document =
        EditorDocument::unlinked(path, "before\n".into(), blake3::hash(b"before\n"), false);
    document.content = text_editor::Content::with_text("after\n");
    document.dirty = true;
    document.revision = 1;
    document.active_save = Some(SaveTicket {
        id: 2,
        identity: identity.clone(),
        revision: 1,
        history_revision: if content_matches(&document.content, "after\n") {
            document.history.revision()
        } else {
            u64::MAX
        },
        snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("after\n")
            .text_snapshot(),
        snapshot: "after\n".into(),
    });
    runtime.document = DocumentState::Editable(document);

    let _refresh = runtime.update(RepositoryAction::RefreshFiles { card });
    let _ = runtime.update(RepositoryAction::SaveFinished {
        identity,
        ticket: 2,
        revision: 1,
        saved_text: "after\n".into(),
        result: Ok(SaveOutcome::Saved {
            content_hash: blake3::hash(b"after\n"),
            durability: SaveDurability::Durable,
        }),
    });

    assert_eq!(runtime.generation, 7);
    assert_eq!(runtime.file_list_generation, 1);
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("the saved document must remain installed");
    };
    assert!(!document.dirty);
    assert!(document.active_save.is_none());
    assert!(!document.conflict);
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
}

#[test]
fn starting_a_save_preserves_the_diff_review_surface() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::starting_a_save_preserves_the_diff_review_surface",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, _) = save_fixture(card, "notes.md", "disk\n");
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!()
    };
    document.replace_text("changed\n");
    runtime.source_surface = SourceSurface::Diff;
    runtime.markdown_mode = MarkdownMode::Preview;

    let _save = runtime.request_save(card, false);

    assert!(runtime.is_saving());
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
}

#[test]
fn autosave_ignores_stale_edits_and_preserves_the_current_surface() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::autosave_ignores_stale_edits_and_preserves_the_current_surface",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, identity) = save_fixture(card, "notes.txt", "disk\n");
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    document.replace_text("first\n");
    let revision = document.revision;
    drop(runtime.auto_save_task(card));
    let obsolete = RepositoryAction::AutoSaveDue {
        identity: identity.clone(),
        revision,
        ticket: runtime.next_auto_save_ticket,
    };
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    document.replace_text("latest\n");
    let revision = document.revision;
    drop(runtime.auto_save_task(card));
    drop(runtime.update(obsolete));
    assert!(!runtime.is_saving());

    runtime.source_surface = SourceSurface::Diff;
    runtime.markdown_mode = MarkdownMode::Preview;
    drop(runtime.update(RepositoryAction::AutoSaveDue {
        identity,
        revision,
        ticket: runtime.next_auto_save_ticket,
    }));
    assert!(runtime.is_saving());
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
    drop(runtime.update(runtime.durable_active_save_completion()));
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    assert_eq!(document.saved_text, "latest\n");
    assert!(!runtime.is_dirty());
}

#[tokio::test]
async fn autosave_preserves_a_selection_replacement_typing_group_but_explicit_save_seals_it() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::autosave_preserves_a_selection_replacement_typing_group_but_explicit_save_seals_it",
    ) {
        return;
    }
    const ORIGINAL: &str =
        "// fixture header\npub fn target() -> i32 { 42 }\n\npub fn caller() -> i32 { target() }\n";
    // Some(true) autosaves mid-run; Some(false) explicitly saves;
    // None covers the native short burst followed only by explicit Save.
    for midway_save in [Some(true), Some(false), None] {
        let directory = Checkout::new();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("find.rs");
        std::fs::write(&path, ORIGINAL).unwrap();
        let card = CardId::new();
        let view = RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: root.clone(),
            worktree: root.clone(),
        };
        let mut runtime = RepositoryRuntime::new_directory(view);
        let task = runtime.update(RepositoryAction::OpenFile {
            card,
            path: "find.rs".into(),
        });
        finish_navigation_task(&mut runtime, task).await;
        assert!(runtime.configure_save_recovery(Default::default(), vec![root]));
        let now = Instant::now();
        let DocumentState::Editable(document) = &mut runtime.document else {
            panic!("native editable fixture");
        };
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position {
                line: 1,
                column: 13,
            },
            selection: Some(text_editor::Position { line: 1, column: 7 }),
        });
        document.history_time = Some(now);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('c')),
        }));
        let first = ORIGINAL.replacen("target", "c", 1);
        assert_eq!(buffer_text(&runtime), first);

        let save = if midway_save == Some(true) {
            // Deliver the accepted 800 ms timer through its real reducer;
            // the following input is still within the 1,000 ms history
            // group. The injected edit clock avoids scheduler-dependent
            // grouping and no test sleeps or background timer runs.
            drop(runtime.auto_save_task(card));
            let selected = runtime.selected.as_ref().unwrap();
            let identity = runtime.current_identity(card, selected.path.clone());
            runtime.update(RepositoryAction::AutoSaveDue {
                identity,
                revision: document_revision(&runtime),
                ticket: runtime.next_auto_save_ticket,
            })
        } else if midway_save == Some(false) {
            runtime.update(RepositoryAction::Save {
                card,
                overwrite: false,
            })
        } else {
            Task::none()
        };
        finish_navigation_task(&mut runtime, save).await;
        if midway_save.is_some() {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
            assert!(!runtime.is_dirty());
        }

        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.history_time =
            Some(now + Duration::from_millis(if midway_save.is_some() { 900 } else { 28 }));
        for character in "hanged".chars() {
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Edit(text_editor::Edit::Insert(character)),
            }));
        }
        let changed = ORIGINAL.replacen("target", "changed", 1);
        assert_eq!(buffer_text(&runtime), changed);
        assert!(runtime.is_dirty());
        let save = runtime.update(RepositoryAction::Save {
            card,
            overwrite: false,
        });
        finish_navigation_task(&mut runtime, save).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);

        drop(runtime.update(RepositoryAction::Undo { card }));
        let expected = if midway_save == Some(false) {
            &first
        } else {
            ORIGINAL
        };
        assert_eq!(buffer_text(&runtime), expected);
        let save = runtime.update(RepositoryAction::Save {
            card,
            overwrite: false,
        });
        finish_navigation_task(&mut runtime, save).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert!(!runtime.is_dirty());
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(buffer_text(&runtime), changed);
        assert!(runtime.is_dirty());
    }
}

#[test]
fn explicit_save_seals_typing_when_autosave_is_clean_or_in_flight() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::explicit_save_seals_typing_when_autosave_is_clean_or_in_flight",
    ) {
        return;
    }
    use cosmic::iced::futures::{FutureExt, StreamExt};

    for complete_before_explicit_save in [true, false] {
        let card = CardId::new();
        let (_directory, mut runtime, identity) = save_fixture(card, "notes.txt", "target\n");
        let now = Instant::now();
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 6 },
            selection: Some(text_editor::Position { line: 0, column: 0 }),
        });
        document.history_time = Some(now);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('c')),
        }));
        drop(runtime.auto_save_task(card));
        drop(runtime.update(RepositoryAction::AutoSaveDue {
            identity,
            revision: document_revision(&runtime),
            ticket: runtime.next_auto_save_ticket,
        }));
        assert!(runtime.is_saving());
        let completion = runtime.durable_active_save_completion();
        if complete_before_explicit_save {
            drop(runtime.update(completion.clone()));
            assert!(!runtime.is_dirty());
        }
        // The explicit gesture cannot start another write in either
        // state, but must still separate the user's next typing group.
        let next_save_ticket = runtime.next_save_ticket;
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        let active_save = document.active_save.clone();
        let task = runtime.update(RepositoryAction::Save {
            card,
            overwrite: false,
        });
        assert_eq!(runtime.next_save_ticket, next_save_ticket);
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert_eq!(document.active_save, active_save);
        // update batches its inner task with Find maintenance. Iced can
        // represent no work as Some(empty stream), so check the actual
        // contract instead of requiring a particular Option encoding.
        // The attested fixture retains its private checkout; neither state
        // starts another write.
        assert_eq!(task.units(), 0);
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            assert!(matches!(stream.next().now_or_never(), Some(None)));
        }
        if !complete_before_explicit_save {
            drop(runtime.update(completion));
        }
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.history_time = Some(now + Duration::from_millis(900));
        for character in "hanged".chars() {
            drop(runtime.update(RepositoryAction::EditorAction {
                card,
                action: text_editor::Action::Edit(text_editor::Edit::Insert(character)),
            }));
        }
        assert_eq!(buffer_text(&runtime), "changed\n");
        assert!(runtime.is_dirty());
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), "c\n");
        assert!(!runtime.is_dirty());
    }
}

#[test]
fn disabling_autosave_invalidates_an_already_scheduled_save() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::disabling_autosave_invalidates_an_already_scheduled_save",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, identity) = save_fixture(card, "notes.txt", "disk\n");
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    document.replace_text("unsaved\n");
    let revision = document.revision;
    drop(runtime.auto_save_task(card));
    let due = RepositoryAction::AutoSaveDue {
        identity,
        revision,
        ticket: runtime.next_auto_save_ticket,
    };
    runtime.set_auto_save(false);
    runtime.set_auto_save(true);
    drop(runtime.auto_save_task(card));
    drop(runtime.update(due));
    assert!(!runtime.is_saving());
    assert!(runtime.is_dirty());
}

#[test]
fn editing_after_an_autosave_conflict_keeps_explicit_recovery_required() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::editing_after_an_autosave_conflict_keeps_explicit_recovery_required",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, identity) = save_fixture(card, "notes.txt", "disk\n");
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    document.replace_text("unsaved\n");
    let revision = document.revision;
    drop(runtime.auto_save_task(card));
    drop(runtime.update(RepositoryAction::AutoSaveDue {
        identity,
        revision,
        ticket: runtime.next_auto_save_ticket,
    }));
    drop(runtime.update(runtime.conflicted_active_save_completion()));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.auto_save_task(card));
    assert!(runtime.has_save_conflict());
    assert!(!runtime.is_saving());
    assert!(runtime.is_dirty());
}

#[test]
fn inspector_projection_tracks_loading_conflict_and_active_diff_facts() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    assert_eq!(runtime.inspector_context().file_list, "Loading");

    let file = RepositoryFile {
        path: PathBuf::from("notes.md"),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: true,
        unstaged: true,
        status: Some("MM".into()),
        directory: false,
    };
    runtime.files = FileListState::Ready(vec![file.clone()]);
    runtime.selected = Some(file);
    let mut document = EditorDocument::unlinked(
        PathBuf::from("notes.md"),
        "before\n".into(),
        blake3::hash(b"before\n"),
        true,
    );
    document.dirty = true;
    document.conflict = true;
    document.error = Some("changed on disk".into());
    runtime.document = DocumentState::Editable(document);
    runtime.source_surface = SourceSurface::Diff;
    runtime.diff_basis = DiffBasis::HeadToIndex;
    runtime.diff_mode = DiffMode::Split;
    runtime.set_diff_state(
        DiffBasis::HeadToIndex,
        DiffState::ready(staged_text_diff(DiffModel {
            line_counts: [0; 2],
            before: Arc::from(""),
            inline_rows: Vec::new(),
            split_rows: Vec::new(),
            inline_change_starts: vec![0, 2],
            split_change_starts: vec![0, 2],
            unchanged: Default::default(),
            additions: 3,
            deletions: 2,
            max_line_width: [0; 2],
            syntax: DiffSyntax::default(),
        })),
    );

    let context = runtime.inspector_context();
    assert_eq!(context.file_count, Some(1));
    assert_eq!(context.surface, "Diff");
    assert_eq!(context.document, "Save conflict");
    assert_eq!(context.document_error.as_deref(), Some("changed on disk"));
    assert!(context.conflict);
    assert!(context.has_staged && context.has_unstaged);
    assert_eq!(context.diff_basis, Some("Staged"));
    assert_eq!(context.diff_mode, Some("Split"));
    assert_eq!(context.diff_state, Some("Ready"));
    assert_eq!(context.diff_additions, Some(3));
    assert_eq!(context.diff_deletions, Some(2));
    assert_eq!(context.diff_change, Some((1, 2)));

    runtime.set_diff_state(
        DiffBasis::HeadToIndex,
        DiffState::Loading {
            request: 1,
            cancellation: CancellationToken::new(),
        },
    );
    runtime.set_diff_state(
        DiffBasis::HeadToIndex,
        DiffState::ready(RenderedDiff::Error("diff failed".into())),
    );
    assert_eq!(
        runtime.inspector_context().diff_error.as_deref(),
        Some("diff failed")
    );

    runtime.files = FileListState::Failed("status failed".into());
    let context = runtime.inspector_context();
    assert_eq!(context.file_list, "Unavailable");
    assert_eq!(context.file_error.as_deref(), Some("status failed"));
}

#[test]
fn direct_diff_basis_action_cannot_select_an_absent_basis() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    runtime.selected = Some(RepositoryFile {
        path: PathBuf::from("only-unstaged.txt"),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some(" M".into()),
        directory: false,
    });
    runtime.diff_basis = DiffBasis::IndexToWorktree;
    let card = CardId::new();
    let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
        card,
        basis: DiffBasis::HeadToIndex,
    });
    assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
}

#[test]
fn successful_save_invalidates_hidden_diff_then_loads_fresh_changes_on_demand() {
    for tracked in [true, false] {
        let view = RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: PathBuf::from("/repo/.git"),
            worktree: PathBuf::from("/repo"),
        };
        let mut runtime = RepositoryRuntime::new(view);
        let card = CardId::new();
        let path = PathBuf::from(if tracked { "tracked.txt" } else { "new.txt" });
        runtime.generation = 1;
        runtime.selected = Some(RepositoryFile {
            path: path.clone(),
            staged_old_path: None,
            unstaged_old_path: None,
            staged: false,
            unstaged: !tracked,
            status: (!tracked).then(|| "??".into()),
            directory: false,
        });
        let mut document = EditorDocument::unlinked(
            path.clone(),
            "before\n".into(),
            blake3::hash(b"before\n"),
            false,
        );
        document.content = text_editor::Content::with_text("after\n");
        document.dirty = true;
        document.revision = 1;
        let identity = AsyncIdentity {
            card,
            worktree: PathBuf::from("/repo"),
            path: path.clone(),
            generation: 1,
        };
        document.active_save = Some(SaveTicket {
            id: 1,
            identity: identity.clone(),
            revision: 1,
            history_revision: if content_matches(&document.content, "after\n") {
                document.history.revision()
            } else {
                u64::MAX
            },
            snapshot_source: text_editor::Content::<cosmic::Renderer>::with_text("after\n")
                .text_snapshot(),
            snapshot: "after\n".into(),
        });
        runtime.document = DocumentState::Editable(document);

        let _ = runtime.update(RepositoryAction::SaveFinished {
            identity,
            ticket: 1,
            revision: 1,
            saved_text: "after\n".into(),
            result: Ok(SaveOutcome::Saved {
                content_hash: blake3::hash(b"after\n"),
                durability: SaveDurability::Durable,
            }),
        });

        assert!(
            runtime
                .selected
                .as_ref()
                .is_some_and(RepositoryFile::changed)
        );
        assert_eq!(
            runtime
                .selected
                .as_ref()
                .and_then(|selected| selected.status.as_deref()),
            Some(if tracked { " M" } else { "??" })
        );
        assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert!(matches!(
            runtime.diff_state(&DiffBasis::IndexToWorktree),
            Some(DiffState::Loading { .. })
        ));
    }
}

#[test]
fn successful_save_marks_a_staged_file_as_staged_and_unstaged() {
    assert_eq!(status_with_unstaged_change(Some("M "), false), "MM");
    assert_eq!(status_with_unstaged_change(None, false), " M");
    for status in ["??", "UU", "D?", "AA", " R", " T"] {
        assert_eq!(status_with_unstaged_change(Some(status), true), status);
    }
}

#[test]
fn diff_completions_are_fenced_per_basis_and_preserve_the_active_viewport() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    let card = CardId::new();
    let path = PathBuf::from("notes.txt");
    runtime.generation = 1;
    runtime.selected = Some(RepositoryFile {
        path: path.clone(),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: true,
        unstaged: true,
        status: Some("MM".into()),
        directory: false,
    });
    runtime.diff_basis = DiffBasis::IndexToWorktree;
    runtime.diff_change = 2;
    runtime.diff_viewport = ListViewport {
        offset: 600.0,
        height: 300.0,
    };
    runtime.set_diff_state(
        DiffBasis::HeadToIndex,
        DiffState::Loading {
            request: 3,
            cancellation: CancellationToken::new(),
        },
    );
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::Loading {
            request: 2,
            cancellation: CancellationToken::new(),
        },
    );
    let identity = AsyncIdentity {
        card,
        worktree: PathBuf::from("/repo"),
        path,
        generation: 1,
    };

    let _ = runtime.update(RepositoryAction::DiffLoaded {
        identity: identity.clone(),
        basis: DiffBasis::HeadToIndex,
        request: 3,
        content: staged_text_diff(model_with(3)),
    });
    assert_eq!(runtime.diff_change, 2);
    assert_eq!(runtime.diff_viewport.offset, 600.0);

    let _ = runtime.update(RepositoryAction::DiffLoaded {
        identity: identity.clone(),
        basis: DiffBasis::IndexToWorktree,
        request: 2,
        content: staged_text_diff(model_with(2)),
    });
    assert_eq!(runtime.diff_change, 0);
    assert_eq!(runtime.diff_viewport.offset, 600.0);
    let _ = runtime.update(RepositoryAction::DiffLoaded {
        identity,
        basis: DiffBasis::IndexToWorktree,
        request: 1,
        content: staged_text_diff(model_with(1)),
    });
    assert!(matches!(
        runtime.diff_state(&DiffBasis::IndexToWorktree),
        Some(DiffState::Ready(RenderedDiff::Text { model, .. }, _)) if model.additions == 2
    ));
}

#[test]
fn stale_file_completion_cannot_replace_current_selection() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    runtime.generation = 9;
    runtime.selected = Some(RepositoryFile {
        path: PathBuf::from("current.rs"),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some(" M".into()),
        directory: false,
    });
    runtime.document = DocumentState::Loading { predecessor: None };
    let stale = AsyncIdentity {
        card: CardId::new(),
        worktree: PathBuf::from("/repo"),
        path: PathBuf::from("older.rs"),
        generation: 8,
    };
    let _ = runtime.update(RepositoryAction::FileLoaded {
        identity: stale,
        result: Ok(LoadedFile {
            source: "older.rs".into(),
            content: FileContent::Editable {
                text: "stale".into(),
                content_hash: blake3::hash(b"stale"),
                markdown: false,
            },
        }),
    });
    assert!(
        matches!(runtime.document, DocumentState::Loading { .. }),
        "a stale load neither fills nor clears the document still on its way"
    );
    assert_eq!(
        runtime.selected.as_ref().map(|file| file.path.as_path()),
        Some(Path::new("current.rs")),
        "the selection the load is for is untouched"
    );
}

#[test]
fn visible_row_projection_bounds_widgets_for_large_virtualized_lists() {
    const ROW_HEIGHT: f32 = 36.0;
    let top = visible_rows(
        100_000,
        ListViewport {
            offset: 0.0,
            height: 720.0,
        },
        ROW_HEIGHT,
    );
    let middle = visible_rows(
        1_000_000,
        ListViewport {
            offset: 12_345_678.0,
            height: 900.0,
        },
        DIFF_ROW_HEIGHT,
    );
    assert!(top.end - top.start <= 34);
    // A partial row at each edge plus the overscan on both sides.
    assert!(
        middle.end - middle.start
            <= (900.0 / DIFF_ROW_HEIGHT).ceil() as usize + 1 + 2 * VISIBLE_ROW_OVERSCAN
    );
    assert_eq!(
        top.top_space + (top.end - top.start) as f32 * ROW_HEIGHT + top.bottom_space,
        100_000.0 * ROW_HEIGHT
    );
    assert_eq!(
        middle.top_space
            + (middle.end - middle.start) as f32 * DIFF_ROW_HEIGHT
            + middle.bottom_space,
        1_000_000.0 * DIFF_ROW_HEIGHT
    );
}

#[test]
fn annotation_measurement_uses_natural_layout_and_preserves_child_interaction() {
    let renderer = headless_renderer();
    let card = CardId::new();
    let (_, identity) = diff_runtime(card);
    let content = widget::column::with_children([
        iced::widget::Space::new().height(42.0).into(),
        widget::column::with_children([
            iced::widget::Space::new().height(DIFF_ROW_HEIGHT).into(),
            widget::button::standard("Reply to review")
                .height(64.0)
                .on_press(RepositoryAction::NextChange { card })
                .into(),
        ])
        .into(),
    ])
    .width(300.0)
    .into();
    let mut element = Element::new(DiffAnnotationMeasure {
        content,
        identity,
        basis: DiffBasis::IndexToWorktree,
        mode: DiffMode::Inline,
        layout_revision: 4,
        review_revision: (2, 3),
        row_height: DIFF_ROW_HEIGHT,
        expected_leading: 0.0,
        expected: BTreeMap::from([(8, 99.0), (20, 80.0)]),
        // Row 8's removed discussion is deliberately absent. An offscreen
        // discussion retains its last measured height until it mounts.
        retained: BTreeMap::from([(20, 80.0)]),
        slots: vec![(0, None), (1, Some(2))],
        pending_anchor: false,
    });
    let mut tree = Tree::new(element.as_widget());
    let bounds = iced::Rectangle::with_size(iced::Size::new(300.0, 600.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, bounds.size()),
    );
    let mut messages = Vec::new();
    let cursor = iced::mouse::Cursor::Available(iced::Point::new(40.0, 90.0));
    element.as_widget_mut().update(
        &mut tree,
        &iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
        Layout::new(&node),
        cursor,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        matches!(&messages[..], [RepositoryAction::DiffAnnotations { leading, rows, review_revision: (2, 3), .. }] if *leading == 42.0 && rows == &[(2,64.0),(20,80.0)])
    );
    messages.clear();
    for event in [
        iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
        iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
    ] {
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Mouse(event),
            Layout::new(&node),
            cursor,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
    }
    assert!(
        matches!(&messages[..], [RepositoryAction::NextChange { card: pressed }] if *pressed == card)
    );
    let accessible = element
        .as_widget()
        .a11y_nodes(Layout::new(&node), &tree, cursor);
    assert!(!accessible.root().is_empty() || !accessible.children().is_empty());
}

#[test]
fn annotation_measurements_reject_replaced_comparison_and_wrong_document() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    let content = || staged_text_diff(crate::diff_view::build_diff("old\n", "new\n"));
    runtime.set_diff_state(DiffBasis::IndexToWorktree, DiffState::ready(content()));
    let measurement = RepositoryAction::DiffAnnotations {
        identity: identity.clone(),
        basis: DiffBasis::IndexToWorktree,
        mode: DiffMode::Inline,
        layout_revision: runtime.diff_layout_revision,
        review_revision: (0, 0),
        leading: 40.0,
        rows: vec![(0, 80.0)],
    };
    runtime.set_diff_state(DiffBasis::IndexToWorktree, DiffState::ready(content()));
    drop(runtime.update(measurement));
    assert_eq!(runtime.diff_rows().unwrap().leading_height(), 0.0);
    let mut stale = identity.clone();
    stale.path = "different.txt".into();
    for (identity, expected_leading) in [(stale, 0.0), (identity, 40.0)] {
        drop(runtime.update(RepositoryAction::DiffAnnotations {
            identity,
            basis: DiffBasis::IndexToWorktree,
            mode: DiffMode::Inline,
            layout_revision: runtime.diff_layout_revision,
            review_revision: (0, 0),
            leading: 40.0,
            rows: vec![(0, 80.0)],
        }));
        assert_eq!(
            runtime.diff_rows().unwrap().leading_height(),
            expected_leading
        );
    }
    assert_eq!(runtime.diff_rows().unwrap().leading_height(), 40.0);
    assert_eq!(
        runtime.diff_rows().unwrap().height(0),
        runtime.diff_typography().line_height + 80.0
    );
}

#[test]
fn gutter_ranges_preserve_side_and_original_anchor_when_backtracking() {
    use crate::github::ReviewDiffSide::{Left, Right};
    use crate::pull_request_review::{Action, discussion::Anchor, discussion_ui};
    use cosmic::iced::futures::StreamExt;
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    for (line, side, extend, expected) in [
        (5, Right, false, (5, 5)),
        (9, Right, true, (5, 9)),
        (3, Right, true, (3, 5)),
        (2, Left, true, (2, 2)),
    ] {
        let task = runtime.update(RepositoryAction::ReviewLine {
            identity: identity.clone(),
            basis: DiffBasis::IndexToWorktree,
            extend,
            action: Action::Discussion(discussion_ui::Action::Fenced {
                generation: 11,
                authority_revision: 7,
                action: Box::new(discussion_ui::Action::Begin(Anchor {
                    path: "notes.txt".into(),
                    side,
                    start_line: line,
                    line,
                })),
            }),
        });
        let mut stream = iced::runtime::task::into_stream(task).unwrap();
        let output = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(stream.next())
            .unwrap();
        let iced::runtime::Action::Output(RepositoryAction::Review { mut action, .. }) = output
        else {
            panic!("review action");
        };
        let anchor = review_begin_anchor(&mut action).unwrap();
        assert_eq!((anchor.start_line, anchor.line), expected);
        assert_eq!(anchor.side, side);
        assert!(matches!(
            action,
            Action::Discussion(discussion_ui::Action::Fenced { generation: 11, .. })
        ));
    }
}

#[test]
fn measured_diff_blocks_preserve_anchor_and_change_navigation() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    let mut model = model_with(2);
    model.inline_rows = (0..20)
        .map(|line| InlineDiffRow {
            old_line: Some(line + 1),
            new_line: Some(line + 1),
            text: "line".into(),
            kind: DiffRowKind::Context,
            changed: Vec::new(),
        })
        .collect();
    model.inline_change_starts = vec![2, 7];
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(model)),
    );
    let row = runtime.diff_typography().line_height;
    // Halfway through row 3, before any annotation is measured.
    runtime.diff_viewport = ListViewport {
        offset: 3.5 * row,
        height: 120.0,
    };
    runtime.diff_horizontal = 44.0;
    drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 60.0, &[(1, 90.0), (3, 30.0)]));
    // Still halfway through row 3, which now carries its 30px block.
    assert_eq!(
        runtime.diff_viewport.offset,
        60.0 + 3.0 * row + 90.0 + 0.5 * (row + 30.0)
    );
    assert_eq!(runtime.diff_horizontal, 44.0);
    let unchanged = runtime.diff_viewport.offset;
    drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 60.0, &[(1, 90.0), (3, 30.0)]));
    assert_eq!(runtime.diff_viewport.offset, unchanged);
    drop(runtime.update(RepositoryAction::NextChange { card }));
    assert_eq!(runtime.diff_viewport.offset, 60.0 + 7.0 * row + 90.0 + 30.0);
    drop(runtime.update(RepositoryAction::PreviousChange { card }));
    assert_eq!(runtime.diff_viewport.offset, 60.0 + 2.0 * row + 90.0);
    drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 0.0, &[]));
    assert_eq!(runtime.diff_viewport.offset, 2.0 * row);
}

#[test]
fn change_navigation_follows_manual_scroll_and_stops_at_the_ends() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    let mut model = model_with(2);
    model.inline_rows = (0..20)
        .map(|line| InlineDiffRow {
            old_line: Some(line + 1),
            new_line: Some(line + 1),
            text: "line".into(),
            kind: DiffRowKind::Context,
            changed: Vec::new(),
        })
        .collect();
    model.inline_change_starts = vec![2, 7, 15];
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(model)),
    );
    let middle = runtime.diff_rows().unwrap().offset(7);
    let last = runtime.diff_rows().unwrap().offset(15);
    runtime.diff_viewport.offset = middle + 1.0;
    runtime.navigate_change(true);
    assert_eq!(runtime.diff_change, 2);
    runtime.realize_current_change();
    assert_eq!(runtime.diff_viewport.offset, last);
    runtime.navigate_change(true);
    assert_eq!(runtime.diff_change, 2);
    runtime.diff_viewport.offset = middle + 1.0;
    runtime.navigate_change(false);
    assert_eq!(runtime.diff_change, 1);
    runtime.diff_viewport.offset = 0.0;
    assert!(!runtime.navigate_change(false));
    assert_eq!(runtime.diff_viewport.offset, 0.0);
}

#[test]
fn mutation_refresh_keeps_only_the_same_files_comparison_visible() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(model_with(3))),
    );
    runtime.diff_viewport.offset = 180.0;
    drop(runtime.refresh_after_mutation(card));
    assert!(runtime.diff_model().is_some());
    assert_eq!(runtime.diff_viewport.offset, 180.0);
    assert!(runtime.selected_staging_selection(true).is_none());
    runtime.selected.as_mut().unwrap().path = PathBuf::from("another.rs");
    assert!(runtime.diff_model().is_none());
}

#[test]
fn folded_changes_step_exactly_when_every_scroll_target_is_clamped() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    let before = (0..100)
        .map(|row| format!("line {row}\n"))
        .collect::<String>();
    let after = before
        .replace("line 20\n", "first change\n")
        .replace("line 80\n", "second change\n");
    runtime.diff_viewport.height = 2000.0;
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
            &before, &after,
        ))),
    );
    assert_eq!(runtime.change_count(), 2);
    runtime.diff_change = 0;
    runtime.realize_current_change();
    assert_eq!(runtime.diff_viewport.offset, 0.0);
    assert!(runtime.navigate_change(true));
    runtime.realize_current_change();
    assert_eq!(runtime.diff_change, 1);
    assert_eq!(runtime.diff_viewport.offset, 0.0);
    assert!(!runtime.navigate_change(true));
    assert!(runtime.navigate_change(false));
    runtime.realize_current_change();
    assert_eq!(runtime.diff_change, 0);
    assert!(!runtime.navigate_change(false));
}

#[test]
fn changing_folding_preference_preserves_the_logical_viewport() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    let before = (0..100)
        .map(|row| format!("line {row}\n"))
        .collect::<String>();
    let after = before.replace("line 50\n", "changed\n");
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
            &before, &after,
        ))),
    );
    let row = runtime
        .diff_model()
        .unwrap()
        .change_starts(runtime.diff_mode)[0];
    let display = runtime
        .diff_layout()
        .unwrap()
        .folded(runtime.diff_mode)
        .display(row);
    runtime.diff_viewport = ListViewport {
        offset: runtime.diff_rows().unwrap().offset(display),
        height: 20.0,
    };
    let folded_offset = runtime.diff_viewport.offset;
    drop(runtime.set_diff_hide_unchanged(false));
    assert_eq!(
        runtime.diff_viewport.offset,
        runtime.diff_rows().unwrap().offset(row)
    );
    drop(runtime.set_diff_hide_unchanged(true));
    assert_eq!(runtime.diff_viewport.offset, folded_offset);
}

#[test]
fn file_discussion_is_offered_only_where_the_diff_can_show_it() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::Loading {
            request: 1,
            cancellation: CancellationToken::new(),
        },
    );
    assert!(
        !runtime.can_show_file_discussion(),
        "a pending comparison may fail and strand a new file draft"
    );
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(RenderedDiff::Error("unreadable".into())),
    );
    assert!(!runtime.can_show_file_discussion());
    // Once a retry provides a renderable diff, a draft can be offered.
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff("", "line\n"))),
    );
    assert!(runtime.can_show_file_discussion());
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(RenderedDiff::Error("unreadable".into())),
    );
    assert!(
        !runtime.can_show_file_discussion(),
        "\"Could not compute diff\" has no leading annotation"
    );
    runtime.basis_resolution = Some(BasisResolution::NoCommits);
    assert!(!runtime.can_show_file_discussion());
}

#[test]
fn revealing_the_diff_top_survives_the_leading_block_being_measured() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
            "",
            &"line\n".repeat(40),
        ))),
    );
    runtime.source_surface = SourceSurface::Editor;
    runtime.diff_viewport.offset = 300.0;
    drop(runtime.update(RepositoryAction::RevealDiffTop { card }));
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert_eq!(runtime.diff_viewport.offset, 0.0);
    // A new file discussion is measured after it mounts. Holding the
    // first code row in place would scroll the discussion out of view.
    drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 180.0, &[]));
    assert_eq!(runtime.diff_viewport.offset, 0.0);
    assert!(runtime.pending_diff_anchor.is_none());
}

#[test]
fn annotated_file_reopen_restores_logical_anchor_after_async_load() {
    for (saved, scrolled, expected) in [
        (350.0, None, 350.0),
        (350.0, Some(60.0), 260.0),
        (100.0, None, 100.0),
    ] {
        let card = CardId::new();
        let (mut runtime, _) = diff_runtime(card);
        let content = || staged_text_diff(crate::diff_view::build_diff("", &"line\n".repeat(20)));
        runtime.set_diff_state(DiffBasis::IndexToWorktree, DiffState::ready(content()));
        drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 200.0, &[]));
        runtime.diff_viewport.offset = saved;
        let file = runtime.selected.clone().unwrap();
        drop(runtime.open_file_entry(card, file.clone()));
        assert!(runtime.pending_diff_anchor.is_some());
        runtime.source_surface = SourceSurface::Diff;
        drop(runtime.ensure_current_diff(card));
        let Some(DiffState::Loading { request, .. }) =
            runtime.diff_state(&DiffBasis::IndexToWorktree)
        else {
            panic!("reopened comparison must load afresh");
        };
        let request = *request;
        drop(runtime.update(RepositoryAction::DiffLoaded {
            identity: runtime.current_identity(card, file.path),
            basis: DiffBasis::IndexToWorktree,
            request,
            content: content(),
        }));
        assert_eq!(
            runtime.diff_viewport.offset,
            if saved >= 200.0 { saved - 200.0 } else { 0.0 }
        );
        // A redraw clamps the temporary uniform document into a taller
        // viewport. This is not the reader choosing a different line.
        drop(runtime.update(RepositoryAction::DiffViewport {
            user_initiated: false,
            card,
            offset: 0.0,
            height: 900.0,
            horizontal: 0.0,
        }));
        assert!(runtime.pending_diff_anchor.is_some());
        if let Some(offset) = scrolled {
            drop(runtime.update(RepositoryAction::DiffViewport {
                user_initiated: true,
                card,
                offset,
                height: 120.0,
                horizontal: 0.0,
            }));
        }
        // The pane returns to its reading height after the temporary
        // redraw; the retained line must then survive measurement.
        runtime.diff_viewport.height = 120.0;
        drop(runtime.set_diff_annotation_heights(DiffMode::Inline, 200.0, &[]));
        assert_eq!(runtime.diff_viewport.offset, expected);
        assert!(runtime.pending_diff_anchor.is_none());
    }
}

#[test]
fn diff_scroll_observer_distinguishes_redraw_from_wheel_without_duplicate_events() {
    let renderer = headless_renderer();
    let card = CardId::new();
    let content = widget::scrollable(iced::widget::Space::new().height(2000.0))
        .height(200.0)
        .width(300.0)
        .on_scroll(move |viewport| RepositoryAction::DiffViewport {
            card,
            offset: viewport.absolute_offset().y,
            height: viewport.bounds().height,
            horizontal: viewport.absolute_offset().x,
            user_initiated: false,
        });
    let mut element = Element::new(DiffScrollObserver {
        content: content.into(),
    });
    let mut tree = Tree::new(element.as_widget());
    let bounds = iced::Rectangle::with_size(iced::Size::new(300.0, 200.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(iced::Size::ZERO, bounds.size()),
    );
    for (event, expected_input) in [
        (
            iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
            false,
        ),
        (
            iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: -3.0 },
            }),
            true,
        ),
    ] {
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            iced::mouse::Cursor::Available(iced::Point::new(20.0, 20.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(messages[0], RepositoryAction::DiffViewport { user_initiated, .. } if user_initiated == expected_input)
        );
    }
}

#[test]
fn actual_widget_trees_stay_bounded_at_top_middle_and_end_of_100k_rows() {
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let mut runtime = RepositoryRuntime::new(view);
    let card = CardId::new();
    let inline = InlineDiffRow {
        old_line: Some(1),
        new_line: Some(1),
        text: "line".into(),
        kind: DiffRowKind::Context,
        changed: Vec::new(),
    };
    let split = SplitDiffRow {
        old_line: Some(1),
        new_line: Some(1),
        old_text: Some("line".into()),
        new_text: Some("line".into()),
        kind: DiffRowKind::Context,
        old_changed: Vec::new(),
        new_changed: Vec::new(),
    };
    runtime.selected = Some(RepositoryFile {
        path: PathBuf::from("large.txt"),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some(" M".into()),
        directory: false,
    });
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(DiffModel {
            line_counts: [0; 2],
            before: Arc::from(""),
            inline_rows: vec![inline; 100_000],
            split_rows: vec![split; 100_000],
            inline_change_starts: vec![0, 50_000, 99_999],
            split_change_starts: vec![0, 50_000, 99_999],
            unchanged: Default::default(),
            additions: 1,
            deletions: 1,
            max_line_width: [34; 2],
            syntax: DiffSyntax::default(),
        })),
    );
    for offset in [
        0.0,
        50_000.0 * DIFF_ROW_HEIGHT,
        100_000.0 * DIFF_ROW_HEIGHT - 720.0,
    ] {
        runtime.diff_viewport = ListViewport {
            offset,
            height: 720.0,
        };
        let element = diff_surface(card, &runtime, None);
        assert!(widget_nodes(&element) < 1_500, "diff widget tree was eager");
    }
}

/// Opens the working comparison afresh and completes its load with `model`.
fn load_fresh_diff(runtime: &mut RepositoryRuntime, card: CardId, model: DiffModel) {
    runtime.diffs.clear();
    runtime.reset_diff_viewports();
    drop(runtime.ensure_current_diff(card));
    let Some(DiffState::Loading { request, .. }) = runtime.diff_state(&DiffBasis::IndexToWorktree)
    else {
        panic!("a fresh comparison loads");
    };
    let request = *request;
    let path = runtime.selected.as_ref().unwrap().path.clone();
    drop(runtime.update(RepositoryAction::DiffLoaded {
        identity: runtime.current_identity(card, path),
        basis: DiffBasis::IndexToWorktree,
        request,
        content: staged_text_diff(model),
    }));
}

/// Two changed lines, at lines 50 and 150 of 200.
fn two_change_model() -> DiffModel {
    let before: String = (1..=200).map(|line| format!("line {line}\n")).collect();
    let after = before
        .replace("line 50\n", "fifty\n")
        .replace("line 150\n", "one hundred fifty\n");
    crate::diff_view::build_diff(&before, &after)
}

/// The new-side source line the comparison's viewport is on.
fn diff_source_line(runtime: &RepositoryRuntime) -> Option<u32> {
    let Some(diff_layout::Anchor::Row { row, .. }) = runtime.current_diff_viewport().row else {
        return None;
    };
    runtime
        .diff_model()?
        .new_line_at_row(runtime.diff_mode, row)
}

#[test]
fn entering_diff_follows_the_editor_line_and_unfolds_it() {
    let card = CardId::new();
    let before = (0..200)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let after = before
        .replace("line 20\n", "changed 20\n")
        .replace("line 180\n", "changed 180\n");
    for mode in [DiffMode::Inline, DiffMode::Split] {
        let (mut runtime, _) = working_diff_runtime(card, &before, &after);
        runtime.diff_mode = mode;
        runtime.source_surface = SourceSurface::Editor;
        let position = text_editor::ScrollPosition {
            line: 100,
            vertical: 3.0,
            horizontal: 12.0,
        };
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!()
        };
        document.content.restore_scroll_position(position);
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert_eq!(diff_source_line(&runtime), Some(101), "{mode:?}");
        // Line 101 sat in the fold between the changes: it is revealed.
        let Some(diff_layout::Anchor::Row { row, .. }) = runtime.current_diff_viewport().row else {
            unreachable!()
        };
        let folded = runtime.diff_layout().unwrap().folded(mode);
        assert!(matches!(
            folded.row(folded.display(row)),
            DisplayRow::Code(_)
        ));
        // Only the part of the fold up to the line and a step past it
        // opens: the rest stays folded, keeping the next change near.
        let model = runtime.diff_model().unwrap();
        let next_change = model.change_starts(mode)[1];
        let between = folded.display(next_change) - folded.display(row);
        assert!(
            between <= diff_layout::FOLD_STEP + diff_layout::FOLD_CONTEXT + 2,
            "{mode:?}: {between} rows to the next change"
        );
        assert!(matches!(
            folded.row(folded.display(row) + diff_layout::FOLD_STEP + 1),
            DisplayRow::Fold { .. }
        ));
        assert!(runtime.pending_diff_source.is_none(), "rows are current");
        // And back: the editor returns to exactly where it was.
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: false }));
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!()
        };
        let restored = document.content.scroll_position();
        assert_eq!((restored.line, restored.horizontal), (100, 12.0));
        assert!((restored.vertical - 3.0).abs() < 0.01);
    }
}

#[test]
fn switching_layouts_after_entering_diff_keeps_the_editor_line() {
    let card = CardId::new();
    let before = (0..200)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let after = before.replace("line 20\n", "changed 20\n");
    let (mut runtime, _) = working_diff_runtime(card, &before, &after);
    runtime.diff_mode = DiffMode::Inline;
    runtime.source_surface = SourceSurface::Diff;
    // Both layouts have been visited, so each has a saved place.
    drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
    drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: false }));
    assert!(runtime.viewports.diff.iter().all(Option::is_some));
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: false }));
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!()
    };
    document
        .content
        .restore_scroll_position(text_editor::ScrollPosition {
            line: 150,
            vertical: 0.0,
            horizontal: 0.0,
        });
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    assert_eq!(diff_source_line(&runtime), Some(151));
    for split in [true, false] {
        drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split }));
        assert_eq!(diff_source_line(&runtime), Some(151), "split: {split}");
    }
}

#[test]
fn an_editor_nobody_moved_keeps_the_comparisons_own_place() {
    let card = CardId::new();
    let before = (0..200)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let after = before
        .replace("line 20\n", "changed 20\n")
        .replace("line 180\n", "changed 180\n");
    for mode in [DiffMode::Inline, DiffMode::Split] {
        // A file just opened in Edit: Diff starts at its first change.
        let (mut runtime, _) = working_diff_runtime(card, &before, &after);
        runtime.diff_mode = mode;
        runtime.diff_viewport.height = 300.0;
        runtime.source_surface = SourceSurface::Editor;
        runtime.reset_diff_viewports();
        let folded = runtime.diff_layout().unwrap().folded(mode).clone();
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert_eq!(
            Some(runtime.diff_viewport.offset),
            runtime.first_change_offset(),
            "{mode:?}"
        );
        assert!(runtime.pending_diff_source.is_none());
        assert_eq!(
            runtime.diff_layout().unwrap().folded(mode),
            &folded,
            "{mode:?}: line 1 is not unfolded in place of the first change"
        );

        // Back to Edit from the top fold and straight back: the
        // comparison stays where it was left, with nothing unfolded.
        drop(runtime.update(RepositoryAction::DiffViewport {
            card,
            offset: 0.0,
            height: 300.0,
            horizontal: 0.0,
            user_initiated: true,
        }));
        assert!(matches!(folded.row(0), DisplayRow::Fold { .. }));
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: false }));
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert_eq!(runtime.diff_viewport.offset, 0.0, "{mode:?}");
        assert_eq!(runtime.diff_layout().unwrap().folded(mode), &folded);
    }
}

#[tokio::test]
async fn entering_diff_holds_the_editor_line_until_stale_rows_rebuild() {
    for mode in [DiffMode::Inline, DiffMode::Split] {
        let card = CardId::new();
        let before = (0..100)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let (mut runtime, _) = working_diff_runtime(card, &before, &before);
        runtime.diff_mode = mode;
        runtime.source_surface = SourceSurface::Editor;
        // Five lines inserted above the reader; the comparison is now
        // behind the buffer until its debounced rebuild.
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                "inserted\n".repeat(5),
            ))),
        }));
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!()
        };
        document
            .content
            .restore_scroll_position(text_editor::ScrollPosition {
                line: 80,
                vertical: 2.0,
                horizontal: 9.0,
            });
        drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
        assert!(!runtime.working_annotations_current());
        assert!(runtime.pending_diff_source.is_some(), "{mode:?}");
        drop(runtime.working_diff_task(card));
        runtime.working_diff.deadline = Some(Instant::now());
        let ticket = runtime.working_diff.timer.unwrap();
        let task = runtime.start_working_diff(card, ticket);
        finish_navigation_task(&mut runtime, task).await;
        assert!(runtime.working_annotations_current());
        assert!(runtime.pending_diff_source.is_none(), "{mode:?}");
        assert_eq!(diff_source_line(&runtime), Some(81), "{mode:?}");
        runtime.align_editor_to_diff();
        assert_eq!(runtime.viewports.editor.line, 80);
        assert!((runtime.viewports.editor.vertical - 2.0).abs() < 0.01);
        assert_eq!(runtime.viewports.editor.horizontal, 9.0);
    }
}

#[test]
fn entering_a_loading_diff_places_the_editor_line_when_rows_arrive() {
    let card = CardId::new();
    let text = (0..100)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let (mut runtime, _) = working_diff_runtime(card, &text, &text);
    runtime.source_surface = SourceSurface::Editor;
    runtime.diff_basis = DiffBasis::HeadToIndex;
    runtime.viewports.editor = text_editor::ScrollPosition {
        line: 70,
        vertical: 3.0,
        horizontal: 8.0,
    };
    runtime.viewports.editor_moved = true;
    runtime.align_diff_to_editor();
    assert!(
        runtime.pending_diff_source.is_some(),
        "no rows to place it in yet"
    );
    runtime.source_surface = SourceSurface::Diff;
    runtime.set_diff_state(
        DiffBasis::HeadToIndex,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff(&text, &text))),
    );
    assert!(runtime.pending_diff_source.is_none());
    assert_eq!(diff_source_line(&runtime), Some(71));
    runtime.align_editor_to_diff();
    assert_eq!(runtime.viewports.editor.line, 70);
    assert!((runtime.viewports.editor.vertical - 3.0).abs() < 0.01);
    assert_eq!(runtime.viewports.editor.horizontal, 8.0);
}

#[test]
fn diff_and_editor_lines_reconcile_native_bare_cr_breaks() {
    let card = CardId::new();
    let text = "first\rsecond\nthird\nlast";
    let (mut runtime, _) = working_diff_runtime(card, text, text);
    runtime.source_surface = SourceSurface::Editor;
    // Editor line 2 is "third": the bare CR splits source line 1 natively.
    runtime.viewports.editor = text_editor::ScrollPosition {
        line: 2,
        vertical: 0.0,
        horizontal: 0.0,
    };
    runtime.viewports.editor_moved = true;
    runtime.align_diff_to_editor();
    runtime.source_surface = SourceSurface::Diff;
    assert_eq!(diff_source_line(&runtime), Some(2));
    runtime.align_editor_to_diff();
    assert_eq!(runtime.viewports.editor.line, 2);
}

#[test]
fn diff_rows_follow_the_editor_typography_and_keep_the_reading_row() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    load_fresh_diff(&mut runtime, card, two_change_model());
    // The default editor line is the default row.
    assert_eq!(runtime.diff_typography().line_height, DIFF_ROW_HEIGHT);
    drop(runtime.update(RepositoryAction::DiffViewport {
        card,
        offset: 10.5 * DIFF_ROW_HEIGHT,
        // Short enough that the reading row stays reachable at both sizes.
        height: 40.0,
        horizontal: 0.0,
        user_initiated: true,
    }));
    let reading = |runtime: &RepositoryRuntime| match runtime.current_diff_viewport().row {
        Some(diff_layout::Anchor::Row { row, fraction }) => (row, (fraction * 1e3).round()),
        other => panic!("a code row is being read: {other:?}"),
    };
    let read = reading(&runtime);
    assert_eq!(read.1, 500.0);
    let baseline = Arc::clone(&runtime.diff_model().unwrap().before);

    runtime.set_editor_font(Font::MONOSPACE, 20.0);
    runtime.set_line_height(1.25);
    let typography = runtime.diff_typography();
    assert_eq!((typography.size, typography.line_height), (20.0, 25.0));
    let rows = runtime.diff_rows().unwrap();
    assert!((rows.offset(2) - rows.offset(1) - 25.0).abs() < 0.01);
    assert!(
        Arc::ptr_eq(&runtime.diff_model().unwrap().before, &baseline),
        "a size change scales the measured widths instead of rebuilding"
    );
    assert_eq!(reading(&runtime), read);
    assert!((runtime.diff_viewport.offset - 10.5 * 25.0).abs() < 0.01);

    // Another family has other advances: its rows are measured again,
    // the old ones staying readable until they arrive, and the reader
    // stays on the same logical row throughout.
    let family = Font {
        family: iced::font::Family::SansSerif,
        ..Font::MONOSPACE
    };
    runtime.set_editor_font(family, 20.0);
    assert!(runtime.stale_diff.is_some());
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
    assert_eq!(reading(&runtime), read);
    drop(runtime.refresh_diff_presentation(card));
    let Some(DiffState::Loading { request, .. }) = runtime.diff_state(&DiffBasis::IndexToWorktree)
    else {
        panic!("the dropped comparison is requested again");
    };
    let request = *request;
    drop(runtime.update(RepositoryAction::DiffLoaded {
        identity,
        basis: DiffBasis::IndexToWorktree,
        request,
        content: staged_text_diff(two_change_model()),
    }));
    assert_eq!(reading(&runtime), read);
    assert!((runtime.diff_viewport.offset - 10.5 * 25.0).abs() < 0.01);
}

#[test]
fn a_syntax_theme_change_requests_the_comparison_again_on_the_same_row() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    load_fresh_diff(&mut runtime, card, two_change_model());
    drop(runtime.update(RepositoryAction::DiffViewport {
        card,
        offset: 10.5 * DIFF_ROW_HEIGHT,
        height: 40.0,
        horizontal: 0.0,
        user_initiated: true,
    }));
    let reading = |runtime: &RepositoryRuntime| match runtime.current_diff_viewport().row {
        Some(diff_layout::Anchor::Row { row, fraction }) => (row, (fraction * 1e3).round()),
        other => panic!("a code row is being read: {other:?}"),
    };
    let read = reading(&runtime);

    runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::Mocha);
    // The old rows stay readable while their successor is requested.
    assert!(runtime.diff_model().is_some());
    drop(runtime.refresh_diff_presentation(card));
    let Some(DiffState::Loading { request, .. }) = runtime.diff_state(&DiffBasis::IndexToWorktree)
    else {
        panic!("the recoloured comparison is requested again");
    };
    let request = *request;
    drop(runtime.update(RepositoryAction::DiffLoaded {
        identity,
        basis: DiffBasis::IndexToWorktree,
        request,
        content: staged_text_diff(two_change_model()),
    }));
    assert_eq!(reading(&runtime), read);
    assert!((runtime.diff_viewport.offset - 10.5 * DIFF_ROW_HEIGHT).abs() < 0.01);
    assert!(runtime.stale_diff.is_none());
}

#[test]
fn a_presentation_change_keeps_what_was_unfolded_and_the_reading_row() {
    let theme = |runtime: &mut RepositoryRuntime| {
        runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::Ocean);
    };
    let family = |runtime: &mut RepositoryRuntime| {
        runtime.set_editor_font(
            Font {
                family: cosmic::iced::font::Family::Serif,
                ..runtime.editor_font
            },
            runtime.editor_font_size,
        );
    };
    for change in [theme, family] {
        let card = CardId::new();
        let (mut runtime, identity) = diff_runtime(card);
        runtime.diff_viewport.height = 300.0;
        load_fresh_diff(&mut runtime, card, two_change_model());
        drop(runtime.update(RepositoryAction::ExpandDiffFold {
            identity: identity.clone(),
            basis: DiffBasis::IndexToWorktree,
            mode: DiffMode::Inline,
            fold: 1,
            run: 51..150,
            expansion: FoldExpansion::Down,
        }));
        // The reader is inside the rows they unfolded.
        let opened = runtime
            .diff_layout()
            .unwrap()
            .folded(DiffMode::Inline)
            .display(60);
        drop(runtime.update(RepositoryAction::DiffViewport {
            card,
            offset: opened as f32 * DIFF_ROW_HEIGHT,
            height: 300.0,
            horizontal: 0.0,
            user_initiated: true,
        }));
        let folded = runtime
            .diff_layout()
            .unwrap()
            .folded(DiffMode::Inline)
            .clone();
        assert_eq!(folded.row(opened), DisplayRow::Code(60));
        let reading = |runtime: &RepositoryRuntime| match runtime.current_diff_viewport().row {
            Some(diff_layout::Anchor::Row { row, fraction }) => (row, (fraction * 1e3).round()),
            other => panic!("a code row is being read: {other:?}"),
        };
        let read = reading(&runtime);

        change(&mut runtime);
        assert_eq!(
            runtime.diff_layout().unwrap().folded(DiffMode::Inline),
            &folded,
            "the comparison on screen stays as it was while it reloads"
        );
        drop(runtime.refresh_diff_presentation(card));
        let Some(DiffState::Loading { request, .. }) =
            runtime.diff_state(&DiffBasis::IndexToWorktree)
        else {
            panic!("the comparison is requested again");
        };
        let request = *request;
        drop(runtime.update(RepositoryAction::DiffLoaded {
            identity,
            basis: DiffBasis::IndexToWorktree,
            request,
            content: staged_text_diff(two_change_model()),
        }));
        assert!(runtime.stale_diff.is_none());
        assert_eq!(
            runtime.diff_layout().unwrap().folded(DiffMode::Inline),
            &folded,
            "the rebuilt comparison keeps what was unfolded"
        );
        assert_eq!(reading(&runtime), read);
        assert_eq!(
            runtime.diff_viewport.offset,
            opened as f32 * DIFF_ROW_HEIGHT
        );
    }
}

#[test]
fn a_presentation_change_restyles_the_live_comparison_and_keeps_its_markers() {
    let card = CardId::new();
    let text = "added\nkept\n";
    let (mut runtime, identity) = working_diff_runtime(card, "removed\nkept\n", text);
    runtime.source_surface = SourceSurface::Editor;
    runtime.working_diff.applied = None;
    drop(runtime.working_diff_task(card));
    let ticket = runtime.working_diff.timer.unwrap();
    runtime.working_diff.deadline = Some(Instant::now());
    drop(runtime.start_working_diff(card, ticket));
    let active = runtime.working_diff.active.as_ref().unwrap();
    let (revision, before) = (active.revision, Arc::clone(&active.before));
    let cancellation = active.cancellation.clone();

    // The palette changes while a job styled the old way is running.
    runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::Mocha);
    drop(runtime.refresh_diff_presentation(card));
    assert!(cancellation.is_cancelled());
    drop(runtime.finish_working_diff(
        identity.clone(),
        revision,
        ticket,
        Arc::clone(&before),
        Ok(Some(crate::diff_view::build_diff(&before, text))),
    ));
    assert!(
        runtime.working_diff.applied.is_none(),
        "a comparison in the old palette is not installed"
    );
    let next = runtime
        .working_diff
        .timer
        .expect("restarted in the new palette");
    runtime.working_diff.deadline = Some(Instant::now());
    drop(runtime.start_working_diff(card, next));
    let active = runtime.working_diff.active.as_ref().unwrap();
    assert_eq!(active.presentation, runtime.diff_presentation);
    let revision = active.revision;
    drop(runtime.finish_working_diff(
        identity.clone(),
        revision,
        next,
        Arc::clone(&before),
        Ok(Some(crate::diff_view::build_diff(&before, text))),
    ));
    assert!(runtime.working_annotations_current());

    // An applied comparison: its added-line markers name source lines,
    // which no palette or font changes, so they stay while it rebuilds.
    let family = Font {
        family: cosmic::iced::font::Family::Serif,
        ..runtime.editor_font
    };
    runtime.set_editor_font(family, runtime.editor_font_size);
    drop(runtime.refresh_diff_presentation(card));
    assert!(runtime.working_annotations_current());
    assert_eq!(runtime.working_diff.additions, vec![0]);
    let rebuild = runtime.working_diff.timer.expect("rows are measured again");
    runtime.working_diff.deadline = Some(Instant::now());
    drop(runtime.start_working_diff(card, rebuild));
    let revision = runtime.working_diff.active.as_ref().unwrap().revision;
    drop(runtime.finish_working_diff(
        identity,
        revision,
        rebuild,
        Arc::clone(&before),
        Ok(Some(crate::diff_view::build_diff(&before, text))),
    ));
    assert!(runtime.working_annotations_current());
    assert!(runtime.working_diff.timer.is_none());
    assert!(runtime.working_diff.active.is_none());
}

/// The buffer is what the reader compares: a Git comparison of the
/// file on disk must not replace the retained rows while they rebuild.
#[test]
fn a_restyle_rebuilds_from_the_buffer_without_asking_git() {
    let card = CardId::new();
    let (mut runtime, identity) = working_diff_runtime(card, "removed\nkept\n", "added\nkept\n");
    runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::Mocha);
    drop(runtime.refresh_diff_presentation(card));
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
    assert!(runtime.diff_model().is_some(), "the retained rows stay");
    assert!(runtime.working_diff.timer.is_some());
    // Returning to Diff while the rows rebuild asks Git nothing either.
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: false }));
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());

    // A rebuild that fails falls back to Git.
    let ticket = runtime.working_diff.timer.unwrap();
    runtime.working_diff.deadline = Some(Instant::now());
    drop(runtime.start_working_diff(card, ticket));
    let active = runtime.working_diff.active.as_ref().unwrap();
    let (revision, before) = (active.revision, Arc::clone(&active.before));
    drop(runtime.finish_working_diff(
        identity,
        revision,
        ticket,
        before,
        Err("Diff worker failed".into()),
    ));
    assert!(matches!(
        runtime.diff_state(&DiffBasis::IndexToWorktree),
        Some(DiffState::Loading { .. })
    ));
    assert!(runtime.diff_model().is_some(), "the retained rows stay");
}

/// Staging, unstaging or committing can move the index a retained
/// comparison was built against, so only Git can replace it, even
/// while a restyle was rebuilding it from the buffer.
#[test]
fn a_git_refresh_replaces_a_retained_comparison_from_git() {
    for restyled in [false, true] {
        let card = CardId::new();
        let (mut runtime, _) = working_diff_runtime(card, "removed\nkept\n", "added\nkept\n");
        if restyled {
            runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::Mocha);
            drop(runtime.refresh_diff_presentation(card));
            assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());
        }
        drop(runtime.refresh_after_mutation(card));
        assert!(runtime.diff_model().is_some(), "the rows stay readable");
        // The status listing that follows the mutation has arrived.
        runtime.mutation_refresh_pending = false;
        drop(runtime.ensure_current_diff(card));
        // Nor may the buffer race Git with a rebuild against the old index.
        drop(runtime.working_diff_task(card));
        if let Some(ticket) = runtime.working_diff.timer {
            runtime.working_diff.deadline = Some(Instant::now());
            drop(runtime.start_working_diff(card, ticket));
        }
        assert!(
            runtime.working_diff.active.is_none(),
            "restyled: {restyled}"
        );
        assert!(
            matches!(
                runtime.diff_state(&DiffBasis::IndexToWorktree),
                Some(DiffState::Loading { .. })
            ),
            "restyled: {restyled}"
        );
    }
}

#[test]
fn diff_rows_lay_out_at_the_editor_size_and_line_height() {
    use cosmic::iced::advanced::{Layout, layout, widget::Tree};
    let renderer = headless_renderer();
    let model = crate::diff_view::build_diff(
        "let value = 1;\n",
        "let value = 2; // a much longer replacement line\n",
    );
    let typography = DiffTypography {
        font: Font::MONOSPACE,
        size: 20.0,
        line_height: 30.0,
    };
    let code_width = typography.code_width(&model);
    let measured = model.max_line_width[usize::from(cosmic::theme::is_dark())] as f32;
    assert!((code_width - measured * 20.0 / DIFF_FONT_SIZE).abs() < 0.01);
    // Line numbers keep the default 12 px, 8 px a digit, beside the
    // default font, and grow with the editor's size.
    let default = DiffTypography::default();
    assert_eq!(default.gutter_size(), 12.0);
    assert_eq!(default.gutter_width(50_000), 5.0 * 8.0 + 12.0);
    assert!(typography.gutter_width(50_000) > default.gutter_width(50_000));
    for mode in [DiffMode::Inline, DiffMode::Split] {
        let mut row = match mode {
            DiffMode::Inline => inline_diff_row_with_review(
                model.inline_rows.last().unwrap(),
                &model.syntax,
                44.0,
                code_width,
                0.0,
                None,
                typography,
            ),
            DiffMode::Split => split_diff_row_with_review(
                model.split_rows.last().unwrap(),
                &model.syntax,
                44.0,
                code_width,
                0.0,
                None,
                typography,
            ),
        };
        let mut tree = Tree::new(row.as_widget());
        let node = row.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1200.0, 1000.0)),
        );
        assert_eq!(node.size().height, 30.0, "{mode:?} row is one editor line");
        let root = Layout::new(&node);
        let half = match mode {
            DiffMode::Inline => root,
            DiffMode::Split => root.children().last().unwrap(),
        };
        let code = half.children().next().unwrap().children().last().unwrap();
        let shaped = code.children().next().unwrap().children().next().unwrap();
        assert_eq!(shaped.bounds().height, 30.0, "{mode:?} text uses the line");
    }
}

#[test]
fn a_first_visit_opens_at_the_first_change_with_context() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.diff_viewport.height = 300.0;
    // Unfolded rows, as a model without unchanged runs has: the first
    // change is far below the top.
    let mut model = model_with(1);
    model.inline_rows = (0..100)
        .map(|line| InlineDiffRow {
            old_line: Some(line + 1),
            new_line: Some(line + 1),
            text: "line".into(),
            kind: DiffRowKind::Context,
            changed: Vec::new(),
        })
        .collect();
    model.inline_change_starts = vec![60, 80];
    load_fresh_diff(&mut runtime, card, model);
    assert_eq!(
        runtime.diff_viewport.offset,
        (60 - diff_layout::FOLD_CONTEXT - 1) as f32 * DIFF_ROW_HEIGHT
    );
    assert_eq!(runtime.diff_change, 0);

    // Folded, the first change is a fold and three context rows down, so
    // the comparison opens at its top.
    load_fresh_diff(&mut runtime, card, two_change_model());
    assert_eq!(runtime.diff_viewport.offset, 0.0);
    let layout = runtime.diff_layout().unwrap();
    let folded = layout.folded(DiffMode::Inline);
    assert!(matches!(folded.row(0), DisplayRow::Fold { fold: 0, .. }));
    assert_eq!(folded.display(49), 4, "the first change");
    assert!(
        folded.len() < 30,
        "two changes in 200 lines fold to a screen"
    );

    // A position the reader chose is theirs: a reload keeps it.
    let chosen = 2.5 * DIFF_ROW_HEIGHT;
    drop(runtime.update(RepositoryAction::DiffViewport {
        card,
        offset: chosen,
        height: 300.0,
        horizontal: 0.0,
        user_initiated: true,
    }));
    let file = runtime.selected.clone().unwrap();
    drop(runtime.open_file_entry(card, file.clone()));
    runtime.source_surface = SourceSurface::Diff;
    drop(runtime.ensure_current_diff(card));
    let Some(DiffState::Loading { request, .. }) = runtime.diff_state(&DiffBasis::IndexToWorktree)
    else {
        panic!("a reopened comparison loads");
    };
    let request = *request;
    drop(runtime.update(RepositoryAction::DiffLoaded {
        identity: runtime.current_identity(card, file.path),
        basis: DiffBasis::IndexToWorktree,
        request,
        content: staged_text_diff(two_change_model()),
    }));
    assert_eq!(runtime.diff_viewport.offset, chosen);
}

#[test]
fn expanding_a_fold_keeps_the_rows_on_screen_and_navigation_exact() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    runtime.diff_viewport.height = 100.0;
    load_fresh_diff(&mut runtime, card, two_change_model());
    let layout = runtime.diff_layout().unwrap();
    let folded = layout.folded(DiffMode::Inline).clone();
    // Model row 148, the second context row above the second change,
    // a third of the way in.
    let display = folded.display(148);
    assert!(matches!(
        folded.row(display - 2),
        DisplayRow::Fold { fold: 1, .. }
    ));
    drop(runtime.update(RepositoryAction::DiffViewport {
        card,
        offset: display as f32 * DIFF_ROW_HEIGHT + 10.0,
        height: 100.0,
        horizontal: 0.0,
        user_initiated: true,
    }));
    let expand = |run: Range<usize>, expansion| RepositoryAction::ExpandDiffFold {
        identity: identity.clone(),
        basis: DiffBasis::IndexToWorktree,
        mode: DiffMode::Inline,
        fold: 1,
        run,
        expansion,
    };
    // A click on a fold drawn from another model names another run.
    drop(runtime.update(expand(52..150, FoldExpansion::Up)));
    assert!(matches!(
        runtime.diff_layout().unwrap().folded(DiffMode::Inline).row(display - 2),
        DisplayRow::Fold { fold: 1, rows } if rows == (54..147)
    ));
    drop(runtime.update(expand(51..150, FoldExpansion::Up)));
    let folded = runtime
        .diff_layout()
        .unwrap()
        .folded(DiffMode::Inline)
        .clone();
    let display = folded.display(148);
    assert_eq!(
        display,
        folded.display(127) + 21,
        "twenty rows revealed above"
    );
    assert_eq!(
        runtime.diff_viewport.offset,
        display as f32 * DIFF_ROW_HEIGHT + 10.0,
        "the row that was on screen stays on screen"
    );

    // Change navigation lands on the change's row in the folded layout,
    // or as near it as the now short document can scroll.
    runtime.diff_viewport.height = 300.0;
    drop(runtime.update(RepositoryAction::NextChange { card }));
    let bottom = folded.len() as f32 * DIFF_ROW_HEIGHT + crate::design::SCROLLBAR_GUTTER - 300.0;
    assert!(bottom < folded.display(150) as f32 * DIFF_ROW_HEIGHT);
    assert_eq!(runtime.diff_viewport.offset, bottom);
    // Reveals are shared by both layouts.
    drop(runtime.update(RepositoryAction::ApplyDiffMode { card, split: true }));
    let split = runtime.diff_layout().unwrap().folded(DiffMode::Split);
    // Split row 126 is line 127, the first line revealed inline.
    let DisplayRow::Code(row) = split.row(split.display(126)) else {
        panic!("the rows revealed inline are revealed side by side");
    };
    assert_eq!(
        runtime.diff_model().unwrap().split_rows[row].new_line,
        Some(127)
    );
    assert!(matches!(
        split.row(split.display(125)),
        DisplayRow::Fold { fold: 1, .. }
    ));
}

#[test]
fn a_rebuild_keeps_what_was_unfolded_and_the_line_on_screen() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    runtime.diff_viewport.height = 300.0;
    load_fresh_diff(&mut runtime, card, two_change_model());
    // The top of the viewport on the second fold itself, expanded down:
    // the fold's first rows take its place.
    let fold = runtime
        .diff_layout()
        .unwrap()
        .folded(DiffMode::Inline)
        .display(60);
    drop(runtime.update(RepositoryAction::DiffViewport {
        card,
        offset: fold as f32 * DIFF_ROW_HEIGHT,
        height: 300.0,
        horizontal: 0.0,
        user_initiated: true,
    }));
    drop(runtime.update(RepositoryAction::ExpandDiffFold {
        identity: identity.clone(),
        basis: DiffBasis::IndexToWorktree,
        mode: DiffMode::Inline,
        fold: 1,
        run: 51..150,
        expansion: FoldExpansion::Down,
    }));
    let folded = runtime
        .diff_layout()
        .unwrap()
        .folded(DiffMode::Inline)
        .clone();
    assert_eq!(folded.row(fold), DisplayRow::Code(54));
    assert_eq!(runtime.diff_viewport.offset, fold as f32 * DIFF_ROW_HEIGHT);

    // The same comparison read again keeps its reveal and its place.
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(two_change_model())),
    );
    assert_eq!(
        runtime.diff_layout().unwrap().folded(DiffMode::Inline),
        &folded
    );
    assert_eq!(runtime.diff_viewport.offset, fold as f32 * DIFF_ROW_HEIGHT);

    // An edit above renumbers the new side and shifts every row by one;
    // the runs keep their lengths, so the reveal holds, and the same line
    // stays at the top.
    let before: String = (1..=200).map(|line| format!("line {line}\n")).collect();
    let after = format!("new first line\n{}", before)
        .replace("line 50\n", "fifty\n")
        .replace("line 150\n", "one hundred fifty\n");
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff(
            &before, &after,
        ))),
    );
    let layout = runtime.diff_layout().unwrap();
    let line_54 = runtime
        .diff_model()
        .unwrap()
        .inline_rows
        .iter()
        .position(|row| row.old_line == Some(54))
        .unwrap();
    let diff_layout::Anchor::Row { row, .. } = layout
        .rows(DiffMode::Inline)
        .anchor(runtime.diff_viewport.offset)
    else {
        panic!("a code row is on screen");
    };
    assert_eq!(
        row,
        layout.folded(DiffMode::Inline).display(line_54),
        "line 54, first on screen before the edit, is still at the top"
    );
}

#[test]
fn a_rebuild_awaiting_its_anchor_still_keeps_what_was_unfolded() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    runtime.diff_viewport.height = 300.0;
    load_fresh_diff(&mut runtime, card, two_change_model());
    drop(runtime.update(RepositoryAction::ExpandDiffFold {
        identity,
        basis: DiffBasis::IndexToWorktree,
        mode: DiffMode::Inline,
        fold: 1,
        run: 51..150,
        expansion: FoldExpansion::Down,
    }));
    let folded = runtime
        .diff_layout()
        .unwrap()
        .folded(DiffMode::Inline)
        .clone();

    // A working-diff rebuild lands while the viewport still waits to
    // place an anchor.
    runtime.pending_diff_anchor = Some(runtime.current_diff_viewport());
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(two_change_model())),
    );
    assert_eq!(
        runtime.diff_layout().unwrap().folded(DiffMode::Inline),
        &folded
    );
}

#[test]
fn a_rebuild_that_removes_the_anchored_line_keeps_visible_rows_mounted() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.diff_viewport.height = 300.0;
    let large: String = (1..=200).map(|line| format!("line {line}\n")).collect();
    load_fresh_diff(&mut runtime, card, crate::diff_view::build_diff("", &large));
    drop(runtime.update(RepositoryAction::DiffViewport {
        card,
        offset: 150.0 * DIFF_ROW_HEIGHT,
        height: 300.0,
        horizontal: 0.0,
        user_initiated: true,
    }));

    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(crate::diff_view::build_diff("", "only\n"))),
    );
    assert_eq!(runtime.diff_viewport.offset, 0.0);
    let visible = visible_diff_rows(runtime.diff_rows().unwrap(), runtime.diff_viewport);
    assert_eq!((visible.start, visible.end), (0, 1));
}

#[test]
fn change_navigation_realizes_far_rows_before_programmatic_scroll() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.diff_viewport.height = 720.0;
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(DiffModel {
            line_counts: [0; 2],
            before: Arc::from(""),
            inline_rows: vec![
                InlineDiffRow {
                    old_line: Some(1),
                    new_line: Some(1),
                    text: "line".into(),
                    kind: DiffRowKind::Context,
                    changed: Vec::new(),
                };
                100_000
            ],
            split_rows: Vec::new(),
            inline_change_starts: vec![0, 50_000, 99_999],
            split_change_starts: Vec::new(),
            unchanged: Default::default(),
            additions: 1,
            deletions: 1,
            max_line_width: [34; 2],
            syntax: DiffSyntax::default(),
        })),
    );
    let _ = runtime.update(RepositoryAction::NextChange { card });
    let middle = visible_rows(100_000, runtime.diff_viewport, DIFF_ROW_HEIGHT);
    assert!(middle.start <= 50_000 && middle.end > 50_000);
    let _ = runtime.update(RepositoryAction::NextChange { card });
    let end = visible_rows(100_000, runtime.diff_viewport, DIFF_ROW_HEIGHT);
    assert!(end.start <= 99_999 && end.end > 99_999);
}

fn completion_edit(range: LspRange, new_text: &str) -> LspCompletionEdit {
    LspCompletionEdit {
        range,
        replace_range: None,
        new_text: new_text.into(),
        snippet: None,
    }
}

fn completion_item(
    edit: LspCompletionEdit,
    additional_edits: Vec<LspCompletionEdit>,
) -> LspCompletionItem {
    LspCompletionItem {
        label: "println!".into(),
        kind: None,
        detail: None,
        documentation: None,
        sort_text: None,
        filter_text: None,
        preselect: false,
        deprecated: false,
        edit,
        additional_edits,
    }
}

fn completion_pointer_step<M: Clone>(
    mut row: Element<'_, M>,
    tree: &mut iced::advanced::widget::Tree,
    renderer: &cosmic::Renderer,
    event: iced::mouse::Event,
) -> Vec<M> {
    use iced::advanced::{Layout, Shell, clipboard, layout, widget::Operation};
    use iced::{Event, Rectangle, Size, mouse};
    struct Bounds(Option<Rectangle>);
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            _: Option<&widget::Id>,
            bounds: Rectangle,
            _: &mut dyn iced::advanced::widget::operation::Focusable,
        ) {
            assert!(
                self.0.replace(bounds).is_none(),
                "one actual completion button"
            );
        }
    }
    tree.diff(row.as_widget_mut());
    let viewport = Rectangle::with_size(Size::new(360.0, 100.0));
    let node = row.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let mut bounds = Bounds(None);
    row.as_widget_mut()
        .operate(tree, Layout::new(&node), renderer, &mut bounds);
    let point = bounds
        .0
        .expect("production completion is a focusable button")
        .center();
    let mut messages = Vec::new();
    row.as_widget_mut().update(
        tree,
        &Event::Mouse(event),
        Layout::new(&node),
        mouse::Cursor::Available(point),
        renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    messages
}

#[test]
fn native_completion_press_belongs_to_the_original_response_including_same_label_edits() {
    use iced::mouse::{Button, Event};
    let renderer = headless_renderer();
    for replacement in [None, Some("old"), Some("new")] {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "foo\n");
        runtime.set_lsp_document(Some(LspDocumentId(41)));
        let item = completion_item(
            completion_edit(
                LspRange {
                    start: LspPosition::new(0, 0),
                    end: LspPosition::new(0, 3),
                },
                "old",
            ),
            vec![],
        );
        runtime.set_editor_completions(vec![item.clone()]);
        let held = runtime.editor_completion_targets(card).remove(0);
        let again = runtime.editor_completion_targets(card).remove(0);
        assert!(
            Arc::ptr_eq(&held.item, &again.item),
            "rendering reuses the accepted payload"
        );
        let row = completion_row(card, &runtime, 0).unwrap();
        let mut tree = Tree::new(row.as_widget());
        drop(row);
        assert!(
            completion_pointer_step(
                completion_row(card, &runtime, 0).unwrap(),
                &mut tree,
                &renderer,
                Event::ButtonPressed(Button::Left)
            )
            .is_empty()
        );
        if let Some(text) = replacement {
            let mut changed = item.clone();
            changed.edit.new_text = text.into();
            runtime.set_editor_completions(vec![changed]);
            assert!(
                !runtime.apply_completion_target(card, &held),
                "held intent cannot cross even an identical response"
            );
        }
        let messages = completion_pointer_step(
            completion_row(card, &runtime, 0).unwrap(),
            &mut tree,
            &renderer,
            Event::ButtonReleased(Button::Left),
        );
        let messages = if replacement.is_some() {
            assert!(
                messages.is_empty(),
                "old press cannot invoke a new response callback"
            );
            assert!(
                completion_pointer_step(
                    completion_row(card, &runtime, 0).unwrap(),
                    &mut tree,
                    &renderer,
                    Event::ButtonPressed(Button::Left)
                )
                .is_empty()
            );
            completion_pointer_step(
                completion_row(card, &runtime, 0).unwrap(),
                &mut tree,
                &renderer,
                Event::ButtonReleased(Button::Left),
            )
        } else {
            messages
        };
        let [
            RepositoryAction::Language {
                action: EditorLspAction::Choose { target, .. },
                ..
            },
        ] = messages.as_slice()
        else {
            panic!("actual button must emit its captured completion");
        };
        assert!(runtime.apply_completion_target(card, target));
        assert_eq!(
            runtime.editor_text().as_deref(),
            Some(if replacement == Some("new") {
                "new\n"
            } else {
                "old\n"
            })
        );
    }
}

#[test]
fn completion_intent_refuses_reopened_documents_and_actual_buffer_edits() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "foo\n");
    runtime.set_lsp_document(Some(LspDocumentId(1)));
    let item = completion_item(
        completion_edit(
            LspRange {
                start: LspPosition::new(0, 0),
                end: LspPosition::new(0, 3),
            },
            "old",
        ),
        vec![],
    );
    runtime.set_editor_completions(vec![item.clone()]);
    let old = runtime.editor_completion_targets(card).remove(0);
    assert!(!runtime.apply_completion_target(CardId::new(), &old));
    runtime.set_lsp_document(Some(LspDocumentId(2)));
    runtime.set_editor_completions(vec![item]);
    assert!(!runtime.apply_completion_target(card, &old));
    let current = runtime.editor_completion_targets(card).remove(0);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    let edited = runtime.editor_text().unwrap();
    assert_ne!(
        document_revision(&runtime),
        current.revision,
        "actual edit changes the captured revision"
    );
    assert!(!runtime.apply_completion_target(card, &current));
    assert_eq!(runtime.editor_text().unwrap(), edited);
}

fn buffer_text(runtime: &RepositoryRuntime) -> String {
    runtime
        .editor_text()
        .expect("test expects an editable buffer")
}

fn document_revision(runtime: &RepositoryRuntime) -> u64 {
    match &runtime.document {
        DocumentState::Editable(document) => document.revision,
        _ => panic!("test expects an editable buffer"),
    }
}

#[test]
fn editor_diagnostic_marks_map_unicode_and_bare_carriage_return_positions() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "α\rbeta\nsecond\n");
    runtime.set_editor_diagnostics(vec![crate::lsp::LspDiagnostic::new(
        LspRange::new(LspPosition::new(0, 2), LspPosition::new(0, 6)),
        crate::lsp::LspSeverity::Error,
        "bad beta".into(),
        None,
        None,
    )]);
    assert_eq!(runtime.language.marks.len(), 1);
    assert_eq!(
        runtime.language.marks[0].start,
        text_editor::Position { line: 1, column: 0 }
    );
    assert_eq!(
        runtime.language.marks[0].end,
        text_editor::Position { line: 1, column: 4 }
    );
    assert_eq!(runtime.language.diagnostics.len(), 1);
    runtime.set_lsp_document(Some(LspDocumentId(1)));
    assert!(
        runtime.language.marks.is_empty(),
        "a new document cannot retain the old document's ranges"
    );
}

#[test]
fn completion_keyboard_navigation_wraps_and_edits_dismiss_the_popup() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "hello\n");
    let item = completion_item(
        completion_edit(
            LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 5)),
            "world",
        ),
        vec![],
    );
    runtime.set_editor_completions(vec![item.clone(), item]);
    drop(runtime.update(RepositoryAction::Language {
        card,
        action: EditorLspAction::Next(false),
    }));
    assert_eq!(runtime.language.selected, 1);
    drop(runtime.update(RepositoryAction::Language {
        card,
        action: EditorLspAction::Next(true),
    }));
    assert_eq!(runtime.language.selected, 0);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
    }));
    assert!(runtime.language.completions.is_empty());
}

fn lsp_version(runtime: &RepositoryRuntime) -> i32 {
    match &runtime.document {
        DocumentState::Editable(document) => document.lsp_version,
        _ => panic!("test expects an editable buffer"),
    }
}

/// Normalises the caret without touching the buffer, and proves in passing
/// that a motion is not a document change.
fn move_to_start(runtime: &mut RepositoryRuntime, card: CardId) {
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Move(text_editor::Motion::DocumentStart),
    }));
    assert_eq!(
        runtime.take_lsp_intents(),
        Vec::new(),
        "moving the caret changes no text, so the server must hear nothing"
    );
}

#[test]
fn loading_an_editable_file_opens_it_at_the_first_version() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Open {
            // Absolute: `AsyncIdentity::path` is worktree-relative and no
            // document URI can be built from it alone.
            absolute_path: PathBuf::from("/repo/notes.txt"),
            worktree: PathBuf::from("/repo"),
            version: 1,
            text: "fn main() {}\n".into(),
        }]
    );
    assert_eq!(
        runtime.take_lsp_intents(),
        Vec::new(),
        "draining is destructive; a notification performed twice is worse than one performed late"
    );
}

/// A row that is a symbolic link opens the file behind it: the buffer is
/// that file's bytes, the language server is told about that file, the
/// chrome names it, and a save goes there rather than over the link.
#[test]
fn a_link_opens_edits_and_saves_its_target() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::a_link_opens_edits_and_saves_its_target",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, _identity) =
        save_fixture_with_source(card, "AGENTS.md", "CLAUDE.md", "# Guidance\n");
    assert_eq!(runtime.selected_path(), Some(Path::new("AGENTS.md")));
    assert_eq!(runtime.linked_source(), Some(Path::new("CLAUDE.md")));
    assert!(matches!(
        runtime.take_lsp_intents().as_slice(),
        [LspIntent::Open { absolute_path, .. }] if absolute_path == &runtime.descriptor.worktree.join("CLAUDE.md")
    ));

    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.request_save(card, false));
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("the buffer stays editable while it saves");
    };
    let ticket = document
        .active_save
        .as_ref()
        .expect("a dirty buffer starts a save");
    assert_eq!(
        ticket.identity.path,
        Path::new("AGENTS.md"),
        "the save is matched back to the row that was opened"
    );
    assert_eq!(
        document.source,
        Path::new("CLAUDE.md"),
        "and its bytes go to the file the link resolves to"
    );

    let (mut plain, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "plain\n");
    assert_eq!(plain.linked_source(), None);
    drop(plain.take_lsp_intents());
}

/// The row stays the link. The comparison, its baseline, and staging
/// follow the target the buffer was read from.
#[test]
fn a_linked_buffer_compares_and_stages_the_target() {
    let card = CardId::new();
    let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "AGENTS.md");
    runtime.files = FileListState::Ready(vec![
        RepositoryFile {
            path: "AGENTS.md".into(),
            staged: true,
            status: Some("M ".into()),
            ..RepositoryFile::default()
        },
        RepositoryFile {
            path: "CLAUDE.md".into(),
            unstaged: true,
            unstaged_old_path: Some(OldPath::Renamed("notes.md".into())),
            status: Some(" R".into()),
            ..RepositoryFile::default()
        },
    ]);
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: "CLAUDE.md".into(),
            content: FileContent::Editable {
                text: "new\n".into(),
                content_hash: blake3::hash(b"new\n"),
                markdown: false,
            },
        }),
    }));
    assert_eq!(runtime.selected_path(), Some(Path::new("AGENTS.md")));
    assert_eq!(
        runtime.diff_pathspec().as_deref(),
        Some(Path::new("CLAUDE.md")),
        "Git is asked for the file the buffer is, not the link row"
    );
    assert_eq!(
        runtime.comparison_old_path().as_deref(),
        Some(Path::new("notes.md")),
        "the old side is the target's rename, which is the index baseline"
    );
    let selection = runtime
        .selected_staging_selection(true)
        .expect("the target has unstaged changes to stage");
    assert_eq!(
        selection.path.as_path(),
        Path::new("CLAUDE.md"),
        "staging names the target"
    );
    assert_eq!(
        selection.old_path.as_ref().map(|path| path.as_path()),
        Some(Path::new("notes.md"))
    );
    assert!(
        runtime.selected_staging_selection(false).is_none(),
        "the link's own staged bit must not unstage the link"
    );
    assert!(runtime.can_show_diff());

    // A target with nothing to stage hides the command rather than
    // acting on the link entry.
    runtime.files = FileListState::Ready(vec![RepositoryFile {
        path: "AGENTS.md".into(),
        unstaged: true,
        status: Some(" M".into()),
        ..RepositoryFile::default()
    }]);
    assert!(runtime.selected_staging_selection(true).is_none());
    assert!(runtime.selected_staging_selection(false).is_none());
}

/// A save through a link changes a row other than the selected one, so
/// the list is asked again rather than the selected row marked modified.
#[test]
fn a_save_through_a_link_refreshes_the_list_instead_of_marking_the_link() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::a_save_through_a_link_refreshes_the_list_instead_of_marking_the_link",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, identity) =
        save_fixture_with_source(card, "AGENTS.md", "CLAUDE.md", "# Guidance\n");
    drop(runtime.take_lsp_intents());
    if let Some(selected) = runtime.selected.as_mut() {
        selected.unstaged = false;
        selected.status = None;
    }
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.request_save(card, false));
    let ticket = match &runtime.document {
        DocumentState::Editable(document) => document.active_save.clone().unwrap(),
        _ => panic!("the buffer stays editable while it saves"),
    };
    drop(runtime.update(RepositoryAction::SaveFinished {
        identity,
        ticket: ticket.id,
        revision: ticket.revision,
        saved_text: ticket.snapshot.clone(),
        result: Ok(SaveOutcome::Saved {
            content_hash: blake3::hash(ticket.snapshot.as_bytes()),
            durability: SaveDurability::Durable,
        }),
    }));
    let selected = runtime.selected.as_ref().unwrap();
    assert!(
        !selected.unstaged && selected.status.is_none(),
        "the link itself did not change: {selected:?}"
    );
    assert!(
        runtime.mutation_refresh_pending,
        "the row that did change is learned from Git"
    );
}

/// The notice names the link that actually failed, which in a chain is
/// not the row, and the link's own target rather than the row's.
#[test]
fn a_refused_link_is_described_by_the_link_that_failed() {
    let card = CardId::new();
    let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "to-dangling");
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: "dangling".into(),
            content: FileContent::ReadOnly(ReadOnlyReason::Symlink {
                target: "missing".into(),
                refusal: LinkRefusal::Dangling,
            }),
        }),
    }));
    let DocumentState::ReadOnly { path, reason } = &runtime.document else {
        panic!("a refused link is read-only");
    };
    assert_eq!(
        read_only_detail(path, reason),
        "dangling is a symbolic link to missing, which does not exist."
    );
    assert_eq!(runtime.linked_source(), Some(Path::new("dangling")));
    assert_eq!(
        read_only_detail(
            Path::new("escape"),
            &ReadOnlyReason::Symlink {
                target: "/etc/passwd".into(),
                refusal: LinkRefusal::LeavesWorktree,
            }
        ),
        "escape is a symbolic link to /etc/passwd, which is outside the worktree or reached through another link."
    );
    assert_eq!(
        read_only_detail(
            Path::new("loop-a"),
            &ReadOnlyReason::Symlink {
                target: "loop-b".into(),
                refusal: LinkRefusal::TooManyLinks,
            }
        ),
        "loop-a is a symbolic link to loop-b, which leads through too many links to follow."
    );
}

#[test]
fn a_read_only_or_unreadable_file_is_never_opened_at_the_boundary() {
    let card = CardId::new();
    for result in [
        Ok(LoadedFile {
            source: "notes.txt".into(),
            content: FileContent::ReadOnly(ReadOnlyReason::Binary),
        }),
        Err(FileIoError::Io("injected read failure".into())),
    ] {
        let (mut runtime, identity) = RepositoryRuntime::lsp_selection_fixture(card);
        drop(runtime.update(RepositoryAction::FileLoaded { identity, result }));
        assert_eq!(
            runtime.take_lsp_intents(),
            Vec::new(),
            "only an editable buffer is a document a language server can serve"
        );
    }
}

#[test]
fn image_preview_never_enters_edit_save_or_language_server_pipelines() {
    let card = CardId::new();
    let (mut runtime, identity) = RepositoryRuntime::lsp_selection_fixture(card);
    runtime.set_auto_save(true);
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: "notes.txt".into(),
            content: FileContent::Image {
                width: 1,
                height: 1,
                rgba: vec![255, 0, 0, 255].into(),
            },
        }),
    }));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.update(RepositoryAction::Save {
        card,
        overwrite: true,
    }));
    assert!(!runtime.is_dirty());
    assert!(!runtime.can_save());
    assert!(!runtime.is_saving());
    assert!(runtime.take_lsp_intents().is_empty());
    assert_eq!(runtime.inspector_context().document, "Image preview");
    assert!(matches!(runtime.document, DocumentState::Image { .. }));
}

/// A card with `book.pdf` open at its first page.
fn document_runtime(card: CardId, pages: usize) -> RepositoryRuntime {
    let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "book.pdf");
    // What `open_file` records before dispatching the read, so the load
    // lands on the tone the test process's theme asks for and does not
    // immediately reconcile itself.
    runtime.document_tone = pdf_tone(&cosmic::theme::active());
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: "book.pdf".into(),
            content: FileContent::Pdf {
                pages,
                page: rendered_document_page(0, pages).page,
            },
        }),
    }));
    runtime
}

fn rendered_document_page(index: usize, pages: usize) -> crate::editor::DocumentPage {
    crate::editor::DocumentPage {
        pages,
        page: crate::editor::PdfPage {
            index,
            width: 2,
            height: 2,
            points_wide: 612.0,
            rgba: vec![255; 16].into(),
        },
    }
}

/// The page, tone, in-flight request and message the viewer is showing.
fn document_view_state(
    runtime: &RepositoryRuntime,
) -> (
    usize,
    crate::editor::PdfTone,
    Option<PdfView>,
    Option<String>,
) {
    let DocumentState::Pdf {
        page,
        tone,
        rendering,
        message,
        ..
    } = &runtime.document
    else {
        panic!("the fixture opens a document");
    };
    (
        *page,
        *tone,
        rendering.as_ref().map(|active| active.requested),
        message.clone(),
    )
}

/// The zoom the viewer is drawing the open page at.
fn document_zoom(runtime: &RepositoryRuntime) -> crate::editor::PdfZoom {
    let DocumentState::Pdf { zoom, .. } = &runtime.document else {
        panic!("the fixture opens a document");
    };
    *zoom
}

/// The view a test expects to be rendering, at the zoom a document opens
/// at.
fn rendering_view(page: usize, tone: crate::editor::PdfTone) -> PdfView {
    PdfView {
        page,
        tone,
        zoom: crate::editor::PdfZoom::default(),
    }
}

/// A tone that is not `tone`, so a test can tell a restored tone from the
/// one it asked for whichever theme the test process runs under.
fn other_document_tone(tone: crate::editor::PdfTone) -> crate::editor::PdfTone {
    match tone {
        crate::editor::PdfTone::Original => crate::editor::PdfTone::Inverted {
            paper: [16, 16, 16],
        },
        crate::editor::PdfTone::Inverted { .. } => crate::editor::PdfTone::Original,
    }
}

#[test]
fn page_turns_taken_during_a_render_cost_one_more_render_and_land_on_the_last_one() {
    let card = CardId::new();
    let mut runtime = document_runtime(card, 30);
    let tone = document_view_state(&runtime).1;
    for page in 1..=20 {
        drop(runtime.update(RepositoryAction::PdfPage {
            card,
            page,
            tone,
            zoom: crate::editor::PdfZoom::default(),
        }));
    }
    let first = runtime.pdf_request;
    assert_eq!(
        first, 1,
        "each render reparses the document and rasterizes a page, so twenty turns over one \
             render must dispatch one render, not twenty"
    );
    let (page, _, requested, _) = document_view_state(&runtime);
    assert_eq!(page, 20, "the counter follows the reader, not the renderer");
    assert_eq!(requested, Some(rendering_view(1, tone)));
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: first,
        result: Ok(rendered_document_page(1, 30)),
    }));
    assert_eq!(
        document_view_state(&runtime).2,
        Some(rendering_view(20, tone)),
        "the turn that was waiting is the last one the reader asked for"
    );
    assert_eq!(runtime.pdf_request, first + 1);
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: first + 1,
        result: Ok(rendered_document_page(20, 30)),
    }));
    let (page, _, requested, _) = document_view_state(&runtime);
    assert_eq!(page, 20, "the reader ends on the page they last asked for");
    assert_eq!(requested, None, "nothing is left queued or in flight");
    assert_eq!(runtime.pdf_request, first + 1);
}

#[test]
fn a_zoom_the_reader_asks_for_renders_that_page_again_at_the_new_size() {
    let card = CardId::new();
    let mut runtime = document_runtime(card, 30);
    let (page, tone, requested, _) = document_view_state(&runtime);
    assert_eq!(requested, None, "the fixture opens with nothing in flight");
    let opened = document_zoom(&runtime);
    assert!(opened.is_fit(), "a document opens at the pane's width");
    let larger = opened.larger().expect("the default zoom can grow");
    drop(runtime.update(RepositoryAction::PdfZoom { card, zoom: larger }));
    assert_eq!(
        document_view_state(&runtime).2,
        Some(PdfView {
            page,
            tone,
            zoom: larger
        }),
        "a page drawn at one size and rendered for another is either soft or too large"
    );
    assert_eq!(document_zoom(&runtime), larger);
}

#[test]
fn a_failed_zoom_leaves_the_controls_describing_the_size_still_drawn() {
    let card = CardId::new();
    let mut runtime = document_runtime(card, 30);
    let opened = document_zoom(&runtime);
    let larger = opened.larger().expect("the default zoom can grow");
    drop(runtime.update(RepositoryAction::PdfZoom { card, zoom: larger }));
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: runtime.pdf_request,
        result: Err("this page could not be rendered".into()),
    }));
    assert_eq!(
        document_zoom(&runtime),
        opened,
        "the zoom control has to describe the page that is on screen"
    );
}

#[test]
fn the_zoom_steps_stay_ordered_and_bounded() {
    let fit = crate::editor::PdfZoom::default();
    assert!(fit.is_fit(), "a document opens fitted to its pane");
    assert_eq!(
        fit.percent(),
        None,
        "a fitted page has no size of its own to report"
    );
    // Either control has to leave Fit for a size the reader can see is
    // different, which means skipping the printed size a fitted page is
    // usually already close to.
    assert_eq!(fit.smaller().and_then(|zoom| zoom.percent()), Some(75));
    assert_eq!(fit.larger().and_then(|zoom| zoom.percent()), Some(125));
    let mut zoom = fit.smaller().unwrap();
    while let Some(smaller) = zoom.smaller() {
        assert!(
            smaller.factor() < zoom.factor(),
            "stepping out has to draw the page smaller"
        );
        zoom = smaller;
    }
    assert_eq!(zoom.smaller(), None, "the smallest step offers no press");
    let mut zoom = fit.larger().unwrap();
    while let Some(larger) = zoom.larger() {
        assert!(
            larger.factor() > zoom.factor(),
            "stepping in has to draw the page larger"
        );
        zoom = larger;
    }
    assert_eq!(zoom.larger(), None, "the largest step offers no press");
    // A fitted page takes the pane's width, and every other step is the
    // size the page would print at times the step.
    assert_eq!(fit.logical_width(612.0), None);
    let printed = crate::editor::PdfZoom::Scaled(2);
    assert_eq!(printed.percent(), Some(100));
    // A point is 1/72 inch against a 96 pixel inch, so a 1000 point page
    // prints 1333.3 logical pixels wide however coarse or fine the raster
    // behind it turned out to be.
    let width = printed.logical_width(1000.0).unwrap();
    assert!(
        (width - 1333.33).abs() < 0.01,
        "{width} is not the width a 1000 point page prints at"
    );
    let doubled = crate::editor::PdfZoom::Scaled(5)
        .logical_width(1000.0)
        .unwrap();
    assert!(
        (doubled - 2.0 * width).abs() < 0.01,
        "{doubled} is not twice the printed width the 200% step promises"
    );
}

#[test]
fn a_failed_page_render_puts_back_the_page_and_tone_that_are_still_drawn() {
    let card = CardId::new();
    let mut runtime = document_runtime(card, 30);
    let (drawn_page, drawn_tone, ..) = document_view_state(&runtime);
    drop(runtime.update(RepositoryAction::PdfPage {
        card,
        page: 7,
        tone: other_document_tone(drawn_tone),
        zoom: crate::editor::PdfZoom::default(),
    }));
    assert_eq!(document_view_state(&runtime).0, 7);
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: runtime.pdf_request,
        result: Err("page 8 is too large to render".into()),
    }));
    let (page, tone, requested, message) = document_view_state(&runtime);
    assert_eq!(
        page, drawn_page,
        "the counter has to describe the page that is on screen"
    );
    assert_eq!(
        tone, drawn_tone,
        "the tone toggle has to describe the page that is on screen"
    );
    assert_eq!(requested, None);
    assert_eq!(message.as_deref(), Some("page 8 is too large to render"));
    drop(runtime.update(RepositoryAction::PdfPage {
        card,
        page: drawn_page + 1,
        tone: drawn_tone,
        zoom: crate::editor::PdfZoom::default(),
    }));
    assert_eq!(
        document_view_state(&runtime).2,
        Some(rendering_view(drawn_page + 1, drawn_tone)),
        "the next turn moves one page from the drawn one rather than skipping it"
    );
}

#[test]
fn a_render_that_fails_under_a_queued_turn_still_takes_the_reader_there() {
    let card = CardId::new();
    let mut runtime = document_runtime(card, 30);
    let tone = document_view_state(&runtime).1;
    drop(runtime.update(RepositoryAction::PdfPage {
        card,
        page: 9,
        tone,
        zoom: crate::editor::PdfZoom::default(),
    }));
    let failing = runtime.pdf_request;
    drop(runtime.update(RepositoryAction::PdfPage {
        card,
        page: 4,
        tone,
        zoom: crate::editor::PdfZoom::default(),
    }));
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: failing,
        result: Err("page 10 is too large to render".into()),
    }));
    let (page, _, requested, message) = document_view_state(&runtime);
    assert_eq!(
        requested,
        Some(rendering_view(4, tone)),
        "the queued turn still happens"
    );
    assert_eq!(page, 4);
    assert_eq!(
        message, None,
        "a failure the reader has already turned away from is not worth a notice"
    );
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: runtime.pdf_request,
        result: Ok(rendered_document_page(4, 30)),
    }));
    assert_eq!(document_view_state(&runtime).0, 4);
}

/// A shell that changes under an open document leaves the page rendered
/// for the other one, so the page is rendered again exactly once, and a
/// theme that asks for the tone already drawn asks for no render at all.
#[test]
fn a_shell_theme_change_renders_the_open_page_again_for_the_new_theme() {
    let card = CardId::new();
    let mut runtime = document_runtime(card, 4);
    // The tone the open baked, which is the one the test process's theme
    // asks for, so nothing is stale before the shell changes.
    let opened = runtime.document_tone;
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: runtime.pdf_request,
        result: Ok(rendered_document_page(0, 4)),
    }));
    assert_eq!(document_view_state(&runtime).1, opened);
    let dispatched = runtime.pdf_request;
    // The shell the open was not rendered for. Which of the two it is
    // follows from the theme the test process runs under, so the other
    // one is chosen rather than named.
    let (other, other_tone) = [cosmic::Theme::dark(), cosmic::Theme::light()]
        .into_iter()
        .map(|theme| (theme.clone(), pdf_tone(&theme)))
        .find(|(_, tone)| *tone != opened)
        .expect("the two shells ask for different tones");
    drop(runtime.retone_document(card, &other));
    let (_, tone, requested, _) = document_view_state(&runtime);
    assert_eq!(
        tone, other_tone,
        "a page rendered for one shell is wrong on the other"
    );
    assert_eq!(requested, Some(rendering_view(0, other_tone)));
    assert_eq!(runtime.pdf_request, dispatched + 1);
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: runtime.pdf_request,
        result: Ok(rendered_document_page(0, 4)),
    }));
    drop(runtime.retone_document(card, &other));
    assert_eq!(
        runtime.pdf_request,
        dispatched + 1,
        "a theme that leaves the tone alone must not rasterize the page again"
    );
}

#[test]
fn a_theme_change_while_the_first_page_rendered_is_reconciled_when_it_arrives() {
    let card = CardId::new();
    let (mut runtime, identity) = RepositoryRuntime::selection_fixture(card, "book.pdf");
    let shell = pdf_tone(&cosmic::theme::active());
    // The load was dispatched under the other shell and the reader changed
    // it while the first page rasterized, when there was no page to
    // re-render.
    runtime.document_tone = other_document_tone(shell);
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: "book.pdf".into(),
            content: FileContent::Pdf {
                pages: 4,
                page: rendered_document_page(0, 4).page,
            },
        }),
    }));
    let (page, tone, requested, _) = document_view_state(&runtime);
    assert_eq!(page, 0);
    assert_eq!(
        tone, shell,
        "the page the load baked for the old shell has to be rendered again for this one"
    );
    assert_eq!(requested, Some(rendering_view(0, shell)));
}

/// A reader who turns a page back to its own colours did so to read it.
/// The shell changing underneath is no reason to take that away, and it
/// is the only thing that stops following the theme being right.
#[test]
fn a_tone_the_reader_chose_outlives_a_theme_change() {
    let card = CardId::new();
    let mut runtime = document_runtime(card, 4);
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: runtime.pdf_request,
        result: Ok(rendered_document_page(0, 4)),
    }));
    let chosen = other_document_tone(runtime.document_tone);
    drop(runtime.update(RepositoryAction::PdfTone { card, tone: chosen }));
    drop(runtime.update(RepositoryAction::PdfPageRendered {
        card,
        request: runtime.pdf_request,
        result: Ok(rendered_document_page(0, 4)),
    }));
    let settled = runtime.pdf_request;
    assert_eq!(document_view_state(&runtime).1, chosen);
    for theme in [cosmic::Theme::dark(), cosmic::Theme::light()] {
        drop(runtime.retone_document(card, &theme));
    }
    assert_eq!(
        runtime.pdf_request, settled,
        "neither shell may rasterize over the tone the reader asked for"
    );
    assert_eq!(document_view_state(&runtime).1, chosen);
}

#[test]
fn an_edit_reports_the_whole_document_at_the_next_version() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    drop(runtime.take_lsp_intents());
    move_to_start(&mut runtime, card);

    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Change {
            version: 2,
            text: "xfn main() {}\n".into(),
        }]
    );
}

#[test]
fn an_exhausted_version_stops_reporting_changes_rather_than_rewinding() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    drop(runtime.take_lsp_intents());
    move_to_start(&mut runtime, card);
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!("the fixture opened an editable buffer");
    };
    document.lsp_version = i32::MAX;

    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    assert_eq!(
        runtime.take_lsp_intents(),
        Vec::new(),
        "the version space is spent; a wrapped version would rewind the server"
    );
    assert_eq!(
        buffer_text(&runtime),
        "xfn main() {}\n",
        "the buffer stays authoritative even when it can no longer be reported"
    );
    assert_eq!(lsp_version(&runtime), i32::MAX, "saturated, never wrapped");
}

#[test]
fn submitting_a_save_and_hitting_a_conflict_both_tell_the_server_nothing() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::submitting_a_save_and_hitting_a_conflict_both_tell_the_server_nothing",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, _) = save_fixture(card, "notes.txt", "fn main() {}\n");
    drop(runtime.take_lsp_intents());
    move_to_start(&mut runtime, card);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.take_lsp_intents());

    drop(runtime.request_save(card, false));
    assert_eq!(
        runtime.take_lsp_intents(),
        Vec::new(),
        "a submitted save has not reached disk, so `didSave` would be a lie"
    );

    let conflicted = runtime.conflicted_active_save_completion();
    drop(runtime.update(conflicted));
    assert!(runtime.has_save_conflict());
    assert_eq!(
        runtime.take_lsp_intents(),
        Vec::new(),
        "a conflict left the file on disk as it was"
    );
}

#[test]
fn a_completed_save_reports_what_reached_disk() {
    if super::retained_save_tests::isolated_test(
        "repository_widget::tests::a_completed_save_reports_what_reached_disk",
    ) {
        return;
    }
    let card = CardId::new();
    let (_directory, mut runtime, _) = save_fixture(card, "notes.txt", "fn main() {}\n");
    drop(runtime.take_lsp_intents());
    move_to_start(&mut runtime, card);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.take_lsp_intents());
    drop(runtime.request_save(card, false));

    let completion = runtime.durable_active_save_completion();
    drop(runtime.update(completion));
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Save {
            text: "xfn main() {}\n".into(),
        }]
    );
}

#[test]
fn discarding_edits_resynchronises_the_server_with_the_saved_text() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    drop(runtime.take_lsp_intents());
    move_to_start(&mut runtime, card);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.take_lsp_intents());

    drop(runtime.update(RepositoryAction::DiscardEdits { card }));
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Change {
            version: 3,
            text: "fn main() {}\n".into(),
        }],
        "discarding rebuilds the buffer without going through an editor action"
    );
}

#[test]
fn abandoning_unsaved_changes_resynchronises_the_server() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    drop(runtime.take_lsp_intents());
    move_to_start(&mut runtime, card);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    drop(runtime.take_lsp_intents());

    assert!(runtime.abandon_unsaved_changes());
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Change {
            version: 3,
            text: "fn main() {}\n".into(),
        }],
        "the dirty-close dialog discards the buffer behind the editor's back"
    );
}

#[test]
fn reloading_closes_the_document_before_opening_it_again() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    drop(runtime.take_lsp_intents());

    drop(runtime.update(RepositoryAction::Reload { card }));
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Close],
        "the reloaded buffer is a different document, however identical its path"
    );

    let identity = AsyncIdentity {
        card,
        worktree: PathBuf::from("/repo"),
        path: PathBuf::from("notes.txt"),
        generation: runtime.generation,
    };
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: "notes.txt".into(),
            content: FileContent::Editable {
                text: "fn main() {}\n".into(),
                content_hash: blake3::hash(b"fn main() {}\n"),
                markdown: false,
            },
        }),
    }));
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Open {
            absolute_path: PathBuf::from("/repo/notes.txt"),
            worktree: PathBuf::from("/repo"),
            version: 1,
            text: "fn main() {}\n".into(),
        }]
    );
}

#[test]
fn opening_another_file_closes_the_one_it_replaces() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    drop(runtime.take_lsp_intents());

    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: PathBuf::from("other.txt"),
    }));
    assert_eq!(runtime.take_lsp_intents(), vec![LspIntent::Close]);
}

#[test]
fn returning_to_the_file_list_closes_the_document() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    drop(runtime.take_lsp_intents());

    drop(runtime.update(RepositoryAction::BackToFiles { card }));
    assert_eq!(runtime.take_lsp_intents(), vec![LspIntent::Close]);
    assert!(runtime.cursor_position().is_none());
}

#[test]
fn the_document_identity_is_stored_for_the_application_that_minted_it() {
    let card = CardId::new();
    // Loading alone does not register a protocol document. Unlike the LSP
    // fixture, this exercises the state before the frontend mints its ID.
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "fn main() {}\n");
    assert_eq!(runtime.lsp_document(), None);
    runtime.set_lsp_document(Some(LspDocumentId(7)));
    assert_eq!(runtime.lsp_document(), Some(LspDocumentId(7)));
    runtime.set_lsp_document(None);
    assert_eq!(runtime.lsp_document(), None);
}

#[test]
fn caret_columns_convert_between_bytes_and_scalars() {
    // Thirteen scalars in seventeen bytes: the accent and the arrow are
    // the whole point.
    let line = "héllo → wörld";
    assert_eq!(line.len(), 17);
    assert_eq!(scalar_column(line, line.len()), 13);
    assert_eq!(byte_column(line, 13), 17);
    assert_eq!(scalar_column(line, 7), 6, "the arrow starts at byte seven");
    assert_eq!(byte_column(line, 6), 7);
    assert_eq!(
        scalar_column(line, 8),
        6,
        "a byte index inside the arrow rounds down instead of panicking"
    );
    assert_eq!(scalar_column(line, 9), 6);
    assert_eq!(
        byte_column(line, 99),
        17,
        "a character past the end clamps to the end of the line"
    );
    assert_eq!(scalar_column(line, 99), 13);
}

#[test]
fn the_caret_round_trips_through_protocol_coordinates_on_a_non_ascii_line() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "héllo → wörld\nplain\n");
    drop(runtime.take_lsp_intents());

    for at in [
        LspPosition::new(0, 0),
        LspPosition::new(0, 6),
        LspPosition::new(0, 9),
        LspPosition::new(0, 13),
        LspPosition::new(1, 5),
    ] {
        assert!(runtime.move_cursor_to(at), "line {} exists", at.line);
        assert_eq!(
            runtime.cursor_position(),
            Some(at),
            "a caret placed at {at:?} must be reported there, not at its byte offset"
        );
    }
    assert_eq!(
        runtime.take_lsp_intents(),
        Vec::new(),
        "jumping the caret changes no text"
    );
    assert!(
        !runtime.move_cursor_to(LspPosition::new(9, 0)),
        "a line the buffer does not have is refused rather than clamped"
    );
}

#[test]
fn select_text_selects_first_match_and_clears_missing_matches() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "Alpha beta ALPHA");

    assert!(runtime.select_text("BETA"));
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    let cursor = document.content.cursor();
    assert_eq!(
        cursor.position,
        text_editor::Position {
            line: 0,
            column: 10,
        }
    );
    assert_eq!(
        cursor.selection,
        Some(text_editor::Position { line: 0, column: 6 })
    );

    assert_eq!(runtime.select_text_match("alpha", 1), Some(2));
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    let cursor = document.content.cursor();
    assert_eq!(
        cursor.position,
        text_editor::Position {
            line: 0,
            column: 16,
        }
    );
    assert_eq!(
        cursor.selection,
        Some(text_editor::Position {
            line: 0,
            column: 11,
        })
    );

    assert!(!runtime.select_text("missing"));
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    assert_eq!(document.content.cursor().selection, None);
}

#[test]
fn the_caret_round_trips_across_windows_line_endings() {
    let card = CardId::new();
    let (mut runtime, _) =
        RepositoryRuntime::lsp_open_fixture(card, "let a = 1;\r\nlet b = 2;\r\n");
    drop(runtime.take_lsp_intents());

    for at in [LspPosition::new(0, 10), LspPosition::new(1, 4)] {
        assert!(runtime.move_cursor_to(at));
        assert_eq!(
            runtime.cursor_position(),
            Some(at),
            "a `\\r\\n` is one terminator and belongs to neither line's content"
        );
    }
}

#[test]
fn a_bare_carriage_return_is_a_line_break_to_the_editor_and_not_to_the_protocol() {
    // The editor's `LineIter` breaks on a lone `\r`; `crate::lsp` counts
    // `\n` alone. This buffer therefore has three lines to the editor and
    // two to the protocol, and a caret that survives the trip proves the
    // conversion goes through byte offsets rather than through a line
    // index that means different things on each side.
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "old\rmac\n");
    drop(runtime.take_lsp_intents());

    let inside_the_second_editor_line = LspPosition::new(0, 5);
    assert!(runtime.move_cursor_to(inside_the_second_editor_line));
    assert_eq!(
        runtime.cursor_position(),
        Some(inside_the_second_editor_line)
    );
    assert!(
        !runtime.move_cursor_to(LspPosition::new(2, 0)),
        "the protocol sees two lines here, whatever the editor does"
    );
}

/// The external caret for the editor-native caret at `line` and byte
/// `column` in a fresh buffer holding `text`.
fn external_caret_at(
    text: &str,
    line: usize,
    column: usize,
) -> Option<crate::external_editor::Caret> {
    let (mut runtime, _) = RepositoryRuntime::open_fixture(CardId::new(), "notes.txt", text);
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    document.content.set_cursor(text_editor::Cursor {
        position: text_editor::Position { line, column },
        selection: None,
    });
    runtime.external_caret()
}

#[test]
fn the_external_caret_counts_lines_the_way_external_editors_break_them() {
    use crate::external_editor::Caret;
    // Every terminator the editor knows, one per line: `\n`, `\r\n`, a
    // lone `\r`, and `\n\r`, which the built-in editor reads as one
    // terminator and VS Code and Zed read as two line breaks.
    let text = "one\ntwo\r\nold\rmac\n\rlast é→😀x\n";
    assert_eq!(external_caret_at(text, 0, 2), Some(Caret::new(0, "on")));
    assert_eq!(external_caret_at(text, 1, 3), Some(Caret::new(1, "two")));
    assert_eq!(external_caret_at(text, 2, 0), Some(Caret::new(2, "")));
    assert_eq!(external_caret_at(text, 3, 1), Some(Caret::new(3, "m")));
    // The column is a byte offset into the editor line; this one is after
    // the emoji.
    assert_eq!(
        external_caret_at(text, 4, "last é→😀".len()),
        Some(Caret::new(5, "last é→😀"))
    );
    // A `\n\r` that ends the buffer is two breaks as well.
    assert_eq!(external_caret_at("x\n\r", 1, 0), Some(Caret::new(2, "")));
}

/// The built-in editor reads `\n\r\n` as `\n\r` and then `\n`, leaving
/// an empty line between the `\r` and its `\n`. External editors read
/// `\n` and then `\r\n`: two breaks, not three.
#[test]
fn the_external_caret_reads_a_newline_before_crlf_as_two_breaks() {
    use crate::external_editor::Caret;
    let text = "a\n\r\nb\n";
    assert_eq!(external_caret_at(text, 2, 1), Some(Caret::new(2, "b")));
    // Between the `\r` and its `\n` is no place an external caret can
    // be; the nearest is the start of the empty line before them.
    assert_eq!(external_caret_at(text, 1, 0), Some(Caret::new(1, "")));
    // `\n`, `\r\n`, then a lone `\r`.
    assert_eq!(
        external_caret_at("a\n\r\n\rb", 2, 1),
        Some(Caret::new(3, "b"))
    );
}

/// Editing can leave a lone `\r` followed by an empty line ending in
/// `\n`, which saves as `\r\n`: one break, not two.
#[test]
fn the_external_caret_pairs_a_lone_carriage_return_with_the_next_newline() {
    use crate::external_editor::Caret;
    let (mut runtime, _) = RepositoryRuntime::open_fixture(CardId::new(), "notes.txt", "a\rx\nb");
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    document.content.set_cursor(text_editor::Cursor {
        position: text_editor::Position { line: 1, column: 0 },
        selection: None,
    });
    document
        .content
        .perform(text_editor::Action::Edit(text_editor::Edit::Delete));
    assert_eq!(document.content.text(), "a\r\nb");
    assert_eq!(
        document
            .content
            .with_line(0, |text, ending| (text.to_owned(), ending)),
        Some(("a".to_owned(), text_editor::LineEnding::Cr))
    );
    assert_eq!(
        document
            .content
            .with_line(1, |text, ending| (text.to_owned(), ending)),
        Some((String::new(), text_editor::LineEnding::Lf))
    );
    let mut caret_at = |line, column| {
        let DocumentState::Editable(document) = &mut runtime.document else {
            unreachable!();
        };
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position { line, column },
            selection: None,
        });
        runtime.external_caret()
    };
    assert_eq!(caret_at(2, 1), Some(Caret::new(1, "b")));
    // Between the `\r` and the `\n`: the nearest external place is the
    // end of `a`.
    assert_eq!(caret_at(1, 0), Some(Caret::new(0, "a")));
    assert_eq!(caret_at(0, 1), Some(Caret::new(0, "a")));
}

#[test]
fn there_is_no_external_caret_without_the_selected_file_open_as_text() {
    let card = CardId::new();
    let (runtime, _) = RepositoryRuntime::selection_fixture(card, "notes.txt");
    assert_eq!(runtime.external_caret(), None);
}

/// A caret in one file must never position another.
#[test]
fn there_is_no_external_caret_when_the_open_buffer_is_not_the_selected_file() {
    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(CardId::new(), "notes.txt", "one\ntwo\n");
    assert!(runtime.external_caret().is_some());
    runtime
        .selected
        .as_mut()
        .expect("the fixture selects its file")
        .path = PathBuf::from("other.txt");
    assert_eq!(runtime.external_caret(), None);
}

#[test]
fn a_caret_after_a_stripped_carriage_return_reports_a_position_the_protocol_can_hold() {
    // `crate::lsp` drops a `\r` sitting immediately before a line's
    // terminating `\n`, or at the end of the buffer, from that line's
    // content — while the editor treats the same `\r` as a terminator of
    // its own and will park a caret after it. A column that counted the
    // `\r` would name a character the protocol's line does not have, which
    // `Snapshot::utf16_position` refuses with `PositionOutOfRange`: every hover,
    // completion and definition asked from that caret would silently fail.
    assert_eq!(
        position_for_offset("a\r", 2),
        LspPosition::new(0, 1),
        "the buffer's trailing bare `\\r` is not a character of line zero"
    );
    assert_eq!(
        position_for_offset("x\n\r\ny", 3),
        LspPosition::new(1, 0),
        "the editor reads `\\n\\r` as one terminator, the protocol reads `\\r\\n`; \
             either way line one is empty"
    );

    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "a\r");
    drop(runtime.take_lsp_intents());
    // The end of this buffer is a place `move_cursor_to` cannot name, so
    // the caret is driven there the way a keypress would.
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Move(text_editor::Motion::DocumentEnd),
    }));
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(0, 1)));
    assert_eq!(runtime.take_lsp_intents(), Vec::new());
}

#[test]
fn every_reported_position_lands_inside_its_own_protocol_line() {
    // Exhaustive over the shapes that pull the editor's line numbering
    // apart from the protocol's: every terminator the editor recognises, a
    // multi-byte scalar, and an unterminated final line, in every order up
    // to four pieces.
    let atoms = ["", "a", "\n", "\r", "\r\n", "\n\r", "é"];
    let mut texts = vec![String::new()];
    for _ in 0..4 {
        texts = texts
            .iter()
            .flat_map(|text| atoms.iter().map(move |atom| format!("{text}{atom}")))
            .collect();
    }

    for text in &texts {
        for offset in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
            let at = position_for_offset(text, offset);
            let span =
                line_span(text, at.line).expect("the line was counted out of this very text");
            let scalars = text[span].chars().count();
            assert!(
                usize::try_from(at.character).unwrap() <= scalars,
                "{text:?} offset {offset} reported {at:?}, but that line holds \
                     {scalars} scalars — `Snapshot::utf16_position` would refuse it"
            );
            let addressed =
                byte_offset(text, at).expect("a reported position addresses a real line");
            assert_eq!(
                position_for_offset(text, addressed),
                at,
                "{text:?} offset {offset} must survive a second conversion unchanged"
            );
        }
    }
}

#[test]
fn a_completion_applies_every_edit_and_reports_one_version() {
    let card = CardId::new();
    let (mut runtime, _) =
        RepositoryRuntime::lsp_open_fixture(card, "use std::fmt;\n\nfn main() {\n    prin\n}\n");
    drop(runtime.take_lsp_intents());

    let item = completion_item(
        completion_edit(
            LspRange::new(LspPosition::new(3, 4), LspPosition::new(3, 8)),
            "println!()",
        ),
        vec![completion_edit(
            LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 0)),
            "use std::io;\n",
        )],
    );
    assert!(runtime.apply_completion(&item));

    let expected = "use std::fmt;\nuse std::io;\n\nfn main() {\n    println!()\n}\n";
    assert_eq!(buffer_text(&runtime), expected);
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Change {
            version: 2,
            text: expected.into(),
        }],
        "one completion is one document transition, not one per edit"
    );
    assert_eq!(lsp_version(&runtime), 2);
    assert_eq!(
        runtime.cursor_position(),
        Some(LspPosition::new(4, 14)),
        "the caret follows the text the user chose, past the edit inserted above it"
    );
    assert!(runtime.is_dirty());
}

#[test]
fn typing_without_an_attached_server_never_serializes_content() {
    let card = CardId::new();
    let source = "let value = 1;\n".repeat(50_000);
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "huge.rs", &source);
    drop(runtime.take_lsp_intents());
    text_editor::take_text_call_count();
    for character in "changed".chars() {
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert(character)),
        }));
        assert!(runtime.take_lsp_intents().is_empty());
    }
    assert!(runtime.is_dirty());
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert!(runtime.take_lsp_intents().is_empty());
    assert_eq!(text_editor::take_text_call_count(), 0);
}

#[test]
fn diagnostic_navigation_uses_bounded_plain_text_and_shared_explicit_copy() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "first\nsecond\n");
    let item = crate::lsp::LspDiagnostic::new(
        LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 1)),
        crate::lsp::LspSeverity::Error,
        "λ".repeat(crate::lsp::DIAGNOSTIC_PREVIEW_BYTES) + " FULL MESSAGE END",
        None,
        None,
    );
    runtime.set_editor_diagnostics(vec![item.clone()]);
    runtime.navigate_diagnostic(true);
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(1, 0)));
    assert!(
        runtime.language.hover.is_empty(),
        "diagnostics bypass Markdown parsing"
    );
    let copy = runtime.language.hover_diagnostic.as_ref().unwrap();
    let text = copy.message_preview();
    assert!(text.len() <= crate::lsp::DIAGNOSTIC_PREVIEW_BYTES);
    assert!(text.contains("preview truncated"));
    assert!(!text.contains("FULL MESSAGE END"));
    assert!(std::sync::Arc::ptr_eq(&copy.protocol, &item.protocol));
    assert!(copy.copy_text().contains("FULL MESSAGE END"));
    runtime.set_editor_diagnostics(vec![]);
    assert!(runtime.language.hover_diagnostic.is_none());
    assert!(runtime.language.hover.is_empty());
}

#[test]
fn diagnostic_navigation_never_parses_html_metadata_or_empty_messages() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "first\nsecond\n");
    for message in [
        "<!--".to_owned() + &"x".repeat(crate::lsp::DIAGNOSTIC_PREVIEW_BYTES),
        "---\n".to_owned() + &"x".repeat(crate::lsp::DIAGNOSTIC_PREVIEW_BYTES),
        String::new(),
    ] {
        let item = crate::lsp::LspDiagnostic::new(
            LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 1)),
            crate::lsp::LspSeverity::Error,
            message.clone(),
            None,
            None,
        );
        runtime.set_editor_diagnostics(vec![item.clone()]);
        runtime.navigate_diagnostic(true);
        assert!(runtime.language.hover.is_empty());
        let shown = runtime
            .language
            .hover_diagnostic
            .as_ref()
            .expect("plain popup and copy remain available");
        assert!(std::sync::Arc::ptr_eq(&shown.protocol, &item.protocol));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&shown.copy_text()).unwrap()["message"],
            message
        );
        if !message.is_empty() {
            assert!(shown.message_preview().contains("preview truncated"));
        }
        runtime.dismiss_editor_results();
        assert!(runtime.language.hover_diagnostic.is_none());
    }
    runtime.set_editor_hover(
        Some("**actual hover markup**"),
        Some(LspPosition::new(0, 0)),
    );
    assert!(runtime.language.hover_diagnostic.is_none());
    assert!(
        !runtime.language.hover.is_empty(),
        "actual server hover still uses Markdown"
    );
}

#[test]
fn diagnostic_navigation_wraps_and_displays_the_message() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "first\nsecond\nthird\n");
    runtime.set_editor_diagnostics(vec![
        crate::lsp::LspDiagnostic::new(
            LspRange::new(LspPosition::new(2, 0), LspPosition::new(2, 3)),
            crate::lsp::LspSeverity::Error,
            "third error".into(),
            None,
            None,
        ),
        crate::lsp::LspDiagnostic::new(
            LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 3)),
            crate::lsp::LspSeverity::Warning,
            "second warning".into(),
            None,
            None,
        ),
    ]);
    runtime.move_cursor_to(LspPosition::new(0, 0));
    runtime.navigate_diagnostic(true);
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(1, 0)));
    assert!(runtime.language.hover_diagnostic.is_some());
    runtime.navigate_diagnostic(true);
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(2, 0)));
    runtime.navigate_diagnostic(true);
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(1, 0)));
    runtime.navigate_diagnostic(false);
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(2, 0)));
}

#[test]
fn a_completion_is_one_undo_step_that_restores_the_caret() {
    let card = CardId::new();
    let original = "use std::fmt;\n\nfn main() {\n    prin\n}\n";
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, original);
    drop(runtime.take_lsp_intents());
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Move(text_editor::Motion::Down),
    }));
    let item = completion_item(
        completion_edit(
            LspRange::new(LspPosition::new(3, 4), LspPosition::new(3, 8)),
            "println!()",
        ),
        vec![completion_edit(
            LspRange::new(LspPosition::new(1, 0), LspPosition::new(1, 0)),
            "use std::io;\n",
        )],
    );
    let caret = runtime.cursor_position();
    assert!(runtime.apply_completion(&item));
    drop(runtime.take_lsp_intents());

    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(
        buffer_text(&runtime),
        original,
        "both edits come back as one"
    );
    assert_eq!(runtime.cursor_position(), caret);
    assert!(!runtime.is_dirty());
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(
        buffer_text(&runtime),
        "use std::fmt;\nuse std::io;\n\nfn main() {\n    println!()\n}\n"
    );
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(4, 14)));
}

#[test]
fn undo_and_redo_step_through_typing_and_report_exact_edits() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "fn main() {}\n");
    // Scheduler pauses in a loaded parallel suite must not become a user
    // typing pause. Explicit idle-boundary coverage lives in editor_history.
    let DocumentState::Editable(document) = &mut runtime.document else {
        panic!("editable fixture");
    };
    document.history_time = Some(Instant::now());
    drop(runtime.take_lsp_intents());
    move_to_start(&mut runtime, card);
    for character in ['a', 'b'] {
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert(character)),
        }));
    }
    drop(runtime.take_lsp_intents());
    drop(runtime.take_lsp_edits());
    let revision = document_revision(&runtime);

    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(buffer_text(&runtime), "fn main() {}\n");
    assert!(
        !runtime.is_dirty(),
        "undoing to the saved text is not a modification"
    );
    assert_ne!(
        document_revision(&runtime),
        revision,
        "autosave must see the change"
    );
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(0, 0)));
    assert_eq!(
        runtime.take_lsp_intents(),
        vec![LspIntent::Change {
            version: 4,
            text: "fn main() {}\n".into(),
        }]
    );
    assert_eq!(
        runtime.take_lsp_edits(),
        vec![LspTextEdit {
            range: LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 2)),
            new_text: String::new(),
        }]
    );

    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(buffer_text(&runtime), "abfn main() {}\n");
    assert!(runtime.is_dirty());
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(0, 2)));

    // Nothing left to redo, and undo with nothing to undo, are no-ops the
    // server never hears about.
    drop(runtime.take_lsp_intents());
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(runtime.take_lsp_intents(), Vec::new());
}

#[test]
fn discarding_unsaved_edits_can_itself_be_undone() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "one\ntwo\n");
    drop(runtime.take_lsp_intents());
    move_to_start(&mut runtime, card);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("zero\n".into()))),
    }));
    assert!(runtime.is_dirty());

    drop(runtime.update(RepositoryAction::DiscardEdits { card }));
    assert_eq!(buffer_text(&runtime), "one\ntwo\n");
    assert!(!runtime.is_dirty());

    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(
        buffer_text(&runtime),
        "zero\none\ntwo\n",
        "a discard is a step like any other, so the edits it dropped come back"
    );
    assert!(runtime.is_dirty());
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(buffer_text(&runtime), "one\ntwo\n");
    drop(runtime.update(RepositoryAction::Redo { card }));
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(
        buffer_text(&runtime),
        "one\ntwo\n",
        "redo replays the discard too"
    );
}

#[test]
fn undo_and_redo_work_while_find_stays_open() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "alpha\n");
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('z')),
    }));
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(buffer_text(&runtime), "alpha\n");
    assert!(runtime.find_is_open());
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(buffer_text(&runtime), "zalpha\n");
    assert!(runtime.find_is_open());
}

#[test]
fn a_completion_whose_edits_overlap_is_refused_rather_than_half_applied() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "let value = 1;\n");
    drop(runtime.take_lsp_intents());

    let overlapping = completion_item(
        completion_edit(
            LspRange::new(LspPosition::new(0, 4), LspPosition::new(0, 9)),
            "other",
        ),
        vec![completion_edit(
            LspRange::new(LspPosition::new(0, 6), LspPosition::new(0, 11)),
            "clash",
        )],
    );
    assert!(!runtime.apply_completion(&overlapping));

    let unaddressable = completion_item(
        completion_edit(
            LspRange::new(LspPosition::new(40, 0), LspPosition::new(40, 1)),
            "nowhere",
        ),
        Vec::new(),
    );
    assert!(!runtime.apply_completion(&unaddressable));

    assert_eq!(buffer_text(&runtime), "let value = 1;\n");
    assert_eq!(lsp_version(&runtime), 1);
    assert_eq!(
        runtime.take_lsp_intents(),
        Vec::new(),
        "a refused completion is not a document change"
    );
    assert!(!runtime.is_dirty());
}

#[test]
fn a_snippet_completion_inserts_its_literal_text() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "    prin\n");
    drop(runtime.take_lsp_intents());

    let mut edit = completion_edit(
        LspRange::new(LspPosition::new(0, 4), LspPosition::new(0, 8)),
        "println!()",
    );
    edit.snippet = Some(LspSnippet {
        text: "println!()".into(),
        stops: vec![LspSnippetStop {
            index: 1,
            range: 9..9,
        }],
    });
    assert!(runtime.apply_completion(&completion_item(edit, Vec::new())));
    assert_eq!(
        buffer_text(&runtime),
        "    println!()\n",
        "tab stops are carried but not entered in this pass"
    );
}

#[test]
fn nothing_is_reported_for_a_card_with_no_editable_buffer() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_selection_fixture(card);
    assert_eq!(runtime.cursor_position(), None);
    assert!(!runtime.move_cursor_to(LspPosition::new(0, 0)));
    assert!(!runtime.apply_completion(&completion_item(
        completion_edit(LspRange::default(), "x"),
        Vec::new()
    )));
    assert_eq!(runtime.take_lsp_intents(), Vec::new());
}

/// A basis switch is a new request. The diff still in flight for the basis
/// the user left must not be able to answer for the one they moved to, and
/// a request the counter has already superseded must not answer at all.
#[test]
fn a_diff_for_a_superseded_basis_cannot_answer_for_the_current_one() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    let commit = DiffBasis::CommitToParent {
        oid: fixture_oid(1),
        parent: Some(fixture_oid(2)),
    };

    // The unstaged diff is in flight when the user switches to the commit
    // comparison, which starts a request of its own.
    runtime.diff_basis = DiffBasis::IndexToWorktree;
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::Loading {
            request: 1,
            cancellation: CancellationToken::new(),
        },
    );
    runtime.next_diff_request = 1;
    let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
        card,
        basis: commit.clone(),
    });
    assert_eq!(runtime.diff_basis, commit);
    assert!(matches!(
        runtime.diff_state(&commit),
        Some(DiffState::Loading { request: 2, .. })
    ));

    // A cancelled answer may still arrive. It must neither recreate its
    // slot nor reset the viewport/change cursor of the active comparison.
    runtime.diff_viewport = ListViewport {
        offset: 480.0,
        height: 300.0,
    };
    runtime.diff_change = 3;
    let _ = runtime.update(RepositoryAction::DiffLoaded {
        identity: identity.clone(),
        basis: DiffBasis::IndexToWorktree,
        request: 1,
        content: staged_text_diff(model_with(9)),
    });
    assert!(
        matches!(
            runtime.diff_state(&commit),
            Some(DiffState::Loading { request: 2, .. })
        ),
        "the superseded basis answered for the current one"
    );
    assert!(runtime.diff_model().is_none());
    assert_eq!(runtime.diff_viewport.offset, 480.0);
    assert_eq!(runtime.diff_change, 3);
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_none());

    // A superseded request for the *current* basis is dropped outright.
    let _ = runtime.update(RepositoryAction::DiffLoaded {
        identity,
        basis: commit.clone(),
        request: 1,
        content: staged_text_diff(model_with(7)),
    });
    assert!(matches!(
        runtime.diff_state(&commit),
        Some(DiffState::Loading { request: 2, .. })
    ));
}

/// A resolution nobody is waiting for cannot install a basis: the
/// selection, or the choice, has moved on since it started.
#[test]
fn a_stale_basis_resolution_cannot_install_a_basis() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    runtime.diff_basis = DiffBasis::IndexToWorktree;

    // Nothing outstanding at all.
    let _ = runtime.update(RepositoryAction::DiffBasisResolved {
        identity: identity.clone(),
        choice: CommittedChoice::Branch,
        result: Ok(ResolvedBasis::Ready(DiffBasis::HeadToIndex)),
    });
    assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
    assert!(runtime.basis_resolution.is_none());

    // Outstanding, but for the other committed choice.
    runtime.basis_resolution = Some(BasisResolution::Resolving(CommittedChoice::LastCommit));
    let _ = runtime.update(RepositoryAction::DiffBasisResolved {
        identity: identity.clone(),
        choice: CommittedChoice::Branch,
        result: Ok(ResolvedBasis::Ready(DiffBasis::HeadToIndex)),
    });
    assert_eq!(
        runtime.basis_resolution,
        Some(BasisResolution::Resolving(CommittedChoice::LastCommit))
    );

    // Outstanding for this choice, but from a selection the card has left.
    runtime.basis_resolution = Some(BasisResolution::Resolving(CommittedChoice::Branch));
    let _ = runtime.update(RepositoryAction::DiffBasisResolved {
        identity: AsyncIdentity {
            generation: 99,
            ..identity.clone()
        },
        choice: CommittedChoice::Branch,
        result: Ok(ResolvedBasis::Ready(DiffBasis::HeadToIndex)),
    });
    assert_eq!(
        runtime.basis_resolution,
        Some(BasisResolution::Resolving(CommittedChoice::Branch))
    );

    // The one that is waited for is accepted, and clears the wait.
    let _ = runtime.update(RepositoryAction::DiffBasisResolved {
        identity,
        choice: CommittedChoice::Branch,
        result: Ok(ResolvedBasis::Ready(DiffBasis::HeadToIndex)),
    });
    assert!(runtime.basis_resolution.is_none());
}

/// The branch comparison's default base is the remote's own symbolic
/// `HEAD`. Without one, nothing is guessed: the refs that could serve as a
/// base are offered instead.
#[test]
fn a_branch_basis_reports_a_missing_default_base_and_offers_the_refs() {
    let root = tempdir().unwrap();
    git_fixture(root.path());
    std::fs::write(root.path().join("file.txt"), "one\n").unwrap();
    run_git(root.path(), ["add", "file.txt"]);
    run_git(root.path(), ["commit", "-qm", "base"]);
    run_git(root.path(), ["branch", "release"]);

    let resolved = resolve_basis(root.path(), CommittedChoice::Branch).unwrap();
    let ResolvedBasis::NeedsBase { candidates } = &resolved else {
        panic!("a repository with no remote HEAD has no default base, got {resolved:?}");
    };
    assert_eq!(
        candidates
            .iter()
            .map(ValidatedRefName::as_str)
            .collect::<Vec<_>>(),
        vec!["refs/heads/release"],
        "the checked-out branch is not a base for itself"
    );

    // A symbolic remote HEAD is the default base, and is used verbatim.
    run_git(
        root.path(),
        ["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    run_git(
        root.path(),
        [
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    assert_eq!(
        resolve_basis(root.path(), CommittedChoice::Branch).unwrap(),
        ResolvedBasis::Ready(DiffBasis::MergeBaseToHead {
            base_ref: ValidatedRefName::parse("refs/remotes/origin/main").unwrap(),
            head: head_oid(root.path()),
            merge_base: head_oid(root.path()),
        })
    );
}

/// The commit comparison is `HEAD` against its own first parent. A root
/// commit has none, which the service compares against the empty tree, and
/// an unborn branch has no commit at all.
#[test]
fn the_commit_basis_resolves_head_against_its_own_parent() {
    let root = tempdir().unwrap();
    git_fixture(root.path());
    assert_eq!(
        resolve_basis(root.path(), CommittedChoice::LastCommit).unwrap(),
        ResolvedBasis::NoCommit
    );

    std::fs::write(root.path().join("file.txt"), "one\n").unwrap();
    run_git(root.path(), ["add", "file.txt"]);
    run_git(root.path(), ["commit", "-qm", "root"]);
    let first = head_oid(root.path());
    assert_eq!(
        resolve_basis(root.path(), CommittedChoice::LastCommit).unwrap(),
        ResolvedBasis::Ready(DiffBasis::CommitToParent {
            oid: first.clone(),
            parent: None,
        })
    );

    std::fs::write(root.path().join("file.txt"), "one\ntwo\n").unwrap();
    run_git(root.path(), ["commit", "-qam", "second"]);
    assert_eq!(
        resolve_basis(root.path(), CommittedChoice::LastCommit).unwrap(),
        ResolvedBasis::Ready(DiffBasis::CommitToParent {
            oid: head_oid(root.path()),
            parent: Some(first),
        })
    );
}

/// With no default base the card says so and offers the choice; picking one
/// installs that comparison and clears the outstanding resolution.
#[test]
fn an_unresolvable_branch_base_offers_the_choice_instead_of_guessing() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    runtime.diff_basis = DiffBasis::IndexToWorktree;
    let candidate = ValidatedRefName::parse("refs/remotes/origin/main").unwrap();
    runtime.basis_resolution = Some(BasisResolution::Resolving(CommittedChoice::Branch));

    let _ = runtime.update(RepositoryAction::DiffBasisResolved {
        identity,
        choice: CommittedChoice::Branch,
        result: Ok(ResolvedBasis::NeedsBase {
            candidates: vec![candidate.clone()],
        }),
    });
    assert_eq!(
        runtime.basis_resolution,
        Some(BasisResolution::NeedsBase(vec![candidate.clone()]))
    );
    let context = runtime.inspector_context();
    assert_eq!(context.diff_state, Some("Choose a base"));
    assert_eq!(
        runtime.diff_basis,
        DiffBasis::IndexToWorktree,
        "no base may be guessed"
    );
    assert!(!context.can_retry_diff);
    let _ = diff_surface(card, &runtime, None);

    // The same surface with nothing to offer still says what is wrong.
    runtime.basis_resolution = Some(BasisResolution::NeedsBase(Vec::new()));
    let _ = diff_surface(card, &runtime, None);
    assert_eq!(
        runtime.inspector_context().diff_state,
        Some("Choose a base")
    );

    runtime.basis_resolution = Some(BasisResolution::NeedsBase(vec![candidate.clone()]));
    let basis = DiffBasis::MergeBaseToHead {
        base_ref: candidate,
        head: fixture_oid(1),
        merge_base: fixture_oid(2),
    };
    let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
        card,
        basis: basis.clone(),
    });
    assert_eq!(runtime.diff_basis, basis);
    assert!(runtime.basis_resolution.is_none());
    assert!(matches!(
        runtime.diff_state(&basis),
        Some(DiffState::Loading { .. })
    ));
}

/// A committed comparison does not depend on local status: a file with no
/// staged half can still be compared against committed history, and
/// choosing that comparison is what starts reading it.
#[test]
fn a_committed_basis_is_offered_for_a_file_with_no_staged_half() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    let selected = runtime
        .selected
        .as_mut()
        .expect("the fixture selects a file");
    selected.staged = false;
    runtime.diff_basis = DiffBasis::IndexToWorktree;
    let branch = DiffBasis::MergeBaseToHead {
        base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        head: fixture_oid(1),
        merge_base: fixture_oid(2),
    };

    let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
        card,
        basis: DiffBasis::HeadToIndex,
    });
    assert_eq!(
        runtime.diff_basis,
        DiffBasis::IndexToWorktree,
        "a file with no staged half has no staged comparison"
    );

    let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
        card,
        basis: branch.clone(),
    });
    assert_eq!(runtime.diff_basis, branch);
    assert!(matches!(
        runtime.diff_state(&branch),
        Some(DiffState::Loading { .. })
    ));
}

/// A file with no local change at all still has a diff surface, and still
/// reaches the committed comparisons — which is the only way to review a
/// file the branch changed and the worktree has not.
#[test]
fn a_file_with_no_local_change_still_reaches_a_committed_comparison() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    let selected = runtime
        .selected
        .as_mut()
        .expect("the fixture selects a file");
    selected.staged = false;
    selected.unstaged = false;
    selected.status = None;
    runtime.source_surface = SourceSurface::Editor;
    runtime.diff_basis = DiffBasis::HeadToIndex;

    let _ = runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true });
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
    assert!(runtime.diff_state(&DiffBasis::HeadToIndex).is_none());
    assert!(!runtime.can_show_diff());

    let commit = DiffBasis::CommitToParent {
        oid: fixture_oid(4),
        parent: Some(fixture_oid(5)),
    };
    let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
        card,
        basis: commit.clone(),
    });
    assert_eq!(runtime.diff_basis, commit);
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert!(runtime.can_show_diff());
    assert!(matches!(
        runtime.diff_state(&commit),
        Some(DiffState::Loading { .. })
    ));

    // With no file selected there is no diff surface to open.
    runtime.selected = None;
    runtime.source_surface = SourceSurface::Editor;
    let _ = runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true });
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
}

/// A committed comparison that does not resolve replaces the body of the
/// diff surface. The control that chose it must survive that, or the user
/// is stranded in a basis with their own diff still cached behind it.
#[test]
fn an_unresolved_comparison_still_offers_the_way_back() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(staged_text_diff(model_with(1))),
    );
    runtime.source_surface = SourceSurface::Diff;

    for resolution in [
        BasisResolution::Resolving(CommittedChoice::Branch),
        // No candidates at all, so even the ref selector offers nothing.
        BasisResolution::NeedsBase(Vec::new()),
        BasisResolution::NoCommits,
    ] {
        runtime.basis_resolution = Some(resolution.clone());
        // The diff the user was reading is still cached, and the basis
        // never moved - so changing basis back is a real escape.
        assert_eq!(runtime.diff_basis, DiffBasis::IndexToWorktree);
        assert!(matches!(
            runtime.diff_state(&DiffBasis::IndexToWorktree),
            Some(DiffState::Ready(RenderedDiff::Text { .. }, _))
        ));
        let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: DiffBasis::IndexToWorktree,
        });
        assert!(runtime.basis_resolution.is_none());
    }
}

/// A committed basis is named by a commit id, so the per-basis diff cache
/// has an open keyspace where the two working bases were a closed one.
/// Stepping through history must not retain one diff model per commit
/// visited, and the eviction must never take the comparison on screen, a
/// working basis, or resurrect an abandoned slot.
#[test]
fn stepping_through_commits_does_not_retain_a_diff_per_commit() {
    let card = CardId::new();
    let (mut runtime, identity) = diff_runtime(card);
    // Previously viewed working comparisons remain cached.
    for basis in [DiffBasis::HeadToIndex, DiffBasis::IndexToWorktree] {
        runtime.set_diff_state(basis, DiffState::ready(staged_text_diff(model_with(1))));
    }
    let commit = |step: u8| DiffBasis::CommitToParent {
        oid: fixture_oid(step * 2),
        parent: Some(fixture_oid(step * 2 + 1)),
    };

    const STEPS: u8 = 12;
    for step in 0..STEPS {
        let basis = commit(step);
        let _ = runtime.update(RepositoryAction::ApplyDiffBasis {
            card,
            basis: basis.clone(),
        });
        drop(runtime.update(RepositoryAction::DiffLoaded {
            identity: identity.clone(),
            basis: basis.clone(),
            request: runtime.next_diff_request,
            content: staged_text_diff(model_with(1)),
        }));
        assert_eq!(runtime.diff_basis, basis, "step {step} was not applied");
        assert!(
            runtime
                .diffs
                .iter()
                .filter(|(basis, _)| is_committed_choice(basis))
                .count()
                <= MAX_RETAINED_COMMITTED_DIFFS,
            "the cache exceeded its cap at step {step}"
        );
    }

    // Neither working basis was evicted to make room.
    assert!(runtime.diff_state(&DiffBasis::HeadToIndex).is_some());
    assert!(runtime.diff_state(&DiffBasis::IndexToWorktree).is_some());
    // The comparison on screen survived, and so did the ones just before
    // it, which is what makes stepping back instant.
    let kept = usize::from(STEPS) - MAX_RETAINED_COMMITTED_DIFFS;
    for step in 0..STEPS {
        let present = runtime.diff_state(&commit(step)).is_some();
        assert_eq!(
            present,
            usize::from(step) >= kept,
            "step {step} retention is wrong"
        );
    }

    // A duplicate answer for an evicted slot must not recreate it.
    let _ = runtime.update(RepositoryAction::DiffLoaded {
        identity,
        basis: commit(0),
        request: 1,
        content: staged_text_diff(model_with(9)),
    });
    assert!(runtime.diff_state(&commit(0)).is_none());
    assert_eq!(runtime.diff_basis, commit(STEPS - 1));
}

/// A commit diff and unstaged work must not read the same. The heading
/// names the comparison, and the inspector has its own static word for it.
#[test]
fn every_basis_names_itself_in_the_chrome() {
    let commit = DiffBasis::CommitToParent {
        oid: fixture_oid(1),
        parent: Some(fixture_oid(2)),
    };
    let root_commit = DiffBasis::CommitToParent {
        oid: fixture_oid(1),
        parent: None,
    };
    let branch = DiffBasis::MergeBaseToHead {
        base_ref: ValidatedRefName::parse("refs/remotes/origin/main").unwrap(),
        head: fixture_oid(1),
        merge_base: fixture_oid(2),
    };
    let pull_request = DiffBasis::PullRequest {
        number: 7,
        base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        head: fixture_oid(1),
        merge_base: fixture_oid(2),
    };
    let labels = [
        &DiffBasis::IndexToWorktree,
        &DiffBasis::HeadToIndex,
        &branch,
        &commit,
        &root_commit,
        &pull_request,
    ]
    .map(basis_label);
    assert_eq!(
        labels,
        [
            "Unstaged changes".to_owned(),
            "Staged changes".to_owned(),
            "Branch vs origin/main".to_owned(),
            format!("Commit {}", fixture_oid(1).short()),
            format!("Root commit {}", fixture_oid(1).short()),
            "#7 vs main".to_owned(),
        ]
    );
    let mut distinct = labels.to_vec();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), labels.len(), "two bases share their chrome");

    assert_eq!(basis_choice_label(&DiffBasis::IndexToWorktree), "Unstaged");
    assert_eq!(basis_choice_label(&DiffBasis::HeadToIndex), "Staged");
    assert_eq!(basis_choice_label(&branch), "Branch");
    assert_eq!(basis_choice_label(&commit), "Commit");
    assert_eq!(basis_choice_label(&root_commit), "Commit");
    assert_eq!(basis_choice_label(&pull_request), "Pull request");
}

/// A comparison with no text rows reports what it actually is, including
/// the states #45's loader could not produce. A conflicted path reports as
/// conflicted and has no two-sided model at all.
#[test]
fn a_comparison_without_rows_reports_what_it_is() {
    let card = CardId::new();
    let (mut runtime, _) = diff_runtime(card);
    runtime.diff_basis = DiffBasis::IndexToWorktree;
    let text = || DiffSide::Text {
        text: "a\n".into(),
        bytes: 2,
    };

    runtime.set_diff_state(
        DiffBasis::IndexToWorktree,
        DiffState::ready(RenderedDiff::NoText(DiffContent::Unmerged {
            base: text(),
            ours: text(),
            theirs: text(),
            worktree: DiffSide::Absent,
        })),
    );
    assert_eq!(runtime.inspector_context().diff_state, Some("Conflicted"));
    assert!(
        runtime.diff_model().is_none(),
        "a conflict has no two-sided model"
    );
    let _ = diff_surface(card, &runtime, None);

    for (new, expected) in [
        (DiffSide::Binary { bytes: 3 }, "Binary"),
        (DiffSide::InvalidUtf8 { bytes: 1 }, "Invalid UTF-8"),
        (DiffSide::Oversize { bytes: 1 << 30 }, "Too large"),
        (DiffSide::NotRegularFile, "Not a regular file"),
        (DiffSide::Submodule { oid: None }, "Submodule"),
        (
            DiffSide::Submodule {
                oid: Some(fixture_oid(3)),
            },
            "Submodule",
        ),
        (
            DiffSide::Unreadable {
                reason: "permission denied".into(),
            },
            "Unreadable",
        ),
        (
            DiffSide::Symlink {
                target: PathBuf::from("elsewhere"),
            },
            "Symlink",
        ),
    ] {
        runtime.set_diff_state(
            DiffBasis::IndexToWorktree,
            DiffState::ready(RenderedDiff::NoText(DiffContent::Comparison {
                old: text(),
                new,
                old_label: "Index".into(),
                new_label: "Working tree".into(),
            })),
        );
        assert_eq!(runtime.inspector_context().diff_state, Some(expected));
        assert!(runtime.diff_model().is_none());
        // The surface renders the fact instead of panicking on a state
        // #45's reduced content type had no variant for.
        let _ = diff_surface(card, &runtime, None);
    }
}

/// A runtime with one selected file that has both working halves and the
/// diff surface showing, which is what every basis fence needs to be
/// observable.
fn diff_runtime(card: CardId) -> (RepositoryRuntime, AsyncIdentity) {
    let mut runtime = RepositoryRuntime::new(RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    });
    let path = PathBuf::from("notes.txt");
    let file = RepositoryFile {
        path: path.clone(),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: true,
        unstaged: true,
        status: Some("MM".into()),
        directory: false,
    };
    runtime.generation = 1;
    runtime.files = FileListState::Ready(vec![file.clone()]);
    runtime.selected = Some(file);
    runtime.source_surface = SourceSurface::Diff;
    let identity = AsyncIdentity {
        card,
        worktree: PathBuf::from("/repo"),
        path,
        generation: 1,
    };
    (runtime, identity)
}

/// A ready text diff carrying the labels the staged basis produces.
fn staged_text_diff(model: DiffModel) -> RenderedDiff {
    RenderedDiff::Text {
        model,
        old_label: "HEAD".into(),
        new_label: "Index".into(),
    }
}

fn model_with(additions: u32) -> DiffModel {
    DiffModel {
        line_counts: [0; 2],
        before: Arc::from(""),
        inline_rows: Vec::new(),
        split_rows: Vec::new(),
        inline_change_starts: Vec::new(),
        split_change_starts: Vec::new(),
        unchanged: Default::default(),
        additions,
        deletions: 0,
        max_line_width: [0; 2],
        syntax: DiffSyntax::default(),
    }
}

/// Every node in the widget tree an element actually builds.
fn widget_nodes(element: &Element<'_, RepositoryAction>) -> usize {
    fn count(tree: &iced::advanced::widget::Tree) -> usize {
        1 + tree.children.iter().map(count).sum::<usize>()
    }
    count(&iced::advanced::widget::Tree::new(element))
}

fn fixture_oid(seed: u8) -> Oid {
    Oid::parse_object(format!("{}{seed:02x}", "ab".repeat(19))).expect("a valid object id")
}

fn head_oid(root: &Path) -> Oid {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success(), "git rev-parse HEAD failed");
    Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
}

/// An unborn branch has no `HEAD` for a merge base to be taken against —
/// even when the remote's own `HEAD` names a base ref perfectly well, which
/// is what makes this different from the commit basis's own empty case.
#[test]
fn an_unborn_branch_reports_no_commit_for_the_branch_basis_too() {
    let origin = tempdir().unwrap();
    git_fixture(origin.path());
    std::fs::write(origin.path().join("seed.txt"), "seed\n").unwrap();
    run_git(origin.path(), ["add", "seed.txt"]);
    run_git(origin.path(), ["commit", "-qm", "seed"]);

    let fetched = tempdir().unwrap();
    git_fixture(fetched.path());
    run_git(
        fetched.path(),
        [
            "remote",
            "add",
            "origin",
            origin.path().to_str().expect("a UTF-8 fixture path"),
        ],
    );
    run_git(fetched.path(), ["fetch", "-q", "origin"]);
    run_git(fetched.path(), ["remote", "set-head", "origin", "-a"]);

    // The base ref resolves: this repository really does know what the
    // remote's default branch is.
    let snapshot = RepositoryService::system("git")
        .snapshot(fetched.path(), &CancellationToken::new())
        .expect("snapshot");
    assert!(snapshot.default_branch_base().is_some());
    // Nothing has been committed here, so there is no HEAD to compare that
    // base with, and both committed choices must say so rather than
    // installing a comparison that dies in Git.
    assert_eq!(
        resolve_basis(fetched.path(), CommittedChoice::Branch),
        Ok(ResolvedBasis::NoCommit)
    );
    assert_eq!(
        resolve_basis(fetched.path(), CommittedChoice::LastCommit),
        Ok(ResolvedBasis::NoCommit)
    );
}

/// Git reports a symlink as the link itself. The buffer behind that link
/// is the target's text, and that text is what a comparison has to read
/// — including the index side a gutter tint is measured from.
#[test]
fn a_symlink_comparison_reads_the_target_text() {
    use std::os::unix::fs::symlink;
    let root = tempdir().unwrap();
    git_fixture(root.path());
    std::fs::write(root.path().join("CLAUDE.md"), "old\n").unwrap();
    symlink("CLAUDE.md", root.path().join("AGENTS.md")).unwrap();
    run_git(root.path(), ["add", "CLAUDE.md", "AGENTS.md"]);
    run_git(root.path(), ["commit", "-qm", "add both"]);
    std::fs::write(root.path().join("CLAUDE.md"), "new\n").unwrap();
    let token = CancellationToken::new();
    let link = load_repository_diff(
        root.path(),
        Path::new("AGENTS.md"),
        &DiffBasis::IndexToWorktree,
        None,
        crate::settings::EditorSyntaxTheme::Auto,
        Font::MONOSPACE,
        &token,
    );
    assert!(
        matches!(link, RenderedDiff::NoText(_)),
        "the link row is not a text diff: {link:?}"
    );
    let target = load_repository_diff(
        root.path(),
        Path::new("CLAUDE.md"),
        &DiffBasis::IndexToWorktree,
        None,
        crate::settings::EditorSyntaxTheme::Auto,
        Font::MONOSPACE,
        &token,
    );
    let RenderedDiff::Text { model, .. } = target else {
        panic!("the target is a text diff: {target:?}");
    };
    assert_eq!(
        model.before.as_ref(),
        "old\n",
        "the baseline is the target's index content"
    );
    assert!(model.additions >= 1);
}

/// An empty repository on a named initial branch, because the branch tests
/// name `main` and Git's default depends on the host's configuration.
fn git_fixture(root: &Path) {
    run_git(root, ["init", "-q", "-b", "main"]);
    run_git(root, ["config", "user.name", "Fixture"]);
    run_git(root, ["config", "user.email", "fixture@example.invalid"]);
}

fn run_git<const N: usize>(root: &Path, args: [&str; N]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

/// A buffer whose tokens are exactly the ones the smoke run found
/// indistinguishable: a comment, a keyword, a string, and a macro call.
const HIGHLIGHTABLE: &str =
    "// what main does\nfn main() {\n    let hello = \"world\";\n    println!(\"{hello}\");\n}\n";

#[test]
fn nested_horizontal_diff_preserves_virtual_offsets_for_descendants() {
    use crate::workspace_widget::forwarding_test::{Traverse, leaf, renderer};
    use cosmic::iced::{Point, Size, Vector};

    let mut renderer = renderer();
    // The canvas cancels horizontal scrolling while a row applies it to
    // its text. Neither real translation may consume the enclosing
    // scrollable's virtual offset before a descendant opens an overlay
    // or participates in an operation.
    for offset in [
        Vector::ZERO,
        Vector::new(4.0, -20.0),
        Vector::new(-9.0, 35.0),
    ] {
        let (content, visits) = leaf::<RepositoryAction>(Size::new(500.0, 60.0), vec![]);
        let row = Element::new(DiffHorizontalContent {
            content,
            width: Length::Fixed(300.0),
            content_width: 500.0,
            offset: 80.0,
            accessible_text: None,
            viewport: None,
        });
        let mut canvas = Element::new(DiffHorizontalContent {
            content: row,
            width: Length::Fixed(100.0),
            content_width: 300.0,
            offset: -80.0,
            accessible_text: None,
            viewport: None,
        });
        let mut tree = Tree::new(&canvas);
        let node = canvas
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(100.0, 60.0)),
            )
            .move_to(Point::new(30.0, 40.0));
        let layout = Layout::new(&node).with_virtual_offset(offset);
        canvas
            .as_widget_mut()
            .operate(&mut tree, layout, &renderer, &mut Traverse);
        assert!(
            canvas
                .as_widget_mut()
                .overlay(&mut tree, layout, &renderer, &layout.bounds(), Vector::ZERO)
                .is_none()
        );
        let mut messages = Vec::new();
        canvas.as_widget_mut().update(
            &mut tree,
            &iced::Event::Mouse(iced::mouse::Event::CursorMoved {
                position: Point::new(50.0, 50.0),
            }),
            layout,
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &layout.bounds(),
        );
        canvas.as_widget().mouse_interaction(
            &tree,
            layout,
            iced::mouse::Cursor::Unavailable,
            &layout.bounds(),
            &renderer,
        );
        canvas.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::default(),
            &renderer::Style::default(),
            layout,
            iced::mouse::Cursor::Unavailable,
            &layout.bounds(),
        );
        for kind in ["operate", "overlay", "update", "mouse_interaction", "draw"] {
            assert_eq!(visits.offsets(kind), vec![offset], "{kind}");
        }
    }
}

#[test]
fn horizontal_diff_forwards_translated_and_clipped_drop_targets() {
    use crate::workspace_widget::forwarding_test::{leaf, renderer};
    use cosmic::iced::{
        Point, Rectangle, Size, Vector, advanced::clipboard::DndDestinationRectangles,
    };
    let renderer = renderer();
    let (content, visits) = leaf::<RepositoryAction>(
        Size::new(300.0, 60.0),
        vec![
            (1, Rectangle::new(Point::ORIGIN, Size::new(50.0, 60.0))),
            (
                2,
                Rectangle::new(Point::new(60.0, 0.0), Size::new(80.0, 60.0)),
            ),
            (
                3,
                Rectangle::new(Point::new(150.0, 0.0), Size::new(80.0, 60.0)),
            ),
        ],
    );
    let mut widget = Element::new(DiffHorizontalContent {
        content,
        width: Length::Fixed(100.0),
        content_width: 300.0,
        offset: 80.0,
        accessible_text: None,
        viewport: None,
    });
    let mut tree = Tree::new(&widget);
    let node = widget
        .as_widget_mut()
        .layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 60.0)),
        )
        .move_to(Point::new(30.0, 40.0));
    let offset = Vector::new(4.0, -20.0);
    let mut destinations = DndDestinationRectangles::new();
    widget.as_widget().drag_destinations(
        &tree,
        Layout::new(&node).with_virtual_offset(offset),
        &renderer,
        &mut destinations,
    );
    let actual: Vec<_> = destinations
        .into_native_rectangles()
        .into_iter()
        .map(|destination| {
            (
                destination.id,
                destination.rectangle.x,
                destination.rectangle.y,
                destination.rectangle.width,
                destination.rectangle.height,
            )
        })
        .collect();
    assert_eq!(
        actual,
        vec![(3, 100.0, 40.0, 30.0, 60.0), (2, 30.0, 40.0, 60.0, 60.0)]
    );
    assert_eq!(visits.offsets("drag"), vec![offset]);
}

/// A headless `cosmic::Renderer`. With no graphics device it falls back to
/// `tiny-skia`, which needs neither a device nor a surface and which can
/// hand back the pixels it drew.
///
/// One per render: the renderer accumulates the layers it is drawn into.
fn headless_renderer() -> cosmic::Renderer {
    use cosmic::iced::Pixels;
    use cosmic::iced::advanced::renderer::Headless;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("executor")
        .block_on(cosmic::Renderer::new(
            Font::MONOSPACE,
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .expect("a headless renderer")
}

#[test]
fn wrapping_reflows_an_open_dirty_buffer_without_changing_its_text() {
    let card = CardId::new();
    let text = format!("{}\nsecond line", "x".repeat(256));
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", &text);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
    }));
    let edited = match &runtime.document {
        DocumentState::Editable(document) => document.content.text(),
        _ => panic!("fixture must be editable"),
    };
    let renderer = headless_renderer();
    let mut tree = Tree::new(editor_surface(card, &runtime).as_widget());

    // Hit below the first visual row. Without wrapping it lands on the
    // second logical line; with wrapping it is still inside the long token.
    // Toggling back exercises reflow of an already mounted editor.
    for (wrap, expected_line) in [(false, 1), (true, 0), (false, 1)] {
        let cursor = runtime.cursor_position();
        runtime.set_wrap_lines(wrap);
        assert_eq!(runtime.cursor_position(), cursor);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Move(text_editor::Motion::DocumentStart),
        }));
        {
            let mut element = editor_surface(card, &runtime);
            tree.diff(element.as_widget_mut());
            element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(260.0, 200.0)),
            );
        }
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Click(iced::Point::new(20.0, 40.0)),
        }));
        assert_eq!(runtime.cursor_position().unwrap().line, expected_line);
        let DocumentState::Editable(document) = &runtime.document else {
            panic!("changing presentation must keep the document open");
        };
        assert_eq!(document.content.text(), edited);
        assert!(document.dirty);
    }
}

/// The RGBA pixels the card's own `view` rasterises, at `size` logical
/// pixels on the dark COSMIC palette.
fn rendered_card(card: CardId, runtime: &RepositoryRuntime, size: (u32, u32)) -> Vec<u8> {
    use cosmic::iced::advanced::renderer::{Headless, Style};
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
    use cosmic::iced::{Color, Rectangle, Size};

    let (width, height) = size;
    let mut renderer = headless_renderer();
    let mut element = view(card, runtime);
    let mut tree = Tree::new(element.as_widget());
    let viewport = Rectangle::with_size(Size::new(width as f32, height as f32));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    element.as_widget().draw(
        &tree,
        &mut renderer,
        &cosmic::Theme::dark(),
        &Style::default(),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &viewport,
    );
    renderer.screenshot(Size::new(width, height), 1.0, Color::BLACK)
}

#[test]
fn opening_an_image_reconciles_the_mounted_file_list_and_renders_pixels() {
    use cosmic::iced::advanced::renderer::{Headless, Style};
    use cosmic::iced::{Color, Rectangle, Size};
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::new(RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: "/repo/.git".into(),
        worktree: "/repo".into(),
    });
    let file = RepositoryFile {
        path: "picture.png".into(),
        staged_old_path: None,
        unstaged_old_path: None,
        staged: false,
        unstaged: true,
        status: Some("??".into()),
        directory: false,
    };
    runtime.files = FileListState::Ready(vec![file.clone()]);
    let mut tree = Tree::new(view(card, &runtime).as_widget());
    let mut renderer = headless_renderer();
    let viewport = Rectangle::with_size(Size::new(320.0, 240.0));
    let _ = runtime.update(RepositoryAction::OpenFile {
        card,
        path: file.path,
    });
    {
        let mut element = view(card, &runtime);
        tree.diff(element.as_widget_mut());
        element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
    }
    let _ = runtime.update(RepositoryAction::FileLoaded {
        identity: AsyncIdentity {
            card,
            worktree: runtime.descriptor.worktree.clone(),
            path: "picture.png".into(),
            generation: runtime.generation,
        },
        result: Ok(LoadedFile {
            source: "picture.png".into(),
            content: FileContent::Image {
                width: 1,
                height: 1,
                rgba: vec![200, 20, 40, 255].into(),
            },
        }),
    });
    let mut element = view(card, &runtime);
    tree.diff(element.as_widget_mut());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    element.as_widget().draw(
        &tree,
        &mut renderer,
        &cosmic::Theme::dark(),
        &Style::default(),
        Layout::new(&node),
        iced::mouse::Cursor::Unavailable,
        &viewport,
    );
    let pixels = renderer.screenshot(Size::new(320, 240), 1.0, Color::BLACK);
    let center = (120 * 320 + 160) * 4;
    assert_eq!(&pixels[center..center + 4], &[200, 20, 40, 255]);
}

/// The colour families a render puts on screen, as (brightest channel,
/// dimmest channel) pairs.
///
/// Counting *distinct colours* would count anti-aliasing: a single
/// coloured glyph fades into the background over a dozen shades. Which
/// channel leads is what survives that fade, so this counts token colours
/// rather than blend steps. Achromatic pixels — the background, the
/// chrome's greys, and unhighlighted text — are excluded outright, and a
/// family needs a real population to count so a stray fringe pixel cannot
/// invent one.
fn colour_families(pixels: &[u8]) -> BTreeSet<(usize, usize)> {
    const MIN_CHROMA: u8 = 32;
    const MIN_PIXELS: usize = 16;

    let mut counts: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for pixel in pixels.chunks_exact(4) {
        let rgb = [pixel[0], pixel[1], pixel[2]];
        let high = rgb.iter().copied().max().expect("three channels");
        let low = rgb.iter().copied().min().expect("three channels");
        if high - low < MIN_CHROMA {
            continue;
        }
        let brightest = rgb
            .iter()
            .enumerate()
            .max_by_key(|&(_, v)| v)
            .expect("max")
            .0;
        let dimmest = rgb
            .iter()
            .enumerate()
            .min_by_key(|&(_, v)| v)
            .expect("min")
            .0;
        *counts.entry((brightest, dimmest)).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|&(_, count)| count >= MIN_PIXELS)
        .map(|(family, _)| family)
        .collect()
}

/// With nothing selected the card reports its file-listing state instead
/// of a second browser. Each state renders its own surface, so a stuck
/// or failed list cannot hide behind "no file selected" — and the
/// failure surface keeps the retry that re-arms the load.
#[test]
fn empty_selection_reports_file_listing_state() {
    let card = CardId::new();
    let view = RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: PathBuf::from("/repo/.git"),
        worktree: PathBuf::from("/repo"),
    };
    let render = |files: FileListState| {
        let mut runtime = RepositoryRuntime::new(view.clone());
        runtime.files = files;
        rendered_card(card, &runtime, (520, 200))
    };
    let loading = render(FileListState::Loading);
    let failed = render(FileListState::Failed("status failed".into()));
    let ready = render(FileListState::Ready(Vec::new()));
    for (name, frame) in [
        ("loading", &loading),
        ("failed", &failed),
        ("ready", &ready),
    ] {
        assert!(
            frame.iter().any(|&channel| channel != 0),
            "{name} rendered nothing at all, so the comparisons below would pass vacuously"
        );
    }
    assert_ne!(loading, failed, "a failed list renders as loading");
    assert_ne!(failed, ready, "a failed list renders as no selection");
    assert_ne!(loading, ready, "a loading list renders as no selection");

    let mut runtime = RepositoryRuntime::new(view);
    runtime.files = FileListState::Failed("status failed".into());
    drop(runtime.update(RepositoryAction::RefreshFiles { card }));
    assert!(
        matches!(runtime.files, FileListState::Loading),
        "the failure surface's retry must re-arm the file load"
    );
}

/// GitHub issue #1 asks for editable *syntax-highlighted* buffers, and the
/// editor that first landed drew every token in one colour. This renders
/// the same bytes twice, changing nothing but the name they were opened
/// under, and reads the pixels back.
#[test]
fn the_editable_buffer_is_highlighted_by_the_grammar_its_name_names() {
    let card = CardId::new();
    let render = |name: &str| {
        let (runtime, _) = RepositoryRuntime::open_fixture(card, name, HIGHLIGHTABLE);
        rendered_card(card, &runtime, (520, 200))
    };
    // `.txt` is plain text to every grammar set, so this is the same card
    // with the highlighting switched off by the only input that chooses it.
    let rust = render("main.rs");
    let plain = render("main.txt");

    assert_eq!(rust.len(), plain.len(), "both renders are one size");
    assert!(
        rust.iter().any(|&channel| channel != 0),
        "the harness drew nothing at all, so every comparison below would pass vacuously"
    );

    // Not a comparison of the two frames byte for byte: the header prints
    // the file name, so they differ on a row that has nothing to do with
    // highlighting, and such a check would pass with the highlighter
    // deleted. Colour is what separates them. Every one of this card's own
    // greys and its single chromatic accent is drawn identically in both,
    // so a colour family the Rust render has and the plain-text one lacks
    // can only have come from a grammar.
    let highlighted = colour_families(&rust);
    let unhighlighted = colour_families(&plain);
    assert!(
        unhighlighted.is_subset(&highlighted),
        "the plain render has a colour the highlighted one lacks, so these two renders differ \
             in more than the grammar and the comparison below proves nothing: {unhighlighted:?} \
             against {highlighted:?}"
    );
    assert!(
        highlighted.len() >= unhighlighted.len() + 2,
        "a keyword, a string and a comment must not all read the same: the highlighted render \
             put {} colour families on screen against the plain render's {} — {highlighted:?} \
             against {unhighlighted:?}",
        highlighted.len(),
        unhighlighted.len()
    );
}

/// Both inputs the highlighter takes, checked where they are decided.
#[test]
fn the_grammar_follows_the_file_name_and_the_scheme_follows_the_palette() {
    use cosmic::iced::advanced::text::Highlighter as _;

    /// Whether a token resolves to a grammar that actually colours
    /// `line`, rather than to the plain-text fallback that colours nothing.
    fn colours(token: &str, line: &str) -> bool {
        let settings = highlighter::Settings {
            theme: highlighter::Theme::Base16Ocean,
            token: token.to_owned(),
        };
        highlighter::Highlighter::new(&settings)
            .highlight_line(line)
            .any(|(_, highlight)| highlight.color().is_some())
    }

    for (path, line) in [
        ("src/main.rs", "fn main() {}"),
        ("README.MD", "# heading"),
        ("Makefile", "all: build"),
        ("deploy/Dockerfile", "FROM scratch"),
        ("deploy/Dockerfile.dev", "FROM scratch"),
        ("src/kernel.cu", "__global__ void kernel() { return; }"),
    ] {
        let settings = highlighter_settings(
            Path::new(path),
            crate::settings::EditorSyntaxTheme::Auto,
            true,
        );
        assert!(
            colours(&settings.token, line),
            "{path} must resolve to a grammar rather than render as flat plain text"
        );
    }

    assert!(
        !colours("no-such-grammar", "fn main() {}"),
        "an unrecognised token must fall back to plain text rather than colour arbitrarily; \
             without that the check above cannot tell a grammar from the fallback"
    );

    // A scheme picks its foregrounds for a background of its own kind, so
    // reading a dark one off a light surface is what makes highlighted
    // text illegible.
    assert!(
        highlighter_settings(
            Path::new("main.rs"),
            crate::settings::EditorSyntaxTheme::Auto,
            true,
        )
        .theme
        .is_dark(),
        "a dark application must highlight with a dark scheme"
    );
    assert!(
        !highlighter_settings(
            Path::new("main.rs"),
            crate::settings::EditorSyntaxTheme::Auto,
            false,
        )
        .theme
        .is_dark(),
        "a light application must highlight with a light scheme"
    );
}

/// Moving the caret is not enough to *show* a jump: an unfocused
/// `text_editor` draws no caret at all, which is what made
/// jump-to-location look like it did nothing. The affordance the
/// application chains after a caret move has to name a target that the
/// view really carries, on the frame after the one that asked for it.
#[test]
fn the_editable_buffer_offers_a_stable_focus_target_the_view_really_carries() {
    use cosmic::iced::Size;
    use cosmic::iced::advanced::widget::Operation as _;
    use cosmic::iced::advanced::widget::operation::{self, Outcome};
    use cosmic::iced::advanced::{Layout, layout, widget::Tree};

    let card = CardId::new();
    let expected = editor_focus_id(card).to_string();
    assert_eq!(
        editor_focus_id(card).to_string(),
        expected,
        "the id must be derived from the card rather than minted: a focus operation is \
             delivered on a later frame than the one that asked for it, and an id that changed in \
             between would address nothing"
    );
    assert_ne!(
        editor_focus_id(CardId::new()).to_string(),
        expected,
        "two repository cards must not contend for one focus target"
    );

    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "main.rs", HIGHLIGHTABLE);
    assert_eq!(
        runtime.editor_focus_target(card).map(|id| id.to_string()),
        Some(expected.clone()),
        "an open editable buffer is exactly the surface a jump lands in"
    );

    // The id is only worth having if the widget tree answers to it, so ask
    // the real `view` to focus it and then ask it back what is focused.
    let landed = {
        let renderer = headless_renderer();
        let mut element = view(card, &runtime);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(520.0, 200.0)),
        );
        let mut focus = operation::focusable::focus::<()>(editor_focus_id(card));
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
        // `Widget::operate` only takes an `Operation<()>`; `black_box` is
        // how iced runs an operation that reports an outcome through one.
        let mut focused = operation::focusable::find_focused();
        let mut probe = operation::black_box::<_, ()>(&mut focused);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        drop(probe);
        match focused.finish() {
            Outcome::Some(id) => Some(id.to_string()),
            Outcome::None | Outcome::Chain(_) => None,
        }
    };
    assert_eq!(
        landed,
        Some(expected.clone()),
        "the focus operation reached no focusable widget under that id: the editor the view \
             builds does not carry it"
    );

    // A surface that builds no editor has nothing to focus, and a focus
    // operation aimed at it would be delivered to no widget at all.
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.markdown = true;
    }
    runtime.markdown_mode = MarkdownMode::Preview;
    assert!(
        runtime.editor_focus_target(card).is_none(),
        "a Markdown preview renders the buffer, it does not edit it"
    );

    runtime.markdown_mode = MarkdownMode::Edit;
    drop(runtime.update(RepositoryAction::ApplySourceSurface { card, diff: true }));
    assert!(
        runtime.editor_focus_target(card).is_none(),
        "the diff surface replaces the editor rather than sitting beside it"
    );

    drop(runtime.update(RepositoryAction::BackToFiles { card }));
    assert!(
        runtime.editor_focus_target(card).is_none(),
        "the file list has no buffer for a caret to be in"
    );
}

#[test]
fn directory_editor_accepts_unlisted_nested_files_but_not_escaping_paths() {
    let root = tempfile::tempdir().unwrap();
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: root.path().to_path_buf(),
        worktree: root.path().to_path_buf(),
    });
    let nested = Path::new("nested/child.txt");
    assert!(runtime.can_open_file(nested));
    assert!(!runtime.can_open_file(Path::new("../outside.txt")));
    assert!(!runtime.can_open_file(Path::new("/outside.txt")));
    drop(runtime.update(RepositoryAction::OpenFile {
        card,
        path: nested.to_path_buf(),
    }));
    assert_eq!(runtime.selected_path(), Some(nested));
}

#[test]
fn repository_browser_opens_ignored_files_without_a_git_listing_entry() {
    let root = tempfile::tempdir().unwrap();
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::file_list_fixture(
        RepositoryView {
            selected_file: None,
            project: ProjectId::new(),
            repository: root.path().to_path_buf(),
            worktree: root.path().to_path_buf(),
        },
        &["tracked.rs"],
    );
    let path = RepoPath::parse("generated/nested/output.txt").unwrap();
    assert!(!runtime.can_open_file(path.as_path()));
    drop(runtime.update(RepositoryAction::OpenFileFromBrowser {
        card,
        path: path.clone(),
    }));
    assert_eq!(runtime.selected_path(), Some(path.as_path()));
    assert!(matches!(runtime.document, DocumentState::Loading { .. }));
    assert!(
        !runtime.selected.as_ref().unwrap().changed(),
        "ignored files are not working changes"
    );
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
}

#[test]
fn ignored_folder_errors_preserve_other_entries_and_wait_for_explicit_retry() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("good")).unwrap();
    std::fs::write(root.path().join("good/output.txt"), "generated").unwrap();
    // A folder can be replaced while its expansion is still mounted.
    std::fs::write(root.path().join("bad"), "replaced directory").unwrap();
    let bad = RepoPath::parse("bad").unwrap();
    let good = RepoPath::parse("good").unwrap();
    let expanded = vec![bad.clone(), good.clone()];
    let fresh_listing = || crate::repository::WorkingFiles {
        ignored_directories: expanded.clone(),
        ..Default::default()
    };
    let mut files = fresh_listing();
    expand_ignored_directories(
        root.path(),
        &mut files,
        &expanded,
        &BTreeMap::new(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(files.ignored_directory_errors.contains_key(&bad));
    assert!(
        files
            .ignored
            .contains(&RepoPath::parse("good/output.txt").unwrap())
    );
    let failures = files.ignored_directory_errors.clone();

    std::fs::remove_file(root.path().join("bad")).unwrap();
    std::fs::create_dir(root.path().join("bad")).unwrap();
    std::fs::write(root.path().join("bad/recovered.txt"), "recovered").unwrap();
    let mut files = fresh_listing();
    expand_ignored_directories(
        root.path(),
        &mut files,
        &expanded,
        &failures,
        &CancellationToken::default(),
    )
    .unwrap();
    assert_eq!(files.ignored_directory_errors, failures);
    assert!(
        !files
            .ignored
            .contains(&RepoPath::parse("bad/recovered.txt").unwrap()),
        "background refreshes must not retry failed folders"
    );

    let mut files = fresh_listing();
    expand_ignored_directories(
        root.path(),
        &mut files,
        &expanded,
        &BTreeMap::new(),
        &CancellationToken::default(),
    )
    .unwrap();
    assert!(files.ignored_directory_errors.is_empty());
    assert!(
        files
            .ignored
            .contains(&RepoPath::parse("bad/recovered.txt").unwrap()),
        "clearing the failure explicitly must retry the listing"
    );
}

#[test]
fn directory_listings_are_one_level_until_a_folder_is_opened() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("nested/deeper")).unwrap();
    std::fs::write(root.path().join("top.txt"), "a").unwrap();
    std::fs::write(root.path().join("nested/child.txt"), "b").unwrap();
    std::fs::write(root.path().join("nested/deeper/leaf.txt"), "c").unwrap();
    let listed = load_directory_files(root.path()).unwrap();
    let names: Vec<_> = listed
        .iter()
        .map(|file| (file.path.to_string_lossy().into_owned(), file.directory))
        .collect();
    assert_eq!(
        names,
        vec![("nested".into(), true), ("top.txt".into(), false)],
        "the first paint must not walk Documents-sized trees"
    );

    let nested = load_directory_listing(root.path(), Path::new("nested")).unwrap();
    let nested_names: Vec<_> = nested
        .iter()
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(nested_names, vec!["nested/child.txt", "nested/deeper"]);
}

#[test]
fn directory_search_is_on_demand_and_capped_to_visible_rows() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("nested/deeper")).unwrap();
    std::fs::write(root.path().join("top.txt"), "a").unwrap();
    std::fs::write(root.path().join("nested/note.md"), "b").unwrap();
    std::fs::write(root.path().join("nested/deeper/note.txt"), "c").unwrap();
    std::fs::write(root.path().join("other.rs"), "d").unwrap();
    let (matches, hidden) =
        search_directory_files(root.path(), "note", &CancellationToken::new()).unwrap();
    let names: Vec<_> = matches
        .iter()
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["nested/deeper/note.txt", "nested/note.md"],
        "search must find nested files without listing the whole tree first"
    );
    assert_eq!(hidden, 0);
    let (none, _) =
        search_directory_files(root.path(), "missing", &CancellationToken::new()).unwrap();
    assert!(none.is_empty());
}

/// `core.fsmonitor` names a program Git runs on every index refresh, and
/// a repository that arrived as an archive chose it. A card lists its
/// files on creation and every ten seconds after, so a second copy of
/// the invocation block that forgets this is a local code-execution
/// vector reachable without a click.
#[test]
fn every_git_invocation_here_disables_the_repository_chosen_fsmonitor_hook() {
    let command = git_command(Path::new("/repo"));
    let argv: Vec<String> = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        argv,
        [
            "git",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-C",
            "/repo",
        ]
    );
}

#[test]
fn an_unchanged_worktree_is_reported_without_rebuilding_the_file_list() {
    let root = tempdir().unwrap();
    git_fixture(root.path());
    std::fs::write(root.path().join("tracked.txt"), "a").unwrap();
    run_git(root.path(), ["add", "tracked.txt"]);

    let FileListRead::Listed(Some(digest), files) =
        read_repository_files(root.path(), None).unwrap()
    else {
        panic!("a read with nothing to compare against always lists");
    };
    assert_eq!(files.len(), 1);
    assert!(
        matches!(
            read_repository_files(root.path(), Some(digest)).unwrap(),
            FileListRead::Unchanged
        ),
        "an idle refresh tick must not rebuild a list nothing has moved"
    );

    std::fs::write(root.path().join("new.txt"), "b").unwrap();
    assert!(matches!(
        read_repository_files(root.path(), Some(digest)).unwrap(),
        FileListRead::Listed(..)
    ));
}

#[test]
fn a_refresh_tick_keeps_expanded_folders_expanded() {
    let root = tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("nested")).unwrap();
    std::fs::write(root.path().join("top.txt"), "a").unwrap();
    std::fs::write(root.path().join("nested/child.txt"), "b").unwrap();
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: root.path().to_path_buf(),
        worktree: root.path().to_path_buf(),
    });
    let identity = |runtime: &RepositoryRuntime, path: &str| AsyncIdentity {
        card,
        worktree: runtime.descriptor.worktree.clone(),
        path: PathBuf::from(path),
        generation: runtime.file_list_generation,
    };

    drop(runtime.initial_load(card));
    drop(runtime.update(RepositoryAction::FilesLoaded {
        identity: identity(&runtime, "."),
        result: Ok(load_directory_files(root.path()).unwrap()),
    }));
    drop(runtime.update(RepositoryAction::ListDirectory {
        card,
        path: "nested".into(),
    }));
    drop(runtime.update(RepositoryAction::DirectoryListed {
        identity: identity(&runtime, "nested"),
        result: Ok(load_directory_listing(root.path(), Path::new("nested")).unwrap()),
    }));
    assert!(runtime.file(Path::new("nested/child.txt")).is_some());

    drop(runtime.refresh_visible(card));
    drop(runtime.update(RepositoryAction::FilesLoaded {
        identity: identity(&runtime, "."),
        result: Ok(load_directory_files(root.path()).unwrap()),
    }));
    assert!(
        runtime.file(Path::new("nested/child.txt")).is_some(),
        "a refresh must not collapse the tree the user opened"
    );
    assert!(runtime.listed_directories.contains(Path::new("nested")));

    // A folder that disappeared underneath the card takes its rows and
    // its expansion with it.
    std::fs::remove_dir_all(root.path().join("nested")).unwrap();
    drop(runtime.refresh_visible(card));
    drop(runtime.update(RepositoryAction::FilesLoaded {
        identity: identity(&runtime, "."),
        result: Ok(load_directory_files(root.path()).unwrap()),
    }));
    assert!(runtime.file(Path::new("nested/child.txt")).is_none());
    assert!(!runtime.listed_directories.contains(Path::new("nested")));
}

#[test]
fn discarding_edits_keeps_the_viewport_where_the_user_left_it() {
    let text = "line\n".repeat(400);
    let mut document = EditorDocument::unlinked(
        PathBuf::from("notes.txt"),
        text.clone(),
        blake3::hash(text.as_bytes()),
        false,
    );
    let anchor = text_editor::ScrollPosition {
        line: 120,
        vertical: 0.0,
        horizontal: 0.0,
    };
    document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('x')));
    assert!(document.dirty);

    // Whatever the buffer was showing when the user gave up on the edit
    // is what it must still be showing afterwards.
    document.content.restore_scroll_position(anchor);
    document.discard_edits();
    assert!(!document.dirty);
    assert_eq!(
        document.content.scroll_position(),
        anchor,
        "abandoning an edit is no reason to throw the reader back to line one"
    );
}

#[test]
fn an_edit_that_restores_the_saved_text_is_not_a_modification() {
    let text = "hello\nworld\n";
    let mut document = EditorDocument::unlinked(
        PathBuf::from("notes.txt"),
        text.into(),
        blake3::hash(text.as_bytes()),
        false,
    );
    document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('!')));
    assert!(document.dirty);
    document.apply(text_editor::Action::Edit(text_editor::Edit::Backspace));
    assert!(
        !document.dirty,
        "dirtiness is the buffer against the saved text, not a count of keystrokes"
    );

    // Same length, different content: the cheap length check cannot
    // answer this one and must not be trusted to.
    document.apply(text_editor::Action::Edit(text_editor::Edit::Delete));
    document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('H')));
    assert_eq!(document.content.text().len(), text.len());
    assert!(document.dirty);
}

#[test]
fn an_edit_at_a_final_bare_carriage_return_resends_the_whole_document() {
    // The protocol does not count a buffer's final bare `\r`, so removing
    // it has no protocol range measured on the edited buffer alone.
    let text = "a\r";
    let mut document = EditorDocument::unlinked(
        PathBuf::from("mac.txt"),
        text.into(),
        blake3::hash(text.as_bytes()),
        false,
    );
    document.apply(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
    assert_eq!(
        document.apply(text_editor::Action::Edit(text_editor::Edit::Backspace)),
        Applied::Changed(None)
    );
    assert_eq!(document.content.text(), "a");
}

#[test]
fn replay_at_a_final_bare_carriage_return_resends_the_whole_document() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::lsp_open_fixture(card, "a\r");
    drop(runtime.take_lsp_intents());
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Move(text_editor::Motion::DocumentEnd),
    }));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Backspace),
    }));
    drop(runtime.take_lsp_intents());
    drop(runtime.take_lsp_edits());

    for (action, version, text) in [
        (RepositoryAction::Undo { card }, 3, "a\r"),
        (RepositoryAction::Redo { card }, 4, "a"),
    ] {
        drop(runtime.update(action));
        assert_eq!(buffer_text(&runtime), text);
        assert!(
            !runtime.lsp_edits_exact,
            "a trailing CR cannot be addressed by a protocol range"
        );
        assert_eq!(
            runtime.take_lsp_intents(),
            vec![LspIntent::Change {
                version,
                text: text.into(),
            }]
        );
        assert!(runtime.take_lsp_edits().is_empty());
    }
}

#[test]
fn retained_reload_restores_input_after_real_toolbar_blur() {
    use cosmic::iced::advanced::{clipboard, widget::operation};
    use cosmic::iced::futures::{StreamExt, executor::block_on};

    fn controls(card: CardId, runtime: &RepositoryRuntime) -> Element<'_, RepositoryAction> {
        let Some(find) = find_strip(card, runtime) else {
            return editor_surface(card, runtime);
        };
        widget::column::with_children(vec![
            icon_button(Icon::Refresh, "Reload file from disk")
                .on_press(RepositoryAction::Reload { card })
                .into(),
            find,
            editor_surface(card, runtime),
        ])
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
    }

    fn apply(
        card: CardId,
        runtime: &RepositoryRuntime,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        task: Task<RepositoryAction>,
    ) {
        let mut element = controls(card, runtime);
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, 480.0)),
        );
        block_on(async {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    let iced::runtime::Action::Widget(mut operation) = action else {
                        panic!("the focus handoff must only operate on widgets");
                    };
                    loop {
                        element.as_widget_mut().operate(
                            tree,
                            Layout::new(&node),
                            renderer,
                            operation.as_mut(),
                        );
                        match operation.finish() {
                            operation::Outcome::Chain(next) => operation = next,
                            _ => break,
                        }
                    }
                }
            }
        });
    }

    fn event(
        card: CardId,
        runtime: &RepositoryRuntime,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        event: iced::Event,
        hit: Option<usize>,
    ) -> Vec<RepositoryAction> {
        let mut element = controls(card, runtime);
        tree.diff(element.as_widget_mut());
        let bounds = iced::Rectangle::with_size(iced::Size::new(640.0, 480.0));
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()),
        );
        struct Bounds(widget::Id, Option<iced::Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                if id.is_some_and(|id| iced::core::id::IdEq::eq(&id.0, &self.0.0)) {
                    self.1 = Some(bounds);
                }
            }
        }
        let replace = if hit == Some(3) {
            let mut probe = Bounds(replace_focus_id(card), None);
            element
                .as_widget_mut()
                .operate(tree, Layout::new(&node), renderer, &mut probe);
            Some(probe.1.expect("the real replacement input must be mounted"))
        } else {
            None
        };
        let cursor = hit.map_or(iced::mouse::Cursor::Unavailable, |index| {
            let child = replace.unwrap_or_else(|| node.children()[index].bounds());
            iced::mouse::Cursor::Available(iced::Point::new(
                child.x + child.width.min(60.0) / 2.0,
                child.y + 12.0,
            ))
        });
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            tree,
            &event,
            Layout::new(&node),
            cursor,
            renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        messages
    }

    fn key(value: &str) -> iced::Event {
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(value.into()),
            modified_key: keyboard::Key::Character(value.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some(value.into()),
            repeat: false,
        })
    }

    let renderer = headless_renderer();
    for owner in [EditorInput::Find, EditorInput::Source, EditorInput::Replace] {
        let card = CardId::new();
        let (directory, mut runtime, _) = reload_read_fixture(card);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        }));
        let focus = runtime.update(RepositoryAction::OpenFind { card });
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "line".into(),
        }));
        if owner == EditorInput::Replace {
            drop(runtime.update(RepositoryAction::ToggleReplace { card }));
        }
        let mut tree = Tree::new(controls(card, &runtime).as_widget());
        apply(card, &runtime, &mut tree, &renderer, focus);
        assert!(
            !runtime.find_input_is_focused(),
            "programmatic focus has no model callback"
        );
        let messages = event(
            card,
            &runtime,
            &mut tree,
            &renderer,
            iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
            None,
        );
        for message in messages {
            drop(runtime.update(message));
        }
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "line".into(),
        }));
        if owner == EditorInput::Source {
            for mouse in [
                iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
            ] {
                let messages = event(
                    card,
                    &runtime,
                    &mut tree,
                    &renderer,
                    iced::Event::Mouse(mouse),
                    Some(2),
                );
                for message in messages {
                    drop(runtime.update(message));
                }
            }
        } else if owner == EditorInput::Replace {
            for mouse in [
                iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
            ] {
                let messages = event(
                    card,
                    &runtime,
                    &mut tree,
                    &renderer,
                    iced::Event::Mouse(mouse),
                    Some(3),
                );
                for message in messages {
                    drop(runtime.update(message));
                }
            }
            let messages = event(card, &runtime, &mut tree, &renderer, key("r"), None);
            for message in messages {
                drop(runtime.update(message));
            }
            assert_eq!(runtime.replace_query, "r");
        }
        assert_eq!(runtime.last_editor_input, owner);
        let edited = runtime.editor_text().unwrap();
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        let cursor = document.content.cursor();
        if owner != EditorInput::Source {
            assert!(cursor.selection.is_some());
        }
        std::fs::remove_file(directory.path().join("notes.txt")).unwrap();
        let pressed = event(
            card,
            &runtime,
            &mut tree,
            &renderer,
            iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
            Some(0),
        );
        if owner == EditorInput::Find {
            assert!(
                pressed.iter().any(|message| matches!(
                    message,
                    RepositoryAction::FindInputFocus { focused: false, .. }
                )),
                "the real outside press must blur Find before Reload"
            );
        }
        for message in pressed {
            drop(runtime.update(message));
        }
        let released = event(
            card,
            &runtime,
            &mut tree,
            &renderer,
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )),
            Some(0),
        );
        assert!(
            released
                .iter()
                .any(|message| matches!(message, RepositoryAction::Reload { .. }))
        );
        for message in released {
            drop(runtime.update(message));
        }
        assert!(runtime.retains_reload_buffer());
        assert_eq!(runtime.reload_input, Some(owner));
        // Mount the actual Loading surface, retiring the focused field.
        apply(card, &runtime, &mut tree, &renderer, Task::none());
        let identity = runtime.current_identity(card, "notes.txt".into());
        finish_reload_read(&mut runtime, identity);
        let target = runtime.loaded_focus_target(card).unwrap();
        let expected = match owner {
            EditorInput::Find => find_focus_id(card),
            EditorInput::Source => editor_focus_id(card),
            EditorInput::Replace => replace_focus_id(card),
        };
        assert_eq!(target, expected);
        apply(
            card,
            &runtime,
            &mut tree,
            &renderer,
            crate::workspace_widget::focus_after_loading(editor_focus_id(card), target),
        );
        // Drain passive native callbacks before checking retained selection.
        let messages = event(
            card,
            &runtime,
            &mut tree,
            &renderer,
            iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
            None,
        );
        for message in messages {
            drop(runtime.update(message));
        }
        let DocumentState::Editable(document) = &runtime.document else {
            panic!()
        };
        assert_eq!(
            document.content.cursor(),
            cursor,
            "restoring {owner:?} changed source selection"
        );
        let messages = event(card, &runtime, &mut tree, &renderer, key("q"), None);
        for message in messages {
            drop(runtime.update(message));
        }
        match owner {
            EditorInput::Find => {
                assert_eq!(runtime.find_query, "lineq");
                assert_eq!(runtime.editor_text().as_deref(), Some(edited.as_str()));
            }
            EditorInput::Replace => {
                assert_eq!(runtime.replace_query, "rq");
                assert_eq!(runtime.editor_text().as_deref(), Some(edited.as_str()));
            }
            EditorInput::Source => {
                assert_eq!(runtime.find_query, "line");
                assert_ne!(runtime.editor_text().as_deref(), Some(edited.as_str()));
            }
        }
    }
}

#[test]
fn programmatic_query_focus_routes_find_keys_once_before_native_input() {
    use cosmic::iced::{
        Event, Rectangle, Size,
        advanced::{
            Layout, Shell, clipboard,
            widget::{Tree, operation},
        },
        mouse,
    };
    let renderer = headless_renderer();
    for (key, modifiers, focused, captured, expected) in [
        (
            keyboard::key::Named::Enter,
            keyboard::Modifiers::empty(),
            false,
            false,
            None,
        ),
        (
            keyboard::key::Named::Enter,
            keyboard::Modifiers::empty(),
            true,
            false,
            Some("next"),
        ),
        (
            keyboard::key::Named::Enter,
            keyboard::Modifiers::SHIFT,
            true,
            false,
            Some("previous"),
        ),
        (
            keyboard::key::Named::Escape,
            keyboard::Modifiers::empty(),
            true,
            false,
            Some("close"),
        ),
        (
            keyboard::key::Named::Escape,
            keyboard::Modifiers::empty(),
            true,
            true,
            None,
        ),
        (
            keyboard::key::Named::ArrowDown,
            keyboard::Modifiers::empty(),
            true,
            false,
            None,
        ),
    ] {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "a.rs", "hit\nhit\nhit\n");
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "hit".into(),
        }));
        assert!(
            !runtime.find_input_is_focused(),
            "operation focus emits no model callback"
        );
        let mut element = find_strip(card, &runtime).unwrap();
        let mut tree = Tree::new(&element);
        let bounds = Rectangle::with_size(Size::new(600.0, 240.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        if focused {
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::focusable::focus::<()>(find_focus_id(card)),
            );
        }
        let event = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        });
        let mut published = Vec::new();
        let mut shell = Shell::new(&mut published);
        if captured {
            shell.capture_event();
        }
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &bounds,
        );
        if expected.is_some() {
            assert!(shell.is_event_captured());
        }
        drop(shell);
        drop(element);
        if let Some(expected) = expected {
            assert_eq!(
                published.len(),
                1,
                "one widget event must emit exactly one Find command"
            );
            assert!(matches!(
                (&published[0], expected),
                (RepositoryAction::FindNext { .. }, "next")
                    | (RepositoryAction::FindPrevious { .. }, "previous")
                    | (RepositoryAction::CloseFind { .. }, "close")
            ));
            drop(runtime.update(published.pop().unwrap()));
            match expected {
                "next" => assert_eq!(runtime.find_match_position(), Some((1, 3))),
                "previous" => assert_eq!(runtime.find_match_position(), Some((2, 3))),
                "close" => assert!(!runtime.find_is_open()),
                _ => unreachable!(),
            }
        } else {
            assert!(
                published.is_empty(),
                "unfocused/captured/non-Find keys belong to their original owner"
            );
            assert_eq!(runtime.find_match_position(), Some((0, 3)));
        }
    }
}

#[test]
fn stepping_back_from_the_first_match_wraps_to_the_last_one() {
    // `index.wrapping_sub(1) % total` looks like it wraps and does not:
    // `usize::MAX % total` is `total - 1` only where `total` divides
    // 2^64, so Previous on three or five matches silently re-selects the
    // first one and looks like a dead button.
    for total in [1usize, 2, 3, 5, 7] {
        let card = CardId::new();
        let text = "alpha\n".repeat(total);
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", &text);
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        }));
        assert_eq!(runtime.find_total, total);
        assert_eq!(runtime.find_index, 0);

        drop(runtime.update(RepositoryAction::FindPrevious { card }));
        assert_eq!(
            runtime.find_index,
            total - 1,
            "Previous from the first of {total} matches must land on the last"
        );
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!();
        };
        assert_eq!(
            document.content.cursor().position.line,
            total - 1,
            "the reported occurrence must be the one actually selected"
        );
    }
}

#[test]
fn find_stays_editable_and_closing_it_preserves_the_caret() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "alpha\n");
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    assert!(runtime.find_is_open());

    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('z')),
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    assert_eq!(
        document.content.text(),
        "zalpha\n",
        "find leaves the buffer editable while it is open"
    );

    drop(runtime.update(RepositoryAction::CloseFind { card }));
    assert!(!runtime.find_is_open());
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('z')),
    }));
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    assert_eq!(
        document.content.text(),
        "zzalpha\n",
        "closing find preserves the caret after edits"
    );
}

#[test]
#[ignore = "explicit complete 8MiB worker admission/planning/native publication benchmark"]
fn benchmark_dense_eight_mib_replace_worker_and_native_publication() {
    const BYTES: usize = 8 * 1024 * 1024;
    for irregular in [false, true] {
        let original = if irregular {
            format!("{}{}", "aλ".repeat(BYTES / 3), "a".repeat(BYTES % 3))
        } else {
            "a".repeat(BYTES)
        };
        assert_eq!(original.len(), BYTES);
        let count = if irregular {
            2 * (BYTES / 3) + BYTES % 3
        } else {
            BYTES
        };
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "dense.txt", &original);
        update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
        if irregular {
            update_find_settled(
                &mut runtime,
                RepositoryAction::FindOptionsChanged {
                    card,
                    option: crate::ui_controls::FindOption::Regex,
                    enabled: true,
                },
            );
        }
        update_find_settled(
            &mut runtime,
            RepositoryAction::FindQueryChanged {
                card,
                query: if irregular { "." } else { "a" }.into(),
            },
        );
        assert_eq!(runtime.find_total, count);
        update_find_settled(
            &mut runtime,
            RepositoryAction::ReplaceQueryChanged {
                card,
                query: "b".into(),
            },
        );
        text_editor::take_text_call_count();
        let started = Instant::now();
        let planning = runtime.update(RepositoryAction::ReplaceMatch { card, all: true });
        let admission = started.elapsed();
        assert!(runtime.find_replacement.is_some());
        assert_eq!(text_editor::take_text_call_count(), 0);
        let started = Instant::now();
        let prepared = find_output(planning);
        let worker_wait = started.elapsed();
        let started = Instant::now();
        let refresh = runtime.update(prepared);
        let publication = started.elapsed();
        let output = buffer_text(&runtime);
        assert_eq!(output.len(), count);
        assert!(output.bytes().all(|byte| byte == b'b'));
        settle_find_tasks(&mut runtime, refresh);
        assert_eq!(runtime.find_total, if irregular { count } else { 0 });
        let started = Instant::now();
        let refresh = runtime.update(RepositoryAction::Undo { card });
        let undo_update = started.elapsed();
        assert_eq!(buffer_text(&runtime), original);
        settle_find_tasks(&mut runtime, refresh);
        assert_eq!(runtime.find_total, count);
        let started = Instant::now();
        let refresh = runtime.update(RepositoryAction::Redo { card });
        let redo_update = started.elapsed();
        assert_eq!(buffer_text(&runtime), output);
        settle_find_tasks(&mut runtime, refresh);
        eprintln!(
            "find8MiB_worker irregular={irregular} input_bytes={BYTES} matches={count} output_bytes={} admission={admission:?} worker_wait={worker_wait:?} native_publication_update={publication:?} undo_update={undo_update:?} redo_update={redo_update:?}; headless native content/update only, no glyph layout/render or application RSS",
            output.len()
        );
    }
}

#[test]
#[ignore = "explicit complete 100k-match Replace All and history benchmark"]
fn large_replace_all_and_history_microbenchmark() {
    let card = CardId::new();
    let original = "token\r\n".repeat(100_000);
    let replaced = "replacement\r\n".repeat(100_000);
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "large.txt", &original);
    update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
    let scan = runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "token".into(),
    });
    update_find_settled(&mut runtime, find_output(scan));
    assert_eq!(runtime.find_total, 100_000);
    update_find_settled(
        &mut runtime,
        RepositoryAction::ReplaceQueryChanged {
            card,
            query: "replacement".into(),
        },
    );
    let started = std::time::Instant::now();
    update_find_settled(
        &mut runtime,
        RepositoryAction::ReplaceMatch { card, all: true },
    );
    let replace_elapsed = started.elapsed();
    assert_eq!(buffer_text(&runtime), replaced);
    update_find_settled(&mut runtime, RepositoryAction::CloseFind { card });
    let started = std::time::Instant::now();
    update_find_settled(&mut runtime, RepositoryAction::Undo { card });
    let undo_elapsed = started.elapsed();
    assert_eq!(buffer_text(&runtime), original);
    let started = std::time::Instant::now();
    update_find_settled(&mut runtime, RepositoryAction::Redo { card });
    let redo_elapsed = started.elapsed();
    assert_eq!(buffer_text(&runtime), replaced);
    eprintln!(
        "100k matches complete Replace All: {replace_elapsed:?}; undo: {undo_elapsed:?}; redo: {redo_elapsed:?} (unoptimized test profile, excludes layout)"
    );
}

#[test]
fn find_edits_refresh_counts_without_moving_the_editing_caret() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "alpha alpha");
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    drop(runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "alpha".into(),
    }));
    assert_eq!(runtime.find_total, 2);
    if let DocumentState::Editable(document) = &mut runtime.document {
        // Native move_to(None) preserves an existing selection. A click
        // places a fresh caret, so replace the selected Find match here.
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position {
                line: 0,
                column: 11,
            },
            selection: None,
        });
        assert!(document.content.selection().is_none());
    }
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('x')),
    }));
    assert_eq!(buffer_text(&runtime), "alpha alphax");
    // Literal search still matches a prefix until whole-word is enabled.
    assert_eq!(runtime.find_total, 2);
    assert_eq!(runtime.cursor_position(), Some(LspPosition::new(0, 12)));
    drop(runtime.update(RepositoryAction::FindOptionsChanged {
        card,
        option: crate::ui_controls::FindOption::WholeWord,
        enabled: true,
    }));
    assert_eq!(runtime.find_total, 1);
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(runtime.find_total, 2);
    assert!(runtime.find_is_open());
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(runtime.find_total, 1);
}

#[test]
fn find_options_scope_and_invalid_patterns_are_applied_to_current_revision() {
    let card = CardId::new();
    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(card, "notes.txt", "alpha ALPHA alpha_beta alpha");
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.content.move_to(text_editor::Cursor {
            position: text_editor::Position {
                line: 0,
                column: 11,
            },
            selection: Some(text_editor::Position { line: 0, column: 0 }),
        });
    }
    update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindQueryChanged {
            card,
            query: "alpha".into(),
        },
    );
    assert_eq!(runtime.find_total, 4);
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindSelectionChanged {
            card,
            enabled: true,
        },
    );
    assert_eq!(runtime.find_total, 2);
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::CaseSensitive,
            enabled: true,
        },
    );
    assert_eq!(runtime.find_total, 1);
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindSelectionChanged {
            card,
            enabled: false,
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::WholeWord,
            enabled: true,
        },
    );
    assert_eq!(runtime.find_total, 2);
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::Regex,
            enabled: true,
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindQueryChanged {
            card,
            query: "[".into(),
        },
    );
    assert_eq!(runtime.find_total, 0);
    assert!(runtime.find_error.is_some());
}

#[test]
fn find_selection_refuses_moved_scope_after_edits_and_history_until_recaptured() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "foo foo foo");
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.content.move_to(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 7 },
            selection: Some(text_editor::Position { line: 0, column: 4 }),
        });
    }
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    drop(runtime.update(RepositoryAction::FindSelectionChanged {
        card,
        enabled: true,
    }));
    assert_eq!(runtime.find_total, 1);
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Move(text_editor::Motion::DocumentStart),
    }));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("xxxx".into()))),
    }));
    assert_eq!(buffer_text(&runtime), "xxxxfoo foo foo");
    assert_eq!(runtime.find_total, 0);
    assert!(
        runtime
            .find_error
            .as_deref()
            .unwrap()
            .contains("captured selection changed")
    );
    drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
        card,
        query: "bar".into(),
    }));
    drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
    assert_eq!(
        buffer_text(&runtime),
        "xxxxfoo foo foo",
        "stale scope must never replace a different occurrence"
    );
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(buffer_text(&runtime), "foo foo foo");
    assert_eq!(
        runtime.find_total, 0,
        "Undo does not silently recapture coordinates"
    );
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(buffer_text(&runtime), "xxxxfoo foo foo");
    assert_eq!(runtime.find_total, 0);
    // Deliberate native selection and toggle recaptures the new middle
    // occurrence, even though a global query selection moves the caret.
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Move(text_editor::Motion::DocumentStart),
    }));
    for _ in 0..8 {
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Move(text_editor::Motion::Right),
        }));
    }
    for _ in 0..3 {
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Select(text_editor::Motion::Right),
        }));
    }
    drop(runtime.update(RepositoryAction::FindSelectionChanged {
        card,
        enabled: false,
    }));
    drop(runtime.update(RepositoryAction::FindSelectionChanged {
        card,
        enabled: true,
    }));
    assert_eq!(runtime.find_total, 1);
    assert!(runtime.find_error.is_none());
    drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
    assert_eq!(buffer_text(&runtime), "xxxxfoo bar foo");
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(buffer_text(&runtime), "xxxxfoo foo foo");
}

#[test]
fn replacement_output_refusal_preserves_buffer_history_revision_and_selection() {
    let card = CardId::new();
    for (original, all, replacement_bytes) in [
        (
            "a".to_owned(),
            false,
            crate::editor::MAX_EDITABLE_FILE_BYTES as usize + 1,
        ),
        (
            "aa".to_owned(),
            true,
            crate::editor::MAX_EDITABLE_FILE_BYTES as usize / 2 + 1,
        ),
        (
            format!("aa{}", "x".repeat(70_000)),
            true,
            crate::editor::MAX_EDITABLE_FILE_BYTES as usize / 2,
        ),
        (
            format!("{}{}", "a".repeat(257), "x".repeat(70_000)),
            true,
            40_000,
        ),
    ] {
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", &original);
        drop(runtime.update(RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
        }));
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), original);
        update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
        update_find_settled(
            &mut runtime,
            RepositoryAction::FindQueryChanged {
                card,
                query: "a".into(),
            },
        );
        update_find_settled(
            &mut runtime,
            RepositoryAction::ReplaceQueryChanged {
                card,
                query: "z".repeat(replacement_bytes),
            },
        );
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!()
        };
        let cursor = document.content.cursor();
        let revision = document.history.revision();
        let undo = document.history.can_undo();
        let redo = document.history.can_redo();
        assert!(redo, "refusal must preserve an existing redo transaction");
        update_find_settled(&mut runtime, RepositoryAction::ReplaceMatch { card, all });
        assert!(
            runtime
                .find_error
                .as_deref()
                .unwrap()
                .contains("result size limit")
        );
        let DocumentState::Editable(document) = &runtime.document else {
            unreachable!()
        };
        assert_eq!(document.content.cursor(), cursor);
        assert_eq!(document.history.revision(), revision);
        assert_eq!(document.history.can_undo(), undo);
        assert_eq!(document.history.can_redo(), redo);
        assert_eq!(buffer_text(&runtime), original);
    }
}

#[test]
fn sparse_replace_all_near_editable_limit_retains_one_small_undo_transaction() {
    let card = CardId::new();
    let middle = format!("{}\n", "x".repeat(127)).repeat(65_535);
    let original = format!("target\n{middle}target\n");
    let expected = original.replace("target", "value");
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "large.txt", &original);
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    let scan = runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "target".into(),
    });
    drop(runtime.update(find_output(scan)));
    assert_eq!(runtime.find_total, 2);
    let previous_version = lsp_version(&runtime);
    drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
        card,
        query: "value".into(),
    }));
    let scan = runtime.update(RepositoryAction::ReplaceMatch { card, all: true });
    drop(runtime.update(find_output(scan)));
    assert_eq!(buffer_text(&runtime), expected);
    assert_eq!(lsp_version(&runtime), previous_version + 1);
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!()
    };
    assert!(document.history.can_undo());
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(buffer_text(&runtime), original);
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(buffer_text(&runtime), expected);
}

#[test]
fn regex_capture_replace_all_and_preserve_case_are_each_one_undo_step() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "call1 call2");
    update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::Regex,
            enabled: true,
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindQueryChanged {
            card,
            query: r"call(\d)".into(),
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::ReplaceQueryChanged {
            card,
            query: "item$1".into(),
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::ReplaceMatch { card, all: true },
    );
    assert_eq!(buffer_text(&runtime), "item1 item2");
    update_find_settled(&mut runtime, RepositoryAction::Undo { card });
    assert_eq!(buffer_text(&runtime), "call1 call2");
    assert!(runtime.find_is_open());

    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(card, "notes.txt", "word Word WORD wOrd");
    update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindQueryChanged {
            card,
            query: "word".into(),
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::ReplaceQueryChanged {
            card,
            query: "value".into(),
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::PreserveCaseChanged {
            card,
            enabled: true,
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::ReplaceMatch { card, all: true },
    );
    assert_eq!(buffer_text(&runtime), "value Value VALUE value");
    update_find_settled(&mut runtime, RepositoryAction::Undo { card });
    assert_eq!(buffer_text(&runtime), "word Word WORD wOrd");
}

#[test]
fn unicode_find_replace_all_is_one_edit_and_renews_matches() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "Straße STRASSE\n");
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    drop(runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "strasse".into(),
    }));
    assert_eq!(runtime.find_total, 2);
    drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
        card,
        query: "road".into(),
    }));
    drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!()
    };
    assert_eq!(document.content.text(), "road road\n");
    assert!(runtime.is_dirty());
    assert_eq!(runtime.find_total, 0);
    drop(runtime.update(RepositoryAction::CloseFind { card }));
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(buffer_text(&runtime), "Straße STRASSE\n");
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(buffer_text(&runtime), "road road\n");
    assert_eq!(runtime.find_total, 0);
}

#[test]
fn replace_all_preserve_case_keeps_greek_context_and_one_undo() {
    for replacement in ["ΟΣ", "ΟΣ\u{301}"] {
        let card = CardId::new();
        let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "greek.txt", "foo foo");
        drop(runtime.update(RepositoryAction::OpenFind { card }));
        drop(runtime.update(RepositoryAction::FindQueryChanged {
            card,
            query: "foo".into(),
        }));
        drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
            card,
            query: replacement.into(),
        }));
        drop(runtime.update(RepositoryAction::PreserveCaseChanged {
            card,
            enabled: true,
        }));
        drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
        let expected = replacement.to_lowercase();
        assert_eq!(buffer_text(&runtime), format!("{expected} {expected}"));
        drop(runtime.update(RepositoryAction::Undo { card }));
        assert_eq!(buffer_text(&runtime), "foo foo");
        drop(runtime.update(RepositoryAction::Redo { card }));
        assert_eq!(buffer_text(&runtime), format!("{expected} {expected}"));
    }
}

#[test]
fn compact_dense_find_preserves_all_occurrences_wraps_and_one_undo_replace_all() {
    let card = CardId::new();
    let text = "a".repeat(16 * 1024);
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "dense.txt", &text);
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    drop(runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "a".into(),
    }));
    assert_eq!(runtime.find_total, text.len());
    assert!(runtime.find_matches.retained_bytes() < 1024);
    let allocation = runtime.find_matches.allocation_id();
    drop(runtime.update(RepositoryAction::FindPrevious { card }));
    assert_eq!(runtime.find_index, text.len() - 1);
    assert_eq!(
        runtime
            .find_matches
            .get(runtime.find_index)
            .unwrap()
            .0
            .column,
        text.len() - 1
    );
    drop(runtime.update(RepositoryAction::FindNext { card }));
    assert_eq!(runtime.find_index, 0);
    assert_eq!(runtime.find_matches.allocation_id(), allocation);
    drop(runtime.update(RepositoryAction::ReplaceQueryChanged {
        card,
        query: "b".into(),
    }));
    drop(runtime.update(RepositoryAction::ReplaceMatch { card, all: true }));
    assert_eq!(buffer_text(&runtime), "b".repeat(text.len()));
    assert_eq!(runtime.find_total, 0);
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(buffer_text(&runtime), text);
    assert_eq!(runtime.find_total, text.len());
    drop(runtime.update(RepositoryAction::Redo { card }));
    assert_eq!(buffer_text(&runtime), "b".repeat(text.len()));
}

fn large_unicode_replacement_fixture() -> (CardId, RepositoryRuntime, String) {
    let card = CardId::new();
    let original = "λa𝄞\r\n".repeat(10_000);
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "unicode.txt", &original);
    update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::Regex,
            enabled: true,
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindQueryChanged {
            card,
            query: "λ(a)𝄞".into(),
        },
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::ReplaceQueryChanged {
            card,
            query: "$1β".into(),
        },
    );
    assert_eq!(runtime.find_total, 10_000);
    (card, runtime, original)
}

#[test]
fn large_replace_planning_keeps_edits_live_and_rejects_obsolete_output() {
    let (card, mut runtime, original) = large_unicode_replacement_fixture();
    use cosmic::iced::futures::{FutureExt, StreamExt};
    // Drop the pause sender before the executor on unwind so its blocking
    // worker always leaves the pause before runtime teardown waits for it.
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    runtime.find_replacement_pause = Some((entered_tx, release_rx));
    text_editor::take_text_call_count();
    let held = runtime.update(RepositoryAction::ReplaceMatch { card, all: true });
    assert!(runtime.find_replacement.is_some());
    assert_eq!(
        text_editor::take_text_call_count(),
        0,
        "admission must capture immutable lines without serializing on UI"
    );
    assert_eq!(buffer_text(&runtime), original);
    let mut stream = iced::runtime::task::into_stream(held).unwrap();
    let admitted = Instant::now();
    executor.block_on(std::future::poll_fn(|cx| {
        assert!(stream.next().poll_unpin(cx).is_pending());
        if entered_rx.try_recv().is_ok() {
            return std::task::Poll::Ready(());
        }
        assert!(admitted.elapsed() < std::time::Duration::from_secs(5));
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }));
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 0 },
            selection: None,
        });
    }
    let edited = runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('z')),
    });
    assert!(
        runtime
            .find_replacement
            .as_ref()
            .unwrap()
            .cancellation
            .is_cancelled()
    );
    release_tx.send(()).unwrap();
    let released = Instant::now();
    let completion = executor.block_on(std::future::poll_fn(|cx| {
        if let std::task::Poll::Ready(completion) = stream.next().poll_unpin(cx) {
            return std::task::Poll::Ready(completion);
        }
        assert!(
            released.elapsed() < std::time::Duration::from_secs(5),
            "replacement fixture did not complete after release"
        );
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }));
    let iced::runtime::Action::Output(callback) = completion.unwrap() else {
        panic!("replacement completion");
    };
    assert!(matches!(
        &callback,
        RepositoryAction::FindReplacementLoaded {
            result: Ok(None),
            ..
        }
    ));
    drop(runtime.update(callback));
    settle_find_tasks(&mut runtime, edited);
    assert_eq!(buffer_text(&runtime), format!("z{original}"));
    assert!(runtime.find_replacement.is_none());
    update_find_settled(&mut runtime, RepositoryAction::Undo { card });
    assert_eq!(buffer_text(&runtime), original);
}

#[test]
fn prepared_replace_result_is_revision_and_intent_fenced_and_one_unicode_undo() {
    let (card, mut runtime, original) = large_unicode_replacement_fixture();
    let task = runtime.update(RepositoryAction::ReplaceMatch { card, all: true });
    let obsolete = find_output(task);
    assert_eq!(
        buffer_text(&runtime),
        original,
        "worker completion alone cannot mutate native state"
    );
    update_find_settled(
        &mut runtime,
        RepositoryAction::ReplaceQueryChanged {
            card,
            query: "$1γ".into(),
        },
    );
    drop(runtime.update(obsolete));
    assert_eq!(
        buffer_text(&runtime),
        original,
        "already-completed output retains its original replacement intent"
    );
    let task = runtime.update(RepositoryAction::ReplaceMatch { card, all: true });
    assert_eq!(buffer_text(&runtime), original);
    settle_find_tasks(&mut runtime, task);
    let expected = "aγ\r\n".repeat(10_000);
    assert_eq!(buffer_text(&runtime), expected);
    assert!(runtime.find_replacement.is_none());
    update_find_settled(&mut runtime, RepositoryAction::Undo { card });
    assert_eq!(buffer_text(&runtime), original);
    update_find_settled(&mut runtime, RepositoryAction::Redo { card });
    assert_eq!(buffer_text(&runtime), expected);
}

#[test]
fn closed_or_replaced_runtime_rejects_prepared_replace_callback() {
    let (card, mut runtime, original) = large_unicode_replacement_fixture();
    let task = runtime.update(RepositoryAction::ReplaceMatch { card, all: true });
    let callback = find_output(task);
    update_find_settled(&mut runtime, RepositoryAction::CloseFind { card });
    drop(runtime.update(callback.clone()));
    assert_eq!(buffer_text(&runtime), original);
    assert!(runtime.find_replacement.is_none());
    let (mut successor, _) = RepositoryRuntime::open_fixture(card, "unicode.txt", &original);
    drop(successor.update(callback));
    assert_eq!(buffer_text(&successor), original);
}

#[test]
fn even_small_regex_search_runs_off_update_thread_without_changing_greedy_matches() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "regex.txt", "AAAA");
    update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindOptionsChanged {
            card,
            option: crate::ui_controls::FindOption::Regex,
            enabled: true,
        },
    );
    let task = runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "A.*|A".into(),
    });
    assert_eq!(runtime.find_total, 0);
    assert!(runtime.find_active.is_some());
    settle_find_tasks(&mut runtime, task);
    assert_eq!(runtime.find_total, 1);
    assert_eq!(runtime.find_matches.get(0).unwrap().1.column, 4);
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindQueryChanged {
            card,
            query: ".*[^A-Z]|[A-Z]".into(),
        },
    );
    assert_eq!(runtime.find_total, 4);
}

#[test]
fn shared_find_preferences_dispatch_refreshed_regex_results() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "a1 a.");
    update_find_settled(&mut runtime, RepositoryAction::OpenFind { card });
    update_find_settled(
        &mut runtime,
        RepositoryAction::FindQueryChanged {
            card,
            query: "a.".into(),
        },
    );
    assert_eq!(runtime.find_total, 1);
    let mut options = runtime.find_preferences().0;
    options.regex = true;
    let task = runtime.update_find_preferences(card, options, true);
    assert_eq!(runtime.find_total, 0);
    assert!(runtime.find_active.is_some());
    settle_find_tasks(&mut runtime, task);
    assert_eq!(runtime.find_total, 2);
    assert_eq!(runtime.find_preferences(), (options, true));
}

#[test]
fn find_navigation_reuses_cached_match_storage_and_open_starts_from_selection() {
    let card = CardId::new();
    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(card, "notes.txt", "alpha alpha alpha\n");
    assert_eq!(runtime.select_text_match("alpha", 1), Some(3));
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    assert_eq!(runtime.find_index, 1);
    let cached = runtime.find_matches.allocation_id();
    for _ in 0..100 {
        drop(runtime.update(RepositoryAction::FindNext { card }));
        assert_eq!(runtime.find_matches.allocation_id(), cached);
        drop(runtime.update(RepositoryAction::FindPrevious { card }));
        assert_eq!(runtime.find_matches.allocation_id(), cached);
        assert_eq!(runtime.find_index, 1);
    }
}

fn settle_find_tasks(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
    use cosmic::iced::futures::StreamExt;
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut pending = vec![task];
            while let Some(task) = pending.pop() {
                let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                    continue;
                };
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(action) = action {
                        assert!(matches!(
                            &action,
                            RepositoryAction::FindLoaded { .. }
                                | RepositoryAction::FindReplacementLoaded { .. }
                        ));
                        pending.push(runtime.update(action));
                    }
                }
            }
        });
}

fn update_find_settled(runtime: &mut RepositoryRuntime, action: RepositoryAction) {
    let task = runtime.update(action);
    settle_find_tasks(runtime, task);
}

fn find_output(task: Task<RepositoryAction>) -> RepositoryAction {
    use cosmic::iced::futures::StreamExt;
    let mut stream = iced::runtime::task::into_stream(task).expect("find worker");
    let output = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(stream.next())
        .unwrap();
    let iced::runtime::Action::Output(action) = output else {
        panic!("find output");
    };
    action
}

#[test]
fn large_find_fences_superseded_queries_reuses_snapshot_and_rejects_closed_results() {
    let card = CardId::new();
    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(card, "notes.txt", &"alpha beta\r\n".repeat(3000));
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    let first = runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "alpha".into(),
    });
    assert_eq!(
        runtime.find_total, 0,
        "large queries must leave the UI thread"
    );
    let snapshot = Arc::clone(&runtime.find_snapshot.as_ref().unwrap().2);
    drop(runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "beta".into(),
    }));
    assert!(
        runtime
            .find_active
            .as_ref()
            .unwrap()
            .cancellation
            .is_cancelled()
    );
    // Retire the only outstanding worker before the latest query gets one.
    let latest = runtime.update(find_output(first));
    assert!(Arc::ptr_eq(
        &snapshot,
        &runtime.find_snapshot.as_ref().unwrap().2
    ));
    assert_eq!(runtime.find_total, 0);
    drop(runtime.update(find_output(latest)));
    assert_eq!(runtime.find_total, 3000);
    let cached = runtime.find_matches.allocation_id();
    drop(runtime.update(RepositoryAction::FindNext { card }));
    assert_eq!(runtime.find_matches.allocation_id(), cached);
    assert!(runtime.find_active.is_none());

    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    document.content.set_cursor(text_editor::Cursor {
        position: text_editor::Position { line: 0, column: 0 },
        selection: None,
    });
    document.apply(text_editor::Action::Edit(text_editor::Edit::Insert('x')));
    runtime.push_lsp_change(None);
    let revised = runtime.find_task(card);
    assert_eq!(
        runtime.find_index, 1,
        "pending searches preserve the selected occurrence"
    );
    drop(runtime.update(find_output(revised)));
    assert_eq!(runtime.find_total, 3000);
    assert_eq!(runtime.find_index, 1);

    let closed = runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "alpha".into(),
    });
    drop(runtime.update(RepositoryAction::CloseFind { card }));
    drop(runtime.update(find_output(closed)));
    assert_eq!(runtime.find_total, 0);
    assert!(runtime.find_matches.is_empty());
    assert!(runtime.find_snapshot.is_none());
    assert!(runtime.find_active.is_none());
}

#[test]
fn large_find_rejects_a_result_for_the_previous_document_revision() {
    let card = CardId::new();
    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(card, "notes.txt", &"alpha beta\n".repeat(3000));
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    let old = runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "alpha".into(),
    });
    let snapshot = Arc::clone(&runtime.find_snapshot.as_ref().unwrap().2);
    let DocumentState::Editable(document) = &mut runtime.document else {
        unreachable!();
    };
    // A buffer edit delivered outside the find bar (such as completion or
    // discard) must invalidate a worker as well as its displayed matches.
    document.content.perform(text_editor::Action::SelectAll);
    document.apply(text_editor::Action::Edit(text_editor::Edit::Paste(
        Arc::new("beta gamma\n".repeat(3000)),
    )));
    let current = runtime.update(find_output(old));
    assert!(!Arc::ptr_eq(
        &snapshot,
        &runtime.find_snapshot.as_ref().unwrap().2
    ));
    assert_eq!(runtime.find_total, 0);
    drop(runtime.update(find_output(current)));
    assert_eq!(runtime.find_total, 0);
    assert!(runtime.find_matches.is_empty());
    assert!(runtime.find_active.is_none());
}

#[test]
fn large_find_rejects_a_completed_result_from_a_replaced_card_runtime() {
    let card = CardId::new();
    let (mut old, _) = RepositoryRuntime::open_fixture(card, "notes.txt", &"alpha\n".repeat(3000));
    drop(old.update(RepositoryAction::OpenFind { card }));
    let old_task = old.update(RepositoryAction::FindQueryChanged {
        card,
        query: "alpha".into(),
    });
    // Completion can already be in the application's event queue when
    // Retry replaces a clean repository runtime under the same card ID.
    let old_output = find_output(old_task);
    drop(old);

    let (mut replacement, _) =
        RepositoryRuntime::open_fixture(card, "notes.txt", &"alpha beta\n".repeat(3000));
    drop(replacement.update(RepositoryAction::OpenFind { card }));
    let current = replacement.update(RepositoryAction::FindQueryChanged {
        card,
        query: "beta".into(),
    });
    let ticket = replacement.find_active.as_ref().unwrap().ticket;
    drop(replacement.update(old_output));
    assert_eq!(replacement.find_active.as_ref().unwrap().ticket, ticket);
    assert!(replacement.find_matches.is_empty());
    assert_eq!(replacement.find_total, 0);

    drop(replacement.update(find_output(current)));
    assert_eq!(replacement.find_total, 3000);
    assert_eq!(replacement.find_matches.get(0).unwrap().0.column, 6);
    assert!(replacement.find_active.is_none());
}

#[test]
fn returning_from_diff_to_edit_preserves_the_visible_source_line() {
    let card = CardId::new();
    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(card, "notes.txt", "line\n".repeat(100).as_str());
    runtime.source_surface = SourceSurface::Diff;
    let mut model = model_with(1);
    model.inline_rows = (0..100)
        .map(|line| InlineDiffRow {
            old_line: Some(line + 1),
            new_line: Some(line + 1),
            text: "line".into(),
            kind: DiffRowKind::Context,
            changed: Vec::new(),
        })
        .collect();
    runtime.set_diff_state(
        runtime.diff_basis.clone(),
        DiffState::ready(staged_text_diff(model)),
    );
    drop(runtime.update(RepositoryAction::DiffViewport {
        card,
        offset: 30.0 * DIFF_ROW_HEIGHT,
        height: 300.0,
        horizontal: 0.0,
        user_initiated: true,
    }));
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: false,
    }));
    assert_eq!(runtime.viewports.editor.line, 30);
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
}

#[test]
fn changing_the_buffer_under_find_renews_the_count_it_reports() {
    let card = CardId::new();
    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(card, "notes.txt", "alpha alpha alpha\n");
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("alpha ".into()))),
    }));
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    drop(runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "alpha".into(),
    }));
    assert_eq!(runtime.find_total, 4);
    drop(runtime.update(RepositoryAction::FindPrevious { card }));
    assert_eq!(runtime.find_index, 3);

    // Discarding rebuilds the buffer behind the open strip: one of the
    // four occurrences it was counting no longer exists.
    drop(runtime.update(RepositoryAction::DiscardEdits { card }));
    assert_eq!(
        runtime.find_total, 3,
        "a stale total keeps claiming matches the buffer no longer has"
    );
    assert!(
        runtime.find_index < runtime.find_total,
        "the selected occurrence must stay inside the total"
    );
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    assert_eq!(document.content.cursor().position.column, 17);
    assert!(
        document.content.selection().is_none(),
        "refreshing matches preserves the clamped editing caret instead of selecting another occurrence"
    );
    drop(runtime.update(RepositoryAction::FindNext { card }));
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    assert_eq!(
        document.content.selection().as_deref(),
        Some("alpha"),
        "explicit Find navigation still selects the next occurrence"
    );
}

#[test]
fn an_empty_query_counts_nothing_and_selects_nothing() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", "alpha beta\n");
    drop(runtime.update(RepositoryAction::OpenFind { card }));
    drop(runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "alpha".into(),
    }));
    assert_eq!(runtime.find_total, 1);

    drop(runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "   ".into(),
    }));
    assert_eq!(runtime.find_total, 0);
    assert_eq!(runtime.find_index, 0);
    let DocumentState::Editable(document) = &runtime.document else {
        unreachable!();
    };
    assert_eq!(
        document.content.cursor().selection,
        None,
        "a query the user cleared leaves nothing highlighted behind it"
    );
}
#[test]
fn sidebar_working_diff_opens_exact_status_without_a_file_list_refresh() {
    let card = CardId::new();
    let (mut runtime, _) = working_diff_runtime(card, "old\n", "working\n");
    let generation = runtime.file_list_generation;
    let file = RepositoryFile {
        path: "new-name.txt".into(),
        staged: true,
        staged_old_path: Some(OldPath::Renamed("old-name.txt".into())),
        ..Default::default()
    };
    drop(runtime.update(RepositoryAction::OpenWorkingDiff {
        card,
        file,
        basis: DiffBasis::HeadToIndex,
    }));
    assert_eq!(runtime.file_list_generation, generation);
    assert_eq!(
        runtime.selected.as_ref().unwrap().path,
        Path::new("new-name.txt")
    );
    assert_eq!(
        runtime.comparison_old_path(),
        Some(PathBuf::from("old-name.txt"))
    );
    assert_eq!(runtime.source_surface, SourceSurface::Diff);
    assert!(matches!(
        runtime.diff_state(&DiffBasis::HeadToIndex),
        Some(DiffState::Loading { .. })
    ));
}

/// A copy reads its source for the diff, but the source is a file of its
/// own: unstaging the copy must not also reset the source's index entry,
/// which can hold a deliberately staged version nothing else keeps.
#[test]
fn a_copy_compares_against_its_source_but_mutates_only_itself() {
    let card = CardId::new();
    for (old_path, mutated_source) in [
        (OldPath::Copied("source.txt".into()), None),
        (
            OldPath::Renamed("source.txt".into()),
            Some(Path::new("source.txt")),
        ),
    ] {
        let (mut runtime, _) = working_diff_runtime(card, "old\n", "working\n");
        drop(runtime.update(RepositoryAction::OpenWorkingDiff {
            card,
            file: RepositoryFile {
                path: "copy.txt".into(),
                staged: true,
                staged_old_path: Some(old_path.clone()),
                ..Default::default()
            },
            basis: DiffBasis::HeadToIndex,
        }));
        assert_eq!(
            runtime.comparison_old_path(),
            Some(PathBuf::from("source.txt")),
            "{old_path:?}"
        );
        let selection = runtime
            .selected_staging_selection(false)
            .expect("the staged path can be unstaged");
        assert_eq!(selection.path.as_path(), Path::new("copy.txt"));
        assert_eq!(
            selection.old_path.as_ref().map(|path| path.as_path()),
            mutated_source,
            "{old_path:?}"
        );
    }
}

#[test]
fn a_copy_source_keeps_its_own_row() {
    let files = assemble_repository_files(b"source.md\0copy.md\0", b"", b"C  copy.md\0source.md\0")
        .unwrap();
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_path())
            .collect::<Vec<_>>(),
        vec![Path::new("copy.md"), Path::new("source.md")]
    );
    assert_eq!(
        files[0].staged_old_path,
        Some(OldPath::Copied("source.md".into()))
    );
}

#[test]
fn contrast_correction_retains_syntax_hue_instead_of_replacing_it() {
    for background in [
        iced::Color::from_rgb8(42, 29, 31),
        iced::Color::from_rgb8(251, 231, 233),
    ] {
        let syntax = iced::Color::from_rgb8(110, 75, 170);
        let adjusted = readable_diff_color(syntax, background);
        assert!(adjusted.relative_contrast(background) >= 4.5);
        assert_ne!(adjusted, iced::Color::WHITE);
        assert_ne!(adjusted, iced::Color::BLACK);
        assert!(adjusted.b > adjusted.r && adjusted.r > adjusted.g);
    }
}

const AGENT_REPLY: &str = "\
## Summary

The build failed because the **cache key** ignored `Cargo.lock`:

1. CI restored a stale target directory.
2. The linker picked up an old object.

```sh
cargo clean
```
";

fn paste(runtime: &mut RepositoryRuntime, card: CardId, text: &str) {
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Paste(std::sync::Arc::new(
            text.into(),
        ))),
    }));
}

/// Lets the edit debounce fire as though typing had stopped long enough ago.
fn settle(runtime: &mut RepositoryRuntime, card: CardId) {
    let ticket = runtime
        .markdown_debounce
        .timer
        .expect("an edit to a nameless buffer arms the debounce");
    runtime.markdown_debounce.deadline = None;
    drop(runtime.update(RepositoryAction::MarkdownDue { card, ticket }));
}

fn editable(runtime: &RepositoryRuntime) -> &EditorDocument {
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("the fixture opens an editable buffer");
    };
    document
}

fn highlight_token(runtime: &RepositoryRuntime) -> String {
    editable(runtime)
        .highlighter(crate::settings::EditorSyntaxTheme::Auto, true)
        .token
}

#[test]
fn a_nameless_buffer_reads_as_its_content_once_typing_settles_never_per_frame() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "Untitled", "");
    assert!(!editable(&runtime).markdown);
    assert_eq!(runtime.language_offer(), Some((false, None)));

    // Pasting a reply in pieces, in the edit surface, reads nothing yet: the
    // language waits for the debounce, which is armed even though nothing
    // parsed is on screen.
    let (head, tail) = AGENT_REPLY.split_at(AGENT_REPLY.len() / 2);
    paste(&mut runtime, card, head);
    paste(&mut runtime, card, tail);
    assert_eq!(runtime.markdown_mode, MarkdownMode::Edit);
    assert_eq!(editable(&runtime).inferences, 0);
    assert!(runtime.markdown_debounce.timer.is_some());

    // Drawing, however often, never infers: the view only reads the result.
    for _ in 0..3 {
        drop(view(card, &runtime));
        drop(selected_file_toolbar(card, &runtime));
        assert_eq!(highlight_token(&runtime), "Untitled");
    }
    assert_eq!(editable(&runtime).inferences, 0);

    settle(&mut runtime, card);
    assert_eq!(editable(&runtime).inferences, 1);
    assert!(editable(&runtime).markdown);
    // Edit-mode highlighting follows the same language as the preview.
    assert_eq!(highlight_token(&runtime), "md");
    // Edit mode does not pay for the preview's parse.
    assert!(editable(&runtime).markdown_items.is_empty());

    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    assert!(!editable(&runtime).markdown_items.is_empty());
    for _ in 0..3 {
        drop(view(card, &runtime));
    }
    assert_eq!(
        editable(&runtime).inferences,
        1,
        "nothing changed to re-read"
    );
}

#[test]
fn a_markdown_buffer_holds_while_an_edit_is_half_typed() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "Untitled", "");
    paste(&mut runtime, card, AGENT_REPLY);
    settle(&mut runtime, card);
    assert!(editable(&runtime).markdown);

    // Select everything and start a new reply: the unclosed emphasis and
    // the lone heading would not have made plain text Markdown, but they
    // keep a Markdown buffer Markdown.
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::SelectAll,
    }));
    paste(&mut runtime, card, "# Plan\n\nThe **ca");
    settle(&mut runtime, card);
    assert!(editable(&runtime).markdown);

    // Prose with no Markdown left in it is plain text again.
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::SelectAll,
    }));
    paste(&mut runtime, card, "Remember to call the dentist.\n");
    settle(&mut runtime, card);
    assert!(!editable(&runtime).markdown);
    assert_eq!(highlight_token(&runtime), "Untitled");
}

#[test]
fn a_chosen_language_wins_over_the_content_and_outlives_a_reload() {
    let card = CardId::new();
    let (mut runtime, identity) = RepositoryRuntime::open_fixture(card, "Untitled", AGENT_REPLY);
    // The load inferred the language on its worker; the fixture says plain,
    // so the runtime keeps what the load reported until an edit.
    drop(runtime.update(RepositoryAction::ChooseLanguage {
        card,
        language: Some(DocumentLanguage::PlainText),
    }));
    assert!(!editable(&runtime).markdown);
    assert_eq!(highlight_token(&runtime), "txt");
    assert_eq!(
        runtime.language_offer(),
        Some((false, Some(DocumentLanguage::PlainText)))
    );

    // Edits no longer arm inference, and the debounce reads nothing.
    let before = editable(&runtime).inferences;
    paste(&mut runtime, card, "\n| a | b |\n|---|---|\n");
    assert!(!editable(&runtime).language_stale());
    if let Some(ticket) = runtime.markdown_debounce.timer {
        runtime.markdown_debounce.deadline = None;
        drop(runtime.update(RepositoryAction::MarkdownDue { card, ticket }));
    }
    assert_eq!(editable(&runtime).inferences, before);
    assert!(!editable(&runtime).markdown);

    // The choice belongs to the file, so the buffer it reloads into keeps it.
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: PathBuf::from("Untitled"),
            content: FileContent::Editable {
                text: AGENT_REPLY.into(),
                content_hash: blake3::hash(AGENT_REPLY.as_bytes()),
                markdown: true,
            },
        }),
    }));
    assert!(!editable(&runtime).markdown);
    assert_eq!(
        runtime.language_choice(Path::new("Untitled")),
        Some(DocumentLanguage::PlainText)
    );

    // Markdown, chosen, previews at once.
    drop(runtime.update(RepositoryAction::ApplyMarkdownMode {
        card,
        preview: true,
    }));
    assert!(editable(&runtime).markdown_items.is_empty());
    drop(runtime.update(RepositoryAction::ChooseLanguage {
        card,
        language: Some(DocumentLanguage::Markdown),
    }));
    assert!(editable(&runtime).markdown);
    assert!(!editable(&runtime).markdown_items.is_empty());

    // Automatic hands it back to the content, read straight away.
    drop(runtime.update(RepositoryAction::ChooseLanguage {
        card,
        language: None,
    }));
    assert_eq!(runtime.language_offer(), Some((true, None)));
    assert_eq!(runtime.language_choice(Path::new("Untitled")), None);
}

#[test]
fn a_named_buffer_offers_no_language_and_highlights_by_its_name() {
    let card = CardId::new();
    let (mut runtime, _) = RepositoryRuntime::open_fixture(card, "notes.txt", AGENT_REPLY);
    assert_eq!(runtime.language_offer(), None);
    drop(runtime.update(RepositoryAction::ChooseLanguage {
        card,
        language: Some(DocumentLanguage::Markdown),
    }));
    assert!(!editable(&runtime).markdown);
    assert_eq!(runtime.language_choice(Path::new("notes.txt")), None);
    paste(&mut runtime, card, "x");
    assert!(!editable(&runtime).language_stale());
    assert_eq!(highlight_token(&runtime), "txt");

    // A README is Markdown by name, and now highlights as Markdown too.
    let (runtime, identity) = RepositoryRuntime::selection_fixture(card, "README");
    let mut runtime = runtime;
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: Ok(LoadedFile {
            source: PathBuf::from("README"),
            content: FileContent::Editable {
                text: "# Title\n".into(),
                content_hash: blake3::hash(b"# Title\n"),
                markdown: true,
            },
        }),
    }));
    assert_eq!(runtime.language_offer(), None);
    assert_eq!(highlight_token(&runtime), "md");
}
