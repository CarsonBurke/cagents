//! Complete Git inventories for deletion and checked-out PR reconciliation.
use super::*;
use std::os::unix::fs::PermissionsExt;

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

pub(super) fn resolved_git_executable(program: &Path) -> Result<PathBuf, String> {
    if program.is_absolute() || program.components().count() > 1 {
        return fs::canonicalize(program).map_err(|error| error.to_string());
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(program))
        .find(|path| {
            fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| "Git executable could not be resolved before reconciliation".into())
        .and_then(|path| fs::canonicalize(path).map_err(|error| error.to_string()))
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
        let mut status = invalid_output(parse_status(&output.stdout), output)?;
        if !status.dirty.is_dirty() {
            // Git's own child status does not inherit these explicit display
            // overrides. Visit every populated child, including nested ones,
            // with the same read-only safety policy in that child's directory.
            let executable = resolved_git_executable(&self.git).map_err(|reason| {
                RepositoryError::InvalidOutput {
                    field: "worktree_safety",
                    reason,
                }
            })?;
            let command = self
                .git_read(
                    worktree,
                    [
                        "submodule",
                        "foreach",
                        "--quiet",
                        "--recursive",
                        include_str!("pr_checkout/submodule_status.sh"),
                    ],
                )
                .env("CAGENTS_PR_GIT", executable);
            let output = self.run_checked(&command, cancellation)?;
            require_complete_inventory(&output)?;
            if !output.stdout.is_empty() {
                // Child paths are relative to different repositories. Only a
                // safety dirty marker is needed; do not publish them as root rows.
                status.dirty.submodule = status.dirty.submodule.saturating_add(1);
            }
        }
        Ok(status)
    }
}
