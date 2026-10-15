//! Durable setup choices, independent of execution or UI state.

use super::*;

/// An exact receipt's execution lease. Retain across file preparation and its
/// progress CAS, then drop before requesting host terminal startup.
pub struct WorktreeSetupGuard<'a> {
    store: &'a StateStore,
    creation_id: OperationId,
    entity_id: String,
    generation: u64,
    lease: fs::File,
}

/// Exact configuration reviewed for one creation. Execution still reloads and
/// verifies this snapshot through `worktree_setup` before preparing commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSetupPlan {
    pub copy: Vec<String>,
    pub commands: Vec<String>,
    pub fingerprint: String,
}

impl WorktreeSetupPlan {
    pub fn requires_trust(&self) -> bool {
        !self.commands.is_empty()
    }
}

/// Retained intent is evidence for explicit recovery, never permission to
/// replay shell effects automatically after an uncertain result or restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeSetupIntent {
    pub primary: Option<PathBuf>,
    pub plan: Option<WorktreeSetupPlan>,
    pub skip: bool,
    pub run_id: Option<OperationId>,
    pub copy_done: bool,
}

/// Administrative identity recorded when a creation settled. Paths alone do
/// not authorize resuming setup in a replacement checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSetupCreationIdentity {
    pub creation_id: OperationId,
    pub common_dir: PathBuf,
    pub admin_dir: Option<PathBuf>,
}

const CREATION_MARKER: &str = "cagents-setup-creation";

fn read_creation_marker(admin: &crate::fs_safety::Root) -> io::Result<Option<OperationId>> {
    let mut file = match crate::fs_safety::open_beneath(admin, Path::new(CREATION_MARKER)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(
            "Invalid worktree creation identity marker",
        ));
    }
    let bytes = crate::fs_safety::read_bounded(&mut file, 64)?;
    if bytes.len() != 37 || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other(
            "Invalid bounded canonical worktree creation marker",
        ));
    }
    let value = std::str::from_utf8(&bytes[..36]).map_err(io::Error::other)?;
    let creation: OperationId = value.parse().map_err(io::Error::other)?;
    if creation.to_string().as_bytes() != &bytes[..36] {
        return Err(io::Error::other("Noncanonical worktree creation marker"));
    }
    Ok(Some(creation))
}

/// Verify a recorded generation without a database lookup, mutation or Git.
/// The caller separately proves the current registered common/admin identity.
pub fn worktree_setup_marker_matches(admin: &Path, creation: OperationId) -> io::Result<bool> {
    crate::secure_file::reject_symlink_components(admin)?;
    let admin = crate::fs_safety::Root::open(admin)?;
    Ok(read_creation_marker(&admin)? == Some(creation))
}

impl WorktreeSetupCreationIdentity {
    /// A marker belonging to the exact original creation survives a move but
    /// disappears when Git removes its admin directory, even if its path or
    /// filesystem inode number is later reused. Never infer it for legacy rows.
    pub fn matches_checkout(&self, common: &Path, admin: &Path) -> io::Result<bool> {
        if self.common_dir != common || self.admin_dir.as_deref() != Some(admin) {
            return Ok(false);
        }
        worktree_setup_marker_matches(admin, self.creation_id)
    }
}

pub(super) fn named_directory_matches(root: &crate::fs_safety::Root) -> bool {
    use std::os::unix::fs::MetadataExt;
    root.metadata()
        .ok()
        .zip(fs::symlink_metadata(root.path()).ok())
        .is_some_and(|(held, named)| {
            named.is_dir() && held.dev() == named.dev() && held.ino() == named.ino()
        })
}

/// Publish through the already-held original admin descriptor, only while
/// the operation's Git lock and exclusive lease prove creation ownership.
/// Never initialize or repair a marker from a setup resume callback.
pub(super) fn record_creation_identity(
    admin: &crate::fs_safety::Root,
    creation: OperationId,
) -> io::Result<()> {
    use std::io::Write;
    match read_creation_marker(admin)? {
        Some(existing) if existing == creation => return Ok(()),
        Some(_) => {
            return Err(io::Error::other(
                "Worktree creation identity belongs to another generation",
            ));
        }
        None => {}
    }
    let (mut file, temporary) =
        crate::fs_safety::create_temporary_in(admin, 0o600).map_err(io::Error::other)?;
    let result = (|| {
        writeln!(file, "{creation}")?;
        file.sync_all()?;
        match crate::fs_safety::rename_new_within(
            admin,
            &temporary,
            std::ffi::OsStr::new(CREATION_MARKER),
        ) {
            Ok(()) => {}
            Err(error)
                if error.is_already_exists() && read_creation_marker(admin)? == Some(creation) => {}
            Err(error) => return Err(io::Error::other(error)),
        }
        crate::fs_safety::sync_directory(admin)
    })();
    let _ = crate::fs_safety::unlink_within(admin, &temporary);
    result
}

fn settled_setup_journal(operation: &PendingOperation) -> Result<WorktreeJournal, StoreError> {
    let creation_id = operation.id;
    let invalid = || {
        StoreError::InvalidState(format!(
            "worktree creation {creation_id} is not a valid settled creation"
        ))
    };
    let journal: WorktreeJournal = serde_json::from_value(operation.payload.clone())?;
    let paths = [
        &journal.common_dir,
        &journal.request.managed_root,
        &journal.request.destination,
    ];
    if operation.status != PendingOperationStatus::Applied
        || operation.entity_kind != "worktree_creation"
        || operation.action != "create"
        || journal.version != 1
        || journal.state != WorktreeOperationState::Created
        || !matches!(
            &journal.outcome,
            Some(WorktreeCreationOutcome::Created { .. })
        )
        || journal.request.operation_id != creation_id
        || journal.generation != operation.base_generation
        || operation.entity_id != journal_identity(&journal.common_dir, &journal.request)
        || journal.lease_path
            != journal
                .common_dir
                .join(format!("cagents-operation-{creation_id}.lease"))
        || !destination_within_managed_root(
            &journal.request.destination,
            &journal.request.managed_root,
        )
        || paths.iter().any(|path| {
            !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
        })
        || journal.base_oid.as_ref().is_none_or(|oid| !is_hex_oid(oid))
        || journal
            .existing_branch_oid
            .as_ref()
            .is_some_and(|oid| !is_hex_oid(oid))
        || !journal.effects_started
        || ValidatedBranchName::parse(journal.request.branch.as_str()).is_err()
        || journal.request.resolved_base_reference().is_err()
    {
        return Err(invalid());
    }
    Ok(journal)
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Read one settled creation without reconciling or executing effects.
    /// Missing rows return None; unsupported or identity-mismatched rows fail
    /// closed. Callers still verify current checkout identity before resuming.
    pub fn worktree_setup_creation(
        &self,
        store: &StateStore,
        creation_id: OperationId,
    ) -> Result<Option<WorktreeOperationReport>, StoreError> {
        let Some(operation) = store.journaled_worktree_operation(creation_id)? else {
            return Ok(None);
        };
        let journal = settled_setup_journal(&operation)?;
        Ok(Some(journal.report(operation.created_at_unix_ms)))
    }

    /// Read recorded identity without Git, reconciliation or mutation. A
    /// legacy missing admin directory remains explicit for callers to refuse.
    pub fn worktree_setup_creation_identity(
        &self,
        store: &StateStore,
        creation_id: OperationId,
    ) -> Result<Option<WorktreeSetupCreationIdentity>, StoreError> {
        let Some(operation) = store.journaled_worktree_operation(creation_id)? else {
            return Ok(None);
        };
        let journal = settled_setup_journal(&operation)?;
        Ok(Some(WorktreeSetupCreationIdentity {
            creation_id,
            common_dir: journal.common_dir,
            admin_dir: journal.admin_dir,
        }))
    }

    /// Update metadata on a settled creation without reconciling or executing
    /// any filesystem, Git or shell effects. The expected intent also rejects
    /// callbacks from an older setup run after an explicit resume.
    pub fn update_worktree_setup_intent(
        &self,
        store: &StateStore,
        creation_id: OperationId,
        expected: Option<&WorktreeSetupIntent>,
        updated: WorktreeSetupIntent,
    ) -> Result<WorktreeOperationReport, StoreError> {
        let operation = store
            .journaled_worktree_operation(creation_id)?
            .ok_or_else(|| {
                StoreError::InvalidState(format!("worktree creation {creation_id} is missing"))
            })?;
        let mut journal = settled_setup_journal(&operation)?;
        if journal.request.setup_intent.as_ref() != expected {
            return Err(StoreError::InvalidState(format!(
                "stale setup intent for worktree creation {creation_id}"
            )));
        }
        journal.request.setup_intent = Some(updated);
        journal.persist(store, &operation.payload)?;
        Ok(journal.report(operation.created_at_unix_ms))
    }

    /// Serialize production setup transitions across copied window journals
    /// and the host's actual setup execution, before taking a SQLite writer.
    pub fn update_worktree_setup_intent_guarded(
        &self,
        store: &StateStore,
        creation_id: OperationId,
        expected: Option<&WorktreeSetupIntent>,
        updated: WorktreeSetupIntent,
    ) -> Result<WorktreeOperationReport, StoreError> {
        let guard = self.worktree_setup_guard(store, creation_id, expected)?;
        self.update_worktree_setup_intent_with_guard(store, creation_id, expected, updated, &guard)
    }

    pub fn worktree_setup_guard<'a>(
        &self,
        store: &'a StateStore,
        creation_id: OperationId,
        expected: Option<&WorktreeSetupIntent>,
    ) -> Result<WorktreeSetupGuard<'a>, StoreError> {
        let initial = store
            .journaled_worktree_operation(creation_id)?
            .ok_or_else(|| StoreError::InvalidState("Setup creation receipt is missing".into()))?;
        let lease = self.worktree_setup_execution_lease(&initial)?;
        // Copies of a known creator journal are not independent authority.
        // Refuse ambiguous owners rather than preparing from divergent intent.
        let mut paths = crate::instance::window_database_paths()?;
        if let Some(directories) = crate::instance::project_dirs() {
            paths.push(directories.data_dir().join("worktree-operations.sqlite3"));
        }
        paths.sort();
        paths.dedup();
        let mut owners = Vec::new();
        for path in paths {
            if path.try_exists()?
                && crate::persistence::read_worktree_operation_at(&path, creation_id)?.is_some()
            {
                owners.push(path);
            }
        }
        if owners.len() > 1
            || (owners.len() == 1
                && owners[0].canonicalize()? != store.database_path().canonicalize()?)
        {
            return Err(StoreError::InvalidState("Multiple window journals retain this setup creation; its unique original authority is unavailable".into()));
        }
        let current = store
            .journaled_worktree_operation(creation_id)?
            .ok_or_else(|| StoreError::InvalidState("Setup creation disappeared".into()))?;
        let journal = settled_setup_journal(&current)?;
        if current.entity_id != initial.entity_id
            || current.base_generation != initial.base_generation
        {
            return Err(StoreError::InvalidState(
                "Setup receipt identity changed".into(),
            ));
        }
        if journal.request.setup_intent.as_ref() != expected {
            return Err(StoreError::InvalidState(
                "Setup intent was superseded before preparation".into(),
            ));
        }
        if journal.lease_identity != Some(lease_file_identity(&lease)?) {
            return Err(StoreError::InvalidState(
                "Setup creation lease identity changed".into(),
            ));
        }
        Ok(WorktreeSetupGuard {
            store,
            creation_id,
            entity_id: current.entity_id,
            generation: current.base_generation,
            lease,
        })
    }

    pub fn update_worktree_setup_intent_with_guard(
        &self,
        store: &StateStore,
        creation_id: OperationId,
        expected: Option<&WorktreeSetupIntent>,
        updated: WorktreeSetupIntent,
        guard: &WorktreeSetupGuard<'_>,
    ) -> Result<WorktreeOperationReport, StoreError> {
        if guard.creation_id != creation_id || !std::ptr::eq(guard.store, store) {
            return Err(StoreError::InvalidState(
                "Setup preparation guard belongs to another receipt or store".into(),
            ));
        }
        let row = store
            .journaled_worktree_operation(creation_id)?
            .ok_or_else(|| StoreError::InvalidState("Setup creation disappeared".into()))?;
        if row.entity_id != guard.entity_id || row.base_generation != guard.generation {
            return Err(StoreError::InvalidState(
                "Setup preparation receipt identity changed".into(),
            ));
        }
        let journal = settled_setup_journal(&row)?;
        if journal.lease_identity != Some(lease_file_identity(&guard.lease)?) {
            return Err(StoreError::InvalidState(
                "Setup preparation lease identity changed".into(),
            ));
        }
        self.update_worktree_setup_intent(store, creation_id, expected, updated)
    }

    pub(crate) fn worktree_setup_execution_lease(
        &self,
        operation: &PendingOperation,
    ) -> Result<fs::File, StoreError> {
        use std::os::unix::fs::MetadataExt;
        let journal = settled_setup_journal(operation)?;
        let expected = journal.lease_identity.ok_or_else(|| {
            StoreError::InvalidState("Setup creation has no original lease identity".into())
        })?;
        // Do not wait on a starting/live setup after a caller's activity census.
        // A retry must obtain fresh activity and intent evidence.
        let lease=worktree_lease(&journal.lease_path,false).map_err(|error|StoreError::InvalidState(format!("Close the previous Worktree setup terminal, including its completed card, before resuming; setup ownership is still held or unavailable: {error}")))?;
        let named = fs::symlink_metadata(&journal.lease_path)?;
        if lease_file_identity(&lease)? != expected
            || !named.file_type().is_file()
            || (named.dev(), named.ino()) != expected
        {
            return Err(StoreError::InvalidState(
                "Setup creation lease was replaced".into(),
            ));
        }
        Ok(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(root: &Path) -> CreateWorktreeRequest {
        CreateWorktreeRequest {
            base_reference: None,
            repository: root.join("repo"),
            managed_root: root.join("managed"),
            destination: root.join("managed/topic"),
            branch: ValidatedBranchName::parse("topic").unwrap(),
            base: BaseRevision::object_id("a".repeat(40)).unwrap(),
            track: false,
            operation_id: OperationId::new(),
            setup_intent: None,
            linear_issue: None,
        }
    }

    fn journal(request: CreateWorktreeRequest) -> WorktreeJournal {
        let common_dir = request.repository.join(".git");
        WorktreeJournal {
            version: 1,
            basis_recording_expected: false,
            generation: 7,
            state: WorktreeOperationState::Created,
            lease_path: common_dir
                .join(format!("cagents-operation-{}.lease", request.operation_id)),
            lease_identity: None,
            effects_started: true,
            checkout_completed: true,
            base_oid: Some("a".repeat(40)),
            existing_branch_oid: None,
            admin_dir: Some(common_dir.join("worktrees/topic")),
            outcome: Some(WorktreeCreationOutcome::Created {
                worktree: WorktreeRecord {
                    id: WorktreeId::new(),
                    path: request.destination.clone(),
                    creation_basis: None,
                    created_at_unix_ms: None,
                    head_oid: Some("a".repeat(40)),
                    branch: Some(request.branch.full_ref()),
                    detached: false,
                    bare: false,
                    lock: WorktreeLock::default(),
                    prunable_reason: None,
                },
                warnings: Vec::new(),
            }),
            warnings: Vec::new(),
            detail: "Created".into(),
            request,
            common_dir,
        }
    }

    fn seed(store: &StateStore, journal: &WorktreeJournal, status: PendingOperationStatus) {
        store
            .begin_operation(&PendingOperation {
                id: journal.request.operation_id,
                entity_kind: "worktree_creation".into(),
                entity_id: journal_identity(&journal.common_dir, &journal.request),
                action: "create".into(),
                base_generation: journal.generation,
                payload: serde_json::to_value(journal).unwrap(),
                status: PendingOperationStatus::Pending,
                created_at_unix_ms: 42,
                updated_at_unix_ms: 42,
            })
            .unwrap();
        if status != PendingOperationStatus::Pending {
            store
                .resolve_operation(journal.request.operation_id, status)
                .unwrap();
        }
    }

    fn intent(request: &CreateWorktreeRequest) -> WorktreeSetupIntent {
        WorktreeSetupIntent {
            primary: Some(request.repository.clone()),
            plan: Some(WorktreeSetupPlan {
                copy: vec![".env".into()],
                commands: vec!["printf setup".into()],
                fingerprint: "b".repeat(64),
            }),
            run_id: Some(OperationId::new()),
            ..Default::default()
        }
    }

    #[test]
    fn creation_marker_is_private_idempotent_bounded_and_never_replaced() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let root = crate::fs_safety::Root::open(directory.path()).unwrap();
        let creation = OperationId::new();
        assert!(!worktree_setup_marker_matches(directory.path(), creation).unwrap());
        record_creation_identity(&root, creation).unwrap();
        record_creation_identity(&root, creation).unwrap();
        assert!(worktree_setup_marker_matches(directory.path(), creation).unwrap());
        assert_eq!(
            fs::metadata(directory.path().join(CREATION_MARKER))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(record_creation_identity(&root, OperationId::new()).is_err());
        assert!(worktree_setup_marker_matches(directory.path(), creation).unwrap());
        fs::write(directory.path().join(CREATION_MARKER), vec![b'a'; 65]).unwrap();
        assert!(worktree_setup_marker_matches(directory.path(), creation).is_err());
        fs::write(
            directory.path().join(CREATION_MARKER),
            format!("{creation}{}invalid", "\n".repeat(100)),
        )
        .unwrap();
        assert!(
            worktree_setup_marker_matches(directory.path(), creation).is_err(),
            "a truncated valid prefix is not a marker"
        );
        fs::remove_file(directory.path().join(CREATION_MARKER)).unwrap();
        symlink("/dev/null", directory.path().join(CREATION_MARKER)).unwrap();
        assert!(worktree_setup_marker_matches(directory.path(), creation).is_err());
    }

    #[test]
    fn late_marker_publication_stays_with_the_held_original_admin_directory() {
        let directory = tempfile::tempdir().unwrap();
        let admin = directory.path().join("admin");
        let moved = directory.path().join("old-admin");
        fs::create_dir(&admin).unwrap();
        let original = crate::fs_safety::Root::open(&admin).unwrap();
        fs::rename(&admin, &moved).unwrap();
        fs::create_dir(&admin).unwrap();
        let creation = OperationId::new();
        record_creation_identity(&original, creation).unwrap();
        assert!(worktree_setup_marker_matches(&moved, creation).unwrap());
        assert!(
            !worktree_setup_marker_matches(&admin, creation).unwrap(),
            "an asynchronous callback cannot initialize the replacement generation"
        );
    }

    #[test]
    fn guarded_setup_transitions_share_the_original_lease_across_copied_stores() {
        let root = tempfile::tempdir().unwrap();
        let mut request = request(root.path());
        let original = intent(&request);
        request.setup_intent = Some(original.clone());
        let mut journal = journal(request);
        fs::create_dir_all(&journal.common_dir).unwrap();
        let lease = worktree_lease(&journal.lease_path, true).unwrap();
        journal.lease_identity = Some(lease_file_identity(&lease).unwrap());
        drop(lease);
        let first = StateStore::open(
            root.path().join("first.sqlite3"),
            crate::model::BootId::new("lease-test"),
        )
        .unwrap();
        let copied = StateStore::open(
            root.path().join("copied.sqlite3"),
            crate::model::BootId::new("lease-test"),
        )
        .unwrap();
        seed(&first, &journal, PendingOperationStatus::Applied);
        seed(&copied, &journal, PendingOperationStatus::Applied);
        let service = RepositoryService::system("git");
        let id = journal.request.operation_id;
        let row = first.journaled_worktree_operation(id).unwrap().unwrap();
        let running = service
            .worktree_setup_guard(&first, id, Some(&original))
            .unwrap();
        let mut updated = original.clone();
        updated.skip = true;
        updated.run_id = Some(OperationId::new());
        assert!(
            service
                .update_worktree_setup_intent_guarded(&copied, id, Some(&original), updated.clone())
                .is_err(),
            "a copied store must not bypass a starting or live execution"
        );
        assert_eq!(
            service
                .worktree_setup_creation(&copied, id)
                .unwrap()
                .unwrap()
                .request
                .unwrap()
                .setup_intent,
            Some(original.clone())
        );
        assert!(
            service
                .update_worktree_setup_intent_with_guard(
                    &copied,
                    id,
                    Some(&original),
                    updated.clone(),
                    &running
                )
                .is_err(),
            "a preparation guard is bound to its exact store"
        );
        let mut prepared = original.clone();
        prepared.copy_done = true;
        service
            .update_worktree_setup_intent_with_guard(
                &first,
                id,
                Some(&original),
                prepared,
                &running,
            )
            .unwrap();
        drop(running);
        assert!(
            service
                .worktree_setup_guard(&first, id, Some(&original))
                .is_err(),
            "stale preparation must fail before filesystem effects"
        );
        service
            .update_worktree_setup_intent_guarded(&copied, id, Some(&original), updated)
            .unwrap();
        let old = fs::File::open(&journal.lease_path).unwrap();
        fs::rename(
            &journal.lease_path,
            journal.lease_path.with_extension("retained"),
        )
        .unwrap();
        drop(worktree_lease(&journal.lease_path, true).unwrap());
        assert!(
            service.worktree_setup_execution_lease(&row).is_err(),
            "replacement lease inode must not inherit execution authority"
        );
        drop(old);
    }

    #[test]
    fn old_requests_and_intents_deserialize_without_setup_fields() {
        let mut value = serde_json::to_value(request(Path::new("/fixture"))).unwrap();
        value.as_object_mut().unwrap().remove("setup_intent");
        assert!(!value.as_object().unwrap().contains_key("base_reference"));
        assert!(
            serde_json::from_value::<CreateWorktreeRequest>(value.clone())
                .unwrap()
                .base_reference
                .is_none()
        );
        assert!(
            serde_json::from_value::<CreateWorktreeRequest>(value)
                .unwrap()
                .setup_intent
                .is_none()
        );
        assert_eq!(
            serde_json::from_str::<WorktreeSetupIntent>("{}").unwrap(),
            WorktreeSetupIntent::default()
        );
    }

    #[test]
    fn setup_progress_survives_reopen_without_executing_or_recreating_the_worktree() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = root.path().join("state.sqlite");
        let request = request(root.path());
        let first = intent(&request);
        let mut journal = journal(request.clone());
        // Reconciliation can settle a checkout after the creating process
        // died before recording its successful command return.
        journal.checkout_completed = false;
        journal.request.setup_intent = Some(first.clone());
        let store = StateStore::open(&path, crate::model::BootId::new("setup-intent")).unwrap();
        seed(&store, &journal, PendingOperationStatus::Applied);
        let mut copied = first.clone();
        copied.copy_done = true;
        let report = RepositoryService::system("must-not-execute-git")
            .update_worktree_setup_intent(
                &store,
                request.operation_id,
                Some(&first),
                copied.clone(),
            )
            .unwrap();
        assert_eq!(report.request.unwrap().setup_intent, Some(copied.clone()));
        assert_eq!(report.state, WorktreeOperationState::Created);
        assert_eq!(report.created_at_unix_ms, 42);
        assert_eq!(report.detail, "Created");
        drop(store);
        let reopened =
            StateStore::open(path, crate::model::BootId::new("setup-intent-restart")).unwrap();
        let row = reopened
            .journaled_worktree_operation(request.operation_id)
            .unwrap()
            .unwrap();
        let saved: WorktreeJournal = serde_json::from_value(row.payload).unwrap();
        assert_eq!(saved.request.setup_intent, Some(copied.clone()));
        assert_eq!(row.status, PendingOperationStatus::Applied);
        let service = RepositoryService::system("must-not-execute-git");
        let before = reopened
            .journaled_worktree_operation(request.operation_id)
            .unwrap()
            .unwrap()
            .payload;
        let read = service
            .worktree_setup_creation(&reopened, request.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(read.request.unwrap().setup_intent, Some(copied));
        assert_eq!(read.state, WorktreeOperationState::Created);
        assert_eq!(read.created_at_unix_ms, 42);
        assert_eq!(
            reopened
                .journaled_worktree_operation(request.operation_id)
                .unwrap()
                .unwrap()
                .payload,
            before
        );
        assert!(!request.destination.exists());
    }

    #[test]
    fn settled_identity_read_preserves_admin_and_legacy_missing_evidence_without_effects() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let store = StateStore::open(
            root.path().join("state.sqlite"),
            crate::model::BootId::new("setup-identity"),
        )
        .unwrap();
        let service = RepositoryService::system("must-not-execute-git");
        let request = request(root.path());
        let journal = journal(request.clone());
        seed(&store, &journal, PendingOperationStatus::Applied);
        let before = store
            .journaled_worktree_operation(request.operation_id)
            .unwrap()
            .unwrap()
            .payload;
        assert_eq!(
            service
                .worktree_setup_creation_identity(&store, request.operation_id)
                .unwrap(),
            Some(WorktreeSetupCreationIdentity {
                creation_id: request.operation_id,
                common_dir: journal.common_dir.clone(),
                admin_dir: journal.admin_dir.clone()
            })
        );
        assert_eq!(
            store
                .journaled_worktree_operation(request.operation_id)
                .unwrap()
                .unwrap()
                .payload,
            before
        );
        let mut legacy = journal.clone();
        legacy.admin_dir = None;
        store
            .update_operation(
                request.operation_id,
                &before,
                &serde_json::to_value(&legacy).unwrap(),
                PendingOperationStatus::Applied,
            )
            .unwrap();
        assert!(
            service
                .worktree_setup_creation_identity(&store, request.operation_id)
                .unwrap()
                .unwrap()
                .admin_dir
                .is_none()
        );
        assert!(
            service
                .worktree_setup_creation_identity(&store, OperationId::new())
                .unwrap()
                .is_none()
        );
        assert!(!request.destination.exists());
    }

    #[test]
    fn stale_callbacks_cannot_overwrite_resumed_run_or_add_intent_to_unsettled_rows() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let store = StateStore::open(
            root.path().join("state.sqlite"),
            crate::model::BootId::new("setup-intent"),
        )
        .unwrap();
        let service = RepositoryService::system("must-not-execute-git");
        let request = request(root.path());
        let original = intent(&request);
        let journal = journal(request.clone());
        seed(&store, &journal, PendingOperationStatus::Applied);
        service
            .update_worktree_setup_intent(&store, request.operation_id, None, original.clone())
            .unwrap();
        let mut resumed = original.clone();
        resumed.run_id = Some(OperationId::new());
        service
            .update_worktree_setup_intent(
                &store,
                request.operation_id,
                Some(&original),
                resumed.clone(),
            )
            .unwrap();
        let mut stale = original.clone();
        stale.copy_done = true;
        assert!(
            service
                .update_worktree_setup_intent(&store, request.operation_id, Some(&original), stale)
                .unwrap_err()
                .to_string()
                .contains("stale setup intent")
        );
        let row = store
            .journaled_worktree_operation(request.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_value::<WorktreeJournal>(row.payload)
                .unwrap()
                .request
                .setup_intent,
            Some(resumed)
        );
        let mut pending = journal.clone();
        pending.request.operation_id = OperationId::new();
        pending.lease_path = pending.common_dir.join(format!(
            "cagents-operation-{}.lease",
            pending.request.operation_id
        ));
        pending.request.destination = pending.request.managed_root.join("other");
        pending.state = WorktreeOperationState::OutcomeUnknown;
        pending.outcome = None;
        seed(&store, &pending, PendingOperationStatus::Pending);
        assert!(
            service
                .update_worktree_setup_intent(&store, pending.request.operation_id, None, original)
                .is_err()
        );
    }

    #[test]
    fn malformed_settled_journals_are_not_rewritten() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let store = StateStore::open(
            root.path().join("state.sqlite"),
            crate::model::BootId::new("setup-intent"),
        )
        .unwrap();
        let request = request(root.path());
        let journal = journal(request.clone());
        seed(&store, &journal, PendingOperationStatus::Applied);
        let prior = serde_json::to_value(&journal).unwrap();
        let mut malformed = prior.clone();
        malformed["lease_path"] = serde_json::json!("/unrelated/lease");
        store
            .update_operation(
                request.operation_id,
                &prior,
                &malformed,
                PendingOperationStatus::Applied,
            )
            .unwrap();
        assert!(
            RepositoryService::system("must-not-execute-git")
                .update_worktree_setup_intent(&store, request.operation_id, None, intent(&request))
                .is_err()
        );
        assert_eq!(
            store
                .journaled_worktree_operation(request.operation_id)
                .unwrap()
                .unwrap()
                .payload,
            malformed
        );
        assert!(
            store
                .journaled_worktree_operation(OperationId::new())
                .unwrap()
                .is_none()
        );
        let service = RepositoryService::system("must-not-execute-git");
        assert!(
            service
                .worktree_setup_creation(&store, request.operation_id)
                .is_err()
        );
        assert!(
            service
                .worktree_setup_creation(&store, OperationId::new())
                .unwrap()
                .is_none()
        );
    }
}
