//! Clone-local ignored-directory policy, separate from content inspection.
//!
//! The caller admits the repository, captures its original common/admin/checkout
//! roots and verifies the current config. These methods use only those roots;
//! they never discover authority again through a mutable checkout `.git`.
use super::*;
use std::os::unix::fs::MetadataExt;

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

/// Preserve order, duplicates and explicit empty overrides. A name identifies a
/// directory component, never a path or a regular file with that basename.
fn validate_names(names: &[String]) -> Result<(), ValidationError> {
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

fn verify_roots(
    common: &fs_safety::Root,
    admin: &fs_safety::Root,
    checkout: &fs_safety::Root,
) -> Result<(), RepositoryError> {
    for root in [common, admin, checkout] {
        let held = root
            .metadata()
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        let named = fs::symlink_metadata(root.path())
            .map_err(|error| ValidationError::Io(error.to_string()))?;
        if !named.is_dir() || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
            return Err(ValidationError::Io(
                "An original repository directory changed; refresh the worktree policy request"
                    .into(),
            )
            .into());
        }
    }
    Ok(())
}

fn require_complete_inventory(output: &ProcessOutput) -> Result<(), RepositoryError> {
    if output.stderr.iter().any(|byte| !byte.is_ascii_whitespace()) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("Git inventory warning");
        return Err(RepositoryError::InvalidOutput {
            field: "worktree_policy",
            reason: format!(
                "Git could not inspect every file; worktree was kept: {}",
                line.chars().take(FAILURE_DETAIL_LIMIT).collect::<String>()
            ),
        });
    }
    Ok(())
}

impl<E: ProcessExecutor> RepositoryService<E> {
    fn policy_command_at(
        &self,
        common: &fs_safety::Root,
        admin: &fs_safety::Root,
        checkout: &fs_safety::Root,
        read: bool,
        literal: bool,
    ) -> CommandSpec {
        let mut git_dir = OsString::from("--git-dir=");
        git_dir.push(admin.process_path());
        let mut work_tree = OsString::from("--work-tree=");
        work_tree.push(checkout.process_path());
        let base = self
            .git_base(&checkout.process_path(), literal)
            .args([git_dir, work_tree])
            .env("GIT_COMMON_DIR", common.process_path());
        if read { read_options(base) } else { base }
    }

    fn run_policy_at(
        &self,
        common: &fs_safety::Root,
        admin: &fs_safety::Root,
        checkout: &fs_safety::Root,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, RepositoryError> {
        verify_roots(common, admin, checkout)?;
        // Roots remain borrowed until this synchronous child has returned.
        // /proc/<parent>/fd paths name its retained descriptors, not child FDs.
        let result = self.run(command, cancellation);
        verify_roots(common, admin, checkout)?;
        Ok(result?)
    }

    pub(crate) fn disposable_ignored_directories_at(
        &self,
        common: &fs_safety::Root,
        admin: &fs_safety::Root,
        checkout: &fs_safety::Root,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>, RepositoryError> {
        let command = self
            .policy_command_at(common, admin, checkout, true, false)
            .args(["config", "--local", "--get", DISPOSABLE_CONFIG]);
        let output = self.run_policy_at(common, admin, checkout, &command, cancellation)?;
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
        validate_names(&names)?;
        Ok(names)
    }

    /// One validated JSON value, written by Git's existing config transaction.
    /// This is clone-common local config, not global or `config.worktree`.
    pub(crate) fn set_disposable_ignored_directories_at(
        &self,
        common: &fs_safety::Root,
        admin: &fs_safety::Root,
        checkout: &fs_safety::Root,
        names: &[String],
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        validate_names(names)?;
        let value =
            serde_json::to_string(names).map_err(|error| ValidationError::Io(error.to_string()))?;
        let command = self
            .policy_command_at(common, admin, checkout, false, false)
            .args(["config", "--local", DISPOSABLE_CONFIG, &value]);
        let output = self.run_policy_at(common, admin, checkout, &command, cancellation)?;
        if !output.success {
            return Err(git_failure(&command, output).into());
        }
        Ok(())
    }

    /// Use the exact policy captured by removal, independently of policy I/O.
    /// Collapse disposable directories first, then expand only other directories
    /// in bounded literal batches. An incomplete inventory never permits removal.
    pub(crate) fn notable_ignored_files_at(
        &self,
        common: &fs_safety::Root,
        admin: &fs_safety::Root,
        checkout: &fs_safety::Root,
        directories: &[String],
        cancellation: &CancellationToken,
    ) -> Result<Vec<RepoPath>, RepositoryError> {
        validate_names(directories)?;
        let names: HashSet<_> = directories.iter().map(OsStr::new).collect();
        let disposable = |path: &RepoPath, directory: bool| {
            let path = Path::new(path.as_os_str());
            let directory = if directory { Some(path) } else { path.parent() };
            directory
                .into_iter()
                .flat_map(Path::components)
                .any(|component| names.contains(component.as_os_str()))
        };
        let command = self
            .policy_command_at(common, admin, checkout, true, true)
            .args([
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "--no-empty-directory",
                "-z",
            ]);
        let output = self.run_policy_at(common, admin, checkout, &command, cancellation)?;
        if !output.success {
            return Err(git_failure(&command, output).into());
        }
        require_complete_inventory(&output)?;
        let mut notable = std::collections::BTreeSet::new();
        let mut expand = Vec::new();
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
                expand.push(path);
            } else {
                notable.insert(path);
            }
        }
        for batch in expand.chunks(64) {
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
            let command = self
                .policy_command_at(common, admin, checkout, true, true)
                .args(arguments);
            let output = self.run_policy_at(common, admin, checkout, &command, cancellation)?;
            if !output.success {
                return Err(git_failure(&command, output).into());
            }
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
                        field: "worktree_policy",
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

    #[derive(Default)]
    struct Script {
        commands: Mutex<Vec<CommandSpec>>,
        outputs: Mutex<std::collections::VecDeque<ProcessOutput>>,
    }
    impl ProcessExecutor for Script {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            self.commands.lock().unwrap().push(command.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }
    struct Fixture {
        _temp: tempfile::TempDir,
        common: fs_safety::Root,
        admin: fs_safety::Root,
        checkout: fs_safety::Root,
        script: Arc<Script>,
    }
    impl Fixture {
        fn new(outputs: Vec<ProcessOutput>) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let [common, admin, checkout] = ["common", "admin", "checkout"].map(|name| {
                let path = temp.path().join(name);
                fs::create_dir(&path).unwrap();
                fs_safety::Root::open(&path).unwrap()
            });
            Self {
                _temp: temp,
                common,
                admin,
                checkout,
                script: Arc::new(Script {
                    outputs: Mutex::new(outputs.into()),
                    ..Default::default()
                }),
            }
        }
        fn get(&self) -> Result<Vec<String>, RepositoryError> {
            RepositoryService::new("git", self.script.clone()).disposable_ignored_directories_at(
                &self.common,
                &self.admin,
                &self.checkout,
                &CancellationToken::new(),
            )
        }
        fn set(&self, names: &[String]) -> Result<(), RepositoryError> {
            RepositoryService::new("git", self.script.clone())
                .set_disposable_ignored_directories_at(
                    &self.common,
                    &self.admin,
                    &self.checkout,
                    names,
                    &CancellationToken::new(),
                )
        }
        fn notable(&self, names: &[String]) -> Result<Vec<RepoPath>, RepositoryError> {
            RepositoryService::new("git", self.script.clone()).notable_ignored_files_at(
                &self.common,
                &self.admin,
                &self.checkout,
                names,
                &CancellationToken::new(),
            )
        }
    }
    fn output(code: i32, stdout: &[u8], stderr: &[u8]) -> ProcessOutput {
        ProcessOutput {
            exit_code: Some(code),
            signal: None,
            success: code == 0,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn defaults_only_for_missing_local_policy_and_explicit_values_stay_exact() {
        let fixture = Fixture::new(vec![output(1, b"", b"")]);
        assert_eq!(
            fixture.get().unwrap(),
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
        );
        for names in [
            Vec::<String>::new(),
            vec![
                "target".into(),
                "target".into(),
                "space cache".into(),
                "缓存".into(),
                "-cache".into(),
            ],
        ] {
            let json = serde_json::to_vec(&names).unwrap();
            let fixture = Fixture::new(vec![output(0, &json, b"")]);
            assert_eq!(fixture.get().unwrap(), names);
            let commands = fixture.script.commands.lock().unwrap();
            assert_eq!(commands.len(), 1);
            assert!(commands[0].arguments.ends_with(&[
                "config".into(),
                "--local".into(),
                "--get".into(),
                DISPOSABLE_CONFIG.into()
            ]));
        }
    }

    #[test]
    fn invalid_policy_fails_closed_without_mutation_or_default_fallback() {
        for name in [
            "",
            ".",
            "..",
            "../cache",
            "a/b",
            "a\\b",
            "new\nline",
            "nul\0name",
            "\u{85}",
        ] {
            let fixture = Fixture::new(Vec::new());
            assert!(fixture.set(&["valid".into(), name.into()]).is_err());
            assert!(fixture.script.commands.lock().unwrap().is_empty());
        }
        for bytes in [b"not JSON".as_slice(), b"{}", b"[\"../cache\"]"] {
            let fixture = Fixture::new(vec![output(0, bytes, b"")]);
            assert!(fixture.get().is_err());
            assert_eq!(fixture.script.commands.lock().unwrap().len(), 1);
        }
        let fixture = Fixture::new(vec![output(128, b"", b"cannot read config")]);
        assert!(fixture.get().is_err());
    }

    #[test]
    fn one_json_write_uses_original_clone_common_admin_and_checkout() {
        let fixture = Fixture::new(vec![output(0, b"", b"")]);
        // Mutable checkout discovery must never supply any of the roots.
        fs::write(
            fixture.checkout.path().join(".git"),
            "gitdir: /another/repo",
        )
        .unwrap();
        let names = vec!["space cache".into(), "-cache".into(), ".private".into()];
        fixture.set(&names).unwrap();
        let commands = fixture.script.commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        let command = &commands[0];
        assert_eq!(
            command.environment.get(OsStr::new("GIT_COMMON_DIR")),
            Some(&fixture.common.process_path().into_os_string())
        );
        for (prefix, root) in [
            ("--git-dir=", &fixture.admin),
            ("--work-tree=", &fixture.checkout),
        ] {
            let mut expected = OsString::from(prefix);
            expected.push(root.process_path());
            assert!(command.arguments.contains(&expected));
        }
        assert!(command.arguments.ends_with(&[
            "config".into(),
            "--local".into(),
            DISPOSABLE_CONFIG.into(),
            serde_json::to_string(&names).unwrap().into()
        ]));
        assert!(
            !command
                .arguments
                .iter()
                .any(|arg| arg == "ls-files" || arg == "status")
        );
    }

    #[test]
    fn notable_inventory_excludes_directory_components_but_keeps_same_named_files() {
        let fixture = Fixture::new(vec![
            output(
                0,
                b"cache.data\0cache/\0node_modules/\0scope/node_modules\0target\0z/\0",
                b"",
            ),
            output(
                0,
                b"cache/target/generated\0cache/user.data\0z/node_modules/output\0z/user.data\0",
                b"",
            ),
        ]);
        let mut names = default_disposable_ignored_directories();
        names.push("cache.data".into());
        assert_eq!(
            fixture.notable(&names).unwrap(),
            [
                "cache.data",
                "cache/user.data",
                "scope/node_modules",
                "target",
                "z/user.data"
            ]
            .map(|s| RepoPath::parse(s).unwrap())
        );
        let commands = fixture.script.commands.lock().unwrap();
        assert_eq!(commands.len(), 2);
        assert!(commands.iter().all(|command| {
            command
                .arguments
                .contains(&OsString::from("--literal-pathspecs"))
                && !command.arguments.contains(&OsString::from("config"))
        }));
        assert!(
            commands[1]
                .arguments
                .ends_with(&["--".into(), "cache".into(), "z".into()])
        );
    }

    #[test]
    fn incomplete_inventory_and_replaced_roots_never_return_a_complete_result() {
        for response in [
            output(0, b"", b"warning: unreadable directory"),
            output(0, b"missing-nul", b""),
            output(0, b"../outside\0", b""),
        ] {
            let fixture = Fixture::new(vec![response]);
            assert!(fixture.notable(&[]).is_err());
        }
        let fixture = Fixture::new(vec![
            output(0, b"cache/\0", b""),
            output(0, b"cache/\0", b""),
        ]);
        assert!(
            fixture.notable(&[]).is_err(),
            "unexpanded nondisposable directory must refuse"
        );
        let fixture = Fixture::new(Vec::new());
        fs::rename(
            fixture.checkout.path(),
            fixture._temp.path().join("original-checkout"),
        )
        .unwrap();
        fs::create_dir(fixture.checkout.path()).unwrap();
        assert!(fixture.get().is_err());
        assert!(fixture.script.commands.lock().unwrap().is_empty());

        struct ReplaceDuringRead {
            path: PathBuf,
            retained: PathBuf,
        }
        impl ProcessExecutor for ReplaceDuringRead {
            fn execute(
                &self,
                _: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                fs::rename(&self.path, &self.retained).unwrap();
                fs::create_dir(&self.path).unwrap();
                Ok(output(0, b"[]", b""))
            }
        }
        let fixture = Fixture::new(Vec::new());
        let service = RepositoryService::new(
            "git",
            Arc::new(ReplaceDuringRead {
                path: fixture.checkout.path().into(),
                retained: fixture._temp.path().join("retained-checkout"),
            }),
        );
        assert!(
            service
                .disposable_ignored_directories_at(
                    &fixture.common,
                    &fixture.admin,
                    &fixture.checkout,
                    &CancellationToken::new(),
                )
                .is_err(),
            "a post-command root replacement cannot publish a valid policy result"
        );
    }

    #[test]
    fn linked_checkout_policy_stays_clone_local_and_drives_real_ignored_inventory() {
        if isolated_git_test(concat!(
            module_path!(),
            "::linked_checkout_policy_stays_clone_local_and_drives_real_ignored_inventory"
        )) {
            return;
        }
        struct PrivateGit {
            global: PathBuf,
        }
        impl ProcessExecutor for PrivateGit {
            fn execute(
                &self,
                command: &CommandSpec,
                cancellation: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                SystemProcessExecutor.execute(
                    &command.clone().env("GIT_CONFIG_GLOBAL", &self.global),
                    cancellation,
                )
            }
        }
        let temp = retained_managed_fixture_directory();
        let global = temp.path().join("global-config");
        let executor = Arc::new(PrivateGit {
            global: global.clone(),
        });
        let run = |path: &Path, args: &[&str]| {
            let command = CommandSpec::new("git")
                .arg("-C")
                .arg(path)
                .args([
                    "-c",
                    "user.name=Policy Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                ])
                .args(args.iter().copied());
            executor
                .execute(&command, &CancellationToken::new())
                .unwrap()
                .require_success(&command.program)
                .unwrap()
                .stdout
        };
        let primary = temp.path().join("primary");
        let other = temp.path().join("other");
        let linked = temp.path().join("linked");
        for path in [&primary, &other] {
            fs::create_dir(path).unwrap();
            run(path, &["init", "-b", "main"]);
        }
        run(
            &primary,
            &["config", "--global", DISPOSABLE_CONFIG, "[\"global-only\"]"],
        );
        let global_before = fs::read(&global).unwrap();
        fs::write(
            primary.join(".gitignore"),
            "node_modules\ntarget/\ncache/\n",
        )
        .unwrap();
        run(&primary, &["add", ".gitignore"]);
        run(&primary, &["commit", "-m", "fixture"]);
        run(
            &primary,
            &[
                "worktree",
                "add",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        );
        let common = fs_safety::Root::open(&primary.join(".git")).unwrap();
        let primary_checkout = fs_safety::Root::open(&primary).unwrap();
        let admin = PathBuf::from(
            String::from_utf8(run(&linked, &["rev-parse", "--absolute-git-dir"]))
                .unwrap()
                .trim(),
        );
        let admin = fs_safety::Root::open(&admin).unwrap();
        let checkout = fs_safety::Root::open(&linked).unwrap();
        let foreign_common = fs_safety::Root::open(&other.join(".git")).unwrap();
        let foreign_checkout = fs_safety::Root::open(&other).unwrap();
        let service = RepositoryService::new("git", executor);
        let cancel = CancellationToken::new();
        assert_eq!(
            service
                .disposable_ignored_directories_at(&common, &admin, &checkout, &cancel)
                .unwrap(),
            default_disposable_ignored_directories(),
            "global config is never a missing-local fallback"
        );
        // Even if the checkout locator redirects, explicit retained roots can
        // address only the original clone; admission remains the caller's job.
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", other.join(".git").display()),
        )
        .unwrap();
        let names = vec!["cache".into(), "cache".into()];
        service
            .set_disposable_ignored_directories_at(&common, &admin, &checkout, &names, &cancel)
            .unwrap();
        assert_eq!(
            service
                .disposable_ignored_directories_at(&common, &common, &primary_checkout, &cancel)
                .unwrap(),
            names
        );
        assert_eq!(
            service
                .disposable_ignored_directories_at(
                    &foreign_common,
                    &foreign_common,
                    &foreign_checkout,
                    &cancel
                )
                .unwrap(),
            default_disposable_ignored_directories()
        );
        assert!(!admin.path().join("config.worktree").exists());
        fs::create_dir_all(linked.join("cache/nested")).unwrap();
        fs::write(linked.join("cache/nested/output"), "generated").unwrap();
        fs::write(linked.join("node_modules"), "same-named user file").unwrap();
        assert_eq!(
            service
                .notable_ignored_files_at(&common, &admin, &checkout, &names, &cancel)
                .unwrap(),
            vec![RepoPath::parse("node_modules").unwrap()]
        );
        service
            .set_disposable_ignored_directories_at(&common, &admin, &checkout, &[], &cancel)
            .unwrap();
        assert!(
            service
                .disposable_ignored_directories_at(&common, &admin, &checkout, &cancel)
                .unwrap()
                .is_empty(),
            "empty local override must not restore defaults"
        );
        assert_eq!(
            service
                .notable_ignored_files_at(&common, &admin, &checkout, &[], &cancel)
                .unwrap(),
            ["cache/nested/output", "node_modules"].map(|path| RepoPath::parse(path).unwrap())
        );
        assert!(
            service
                .set_disposable_ignored_directories_at(
                    &common,
                    &admin,
                    &checkout,
                    &["../outside".into()],
                    &cancel
                )
                .is_err()
        );
        assert!(
            service
                .disposable_ignored_directories_at(&common, &admin, &checkout, &cancel)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&global).unwrap(), global_before);
        assert!(!common.path().join("config.lock").exists());
    }
}
