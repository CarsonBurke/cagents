//! Checked branch deletion shared by worktree removal surfaces.
#[cfg(any(feature = "ui", feature = "host"))]
mod force_delete;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchDeleteFailureKind {
    CheckedOutElsewhere,
    Unmerged,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{detail}")]
pub struct BranchDeleteFailure {
    pub kind: BranchDeleteFailureKind,
    pub detail: String,
}

#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) fn git_command(repository: &std::path::Path) -> crate::repository::CommandSpec {
    crate::repository::RepositoryService::system("git")
        .git_base(repository, false)
        .timeout(std::time::Duration::from_secs(30))
}

#[cfg(any(feature = "ui", feature = "host"))]
pub fn delete_branch(
    repository: &std::path::Path,
    branch: &str,
    force: bool,
    cancellation: &crate::repository::CancellationToken,
) -> Result<(), String> {
    delete_branch_checked(repository, branch, force, cancellation)
        .map_err(|error| error.to_string())
}

#[cfg(any(feature = "ui", feature = "host"))]
pub fn delete_branch_checked(
    repository: &std::path::Path,
    branch: &str,
    force: bool,
    cancellation: &crate::repository::CancellationToken,
) -> Result<(), BranchDeleteFailure> {
    delete_branch_checked_at(repository, branch, force, None, cancellation)
}

/// `branch` is always an exact local branch name, never a full ref.
/// A force confirmation can bind its consent to the branch commit it showed.
/// Forced deletion validates the shown OID inside Git's prepared reference
/// transaction while preserving native checkout refusal and config cleanup.
#[cfg(any(feature = "ui", feature = "host"))]
pub fn delete_branch_checked_at(
    repository: &std::path::Path,
    branch: &str,
    force: bool,
    expected_oid: Option<&str>,
    cancellation: &crate::repository::CancellationToken,
) -> Result<(), BranchDeleteFailure> {
    use crate::repository::{
        Oid, ProcessExecutor, RepositoryService, SystemProcessExecutor, ValidatedBranchName,
    };
    let other = |detail: String| BranchDeleteFailure {
        kind: BranchDeleteFailureKind::Other,
        detail,
    };
    let branch = ValidatedBranchName::parse(branch).map_err(|error| other(error.to_string()))?;
    let service = RepositoryService::system("git");
    let checked_out = || -> Result<(), BranchDeleteFailure> {
        let records = service
            .list_worktrees(repository, cancellation)
            .map_err(|error| other(error.to_string()))?;
        if let Some(record) = records
            .iter()
            .find(|record| record.branch.as_deref() == Some(branch.full_ref().as_str()))
        {
            return Err(BranchDeleteFailure {
                kind: BranchDeleteFailureKind::CheckedOutElsewhere,
                detail: format!(
                    "Branch {} is checked out at {}",
                    branch.as_str(),
                    record.path.display()
                ),
            });
        }
        Ok(())
    };
    checked_out()?;
    let read_oid = |revision: &str| -> Result<Oid, BranchDeleteFailure> {
        let command =
            git_command(repository).args(["rev-parse", "--verify", "--end-of-options", revision]);
        let output = SystemProcessExecutor
            .execute(&command, cancellation)
            .and_then(|output| output.require_success(&command.program))
            .map_err(|error| other(error.reported_detail()))?;
        Oid::parse_object(
            String::from_utf8(output.stdout)
                .map_err(|error| other(error.to_string()))?
                .trim(),
        )
        .map_err(|error| other(error.to_string()))
    };
    let full_ref = branch.full_ref();
    let observed = read_oid(&full_ref)?;
    if expected_oid.is_some_and(|expected| expected != observed.as_str()) {
        return Err(other(
            "The branch changed after confirmation; review it again".into(),
        ));
    }
    if !force {
        let upstream_revision = format!("{}@{{upstream}}", branch.as_str());
        let upstream_command = git_command(repository).args([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &upstream_revision,
        ]);
        let upstream = SystemProcessExecutor
            .execute(&upstream_command, cancellation)
            .map_err(|error| other(error.reported_detail()))?;
        let target = if upstream.success {
            Oid::parse_object(
                String::from_utf8(upstream.stdout)
                    .map_err(|error| other(error.to_string()))?
                    .trim(),
            )
            .map_err(|error| other(error.to_string()))?
        } else if matches!(upstream.exit_code, Some(1 | 128)) {
            read_oid("HEAD")?
        } else {
            return Err(other(
                "Could not determine the branch's merge target; it was retained".into(),
            ));
        };
        let command = git_command(repository).args([
            "merge-base",
            "--is-ancestor",
            observed.as_str(),
            target.as_str(),
        ]);
        let output = SystemProcessExecutor
            .execute(&command, cancellation)
            .map_err(|error| other(error.reported_detail()))?;
        if output.exit_code == Some(1) {
            return Err(BranchDeleteFailure {
                kind: BranchDeleteFailureKind::Unmerged,
                detail: format!(
                    "Branch {} contains commits not merged into its upstream or HEAD",
                    branch.as_str()
                ),
            });
        }
        output
            .require_success(&command.program)
            .map_err(|error| other(error.reported_detail()))?;
    }
    // Fence checkout transitions in all currently registered worktrees. Git's
    // final native deletion still rechecks its complete checkout inventory.
    let mut head_locks = Vec::new();
    if force {
        let records = service
            .list_worktrees(repository, cancellation)
            .map_err(|error| other(error.to_string()))?;
        let mut heads = records
            .iter()
            .filter(|record| !record.bare)
            .map(|record| {
                service
                    .git_dir(&record.path, cancellation)
                    .map(|directory| directory.join("HEAD"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| other(error.to_string()))?;
        heads.sort();
        heads.dedup();
        for head in heads {
            head_locks.push(crate::repository::GitFileLock::acquire(&head).map_err(other)?);
        }
        for lock in &head_locks {
            lock.check().map_err(other)?;
        }
    }
    checked_out()?;
    if force {
        force_delete::delete(
            repository,
            branch.as_str(),
            &full_ref,
            observed.as_str(),
            cancellation,
        )
        .map_err(other)?;
    } else {
        let command = git_command(repository).args(["branch", "-d", "--", branch.as_str()]);
        SystemProcessExecutor
            .execute(&command, cancellation)
            .and_then(|output| output.require_success(&command.program))
            .map_err(|error| other(error.reported_detail()))?;
    }
    Ok(())
}
#[cfg(all(test, any(feature = "ui", feature = "host")))]
mod branch_delete_tests {
    use super::*;
    use crate::repository::CancellationToken;
    use std::{fs, path::Path, process::Command};

    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args([
                "-c",
                "user.name=Branch Contract",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    #[test]
    fn branch_delete_distinguishes_checkout_unmerged_and_stale_force_consent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git(path, &["init", "-q", "-b", "main"]);
        fs::write(path.join("file"), "base").unwrap();
        git(path, &["add", "file"]);
        git(path, &["commit", "-qm", "base"]);
        let cancel = CancellationToken::new();
        let checked = delete_branch_checked(path, "main", true, &cancel).unwrap_err();
        assert_eq!(checked.kind, BranchDeleteFailureKind::CheckedOutElsewhere);
        git(path, &["branch", "merged"]);
        let base = git(path, &["rev-parse", "refs/heads/merged"]);
        delete_branch_checked_at(path, "merged", false, Some(&base), &cancel).unwrap();
        assert!(git(path, &["branch", "--list", "merged"]).is_empty());
        git(path, &["checkout", "-qb", "unmerged"]);
        fs::write(path.join("file"), "unmerged").unwrap();
        git(path, &["commit", "-qam", "new"]);
        let first = git(path, &["rev-parse", "HEAD"]);
        git(path, &["checkout", "-q", "main"]);
        let error =
            delete_branch_checked_at(path, "unmerged", false, Some(&first), &cancel).unwrap_err();
        assert_eq!(error.kind, BranchDeleteFailureKind::Unmerged);
        git(path, &["checkout", "-q", "unmerged"]);
        fs::write(path.join("file"), "advanced").unwrap();
        git(path, &["commit", "-qam", "advance"]);
        let latest = git(path, &["rev-parse", "HEAD"]);
        git(path, &["checkout", "-q", "main"]);
        let error =
            delete_branch_checked_at(path, "unmerged", true, Some(&first), &cancel).unwrap_err();
        assert_eq!(error.kind, BranchDeleteFailureKind::Other);
        assert_eq!(git(path, &["rev-parse", "refs/heads/unmerged"]), latest);
        git(
            path,
            &[
                "config",
                "--local",
                "branch.unmerged.description",
                "discard only after confirmed",
            ],
        );
        git(path, &["pack-refs", "--all"]);
        delete_branch_checked_at(path, "unmerged", true, Some(&latest), &cancel).unwrap();
        assert!(git(path, &["branch", "--list", "unmerged"]).is_empty());
        let config = Command::new("git")
            .current_dir(path)
            .args(["config", "--local", "--get", "branch.unmerged.description"])
            .output()
            .unwrap();
        assert_eq!(config.status.code(), Some(1));
    }
    #[test]
    fn branch_deletion_preserves_exact_names_and_dotted_config_subsections() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git(path, &["init", "-q", "-b", "main"]);
        fs::write(path.join("file"), "base").unwrap();
        git(path, &["add", "file"]);
        git(path, &["commit", "-qm", "base"]);
        let cancel = CancellationToken::new();
        git(path, &["branch", "topic"]);
        git(path, &["branch", "refs/heads/topic"]);
        let base = git(path, &["rev-parse", "HEAD"]);
        delete_branch_checked_at(path, "refs/heads/topic", true, Some(&base), &cancel).unwrap();
        assert_eq!(git(path, &["rev-parse", "refs/heads/topic"]), base);
        assert!(git(path, &["branch", "--list", "refs/heads/topic"]).is_empty());
        git(
            path,
            &[
                "config",
                "--local",
                "branch.topic.child.description",
                "preserved",
            ],
        );
        delete_branch_checked_at(path, "topic", true, Some(&base), &cancel).unwrap();
        assert_eq!(
            git(
                path,
                &[
                    "config",
                    "--local",
                    "--get",
                    "branch.topic.child.description"
                ]
            ),
            "preserved"
        );
        git(path, &["branch", "topic"]);
        git(
            path,
            &["config", "--local", "branch.topic.description", "remove"],
        );
        delete_branch_checked_at(path, "topic", true, Some(&base), &cancel).unwrap();
        assert_eq!(
            git(
                path,
                &[
                    "config",
                    "--local",
                    "--get",
                    "branch.topic.child.description"
                ]
            ),
            "preserved"
        );
    }
    #[test]
    fn force_deletion_fences_checkout_and_changed_commit_during_final_effect() {
        use std::{
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };
        const MODE: &str = "CAGENTS_BRANCH_RACE_MODE";
        if let Ok(mode) = std::env::var(MODE) {
            let root =
                std::path::PathBuf::from(std::env::var_os("CAGENTS_BRANCH_RACE_ROOT").unwrap());
            let path = root.join("repo");
            git(&path, &["init", "-q", "-b", "main"]);
            fs::write(path.join("file"), "base").unwrap();
            git(&path, &["add", "file"]);
            git(&path, &["commit", "-qm", "base"]);
            let base = git(&path, &["rev-parse", "HEAD"]);
            git(&path, &["branch", "topic"]);
            let other = root.join("other");
            git(
                &path,
                &["worktree", "add", "-qb", "other", other.to_str().unwrap()],
            );
            // A different commit may become the branch head after confirmation.
            fs::write(path.join("file"), "advance").unwrap();
            git(&path, &["commit", "-qam", "advance"]);
            let next = git(&path, &["rev-parse", "HEAD"]);
            fs::write(root.join("advance-oid"), &next).unwrap();
            git(
                &path,
                &[
                    "config",
                    "--local",
                    "branch.topic.description",
                    "retain on refusal",
                ],
            );
            let policy = if mode == "policy" {
                let hooks = path.join("relative'hooks");
                fs::create_dir(&hooks).unwrap();
                git(
                    &path,
                    &["config", "--local", "core.hooksPath", "relative'hooks"],
                );
                let delegate = hooks.join("cagents-policy");
                fs::write(
                    &delegate,
                    r##"#!/bin/sh
set -eu
[ "$(git config --get cagents.inherited)" = parameters ]
[ "$(git config --get cagents.count)" = count ]
[ "$(git config --get core.fsmonitor)" = false ]
/bin/cat >/dev/null
printf '%s\n' "$1" >> "$CAGENTS_BRANCH_RACE_ROOT/policy-phases"
"##,
                )
                .unwrap();
                fs::set_permissions(&delegate, fs::Permissions::from_mode(0o700)).unwrap();
                let hook = hooks.join("reference-transaction");
                fs::write(
                    &hook,
                    "#!/bin/sh\n/bin/cat > \"$CAGENTS_BRANCH_RACE_ROOT/policy-input.$1\"\nexec git hook run cagents-policy -- \"$@\"\n",
                )
                .unwrap();
                hook
            } else {
                let hook = path.join(".git/hooks/reference-transaction");
                fs::write(&hook,"#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s\\n' \"$1\" >> \"$CAGENTS_BRANCH_RACE_ROOT/policy-phases\"\n").unwrap();
                hook
            };
            fs::set_permissions(&policy, fs::Permissions::from_mode(0o700)).unwrap();
            let result = delete_branch_checked_at(
                &path,
                "topic",
                true,
                Some(&base),
                &CancellationToken::new(),
            );
            if mode == "checkout" {
                result.unwrap();
                assert_ne!(
                    fs::read_to_string(root.join("checkout-status"))
                        .unwrap()
                        .trim(),
                    "0"
                );
                assert_eq!(git(&other, &["symbolic-ref", "HEAD"]), "refs/heads/other");
                assert!(git(&path, &["branch", "--list", "topic"]).is_empty());
            } else if mode == "policy" {
                result.unwrap();
                assert!(git(&path, &["branch", "--list", "topic"]).is_empty());
                assert!(
                    fs::read_to_string(root.join("policy-input.prepared"))
                        .unwrap()
                        .contains(" refs/heads/topic\n")
                );
            } else {
                assert!(result.is_err());
                assert_eq!(git(&path, &["rev-parse", "refs/heads/topic"]), next);
                assert_eq!(
                    git(
                        &path,
                        &["config", "--local", "--get", "branch.topic.description"]
                    ),
                    "retain on refusal"
                );
            }
            // The capability probe must not invoke the user's policy hook.
            // Actual deletion still invokes it once per native transaction phase.
            let phases = fs::read_to_string(root.join("policy-phases")).unwrap();
            assert!(phases.lines().any(|phase| phase == "prepared"));
            if mode == "policy" {
                assert_eq!(
                    phases.lines().filter(|phase| *phase == "prepared").count(),
                    1
                );
            }
            return;
        }
        for mode in ["checkout", "advance", "policy"] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let root = directory.path();
            fs::create_dir(root.join("repo")).unwrap();
            fs::create_dir(root.join("bin")).unwrap();
            let wrapper = root.join("bin/git");
            fs::write(&wrapper,r##"#!/bin/sh
set -u
case " $* " in
  *' branch -D -- topic '*|*' update-ref --no-deref -d refs/heads/topic '*)
    if [ "$CAGENTS_BRANCH_RACE_MODE" = checkout ]; then
      /usr/bin/git -C "$CAGENTS_BRANCH_RACE_ROOT/other" checkout -q topic
      printf '%s\n' "$?" > "$CAGENTS_BRANCH_RACE_ROOT/checkout-status"
    elif [ "$CAGENTS_BRANCH_RACE_MODE" = advance ]; then
      next=$(/bin/cat "$CAGENTS_BRANCH_RACE_ROOT/advance-oid")
      /usr/bin/git -C "$CAGENTS_BRANCH_RACE_ROOT/repo" update-ref refs/heads/topic "$next" || exit 90
    fi
    ;;
esac
exec /usr/bin/git "$@"
"##).unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
            let mut path = std::ffi::OsString::from(root.join("bin"));
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            let mut child=Command::new(std::env::current_exe().unwrap())
                .args(["--exact","worktree_branch::branch_delete_tests::force_deletion_fences_checkout_and_changed_commit_during_final_effect","--nocapture"])
                .env(MODE,mode).env("CAGENTS_BRANCH_RACE_ROOT",root).env("PATH",path)
                .env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_CONFIG_SYSTEM","/dev/null")
                .env("GIT_CONFIG_PARAMETERS","'cagents.inherited=parameters'")
                .env("GIT_CONFIG_COUNT","1").env("GIT_CONFIG_KEY_0","cagents.count").env("GIT_CONFIG_VALUE_0","count")
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!(
                        "branch race child timed out: {:?}",
                        child.wait_with_output()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
        }
    }
}
