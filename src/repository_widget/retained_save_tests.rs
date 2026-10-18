//! Own the real lazy Runtime task boundary; synthetic FileLoaded is not attestation.
use super::*;
use cosmic::iced::futures::StreamExt;

/// Re-executes exactly one owning test with private filesystem identity. No
/// environment variable is changed in the parallel parent test process.
pub(crate) fn isolated_test(name: &str) -> bool {
    const MARKER: &str = "CAGENTS_ROOTED_SAVE_TEST_CHILD";
    const RECEIPT: &str = "CAGENTS_ROOTED_SAVE_TEST_RECEIPT";
    if std::env::var(MARKER).as_deref() == Ok(name) {
        std::fs::write(
            std::env::var_os(RECEIPT).expect("isolated child receipt"),
            name,
        )
        .unwrap();
        return false;
    }
    let directory = tempfile::tempdir().unwrap();
    let receipt = directory.path().join("entered-test");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg(name)
        .arg("--exact")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(MARKER, name)
        .env(RECEIPT, &receipt);
    for (key, leaf) in [
        ("HOME", "home"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let path = directory.path().join(leaf);
        std::fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        command.env(key, path);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "isolated {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(receipt).unwrap(),
        name,
        "the exact owning test must have executed"
    );
    true
}

/// A checkout nested below an owned private parent, so production recovery
/// can select that parent without weakening outside-checkout placement rules.
pub(super) struct Checkout {
    _parent: tempfile::TempDir,
    source: PathBuf,
}

impl Checkout {
    pub(super) fn new() -> Self {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("checkout");
        std::fs::create_dir(&source).unwrap();
        Self {
            _parent: parent,
            source,
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.source
    }
}

async fn finish(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) {
    if let Some(mut stream) = iced::runtime::task::into_stream(task) {
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(action) = action {
                drop(runtime.update(action));
            }
        }
    }
}

async fn loaded(path: &Path) -> (RepositoryRuntime, CardId, crate::operation_recovery::Shared) {
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
        project: ProjectId::new(),
        repository: path.to_owned(),
        worktree: path.to_owned(),
        selected_file: None,
    });
    let task = runtime.update(RepositoryAction::OpenFile {
        card,
        path: "file.txt".into(),
    });
    finish(&mut runtime, task).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("baseline"));
    assert!(runtime.original_read_attested);
    let custody = crate::operation_recovery::Shared::default();
    assert!(runtime.configure_save_recovery(custody.clone(), vec![path.to_owned()]));
    (runtime, card, custody)
}

fn insert(runtime: &mut RepositoryRuntime, card: CardId, character: char) {
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert(character)),
    }));
}

#[tokio::test]
async fn unpolled_runtime_save_refuses_replaced_original_root_with_same_baseline() {
    if isolated_test(
        "repository_widget::retained_save_tests::unpolled_runtime_save_refuses_replaced_original_root_with_same_baseline",
    ) {
        return;
    }
    let parent = tempfile::tempdir().unwrap();
    let named = parent.path().join("checkout");
    let moved = parent.path().join("original");
    std::fs::create_dir(&named).unwrap();
    std::fs::write(named.join("file.txt"), "baseline").unwrap();
    let (mut runtime, card, custody) = loaded(&named).await;
    insert(&mut runtime, card, '!');
    let save = runtime.request_save(card, false);
    assert!(runtime.is_saving());
    assert!(custody.lock().unwrap().is_pending());
    std::fs::rename(&named, &moved).unwrap();
    std::fs::create_dir(&named).unwrap();
    std::fs::write(named.join("file.txt"), "baseline").unwrap();
    finish(&mut runtime, save).await;
    assert_eq!(
        std::fs::read_to_string(named.join("file.txt")).unwrap(),
        "baseline"
    );
    assert_eq!(
        std::fs::read_to_string(moved.join("file.txt")).unwrap(),
        "baseline"
    );
    assert!(runtime.is_dirty());
    assert!(!runtime.is_saving());
    assert!(runtime.save_error().is_some());
    assert!(runtime.blocks_navigation());
    assert!(!custody.lock().unwrap().is_pending());
}

#[tokio::test]
async fn runtime_save_completion_keeps_newer_buffer_dirty_and_records_written_snapshot() {
    if isolated_test(
        "repository_widget::retained_save_tests::runtime_save_completion_keeps_newer_buffer_dirty_and_records_written_snapshot",
    ) {
        return;
    }
    let directory = Checkout::new();
    std::fs::write(directory.path().join("file.txt"), "baseline").unwrap();
    let (mut runtime, card, custody) = loaded(directory.path()).await;
    insert(&mut runtime, card, '!');
    let submitted = runtime.editor_text().unwrap();
    let save = runtime.request_save(card, false);
    insert(&mut runtime, card, '?');
    let newer = runtime.editor_text().unwrap();
    finish(&mut runtime, save).await;
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).unwrap(),
        submitted
    );
    assert_eq!(runtime.editor_text().unwrap(), newer);
    assert!(runtime.is_dirty());
    assert!(!runtime.is_saving());
    assert!(runtime.save_error().is_none());
    assert!(!custody.lock().unwrap().is_pending());
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("editable document")
    };
    assert_eq!(document.saved_text, submitted);
}

#[tokio::test]
async fn dropping_unpolled_runtime_save_retires_custody_without_writing() {
    if isolated_test(
        "repository_widget::retained_save_tests::dropping_unpolled_runtime_save_retires_custody_without_writing",
    ) {
        return;
    }
    let directory = Checkout::new();
    std::fs::write(directory.path().join("file.txt"), "baseline").unwrap();
    let (mut runtime, card, custody) = loaded(directory.path()).await;
    insert(&mut runtime, card, '!');
    let save = runtime.request_save(card, false);
    assert!(custody.lock().unwrap().is_pending());
    drop(save);
    assert!(!custody.lock().unwrap().is_pending());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).unwrap(),
        "baseline"
    );
    assert!(
        runtime.is_saving(),
        "a dropped callback cannot clear the Runtime ticket"
    );
    assert!(runtime.blocks_navigation());
}

#[tokio::test]
async fn synthetic_file_loaded_cannot_borrow_an_original_root_to_admit_save() {
    if isolated_test(
        "repository_widget::retained_save_tests::synthetic_file_loaded_cannot_borrow_an_original_root_to_admit_save",
    ) {
        return;
    }
    let directory = Checkout::new();
    std::fs::write(directory.path().join("file.txt"), "baseline").unwrap();
    let (mut runtime, card, custody) = loaded(directory.path()).await;
    let identity = runtime.current_identity(card, "file.txt".into());
    drop(runtime.update(RepositoryAction::FileLoaded {
        identity,
        result: crate::editor::load_file(
            directory.path(),
            Path::new("file.txt"),
            crate::editor::PdfTone::Original,
        ),
    }));
    insert(&mut runtime, card, '!');
    assert_eq!(runtime.request_save(card, false).units(), 0);
    assert!(!custody.lock().unwrap().is_pending());
    assert!(!runtime.is_saving());
    assert!(runtime.save_error().is_some());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).unwrap(),
        "baseline"
    );
}

#[tokio::test]
async fn explicit_save_seals_typing_even_when_an_existing_save_is_in_flight() {
    if isolated_test(
        "repository_widget::retained_save_tests::explicit_save_seals_typing_even_when_an_existing_save_is_in_flight",
    ) {
        return;
    }
    let directory = Checkout::new();
    std::fs::write(directory.path().join("file.txt"), "baseline").unwrap();
    let (mut runtime, card, _) = loaded(directory.path()).await;
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.history_time = Some(Instant::now());
    }
    insert(&mut runtime, card, 'a');
    let before_save = runtime.editor_text().unwrap();
    let save = runtime.request_save_inner(card, false, false);
    assert!(runtime.is_saving());
    assert_eq!(runtime.request_save(card, false).units(), 0);
    insert(&mut runtime, card, 'b');
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(runtime.editor_text().unwrap(), before_save);
    drop(save);
}

#[tokio::test]
async fn automatic_save_does_not_seal_an_ongoing_typing_group() {
    if isolated_test(
        "repository_widget::retained_save_tests::automatic_save_does_not_seal_an_ongoing_typing_group",
    ) {
        return;
    }
    let directory = Checkout::new();
    std::fs::write(directory.path().join("file.txt"), "baseline").unwrap();
    let (mut runtime, card, _) = loaded(directory.path()).await;
    if let DocumentState::Editable(document) = &mut runtime.document {
        document.history_time = Some(Instant::now());
    }
    insert(&mut runtime, card, 'a');
    let save = runtime.request_save_inner(card, false, false);
    assert!(runtime.is_saving());
    insert(&mut runtime, card, 'b');
    drop(runtime.update(RepositoryAction::Undo { card }));
    assert_eq!(runtime.editor_text().as_deref(), Some("baseline"));
    drop(save);
}

// Existing product support: a directly owned private temporary checkout must
// remain saveable. The current ancestor-only recovery proposal is predicted
// to refuse here; keep that availability regression visible until resolved.
#[tokio::test]
async fn direct_private_temporary_checkout_remains_saveable() {
    if isolated_test(
        "repository_widget::retained_save_tests::direct_private_temporary_checkout_remains_saveable",
    ) {
        return;
    }
    for parent in ["/tmp", "/var/tmp"] {
        let directory = tempfile::tempdir_in(parent).unwrap();
        std::fs::write(directory.path().join("file.txt"), "baseline").unwrap();
        let (mut runtime, card, _) = loaded(directory.path()).await;
        runtime.set_auto_save(false);
        insert(&mut runtime, card, '!');
        let submitted = runtime.editor_text().unwrap();
        assert_ne!(submitted, "baseline");
        assert!(runtime.is_dirty());
        let save = runtime.request_save(card, false);
        finish(&mut runtime, save).await;
        assert_eq!(
            std::fs::read_to_string(directory.path().join("file.txt")).unwrap(),
            submitted,
            "a directly owned private temporary checkout must remain saveable"
        );
        assert!(!runtime.is_dirty());
        assert!(runtime.save_error().is_none());
    }
}

#[tokio::test]
async fn rejected_sticky_candidate_does_not_hide_a_safe_outside_ancestor() {
    if isolated_test(
        "repository_widget::retained_save_tests::rejected_sticky_candidate_does_not_hide_a_safe_outside_ancestor",
    ) {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let enclosing = parent.path().join("enclosing-repository");
    let mut shared = enclosing.clone();
    std::fs::create_dir(&enclosing).unwrap();
    std::fs::create_dir(enclosing.join(".git")).unwrap();
    // More sticky candidates than the bootstrap allowance must not consume
    // the preexisting protected-parent discovery allowance.
    for index in 0..10 {
        shared = shared.join(format!("shared-{index}"));
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    }
    let source = shared.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file.txt"), "baseline").unwrap();
    let (mut runtime, card, _) = loaded(&source).await;
    runtime.set_auto_save(false);
    insert(&mut runtime, card, '!');
    let submitted = runtime.editor_text().unwrap();
    assert_ne!(submitted, "baseline");
    assert!(runtime.is_dirty());
    let save = runtime.request_save(card, false);
    finish(&mut runtime, save).await;
    assert_eq!(
        std::fs::read_to_string(source.join("file.txt")).unwrap(),
        submitted
    );
    assert!(!runtime.is_dirty());
    assert!(runtime.save_error().is_none());
}
