//! Fresh worktree deletion evidence shared by manual and automatic removal.
use super::*;
use std::collections::HashSet;

const DISPOSABLE_CONFIG: &str = "cagents.cleanupDisposableDirectories";

fn require_complete_inventory(output: &ProcessOutput) -> Result<(), RepositoryError> {
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

pub fn default_disposable_ignored_directories() -> Vec<String> {
    [
        "node_modules",
        "target",
        "dist",
        "build",
        ".venv",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Names refer only to directories, at any nesting depth; an ignored file with
/// the same name is user data. Invalid configuration fails closed.
fn validate_disposable_directories(names: &[String]) -> Result<(), ValidationError> {
    if names.iter().any(|name| {
        name.is_empty()
            || matches!(name.as_str(), "." | "..")
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
    }) {
        return Err(ValidationError::Io(
            "Disposable ignored directories must be simple directory names".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSafetySnapshot {
    /// Detects changed paths and file identities even when counts are unchanged.
    pub fingerprint: String,
    pub modified: usize,
    pub untracked: usize,
    pub notable_ignored: Vec<RepoPath>,
    pub unpushed_commits: u64,
    pub detached_only_commits: u64,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
}

impl WorktreeSafetySnapshot {
    pub fn files_to_discard(&self) -> usize {
        self.modified + self.untracked + self.notable_ignored.len()
    }
    pub fn requires_confirmation(&self) -> bool {
        self.files_to_discard() != 0
            || self.unpushed_commits != 0
            || self.detached_only_commits != 0
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// Safety decisions require a complete census, even when Git returns
    /// success while warning that a directory could not be read.
    pub(crate) fn status_for_removal(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<StatusSnapshot, RepositoryError> {
        let output = self.run_checked(&self.status_command(worktree), cancellation)?;
        require_complete_inventory(&output)?;
        invalid_output(parse_status(&output.stdout), output)
    }

    pub fn disposable_ignored_directories(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>, RepositoryError> {
        let command = self.git_read(worktree, ["config", "--local", "--get", DISPOSABLE_CONFIG]);
        let output = self.run(&command, cancellation)?;
        if output.exit_code == Some(1) {
            return Ok(default_disposable_ignored_directories());
        }
        if !output.success {
            return Err(git_failure(&command, output).into());
        }
        let names: Vec<String> = serde_json::from_slice(&output.stdout).map_err(|error| {
            ValidationError::Io(format!(
                "Invalid disposable ignored directory configuration: {error}"
            ))
        })?;
        validate_disposable_directories(&names)?;
        Ok(names)
    }

    /// Stored per clone, shared by its linked worktrees without changing tracked files.
    pub fn set_disposable_ignored_directories(
        &self,
        worktree: &Path,
        names: &[String],
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        validate_disposable_directories(names)?;
        let value =
            serde_json::to_string(names).map_err(|error| ValidationError::Io(error.to_string()))?;
        self.run_checked(
            &self.git_mutation(worktree, ["config", "--local", DISPOSABLE_CONFIG, &value]),
            cancellation,
        )?;
        Ok(())
    }

    /// Enumerate ignored files, never collapsed directories: matching a known
    /// build directory must not accidentally permit a similarly named file.
    pub fn notable_ignored_files(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RepoPath>, RepositoryError> {
        let directories = self.disposable_ignored_directories(worktree, cancellation)?;
        let names: HashSet<_> = directories.iter().map(OsStr::new).collect();
        let disposable = |path: &RepoPath, directory: bool| {
            let path = Path::new(path.as_os_str());
            let directory = if directory { Some(path) } else { path.parent() };
            directory
                .into_iter()
                .flat_map(Path::components)
                .any(|component| names.contains(component.as_os_str()))
        };
        // Collapse wholly ignored directories first, so target/node_modules
        // need not enumerate hundreds of thousands of disposable artifacts.
        let command = self.git_read(
            worktree,
            [
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "--no-empty-directory",
                "-z",
            ],
        );
        let output = self.run_checked(&command, cancellation)?;
        require_complete_inventory(&output)?;
        let mut notable = std::collections::BTreeSet::new();
        let mut directories = Vec::new();
        for (index, raw) in nul_fields(&output.stdout, "ignored worktree files")?
            .into_iter()
            .filter(|raw| !raw.is_empty())
            .enumerate()
        {
            let directory = raw.ends_with(b"/");
            let path = repo_path_field(raw, "ignored worktree files", index)?;
            if disposable(&path, directory) {
                continue;
            }
            if directory {
                directories.push(path);
            } else {
                notable.insert(path);
            }
        }
        // Expand only non-disposable directories, keeping exact file counts
        // and fingerprints rather than treating a whole folder as one file.
        // Small batches also keep ignored-prefix queries below argv limits.
        for batch in directories.chunks(64) {
            let mut arguments: Vec<OsString> = [
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "-z",
                "--",
            ]
            .into_iter()
            .map(OsString::from)
            .collect();
            arguments.extend(batch.iter().map(|path| path.as_os_str().to_owned()));
            let output =
                self.run_checked(&self.git_literal_read(worktree, arguments), cancellation)?;
            require_complete_inventory(&output)?;
            for (index, raw) in nul_fields(&output.stdout, "ignored worktree files")?
                .into_iter()
                .filter(|raw| !raw.is_empty())
                .enumerate()
            {
                let directory = raw.ends_with(b"/");
                let path = repo_path_field(raw, "ignored worktree files", index)?;
                if disposable(&path, directory) {
                    continue;
                }
                if directory {
                    return Err(RepositoryError::InvalidOutput {
                        field: "worktree_safety",
                        reason: format!(
                            "Cannot inspect all files inside ignored directory {path}; worktree was kept"
                        ),
                    });
                }
                notable.insert(path);
            }
        }
        Ok(notable.into_iter().collect())
    }

    /// Call on a worker at dialog open and again immediately before removal.
    /// No cached sidebar count can authorize losing work.
    pub fn worktree_safety(
        &self,
        worktree: &Path,
        cancellation: &CancellationToken,
    ) -> Result<WorktreeSafetySnapshot, RepositoryError> {
        let status = self.status_for_removal(worktree, cancellation)?;
        let modified = status
            .entries
            .iter()
            .filter(|entry| entry.area != ChangeArea::Untracked)
            .map(|entry| &entry.path)
            .collect::<HashSet<_>>()
            .len();
        let head = self.resolve_head(worktree, cancellation)?;
        let count = |detached: bool| -> Result<u64, RepositoryError> {
            if head.is_none() {
                return Ok(0);
            }
            let mut command = self.git_read(
                worktree,
                ["rev-list", "--count", "HEAD", "--not", "--remotes"],
            );
            if detached {
                command = command.arg("--branches");
            }
            let output = self.run_checked(&command, cancellation)?;
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse()
                .map_err(|_| RepositoryError::InvalidOutput {
                    field: "worktree_commit_count",
                    reason: "Git did not return a commit count".into(),
                })
        };
        let notable_ignored = self.notable_ignored_files(worktree, cancellation)?;
        let mut fingerprint = sha2::Sha256::new();
        use sha2::Digest;
        use std::os::unix::fs::MetadataExt;
        fingerprint.update(
            serde_json::to_vec(&status).map_err(|error| ValidationError::Io(error.to_string()))?,
        );
        let paths: std::collections::BTreeSet<_> = status
            .entries
            .iter()
            .map(|entry| &entry.path)
            .chain(notable_ignored.iter())
            .collect();
        let root = fs_safety::Root::open(worktree)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        for path in paths {
            use std::os::unix::ffi::OsStrExt;
            fingerprint.update(path.as_os_str().as_bytes());
            fingerprint.update([0]);
            match fs_safety::metadata_beneath(&root, Path::new(path.as_os_str())) {
                Ok(metadata) => {
                    // Git reports an untracked nested repository as one
                    // directory. Its own timestamps cannot detect edits to
                    // existing descendants, so it cannot authorize deletion.
                    if metadata.is_dir() {
                        return Err(RepositoryError::InvalidOutput {
                            field: "worktree_safety",
                            reason: format!(
                                "Cannot inspect all files inside directory {path}; worktree was kept"
                            ),
                        });
                    }
                    for value in [
                        metadata.dev(),
                        metadata.ino(),
                        metadata.len(),
                        metadata.mtime() as u64,
                        metadata.mtime_nsec() as u64,
                        metadata.ctime() as u64,
                        metadata.ctime_nsec() as u64,
                    ] {
                        fingerprint.update(value.to_le_bytes());
                    }
                }
                Err(error) if error.is_not_found() => fingerprint.update(b"missing"),
                Err(error) => return Err(ValidationError::Io(error.to_string()).into()),
            }
        }
        Ok(WorktreeSafetySnapshot {
            fingerprint: format!("{:x}", fingerprint.finalize()),
            modified,
            untracked: status
                .entries
                .iter()
                .filter(|entry| entry.area == ChangeArea::Untracked)
                .count(),
            notable_ignored,
            unpushed_commits: count(false)?,
            detached_only_commits: if status.branch().is_none() {
                count(true)?
            } else {
                0
            },
            branch: status.branch().map(str::to_owned),
            head_oid: head.map(|oid| oid.to_string()),
        })
    }

    /// Rescue the exact observed detached commit before deleting its worktree.
    /// Existing branches are never overwritten, including a same-named rescue.
    pub fn rescue_detached_worktree(
        &self,
        worktree: &Path,
        observed: &WorktreeSafetySnapshot,
        cancellation: &CancellationToken,
    ) -> Result<String, RepositoryError> {
        let head = observed.head_oid.as_deref().ok_or_else(|| {
            ValidationError::InvalidBase("Detached worktree has no commit".into())
        })?;
        let current = self.worktree_safety(worktree, cancellation)?;
        if &current != observed || current.branch.is_some() {
            return Err(ValidationError::InvalidBase(
                "Worktree contents or HEAD changed; review its safety again".into(),
            )
            .into());
        }
        let branch = format!("rescue/{}", &head[..12.min(head.len())]);
        self.run_checked(
            &self.git_mutation(worktree, ["branch", "--", &branch, head]),
            cancellation,
        )?;
        Ok(branch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn ignored_user_data_blocks_but_disposable_directories_and_nested_files_do_not() {
        let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = fixture.path();
        git(root, &["init", "-b", "main"]);
        fs::write(
            root.join(".gitignore"),
            ".env\nnode_modules\ntarget/\ncache/\n",
        )
        .unwrap();
        git(root, &["add", ".gitignore"]);
        git(root, &["commit", "-m", "initial"]);
        fs::create_dir_all(root.join("target/nested")).unwrap();
        fs::write(root.join("target/nested/output"), "generated").unwrap();
        let service = RepositoryService::system("git");
        let cancellation = CancellationToken::new();
        assert!(
            service
                .notable_ignored_files(root, &cancellation)
                .unwrap()
                .is_empty()
        );
        fs::write(root.join(".env"), "secret").unwrap();
        assert_eq!(
            service.notable_ignored_files(root, &cancellation).unwrap(),
            vec![RepoPath::from_bytes(b".env").unwrap()]
        );
        fs::remove_file(root.join(".env")).unwrap();
        // A known-disposable directory name is never a disposable regular file.
        fs::write(root.join("node_modules"), "user data").unwrap();
        assert_eq!(
            service.notable_ignored_files(root, &cancellation).unwrap(),
            vec![RepoPath::from_bytes(b"node_modules").unwrap()]
        );
        fs::remove_file(root.join("node_modules")).unwrap();
        fs::create_dir_all(root.join("cache/target/nested")).unwrap();
        fs::write(root.join("cache/target/nested/generated"), "disposable").unwrap();
        assert!(
            service
                .notable_ignored_files(root, &cancellation)
                .unwrap()
                .is_empty()
        );
        fs::write(root.join("cache/data"), "custom").unwrap();
        assert!(
            !service
                .notable_ignored_files(root, &cancellation)
                .unwrap()
                .is_empty()
        );
        let mut names = default_disposable_ignored_directories();
        names.push("cache".into());
        service
            .set_disposable_ignored_directories(root, &names, &cancellation)
            .unwrap();
        assert!(
            service
                .notable_ignored_files(root, &cancellation)
                .unwrap()
                .is_empty()
        );
        // Git sorts a file cache.data before directory cache/. Preserve each
        // record's directory flag before canonical path sorting removes '/'.
        fs::write(
            root.join(".gitignore"),
            ".env\nnode_modules\ntarget/\ncache/\ncache.data\n",
        )
        .unwrap();
        fs::write(root.join("cache.data"), "user data").unwrap();
        names.push("cache.data".into());
        service
            .set_disposable_ignored_directories(root, &names, &cancellation)
            .unwrap();
        assert_eq!(
            service.notable_ignored_files(root, &cancellation).unwrap(),
            vec![RepoPath::from_bytes(b"cache.data").unwrap()]
        );
        assert!(
            service
                .set_disposable_ignored_directories(root, &["../secret".into()], &cancellation)
                .is_err()
        );
    }

    #[test]
    fn unreadable_inventory_warnings_never_authorize_removal() {
        use std::os::unix::fs::PermissionsExt;
        // Root bypasses directory permissions. The fw16 validation runs as
        // the ordinary user and exercises each real Git warning below.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = fixture.path();
        git(root, &["init", "-b", "main"]);
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        git(root, &["add", ".gitignore"]);
        git(root, &["commit", "-m", "initial"]);
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        for name in ["ignored", "ignored/locked", "untracked"] {
            let unreadable = root.join(name);
            fs::create_dir_all(&unreadable).unwrap();
            fs::write(unreadable.join("secret"), "preserve").unwrap();
            // Git can return a partial listing and a warning together.
            if name == "ignored/locked" {
                fs::write(root.join("ignored/readable"), "preserve").unwrap();
            }
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o300)).unwrap();
            let result = if name == "untracked" {
                service.status_for_removal(root, &cancel).map(|_| ())
            } else {
                service.notable_ignored_files(root, &cancel).map(|_| ())
            };
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Git could not inspect every file"),
                "{error}"
            );
            assert_eq!(
                fs::read_to_string(unreadable.join("secret")).unwrap(),
                "preserve"
            );
            fs::remove_dir_all(root.join(if name.starts_with("ignored") {
                "ignored"
            } else {
                "untracked"
            }))
            .unwrap();
        }
        struct LocksAfterCollapsedInventory(PathBuf);
        impl ProcessExecutor for LocksAfterCollapsedInventory {
            fn execute(
                &self,
                command: &CommandSpec,
                cancellation: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                let output = SystemProcessExecutor.execute(command, cancellation)?;
                if command
                    .arguments
                    .iter()
                    .any(|argument| argument == "--directory")
                {
                    assert!(output.success && output.stderr.is_empty());
                    fs::set_permissions(&self.0, fs::Permissions::from_mode(0o300)).unwrap();
                }
                Ok(output)
            }
        }
        // Change permissions after a complete collapsed scan. This forces
        // the production expansion call to receive the real Git warning.
        let locked = root.join("ignored/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("secret"), "preserve").unwrap();
        let service = RepositoryService::new(
            "git",
            Arc::new(LocksAfterCollapsedInventory(locked.clone())),
        );
        let result = service.notable_ignored_files(root, &cancel);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Git could not inspect every file")
        );
        assert_eq!(
            fs::read_to_string(locked.join("secret")).unwrap(),
            "preserve"
        );
    }

    #[test]
    fn incomplete_nested_repository_inventories_refuse_deletion_before_and_after_child_edits() {
        let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = fixture.path();
        git(root, &["init", "-b", "main"]);
        fs::write(root.join(".gitignore"), "ignored/\ntarget/\n").unwrap();
        git(root, &["add", ".gitignore"]);
        git(root, &["commit", "-m", "initial"]);
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        for name in ["ignored", "untracked"] {
            let nested = root.join(name);
            fs::create_dir(&nested).unwrap();
            git(&nested, &["init", "-b", "main"]);
            fs::write(nested.join("data"), "original").unwrap();
            for content in ["original", "changed descendant"] {
                fs::write(nested.join("data"), content).unwrap();
                let error = service.worktree_safety(root, &cancel).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("Cannot inspect all files inside"),
                    "{error}"
                );
                assert_eq!(fs::read_to_string(nested.join("data")).unwrap(), content);
            }
            fs::remove_dir_all(nested).unwrap();
        }
        // Explicitly disposable ignored directories remain policy exclusions.
        let disposable = root.join("target");
        fs::create_dir(&disposable).unwrap();
        git(&disposable, &["init", "-b", "main"]);
        fs::write(disposable.join("data"), "generated").unwrap();
        assert!(
            service
                .worktree_safety(root, &cancel)
                .unwrap()
                .notable_ignored
                .is_empty()
        );
    }

    #[test]
    fn fresh_safety_counts_untracked_ignored_unpushed_and_detached_only_work() {
        let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
        let repository = fixture.path().join("repo");
        let worktree = fixture.path().join("worktree");
        fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        fs::write(repository.join(".gitignore"), ".env\n").unwrap();
        git(&repository, &["add", ".gitignore"]);
        git(&repository, &["commit", "-m", "initial"]);
        let initial = git(&repository, &["rev-parse", "HEAD"]);
        git(
            &repository,
            &["update-ref", "refs/remotes/origin/main", &initial],
        );
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                "feature",
                worktree.to_str().unwrap(),
            ],
        );
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        let clean = service.worktree_safety(&worktree, &cancel).unwrap();
        assert!(!clean.requires_confirmation());
        assert_eq!(clean.branch.as_deref(), Some("feature"));
        fs::write(worktree.join("new file"), "new").unwrap();
        fs::write(worktree.join(".env"), "secret").unwrap();
        let fresh = service.worktree_safety(&worktree, &cancel).unwrap();
        assert_eq!(fresh.untracked, 1);
        assert_eq!(fresh.notable_ignored.len(), 1);
        assert_eq!(fresh.files_to_discard(), 2);
        fs::remove_file(worktree.join("new file")).unwrap();
        fs::write(worktree.join("replacement file"), "new").unwrap();
        let replaced = service.worktree_safety(&worktree, &cancel).unwrap();
        assert_eq!(fresh.files_to_discard(), replaced.files_to_discard());
        assert_ne!(fresh.fingerprint, replaced.fingerprint);
        fs::rename(worktree.join("replacement file"), worktree.join("new file")).unwrap();
        git(&worktree, &["add", "new file"]);
        git(&worktree, &["commit", "-m", "local work"]);
        assert_eq!(
            service
                .worktree_safety(&worktree, &cancel)
                .unwrap()
                .unpushed_commits,
            1
        );
        git(&worktree, &["checkout", "--detach"]);
        git(
            &worktree,
            &["commit", "--allow-empty", "-m", "detached work"],
        );
        let detached = service.worktree_safety(&worktree, &cancel).unwrap();
        assert!(detached.branch.is_none());
        assert_eq!(detached.detached_only_commits, 1);
        fs::write(worktree.join("late untracked"), "preserve").unwrap();
        assert!(
            service
                .rescue_detached_worktree(&worktree, &detached, &cancel)
                .is_err()
        );
        assert!(
            git(
                &repository,
                &["for-each-ref", "--format=%(refname)", "refs/heads/rescue/"]
            )
            .is_empty()
        );
        fs::remove_file(worktree.join("late untracked")).unwrap();
        let rescue = service
            .rescue_detached_worktree(&worktree, &detached, &cancel)
            .unwrap();
        assert!(rescue.starts_with("rescue/"));
        assert_eq!(
            service
                .worktree_safety(&worktree, &cancel)
                .unwrap()
                .detached_only_commits,
            0
        );
        assert_eq!(
            git(&repository, &["rev-parse", &rescue]),
            detached.head_oid.unwrap()
        );
    }
}
