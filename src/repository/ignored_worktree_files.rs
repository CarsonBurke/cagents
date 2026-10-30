//! Ignored user-file inventory; it grants no deletion authority.
use super::complete_inventory::require_complete_inventory;
use super::*;

const DISPOSABLE_CONFIG: &str = "cagents.cleanupDisposableDirectories";

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

impl<E: ProcessExecutor> RepositoryService<E> {
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
        if isolated_git_test(concat!(
            module_path!(),
            "::ignored_user_data_blocks_but_disposable_directories_and_nested_files_do_not"
        )) {
            return;
        }
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
        if isolated_git_test(concat!(
            module_path!(),
            "::unreadable_inventory_warnings_never_authorize_removal"
        )) {
            return;
        }
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
}
