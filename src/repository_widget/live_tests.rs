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
    assert!(!first.document_reading);
    assert!(!first.document_reload_pending);
    settle(&mut first, ignored).await;
    assert_eq!(first.editor_text().as_deref(), Some("AA"));
    std::fs::write(root.path().join("target/a.txt"), "AAA").unwrap();
    text(a, &mut first, "AAA").await;
}
#[tokio::test]
async fn dirty_buffer_survives_change_deletion_and_explicit_reload() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("live.txt");
    std::fs::write(&path, "disk").unwrap();
    let (card, mut runtime) = opened(root.path(), "live.txt").await;
    for action in [
        RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::SelectAll,
        },
        RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                "my unsaved edits".into(),
            ))),
        },
        RepositoryAction::EditorAction {
            card,
            action: text_editor::Action::SelectAll,
        },
        RepositoryAction::OpenFind { card },
        RepositoryAction::FindQueryChanged {
            card,
            query: "unsaved".into(),
        },
        RepositoryAction::FindSelectionChanged {
            card,
            enabled: true,
        },
        RepositoryAction::FindInputFocus {
            card,
            focused: true,
        },
    ] {
        let task = runtime.update(action);
        settle(&mut runtime, task).await;
    }
    assert_eq!(runtime.find_total, 1);
    let scope = runtime.find_selection;
    let candidate = runtime.find_selection_candidate;
    let cursor = runtime.cursor_position();

    std::fs::write(&path, "external").unwrap();
    while !runtime.has_save_conflict() {
        change(card, &mut runtime).await;
    }
    assert_eq!(runtime.editor_text().as_deref(), Some("my unsaved edits"));
    std::fs::remove_file(&path).unwrap();
    change(card, &mut runtime).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("my unsaved edits"));
    assert!(runtime.has_save_conflict());
    // The actual asynchronous read fails while the same native watch remains
    // responsible for later recreation. Reload is a proposed replacement.
    let task = runtime.update(RepositoryAction::Reload { card });
    settle(&mut runtime, task).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("my unsaved edits"));
    assert!(runtime.is_dirty() && runtime.blocks_navigation());
    assert!(
        runtime
            .save_error()
            .unwrap()
            .contains("your edits were kept")
    );
    assert_eq!(runtime.cursor_position(), cursor);
    assert_eq!(runtime.find_query, "unsaved");
    assert_eq!(runtime.find_selection, scope);
    assert_eq!(runtime.find_selection_candidate, candidate);
    assert_eq!(runtime.find_total, 1);
    assert!(runtime.find_error.is_none());
    assert_eq!(runtime.loaded_focus_target(card), Some(find_focus_id(card)));
    assert_eq!(
        runtime.take_restored_editor_focus(card),
        Some(find_focus_id(card))
    );
    let task = runtime.update(RepositoryAction::Undo { card });
    settle(&mut runtime, task).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("disk"));
    let task = runtime.update(RepositoryAction::Redo { card });
    settle(&mut runtime, task).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("my unsaved edits"));
    std::fs::write(&path, "replacement").unwrap();
    let task = runtime.update(RepositoryAction::Reload { card });
    let mut stream = iced::runtime::task::into_stream(task).unwrap();
    let mut completed_read = None;
    while let Some(action) = stream.next().await {
        if let iced::runtime::Action::Output(action) = action {
            assert!(matches!(action, RepositoryAction::FileLoadedAt { .. }));
            assert!(completed_read.replace(action).is_none());
        }
    }
    // Remove the file after the armed read completes but before its result is
    // delivered. The obsolete watched result cannot release the predecessor.
    let completed_read = completed_read.unwrap();
    let RepositoryAction::FileLoadedAt { result, .. } = &completed_read else {
        unreachable!()
    };
    let stamped_revision = result.as_ref().unwrap().watch_revision.unwrap();
    std::fs::remove_file(&path).unwrap();
    let watch = runtime.document_watch.as_ref().unwrap().clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        while watch.revision() == stamped_revision {
            watch.receiver().recv().await.unwrap();
        }
    })
    .await
    .unwrap();
    let task = runtime.update(completed_read);
    assert!(matches!(&runtime.document,
        DocumentState::Loading { predecessor: Some(document) }
            if document.content.text() == "my unsaved edits"));
    settle(&mut runtime, task).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("my unsaved edits"));
    assert!(!runtime.original_read_attested);
    assert!(
        runtime
            .save_error()
            .unwrap()
            .contains("your edits were kept")
    );
    assert!(
        watch.error().is_none(),
        "native registration survives rollback"
    );
    std::fs::write(&path, "replacement").unwrap();
    let task = runtime.update(RepositoryAction::Reload { card });
    settle(&mut runtime, task).await;
    text(card, &mut runtime, "replacement").await;
    assert!(!runtime.is_dirty());
    assert!(!runtime.has_save_conflict());
    std::fs::write(&path, "live after recovery").unwrap();
    text(card, &mut runtime, "live after recovery").await;
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
    assert!(
        runtime.take_restored_editor_focus(card).is_none(),
        "external changes must not steal keyboard focus"
    );
    if let DocumentState::Editable(document) = &runtime.document {
        assert!(
            !document.markdown_items.is_empty(),
            "selected preview must be reparsed"
        );
        assert!(document.preview_text.contains("THREE"));
        assert_eq!(
            document.content.cursor().position,
            text_editor::Position { line: 2, column: 3 }
        );
    } else {
        panic!("text document lost");
    }
}

#[tokio::test]
async fn native_alias_parent_deduplicates_and_survives_other_owner_close() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("real/checkout")).unwrap();
    std::os::unix::fs::symlink("real", root.path().join("alias")).unwrap();
    let real = root.path().join("real/checkout");
    let alias = root.path().join("alias/checkout");
    std::fs::write(real.join("live.txt"), "first").unwrap();
    let (a, mut first) = opened(&real, "live.txt").await;
    let (b, mut second) = opened(&alias, "live.txt").await;
    #[cfg(target_os = "linux")]
    assert_eq!(
        native_watch_count(&real),
        1,
        "two aliases must share one active native watch"
    );
    std::fs::write(real.join("live.txt"), "both updated").unwrap();
    text(a, &mut first, "both updated").await;
    text(b, &mut second, "both updated").await;
    drop(second);
    // Command ordering makes arm an acknowledgement of preceding unwatch.
    first
        .document_watch
        .as_ref()
        .unwrap()
        .arm(vec![real.join("live.txt")]);
    std::fs::write(real.join("live.txt"), "still watched").unwrap();
    text(a, &mut first, "still watched").await;
    drop(first);
    let barrier_root = tempfile::tempdir().unwrap();
    let barrier = DocumentWatch::new();
    barrier.arm(vec![barrier_root.path().join("barrier.txt")]);
    #[cfg(target_os = "linux")]
    assert_eq!(
        native_watch_count(&real),
        0,
        "last document close must release its native watch"
    );
}
#[tokio::test]
async fn native_rapid_parent_recreation_and_atomic_write_bursts_converge() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("nested/deep")).unwrap();
    let path = root.path().join("nested/deep/live.txt");
    std::fs::write(&path, "first").unwrap();
    let (card, mut runtime) = opened(root.path(), "nested/deep/live.txt").await;
    std::fs::rename(root.path().join("nested"), root.path().join("moved")).unwrap();
    std::fs::create_dir_all(root.path().join("nested/deep")).unwrap();
    std::fs::write(&path, "immediate").unwrap();
    text(card, &mut runtime, "immediate").await;
    for n in 0..64 {
        std::fs::write(path.with_extension("tmp"), format!("marker {n:02}")).unwrap();
        std::fs::rename(path.with_extension("tmp"), &path).unwrap();
    }
    text(card, &mut runtime, "marker 63").await;
    std::fs::write(&path, "independent later write").unwrap();
    text(card, &mut runtime, "independent later write").await;
}
#[tokio::test]
async fn clean_reload_invalidates_revision_zero_find_cache() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("live.txt");
    std::fs::write(&path, "needle needle").unwrap();
    let (card, mut runtime) = opened(root.path(), "live.txt").await;
    let task = runtime.update(RepositoryAction::OpenFind { card });
    settle(&mut runtime, task).await;
    let task = runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "needle".into(),
    });
    settle(&mut runtime, task).await;
    assert_eq!(runtime.find_total, 2);
    let old_key = runtime.find_cache.clone();
    std::fs::write(&path, "needle erased").unwrap();
    text(card, &mut runtime, "needle erased").await;
    assert!(old_key != runtime.find_cache);
    assert!(runtime.find_is_open());
    assert_eq!(runtime.find_query, "needle");
    assert_eq!(runtime.find_total, 1);
    assert!(runtime.take_restored_editor_focus(card).is_none());

    let task = runtime.update(RepositoryAction::CloseFind { card });
    settle(&mut runtime, task).await;
    let task = runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::SelectAll,
    });
    settle(&mut runtime, task).await;
    let task = runtime.update(RepositoryAction::OpenFind { card });
    settle(&mut runtime, task).await;
    let task = runtime.update(RepositoryAction::FindQueryChanged {
        card,
        query: "needle".into(),
    });
    settle(&mut runtime, task).await;
    let task = runtime.update(RepositoryAction::FindSelectionChanged {
        card,
        enabled: true,
    });
    settle(&mut runtime, task).await;
    assert_eq!(runtime.find_total, 1);
    assert!(runtime.find_selection_candidate.is_some());
    std::fs::write(&path, "erased needle").unwrap();
    text(card, &mut runtime, "erased needle").await;
    assert_eq!(runtime.find_total, 0);
    assert!(runtime.find_selection_candidate.is_none());
    assert!(runtime.find_selection.is_some());
    assert!(
        runtime
            .find_error
            .as_deref()
            .unwrap()
            .contains("captured selection changed")
    );
    let task = runtime.update(RepositoryAction::ReplaceQueryChanged {
        card,
        query: "changed".into(),
    });
    settle(&mut runtime, task).await;
    let task = runtime.update(RepositoryAction::ReplaceMatch { card, all: true });
    settle(&mut runtime, task).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("erased needle"));
    let task = runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::SelectAll,
    });
    settle(&mut runtime, task).await;
    for enabled in [false, true] {
        let task = runtime.update(RepositoryAction::FindSelectionChanged { card, enabled });
        settle(&mut runtime, task).await;
    }
    assert_eq!(runtime.find_total, 1);
    assert!(runtime.find_error.is_none());
}

#[test]
fn native_registration_failure_is_visible_and_explicit_retry_restores_delivery() {
    const CHILD: &str = "CAGENTS_DOCUMENT_WATCH_LIMIT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "repository_widget::live_tests::native_registration_failure_is_visible_and_explicit_retry_restores_delivery", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("live.txt");
    std::fs::write(&path, "before").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let watch = DocumentWatch::new();
    // Only this disposable child changes its own limit. Exhaust real native
    // descriptors, then release them and explicitly rearm the same watch.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    limit.rlim_cur = limit.rlim_cur.min(64);
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let mut held = Vec::new();
    while let Ok(file) = std::fs::File::open("/dev/null") {
        held.push(file);
    }
    watch.arm(vec![path.clone()]);
    assert!(
        watch.error().is_some(),
        "native registration failure was hidden"
    );
    drop(held);
    watch.arm(vec![path.clone()]);
    assert!(
        watch.error().is_none(),
        "explicit native rearm failed: {:?}",
        watch.error()
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        native_watch_count(root.path()),
        1,
        "recovery must install a real native watch"
    );
    while watch.receiver().try_recv().is_ok() {}
    std::fs::write(&path, "after!").unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), watch.receiver().recv())
            .await
            .expect("recovered real native watch did not deliver")
            .unwrap();
    });
}

#[cfg(target_os = "linux")]
fn native_watch_count(path: &Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let inode = path.metadata().unwrap().ino();
    std::fs::read_dir("/proc/self/fdinfo")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .flat_map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
        .filter(|line| {
            line.starts_with("inotify wd:")
                && line.split_whitespace().any(|part| {
                    part.strip_prefix("ino:")
                        .and_then(|value| u64::from_str_radix(value, 16).ok())
                        == Some(inode)
                })
        })
        .count()
}
