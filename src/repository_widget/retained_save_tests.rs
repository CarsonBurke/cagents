//! Own the real lazy Runtime task boundary; synthetic FileLoaded is not attestation.
use super::*;
use crate::model::ProjectId;
use cosmic::iced::futures::StreamExt;

/// Re-executes exactly one owning test with private filesystem identity. No
/// environment variable is changed in the parallel parent test process.
pub(crate) fn isolated_test(name: &str) -> bool {
    isolated_test_inner(name, false)
}

fn isolated_test_inner(name: &str, ignored: bool) -> bool {
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
    if ignored {
        command.arg("--ignored");
    }
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
    // The retained read/root association must remain usable after the first
    // atomic replacement, and its refreshed disk hash must admit the next Save.
    let second = runtime.request_save(card, false);
    assert!(runtime.is_saving());
    finish(&mut runtime, second).await;
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).unwrap(),
        newer
    );
    assert_eq!(runtime.editor_text().unwrap(), newer);
    assert!(!runtime.is_dirty());
    assert!(runtime.save_error().is_none());
    assert!(!custody.lock().unwrap().is_pending());
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
    let save = runtime.request_save_inner(card, false, SaveTrigger::Automatic);
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
    let save = runtime.request_save_inner(card, false, SaveTrigger::Automatic);
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

// Root `/` is model scope only. Drive OpenFile directly, keep Editor surface,
// and never call initial_load/directory listing or application LSP drains.
// Mutations are confined to the caller's exclusively owned fixture names.
async fn root_scope_owned_file_round_trip(path: &Path) {
    let source = PathBuf::from("/");
    let relative = path.strip_prefix(&source).unwrap().to_owned();
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
        project: ProjectId::new(),
        repository: source.clone(),
        worktree: source.clone(),
        selected_file: None,
    });
    runtime.set_auto_save(false);
    let load = runtime.update(RepositoryAction::OpenFile {
        card,
        path: relative,
    });
    finish(&mut runtime, load).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("baseline"));
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
    assert!(!runtime.mutation_refresh_pending);
    assert!(!runtime.saves_elsewhere());
    assert!(runtime.original_read_attested);
    let custody = crate::operation_recovery::Shared::default();
    assert!(runtime.configure_save_recovery(
        custody.clone(),
        vec![source, path.parent().unwrap().to_owned()]
    ));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
    }));
    let submitted = runtime.editor_text().unwrap();
    assert_ne!(submitted, "baseline");
    assert!(runtime.is_dirty());
    let save = runtime.request_save(card, false);
    assert!(runtime.is_saving());
    let mut completions = Vec::new();
    if let Some(mut stream) = iced::runtime::task::into_stream(save) {
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(action) = action {
                if let RepositoryAction::RetainedSaveFinished {
                    ticket,
                    revision,
                    result,
                    ..
                } = &action
                {
                    // Observe the typed worker result before update flattens its
                    // error or consumes its outcome. Keep no recovery owner.
                    let result = match result {
                        Ok(saved) => format!(
                            "Ok({:?}); recovery_present={}",
                            saved.outcome(),
                            saved.recovery_authority().is_some()
                        ),
                        Err(crate::editor::retained_save::RetainedSaveError::Io(error)) => {
                            format!("Err(Io({error:?})); recovery_present=false")
                        }
                        Err(crate::editor::retained_save::RetainedSaveError::Recovery {
                            message,
                            ..
                        }) => format!("Err(Recovery({message:?})); recovery_present=true"),
                    };
                    completions.push(format!(
                        "RetainedSaveFinished ticket={ticket} revision={revision} result={result}"
                    ));
                }
                drop(runtime.update(action));
            }
        }
    }
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        submitted,
        "Save must use the actual qualified edited-file parent despite Root scope; \
         path={path:?}; completion_count={}; completions={completions:?}; \
         editor_text={:?}; dirty={}; saving={}; save_error={:?}; \
         editor_recovery={}; durability_warning={}; conflict={}; custody_pending={}",
        completions.len(),
        runtime.editor_text(),
        runtime.is_dirty(),
        runtime.is_saving(),
        runtime.save_error(),
        runtime.has_editor_recovery(),
        runtime.has_durability_warning(),
        runtime.has_save_conflict(),
        custody.lock().unwrap().is_pending()
    );
    assert!(!runtime.is_dirty());
    assert!(runtime.save_error().is_none());
    assert!(!custody.lock().unwrap().is_pending());
}

#[tokio::test]
async fn root_scope_saves_owned_private_and_sticky_parent_files() {
    if isolated_test(
        "repository_widget::retained_save_tests::root_scope_saves_owned_private_and_sticky_parent_files",
    ) {
        return;
    }
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };
    let euid = unsafe { libc::geteuid() };
    for parent in ["/tmp", "/var/tmp"] {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)
            .unwrap();
        let private_metadata = std::fs::metadata(directory.path()).unwrap();
        assert_eq!(private_metadata.uid(), euid);
        assert_eq!(private_metadata.mode() & 0o7777, 0o700);
        let inner = directory.path().join("nested-checkout");
        std::fs::create_dir(&inner).unwrap();
        // The marker exercises nested checkout ancestor cleanup leasing; no
        // Git subprocess or host repository discovery is involved.
        std::fs::create_dir(inner.join(".git")).unwrap();
        let path = inner.join("file.txt");
        std::fs::write(&path, "baseline").unwrap();
        root_scope_owned_file_round_trip(&path).await;

        // Qualify our actual edited parent without assuming public temporary
        // directory ownership. A protected ancestor may still supply recovery;
        // this asserts the owned-sticky save contract, not a bootstrap route.
        let sticky_parent = tempfile::Builder::new()
            .prefix(".cagents-save-owned-sticky-parent-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)
            .unwrap();
        let owned_metadata = std::fs::metadata(sticky_parent.path()).unwrap();
        assert_eq!(owned_metadata.uid(), euid);
        assert_eq!(owned_metadata.mode() & 0o7777, 0o700);
        std::fs::set_permissions(
            sticky_parent.path(),
            std::fs::Permissions::from_mode(0o1777),
        )
        .unwrap();
        let sticky_metadata = std::fs::metadata(sticky_parent.path()).unwrap();
        assert_eq!(sticky_metadata.uid(), euid);
        assert_eq!(sticky_metadata.mode() & 0o7777, 0o1777);
        let proof = crate::fs_safety::StickyParent::acquire(std::sync::Arc::new(
            crate::fs_safety::Root::open(sticky_parent.path()).unwrap(),
        ))
        .expect("the newly owned sticky parent must satisfy production qualification");
        drop(proof);

        let mut file = tempfile::Builder::new()
            .prefix(".cagents-save-owned-file-")
            .suffix(".txt")
            .tempfile_in(sticky_parent.path())
            .unwrap();
        file.write_all(b"baseline").unwrap();
        file.flush().unwrap();
        root_scope_owned_file_round_trip(file.path()).await;
    }
}

// Public temporary-parent ownership is an external capability. Keep its
// positive contrast distinct from mandatory owned-parent coverage. Foreign
// ownership alone is not a general save-refusal oracle: a protected ancestor
// may supply valid save-local recovery.
#[tokio::test]
#[ignore = "requires both public temporary parents to satisfy production sticky-parent qualification"]
async fn root_scope_external_temporary_parent_contrast() {
    if isolated_test_inner(
        "repository_widget::retained_save_tests::root_scope_external_temporary_parent_contrast",
        true,
    ) {
        return;
    }
    use std::io::Write;
    let parents = ["/tmp", "/var/tmp"];
    let qualifications = parents.map(|parent| {
        crate::fs_safety::Root::open(Path::new(parent))
            .and_then(|root| crate::fs_safety::StickyParent::acquire(std::sync::Arc::new(root)))
    });
    let failures: Vec<_> = parents
        .iter()
        .zip(&qualifications)
        .filter_map(|(parent, result)| {
            result.as_ref().err().map(|error| {
                format!(
                    "parent={parent}; kind={:?}; raw_errno={:?}; error={error}",
                    error.kind(),
                    error.raw_os_error()
                )
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "ROOT_SCOPE_EXTERNAL_STICKY_UNQUALIFIED: {failures:?}"
    );
    drop(qualifications);
    // Both parents qualified before creating either contrast file. Keep only
    // the ordinary fixture handles during the unchanged real save helper.
    for parent in parents {
        let mut file = tempfile::Builder::new()
            .prefix(".cagents-save-owned-file-")
            .suffix(".txt")
            .tempfile_in(parent)
            .unwrap();
        file.write_all(b"baseline").unwrap();
        file.flush().unwrap();
        root_scope_owned_file_round_trip(file.path()).await;
    }
}

#[tokio::test]
async fn root_scope_can_publish_to_a_write_search_only_parent() {
    if isolated_test(
        "repository_widget::retained_save_tests::root_scope_can_publish_to_a_write_search_only_parent",
    ) {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let owner = tempfile::tempdir_in("/tmp").unwrap();
    let parent = owner.path().join("write-search-only");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("file.txt");
    std::fs::write(&path, "baseline").unwrap();
    // Always restore this owned fixture directory before TempDir cleanup,
    // including when an assertion or the actual worker panics.
    struct RestoreReadAccess(PathBuf);
    impl Drop for RestoreReadAccess {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    let restore_read_access = RestoreReadAccess(parent.clone());
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o300)).unwrap();
    let source = PathBuf::from("/");
    let card = CardId::new();
    let mut runtime = RepositoryRuntime::new_directory(RepositoryView {
        project: ProjectId::new(),
        repository: source.clone(),
        worktree: source.clone(),
        selected_file: None,
    });
    runtime.set_auto_save(false);
    let load = runtime.update(RepositoryAction::OpenFile {
        card,
        path: path.strip_prefix(&source).unwrap().to_owned(),
    });
    finish(&mut runtime, load).await;
    assert_eq!(runtime.editor_text().as_deref(), Some("baseline"));
    assert_eq!(runtime.source_surface, SourceSurface::Editor);
    assert!(!runtime.mutation_refresh_pending);
    assert!(!runtime.saves_elsewhere());
    let custody = crate::operation_recovery::Shared::default();
    assert!(runtime.configure_save_recovery(custody.clone(), vec![source]));
    drop(runtime.update(RepositoryAction::EditorAction {
        card,
        action: text_editor::Action::Edit(text_editor::Edit::Insert('!')),
    }));
    let submitted = runtime.editor_text().unwrap();
    assert_ne!(submitted, "baseline");
    assert!(runtime.is_dirty());
    let save = runtime.request_save(card, false);
    let mut completion = None;
    if let Some(mut stream) = iced::runtime::task::into_stream(save) {
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(action) = action {
                if let RepositoryAction::RetainedSaveFinished { result, .. } = &action {
                    assert!(completion.replace(result.clone()).is_none());
                }
                drop(runtime.update(action));
            }
        }
    }
    // Restore before inspecting the result; the guard also covers unwinding.
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    drop(restore_read_access);
    let saved = completion
        .expect("actual Save completion")
        .expect("publication succeeds");
    let SaveOutcome::Saved {
        content_hash,
        durability,
    } = saved.outcome()
    else {
        panic!("write/search permission must permit publication");
    };
    assert_eq!(*content_hash, blake3::hash(submitted.as_bytes()));
    let warned = match durability {
        SaveDurability::Durable => false,
        SaveDurability::Unconfirmed { error } => {
            assert!(!error.is_empty());
            true
        }
    };
    // Read-restricted directory sync may be Unconfirmed. That is a warning
    // about a successful publication, not a failed Save or dirty buffer.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), submitted);
    assert!(!runtime.is_dirty());
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("editable after Save");
    };
    assert!(document.error.is_none());
    assert!(document.active_save.is_none());
    assert_eq!(document.saved_text, submitted);
    assert_eq!(document.durability_warning.is_some(), warned);
    assert_eq!(runtime.has_durability_warning(), warned);
    assert_eq!(runtime.save_error().is_some(), warned);
    assert_eq!(runtime.blocks_navigation(), warned);
    assert!(saved.recovery_authority().is_none());
    assert!(document.cleanup_recovery.is_none());
    assert!(!runtime.has_editor_recovery());
    assert!(!custody.lock().unwrap().is_pending());
    drop(runtime.reconcile_operation_recovery(card));
    assert!(!custody.lock().unwrap().is_pending());
    assert_eq!(runtime.has_durability_warning(), warned);
}

#[tokio::test]
async fn resolved_cleanup_keeps_warning_until_a_real_save_qualifies_the_baseline() {
    if isolated_test(
        "repository_widget::retained_save_tests::resolved_cleanup_keeps_warning_until_a_real_save_qualifies_the_baseline",
    ) {
        return;
    }
    let directory = Checkout::new();
    std::fs::write(directory.path().join("file.txt"), "baseline").unwrap();
    let (mut runtime, card, custody) = loaded(directory.path()).await;
    runtime.set_auto_save(false);
    insert(&mut runtime, card, '!');
    let submitted = runtime.editor_text().unwrap();
    let extra = Arc::new(std::sync::Mutex::new(None));
    let captured = extra.clone();
    // The normal Runtime worker still qualifies the original baseline,
    // publishes, finishes context/custody and delivers its typed completion.
    let save = runtime.request_save_with_writer(
        card,
        false,
        true,
        move |root, path, text, hash, overwrite, context| {
            crate::editor::retained_save::save_file_with_cleanup_warning_at(
                root,
                path,
                text,
                hash,
                overwrite,
                context,
                |_, holding| {
                    let (file, name) = crate::fs_safety::create_temporary_in(holding, 0o600)
                        .map_err(std::io::Error::other)?;
                    *captured.lock().unwrap() = Some((holding.try_clone()?, name, file));
                    Ok(())
                },
            )
        },
    );
    assert!(runtime.is_saving());
    finish(&mut runtime, save).await;
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).unwrap(),
        submitted
    );
    assert!(!runtime.is_dirty());
    assert!(
        runtime
            .save_error()
            .unwrap()
            .contains("injected source-parent directory sync failure")
    );
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("editable");
    };
    let authority = document
        .cleanup_recovery
        .clone()
        .expect("producer cleanup authority");
    assert!(!authority.is_resolved());
    assert!(custody.lock().unwrap().is_pending());
    let (holding, name, file) = extra.lock().unwrap().take().expect("owned extra entry");
    drop(file);
    crate::fs_safety::unlink_within(&holding, &name).unwrap();
    let retry = custody
        .lock()
        .unwrap()
        .update(crate::operation_recovery::Action::Retry(authority.clone()));
    let mut stream = iced::runtime::task::into_stream(retry).expect("actual Retry worker");
    let finished = loop {
        match stream.next().await.expect("actual Finished output") {
            iced::runtime::Action::Output(action) => break action,
            _ => {}
        }
    };
    assert!(matches!(
        &finished,
        crate::operation_recovery::Action::Finished { result: Ok(()), .. }
    ));
    // Hold the actual worker completion before State/App reconciliation: the
    // shared authority has resolved, while the document still retains its marker.
    assert!(authority.is_resolved());
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("editable");
    };
    assert!(document.cleanup_recovery.as_ref().unwrap().is_resolved());
    assert!(
        runtime.has_durability_warning(),
        "resolved cleanup hid the Save durability warning before Finished routing"
    );
    assert!(runtime.blocks_navigation());
    assert!(runtime.can_save());
    drop(custody.lock().unwrap().update(finished));
    drop(runtime.reconcile_operation_recovery(card));
    assert!(runtime.has_durability_warning());
    assert!(runtime.blocks_navigation());
    assert!(runtime.can_save());
    let DocumentState::Editable(document) = &runtime.document else {
        panic!("editable");
    };
    assert!(document.cleanup_recovery.is_none());
    // A real subsequent Save, including regular-file and directory sync,
    // owns the transition back to a qualified baseline.
    let retry = runtime.request_save(card, false);
    assert!(runtime.is_saving());
    finish(&mut runtime, retry).await;
    assert_eq!(
        std::fs::read_to_string(directory.path().join("file.txt")).unwrap(),
        submitted
    );
    assert!(!runtime.has_durability_warning());
    assert!(!runtime.is_dirty());
    assert!(runtime.save_error().is_none());
}
