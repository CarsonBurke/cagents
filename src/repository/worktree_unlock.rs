//! Explicit recovery of a settled creation's leftover Git worktree lock.
use super::*;

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Read-only eligibility check. A familiar lock label alone is insufficient:
    /// its durable creation receipt, administrative identity and exclusive
    /// process lease must all agree. Unknown/foreign locks remain untouched.
    pub fn stale_worktree_lock(
        &self, store: &StateStore, repository: &Path, path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        self.stale_worktree_lock_guard(store, repository, path, cancellation).map(drop)
    }

    /// Explicit user action, never startup reconciliation. Repeat the proof and
    /// retain the process lease through Git so an owning child cannot resume.
    pub fn unlock_stale_worktree(
        &self, store: &StateStore, repository: &Path, path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let lease = self.stale_worktree_lock_guard(store, repository, path, cancellation)?;
        let mut command = self.git_mutation(repository, [OsString::from("worktree"), OsString::from("unlock"), path.as_os_str().to_owned()]);
        command.inherited_lease = Some(lease.as_raw_fd());
        self.run_checked(&command, cancellation).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn stale_worktree_lock_guard(
        &self, store: &StateStore, repository: &Path, path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<fs::File, String> {
        let common = self.common_dir(repository, cancellation).map_err(|error| error.to_string())?;
        let admin = self.git_dir(path, cancellation).map_err(|error| error.to_string())?;
        let record = self.list_worktrees(repository, cancellation).map_err(|error| error.to_string())?
            .into_iter().find(|record| record.path == path).ok_or("Worktree is no longer registered")?;
        let reason = record.lock.reason.as_deref().filter(|_| record.lock.locked).ok_or("Worktree is not locked")?;
        let operation_id = reason.strip_prefix("cagents:").ok_or("This lock belongs to another tool; unlock it explicitly in Git")?;
        let operation = store.journaled_worktree_operations().map_err(|error| error.to_string())?
            .into_iter().find(|operation| operation.id.to_string() == operation_id)
            .ok_or("No matching cagents creation receipt; lock ownership cannot be verified")?;
        let journal: WorktreeJournal = serde_json::from_value(operation.payload).map_err(|_| "Malformed cagents creation receipt")?;
        if journal.version != 1 || operation.action != "create"
            || operation.status != PendingOperationStatus::Applied
            || journal.state != WorktreeOperationState::Created
            || journal.request.operation_id != operation.id
            || journal.generation != operation.base_generation
            || operation.entity_id != journal_identity(&common, &journal.request)
            || journal.common_dir != common
            || journal.admin_dir.as_ref() != Some(&admin)
            || record.branch.as_deref() != Some(journal.request.branch.full_ref().as_str())
            || journal.lease_path != common.join(format!("cagents-operation-{}.lease", operation.id))
            || !matches!(&journal.outcome, Some(WorktreeCreationOutcome::Created { .. }))
        { return Err("The creation is unresolved or its ownership changed; leave its lock in place".into()); }
        let lease = worktree_lease(&journal.lease_path, false).map_err(|error| format!("Creation process lease is active or unavailable: {error}"))?;
        if lease_file_identity(&lease).ok() != journal.lease_identity {
            return Err("Creation process lease identity changed; leave its lock in place".into());
        }
        // Check the label again after acquiring the lease. Git's lock is not a
        // lease against arbitrary external Git, so never infer ownership from
        // the earlier sidebar snapshot.
        let fresh = self.list_worktrees(repository, cancellation).map_err(|error| error.to_string())?
            .into_iter().find(|fresh| fresh.id == record.id && fresh.path == record.path)
            .ok_or("Worktree identity changed while checking its lock")?;
        if !fresh.lock.locked || fresh.lock.reason.as_deref() != Some(reason)
            || fresh.branch != record.branch
            || self.git_dir(path, cancellation).map_err(|error| error.to_string())? != admin
        { return Err("Worktree lock ownership changed; leave it untouched".into()); }
        Ok(lease)
    }
}
