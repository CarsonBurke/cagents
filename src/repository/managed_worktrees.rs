//! Clone-local ignore policy shared by local and device worktree creation.
use super::*;

const MAX_EXCLUDE_BYTES: u64 = 4 * 1024 * 1024;

impl<E: ProcessExecutor> RepositoryService<E> {
    fn managed_primary_checkout(
        &self,
        repository: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<PathBuf>, RepositoryError> {
        let records = self.list_worktrees(repository, cancellation)?;
        let primary = records
            .first()
            .ok_or_else(|| ValidationError::Io("Repository has no primary registration".into()))?;
        if primary.bare {
            return Ok(None);
        }
        let common = self.common_dir(repository, cancellation)?;
        // A separate Git directory's primary list entry can name metadata,
        // rather than its checkout. Keep the same resolution as the registered
        // HEAD guard, and verify that the result belongs to this common dir.
        let primary = if primary.path == common {
            let configured = if self.git_dir(repository, cancellation)? == common {
                self.invocation_worktree(repository, cancellation)
                    .or_else(|error| {
                        if cancellation.is_cancelled() {
                            Err(error)
                        } else {
                            self.configured_primary_worktree(repository, cancellation)
                        }
                    })?
            } else {
                self.configured_primary_worktree(repository, cancellation)?
            };
            common.join(configured)
        } else {
            primary.path.clone()
        };
        let primary = primary
            .canonicalize()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        if self.git_dir(&primary, cancellation)? != common {
            return Err(ValidationError::Io(
                "Primary checkout no longer belongs to the original repository".into(),
            )
            .into());
        }
        Ok(Some(primary))
    }

    /// Ignore an in-checkout managed root without editing tracked .gitignore.
    /// External roots and directories already ignored by Git need no write.
    pub fn ensure_managed_directory_ignored(
        &self,
        repository: &Path,
        managed_root: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        let Some(primary) = self.managed_primary_checkout(repository, cancellation)? else {
            return Ok(());
        };
        let managed = canonical_managed_directory(managed_root)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let Ok(relative) = managed.strip_prefix(&primary) else {
            return Ok(());
        };
        if relative.as_os_str().is_empty() {
            return Err(ValidationError::Io(
                "The repository root cannot be its managed worktree directory".into(),
            )
            .into());
        }
        let mut directory = relative.as_os_str().to_owned();
        directory.push("/");
        let check = self.git_read(
            &primary,
            [
                OsString::from("check-ignore"),
                "--quiet".into(),
                "--no-index".into(),
                "--".into(),
                directory,
            ],
        );
        let output = self.run(&check, cancellation)?;
        if output.success {
            return Ok(());
        }
        if output.exit_code != Some(1) {
            return Err(git_failure(&check, output).into());
        }
        let mut pattern = vec![b'/'];
        for byte in relative.as_os_str().as_bytes() {
            if matches!(byte, b'\n' | b'\r') {
                return Err(ValidationError::Io(
                    "Managed worktree directory contains a newline".into(),
                )
                .into());
            }
            if matches!(byte, b'\\' | b'!' | b'#' | b'*' | b'?' | b'[' | b']' | b' ') {
                pattern.push(b'\\');
            }
            pattern.push(*byte);
        }
        pattern.push(b'/');

        let common = self.common_dir(repository, cancellation)?;
        let common = fs_safety::Root::open(&common)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let info = fs_safety::create_directories_beneath(&common, Path::new("info"), 0o777)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let mut lock = GitFileLock::acquire_beneath(&info, OsStr::new("exclude"))
            .map_err(ValidationError::Io)?;
        let mut bytes = match fs_safety::open_beneath(&lock.parent, Path::new("exclude")) {
            Ok(mut file) => {
                if !file
                    .metadata()
                    .map_err(|error| ValidationError::Io(error.to_string()))?
                    .is_file()
                {
                    return Err(
                        ValidationError::Io("Git exclude is not a regular file".into()).into(),
                    );
                }
                fs_safety::read_bounded(&mut file, MAX_EXCLUDE_BYTES)
                    .map_err(|error| ValidationError::Io(error.to_string()))?
            }
            Err(error) if error.is_not_found() => Vec::new(),
            Err(error) => return Err(ValidationError::Io(error.to_string()).into()),
        };
        if bytes.len() as u64 > MAX_EXCLUDE_BYTES {
            return Err(ValidationError::Io(
                "Existing Git exclude file is too large; no ignore rules were changed".into(),
            )
            .into());
        }
        if bytes
            .split(|byte| *byte == b'\n')
            .any(|line| line == pattern)
        {
            return Ok(());
        }
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(&pattern);
        bytes.push(b'\n');
        lock.file
            .write_all(&bytes)
            .and_then(|_| lock.file.sync_all())
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        lock.version = GitFileLock::version(
            &lock
                .file
                .metadata()
                .map_err(|error| ValidationError::Io(error.to_string()))?,
        );
        lock.check().map_err(ValidationError::Io)?;
        fs_safety::rename_within(&lock.parent, &lock.name, OsStr::new("exclude"))
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        lock.active = false;
        fs_safety::sync_directory(&lock.parent)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        Ok(())
    }

    /// Preserve legacy locations while excluding them on the next creation.
    pub fn ignore_legacy_managed_worktrees(
        &self,
        repository: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        let Some(primary) = self.managed_primary_checkout(repository, cancellation)? else {
            return Ok(());
        };
        let legacy = primary.join("worktrees");
        if self
            .list_worktrees(repository, cancellation)?
            .iter()
            .skip(1)
            .any(|record| record.path.starts_with(&legacy))
        {
            self.ensure_managed_directory_ignored(repository, &legacy, cancellation)?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn isolated_git_test(name: &str) -> bool {
    let name = name
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(name);
    if std::env::var("CAGENTS_MANAGED_DIRECTORY_TEST").as_deref() == Ok(name) {
        return false;
    }
    let mut command = CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--test-threads=1"])
        .timeout(Duration::from_secs(120))
        .output_limits(1024 * 1024, 1024 * 1024);
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command = command.env_remove(key);
        }
    }
    command = command
        .env("CAGENTS_MANAGED_DIRECTORY_TEST", name)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "4")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_1", "/dev/null")
        .env("GIT_CONFIG_KEY_2", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env("GIT_CONFIG_KEY_3", "init.templateDir")
        .env("GIT_CONFIG_VALUE_3", "");
    let output = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.success,
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains(&format!("test {name} ... ok")), "{stdout}");
    assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let command = CommandSpec::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args.iter().copied())
            .timeout(Duration::from_secs(20))
            .output_limits(1024 * 1024, 64 * 1024);
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        assert!(
            output.success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["commit", "--allow-empty", "-m", "initial"]);
        (directory, root.canonicalize().unwrap())
    }

    #[test]
    fn exclusion_is_idempotent_preserves_rules_and_skips_external_or_ignored_roots() {
        if isolated_git_test(concat!(
            module_path!(),
            "::exclusion_is_idempotent_preserves_rules_and_skips_external_or_ignored_roots"
        )) {
            return;
        }
        let (_directory, root) = fixture();
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        let exclude = root.join(".git/info/exclude");
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::write(&exclude, b"# keep this rule\n/local-only").unwrap();
        let original = fs::read(&exclude).unwrap();
        service
            .ensure_managed_directory_ignored(
                &root,
                &root.parent().unwrap().join("external"),
                &cancel,
            )
            .unwrap();
        assert_eq!(fs::read(&exclude).unwrap(), original);
        fs::write(
            root.join(".gitignore"),
            b"already/\n.cagents-ignore-check\n",
        )
        .unwrap();
        service
            .ensure_managed_directory_ignored(&root, &root.join("already"), &cancel)
            .unwrap();
        assert_eq!(fs::read(&exclude).unwrap(), original);
        let managed = root.join("trees [local]!");
        service
            .ensure_managed_directory_ignored(&root, &managed, &cancel)
            .unwrap();
        let once = fs::read(&exclude).unwrap();
        assert!(once.starts_with(b"# keep this rule\n/local-only\n"));
        service
            .ensure_managed_directory_ignored(&root, &managed, &cancel)
            .unwrap();
        assert_eq!(fs::read(&exclude).unwrap(), once);
        fs::create_dir(&managed).unwrap();
        fs::write(managed.join("artifact"), b"local data").unwrap();
        assert_eq!(
            git(&root, &["check-ignore", "trees [local]!/artifact"]),
            b"trees [local]!/artifact\n"
        );
        assert_eq!(
            fs::read(root.join(".gitignore")).unwrap(),
            b"already/\n.cagents-ignore-check\n"
        );
    }

    #[test]
    fn exclusion_refuses_oversized_or_symlinked_metadata_without_truncation() {
        if isolated_git_test(concat!(
            module_path!(),
            "::exclusion_refuses_oversized_or_symlinked_metadata_without_truncation"
        )) {
            return;
        }
        let (directory, root) = fixture();
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        let exclude = root.join(".git/info/exclude");
        fs::create_dir_all(root.join(".git/info")).unwrap();
        let bytes = vec![b'#'; MAX_EXCLUDE_BYTES as usize + 1];
        fs::write(&exclude, &bytes).unwrap();
        assert!(
            service
                .ensure_managed_directory_ignored(&root, &root.join(".worktrees"), &cancel)
                .is_err()
        );
        assert_eq!(fs::read(&exclude).unwrap(), bytes);
        assert!(!root.join(".git/info/exclude.lock").exists());
        fs::remove_file(&exclude).unwrap();
        let foreign = directory.path().join("foreign");
        fs::write(&foreign, b"preserve").unwrap();
        std::os::unix::fs::symlink(&foreign, &exclude).unwrap();
        assert!(
            service
                .ensure_managed_directory_ignored(&root, &root.join(".worktrees"), &cancel)
                .is_err()
        );
        assert_eq!(fs::read(&foreign).unwrap(), b"preserve");
        assert!(fs::symlink_metadata(&exclude).unwrap().is_symlink());
        assert!(!root.join(".git/info/exclude.lock").exists());
    }

    #[test]
    fn legacy_worktrees_are_excluded_without_moving_them() {
        if isolated_git_test(concat!(
            module_path!(),
            "::legacy_worktrees_are_excluded_without_moving_them"
        )) {
            return;
        }
        let (_directory, root) = fixture();
        let legacy = root.join("worktrees/old");
        git(
            &root,
            &["worktree", "add", "-b", "old", legacy.to_str().unwrap()],
        );
        fs::write(legacy.join("local-data"), b"keep").unwrap();
        let service = RepositoryService::system("git");
        service
            .ignore_legacy_managed_worktrees(&root, &CancellationToken::new())
            .unwrap();
        assert_eq!(fs::read(legacy.join("local-data")).unwrap(), b"keep");
        assert!(git(&root, &["status", "--porcelain", "-uall"]).is_empty());
        assert_eq!(
            fs::read_to_string(root.join(".git/info/exclude")).unwrap(),
            "/worktrees/\n"
        );
    }

    #[test]
    fn separate_git_directory_excludes_the_actual_primary_checkout() {
        if isolated_git_test(concat!(
            module_path!(),
            "::separate_git_directory_excludes_the_actual_primary_checkout"
        )) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("checkout");
        let metadata = directory.path().join("metadata");
        fs::create_dir(&root).unwrap();
        git(
            &root,
            &[
                "init",
                "-b",
                "main",
                "--separate-git-dir",
                metadata.to_str().unwrap(),
            ],
        );
        git(&root, &["commit", "--allow-empty", "-m", "initial"]);
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        service
            .ensure_managed_directory_ignored(&root, &root.join(".worktrees"), &cancel)
            .unwrap();
        let once = fs::read(metadata.join("info/exclude")).unwrap();
        assert_eq!(once, b"/.worktrees/\n");
        git(&root, &["config", "core.worktree", root.to_str().unwrap()]);
        let linked = directory.path().join("linked");
        git(
            &root,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        service
            .ensure_managed_directory_ignored(&linked, &root.join(".worktrees"), &cancel)
            .unwrap();
        assert_eq!(fs::read(metadata.join("info/exclude")).unwrap(), once);
    }

    #[cfg(feature = "ui")]
    #[test]
    fn journaled_default_creates_two_clean_worktrees_and_migrates_legacy_location() {
        if isolated_git_test(concat!(
            module_path!(),
            "::journaled_default_creates_two_clean_worktrees_and_migrates_legacy_location"
        )) {
            return;
        }
        let (directory, root) = fixture();
        let legacy = root.join("worktrees/old");
        git(
            &root,
            &["worktree", "add", "-b", "old", legacy.to_str().unwrap()],
        );
        fs::write(legacy.join("local-data"), b"keep").unwrap();
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        let store = StateStore::open(
            directory.path().join("journal.sqlite3"),
            crate::model::BootId::new("managed-test"),
        )
        .unwrap();
        for branch in ["one", "two"] {
            let input = WorktreeInput {
                repository: root.clone(),
                input: branch.into(),
                base: String::new(),
                managed_directory: String::new(),
                destination: String::new(),
                operation_id: OperationId::new(),
            };
            let request = service.resolve_worktree_request(&input, &cancel).unwrap();
            assert_eq!(request.managed_root, root.join(".worktrees"));
            let (send, _recv) = async_channel::bounded(8);
            let report = service.create_worktree_journaled(&store, &request, 1, &cancel, &send);
            assert!(
                matches!(
                    report.outcome,
                    Some(WorktreeCreationOutcome::Created { .. })
                ),
                "{report:?}"
            );
        }
        for configured in [
            "worktrees/".to_owned(),
            directory
                .path()
                .join("external")
                .to_str()
                .unwrap()
                .to_owned(),
        ] {
            let request = service
                .resolve_worktree_request(
                    &WorktreeInput {
                        repository: root.clone(),
                        input: "configured".into(),
                        base: String::new(),
                        managed_directory: configured.clone(),
                        destination: String::new(),
                        operation_id: OperationId::new(),
                    },
                    &cancel,
                )
                .unwrap();
            assert_eq!(
                request.managed_root,
                canonical_managed_directory(&root.join(configured)).unwrap()
            );
        }
        assert!(git(&root, &["status", "--porcelain", "-uall"]).is_empty());
        assert_eq!(fs::read(legacy.join("local-data")).unwrap(), b"keep");
        let rules = fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert_eq!(
            rules.lines().filter(|line| *line == "/.worktrees/").count(),
            1
        );
        assert_eq!(
            rules.lines().filter(|line| *line == "/worktrees/").count(),
            1
        );
    }
}
