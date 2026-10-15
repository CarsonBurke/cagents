//! Durable setup choices, independent of execution or UI state.

use super::*;

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

fn settled_setup_journal(operation: &PendingOperation) -> Result<WorktreeJournal, StoreError> {
    let creation_id = operation.id;
    let invalid = || StoreError::InvalidState(format!(
        "worktree creation {creation_id} is not a valid settled creation"
    ));
    let journal: WorktreeJournal = serde_json::from_value(operation.payload.clone())?;
    let paths = [&journal.common_dir, &journal.request.managed_root, &journal.request.destination];
    if operation.status != PendingOperationStatus::Applied
        || operation.entity_kind != "worktree_creation"
        || operation.action != "create"
        || journal.version != 1
        || journal.state != WorktreeOperationState::Created
        || !matches!(&journal.outcome, Some(WorktreeCreationOutcome::Created { .. }))
        || journal.request.operation_id != creation_id
        || journal.generation != operation.base_generation
        || operation.entity_id != journal_identity(&journal.common_dir, &journal.request)
        || journal.lease_path != journal.common_dir.join(format!("cagents-operation-{creation_id}.lease"))
        || !destination_within_managed_root(&journal.request.destination, &journal.request.managed_root)
        || paths.iter().any(|path| !path.is_absolute() || path.components().any(|part| matches!(part, std::path::Component::ParentDir)))
        || journal.base_oid.as_ref().is_none_or(|oid| !is_hex_oid(oid))
        || journal.existing_branch_oid.as_ref().is_some_and(|oid| !is_hex_oid(oid))
        || !journal.effects_started
        || ValidatedBranchName::parse(journal.request.branch.as_str()).is_err()
        || journal.request.base.validate().is_err()
    {
        return Err(invalid());
    }
    Ok(journal)
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Verify metadata from an already loaded journal snapshot. Bulk readers
    /// avoid one database query per receipt; explicit effects still use CAS.
    pub fn worktree_setup_creation_from_row(
        &self,
        operation: &PendingOperation,
    ) -> Result<WorktreeOperationReport, StoreError> {
        let journal=settled_setup_journal(operation)?;
        Ok(journal.report(operation.created_at_unix_ms))
    }

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
        self.worktree_setup_creation_from_row(&operation).map(Some)
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
        let operation = store.journaled_worktree_operation(creation_id)?.ok_or_else(|| {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(root: &Path) -> CreateWorktreeRequest {
        CreateWorktreeRequest {
            linear_issue: None,
            repository: root.join("repo"),
            managed_root: root.join("managed"),
            destination: root.join("managed/topic"),
            branch: ValidatedBranchName::parse("topic").unwrap(),
            base: BaseRevision::object_id("a".repeat(40)).unwrap(),
            track: false,
            operation_id: OperationId::new(),
            setup_intent: None,
        }
    }

    fn journal(request: CreateWorktreeRequest) -> WorktreeJournal {
        let common_dir = request.repository.join(".git");
        WorktreeJournal {
            version: 1,
            generation: 7,
            state: WorktreeOperationState::Created,
            lease_path: common_dir.join(format!("cagents-operation-{}.lease", request.operation_id)),
            lease_identity: None,
            effects_started: true,
            checkout_completed: true,
            base_oid: Some("a".repeat(40)),
            existing_branch_oid: None,
            admin_dir: Some(common_dir.join("worktrees/topic")),
            outcome: Some(WorktreeCreationOutcome::Created {
                worktree: WorktreeRecord {
                    id: WorktreeId::new(), path: request.destination.clone(),
                    created_at_unix_ms: None, head_oid: Some("a".repeat(40)),
                    branch: Some(request.branch.full_ref()), detached: false,
                    bare: false, lock: WorktreeLock::default(), prunable_reason: None,
                },
                warnings: Vec::new(),
            }),
            warnings: Vec::new(),
            detail: "Created".into(),
            request, common_dir,
        }
    }

    fn seed(store: &StateStore, journal: &WorktreeJournal, status: PendingOperationStatus) {
        store.begin_operation(&PendingOperation {
            id: journal.request.operation_id,
            entity_kind: "worktree_creation".into(),
            entity_id: journal_identity(&journal.common_dir, &journal.request),
            action: "create".into(), base_generation: journal.generation,
            payload: serde_json::to_value(journal).unwrap(),
            status: PendingOperationStatus::Pending,
            created_at_unix_ms: 42, updated_at_unix_ms: 42,
        }).unwrap();
        if status != PendingOperationStatus::Pending {
            store.resolve_operation(journal.request.operation_id, status).unwrap();
        }
    }

    fn intent(request: &CreateWorktreeRequest) -> WorktreeSetupIntent {
        WorktreeSetupIntent {
            primary: Some(request.repository.clone()),
            plan: Some(WorktreeSetupPlan {copy: vec![".env".into()], commands: vec!["printf setup".into()], fingerprint: "b".repeat(64)}),
            run_id: Some(OperationId::new()), ..Default::default()
        }
    }

    #[test]
    fn old_requests_and_intents_deserialize_without_setup_fields() {
        let mut value = serde_json::to_value(request(Path::new("/fixture"))).unwrap();
        value.as_object_mut().unwrap().remove("setup_intent");
        assert!(serde_json::from_value::<CreateWorktreeRequest>(value).unwrap().setup_intent.is_none());
        assert_eq!(serde_json::from_str::<WorktreeSetupIntent>("{}").unwrap(), WorktreeSetupIntent::default());
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
        let mut copied = first.clone(); copied.copy_done = true;
        let report = RepositoryService::system("must-not-execute-git")
            .update_worktree_setup_intent(&store, request.operation_id, Some(&first), copied.clone()).unwrap();
        assert_eq!(report.request.unwrap().setup_intent, Some(copied.clone()));
        assert_eq!(report.state, WorktreeOperationState::Created);
        assert_eq!(report.created_at_unix_ms, 42);
        assert_eq!(report.detail, "Created");
        drop(store);
        let reopened = StateStore::open(path, crate::model::BootId::new("setup-intent-restart")).unwrap();
        let row = reopened.journaled_worktree_operation(request.operation_id).unwrap().unwrap();
        let saved: WorktreeJournal = serde_json::from_value(row.payload).unwrap();
        assert_eq!(saved.request.setup_intent, Some(copied.clone()));
        assert_eq!(row.status, PendingOperationStatus::Applied);
        let service = RepositoryService::system("must-not-execute-git");
        let before = reopened.journaled_worktree_operation(request.operation_id).unwrap().unwrap().payload;
        let read = service.worktree_setup_creation(&reopened, request.operation_id).unwrap().unwrap();
        assert_eq!(read.request.unwrap().setup_intent, Some(copied));
        assert_eq!(read.state, WorktreeOperationState::Created);
        assert_eq!(read.created_at_unix_ms, 42);
        assert_eq!(reopened.journaled_worktree_operation(request.operation_id).unwrap().unwrap().payload, before);
        assert!(!request.destination.exists());
    }

    #[test]
    fn stale_callbacks_cannot_overwrite_resumed_run_or_add_intent_to_unsettled_rows() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let store = StateStore::open(root.path().join("state.sqlite"), crate::model::BootId::new("setup-intent")).unwrap();
        let service = RepositoryService::system("must-not-execute-git");
        let request = request(root.path());
        let original = intent(&request);
        let journal = journal(request.clone());
        seed(&store, &journal, PendingOperationStatus::Applied);
        service.update_worktree_setup_intent(&store, request.operation_id, None, original.clone()).unwrap();
        let mut resumed = original.clone(); resumed.run_id = Some(OperationId::new());
        service.update_worktree_setup_intent(&store, request.operation_id, Some(&original), resumed.clone()).unwrap();
        let mut stale = original.clone(); stale.copy_done = true;
        assert!(service.update_worktree_setup_intent(&store, request.operation_id, Some(&original), stale).unwrap_err().to_string().contains("stale setup intent"));
        let row = store.journaled_worktree_operation(request.operation_id).unwrap().unwrap();
        assert_eq!(serde_json::from_value::<WorktreeJournal>(row.payload).unwrap().request.setup_intent, Some(resumed));
        let mut pending = journal.clone();
        pending.request.operation_id = OperationId::new();
        pending.lease_path = pending.common_dir.join(format!("cagents-operation-{}.lease", pending.request.operation_id));
        pending.request.destination = pending.request.managed_root.join("other");
        pending.state = WorktreeOperationState::OutcomeUnknown;
        pending.outcome = None;
        seed(&store, &pending, PendingOperationStatus::Pending);
        assert!(service.update_worktree_setup_intent(&store, pending.request.operation_id, None, original).is_err());
    }

    #[test]
    fn malformed_settled_journals_are_not_rewritten() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let store = StateStore::open(root.path().join("state.sqlite"), crate::model::BootId::new("setup-intent")).unwrap();
        let request = request(root.path());
        let journal = journal(request.clone());
        seed(&store, &journal, PendingOperationStatus::Applied);
        let prior = serde_json::to_value(&journal).unwrap();
        let mut malformed = prior.clone();
        malformed["lease_path"] = serde_json::json!("/unrelated/lease");
        store.update_operation(request.operation_id, &prior, &malformed, PendingOperationStatus::Applied).unwrap();
        assert!(RepositoryService::system("must-not-execute-git")
            .update_worktree_setup_intent(&store, request.operation_id, None, intent(&request)).is_err());
        assert_eq!(store.journaled_worktree_operation(request.operation_id).unwrap().unwrap().payload, malformed);
        assert!(store.journaled_worktree_operation(OperationId::new()).unwrap().is_none());
        let service = RepositoryService::system("must-not-execute-git");
        assert!(service.worktree_setup_creation(&store, request.operation_id).is_err());
        assert!(service.worktree_setup_creation(&store, OperationId::new()).unwrap().is_none());
    }
}
