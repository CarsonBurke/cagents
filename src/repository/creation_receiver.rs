//! Worker-only validation of transferred ORIGINAL creation roles. This is not
//! host registration or start authority: the authenticated audience/required-row
//! transaction and the original grant's first-effect gate remain mandatory.
use super::*;

pub(crate) struct ReceivedCreationScopes {
    operation: PendingOperation,
    record: WorktreeRecord,
    source: WorktreeAuthority,
    destination: WorktreeAuthority,
}

impl ReceivedCreationScopes {
    /// `roots` must come from the exact authenticated six-role rights phase,
    /// already pinned without reopening paths. Read the durable operation from
    /// the owning StateStore, never accept a caller-provided journal payload.
    pub(crate) fn validate_received(
        store: &StateStore, operation: OperationId, generation: u64,
        roots: [fs_safety::Root; 6],
    ) -> Result<Self, RepositoryError> {
        let saved = store.journaled_worktree_operation(operation).map_err(|error| invalid(&error.to_string()))?
            .ok_or_else(|| invalid("The exact settled creation journal is unavailable"))?;
        let journal: WorktreeJournal = serde_json::from_value(saved.payload.clone())
            .map_err(|_| invalid("The settled creation journal is malformed"))?;
        if saved.entity_kind != "worktree_creation" || saved.action != "create"
            || saved.status != PendingOperationStatus::Applied || saved.base_generation != generation
            || journal.version != 1 || journal.state != WorktreeOperationState::Created
            || journal.request.operation_id != operation || journal.generation != generation
            || saved.entity_id != journal_identity(&journal.common_dir, &journal.request)
            || journal.lease_path != journal.common_dir.join(format!("cagents-operation-{operation}.lease"))
            || journal.lease_identity.is_none() || !journal.effects_started || !journal.checkout_completed
            || journal.base_oid.as_ref().is_none_or(|oid| !is_hex_oid(oid))
            || journal.existing_branch_oid.as_ref().is_some_and(|oid| !is_hex_oid(oid))
            || journal.observed_branch_oid.as_ref().is_some_and(|oid| !is_hex_oid(oid))
            || (journal.detached_creation && journal.existing_branch_oid.is_none())
            || (journal.branch_advance_attempted && (!journal.detached_creation
                || journal.observed_branch_oid != journal.base_oid))
            || ValidatedBranchName::parse(journal.request.branch.as_str()).is_err()
            || journal.request.base.validate().is_err()
        { return Err(invalid("The operation is not an exact supported settled creation")); }
        let report = journal.report(saved.created_at_unix_ms);
        let Some(WorktreeCreationOutcome::Created { worktree: record, .. }) = report.outcome else {
            return Err(invalid("The settled creation has no exact Created outcome"));
        };
        if record.path != journal.request.destination || record.lock.locked
            || record.lock.reason.is_some() || record.bare || record.detached
            || record.prunable_reason.is_some()
            || record.branch.as_deref() != Some(journal.request.branch.full_ref().as_str())
            || record.head_oid.as_ref().is_none_or(|oid| !is_hex_oid(oid))
        { return Err(invalid("The settled creation outcome is not a live unlocked requested worktree")); }
        let admin = journal.admin_dir.as_deref().ok_or_else(|| invalid("Creation admin identity is missing"))?;
        journal.creation_authority.as_ref().ok_or_else(|| invalid("Legacy creation has no original physical witnesses"))?
            .validate_received_roles(operation, &journal.request, &journal.common_dir, admin, &roots)?;
        // Validate current backlinks through retained admin, never accept a
        // directory-shaped FD in the wrong role or follow a substituted name.
        let [source_root, source_admin, source_common, destination_root, destination_admin, destination_common] = roots;
        match fs_safety::metadata_beneath(&destination_admin, Path::new("locked")) {
            Err(error) if error.is_not_found() => {},
            _ => return Err(invalid("The original created checkout is locked or unknown")),
        }
        let backlink = read_regular(&destination_admin, "gitdir")?;
        if trim_one_line_ending(&backlink) != record.path.join(".git").as_os_str().as_encoded_bytes()
            || trim_one_line_ending(&read_regular(&destination_admin, "commondir")?) != b"../.."
            || trim_one_line_ending(&read_regular(&destination_admin, "HEAD")?)
                != format!("ref: {}", journal.request.branch.full_ref()).as_bytes()
        { return Err(invalid("The original created worktree backlinks or branch changed")); }
        let source = WorktreeAuthority::from_retained_roots(source_root, source_admin, source_common)?;
        let destination = WorktreeAuthority::from_retained_roots(destination_root, destination_admin, destination_common)?;
        Ok(Self {operation: saved, record, source, destination})
    }
    pub(crate) fn operation(&self) -> OperationId { self.operation.id }
    pub(crate) fn generation(&self) -> u64 { self.operation.base_generation }
    pub(crate) fn record(&self) -> &WorktreeRecord { &self.record }
    pub(crate) fn source(&self) -> &WorktreeAuthority { &self.source }
    pub(crate) fn destination(&self) -> &WorktreeAuthority { &self.destination }
    /// Call under the actual host's source/intent admission guard immediately
    /// before retaining/using its grant. This does not acquire that guard or
    /// certify any provider, filesystem effect, or Started receipt.
    pub(crate) fn revalidate(&self, store: &StateStore) -> Result<(), RepositoryError> {
        if store.journaled_worktree_operation(self.operation.id).map_err(|error| invalid(&error.to_string()))?.as_ref() != Some(&self.operation) {
            return Err(invalid("The original settled creation operation changed"));
        }
        self.source.validate()?;
        self.destination.validate()
    }
}
fn invalid(message: &str) -> RepositoryError { ValidationError::Io(message.into()).into() }
fn read_regular(root: &fs_safety::Root, name: &str) -> Result<Vec<u8>, RepositoryError> {
    let mut file = fs_safety::open_beneath(root, Path::new(name)).map_err(|e| invalid(&e.to_string()))?;
    if !file.metadata().map_err(|e| invalid(&e.to_string()))?.is_file() {
        return Err(invalid("The original creation metadata is not a regular file"));
    }
    let mut bytes=Vec::new();
    (&mut file).take(8193).read_to_end(&mut bytes).map_err(|e| invalid(&e.to_string()))?;
    if bytes.len()>8192 || bytes.contains(&0) { return Err(invalid("The original creation metadata is oversized or invalid")); }
    Ok(bytes)
}
