//! In-process creation ownership is distinct from a serialized path description.
//!
//! No type here reconstructs authority from a report path. The original live
//! session owns private publication receipts on every outcome; only a verified
//! active-to-settled transition grants the created-worktree capability.
use super::*;

#[derive(Clone, Debug)]
#[must_use = "retain or explicitly adopt the live creation ownership before dropping its path report"]
pub struct RetainedWorktreeCreation {
    pub report: WorktreeOperationReport,
    pub created: Option<SettledCreatedWorktree>,
    pub authority_error: Option<String>,
    retention: Option<CreationRetention>,
}

/// Keeps original roots and unresolved private publication receipts alive.
/// This is not a serialized recovery description and has no filesystem Drop.
#[derive(Clone)]
pub struct CreationRetention {
    operation: OperationId,
    generation: u64,
    source: WorktreeAuthority,
    _session: Arc<creation_authority::CreationSession>,
    original_store: Arc<StateStore>,
}

impl fmt::Debug for CreationRetention {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CreationRetention")
            .field("operation", &self.operation)
            .field("generation", &self.generation)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl CreationRetention {
    pub fn operation(&self) -> OperationId { self.operation }
    pub fn generation(&self) -> u64 { self.generation }
    pub fn source(&self) -> &WorktreeAuthority { &self.source }
    #[cfg(test)]
    pub(super) fn session_weak(&self) -> std::sync::Weak<creation_authority::CreationSession> {
        Arc::downgrade(&self._session)
    }
}

#[derive(Clone, Debug)]
pub struct SettledCreatedWorktree {
    operation: OperationId,
    generation: u64,
    record: WorktreeRecord,
    source: WorktreeAuthority,
    destination: WorktreeAuthority,
    _retention: CreationRetention,
}

/// Transport consumes original retained handles, never path-based recapture.
/// These are ordinary in-process handles, not a serializable authority DTO.
#[derive(Debug)]
pub(crate) struct SettledCreationRoots {
    pub operation: OperationId,
    pub generation: u64,
    pub record: WorktreeRecord,
    pub source: (fs_safety::Root, fs_safety::Root, fs_safety::Root),
    pub destination: (fs_safety::Root, fs_safety::Root, fs_safety::Root),
}

impl SettledCreatedWorktree {
    pub fn operation(&self) -> OperationId { self.operation }
    pub fn generation(&self) -> u64 { self.generation }
    pub fn record(&self) -> &WorktreeRecord { &self.record }
    pub fn source(&self) -> &WorktreeAuthority { &self.source }
    pub fn destination(&self) -> &WorktreeAuthority { &self.destination }
    pub fn matches_outcome(&self, outcome: &WorktreeCreationOutcome) -> bool {
        matches!(outcome, WorktreeCreationOutcome::Created { worktree, .. } if *worktree == self.record)
    }
    /// Filesystem work: call only on a blocking worker. This validates both
    /// ORIGINAL scopes without minting replacement authority from their names.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        self.source.validate()?;
        self.destination.validate()
    }
    /// Blocking-worker evidence read through the SAME connection retained by
    /// the actual creator. No caller-provided store or path can substitute it.
    /// The result is producer evidence, not host audience/start authority.
    pub(crate) fn read_original_journal(&self) -> Result<ValidatedCreationOperation, RepositoryError> {
        let roots = self.transport_roots()?;
        let checked = ReceivedCreationScopes::validate_received(
            &self._retention.original_store, roots.operation, roots.generation,
            [roots.source.0, roots.source.1, roots.source.2,
             roots.destination.0, roots.destination.1, roots.destination.2],
        )?;
        if checked.record() != &self.record {
            return Err(ValidationError::Io("The original settled creation record changed".into()).into());
        }
        Ok(checked.into_validated_operation())
    }
    pub(crate) fn transport_roots(&self) -> Result<SettledCreationRoots, RepositoryError> {
        self.validate()?;
        Ok(SettledCreationRoots { operation: self.operation, generation: self.generation,
            record: self.record.clone(), source: self.source.retained_roots()?,
            destination: self.destination.retained_roots()? })
    }
}

pub(super) struct ActiveCreated {
    operation: OperationId,
    generation: u64,
    source: WorktreeAuthority,
    destination: WorktreeAuthority,
    record: WorktreeRecord,
}
impl ActiveCreated {
    pub(super) fn new(operation: OperationId, generation: u64,
        source: WorktreeAuthority, destination: WorktreeAuthority, record: WorktreeRecord) -> Self {
        Self { operation, generation, source, destination, record }
    }
    fn settle(self, report: &WorktreeOperationReport, retention: CreationRetention)
        -> Result<SettledCreatedWorktree, RepositoryError> {
        let invalid = || ValidationError::Io("The original created-worktree authority did not settle with its exact durable operation".into());
        let Some(WorktreeCreationOutcome::Created { worktree, .. }) = report.outcome.as_ref() else { return Err(invalid().into()); };
        let mut expected = self.record.clone();
        expected.lock = WorktreeLock::default();
        if expected.created_at_unix_ms.is_none() { expected.created_at_unix_ms = Some(report.created_at_unix_ms); }
        if *worktree != expected || report.operation_id != self.operation || report.generation != self.generation
            || report.state != WorktreeOperationState::Created
            || report.request.as_ref().is_none_or(|request| request.operation_id != self.operation
                || request.repository != self.source.path() || request.destination != self.destination.path())
            || worktree.path != self.destination.path() || worktree.lock.locked
        { return Err(invalid().into()); }
        self.source.validate()?;
        self.destination.validate()?;
        let (_, admin, _) = self.destination.retained_roots()?;
        match fs_safety::metadata_beneath(&admin, Path::new("locked")) {
            Err(error) if error.is_not_found() => {},
            _ => return Err(ValidationError::Io("Created worktree remains locked or its unlock state is unknown; automatic opening was withheld".into()).into()),
        }
        Ok(SettledCreatedWorktree { operation: self.operation, generation: self.generation,
            record: worktree.clone(), source: self.source, destination: self.destination,
            _retention: retention })
    }
}

impl RetainedWorktreeCreation {
    pub fn retention(&self) -> Option<&CreationRetention> { self.retention.as_ref() }
    pub(super) fn refused(report: WorktreeOperationReport) -> Self {
        Self { report, created: None, authority_error: None, retention: None }
    }
    pub(super) fn from_original(report: WorktreeOperationReport,
        session: Arc<creation_authority::CreationSession>, source: WorktreeAuthority,
        original_store: Arc<StateStore>,
    ) -> Self {
        let retention = CreationRetention { operation: report.operation_id,
            generation: report.generation, source, _session: session.clone(), original_store };
        let candidate = session.pending_settlement.lock().unwrap_or_else(|p|p.into_inner()).take();
        let (created, authority_error) = if report.state == WorktreeOperationState::Created {
            match candidate.ok_or_else(|| RepositoryError::from(ValidationError::Io("The Created report has no original pre-unlock authority; automatic opening was withheld".into())))
                .and_then(|candidate: ActiveCreated| candidate.settle(&report, retention.clone())) {
                Ok(created) => (Some(created), None),
                Err(error) => (None, Some(error.to_string())),
            }
        } else { (None, None) };
        Self { report, created, authority_error, retention: Some(retention) }
    }
}
