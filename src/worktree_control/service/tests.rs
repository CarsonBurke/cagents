//! Isolated real-Git service contracts, including controlled setup-holder
//! PTY cases. No application, UI or provider is launched.
use super::*;
use crate::process::{CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput};
use std::{ffi::OsStr, fs, io::Write};

struct Fixture {
    _root: tempfile::TempDir,
    repository: PathBuf,
    store: StateStore,
    settings: crate::settings::AppSettings,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let repository = root.path().join("repository");
        fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "-q", "-b", "main"]);
        fs::write(repository.join("tracked"), "original\n").unwrap();
        git(&repository, &["add", "tracked"]);
        git(&repository, &["commit", "-qm", "base"]);
        let remote = root.path().join("remote.git");
        git(
            &repository,
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repository, &["push", "-q", "origin", "main"]);
        let store = StateStore::open(
            root.path().join("journal.sqlite3"),
            crate::model::BootId::new("worktree-service-fixture"),
        )
        .unwrap();
        let settings = crate::settings::AppSettings {
            managed_worktree_directory: root.path().join("managed").to_str().unwrap().into(),
            ..Default::default()
        };
        Self {
            _root: root,
            repository,
            store,
            settings,
        }
    }
    fn operation(&self, branch: &str, no_setup: bool) -> Operation {
        Operation::Create {
            repository: self.repository.clone(),
            branch: branch.into(),
            base: Some("main".into()),
            linear: None,
            no_setup,
        }
    }
    fn execute(&self, operation: Operation) -> Execution {
        execute_with(
            &RepositoryService::system("git"),
            operation,
            &self.store,
            |_| Ok(Vec::new()),
            Some(self.settings.clone()),
        )
        .unwrap_or_else(|error| panic!("service refused: {error}"))
    }
    fn create(&self, branch: &str) -> Created {
        let Execution::Reply {
            reply: Reply::Created(created),
            setup: None,
        } = self.execute(self.operation(branch, true))
        else {
            panic!("unexpected result");
        };
        assert!(created.error.is_none(), "{created:?}");
        created
    }
}
fn git(directory: &Path, arguments: &[&str]) -> String {
    let command = crate::worktree_branch::git_command(directory)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
        ])
        .args(arguments.iter().copied())
        .timeout(std::time::Duration::from_secs(15));
    let output = SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap();
    assert!(
        output.success,
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}
fn isolated(name: &str) -> bool {
    crate::repository::test_fixture::isolated_fixture_case(name)
}

#[test]
fn original_policy_routes_remain_available_with_a_broken_worktree_index() {
    if isolated(concat!(
        module_path!(),
        "::original_policy_routes_remain_available_with_a_broken_worktree_index"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let created = fixture.create("policy");
    let service = RepositoryService::system("git");
    let admin = service
        .git_dir(&created.path, &CancellationToken::new())
        .unwrap();
    fs::write(admin.join("index"), b"broken index").unwrap();
    let get = || Operation::GetDisposableIgnoredDirectories {
        repository: fixture.repository.clone(),
        path: created.path.clone(),
    };
    let Execution::Reply {
        reply: Reply::DisposableIgnoredDirectories { directories },
        ..
    } = fixture.execute(get())
    else {
        panic!("wrong policy reply");
    };
    assert_eq!(
        directories,
        crate::repository::default_disposable_ignored_directories()
    );
    let set = |directories| Operation::SetDisposableIgnoredDirectories {
        repository: fixture.repository.clone(),
        path: created.path.clone(),
        directories,
    };
    let Execution::Reply {
        reply: Reply::DisposableIgnoredDirectories { directories },
        ..
    } = fixture.execute(set(vec!["cache".into(), "cache ü".into()]))
    else {
        panic!("wrong policy reply");
    };
    assert_eq!(directories, ["cache", "cache ü"]);
    assert!(
        execute_with(
            &service,
            set(vec!["../escape".into()]),
            &fixture.store,
            |_| Ok(Vec::new()),
            None
        )
        .is_err()
    );
    let Execution::Reply {
        reply: Reply::DisposableIgnoredDirectories { directories },
        ..
    } = fixture.execute(get())
    else {
        panic!("wrong policy reply");
    };
    assert_eq!(directories, ["cache", "cache ü"]);
    let Execution::Reply {
        reply: Reply::DisposableIgnoredDirectories { directories },
        ..
    } = fixture.execute(set(Vec::new()))
    else {
        panic!("wrong policy reply");
    };
    assert!(
        directories.is_empty(),
        "explicit empty policy must not become defaults"
    );
}

#[test]
fn original_disposable_directory_policy_is_used_by_actual_guarded_removal() {
    if isolated(concat!(
        module_path!(),
        "::original_disposable_directory_policy_is_used_by_actual_guarded_removal"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let service = RepositoryService::system("git");
    let cancel = CancellationToken::new();
    let created = fixture.create("disposable");
    let common = service.common_dir(&created.path, &cancel).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(common.join("info/exclude"))
        .unwrap()
        .write_all(b"\nnode_modules/\ncache\n")
        .unwrap();
    fs::create_dir(created.path.join("node_modules")).unwrap();
    fs::write(created.path.join("node_modules/generated"), b"build output").unwrap();
    let prepared = PreparedRemoval::prepare(
        &service,
        fixture.repository.clone(),
        created.path.to_str().unwrap().into(),
        None,
        false,
        false,
        &cancel,
    )
    .unwrap();
    assert_eq!(
        prepared
            .remove(&fixture.store, |_| Ok(()), || Ok(()))
            .unwrap()
            .path,
        created.path
    );
    assert!(!created.path.exists());
    let created = fixture.create("same-named-file");
    fixture.execute(Operation::SetDisposableIgnoredDirectories {
        repository: fixture.repository.clone(),
        path: created.path.clone(),
        directories: vec!["cache".into()],
    });
    fs::write(created.path.join("cache"), b"user data").unwrap();
    assert!(
        PreparedRemoval::prepare(
            &service,
            fixture.repository.clone(),
            created.path.to_str().unwrap().into(),
            None,
            false,
            false,
            &cancel
        )
        .is_err()
    );
    assert_eq!(fs::read(created.path.join("cache")).unwrap(), b"user data");
}

#[test]
fn changed_policy_or_notable_file_invalidates_original_confirmation_before_removal() {
    if isolated(concat!(
        module_path!(),
        "::changed_policy_or_notable_file_invalidates_original_confirmation_before_removal"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let service = RepositoryService::system("git");
    let cancel = CancellationToken::new();
    let created = fixture.create("policy-race");
    let common = service.common_dir(&created.path, &cancel).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(common.join("info/exclude"))
        .unwrap()
        .write_all(b"\ntarget/\n.env\n")
        .unwrap();
    fs::create_dir(created.path.join("target")).unwrap();
    fs::write(created.path.join("target/output"), b"generated").unwrap();
    let prepared = PreparedRemoval::prepare(
        &service,
        fixture.repository.clone(),
        created.path.to_str().unwrap().into(),
        None,
        false,
        false,
        &cancel,
    )
    .unwrap();
    fixture.execute(Operation::SetDisposableIgnoredDirectories {
        repository: fixture.repository.clone(),
        path: created.path.clone(),
        directories: Vec::new(),
    });
    assert!(
        prepared
            .remove(&fixture.store, |_| Ok(()), || Ok(()))
            .is_err()
    );
    assert!(created.path.exists());
    fs::write(created.path.join(".env"), b"before").unwrap();
    let Execution::Reply {
        reply: Reply::Inspected { snapshot, .. },
        ..
    } = fixture.execute(Operation::Inspect {
        repository: fixture.repository.clone(),
        path: created.path.clone(),
    })
    else {
        panic!("wrong inspect reply");
    };
    let snapshot: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    let fingerprint = snapshot["fingerprint"].as_str().unwrap();
    fs::write(
        created.path.join(".env"),
        b"later user data with same path count",
    )
    .unwrap();
    assert!(
        PreparedRemoval::prepare_confirmed(
            &service,
            fixture.repository.clone(),
            created.path.to_str().unwrap().into(),
            None,
            true,
            false,
            Some(fingerprint),
            &cancel
        )
        .is_err()
    );
    assert!(created.path.join(".env").exists());
}

#[test]
fn restored_inspect_and_policy_refuse_another_physical_repository() {
    if isolated(concat!(
        module_path!(),
        "::restored_inspect_and_policy_refuse_another_physical_repository"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let other = Fixture::new();
    let created = fixture.create("original");
    let replacement = other.create("foreign");
    for operation in [
        Operation::Inspect {
            repository: fixture.repository.clone(),
            path: replacement.path.clone(),
        },
        Operation::SetDisposableIgnoredDirectories {
            repository: fixture.repository.clone(),
            path: replacement.path.clone(),
            directories: vec!["private".into()],
        },
    ] {
        assert!(
            execute_with(
                &RepositoryService::system("git"),
                operation,
                &fixture.store,
                |_| Ok(Vec::new()),
                None
            )
            .is_err()
        );
    }
    let original_marker = fs::read(created.path.join(".git")).unwrap();
    fs::write(
        created.path.join(".git"),
        fs::read(replacement.path.join(".git")).unwrap(),
    )
    .unwrap();
    assert!(
        execute_with(
            &RepositoryService::system("git"),
            Operation::SetDisposableIgnoredDirectories {
                repository: fixture.repository.clone(),
                path: created.path.clone(),
                directories: vec!["private".into()]
            },
            &fixture.store,
            |_| Ok(Vec::new()),
            None
        )
        .is_err()
    );
    fs::write(created.path.join(".git"), original_marker).unwrap();
    let Execution::Reply {
        reply: Reply::DisposableIgnoredDirectories { directories },
        ..
    } = other.execute(Operation::GetDisposableIgnoredDirectories {
        repository: other.repository.clone(),
        path: replacement.path,
    })
    else {
        panic!("wrong policy reply");
    };
    assert_eq!(
        directories,
        crate::repository::default_disposable_ignored_directories()
    );
}

#[test]
fn original_standalone_branch_route_retains_safe_and_exact_force_consent() {
    if isolated(concat!(
        module_path!(),
        "::original_standalone_branch_route_retains_safe_and_exact_force_consent"
    )) {
        return;
    }
    let fixture = Fixture::new();
    git(&fixture.repository, &["branch", "merged"]);
    let remove = |branch: &str, force, expected_oid| Operation::DeleteBranch {
        repository: fixture.repository.clone(),
        branch: branch.into(),
        force,
        expected_oid,
    };
    assert!(matches!(
        fixture.execute(remove("merged", false, None)),
        Execution::Reply {
            reply: Reply::BranchDeleted { .. },
            ..
        }
    ));
    let created = fixture.create("checked-out");
    assert!(matches!(
        fixture.execute(remove("checked-out", true, None)),
        Execution::Reply {
            reply: Reply::BranchDeleteFailed {
                outcome_unknown: false,
                ..
            },
            ..
        }
    ));
    let head = git(&created.path, &["rev-parse", "HEAD"]);
    assert!(matches!(
        fixture.execute(remove("checked-out", true, Some(head.clone()))),
        Execution::Reply {
            reply: Reply::BranchDeleteFailed {
                failure: crate::worktree_branch::BranchDeleteFailure {
                    kind: crate::worktree_branch::BranchDeleteFailureKind::CheckedOutElsewhere,
                    ..
                },
                ..
            },
            ..
        }
    ));
    git(&created.path, &["checkout", "--detach", "-q"]);
    assert!(matches!(
        fixture.execute(remove("checked-out", true, Some("0".repeat(head.len())))),
        Execution::Reply {
            reply: Reply::BranchDeleteFailed { .. },
            ..
        }
    ));
    assert!(matches!(
        fixture.execute(remove("checked-out", true, Some(head))),
        Execution::Reply {
            reply: Reply::BranchDeleted { .. },
            ..
        }
    ));
}

#[test]
fn shared_journal_creation_and_listing_retain_configured_path_and_ticket() {
    if isolated(concat!(
        module_path!(),
        "::shared_journal_creation_and_listing_retain_configured_path_and_ticket"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let mut operation = fixture.operation("topic", true);
    let Operation::Create { linear, .. } = &mut operation else {
        unreachable!()
    };
    *linear = Some("eng-123".into());
    let Execution::Reply {
        reply: Reply::Created(created),
        setup: None,
    } = fixture.execute(operation)
    else {
        panic!("unexpected result");
    };
    assert!(created.error.is_none());
    assert!(
        created
            .path
            .starts_with(Path::new(&fixture.settings.managed_worktree_directory))
    );
    assert_eq!(
        crate::linear::context::resolve_at(&created.path)
            .unwrap()
            .issue_id,
        "ENG-123"
    );
    let report = RepositoryService::system("git")
        .worktree_setup_creation(&fixture.store, created.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(report.state, WorktreeOperationState::Created);
    let Execution::Reply {
        reply: Reply::Listed { worktrees },
        ..
    } = fixture.execute(Operation::List {
        repository: fixture.repository.clone(),
    })
    else {
        panic!("unexpected list");
    };
    assert_eq!(worktrees.len(), 2);
    let row = worktrees
        .iter()
        .find(|row| row.path == created.path)
        .unwrap();
    assert_eq!(row.branch.as_deref(), Some("topic"));
    assert!(!row.primary && row.dirty == Some(false));
    assert_eq!((row.ahead, row.behind), (None, None));
}

#[derive(Default)]
struct CreatedStatusCounts {
    adds: std::sync::atomic::AtomicUsize,
    injected_status_failures: std::sync::atomic::AtomicUsize,
}
#[derive(Clone)]
struct FailedCreatedStatus {
    counts: std::sync::Arc<CreatedStatusCounts>,
}
impl ProcessExecutor for FailedCreatedStatus {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        use std::sync::atomic::Ordering::SeqCst;
        if command
            .arguments
            .windows(2)
            .any(|pair| pair[0] == OsStr::new("worktree") && pair[1] == OsStr::new("add"))
        {
            self.counts.adds.fetch_add(1, SeqCst);
        }
        let status = command
            .arguments
            .iter()
            .any(|value| value == OsStr::new("status"));
        let linked_checkout = command
            .current_dir
            .as_ref()
            .is_some_and(|path| path.join(".git").is_file());
        if status && linked_checkout {
            self.counts.injected_status_failures.fetch_add(1, SeqCst);
            return Err(crate::process::process_failure(
                command,
                crate::process::ProcessFailureKind::Io,
                "injected post-creation status failure",
                false,
            ));
        }
        SystemProcessExecutor.execute(command, cancellation)
    }
}
#[test]
fn status_failure_after_real_commit_returns_created_receipt_without_a_retry() {
    if isolated(concat!(
        module_path!(),
        "::status_failure_after_real_commit_returns_created_receipt_without_a_retry"
    )) {
        return;
    }
    use std::sync::atomic::Ordering::SeqCst;
    let fixture = Fixture::new();
    let counts = std::sync::Arc::new(CreatedStatusCounts::default());
    let service = RepositoryService::new(
        "git",
        std::sync::Arc::new(FailedCreatedStatus {
            counts: counts.clone(),
        }),
    );
    let result = execute_with(
        &service,
        fixture.operation("topic", true),
        &fixture.store,
        |_| Ok(Vec::new()),
        Some(fixture.settings.clone()),
    )
    .unwrap();
    let Execution::Reply {
        reply: Reply::Created(created),
        ..
    } = result
    else {
        panic!("committed creation was lost");
    };
    assert_eq!(
        counts.adds.load(SeqCst),
        1,
        "real creation must not retry Git effects"
    );
    assert_eq!(
        counts.injected_status_failures.load(SeqCst),
        1,
        "the intended post-creation enrichment boundary must actually fail"
    );
    assert!(created.path.join("tracked").is_file());
    assert_eq!(created.branch.as_deref(), Some("topic"));
    assert!(created.worktree.is_none());
    assert!(
        created
            .error
            .as_deref()
            .is_some_and(|error| error.contains("injected post-creation"))
    );
    let report = service
        .worktree_setup_creation(&fixture.store, created.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(report.operation_id, created.operation_id);
    assert_eq!(report.state, WorktreeOperationState::Created);
    let WorktreeCreationOutcome::Created { worktree, .. } = report.outcome.unwrap() else {
        panic!("durable committed outcome was lost");
    };
    assert_eq!(worktree.path, created.path);
    let inventory = git(&fixture.repository, &["worktree", "list", "--porcelain"]);
    assert_eq!(inventory.matches("branch refs/heads/topic").count(), 1);
}

#[test]
fn removal_rechecks_real_content_and_preserves_a_late_busy_owner() {
    if isolated(concat!(
        module_path!(),
        "::removal_rechecks_real_content_and_preserves_a_late_busy_owner"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let created = fixture.create("topic");
    let prepare = || {
        PreparedRemoval::prepare(
            &RepositoryService::system("git"),
            fixture.repository.clone(),
            "topic".into(),
            None,
            false,
            false,
            &CancellationToken::new(),
        )
        .unwrap()
    };
    let prepared = prepare();
    fs::write(created.path.join("late-file"), "must remain").unwrap();
    let failure = prepared
        .remove(&fixture.store, |_| Ok(()), || Ok(()))
        .unwrap_err();
    assert!(failure.outcome_unknown);
    assert_eq!(
        fs::read_to_string(created.path.join("late-file")).unwrap(),
        "must remain"
    );
    fs::remove_file(created.path.join("late-file")).unwrap();
    let failure = prepare()
        .remove(
            &fixture.store,
            |_| Err("late live session".into()),
            || panic!("busy admission must refuse before final check"),
        )
        .unwrap_err();
    assert!(failure.error.contains("late live session"));
    assert!(created.path.is_dir());
    let removed = prepare()
        .remove(&fixture.store, |_| Ok(()), || Ok(()))
        .unwrap();
    assert_eq!(removed.path, created.path);
    assert!(!created.path.exists());
    assert_eq!(
        git(
            &fixture.repository,
            &["rev-parse", "--verify", "refs/heads/topic"]
        )
        .len(),
        40
    );
    let replacement = fixture.create("topic");
    assert_eq!(replacement.path, created.path);
    assert_ne!(replacement.operation_id, created.operation_id);
    assert_eq!(
        prepare()
            .remove(&fixture.store, |_| Ok(()), || Ok(()))
            .unwrap()
            .path,
        replacement.path
    );
    assert!(!replacement.path.exists());
}

#[test]
fn force_removal_keeps_detached_history_and_separates_safe_branch_deletion() {
    if isolated(concat!(
        module_path!(),
        "::force_removal_keeps_detached_history_and_separates_safe_branch_deletion"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let created = fixture.create("topic");
    git(&created.path, &["checkout", "--detach", "-q"]);
    fs::write(created.path.join("tracked"), "detached commit\n").unwrap();
    git(&created.path, &["commit", "-qam", "detached work"]);
    let head = git(&created.path, &["rev-parse", "HEAD"]);
    let service = RepositoryService::system("git");
    assert!(
        PreparedRemoval::prepare(
            &service,
            fixture.repository.clone(),
            created.path.to_str().unwrap().into(),
            None,
            false,
            false,
            &CancellationToken::new()
        )
        .is_err()
    );
    let prepared = PreparedRemoval::prepare(
        &service,
        fixture.repository.clone(),
        created.path.to_str().unwrap().into(),
        None,
        true,
        true,
        &CancellationToken::new(),
    )
    .unwrap();
    let removed = prepared
        .remove(&fixture.store, |_| Ok(()), || Ok(()))
        .unwrap();
    let rescue = removed.rescue_branch.unwrap();
    assert_eq!(
        git(
            &fixture.repository,
            &["rev-parse", &format!("refs/heads/{rescue}")]
        ),
        head
    );
    assert!(removed.branch_deleted.is_none());
    assert!(!created.path.exists());
}

#[test]
fn trusted_setup_binds_exact_recipe_and_a_free_lease_does_not_erase_execution() {
    if isolated(concat!(
        module_path!(),
        "::trusted_setup_binds_exact_recipe_and_a_free_lease_does_not_erase_execution"
    )) {
        return;
    }
    let fixture = Fixture::new();
    fs::create_dir(fixture.repository.join(".cagents")).unwrap();
    fs::write(
        fixture.repository.join(".cagents/worktree.toml"),
        "copy=['.env']\nsetup=['printf ready']\n",
    )
    .unwrap();
    fs::write(
        fixture.repository.join(".env"),
        "PRIVATE_FIXTURE_VALUE=preserved\n",
    )
    .unwrap();
    let plan = crate::worktree_setup::load(&fixture.repository)
        .unwrap()
        .unwrap();
    crate::worktree_setup::trust(&fixture.repository, &plan, &CancellationToken::new()).unwrap();
    let Execution::Reply {
        reply: Reply::Created(created),
        setup: Some(prepared),
    } = fixture.execute(fixture.operation("setup", false))
    else {
        panic!("setup was not retained");
    };
    assert!(created.setup.as_ref().unwrap().error.is_none());
    assert_eq!(
        fs::read_to_string(created.path.join(".env")).unwrap(),
        "PRIVATE_FIXTURE_VALUE=preserved\n"
    );
    let removal = PreparedRemoval::prepare(
        &RepositoryService::system("git"),
        fixture.repository.clone(),
        "setup".into(),
        None,
        true,
        false,
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(
        removal
            .remove(&fixture.store, |_| Ok(()), || Ok(()))
            .is_err(),
        "prepared setup must retain directory protection before publication"
    );
    assert!(created.path.exists());
    let recipe = prepared.recipe.as_ref().unwrap();
    assert!(prepared.custody.matches_recipe(recipe));
    for change in 0..4 {
        let mut changed = recipe.clone();
        match change {
            0 => changed.program = "/bin/false".into(),
            1 => changed.arguments.push("extra".into()),
            2 => {
                changed.environment.insert("UNREVIEWED".into(), "1".into());
            }
            _ => {
                changed.unset_environment.insert("PATH".into());
            }
        }
        assert!(!prepared.custody.matches_recipe(&changed));
    }
    let service = RepositoryService::system("git");
    prepared
        .custody
        .reserve(
            &service,
            &fixture.store,
            crate::model::TerminalId::new(),
            recipe,
        )
        .unwrap();
    assert!(
        prepared
            .custody
            .reserve(
                &service,
                &fixture.store,
                crate::model::TerminalId::new(),
                recipe
            )
            .is_err()
    );
    let admin = service
        .git_dir(&created.path, &CancellationToken::new())
        .unwrap();
    let admin_root = crate::fs_safety::Root::open(&admin).unwrap();
    assert!(crate::repository::worktree_setup_removal_is_quiescent(&admin_root).is_err());
    drop(prepared);
    let marker = admin.join("cagents-setup-creation");
    let marker_bytes = fs::read(&marker).unwrap();
    fs::remove_file(&marker).unwrap();
    assert!(
        crate::repository::worktree_setup_removal_is_quiescent(&admin_root).is_err(),
        "missing generation must not erase retained Reserved authority"
    );
    fs::write(&marker, marker_bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
    let report = service
        .worktree_setup_creation(&fixture.store, created.operation_id)
        .unwrap()
        .unwrap();
    let intent = report.request.unwrap().setup_intent.unwrap();
    assert!(
        service
            .worktree_setup_custody(&fixture.store, created.operation_id, &intent)
            .is_err(),
        "free execution flock must not certify descendant cleanup"
    );
    assert!(
        !created.path.join("ready").exists(),
        "component fixture never executed a shell recipe"
    );
}

#[test]
fn branch_cleanup_cannot_follow_a_rewritten_repository_gitfile() {
    if isolated(concat!(
        module_path!(),
        "::branch_cleanup_cannot_follow_a_rewritten_repository_gitfile"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let root = fixture._root.path();
    let original = root.join("original-checkout");
    let original_admin = root.join("original-admin");
    let replacement = root.join("replacement-checkout");
    let victim = root.join("original-topic");
    git(
        root,
        &[
            "clone",
            "-q",
            "--separate-git-dir",
            original_admin.to_str().unwrap(),
            fixture.repository.to_str().unwrap(),
            original.to_str().unwrap(),
        ],
    );
    git(
        root,
        &[
            "clone",
            "-q",
            fixture.repository.to_str().unwrap(),
            replacement.to_str().unwrap(),
        ],
    );
    git(
        &original,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "topic",
            victim.to_str().unwrap(),
            "main",
        ],
    );
    git(&replacement, &["branch", "topic", "main"]);
    let replacement_before = git(&replacement, &["rev-parse", "refs/heads/topic"]);
    assert_eq!(
        git(&original, &["rev-parse", "refs/heads/topic"]),
        replacement_before
    );
    let gitfile = original.join(".git");
    let old_gitfile = fs::read(&gitfile).unwrap();
    let prepared = PreparedRemoval::prepare(
        &RepositoryService::system("git"),
        original.clone(),
        "topic".into(),
        None,
        false,
        true,
        &CancellationToken::new(),
    )
    .unwrap();
    let result = prepared.remove_with_post_git_checkpoint(
        &fixture.store,
        |_| Ok(()),
        || Ok(()),
        || {
            // Exact causal boundary: successful real removal, then mutable checkout
            // discovery changes before optional safe-d. Neither held directory moves.
            assert!(!victim.exists());
            let temporary = original.join(".git-redirect-proposal");
            fs::write(
                &temporary,
                format!("gitdir: {}\n", replacement.join(".git").display()),
            )
            .unwrap();
            fs::rename(temporary, &gitfile).unwrap();
        },
    );
    fs::write(&gitfile, old_gitfile).unwrap();
    let removed = result.unwrap();
    assert_eq!(removed.path, victim);
    assert_eq!(removed.branch_deleted.as_deref(), Some("topic"));
    assert!(removed.branch_error.is_none());
    assert_eq!(
        git(&replacement, &["rev-parse", "refs/heads/topic"]),
        replacement_before,
        "unreviewed replacement repository branch was mutated"
    );
    assert!(
        !git(
            &original,
            &["for-each-ref", "--format=%(refname)", "refs/heads/topic"]
        )
        .contains("refs/heads/topic")
    );
}

#[test]
fn list_keeps_healthy_and_unavailable_checkout_evidence() {
    if isolated(concat!(
        module_path!(),
        "::list_keeps_healthy_and_unavailable_checkout_evidence"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let healthy = fixture.create("healthy");
    let missing = fixture.create("missing");
    fs::rename(
        &missing.path,
        fixture._root.path().join("parked-missing-checkout"),
    )
    .unwrap();
    let result = execute_with(
        &RepositoryService::system("git"),
        Operation::List {
            repository: fixture.repository.clone(),
        },
        &fixture.store,
        |path| {
            if path == missing.path {
                Err("injected unavailable session census".into())
            } else {
                Ok(Vec::new())
            }
        },
        None,
    )
    .unwrap();
    let Execution::Reply {
        reply: Reply::Listed { worktrees },
        setup: None,
    } = result
    else {
        panic!("inventory was lost");
    };
    assert_eq!(worktrees.len(), 3);
    let healthy = worktrees
        .iter()
        .find(|row| row.path == healthy.path)
        .unwrap();
    assert_eq!(healthy.dirty, Some(false));
    assert!(healthy.status_error.is_none());
    assert_eq!(healthy.live_sessions.as_deref(), Some([].as_slice()));
    let unavailable = worktrees
        .iter()
        .find(|row| row.path == missing.path)
        .unwrap();
    assert_eq!(unavailable.branch.as_deref(), Some("missing"));
    assert_eq!(unavailable.dirty, None, "unavailable must not appear clean");
    assert!(
        unavailable
            .status_error
            .as_deref()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!((unavailable.ahead, unavailable.behind), (None, None));
    assert!(unavailable.live_sessions.is_none());
    assert_eq!(
        unavailable.session_error.as_deref(),
        Some("injected unavailable session census")
    );
    let json = serde_json::to_value(unavailable).unwrap();
    assert!(json["dirty"].is_null() && json["live_sessions"].is_null());
}

#[test]
fn separate_git_metadata_uses_real_primary_setup_sources() {
    if isolated(concat!(
        module_path!(),
        "::separate_git_metadata_uses_real_primary_setup_sources"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let root = fixture._root.path();
    let primary = root.join("separate-primary");
    let metadata = root.join("separate-admin");
    git(
        root,
        &[
            "clone",
            "-q",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
            fixture.repository.to_str().unwrap(),
            primary.to_str().unwrap(),
        ],
    );
    fs::create_dir(primary.join(".cagents")).unwrap();
    fs::write(
        primary.join(".cagents/worktree.toml"),
        "copy=['.env']\nsetup=['printf reviewed']\n",
    )
    .unwrap();
    fs::write(primary.join(".env"), "FIXTURE_ONLY=source-checkout\n").unwrap();
    let operation = Operation::Create {
        repository: metadata.clone(),
        branch: "setup-topic".into(),
        base: Some("main".into()),
        linear: None,
        no_setup: false,
    };
    let service = RepositoryService::system("git");
    let untrusted = execute_with(
        &service,
        operation.clone(),
        &fixture.store,
        |_| Ok(Vec::new()),
        Some(fixture.settings.clone()),
    );
    assert!(
        untrusted.is_err(),
        "real primary setup must not be silently skipped"
    );
    let plan = crate::worktree_setup::load(&primary).unwrap().unwrap();
    crate::worktree_setup::trust(&primary, &plan, &CancellationToken::new()).unwrap();
    let result = execute_with(
        &service,
        operation,
        &fixture.store,
        |_| Ok(Vec::new()),
        Some(fixture.settings.clone()),
    )
    .unwrap();
    let Execution::Reply {
        reply: Reply::Created(created),
        setup: Some(prepared),
    } = result
    else {
        panic!("real primary plan was not retained");
    };
    assert!(created.error.is_none(), "{created:?}");
    assert_eq!(
        fs::read_to_string(created.path.join(".env")).unwrap(),
        "FIXTURE_ONLY=source-checkout\n"
    );
    assert!(
        prepared
            .custody
            .matches_recipe(prepared.recipe.as_ref().unwrap())
    );
    let receipt = service
        .worktree_setup_creation(&fixture.store, created.operation_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt
            .request
            .unwrap()
            .setup_intent
            .unwrap()
            .primary
            .as_deref(),
        Some(primary.as_path())
    );
    assert!(!metadata.join(".env").exists());
    // No holder, shell or provider executes in this component contract.
}

#[test]
fn explicit_skip_can_recover_an_unreadable_current_plan_without_replay() {
    if isolated(concat!(
        module_path!(),
        "::explicit_skip_can_recover_an_unreadable_current_plan_without_replay"
    )) {
        return;
    }
    let fixture = Fixture::new();
    fs::create_dir(fixture.repository.join(".cagents")).unwrap();
    let configuration = fixture.repository.join(".cagents/worktree.toml");
    fs::write(&configuration, "setup=['printf must-not-run']\n").unwrap();
    let original = crate::worktree_setup::load(&fixture.repository)
        .unwrap()
        .unwrap();
    crate::worktree_setup::trust(&fixture.repository, &original, &CancellationToken::new())
        .unwrap();
    let Execution::Reply {
        reply: Reply::Created(created),
        setup: Some(prepared),
    } = fixture.execute(fixture.operation("recover-skip", false))
    else {
        panic!("setup was not prepared");
    };
    let old_run = Some(prepared.custody.run);
    drop(prepared); // no reservation or shell execution has occurred
    let invalid = "setup = [invalid, unfinished\n";
    fs::write(&configuration, invalid).unwrap();
    let Execution::Reply {
        reply: Reply::SetupReview(review),
        setup: None,
    } = fixture.execute(Operation::ReviewSetup {
        repository: fixture.repository.clone(),
        path: created.path.clone(),
    })
    else {
        panic!("missing explicit review");
    };
    assert!(review.current_error.is_some());
    assert!(review.current_plan.is_none());
    assert_eq!(review.intent.creation_id, created.operation_id);
    let result = fixture.execute(Operation::ResumeSetup {
        repository: fixture.repository.clone(),
        path: created.path.clone(),
        creation_id: created.operation_id,
        expected_run: old_run,
        expected_fingerprint: None,
        trust: false,
        skip: true,
    });
    let Execution::Reply {
        reply:
            Reply::SetupPrepared {
                skip: true,
                run_id,
                error: None,
                ..
            },
        setup: None,
    } = result
    else {
        panic!("explicit skip was stranded by invalid nonexecuting config");
    };
    assert_ne!(Some(run_id), old_run);
    let service = RepositoryService::system("git");
    let receipt = service
        .worktree_setup_creation(&fixture.store, created.operation_id)
        .unwrap()
        .unwrap();
    let updated = receipt.request.unwrap().setup_intent.unwrap();
    assert!(updated.skip && updated.copy_done);
    assert_eq!(updated.run_id, Some(run_id));
    assert_eq!(fs::read_to_string(configuration).unwrap(), invalid);
}

fn prepared_setup(fixture: &Fixture, branch: &str, command: &str) -> (Created, PreparedSetup) {
    fs::create_dir_all(fixture.repository.join(".cagents")).unwrap();
    let commands = toml::Value::Array(vec![toml::Value::String(command.into())]);
    fs::write(
        fixture.repository.join(".cagents/worktree.toml"),
        format!("setup = {commands}\n"),
    )
    .unwrap();
    let plan = crate::worktree_setup::load(&fixture.repository)
        .unwrap()
        .unwrap();
    crate::worktree_setup::trust(&fixture.repository, &plan, &CancellationToken::new()).unwrap();
    let Execution::Reply {
        reply: Reply::Created(created),
        setup: Some(prepared),
    } = fixture.execute(fixture.operation(branch, false))
    else {
        panic!("reviewed setup was not prepared");
    };
    (created, prepared)
}
fn skip_original(fixture: &Fixture, created: &Created, run: OperationId) {
    let Execution::Reply {
        reply:
            Reply::SetupPrepared {
                skip: true,
                run_id,
                error: None,
                ..
            },
        setup: None,
    } = fixture.execute(Operation::ResumeSetup {
        repository: fixture.repository.clone(),
        path: created.path.clone(),
        creation_id: created.operation_id,
        expected_run: Some(run),
        expected_fingerprint: None,
        trust: false,
        skip: true,
    })
    else {
        panic!("proved no-launch setup could not be explicitly skipped");
    };
    assert_ne!(run, run_id);
}
#[test]
fn reserved_rename_sync_failure_proves_no_launch_and_allows_fresh_review() {
    if isolated(concat!(
        module_path!(),
        "::reserved_rename_sync_failure_proves_no_launch_and_allows_fresh_review"
    )) {
        return;
    }
    let mut fixture = Fixture::new();
    fixture._root.disable_cleanup(true);
    let (created, prepared) =
        prepared_setup(&fixture, "reserve-fault", "printf forbidden > executed");
    let run = prepared.custody.run;
    let proof = holder_proof();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _owner = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let terminal = crate::model::TerminalId::new();
    let recipe = prepared.recipe.as_ref().unwrap();
    let source = registered_source(recipe.clone(), terminal, &proof);
    let result = prepared.custody.start(
        &RepositoryService::system("git"),
        &fixture.store,
        terminal,
        recipe,
        |authority| {
            crate::terminal::TerminalHandle::spawn(
                source.clone(),
                setup_config(&source, authority.with_reservation_sync_failure()),
                |_| {},
            )
            .map_err(|error| error.to_string())
        },
    );
    assert!(result.unwrap_err().contains("after rename"));
    assert!(
        !created.path.join("executed").exists(),
        "failed holder reservation must leave no setup command artifact"
    );
    await_original_helpers_reaped(&fixture);
    drop(prepared);
    skip_original(&fixture, &created, run);
    fixture._root.disable_cleanup(false);
}

fn await_original_helpers_reaped(fixture: &Fixture) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crate::daemon::child_reaper::has_outstanding_ownership() {
        assert!(
            std::time::Instant::now() < deadline,
            "original setup holder ownership remains; fixture retained at {}",
            fixture._root.path().display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn registered_source(
    recipe: crate::model::LaunchRecipe,
    terminal: crate::model::TerminalId,
    proof: &crate::model::HostSessionProof,
) -> crate::model::TerminalSession {
    crate::model::TerminalSession {
        id: terminal,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        launch: recipe,
        transcript_path: None,
        resume_hint: None,
        continuity: crate::model::TerminalContinuity::DetachedSameBoot {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        },
    }
}
fn holder_proof() -> crate::model::HostSessionProof {
    crate::model::HostSessionProof {
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_session_id: crate::model::HostSessionId::new("worktree-custody-fixture"),
        host_incarnation: crate::model::HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
    }
}
fn setup_config(
    source: &crate::model::TerminalSession,
    authority: crate::terminal::holder::setup::Authority,
) -> crate::terminal::TerminalConfig {
    crate::terminal::TerminalConfig {
        initially_visible: false,
        setup_authority: Some(authority),
        holder_recovery: Some(crate::terminal::holder::Recovery {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: Default::default(),
            execution_environment: None,
            execution_recipe: None,
        }),
        ..Default::default()
    }
}

#[test]
fn ready_setup_cancelled_before_commit_cannot_create_a_root_and_releases_original_lease() {
    if isolated(concat!(
        module_path!(),
        "::ready_setup_cancelled_before_commit_cannot_create_a_root_and_releases_original_lease"
    )) {
        return;
    }
    let mut fixture = Fixture::new();
    fixture._root.disable_cleanup(true);
    let (created, prepared) =
        prepared_setup(&fixture, "ready-cancel", "printf forbidden > executed");
    let run = prepared.custody.run;
    let proof = holder_proof();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _owner = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let terminal = crate::model::TerminalId::new();
    let recipe = prepared.recipe.as_ref().unwrap();
    let source = registered_source(recipe.clone(), terminal, &proof);
    let prepared_binding = prepared.custody.holder_authority(terminal).unwrap().binding;

    let (ready, observed) = crossbeam_channel::bounded(1);
    let (resume, proceed) = crossbeam_channel::bounded(1);
    std::thread::scope(|scope| {
        let mut worker = None;
        let outcome = prepared.custody.start(
            &RepositoryService::system("git"),
            &fixture.store,
            terminal,
            recipe,
            |authority| {
                let config =
                    setup_config(&source, authority.with_commit_checkpoint(ready, proceed));
                worker =
                    Some(scope.spawn(move || {
                        crate::terminal::TerminalHandle::spawn(source, config, |_| {})
                    }));
                observed
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .map_err(|_| "actual original holder never reached Ready".to_owned())?;
                assert_eq!(fs::read(prepared_binding.admin_path.join("cagents-setup-execution")).unwrap(), prepared_binding.predecessor, "Ready must retain the exact inactive predecessor, not orphan Reserved before an owner commits");
                // Ready proves actual typed descriptors/subreaper exist, but no
                // PTY/root does. This original attempt must permanently cancel a
                // late worker before its CommitSetup CAS and private wire send.
                Err::<(), _>("injected supervisor startup failure after actual Ready".to_owned())
            },
        );
        assert!(outcome.unwrap_err().contains("after actual Ready"));
        resume.send(()).unwrap();
        assert!(worker.take().unwrap().join().unwrap().is_err());
    });
    assert!(
        !created.path.join("executed").exists(),
        "cancelled worker executed a late shell"
    );
    await_original_helpers_reaped(&fixture);
    drop(prepared);
    skip_original(&fixture, &created, run);
    fixture._root.disable_cleanup(false);
}

#[test]
fn canonical_journal_writer_fences_run_replacement_through_startup() {
    if isolated(concat!(
        module_path!(),
        "::canonical_journal_writer_fences_run_replacement_through_startup"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let (created, prepared) =
        prepared_setup(&fixture, "writer-fence", "printf forbidden > executed");
    let service = RepositoryService::system("git");
    let original = service
        .worktree_setup_creation(&fixture.store, created.operation_id)
        .unwrap()
        .unwrap()
        .request
        .unwrap()
        .setup_intent
        .unwrap();
    let other = StateStore::open(
        fixture._root.path().join("journal.sqlite3"),
        fixture.store.current_boot_id().clone(),
    )
    .unwrap();
    let (entered, begin) = crossbeam_channel::bounded(1);
    let (trying, attempted) = crossbeam_channel::bounded(1);
    let (finished, observed) = crossbeam_channel::bounded(1);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            begin.recv().unwrap();
            let mut next = original.clone();
            next.run_id = Some(OperationId::new());
            trying.send(()).unwrap();
            let result = service.update_worktree_setup_intent(
                &other,
                created.operation_id,
                Some(&original),
                next,
            );
            finished.send(result.is_ok()).unwrap();
        });
        let result = prepared.custody.start(
            &service,
            &fixture.store,
            crate::model::TerminalId::new(),
            prepared.recipe.as_ref().unwrap(),
            |_| {
                entered.send(()).unwrap();
                attempted.recv().unwrap();
                assert!(
                    matches!(
                        observed.recv_timeout(std::time::Duration::from_millis(100)),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout)
                    ),
                    "another SQLite connection replaced an in-flight original run"
                );
                Err::<(), _>("injected no-launch end of startup".into())
            },
        );
        assert!(result.is_err());
        assert!(
            observed
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        );
        worker.join().unwrap();
    });
    assert!(!created.path.join("executed").exists());
}

#[test]
fn setup_root_exit_reaps_escaped_descendant_before_reporting_completion() {
    if isolated(concat!(
        module_path!(),
        "::setup_root_exit_reaps_escaped_descendant_before_reporting_completion"
    )) {
        return;
    }
    let mut fixture = Fixture::new();
    fixture._root.disable_cleanup(true);
    let script = "import os,signal,time\npid=os.fork()\nif pid==0:\n os.setsid();signal.signal(signal.SIGHUP,signal.SIG_IGN)\n with open('escaped.pid.tmp','w') as f: f.write(str(os.getpid()));f.flush();os.fsync(f.fileno())\n os.rename('escaped.pid.tmp','escaped.pid')\n while True: time.sleep(1)\nelse:\n while not os.path.exists('escaped.pid'): time.sleep(.005)\n print('fixture-ready',flush=True)\n input()\n";
    let command = format!("/usr/bin/python3 -c '{}'", script.replace('\'', "'\\''"));
    let (created, prepared) = prepared_setup(&fixture, "family-cleanup", &command);
    let proof = holder_proof();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _owner = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let terminal = crate::model::TerminalId::new();
    let recipe = prepared.recipe.as_ref().unwrap();
    let source = registered_source(recipe.clone(), terminal, &proof);
    let handle = prepared
        .custody
        .start(
            &RepositoryService::system("git"),
            &fixture.store,
            terminal,
            recipe,
            |authority| {
                crate::terminal::TerminalHandle::spawn(
                    source.clone(),
                    setup_config(&source, authority),
                    |_| {},
                )
                .map_err(|error| error.to_string())
            },
        )
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !created.path.join("escaped.pid").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let pid: i32 = fs::read_to_string(created.path.join("escaped.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let descendant = crate::daemon::holder_registry::Process::read(pid).unwrap();
    assert!(descendant.is_current());
    handle.input(b"\n".to_vec()).unwrap();
    let events = handle.events();
    let mut successful_exit = false;
    while std::time::Instant::now() < deadline {
        if let Ok(event) = events.recv_timeout(std::time::Duration::from_millis(100)) {
            if let crate::terminal::TerminalEvent::Exited {
                exit_code: 0,
                signal: None,
                ..
            } = event
            {
                assert!(
                    !descendant.is_current(),
                    "setup success preceded exact escaped-descendant cleanup"
                );
                successful_exit = true;
                break;
            }
        }
    }
    assert!(
        successful_exit,
        "original setup root did not report successful exit after family cleanup"
    );
    assert!(!descendant.is_current());
    let holder = handle.holder_identity().unwrap().holder.clone();
    handle.close_with_receipt(&source, &proof).unwrap();
    let release_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while holder.is_current() {
        assert!(
            std::time::Instant::now() < release_deadline,
            "original setup holder retained; fixture preserved at {}",
            fixture._root.path().display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(handle);
    drop(prepared);
    fixture._root.disable_cleanup(false);
}
