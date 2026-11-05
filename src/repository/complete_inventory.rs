//! Complete Git inventories for deletion and checked-out PR reconciliation.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn census_error(failure: ProcessFailure) -> RepositoryError {
    if failure.kind == crate::process::ProcessFailureKind::Exit {
        // Preparation presents RepositoryError directly. Retain the bounded
        // causal diagnostic rather than only the helper's numeric exit code.
        RepositoryError::InvalidOutput {
            field: "worktree_safety",
            reason: failure.reported_detail(),
        }
    } else {
        failure.into()
    }
}

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
        let output = self
            .run_checked(&command, cancellation)
            .map_err(census_error)?;
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
            let helper_error = |error: std::io::Error| RepositoryError::InvalidOutput {
                field: "worktree_safety",
                reason: format!("Git census helpers could not be retained: {error}"),
            };
            let helpers = tempfile::tempdir().map_err(helper_error)?;
            fs::write(
                helpers.path().join("submodule-status"),
                include_bytes!("pr_checkout/submodule_status.sh"),
            )
            .map_err(helper_error)?;
            fs::write(
                helpers.path().join("check-submodule-paths"),
                include_bytes!("pr_checkout/submodule_paths.sh"),
            )
            .map_err(helper_error)?;
            let root_guard = CommandSpec::new("/bin/sh")
                .arg(helpers.path().join("submodule-status"))
                .arg("guard")
                .cwd(worktree)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_PAGER", "cat")
                .env("CAGENTS_PR_GIT", &executable)
                .env("CAGENTS_PR_SUBMODULE_HELPERS", helpers.path());
            let output = self
                .run_checked(&root_guard, cancellation)
                .map_err(census_error)?;
            require_complete_inventory(&output)?;
            let command = self
                .git_read(
                    worktree,
                    [
                        "submodule",
                        "foreach",
                        "--quiet",
                        "--recursive",
                        "/bin/sh \"$CAGENTS_PR_SUBMODULE_HELPERS/submodule-status\"",
                    ],
                )
                .env("CAGENTS_PR_GIT", executable)
                .env("CAGENTS_PR_SUBMODULE_HELPERS", helpers.path());
            let output = self
                .run_checked(&command, cancellation)
                .map_err(census_error)?;
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
