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
                    })
            } else {
                self.configured_primary_worktree(repository, cancellation)
            };
            let configured = match configured {
                Ok(path) => path,
                // A linked invocation cannot discover a separate primary with
                // no core.worktree backlink. Its own registered checkout is
                // still usable; do not invent a primary path for migration.
                Err(RepositoryError::Process(error))
                    if error.kind == ProcessFailureKind::Exit && error.exit_code == Some(1) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
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
        self.ensure_managed_directory_or_destination_ignored(
            repository,
            managed_root,
            None,
            cancellation,
        )
    }

    pub(super) fn ensure_creation_destination_ignored(
        &self,
        request: &CreateWorktreeRequest,
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        self.ensure_managed_directory_or_destination_ignored(
            &request.repository,
            request
                .managed_ignore_root
                .as_deref()
                .unwrap_or(&request.destination),
            Some(&request.destination),
            cancellation,
        )
    }

    fn ensure_managed_directory_or_destination_ignored(
        &self,
        repository: &Path,
        managed_root: &Path,
        destination: Option<&Path>,
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        let mut managed = canonical_managed_directory(managed_root)
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        // The destination determines which checkout's status must exclude it.
        // A configured parent can be that checkout itself, or lie outside it.
        let placement = destination.unwrap_or(&managed);
        let mut checkout: Option<PathBuf> = None;
        // A project/device can be opened at a linked checkout, including one
        // outside the primary. Common exclude patterns are relative to EACH
        // checkout, so use its own most-specific containing root.
        for record in self
            .list_worktrees(repository, cancellation)?
            .iter()
            .skip(1)
        {
            if record.bare || !placement.starts_with(&record.path) {
                continue;
            }
            if checkout.as_ref().is_none_or(|current| {
                record.path.components().count() > current.components().count()
            }) {
                let candidate = record
                    .path
                    .canonicalize()
                    .map_err(|error| ValidationError::Io(error.to_string()))?;
                if self.common_dir(&candidate, cancellation)?
                    != self.common_dir(repository, cancellation)?
                {
                    return Err(ValidationError::Io(
                        "Managed checkout belongs to another repository".into(),
                    )
                    .into());
                }
                checkout = Some(candidate);
            }
        }
        if checkout.is_none() {
            checkout = self
                .managed_primary_checkout(repository, cancellation)?
                .filter(|path| placement.starts_with(path));
        }
        let Some(primary) = checkout else {
            return Ok(());
        };
        if let Some(destination) = destination {
            if managed == primary || !managed.starts_with(&primary) {
                // Never hide a whole checkout or a shared ancestor. Keep the
                // valid location and exclude only this operation's child.
                managed = destination.to_owned();
            }
        }
        self.ensure_managed_directory_in_checkout_ignored(
            repository,
            &primary,
            &managed,
            cancellation,
        )
    }

    fn ensure_managed_directory_in_checkout_ignored(
        &self,
        repository: &Path,
        primary: &Path,
        managed: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        let relative = managed
            .strip_prefix(primary)
            .map_err(|_| ValidationError::DestinationOutsideRoot)?;
        if relative.as_os_str().is_empty() {
            return Err(ValidationError::Io(
                "The repository root cannot be its managed worktree directory".into(),
            )
            .into());
        }
        let mut directory = relative.as_os_str().to_owned();
        directory.push("/");
        let check = self.git_read(
            primary,
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
        // The first effective query already said this directory is visible.
        // A matching earlier line can have been cancelled by a later negation.
        // If our exact rule is already last, a higher-precedence .gitignore is
        // overriding it; preserve that file and refuse instead of adding copies.
        if bytes
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.is_empty())
            == Some(pattern.as_slice())
        {
            return Err(ValidationError::Io(
                "A higher-priority Git ignore rule keeps the managed directory visible; update that rule before creating the worktree".into(),
            ).into());
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
        let effective = self.run(&check, cancellation)?;
        if !effective.success {
            if effective.exit_code != Some(1) {
                return Err(git_failure(&check, effective).into());
            }
            return Err(ValidationError::Io(
                "A higher-priority Git ignore rule keeps the managed directory visible; update that rule before creating the worktree".into(),
            ).into());
        }
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
            .any(|record| record.path != legacy && record.path.starts_with(&legacy))
        {
            let legacy = canonical_managed_directory(&legacy)
                .map_err(|error| ValidationError::Io(error.to_string()))?;
            // This is a collection relative to the known primary, even when
            // one registered checkout happens to occupy the collection root.
            self.ensure_managed_directory_in_checkout_ignored(
                repository,
                &primary,
                &legacy,
                cancellation,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn isolated_git_test(name: &str) -> bool {
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let name = name
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(name);
    if std::env::var("CAGENTS_MANAGED_DIRECTORY_TEST").as_deref() == Ok(name) {
        return false;
    }
    let directory = tempfile::Builder::new()
        .prefix("cagents-managed-contract-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
        .keep();
    let mut command = CommandSpec::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--nocapture",
            "--test-threads=1",
            "--color=never",
        ])
        .timeout(Duration::from_secs(120))
        .output_limits(1024 * 1024, 1024 * 1024);
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command = command.env_remove(key);
        }
    }
    command = command
        .env("CAGENTS_MANAGED_DIRECTORY_TEST", name)
        .env("CAGENTS_MANAGED_DIRECTORY_ROOT", &directory)
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
    let (success, stdout, stderr, status) =
        match SystemProcessExecutor.execute(&command, &CancellationToken::new()) {
            Ok(output) => (
                output.success,
                output.stdout,
                output.stderr,
                format!("exit {:?}, signal {:?}", output.exit_code, output.signal),
            ),
            Err(failure) => (
                false,
                failure.stdout,
                failure.stderr,
                format!("{:?}: {}", failure.kind, failure.message),
            ),
        };
    for (name, bytes) in [("stdout", &stdout), ("stderr", &stderr)] {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(name))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    eprintln!(
        "Retained managed-directory contract root: {}",
        directory.display()
    );
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(success, "{status}\n{stdout}\n{stderr}");
    assert!(
        stdout
            .lines()
            .any(|line| line.contains(&format!("test {name} ... ok"))),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "{stdout}"
    );
    print!("{stdout}");
    eprint!("{stderr}");
    true
}

#[cfg(test)]
pub(crate) fn retained_managed_fixture_directory() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let parent = std::env::var_os("CAGENTS_MANAGED_DIRECTORY_ROOT")
        .expect("owning private managed-directory contract namespace");
    let mut directory = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
        .unwrap();
    // Child completion alone cannot retire the original repositories/journals.
    directory.disable_cleanup(true);
    directory
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
        let directory = retained_managed_fixture_directory();
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
    fn effective_ignore_repairs_later_exclude_negation_and_preserves_tracked_rules() {
        if isolated_git_test(concat!(
            module_path!(),
            "::effective_ignore_repairs_later_exclude_negation_and_preserves_tracked_rules"
        )) {
            return;
        }
        let (_directory, root) = fixture();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        let exclude = root.join(".git/info/exclude");
        let original = b"/.worktrees/\n!/.worktrees/\n";
        fs::write(&exclude, original).unwrap();
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        service
            .ensure_managed_directory_ignored(&root, &root.join(".worktrees"), &cancel)
            .unwrap();
        let repaired = fs::read(&exclude).unwrap();
        assert!(repaired.starts_with(original));
        assert!(repaired.ends_with(b"!/.worktrees/\n/.worktrees/\n"));
        service
            .ensure_managed_directory_ignored(&root, &root.join(".worktrees"), &cancel)
            .unwrap();
        assert_eq!(fs::read(&exclude).unwrap(), repaired);
        let tracked_rule = b"!/.worktrees/\n";
        fs::write(root.join(".gitignore"), tracked_rule).unwrap();
        git(&root, &["add", ".gitignore"]);
        assert!(
            service
                .ensure_managed_directory_ignored(&root, &root.join(".worktrees"), &cancel)
                .is_err()
        );
        assert_eq!(fs::read(&exclude).unwrap(), repaired);
        assert_eq!(fs::read(root.join(".gitignore")).unwrap(), tracked_rule);
    }

    #[test]
    fn separate_git_directory_excludes_the_actual_primary_checkout() {
        if isolated_git_test(concat!(
            module_path!(),
            "::separate_git_directory_excludes_the_actual_primary_checkout"
        )) {
            return;
        }
        let directory = retained_managed_fixture_directory();
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
        let linked = directory.path().join("linked");
        git(
            &root,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        service
            .ensure_managed_directory_ignored(&linked, &root.join(".worktrees"), &cancel)
            .unwrap();
        service
            .ignore_legacy_managed_worktrees(&linked, &cancel)
            .unwrap();
        service
            .ensure_managed_directory_ignored(&linked, &linked.join(".worktrees"), &cancel)
            .unwrap();
        git(&root, &["config", "core.worktree", root.to_str().unwrap()]);
        service
            .ensure_managed_directory_ignored(&linked, &root.join(".worktrees"), &cancel)
            .unwrap();
        assert_eq!(fs::read(metadata.join("info/exclude")).unwrap(), once);
    }

    #[test]
    fn linked_checkout_uses_its_own_relative_ignore_pattern() {
        if isolated_git_test(concat!(
            module_path!(),
            "::linked_checkout_uses_its_own_relative_ignore_pattern"
        )) {
            return;
        }
        let (directory, root) = fixture();
        let linked = directory.path().join("external-linked");
        git(
            &root,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        service
            .ensure_managed_directory_ignored(&linked, &linked.join(".worktrees"), &cancel)
            .unwrap();
        assert_eq!(
            fs::read(root.join(".git/info/exclude")).unwrap(),
            b"/.worktrees/\n"
        );
        fs::create_dir(linked.join(".worktrees")).unwrap();
        fs::write(linked.join(".worktrees/artifact"), b"keep").unwrap();
        assert!(git(&linked, &["status", "--porcelain", "-uall"]).is_empty());
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

    #[cfg(feature = "ui")]
    #[test]
    fn journaled_overrides_ignore_only_the_destination_and_preserve_legacy_requests() {
        if isolated_git_test(concat!(
            module_path!(),
            "::journaled_overrides_ignore_only_the_destination_and_preserve_legacy_requests"
        )) {
            return;
        }
        let (directory, root) = fixture();
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        let store = StateStore::open(
            directory.path().join("journal.sqlite3"),
            crate::model::BootId::new("override-test"),
        )
        .unwrap();
        fs::create_dir(root.join("shared")).unwrap();
        fs::write(root.join("shared/keep"), b"unrelated data").unwrap();
        for (branch, destination) in [
            ("direct", "custom"),
            ("nested", "shared/chosen"),
            ("legacy", "shared/legacy"),
        ] {
            let mut request = service
                .resolve_worktree_request(
                    &WorktreeInput {
                        repository: root.clone(),
                        input: branch.into(),
                        base: String::new(),
                        managed_directory: String::new(),
                        destination: destination.into(),
                        operation_id: OperationId::new(),
                    },
                    &cancel,
                )
                .unwrap();
            assert_eq!(
                request.managed_ignore_root.as_ref(),
                Some(&request.destination)
            );
            if branch == "legacy" {
                let mut old = serde_json::to_value(&request).unwrap();
                old.as_object_mut().unwrap().remove("managed_ignore_root");
                request = serde_json::from_value(old.clone()).unwrap();
                assert!(request.managed_ignore_root.is_none());
                assert_eq!(serde_json::to_value(&request).unwrap(), old);
            }
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
        let rules = fs::read_to_string(root.join(".git/info/exclude")).unwrap();
        assert_eq!(rules, "/custom/\n/shared/chosen/\n/shared/legacy/\n");
        assert_eq!(
            git(&root, &["status", "--porcelain", "-uall"]),
            b"?? shared/keep\n"
        );
        assert_eq!(
            fs::read(root.join("shared/keep")).unwrap(),
            b"unrelated data"
        );
        let restored = service.rehydrate_worktree_operations(&store).unwrap();
        assert_eq!(restored.len(), 3);
        assert!(
            restored
                .iter()
                .all(|report| report.state == WorktreeOperationState::Created)
        );
        assert!(restored.iter().any(|report| {
            report.request.as_ref().is_some_and(|request| {
                request.branch.as_str() == "legacy" && request.managed_ignore_root.is_none()
            })
        }));
        assert_eq!(
            fs::read_to_string(root.join(".git/info/exclude")).unwrap(),
            rules
        );
    }

    #[cfg(feature = "ui")]
    #[test]
    fn journaled_checkout_root_parents_ignore_only_created_destinations() {
        if isolated_git_test(concat!(
            module_path!(),
            "::journaled_checkout_root_parents_ignore_only_created_destinations"
        )) {
            return;
        }
        for variant in 0..3 {
            let (directory, root) = fixture();
            let service = RepositoryService::system("git");
            let cancel = CancellationToken::new();
            let store = StateStore::open(
                directory.path().join("journal.sqlite3"),
                crate::model::BootId::new("checkout-root-parent"),
            )
            .unwrap();
            fs::write(root.join("unrelated.txt"), b"keep visible").unwrap();
            let managed = match variant {
                0 => ".".to_owned(),
                1 => root.to_str().unwrap().to_owned(),
                _ => "..".to_owned(),
            };
            // A checkout itself named worktrees is not a legacy collection.
            // The next creation must not try to ignore its entire checkout.
            let branches = if variant == 2 {
                vec!["repo/ancestor-one", "repo/ancestor-two"]
            } else {
                vec!["worktrees", "next"]
            };
            for branch in branches {
                let request = service
                    .resolve_worktree_request(
                        &WorktreeInput {
                            repository: root.clone(),
                            input: branch.into(),
                            base: String::new(),
                            managed_directory: managed.clone(),
                            destination: String::new(),
                            operation_id: OperationId::new(),
                        },
                        &cancel,
                    )
                    .unwrap();
                let expected = root.join(branch.rsplit('/').next().unwrap());
                assert_eq!(request.destination, expected);
                let (send, _receive) = async_channel::bounded(8);
                let report = service.create_worktree_journaled(&store, &request, 1, &cancel, &send);
                assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
                assert!(expected.join(".git").is_file());
                if variant == 0 && branch == "worktrees" {
                    let nested = service
                        .resolve_worktree_request(
                            &WorktreeInput {
                                repository: root.clone(),
                                input: "nested-legacy".into(),
                                base: String::new(),
                                managed_directory: "worktrees/".into(),
                                destination: String::new(),
                                operation_id: OperationId::new(),
                            },
                            &cancel,
                        )
                        .unwrap();
                    assert_eq!(nested.destination, root.join("worktrees/nested-legacy"));
                    let report =
                        service.create_worktree_journaled(&store, &nested, 1, &cancel, &send);
                    assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
                }
            }
            assert_eq!(
                git(&root, &["status", "--porcelain", "-uall"]),
                b"?? unrelated.txt\n"
            );
            assert_eq!(
                fs::read(root.join("unrelated.txt")).unwrap(),
                b"keep visible"
            );
            let rules = fs::read_to_string(root.join(".git/info/exclude")).unwrap();
            assert!(!rules.lines().any(|line| line == "/"));
            assert!(rules.lines().any(|line| line
                == if variant == 2 {
                    "/ancestor-one/"
                } else {
                    "/worktrees/"
                }));
        }
    }

    #[cfg(feature = "ui")]
    #[test]
    fn journaled_creation_refuses_effectively_unignored_destination() {
        if isolated_git_test(concat!(
            module_path!(),
            "::journaled_creation_refuses_effectively_unignored_destination"
        )) {
            return;
        }
        let (directory, root) = fixture();
        let service = RepositoryService::system("git");
        let cancel = CancellationToken::new();
        let store = StateStore::open(
            directory.path().join("journal.sqlite3"),
            crate::model::BootId::new("conflict-test"),
        )
        .unwrap();
        fs::write(root.join(".gitignore"), b"!/.worktrees/\n").unwrap();
        let request = service
            .resolve_worktree_request(
                &WorktreeInput {
                    repository: root.clone(),
                    input: "conflict".into(),
                    base: String::new(),
                    managed_directory: String::new(),
                    destination: String::new(),
                    operation_id: OperationId::new(),
                },
                &cancel,
            )
            .unwrap();
        let (send, _recv) = async_channel::bounded(8);
        let report = service.create_worktree_journaled(&store, &request, 1, &cancel, &send);
        assert!(
            !matches!(
                report.outcome,
                Some(WorktreeCreationOutcome::Created { .. })
            ),
            "{report:?}"
        );
        assert!(report.detail.contains("higher-priority"), "{report:?}");
        assert!(!request.destination.exists());
        assert!(
            service
                .resolve_optional_revision(&root, "refs/heads/conflict", &cancel)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fs::read(root.join(".gitignore")).unwrap(),
            b"!/.worktrees/\n"
        );
    }
}
