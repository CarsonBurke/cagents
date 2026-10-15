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

/// A cheap in-process snapshot of private receipt obligations. Zero counts
/// never mean the journal/outcome is reconciled, nor grant release or opening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreationPrivateRecoverySummary {
    pub pending_preparations: usize,
    pub pending_publications: usize,
}
impl CreationPrivateRecoverySummary {
    pub fn is_clear(self) -> bool {
        self.pending_preparations == 0 && self.pending_publications == 0
    }
}

/// Result of original private steps only. This does not mutate a public
/// checkout, settle an Unknown report, or create a new opening capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreationPrivateRetry {
    pub pending: CreationPrivateRecoverySummary,
    pub failures: Vec<String>,
}

/// Keeps original roots and unresolved private publication receipts alive.
/// This is not a serialized recovery description and has no filesystem Drop.
#[derive(Clone, Debug)]
pub struct CreationRetention {
    operation: OperationId,
    generation: u64,
    source: WorktreeAuthority,
    _session: Arc<creation_authority::CreationSession>,
}

impl CreationRetention {
    pub fn operation(&self) -> OperationId {
        self.operation
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn source(&self) -> &WorktreeAuthority {
        &self.source
    }
    /// Nonblocking: reads only original-session atomic counters, with no
    /// filesystem calls or worker-held recovery mutex acquisition.
    pub fn recovery_summary(&self) -> CreationPrivateRecoverySummary {
        self._session.recovery_summary()
    }
    /// Blocking-worker only. Retry original private preparation/mode/staging
    /// steps; retain the SAME owner on failure. No public/quarantine removal.
    pub fn retry_private_steps(&self) -> Result<CreationPrivateRetry, RepositoryError> {
        self._session.retry_private_steps()
    }
    #[cfg(test)]
    pub(super) fn session_weak(&self) -> std::sync::Weak<creation_authority::CreationSession> {
        Arc::downgrade(&self._session)
    }
}

/// Original scopes settled before unlock publication, with no known private
/// preparation/staging obligation. Transfer the SAME carrier to the mounted
/// card/acknowledged host owner; a path/record projection is not that transfer.
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
    pub fn operation(&self) -> OperationId {
        self.operation
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn record(&self) -> &WorktreeRecord {
        &self.record
    }
    pub fn source(&self) -> &WorktreeAuthority {
        &self.source
    }
    pub fn destination(&self) -> &WorktreeAuthority {
        &self.destination
    }
    pub fn matches_outcome(&self, outcome: &WorktreeCreationOutcome) -> bool {
        matches!(outcome, WorktreeCreationOutcome::Created { worktree, .. } if *worktree == self.record)
    }
    /// Filesystem work: call only on a blocking worker. This validates both
    /// ORIGINAL scopes without minting replacement authority from their names.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        self.source.validate()?;
        self.destination.validate()
    }
    pub(crate) fn transport_roots(&self) -> Result<SettledCreationRoots, RepositoryError> {
        self.validate()?;
        Ok(SettledCreationRoots {
            operation: self.operation,
            generation: self.generation,
            record: self.record.clone(),
            source: self.source.retained_roots()?,
            destination: self.destination.retained_roots()?,
        })
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
    pub(super) fn new(
        operation: OperationId,
        generation: u64,
        source: WorktreeAuthority,
        destination: WorktreeAuthority,
        record: WorktreeRecord,
    ) -> Self {
        Self {
            operation,
            generation,
            source,
            destination,
            record,
        }
    }
    fn settle(
        self,
        report: &WorktreeOperationReport,
        retention: CreationRetention,
    ) -> Result<SettledCreatedWorktree, RepositoryError> {
        let invalid = || {
            ValidationError::Io("The original created-worktree authority did not settle with its exact durable operation".into())
        };
        let Some(WorktreeCreationOutcome::Created { worktree, .. }) = report.outcome.as_ref()
        else {
            return Err(invalid().into());
        };
        let mut expected = self.record.clone();
        expected.lock = WorktreeLock::default();
        if expected.created_at_unix_ms.is_none() {
            expected.created_at_unix_ms = Some(report.created_at_unix_ms);
        }
        if *worktree != expected
            || report.operation_id != self.operation
            || report.generation != self.generation
            || report.state != WorktreeOperationState::Created
            || report.request.as_ref().is_none_or(|request| {
                request.operation_id != self.operation
                    || request.repository != self.source.path()
                    || request.destination != self.destination.path()
            })
            || worktree.path != self.destination.path()
            || worktree.lock.locked
        {
            return Err(invalid().into());
        }
        if !retention.recovery_summary().is_clear() {
            return Err(ValidationError::Io("Private creation recovery is still pending; no settled opening authority was granted".into()).into());
        }
        self.source.validate()?;
        self.destination.validate()?;
        let (_, admin, _) = self.destination.retained_roots()?;
        match fs_safety::metadata_beneath(&admin, Path::new("locked")) {
            Err(error) if error.is_not_found() => {},
            _ => return Err(ValidationError::Io("Created worktree remains locked or its unlock state is unknown; automatic opening was withheld".into()).into()),
        }
        Ok(SettledCreatedWorktree {
            operation: self.operation,
            generation: self.generation,
            record: worktree.clone(),
            source: self.source,
            destination: self.destination,
            _retention: retention,
        })
    }
}

impl RetainedWorktreeCreation {
    /// Describe a preflight refusal before any creation session or effect.
    /// This cannot produce a Created outcome or any live creation authority.
    pub fn not_started(
        request: &CreateWorktreeRequest,
        generation: u64,
        detail: String,
    ) -> Self {
        Self::refused(WorktreeOperationReport {
            operation_id: request.operation_id,
            generation,
            request: Some(request.clone()),
            state: WorktreeOperationState::NotCreated,
            outcome: None,
            detail,
            created_at_unix_ms: operation_timestamp(),
        })
    }

    pub fn retention(&self) -> Option<&CreationRetention> {
        self.retention.as_ref()
    }
    pub(super) fn refused(report: WorktreeOperationReport) -> Self {
        Self {
            report,
            created: None,
            authority_error: None,
            retention: None,
        }
    }
    pub(super) fn from_original(
        report: WorktreeOperationReport,
        session: Arc<creation_authority::CreationSession>,
        source: WorktreeAuthority,
    ) -> Self {
        let retention = CreationRetention {
            operation: report.operation_id,
            generation: report.generation,
            source,
            _session: session.clone(),
        };
        let candidate = session
            .pending_settlement
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let (created, authority_error) = if report.state == WorktreeOperationState::Created {
            match candidate.ok_or_else(|| RepositoryError::from(ValidationError::Io("The Created report has no original pre-unlock authority; automatic opening was withheld".into())))
                .and_then(|candidate: ActiveCreated| candidate.settle(&report, retention.clone())) {
                Ok(created) => (Some(created), None),
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        Self {
            report,
            created,
            authority_error,
            retention: Some(retention),
        }
    }
}
