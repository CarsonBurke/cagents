//! Independent branch deletion keeps the original repository and native fences.
use super::*;
use crate::{
    fs_safety::Root,
    worktree_branch::{BranchDeleteFailure, BranchDeleteFailureKind},
};
use std::os::unix::fs::MetadataExt;

fn check(root: &Root) -> Result<(), String> {
    let held = root.metadata().map_err(|error| error.to_string())?;
    let named = std::fs::symlink_metadata(root.path()).map_err(|error| error.to_string())?;
    if !named.is_dir() || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
        return Err(
            "The original repository directory changed; inspect before another request".into(),
        );
    }
    Ok(())
}

pub(super) fn delete<E: ProcessExecutor>(
    service: &Service<E>,
    repository: &Path,
    branch: String,
    force: bool,
    expected_oid: Option<String>,
    cancel: &CancellationToken,
) -> Result<Reply, String> {
    if force && expected_oid.is_none() {
        return Ok(Reply::BranchDeleteFailed {
            branch,
            failure: BranchDeleteFailure {
                kind: BranchDeleteFailureKind::Other,
                detail: "Force deletion requires the exact branch commit shown in confirmation"
                    .into(),
            },
            outcome_unknown: false,
        });
    }
    let repository = Root::open(
        &repository
            .canonicalize()
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let common = service
        .common_dir(repository.path(), cancel)
        .map_err(|error| error.to_string())?;
    let common = Root::open(&common).map_err(|error| error.to_string())?;
    check(&repository)?;
    check(&common)?;
    // This existing primitive retains every registered admin/HEAD, native
    // branch refusal and expected-OID proof through Git's prepared transaction.
    let result = crate::worktree_branch::delete_branch_checked_at(
        &common.process_path(),
        &branch,
        force,
        expected_oid.as_deref(),
        cancel,
    );
    let post = check(&repository).and_then(|_| check(&common));
    match (result, post) {
        (Ok(()), Ok(())) => Ok(Reply::BranchDeleted { branch }),
        (result, post) => {
            let mut failure = result.err().unwrap_or_else(|| BranchDeleteFailure {
                kind: BranchDeleteFailureKind::Other,
                detail: "Branch deletion committed, but its final physical repository proof failed"
                    .into(),
            });
            if let Err(error) = post {
                failure.detail.push_str(&format!("; {error}"));
            }
            // An error after entry into Git can follow successful ref deletion
            // and failed hook/config cleanup. Never certify rollback or replay.
            Ok(Reply::BranchDeleteFailed {
                branch,
                failure,
                outcome_unknown: true,
            })
        }
    }
}
