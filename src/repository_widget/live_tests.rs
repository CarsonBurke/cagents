//! Contract tests use the real native backend and actual asynchronous reader.
use super::*;
use iced::futures::StreamExt;

async fn settle(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
    let mut tasks = vec![task];
    while let Some(task) = tasks.pop() {
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(action) = action {
                    tasks.push(runtime.update(action));
                }
            }
        }
    }
}
async fn opened(root: &Path, path: &str) -> (CardId, RepositoryRuntime) {
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
        project: crate::model::ProjectId::new(),
        repository: root.into(),
        worktree: root.into(),
        selected_file: Some(path.into()),
    });
    let task = runtime.open_file(card, path.into());
    settle(&mut runtime, task).await;
    assert!(
        runtime.document_watch.as_ref().unwrap().error().is_none(),
        "native watch must be active"
    );
    (card, runtime)
}
async fn change(card: CardId, runtime: &mut RepositoryRuntime) {
    let watch = runtime.document_watch.as_ref().unwrap().clone();
    tokio::time::timeout(Duration::from_secs(5), watch.receiver().recv())
        .await
        .expect("native event timed out (no polling fallback)")
        .unwrap();
    let task = runtime.update(RepositoryAction::DocumentChanged {
        card,
        watch: watch.id(),
    });
    settle(runtime, task).await;
}
async fn text(card: CardId, runtime: &mut RepositoryRuntime, expected: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.editor_text().as_deref() != Some(expected) {
            change(card, runtime).await;
        }
    })
    .await
    .expect("native update failed to converge");
}
#[tokio::test]
async fn native_in_place_same_size_and_repeated_atomic_replacement() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("live.txt");
    std::fs::write(&path, "before\n").unwrap();
    let (card, mut runtime) = opened(root.path(), "live.txt").await;
    std::fs::write(&path, "after!\n").unwrap();
    text(card, &mut runtime, "after!\n").await;
    for marker in ["second\n", "third!\n", "fourth\n"] {
        std::fs::write(root.path().join("save.tmp"), marker).unwrap();
        std::fs::rename(root.path().join("save.tmp"), &path).unwrap();
        text(card, &mut runtime, marker).await;
    }
}
#[tokio::test]
async fn native_delete_recreate_and_replaced_nested_parent() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("nested/deep")).unwrap();
    let path = root.path().join("nested/deep/live.txt");
    std::fs::write(&path, "before").unwrap();
    let (card, mut runtime) = opened(root.path(), "nested/deep/live.txt").await;
    std::fs::remove_file(&path).unwrap();
    while !matches!(runtime.document, DocumentState::Failed { .. }) {
        change(card, &mut runtime).await;
    }
    std::fs::write(&path, "recreated").unwrap();
    text(card, &mut runtime, "recreated").await;
    std::fs::rename(root.path().join("nested"), root.path().join("moved")).unwrap();
    while !matches!(runtime.document, DocumentState::Failed { .. }) {
        change(card, &mut runtime).await;
    }
    std::fs::create_dir_all(root.path().join("nested/deep")).unwrap();
    std::fs::write(&path, "new parent").unwrap();
    text(card, &mut runtime, "new parent").await;
    std::fs::write(&path, "still live").unwrap();
    text(card, &mut runtime, "still live").await;
}
#[tokio::test]
async fn native_background_tabs_symlink_target_and_close_reopen() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("target")).unwrap();
    std::fs::write(root.path().join("target/a.txt"), "A").unwrap();
    std::fs::write(root.path().join("b.txt"), "B").unwrap();
    std::os::unix::fs::symlink("target/a.txt", root.path().join("link.txt")).unwrap();
    let (a, mut first) = opened(root.path(), "link.txt").await;
    let (b, mut second) = opened(root.path(), "b.txt").await;
    std::fs::write(root.path().join("target/a.txt"), "AA").unwrap();
    std::fs::write(root.path().join("b.txt"), "BB").unwrap();
    text(a, &mut first, "AA").await;
    text(b, &mut second, "BB").await;
    let departed = first.document_watch.as_ref().unwrap().id();
    drop(first);
    let (a, mut first) = opened(root.path(), "link.txt").await;
    assert_ne!(departed, first.document_watch.as_ref().unwrap().id());
    let ignored = first.update(RepositoryAction::DocumentChanged {
        card: a,
        watch: departed,
    });
    assert!(iced::runtime::task::into_stream(ignored).is_none());
    std::fs::write(root.path().join("target/a.txt"), "AAA").unwrap();
    text(a, &mut first, "AAA").await;
}
#[tokio::test]
async fn dirty_buffer_survives_change_deletion_and_explicit_reload() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("live.txt");
    std::fs::write(&path, "disk").unwrap();
    let (card, mut runtime) = opened(root.path(), "live.txt").await;
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.replace_text("my unsaved edits");
    }
    std::fs::write(&path, "external").unwrap();
    while !runtime.has_save_conflict() {
        change(card, &mut runtime).await;
    }
    assert_eq!(runtime.editor_text().as_deref(), Some("my unsaved edits"));
    std::fs::remove_file(&path).unwrap();
    change(card, &mut runtime).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("my unsaved edits"));
    assert!(runtime.has_save_conflict());
    std::fs::write(&path, "replacement").unwrap();
    let task = runtime.update(RepositoryAction::Reload { card });
    settle(&mut runtime, task).await;
    text(card, &mut runtime, "replacement").await;
    assert!(!runtime.is_dirty());
    assert!(!runtime.has_save_conflict());
}
#[tokio::test]
async fn background_read_cannot_overwrite_edits_or_newer_selection() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "disk A").unwrap();
    std::fs::write(root.path().join("b.txt"), "disk B").unwrap();
    let (card, mut runtime) = opened(root.path(), "a.txt").await;
    std::fs::write(root.path().join("a.txt"), "new A").unwrap();
    let identity = runtime.current_identity(card, "a.txt".into());
    let mut read = crate::editor::read_file_at(
        root.path(),
        Path::new("a.txt"),
        crate::editor::PdfTone::Original,
        runtime.original_editor_root.clone(),
    )
    .unwrap();
    read.watch_revision = Some(runtime.document_watch.as_ref().unwrap().revision());
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.replace_text("typed while loading");
    }
    drop(runtime.update(RepositoryAction::FileLoadedAt {
        identity: identity.clone(),
        result: Ok(read),
    }));
    assert_eq!(
        runtime.editor_text().as_deref(),
        Some("typed while loading")
    );
    assert!(runtime.has_save_conflict());
    let task = runtime.open_file(card, "b.txt".into());
    settle(&mut runtime, task).await;
    let old = crate::editor::read_file_at(
        root.path(),
        Path::new("a.txt"),
        crate::editor::PdfTone::Original,
        None,
    )
    .unwrap();
    drop(runtime.update(RepositoryAction::FileLoadedAt {
        identity,
        result: Ok(old),
    }));
    assert_eq!(runtime.editor_text().as_deref(), Some("disk B"));
}
#[tokio::test]
async fn clean_reload_retains_caret_scroll_and_preview_mode() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("live.md");
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    let (card, mut runtime) = opened(root.path(), "live.md").await;
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.content.set_cursor(text_editor::Cursor {
            position: text_editor::Position { line: 2, column: 3 },
            selection: None,
        });
    }
    runtime.markdown_mode = MarkdownMode::Preview;
    std::fs::write(&path, "ONE\nTWO\nTHREE\nFOUR\n").unwrap();
    text(card, &mut runtime, "ONE\nTWO\nTHREE\nFOUR\n").await;
    assert_eq!(runtime.markdown_mode, MarkdownMode::Preview);
    if let DocumentState::Editable(document) = &runtime.document {
        assert_eq!(
            document.content.cursor().position,
            text_editor::Position { line: 2, column: 3 }
        );
    } else {
        panic!("text document lost");
    }
}
