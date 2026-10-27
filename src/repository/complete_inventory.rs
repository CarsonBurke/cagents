//! Complete Git inventories for deletion and checked-out PR reconciliation.
use super::*;

pub(super) fn require_complete_inventory(output: &ProcessOutput) -> Result<(), RepositoryError> {
    if output.stderr.iter().any(|byte| !byte.is_ascii_whitespace()) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("Git inventory warning");
        let detail: String = line.chars().take(FAILURE_DETAIL_LIMIT).collect();
        return Err(RepositoryError::InvalidOutput {
            field: "worktree_safety",
            reason: format!(
                "Git could not inspect every file; worktree was kept: {}",
                detail
            ),
        });
    }
    Ok(())
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Safety decisions require a complete census, even when Git returns
    /// success while warning that a directory could not be read.
    pub(crate) fn status_for_removal(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<StatusSnapshot, RepositoryError> {
        // A display preference must not hide tracked or untracked work inside
        // a submodule from an operation deciding whether local data is safe.
        let command = self
            .status_command(worktree)
            .arg("--ignore-submodules=none");
        let output = self.run_checked(&command, cancellation)?;
        require_complete_inventory(&output)?;
        invalid_output(parse_status(&output.stdout), output)
    }
}
