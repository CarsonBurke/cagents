//! Checked branch deletion shared by worktree removal surfaces.
#[cfg(any(feature = "ui", feature = "host"))]
mod force_delete;
#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) mod registered_heads;
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
    let head_guards = if force {
        let mut guards =
            registered_heads::RegisteredHeads::capture(&service, repository, cancellation)
                .map_err(other)?;
        guards.acquire().map_err(other)?;
        Some(guards)
    } else {
        None
    };
    checked_out()?;
    if force {
        force_delete::delete(
            repository,
            branch.as_str(),
            &full_ref,
            observed.as_str(),
            cancellation,
            head_guards
                .as_ref()
                .expect("force captured registered HEADs"),
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
        let mut command = Command::new("git");
        for (name, _) in
            std::env::vars_os().filter(|(name, _)| name.as_encoded_bytes().starts_with(b"GIT_"))
        {
            command.env_remove(name);
        }
        let output = command
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
                "-c",
                "core.fsmonitor=false",
                "-c",
                "init.templateDir=",
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
/bin/cat > "$CAGENTS_BRANCH_RACE_ROOT/delegate-input.$1"
printf '%s\n' "$1" >> "$CAGENTS_BRANCH_RACE_ROOT/policy-phases"
"##,
                )
                .unwrap();
                fs::set_permissions(&delegate, fs::Permissions::from_mode(0o700)).unwrap();
                let hook = hooks.join("reference-transaction");
                let quoted_delegate =
                    format!("'{}'", delegate.to_str().unwrap().replace('\'', "'\\''"));
                fs::write(
                    &hook,
                    format!("#!/bin/sh\n/bin/cat > \"$CAGENTS_BRANCH_RACE_ROOT/policy-input.$1\"\nexec {quoted_delegate} \"$@\" < \"$CAGENTS_BRANCH_RACE_ROOT/policy-input.$1\"\n"),
                )
                .unwrap();
                hook
            } else {
                let hook = path.join(".git/hooks/reference-transaction");
                fs::create_dir_all(hook.parent().unwrap()).unwrap();
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
                assert_eq!(
                    fs::read(root.join("delegate-input.prepared")).unwrap(),
                    fs::read(root.join("policy-input.prepared")).unwrap()
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
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                "{mode}: child did not execute its owning regression: {output:?}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn registration_fixture(
        root: &Path,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        String,
    ) {
        let repository = root.join("repo");
        fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        fs::write(repository.join("file"), "base").unwrap();
        git(&repository, &["add", "file"]);
        git(&repository, &["commit", "-qm", "base"]);
        git(&repository, &["branch", "topic"]);
        git(
            &repository,
            &["config", "branch.topic.description", "preserve"],
        );
        let y = root.join("Y");
        let z = root.join("Z");
        git(
            &repository,
            &["worktree", "add", "-qb", "y", y.to_str().unwrap()],
        );
        git(
            &repository,
            &["worktree", "add", "-qb", "z", z.to_str().unwrap()],
        );
        let oid = git(&repository, &["rev-parse", "refs/heads/topic"]);
        (repository, y, z, oid)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn registered_head_fences_accept_completed_move_repair_and_bare_repository() {
        let directory = tempfile::tempdir().unwrap();
        let (repository, y, _, oid) = registration_fixture(directory.path());
        let moved = directory.path().join("moved 'checkout\n");
        git(
            &repository,
            &[
                "worktree",
                "move",
                y.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        // Native move and an explicit completed repair remain ordinary supported
        // registrations. The unusual path also exercises byte quoting in proof.
        git(
            &repository,
            &["worktree", "repair", moved.to_str().unwrap()],
        );
        delete_branch_checked_at(
            &repository,
            "topic",
            true,
            Some(&oid),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(git(&moved, &["symbolic-ref", "HEAD"]), "refs/heads/y");
        assert!(git(&repository, &["branch", "--list", "topic"]).is_empty());
        let bare = directory.path().join("bare.git");
        git(
            directory.path(),
            &[
                "clone",
                "--bare",
                repository.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        git(&bare, &["branch", "topic"]);
        let oid = git(&bare, &["rev-parse", "refs/heads/topic"]);
        delete_branch_checked_at(&bare, "topic", true, Some(&oid), &CancellationToken::new())
            .unwrap();
        assert!(git(&bare, &["branch", "--list", "topic"]).is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn registered_head_fences_refuse_inconsistent_forward_and_backlink_records() {
        for mode in ["alias", "missing", "duplicate"] {
            let directory = tempfile::tempdir().unwrap();
            let (repository, y, z, oid) = registration_fixture(directory.path());
            let admin_y = std::path::PathBuf::from(git(&y, &["rev-parse", "--absolute-git-dir"]));
            let admin_z = std::path::PathBuf::from(git(&z, &["rev-parse", "--absolute-git-dir"]));
            let original_y_head = fs::read(admin_y.join("HEAD")).unwrap();
            let original_z_head = fs::read(admin_z.join("HEAD")).unwrap();
            match mode {
                "alias" => fs::copy(z.join(".git"), y.join(".git"))
                    .map(|_| ())
                    .unwrap(),
                "missing" => fs::remove_file(admin_y.join("gitdir")).unwrap(),
                "duplicate" => fs::copy(admin_y.join("gitdir"), admin_z.join("gitdir"))
                    .map(|_| ())
                    .unwrap(),
                _ => unreachable!(),
            }
            let result = delete_branch_checked_at(
                &repository,
                "topic",
                true,
                Some(&oid),
                &CancellationToken::new(),
            );
            assert!(result.is_err(), "{mode}: {result:?}");
            assert_eq!(
                git(&repository, &["rev-parse", "refs/heads/topic"]),
                oid,
                "{mode}"
            );
            assert_eq!(
                git(
                    &repository,
                    &["config", "--get", "branch.topic.description"]
                ),
                "preserve"
            );
            assert_eq!(fs::read(admin_y.join("HEAD")).unwrap(), original_y_head);
            assert_eq!(fs::read(admin_z.join("HEAD")).unwrap(), original_z_head);
            assert!(!admin_y.join("HEAD.lock").exists());
            assert!(!admin_z.join("HEAD.lock").exists());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn registered_head_fences_recheck_at_native_transaction_and_policy_boundary() {
        use std::{
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };
        const MODE: &str = "CAGENTS_REGISTERED_HEAD_MODE";
        if let Ok(mode) = std::env::var(MODE) {
            let root =
                std::path::PathBuf::from(std::env::var_os("CAGENTS_REGISTERED_HEAD_ROOT").unwrap());
            let (repository, y, z, oid) = registration_fixture(&root);
            let admin_y = std::path::PathBuf::from(git(&y, &["rev-parse", "--absolute-git-dir"]));
            let admin_z = std::path::PathBuf::from(git(&z, &["rev-parse", "--absolute-git-dir"]));
            fs::write(root.join("admin-y"), admin_y.as_os_str().as_encoded_bytes()).unwrap();
            fs::write(root.join("admin-z"), admin_z.as_os_str().as_encoded_bytes()).unwrap();
            let hook = repository.join(".git/hooks/reference-transaction");
            fs::create_dir_all(hook.parent().unwrap()).unwrap();
            fs::write(&hook, r##"#!/bin/sh
set -eu
/bin/cat >/dev/null
[ "$1" = prepared ] || exit 0
printf prepared >> "$CAGENTS_REGISTERED_HEAD_ROOT/policy-ran"
if [ "$CAGENTS_REGISTERED_HEAD_MODE" = checkout ]; then
  admin=$(/bin/cat "$CAGENTS_REGISTERED_HEAD_ROOT/admin-y")
  if /usr/bin/git --git-dir="$admin" --work-tree="$CAGENTS_REGISTERED_HEAD_ROOT/Y" symbolic-ref HEAD refs/heads/topic; then
    printf switched > "$CAGENTS_REGISTERED_HEAD_ROOT/checkout-result"
  else
    printf refused > "$CAGENTS_REGISTERED_HEAD_ROOT/checkout-result"
  fi
elif [ "$CAGENTS_REGISTERED_HEAD_MODE" = registration ]; then
  /usr/bin/git -c core.hooksPath=/dev/null -C "$CAGENTS_REGISTERED_HEAD_ROOT/repo" worktree add --detach "$CAGENTS_REGISTERED_HEAD_ROOT/new" HEAD
fi
"##).unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
            let result = delete_branch_checked_at(
                &repository,
                "topic",
                true,
                Some(&oid),
                &CancellationToken::new(),
            );
            if mode == "checkout" {
                result.unwrap();
                assert_eq!(fs::read(root.join("checkout-result")).unwrap(), b"refused");
                assert_eq!(
                    fs::read(admin_y.join("HEAD")).unwrap(),
                    b"ref: refs/heads/y\n"
                );
                assert!(git(&repository, &["branch", "--list", "topic"]).is_empty());
            } else {
                assert!(result.is_err(), "{mode}: {result:?}");
                assert_eq!(git(&repository, &["rev-parse", "refs/heads/topic"]), oid);
                assert_eq!(
                    git(
                        &repository,
                        &["config", "--get", "branch.topic.description"]
                    ),
                    "preserve"
                );
                if mode == "registration" {
                    assert!(root.join("policy-ran").exists());
                    assert_eq!(git(&root.join("new"), &["rev-parse", "HEAD"]), oid);
                } else {
                    // The final native launch raced capture, so the prepared
                    // proof must refuse before delegating repository policy.
                    assert!(!root.join("policy-ran").exists());
                }
                if mode == "admin" {
                    assert_eq!(
                        fs::read(admin_y.join("foreign")).unwrap(),
                        b"foreign registration"
                    );
                    assert!(!admin_y.join("HEAD.lock").exists());
                    assert!(!admin_y.with_extension("old").join("HEAD.lock").exists());
                }
            }
            return;
        }
        for mode in ["alias", "admin", "checkout", "registration"] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let root = directory.path();
            fs::create_dir(root.join("bin")).unwrap();
            let wrapper = root.join("bin/git");
            fs::write(
                &wrapper,
                r##"#!/bin/sh
set -eu
case " $* " in
  *' branch -D -- topic '*)
    if [ "$CAGENTS_REGISTERED_HEAD_MODE" = alias ]; then
      /bin/cp "$CAGENTS_REGISTERED_HEAD_ROOT/Z/.git" "$CAGENTS_REGISTERED_HEAD_ROOT/Y/.git"
    elif [ "$CAGENTS_REGISTERED_HEAD_MODE" = admin ]; then
      admin=$(/bin/cat "$CAGENTS_REGISTERED_HEAD_ROOT/admin-y")
      /bin/mv "$admin" "$admin.old"
      /bin/mkdir "$admin"
      printf 'foreign registration' > "$admin/foreign"
    fi
    ;;
esac
exec /usr/bin/git "$@"
"##,
            )
            .unwrap();
            fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
            let mut path = std::ffi::OsString::from(root.join("bin"));
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "worktree_branch::branch_delete_tests::registered_head_fences_recheck_at_native_transaction_and_policy_boundary", "--nocapture"])
                .env(MODE, mode).env("CAGENTS_REGISTERED_HEAD_ROOT", root).env("PATH", path)
                .env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null")
                .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!(
                        "registration race child timed out: {:?}",
                        child.wait_with_output()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                "{mode}: child did not execute its owning regression: {output:?}"
            );
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn registered_head_fences_resolve_configured_separate_primary_checkout() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(
            root,
            &[
                "init",
                "-q",
                "-b",
                "main",
                "--separate-git-dir",
                "metadata",
                "checkout",
            ],
        );
        let checkout = root.join("checkout");
        let metadata = root.join("metadata");
        git(&checkout, &["config", "core.worktree", "../checkout"]);
        fs::write(checkout.join("file"), "base").unwrap();
        git(&checkout, &["add", "file"]);
        git(&checkout, &["commit", "-qm", "base"]);
        let linked = root.join("linked");
        git(
            &checkout,
            &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
        );
        // Both metadata and a linked invocation must resolve the primary from
        // the original common config, then prove its locator back to common.
        for invocation in [&checkout, &metadata, &linked] {
            git(&checkout, &["branch", "topic"]);
            let oid = git(&checkout, &["rev-parse", "refs/heads/topic"]);
            delete_branch_checked_at(
                invocation,
                "topic",
                true,
                Some(&oid),
                &CancellationToken::new(),
            )
            .unwrap();
            assert!(git(&checkout, &["branch", "--list", "topic"]).is_empty());
            assert_eq!(git(&checkout, &["symbolic-ref", "HEAD"]), "refs/heads/main");
            assert_eq!(git(&linked, &["symbolic-ref", "HEAD"]), "refs/heads/linked");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn registered_head_fences_accept_separate_checkout_without_configured_backlink() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(
            root,
            &[
                "init",
                "-q",
                "-b",
                "main",
                "--separate-git-dir",
                "metadata",
                "checkout",
            ],
        );
        let checkout = root.join("checkout");
        let metadata = root.join("metadata");
        fs::write(checkout.join("file"), "base").unwrap();
        git(&checkout, &["add", "file"]);
        git(&checkout, &["commit", "-qm", "base"]);
        git(&checkout, &["branch", "topic"]);
        let oid = git(&checkout, &["rev-parse", "refs/heads/topic"]);
        delete_branch_checked_at(
            &checkout,
            "topic",
            true,
            Some(&oid),
            &CancellationToken::new(),
        )
        .unwrap();
        git(&checkout, &["branch", "topic"]);
        let unknown = delete_branch_checked_at(
            &metadata,
            "topic",
            true,
            Some(&oid),
            &CancellationToken::new(),
        );
        assert!(
            unknown.is_err(),
            "metadata without a primary backlink must not guess a checkout"
        );
        assert_eq!(git(&checkout, &["rev-parse", "refs/heads/topic"]), oid);
        assert_eq!(git(&checkout, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    }
}
