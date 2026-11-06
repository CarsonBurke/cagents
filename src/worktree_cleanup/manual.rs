//! A conservative guard for deletion surfaces without granular discard consent.
//!
//! A generic confirmation never authorizes destroying local files or an
//! unreferenced HEAD. Callers inspect before stopping sessions, then verify
//! again immediately before running the non-forcing Git command.
use std::{
    ffi::OsStr,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    process::{
        CancellationToken, CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput,
        SystemProcessExecutor,
    },
    repository::{Oid, RepositoryService},
};

use super::RemovedWorktree;

const CHANGED: &str = "The worktree changed while preparing deletion; review it and retry.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManualRemoval {
    repository: PathBuf,
    path: PathBuf,
    common_dir: PathBuf,
    admin_dir: PathBuf,
    branch: Option<String>,
    head: String,
    directory_identity: (u64, u64),
    common_identity: (u64, u64),
    admin_identity: (u64, u64),
    unpushed: u64,
}

impl ManualRemoval {
    /// Reads actual repository state, independently of sidebar caches. All
    /// commands are bounded; warnings and incomplete reads preserve the tree.
    pub(crate) fn capture(
        repository: &Path,
        path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Self, String> {
        for directory in [repository, path] {
            if !directory.is_absolute() {
                return Err("Worktree deletion requires absolute paths.".into());
            }
            crate::secure_file::reject_symlink_components(directory)
                .map_err(|error| error.to_string())?;
            if directory
                .canonicalize()
                .map_err(|error| error.to_string())?
                != directory
            {
                return Err(CHANGED.into());
            }
        }
        let service = RepositoryService::new("git", Arc::new(SafetyExecutor));
        let records = service
            .list_worktrees(repository, cancellation)
            .map_err(|error| error.to_string())?;
        let record = records
            .iter()
            .find(|record| record.path == path)
            .ok_or("The worktree is no longer registered; refresh before deleting.")?;
        if records.first().is_some_and(|primary| primary.path == path) || record.bare {
            return Err("The primary worktree cannot be deleted.".into());
        }
        if record.lock.locked {
            return Err("The worktree is locked; unlock it explicitly in Git first.".into());
        }
        if record.prunable_reason.is_some() {
            return Err("The worktree is unavailable; repair it in Git before deleting.".into());
        }
        if records
            .iter()
            .any(|record| record.path != path && record.path.starts_with(path))
        {
            return Err(
                "Another registered worktree is inside this one; keep it before deleting.".into(),
            );
        }
        let common_dir = service
            .common_dir(repository, cancellation)
            .map_err(|error| error.to_string())?;
        if service
            .common_dir(path, cancellation)
            .map_err(|error| error.to_string())?
            != common_dir
        {
            return Err("The worktree's repository changed; refresh before deleting.".into());
        }
        let admin_dir = service
            .git_dir(path, cancellation)
            .map_err(|error| error.to_string())?;
        // core.worktree (including config.worktree) can redirect all file
        // inventories while Git still removes the registered directory.
        let top_level = read(
            path,
            ["rev-parse", "--path-format=absolute", "--show-toplevel"],
            cancellation,
        )?;
        if top_level.strip_suffix(b"\n") != Some(path.as_os_str().as_bytes()) {
            return Err(
                "Git is inspecting a different worktree root; the directory was kept.".into(),
            );
        }
        let directory_identity = identity(path)?;
        let common_identity = identity(&common_dir)?;
        let admin_identity = identity(&admin_dir)?;
        let head = record
            .head_oid
            .as_deref()
            .ok_or("The worktree has no HEAD commit; keep it before deleting.")?;
        Oid::parse_object(head).map_err(|error| error.to_string())?;

        // A branch or HEAD may have changed after `worktree list` returned.
        let actual_head = read(
            path,
            ["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"],
            cancellation,
        )?;
        if actual_head.trim_ascii_end() != head.as_bytes() {
            return Err(CHANGED.into());
        }
        let symbolic = output(
            &read_command(path).args(["symbolic-ref", "-q", "HEAD"]),
            cancellation,
        )?;
        let branch = match symbolic.exit_code {
            Some(0) => Some(
                std::str::from_utf8(symbolic.stdout.trim_ascii_end())
                    .map_err(|_| "Git returned an invalid worktree branch.".to_owned())?
                    .to_owned(),
            ),
            Some(1) if symbolic.stdout.is_empty() => None,
            _ => return Err("Git could not determine the worktree's branch; it was kept.".into()),
        };
        if branch != record.branch || record.detached != branch.is_none() {
            return Err(CHANGED.into());
        }
        if let Some(branch) = &branch {
            if !branch.starts_with("refs/heads/") {
                return Err(
                    "The worktree HEAD is not a local branch; keep it before deleting.".into(),
                );
            }
            let branch_commit = read(
                path,
                [
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{branch}^{{commit}}"),
                ],
                cancellation,
            )?;
            if branch_commit.trim_ascii_end() != head.as_bytes() {
                return Err(CHANGED.into());
            }
        }

        for operation in [
            "MERGE_HEAD",
            "rebase-merge",
            "rebase-apply",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
            "sequencer",
        ] {
            if admin_dir.join(operation).symlink_metadata().is_ok() {
                return Err(
                    "A Git operation is in progress; finish it before deleting the worktree."
                        .into(),
                );
            }
        }
        if !read(
            path,
            [
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "refs/worktree/",
                "refs/bisect/",
                "refs/rewritten/",
            ],
            cancellation,
        )?
        .is_empty()
        {
            return Err("The worktree has local refs that deletion would lose; keep them in a branch first.".into());
        }
        // Status deliberately trusts these index flags. A generic deletion
        // must not trust them to prove that present files have no local edits.
        for entry in read(path, ["ls-files", "-v", "-z"], cancellation)?
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            if entry.len() < 3 || entry[1] != b' ' {
                return Err(
                    "Git returned an incomplete tracked-file inventory; the worktree was kept."
                        .into(),
                );
            }
            let tag = entry[0];
            let skip_present = if tag == b'S' {
                match path.join(OsStr::from_bytes(&entry[2..])).symlink_metadata() {
                    Ok(_) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error.to_string()),
                }
            } else {
                false
            };
            if tag.is_ascii_lowercase() || skip_present {
                return Err("The worktree has files hidden by assume-unchanged or skip-worktree; clear those flags before deleting.".into());
            }
        }
        let status = service
            .status_for_removal(path, cancellation)
            .map_err(|error| error.to_string())?;
        if status.untracked_omitted || status.dirty.is_dirty() || !status.entries.is_empty() {
            return Err("The worktree has uncommitted changes or untracked files; commit or move them before deleting.".into());
        }
        // Inspect with case folding disabled too: a repository moved from a
        // case-insensitive disk can otherwise hide a distinct local file.
        if !read(
            path,
            [
                "ls-files",
                "--others",
                "--exclude-standard",
                "--directory",
                "--no-empty-directory",
                "-z",
            ],
            cancellation,
        )?
        .is_empty()
        {
            return Err(
                "The worktree has untracked files; commit or move them before deleting.".into(),
            );
        }
        // Even ignored build directories are preserved here: this deletion
        // surface has no separate consent to discard particular ignored data.
        let ignored = read(
            path,
            [
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "--no-empty-directory",
                "-z",
            ],
            cancellation,
        )?;
        if !ignored.is_empty() {
            return Err("The worktree has ignored files, which can include .env or local data; move them before deleting.".into());
        }
        // Removing the administrative directory also deletes this worktree's
        // HEAD reflog. A detached commit can survive only there after switching
        // back to a branch or resetting HEAD, so judge those commits as well.
        let reflog = read(
            path,
            ["reflog", "show", "--format=%H", "HEAD"],
            cancellation,
        )?;
        let detached_only = count(
            path,
            head,
            &["--branches", "--remotes", "--tags"],
            reflog,
            cancellation,
        )?;
        if detached_only != 0 {
            return Err(format!(
                "The worktree has {detached_only} commit(s) on no branch, remote or tag; create a branch before deleting."
            ));
        }
        let unpushed = count(path, head, &["--remotes"], Vec::new(), cancellation)?;
        Ok(Self {
            repository: repository.to_owned(),
            path: path.to_owned(),
            common_dir,
            admin_dir,
            branch,
            head: head.to_owned(),
            directory_identity,
            common_identity,
            admin_identity,
            unpushed,
        })
    }

    pub(crate) fn verify(&self, cancellation: &CancellationToken) -> Result<(), String> {
        let current = Self::capture(&self.repository, &self.path, cancellation)?;
        if current == *self {
            Ok(())
        } else {
            Err(CHANGED.into())
        }
    }

    pub(crate) fn removed(&self) -> RemovedWorktree {
        RemovedWorktree {
            common_dir: self.common_dir.clone(),
            admin_dir: self.admin_dir.clone(),
            path: self.path.clone(),
            branch: self.branch.clone(),
        }
    }

    pub(crate) fn command(&self) -> CommandSpec {
        git_command(&self.repository)
            .args(["worktree", "remove", "--"])
            .arg(self.path.as_os_str())
            .timeout(Duration::from_secs(30))
            .output_limits(64 * 1024, 64 * 1024)
    }
}

fn identity(path: &Path) -> Result<(u64, u64), String> {
    let metadata = path.symlink_metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir() {
        return Err("The worktree or its Git directory changed; it was kept.".into());
    }
    Ok((metadata.dev(), metadata.ino()))
}

/// Apply the same environment policy to RepositoryService's complete status
/// helper and to the direct reads and final removal command.
struct SafetyExecutor;

impl ProcessExecutor for SafetyExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        SystemProcessExecutor.execute(&safe_environment(command.clone()), cancellation)
    }
}

fn safe_environment(mut command: CommandSpec) -> CommandSpec {
    command = command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1");
    for name in [
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_ATTR_SOURCE",
        "GIT_REPLACE_REF_BASE",
        "GIT_SHALLOW_FILE",
        "GIT_QUARANTINE_PATH",
        "GIT_LITERAL_PATHSPECS",
        "GIT_GLOB_PATHSPECS",
        "GIT_NOGLOB_PATHSPECS",
        "GIT_ICASE_PATHSPECS",
        "GIT_TEST_SPLIT_INDEX",
        "GIT_LFS_SKIP_SMUDGE",
    ] {
        command = command.env_remove(name);
    }
    command
}

fn git_command(path: &Path) -> CommandSpec {
    safe_environment(RepositoryService::system("git").git_base(path, false))
        .timeout(Duration::from_secs(30))
        .output_limits(4 * 1024 * 1024, 64 * 1024)
}

fn read_command(path: &Path) -> CommandSpec {
    git_command(path)
        .args(["--no-optional-locks", "-c", "core.ignoreCase=false"])
        .env("GIT_OPTIONAL_LOCKS", "0")
}

fn output(
    command: &CommandSpec,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, String> {
    let output = SystemProcessExecutor
        .execute(command, cancellation)
        .map_err(|error| error.reported_detail())?;
    if output.stderr.iter().any(|byte| !byte.is_ascii_whitespace()) {
        return Err("Git could not inspect every file or commit; the worktree was kept.".into());
    }
    Ok(output)
}

fn read<const N: usize>(
    path: &Path,
    arguments: [&str; N],
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, String> {
    let command = read_command(path).args(arguments);
    output(&command, cancellation)?
        .require_success(&command.program)
        .map(|output| output.stdout)
        .map_err(|error| error.reported_detail())
}

fn count(
    path: &Path,
    head: &str,
    excluded: &[&str],
    reflog: Vec<u8>,
    cancellation: &CancellationToken,
) -> Result<u64, String> {
    let command = read_command(path)
        .args(["rev-list", "--count", head, "--stdin", "--not"])
        .args(excluded.iter().copied())
        .stdin(reflog);
    let output = output(&command, cancellation)?
        .require_success(&command.program)
        .map_err(|error| error.reported_detail())?;
    std::str::from_utf8(output.stdout.trim_ascii_end())
        .ok()
        .and_then(|count| count.parse().ok())
        .ok_or_else(|| "Git returned an incomplete commit count; the worktree was kept.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        repository: PathBuf,
        linked: PathBuf,
    }

    fn git(path: &Path, arguments: &[&str]) -> String {
        let command = RepositoryService::system("git")
            .git_base(path, false)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(arguments.iter().copied());
        String::from_utf8(
            SystemProcessExecutor
                .execute(&command, &CancellationToken::new())
                .unwrap()
                .require_success(&command.program)
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned()
    }

    impl Fixture {
        fn new() -> Self {
            let directory = crate::repository::retained_managed_fixture_directory();
            let repository = directory.path().join("repository");
            let linked = directory.path().join("linked");
            git(directory.path(), &["init", "-b", "main", "repository"]);
            std::fs::write(repository.join("tracked"), "initial\n").unwrap();
            std::fs::write(repository.join(".gitignore"), ".env\nlocal/\ntarget/\n").unwrap();
            git(&repository, &["add", "."]);
            git(&repository, &["commit", "-m", "initial"]);
            git(
                &repository,
                &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
            );
            Self {
                _directory: directory,
                repository,
                linked,
            }
        }

        fn capture(&self) -> Result<ManualRemoval, String> {
            ManualRemoval::capture(&self.repository, &self.linked, &CancellationToken::new())
        }

        fn remove(&self) -> ManualRemoval {
            let removal = self.capture().unwrap();
            removal.verify(&CancellationToken::new()).unwrap();
            let command = removal.command();
            assert!(
                !command
                    .arguments
                    .iter()
                    .any(|argument| argument == "--force")
            );
            SystemProcessExecutor
                .execute(&command, &CancellationToken::new())
                .unwrap()
                .require_success(&command.program)
                .unwrap();
            assert!(!self.linked.exists());
            removal
        }
    }

    #[test]
    fn manual_deletion_preserves_every_kind_of_uncommitted_file() {
        if crate::repository::isolated_git_test(concat!(
            module_path!(),
            "::manual_deletion_preserves_every_kind_of_uncommitted_file"
        )) {
            return;
        }
        let fixture = Fixture::new();
        for relative in [
            "untracked",
            ".env",
            "local/nested/data.db",
            "target/nested/data",
        ] {
            let file = fixture.linked.join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "preserve\n").unwrap();
            assert!(fixture.capture().is_err(), "{relative}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "preserve\n");
            std::fs::remove_file(file).unwrap();
        }
        std::fs::write(fixture.linked.join("tracked"), "modified\n").unwrap();
        assert!(fixture.capture().unwrap_err().contains("uncommitted"));
        git(&fixture.linked, &["add", "tracked"]);
        assert!(fixture.capture().unwrap_err().contains("uncommitted"));
        git(&fixture.linked, &["reset", "--hard", "HEAD"]);
        fixture.remove();
    }

    #[test]
    fn manual_deletion_keeps_named_unpushed_commits_and_rejects_detached_only_work() {
        if crate::repository::isolated_git_test(concat!(
            module_path!(),
            "::manual_deletion_keeps_named_unpushed_commits_and_rejects_detached_only_work"
        )) {
            return;
        }
        let fixture = Fixture::new();
        git(
            &fixture.linked,
            &["commit", "--allow-empty", "-m", "local work"],
        );
        let head = git(&fixture.linked, &["rev-parse", "HEAD"]);
        let removal = fixture.remove();
        assert!(removal.unpushed > 0);
        assert_eq!(git(&fixture.repository, &["rev-parse", "feature"]), head);
        assert_eq!(
            removal.removed().branch.as_deref(),
            Some("refs/heads/feature")
        );

        let fixture = Fixture::new();
        git(&fixture.linked, &["checkout", "--detach"]);
        fixture.remove(); // A local branch still reaches this HEAD.

        let fixture = Fixture::new();
        git(&fixture.linked, &["checkout", "--detach"]);
        git(
            &fixture.linked,
            &["commit", "--allow-empty", "-m", "detached work"],
        );
        assert!(fixture.capture().unwrap_err().contains("1 commit(s)"));
        assert!(fixture.linked.is_dir());

        let detached_head = git(&fixture.linked, &["rev-parse", "HEAD"]);
        git(&fixture.linked, &["switch", "feature"]);
        assert!(fixture.capture().unwrap_err().contains("1 commit(s)"));
        // A rescue ref preserves the reflog-only work and permits removal.
        git(&fixture.linked, &["branch", "rescue", &detached_head]);
        fixture.remove();
        assert_eq!(
            git(&fixture.repository, &["rev-parse", "rescue"]),
            detached_head
        );
    }

    #[test]
    fn manual_deletion_verifies_changes_and_git_refuses_a_late_untracked_file() {
        if crate::repository::isolated_git_test(concat!(
            module_path!(),
            "::manual_deletion_verifies_changes_and_git_refuses_a_late_untracked_file"
        )) {
            return;
        }
        let fixture = Fixture::new();
        let removal = fixture.capture().unwrap();
        std::fs::write(fixture.linked.join(".env"), "preserve\n").unwrap();
        assert!(
            removal
                .verify(&CancellationToken::new())
                .unwrap_err()
                .contains("ignored")
        );
        std::fs::remove_file(fixture.linked.join(".env")).unwrap();
        git(
            &fixture.linked,
            &["commit", "--allow-empty", "-m", "new head"],
        );
        assert_eq!(
            removal.verify(&CancellationToken::new()).unwrap_err(),
            CHANGED
        );
        let removal = fixture.capture().unwrap();
        removal.verify(&CancellationToken::new()).unwrap();
        std::fs::write(fixture.linked.join("late"), "preserve\n").unwrap();
        let command = removal.command();
        assert!(
            !SystemProcessExecutor
                .execute(&command, &CancellationToken::new())
                .unwrap()
                .success
        );
        assert_eq!(
            std::fs::read_to_string(fixture.linked.join("late")).unwrap(),
            "preserve\n"
        );
    }

    #[test]
    fn manual_deletion_preserves_edits_hidden_from_git_status() {
        if crate::repository::isolated_git_test(concat!(
            module_path!(),
            "::manual_deletion_preserves_edits_hidden_from_git_status"
        )) {
            return;
        }
        let fixture = Fixture::new();
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            git(&fixture.linked, &["update-index", flag, "tracked"]);
            std::fs::write(fixture.linked.join("tracked"), "hidden edit\n").unwrap();
            assert_eq!(git(&fixture.linked, &["status", "--porcelain"]), "");
            assert!(fixture.capture().unwrap_err().contains("hidden"));
            assert_eq!(
                std::fs::read_to_string(fixture.linked.join("tracked")).unwrap(),
                "hidden edit\n"
            );
            git(
                &fixture.linked,
                &[
                    "update-index",
                    "--no-assume-unchanged",
                    "--no-skip-worktree",
                    "tracked",
                ],
            );
            git(&fixture.linked, &["restore", "tracked"]);
        }
    }

    #[test]
    fn manual_deletion_keeps_files_when_a_safety_read_is_incomplete() {
        if crate::repository::isolated_git_test(concat!(
            module_path!(),
            "::manual_deletion_keeps_files_when_a_safety_read_is_incomplete"
        )) {
            return;
        }
        let fixture = Fixture::new();
        let command = read_command(&fixture.linked)
            .args(["ls-files", "-z"])
            .output_limits(1, 64 * 1024);
        assert!(output(&command, &CancellationToken::new()).is_err());
        assert_eq!(
            std::fs::read_to_string(fixture.linked.join("tracked")).unwrap(),
            "initial\n"
        );
        git(&fixture.linked, &["config", "core.ignoreCase", "true"]);
        std::fs::write(fixture.linked.join("TRACKED"), "distinct local data\n").unwrap();
        assert!(fixture.capture().unwrap_err().contains("untracked"));
        assert_eq!(
            std::fs::read_to_string(fixture.linked.join("TRACKED")).unwrap(),
            "distinct local data\n"
        );
    }

    #[test]
    fn manual_deletion_refuses_redirected_worktree_inventory() {
        if crate::repository::isolated_git_test(concat!(
            module_path!(),
            "::manual_deletion_refuses_redirected_worktree_inventory"
        )) {
            return;
        }
        let fixture = Fixture::new();
        let outside = fixture.repository.parent().unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        for file in ["tracked", ".gitignore"] {
            std::fs::copy(fixture.repository.join(file), outside.join(file)).unwrap();
        }
        std::fs::write(fixture.linked.join(".env"), "preserve\n").unwrap();
        git(
            &fixture.repository,
            &["config", "extensions.worktreeConfig", "true"],
        );
        git(
            &fixture.linked,
            &[
                "config",
                "--worktree",
                "core.worktree",
                outside.to_str().unwrap(),
            ],
        );
        assert_eq!(git(&fixture.linked, &["status", "--porcelain"]), "");
        assert!(
            fixture
                .capture()
                .unwrap_err()
                .contains("different worktree root")
        );
        assert_eq!(
            std::fs::read(fixture.linked.join(".env")).unwrap(),
            b"preserve\n"
        );
    }
}
