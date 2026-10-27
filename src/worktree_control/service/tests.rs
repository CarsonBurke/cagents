//! In-process service contracts with real Git; no daemon, UI, PTY or provider.
use super::*;
use crate::process::{CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput};
use std::{ffi::OsStr, fs};

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

#[derive(Clone)]
struct FailedCreatedStatus;
impl ProcessExecutor for FailedCreatedStatus {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let status = command
            .arguments
            .iter()
            .any(|value| value == OsStr::new("status"));
        let settled = command
            .current_dir
            .as_ref()
            .is_some_and(|path| path.join(".git").is_file());
        if status && settled {
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
    let fixture = Fixture::new();
    let service = RepositoryService::new("git", std::sync::Arc::new(FailedCreatedStatus));
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
    assert_eq!(report.state, WorktreeOperationState::Created);
    assert_eq!(
        git(&fixture.repository, &["worktree", "list", "--porcelain"])
            .matches("branch refs/heads/topic")
            .count(),
        1
    );
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
    drop(prepared);
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
