use super::*;
use std::{
    process::{Command, Output},
    sync::Mutex,
};

type Interceptor = Box<dyn FnMut(&CommandSpec) + Send>;

#[derive(Default)]
struct NativeExecutor {
    interceptor: Mutex<Option<Interceptor>>,
    fetch_failure: Mutex<Option<Result<ProcessOutput, ProcessFailure>>>,
}

impl ProcessExecutor for NativeExecutor {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        if let Some(interceptor) = &mut *self.interceptor.lock().unwrap() {
            interceptor(command);
        }
        if command.arguments.iter().any(|argument| argument == "fetch") {
            if let Some(failure) = self.fetch_failure.lock().unwrap().take() {
                return failure;
            }
        }
        let inherited_count = command
            .environment
            .get(OsStr::new("GIT_CONFIG_COUNT"))
            .cloned()
            .unwrap_or_else(|| "0".into());
        let command = command
            .clone()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", inherited_count)
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.test");
        SystemProcessExecutor.execute(&command, cancellation)
    }
}

fn run_git(directory: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "0")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_text(directory: &Path, args: &[&str]) -> String {
    String::from_utf8(run_git(directory, args).stdout)
        .unwrap()
        .trim_end_matches('\n')
        .to_owned()
}

struct Fixture {
    temporary: tempfile::TempDir,
    root: PathBuf,
    bare: PathBuf,
    original: Oid,
    executor: Arc<NativeExecutor>,
    service: RepositoryService<NativeExecutor>,
}

impl Fixture {
    fn new(tracked: bool) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("checkout");
        let bare = temporary.path().join("remote.git");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&bare).unwrap();
        run_git(&root, &["init", "--initial-branch=feature"]);
        run_git(&bare, &["init", "--bare", "--initial-branch=feature"]);
        fs::write(root.join("file"), "old\n").unwrap();
        fs::write(root.join("other"), "unchanged\n").unwrap();
        fs::write(root.join(".gitignore"), "secret\n").unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-qm", "old"]);
        run_git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://fixture.invalid/owner/repo.git",
            ],
        );
        run_git(
            &root,
            &[
                "config",
                &format!("url.file://{}/.insteadOf", bare.display()),
                "https://fixture.invalid/owner/repo.git",
            ],
        );
        run_git(&root, &["push", "origin", "feature"]);
        if tracked {
            run_git(
                &root,
                &["branch", "--set-upstream-to=origin/feature", "feature"],
            );
        }
        let original = Oid::parse_object(git_text(&root, &["rev-parse", "HEAD"])).unwrap();
        let executor = Arc::new(NativeExecutor::default());
        let service = RepositoryService::new("git", executor.clone());
        Self {
            temporary,
            root,
            bare,
            original,
            executor,
            service,
        }
    }

    fn request(&self, expected_head: Oid) -> ExistingPrCheckoutRequest {
        ExistingPrCheckoutRequest {
            repository: self.root.clone(),
            worktree: self.root.clone(),
            branch: ValidatedBranchName::parse("feature").unwrap(),
            head_repository: GitHubRepoKey::new("fixture.invalid", None, "owner", "repo").unwrap(),
            head_branch: ValidatedBranchName::parse("feature").unwrap(),
            expected_head,
            remote: "origin".into(),
            tracking: PrCheckoutTracking::RemoteBranch,
        }
    }

    fn advance(&self, configure: impl FnOnce(&Path)) -> Oid {
        let producer = self.temporary.path().join("producer");
        run_git(
            self.temporary.path(),
            &[
                "clone",
                self.bare.to_str().unwrap(),
                producer.to_str().unwrap(),
            ],
        );
        configure(&producer);
        run_git(&producer, &["add", "--all"]);
        run_git(&producer, &["commit", "-qm", "advance"]);
        run_git(&producer, &["push"]);
        Oid::parse_object(git_text(&producer, &["rev-parse", "HEAD"])).unwrap()
    }

    fn intercept(&self, action: impl FnMut(&CommandSpec) + Send + 'static) {
        *self.executor.interceptor.lock().unwrap() = Some(Box::new(action));
    }

    fn prepare(&self, request: &ExistingPrCheckoutRequest) -> ExistingPrCheckoutPlan {
        self.service
            .prepare_existing_pr_checkout(request, &CancellationToken::default())
            .unwrap()
    }

    fn apply(
        &self,
        plan: &ExistingPrCheckoutPlan,
    ) -> Result<VerifiedPrCheckout, PrCheckoutFailure> {
        self.service
            .reconcile_existing_pr_checkout(plan, &CancellationToken::default())
    }
}

fn is_effect(command: &CommandSpec) -> bool {
    command
        .environment
        .get(OsStr::new("CAGENTS_PR_MODE"))
        .is_some_and(|mode| mode == "effect")
}

#[test]
fn missing_tracking_is_confirmed_atomically_and_bare_push_updates_pr() {
    let fixture = Fixture::new(false);
    run_git(&fixture.root, &["config", "example.unrelated", "preserved"]);
    let path = fixture.root.join(".git/config");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let metadata = fs::metadata(&path).unwrap();
    let before = fs::read(&path).unwrap();
    let request = fixture.request(fixture.original.clone());
    let plan = fixture.prepare(&request);
    assert_eq!(plan.disposition(), PrCheckoutDisposition::RepairTracking);
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "preparation must not repair tracking"
    );
    let verified = fixture.apply(&plan).unwrap();
    assert_eq!(verified.path(), fixture.root);
    assert_eq!(verified.head(), &fixture.original);
    assert_eq!(verified.upstream(), Some("refs/remotes/origin/feature"));
    assert_eq!(
        git_text(&fixture.root, &["config", "example.unrelated"]),
        "preserved"
    );
    let after = fs::metadata(&path).unwrap();
    assert_eq!(
        (after.uid(), after.gid(), after.mode() & 0o7777),
        (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777)
    );
    fs::write(fixture.root.join("file"), "fix\n").unwrap();
    run_git(&fixture.root, &["commit", "-qam", "fix review"]);
    run_git(&fixture.root, &["push"]);
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        git_text(&fixture.bare, &["rev-parse", "refs/heads/feature"])
    );
}

#[test]
fn advanced_pr_fast_forwards_existing_worktree_and_preserves_unrelated_ignored_tree() {
    for tracked in [false, true] {
        let fixture = Fixture::new(tracked);
        fs::create_dir(fixture.root.join("secret")).unwrap();
        fs::write(fixture.root.join("secret/kept"), "ignored local data\n").unwrap();
        let new = fixture.advance(|root| fs::write(root.join("file"), "new\n").unwrap());
        let request = fixture.request(new.clone());
        let plan = fixture.prepare(&request);
        assert_eq!(
            plan.disposition(),
            if tracked {
                PrCheckoutDisposition::FastForward
            } else {
                PrCheckoutDisposition::FastForwardAndRepair
            }
        );
        let verified = fixture.apply(&plan).unwrap();
        assert_eq!(verified.head(), &new);
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            new.as_str()
        );
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "origin/feature"]),
            new.as_str()
        );
        assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"new\n");
        assert_eq!(
            fs::read(fixture.root.join("secret/kept")).unwrap(),
            b"ignored local data\n"
        );
        assert!(git_text(&fixture.root, &["status", "--porcelain"]).is_empty());
    }
}

#[test]
fn repeated_current_checkout_is_verified_without_fetch_or_losing_local_edits() {
    let fixture = Fixture::new(true);
    fs::write(fixture.root.join("other"), "local edit\n").unwrap();
    let plan = fixture.prepare(&fixture.request(fixture.original.clone()));
    assert_eq!(plan.disposition(), PrCheckoutDisposition::AlreadyCurrent);
    fixture.intercept(|command| {
        assert!(!command.arguments.iter().any(|argument| argument == "fetch"))
    });
    fixture.apply(&plan).unwrap();
    assert_eq!(
        fs::read(fixture.root.join("other")).unwrap(),
        b"local edit\n"
    );
}

#[test]
fn readonly_pr_branch_updates_exact_head_without_configuring_upstream() {
    let fixture = Fixture::new(false);
    run_git(&fixture.root, &["branch", "-m", "pr/7"]);
    let new = fixture.advance(|root| fs::write(root.join("file"), "readonly new\n").unwrap());
    let mut request = fixture.request(new.clone());
    request.branch = ValidatedBranchName::parse("pr/7").unwrap();
    request.tracking = PrCheckoutTracking::ReadOnly;
    let verified = fixture.apply(&fixture.prepare(&request)).unwrap();
    assert_eq!(verified.head(), &new);
    assert_eq!(verified.upstream(), None);
    assert!(
        git_text(
            &fixture.root,
            &["for-each-ref", "--format=%(upstream)", "refs/heads/pr/7"]
        )
        .is_empty()
    );
}

#[test]
fn alias_branch_requires_explicit_upstream_push_mode_and_never_rewrites_policy() {
    let fixture = Fixture::new(true);
    run_git(&fixture.root, &["branch", "-m", "alias"]);
    let mut request = fixture.request(fixture.original.clone());
    request.branch = ValidatedBranchName::parse("alias").unwrap();
    let before = fs::read(fixture.root.join(".git/config")).unwrap();
    assert!(
        fixture
            .service
            .prepare_existing_pr_checkout(&request, &CancellationToken::default())
            .unwrap_err()
            .detail
            .contains("push mode")
    );
    assert_eq!(fs::read(fixture.root.join(".git/config")).unwrap(), before);
    run_git(&fixture.root, &["config", "push.default", "upstream"]);
    fixture.apply(&fixture.prepare(&request)).unwrap();
    fs::write(fixture.root.join("file"), "alias fix\n").unwrap();
    run_git(&fixture.root, &["commit", "-qam", "alias fix"]);
    run_git(&fixture.root, &["push"]);
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        git_text(&fixture.bare, &["rev-parse", "feature"])
    );
    assert_eq!(
        git_text(&fixture.root, &["config", "push.default"]),
        "upstream"
    );
}

#[test]
fn partial_conflicting_and_push_override_configuration_refuses_without_mutation() {
    for (key, value) in [
        ("branch.feature.remote", "origin"),
        ("branch.feature.merge", "refs/heads/feature"),
        ("branch.feature.pushRemote", "another"),
        ("remote.pushDefault", "another"),
        ("remote.origin.push", "refs/heads/other:refs/heads/other"),
        ("push.default", "matching"),
    ] {
        let fixture = Fixture::new(false);
        run_git(&fixture.root, &["config", key, value]);
        let before = fs::read(fixture.root.join(".git/config")).unwrap();
        assert!(
            fixture
                .service
                .prepare_existing_pr_checkout(
                    &fixture.request(fixture.original.clone()),
                    &CancellationToken::default()
                )
                .is_err(),
            "{key}"
        );
        assert_eq!(fs::read(fixture.root.join(".git/config")).unwrap(), before);
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
    }
}

#[test]
fn dirty_or_locally_ahead_checkout_refuses_and_preserves_every_edit() {
    for staged in [false, true] {
        let fixture = Fixture::new(true);
        let new = fixture.advance(|root| fs::write(root.join("file"), "PR\n").unwrap());
        fs::write(fixture.root.join("other"), "local\n").unwrap();
        if staged {
            run_git(&fixture.root, &["add", "other"]);
        }
        assert!(
            fixture
                .service
                .prepare_existing_pr_checkout(&fixture.request(new), &CancellationToken::default())
                .unwrap_err()
                .detail
                .contains("local changes")
        );
        assert_eq!(fs::read(fixture.root.join("other")).unwrap(), b"local\n");
    }
    let fixture = Fixture::new(true);
    fs::write(fixture.root.join("file"), "local commit\n").unwrap();
    run_git(&fixture.root, &["commit", "-qam", "local"]);
    let local = git_text(&fixture.root, &["rev-parse", "HEAD"]);
    assert!(
        fixture
            .service
            .prepare_existing_pr_checkout(
                &fixture.request(fixture.original.clone()),
                &CancellationToken::default()
            )
            .unwrap_err()
            .detail
            .contains("ahead or has diverged")
    );
    assert_eq!(git_text(&fixture.root, &["rev-parse", "HEAD"]), local);
}

#[test]
fn same_oid_branch_switch_at_effect_is_refused_under_native_ref_locks() {
    let fixture = Fixture::new(true);
    run_git(&fixture.root, &["branch", "other-branch"]);
    let new = fixture.advance(|root| fs::write(root.join("file"), "new\n").unwrap());
    let plan = fixture.prepare(&fixture.request(new));
    let root = fixture.root.clone();
    let mut injected = false;
    fixture.intercept(move |command| {
        if is_effect(command) && !injected {
            injected = true;
            run_git(&root, &["symbolic-ref", "HEAD", "refs/heads/other-branch"]);
        }
    });
    let failure = fixture.apply(&plan).unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    assert_eq!(
        git_text(&fixture.root, &["symbolic-ref", "HEAD"]),
        "refs/heads/other-branch"
    );
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "feature"]),
        fixture.original.as_str()
    );
    assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"old\n");
}

#[test]
fn concurrent_config_edit_and_dirty_write_at_effect_are_preserved() {
    for config in [false, true] {
        let fixture = Fixture::new(true);
        let new = fixture.advance(|root| fs::write(root.join("file"), "new\n").unwrap());
        let plan = fixture.prepare(&fixture.request(new));
        let root = fixture.root.clone();
        let mut injected = false;
        fixture.intercept(move |command| {
            if is_effect(command) && !injected {
                injected = true;
                if config {
                    run_git(&root, &["config", "example.concurrent", "retained"]);
                } else {
                    fs::write(root.join("file"), "concurrent user edit\n").unwrap();
                }
            }
        });
        let failure = fixture.apply(&plan).unwrap_err();
        assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
        if config {
            assert_eq!(
                git_text(&fixture.root, &["config", "example.concurrent"]),
                "retained"
            );
        } else {
            assert_eq!(
                fs::read(fixture.root.join("file")).unwrap(),
                b"concurrent user edit\n"
            );
        }
    }
}

#[test]
fn ignored_collision_created_after_census_is_preserved_at_checkout_effect() {
    let mut fixture = Fixture::new(true);
    let new = fixture.advance(|root| {
        fs::write(root.join("secret"), "PR secret\n").unwrap();
        run_git(root, &["add", "--force", "secret"]);
    });
    let wrapper = fixture.temporary.path().join("late-collision-git");
    fs::write(&wrapper, r#"#!/bin/sh
set -eu
if [ "${CAGENTS_PR_MODE:-}" = effect ]; then
    for argument do
        if [ "$argument" = read-tree ]; then
            # This runs after the additions/xargs census, immediately before
            # the actual native checkout command. It models an ordinary build
            # or editor creating ignored data without taking Git's locks.
            printf 'late ignored local data\n' > "$CAGENTS_PR_WORKTREE_FD/secret"
            printf injected > "$0.entered"
            break
        fi
    done
fi
exec /usr/bin/git "$@"
"#).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.service = RepositoryService::new(&wrapper, fixture.executor.clone());
    let plan = fixture.prepare(&fixture.request(new));
    let before_index = fs::read(fixture.root.join(".git/index")).unwrap();
    let before_config = fs::read(fixture.root.join(".git/config")).unwrap();
    let result = fixture.apply(&plan);
    assert_eq!(fs::read(wrapper.with_extension("entered")).unwrap(), b"injected");
    assert_eq!(fs::read(fixture.root.join("secret")).unwrap(), b"late ignored local data\n",
        "a clean census must not authorize overwriting data created before checkout");
    assert!(result.is_err(), "the late collision must withhold verified success");
    assert_eq!(git_text(&fixture.root, &["rev-parse", "HEAD"]), fixture.original.as_str());
    assert_eq!(fs::read(fixture.root.join(".git/index")).unwrap(), before_index);
    assert_eq!(fs::read(fixture.root.join(".git/config")).unwrap(), before_config);
}

#[test]
fn ignored_collision_refuses_and_preserves_original_secret() {
    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| {
        fs::write(root.join("secret"), "PR secret\n").unwrap();
        run_git(root, &["add", "--force", "secret"]);
    });
    fs::write(fixture.root.join("secret"), "local ignored secret\n").unwrap();
    let failure = fixture
        .apply(&fixture.prepare(&fixture.request(new)))
        .unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    assert_eq!(
        fs::read(fixture.root.join("secret")).unwrap(),
        b"local ignored secret\n"
    );
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
}

#[test]
fn ignored_symlink_ancestor_refuses_without_touching_its_external_target() {
    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| {
        fs::create_dir(root.join("secret")).unwrap();
        fs::write(root.join("secret/incoming"), "PR data\n").unwrap();
        run_git(root, &["add", "--force", "secret/incoming"]);
    });
    let external = fixture.temporary.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("kept"), "external secret\n").unwrap();
    std::os::unix::fs::symlink(&external, fixture.root.join("secret")).unwrap();
    let failure = fixture
        .apply(&fixture.prepare(&fixture.request(new)))
        .unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    assert_eq!(
        fs::read(external.join("kept")).unwrap(),
        b"external secret\n"
    );
    assert!(!external.join("incoming").exists());
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
}

#[test]
fn plan_debug_never_formats_configuration_bytes_or_credential_urls() {
    let fixture = Fixture::new(true);
    let secret = "fixture-authorization-sentinel";
    let token = "fixture-url-token-sentinel";
    run_git(&fixture.root, &["config", "http.extraHeader", secret]);
    run_git(
        &fixture.root,
        &[
            "config",
            "remote.origin.url",
            &format!("https://{token}@fixture.invalid/owner/repo.git"),
        ],
    );
    let plan = fixture.prepare(&fixture.request(fixture.original.clone()));
    let debug = format!("{plan:?}");
    assert!(!debug.contains(secret));
    assert!(!debug.contains(token));
    // Derived Vec<u8> Debug prints decimal bytes, not the original secret.
    // Rejecting the complete byte-vector representation catches that leak.
    assert!(!debug.contains(&format!("{:?}", plan.config)));
    assert!(debug.contains("config_bytes"));
}

#[test]
fn replacing_worktree_path_at_effect_never_updates_replacement_or_wrong_inode() {
    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| fs::write(root.join("file"), "PR\n").unwrap());
    let plan = fixture.prepare(&fixture.request(new));
    let root = fixture.root.clone();
    let moved = fixture.temporary.path().join("moved-original");
    let original_path = moved.clone();
    let mut injected = false;
    fixture.intercept(move |command| {
        if is_effect(command) && !injected {
            injected = true;
            fs::rename(&root, &moved).unwrap();
            fs::create_dir(&root).unwrap();
            run_git(&root, &["init", "--initial-branch=replacement"]);
            fs::write(root.join("untouched"), "replacement data\n").unwrap();
        }
    });
    let failure = fixture.apply(&plan).unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    assert_eq!(
        fs::read(fixture.root.join("untouched")).unwrap(),
        b"replacement data\n"
    );
    assert_eq!(fs::read(original_path.join("file")).unwrap(), b"old\n");
    assert_eq!(
        git_text(&original_path, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
    assert!(
        failure.detail.contains(original_path.to_str().unwrap()),
        "retained evidence must follow its actual directory"
    );
}

#[test]
fn direct_config_edit_during_staging_is_preserved_and_pair_is_not_published() {
    let fixture = Fixture::new(false);
    let plan = fixture.prepare(&fixture.request(fixture.original.clone()));
    let config = fixture.root.join(".git/config");
    let mut injected = false;
    fixture.intercept(move |command| {
        if !injected
            && command.arguments.iter().any(|arg| arg == "--add")
            && command
                .arguments
                .iter()
                .any(|arg| arg == "branch.feature.merge")
        {
            injected = true;
            fs::OpenOptions::new()
                .append(true)
                .open(&config)
                .unwrap()
                .write_all(b"\n# concurrent editor change\n")
                .unwrap();
        }
    });
    let failure = fixture.apply(&plan).unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    let config = fs::read_to_string(fixture.root.join(".git/config")).unwrap();
    assert!(config.contains("# concurrent editor change"));
    assert!(!config.contains("[branch \"feature\"]"));
}

#[test]
fn concurrent_private_config_snapshot_never_widens_the_named_lock_or_exposes_new_bytes() {
    let fixture = Fixture::new(false);
    let config = fixture.root.join(".git/config");
    fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
    let plan = fixture.prepare(&fixture.request(fixture.original.clone()));
    let old = fs::read(&config).unwrap();
    let mut new = old.clone();
    new.extend_from_slice(b"\n[http]\n extraHeader = private-new-secret\n");
    let expected = new.clone();
    let lock = config.with_extension("lock");
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let changed_hook = changed.clone();
    let checked_hook = checked.clone();
    let config_hook = config.clone();
    effect::RECONCILIATION_TEST_HOOK.with_borrow_mut(|hook| {
        *hook = Some(Box::new(move |phase| {
            if phase == "after_source_metadata" {
                fs::set_permissions(&config_hook, fs::Permissions::from_mode(0o600)).unwrap();
                fs::write(&config_hook, &new).unwrap();
                changed_hook.store(true, std::sync::atomic::Ordering::SeqCst);
            } else if phase == "before_source_validation" {
                assert_eq!(fs::metadata(&lock).unwrap().mode() & 0o777, 0o600);
                assert!(
                    fs::read(&lock).unwrap().is_empty(),
                    "new private bytes must never reach the old public lock"
                );
                checked_hook.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }))
    });
    let result = fixture.apply(&plan);
    effect::RECONCILIATION_TEST_HOOK.with_borrow_mut(|hook| *hook = None);
    let failure = result.unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    assert!(changed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(checked.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(fs::read(&config).unwrap(), expected);
    assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o600);
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
}

#[test]
fn foreign_index_lock_is_preserved_without_publishing_tracking() {
    let fixture = Fixture::new(false);
    let plan = fixture.prepare(&fixture.request(fixture.original.clone()));
    fs::write(
        fixture.root.join(".git/index.lock"),
        b"foreign lock evidence",
    )
    .unwrap();
    let failure = fixture.apply(&plan).unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    assert_eq!(
        fs::read(fixture.root.join(".git/index.lock")).unwrap(),
        b"foreign lock evidence"
    );
    assert!(
        !fs::read_to_string(fixture.root.join(".git/config"))
            .unwrap()
            .contains("[branch \"feature\"]")
    );
}

#[test]
fn original_policy_is_chained_with_its_hook_configuration_and_aborted_effects_are_honest() {
    for reject in ["none", "prepared", "committed"] {
        let fixture = Fixture::new(true);
        let new = fixture.advance(|root| fs::write(root.join("file"), "policy PR\n").unwrap());
        let policy = fixture.root.join(".git/policy quoted' path");
        fs::create_dir(&policy).unwrap();
        run_git(
            &fixture.root,
            &["config", "core.hooksPath", policy.to_str().unwrap()],
        );
        let hook = policy.join("reference-transaction");
        fs::write(&hook, format!("#!/bin/sh\nset -eu\ninput=$(/bin/cat)\nprintf '%s:%s\\n' \"$1\" \"$(git config core.hooksPath)\" >> \"$(git rev-parse --git-path policy-events)\"\nif [ \"$1\" = {reject} ] && printf '%s' \"$input\" | /usr/bin/grep -F ' refs/heads/feature' >/dev/null; then printf '%s\\n' 'fixture policy refusal' >&2; exit 1; fi\n")).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        let outcome = fixture.apply(&fixture.prepare(&fixture.request(new.clone())));
        let events = fs::read_to_string(fixture.root.join(".git/policy-events")).unwrap();
        assert!(
            events
                .lines()
                .all(|line| line.ends_with(policy.to_str().unwrap())),
            "original hooksPath must be restored for delegated Git: {events}"
        );
        match reject {
            "none" => {
                outcome.unwrap();
                assert_eq!(
                    git_text(&fixture.root, &["rev-parse", "HEAD"]),
                    new.as_str()
                );
            }
            "prepared" => {
                assert_eq!(outcome.unwrap_err().kind, PrCheckoutFailureKind::Refused);
                assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"old\n");
            }
            "committed" => {
                assert_eq!(
                    outcome.unwrap_err().kind,
                    PrCheckoutFailureKind::PartialOrUnknown
                );
                assert_eq!(
                    git_text(&fixture.root, &["rev-parse", "HEAD"]),
                    new.as_str()
                );
                assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"policy PR\n");
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn native_addition_guard_preserves_non_utf8_ignored_data_and_refuses_file_to_directory() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new(true);
    let odd = OsString::from_vec(b"odd\xff\n[] -entry".to_vec());
    let incoming = odd.clone();
    let new = fixture.advance(|root| {
        fs::create_dir(root.join("secret")).unwrap();
        fs::write(root.join("secret").join(incoming), b"PR\n").unwrap();
        run_git(root, &["add", "--force", "secret"]);
    });
    fs::create_dir(fixture.root.join("secret")).unwrap();
    fs::write(fixture.root.join("secret").join(&odd), b"user bytes\n").unwrap();
    assert_eq!(
        fixture
            .apply(&fixture.prepare(&fixture.request(new)))
            .unwrap_err()
            .kind,
        PrCheckoutFailureKind::Refused
    );
    assert_eq!(
        fs::read(fixture.root.join("secret").join(&odd)).unwrap(),
        b"user bytes\n"
    );

    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| {
        fs::remove_file(root.join("file")).unwrap();
        fs::create_dir(root.join("file")).unwrap();
        fs::write(root.join("file/child"), b"PR\n").unwrap();
    });
    let failure = fixture
        .apply(&fixture.prepare(&fixture.request(new)))
        .unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"old\n");
}

#[test]
fn new_paths_with_newlines_and_globs_are_checked_and_installed_literally() {
    let fixture = Fixture::new(true);
    let path = "new []\n -entry";
    let new = fixture.advance(|root| fs::write(root.join(path), "literal\n").unwrap());
    fixture
        .apply(&fixture.prepare(&fixture.request(new)))
        .unwrap();
    assert_eq!(fs::read(fixture.root.join(path)).unwrap(), b"literal\n");
}

#[test]
fn truncated_or_over_limit_addition_census_refuses_before_checkout() {
    for payload in ["printf x", "/usr/bin/head -c 16777217 /dev/zero"] {
        let mut fixture = Fixture::new(true);
        let new = fixture.advance(|root| fs::write(root.join("new-path"), "new\n").unwrap());
        let wrapper = fixture.temporary.path().join("bounded-git-wrapper");
        fs::write(&wrapper, format!("#!/bin/sh\nfor argument do if [ \"$argument\" = diff-tree ]; then {payload}; exit 0; fi; done\nexec /usr/bin/git \"$@\"\n")).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        fixture.service = RepositoryService::new(&wrapper, fixture.executor.clone());
        let failure = fixture
            .apply(&fixture.prepare(&fixture.request(new)))
            .unwrap_err();
        assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
        assert!(!fixture.root.join("new-path").exists());
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
    }
}

#[test]
fn unsupported_native_transaction_refuses_before_any_checkout_effect() {
    let mut fixture = Fixture::new(true);
    let new = fixture.advance(|root| fs::write(root.join("file"), "new\n").unwrap());
    let wrapper = fixture.temporary.path().join("unsupported-git-wrapper");
    fs::write(&wrapper, "#!/bin/sh\nif [ \"${CAGENTS_PR_MODE:-}\" = probe ]; then printf '%s\\n' 'fixture: prepared reference transactions unavailable' >&2; exit 128; fi\nexec /usr/bin/git \"$@\"\n").unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.service = RepositoryService::new(&wrapper, fixture.executor.clone());
    let failure = fixture
        .apply(&fixture.prepare(&fixture.request(new)))
        .unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    assert!(failure.detail.contains("unavailable"));
    assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"old\n");
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
}

#[test]
fn detached_policy_descendant_keeps_operation_running_and_protected_through_cancellation() {
    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| fs::write(root.join("file"), "new\n").unwrap());
    let policy = fixture.root.join(".git/descendant-policy");
    fs::create_dir(&policy).unwrap();
    run_git(
        &fixture.root,
        &["config", "core.hooksPath", policy.to_str().unwrap()],
    );
    let hook = policy.join("reference-transaction");
    fs::write(&hook, "#!/bin/sh\nset -eu\ninput=$(/bin/cat)\nif [ \"$1\" = committed ] && printf '%s' \"$input\" | /usr/bin/grep -F ' refs/heads/feature' >/dev/null; then\n /usr/bin/setsid /usr/bin/timeout 5 /bin/sh -c 'touch \"$CAGENTS_PR_PRIVATE/descendant-started\"; while [ ! -e \"$CAGENTS_PR_PRIVATE/release-descendant\" ]; do /bin/sleep 0.02; done; touch \"$CAGENTS_PR_PRIVATE/descendant-retired\"' </dev/null >/dev/null 2>&1 &\nfi\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let plan = fixture.prepare(&fixture.request(new.clone()));
    let cancel = CancellationToken::default();
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let service = &fixture.service;
        let plan = &plan;
        let worker_cancel = &cancel;
        let worker = scope.spawn(move || {
            sender
                .send(service.reconcile_existing_pr_checkout(plan, worker_cancel))
                .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let operation = loop {
            if let Some(path) = fs::read_dir(fixture.root.join(".git"))
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("cagents-pr-checkout-")
                })
                .map(|entry| entry.path())
                .find(|path| path.join("descendant-started").exists())
            {
                break path;
            }
            assert!(Instant::now() < deadline, "owned fixture policy must start");
            thread::sleep(Duration::from_millis(5));
        };
        cancel.cancel();
        let lease = fs::File::open(operation.join("lease")).unwrap();
        // SAFETY: owned independent file remains live for this probe.
        assert_ne!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(
            receiver.recv_timeout(Duration::from_millis(100)).is_err(),
            "cancellation is a request, not process settlement"
        );
        fs::write(operation.join("release-descendant"), b"release").unwrap();
        let failure = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert_eq!(failure.kind, PrCheckoutFailureKind::PartialOrUnknown);
        assert!(operation.join("descendant-retired").exists());
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        worker.join().unwrap();
    });
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        new.as_str()
    );
}

#[test]
fn policy_child_finishing_before_first_lease_probe_cannot_authorize_a_stale_receipt() {
    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| fs::write(root.join("file"), "new\n").unwrap());
    let policy = fixture.root.join(".git/late-policy");
    fs::create_dir(&policy).unwrap();
    run_git(
        &fixture.root,
        &["config", "core.hooksPath", policy.to_str().unwrap()],
    );
    let hook = policy.join("reference-transaction");
    fs::write(&hook, r#"#!/bin/sh
set -eu
input=$(/bin/cat)
if [ "$1" = committed ] && printf '%s' "$input" | /usr/bin/grep -F ' refs/heads/feature' >/dev/null; then
 /usr/bin/setsid /usr/bin/timeout 5 /bin/sh -c '
  touch "$CAGENTS_PR_PRIVATE/late-child-started"
  while [ ! -e "$CAGENTS_PR_PRIVATE/release-late-child" ]; do /bin/sleep 0.02; done
  printf "\n[branch \"feature\"]\n merge = refs/heads/changed-by-child\n" >> "$CAGENTS_PR_COMMON_FD/config"
  touch "$CAGENTS_PR_PRIVATE/late-child-retired"
 ' </dev/null >/dev/null 2>&1 &
fi
"#).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let plan = fixture.prepare(&fixture.request(new.clone()));
    let root = fixture.root.clone();
    let synchronized = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let synchronized_hook = synchronized.clone();
    effect::RECONCILIATION_TEST_HOOK.with_borrow_mut(|hook| {
        *hook = Some(Box::new(move |phase| {
            if phase != "before_lease_observation" {
                return;
            }
            let operation = fs::read_dir(root.join(".git"))
                .unwrap()
                .map(Result::unwrap)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("cagents-pr-checkout-")
                })
                .unwrap()
                .path();
            fs::write(operation.join("release-late-child"), b"release").unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let observer = fs::File::open(operation.join("lease")).unwrap();
            loop {
                // SAFETY: independent descriptor pins the original lease inode.
                if unsafe { libc::flock(observer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "owned child must retire before first real observation"
                );
                thread::sleep(Duration::from_millis(5));
            }
            assert!(operation.join("late-child-retired").exists());
            synchronized_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        }))
    });
    let result = fixture.apply(&plan);
    effect::RECONCILIATION_TEST_HOOK.with_borrow_mut(|hook| *hook = None);
    let failure = result.unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::PartialOrUnknown);
    assert!(synchronized.load(std::sync::atomic::Ordering::SeqCst));
    assert!(failure.detail.contains("after its last policy hook"));
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        new.as_str()
    );
    assert!(
        String::from_utf8(fs::read(fixture.root.join(".git/config")).unwrap())
            .unwrap()
            .contains("changed-by-child")
    );
}

#[test]
fn packed_nested_branches_repair_absent_tracking_and_can_fast_forward() {
    for advance in [false, true] {
        let fixture = Fixture::new(false);
        run_git(&fixture.root, &["branch", "-m", "nested/feature"]);
        run_git(&fixture.root, &["push", "origin", "nested/feature"]);
        run_git(
            &fixture.bare,
            &["symbolic-ref", "HEAD", "refs/heads/nested/feature"],
        );
        let expected = if advance {
            fixture.advance(|root| fs::write(root.join("file"), "nested new\n").unwrap())
        } else {
            fixture.original.clone()
        };
        run_git(&fixture.root, &["pack-refs", "--all", "--prune"]);
        assert!(!fixture.root.join(".git/refs/heads/nested").exists());
        let mut request = fixture.request(expected.clone());
        request.branch = ValidatedBranchName::parse("nested/feature").unwrap();
        request.head_branch = request.branch.clone();
        let verified = fixture.apply(&fixture.prepare(&request)).unwrap();
        assert_eq!(verified.head(), &expected);
        assert_eq!(
            verified.upstream(),
            Some("refs/remotes/origin/nested/feature")
        );
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            expected.as_str()
        );
    }
}

#[test]
fn missing_ref_namespace_never_follows_a_foreign_symlink_parent() {
    let fixture = Fixture::new(false);
    let external = fixture.temporary.path().join("external-ref-storage");
    fs::create_dir(&external).unwrap();
    std::os::unix::fs::symlink(&external, fixture.root.join(".git/refs/heads/foreign")).unwrap();
    let root = fs_safety::Root::open(&fixture.root.join(".git")).unwrap();
    assert!(effect::lock_at(&root, Path::new("refs/heads/foreign/nested/feature")).is_err());
    assert_eq!(fs::read_dir(&external).unwrap().count(), 0);
}

#[test]
fn explicit_preserve_local_keeps_ahead_commits_and_all_dirty_bytes_without_fetch_or_repair() {
    let fixture = Fixture::new(true);
    fs::write(fixture.root.join("file"), "local commit\n").unwrap();
    run_git(&fixture.root, &["commit", "-qam", "local work"]);
    let local = git_text(&fixture.root, &["rev-parse", "HEAD"]);
    fs::write(fixture.root.join("file"), "staged local\n").unwrap();
    run_git(&fixture.root, &["add", "file"]);
    fs::write(fixture.root.join("other"), "unstaged local\n").unwrap();
    fs::write(fixture.root.join("untracked"), "untracked local\n").unwrap();
    let index = fs::read(fixture.root.join(".git/index")).unwrap();
    let config = fs::read(fixture.root.join(".git/config")).unwrap();
    let request = fixture.request(fixture.original.clone());
    assert!(
        fixture
            .service
            .prepare_existing_pr_checkout(&request, &CancellationToken::default())
            .is_err()
    );
    let plan = fixture
        .service
        .prepare_existing_pr_checkout_preserving_local(&request, &CancellationToken::default())
        .unwrap();
    assert_eq!(
        plan.disposition(),
        PrCheckoutDisposition::OpenPreservingLocal
    );
    assert_eq!(plan.local_head().as_str(), local);
    assert_eq!(plan.request().expected_head, fixture.original);
    assert!(plan.notice().contains(&local));
    assert!(plan.notice().contains(fixture.original.as_str()));
    fixture.intercept(|command| {
        assert!(
            !command
                .arguments
                .iter()
                .any(|arg| arg == "fetch" || arg == "--add" || arg == "read-tree")
        );
        if command.arguments.iter().any(|arg| arg == "update-ref") {
            assert_eq!(
                command.environment.get(OsStr::new("CAGENTS_PR_MODE")),
                Some(&OsString::from("probe"))
            );
        }
    });
    let verified = fixture.apply(&plan).unwrap();
    assert_eq!(
        verified.disposition(),
        PrCheckoutDisposition::OpenPreservingLocal
    );
    assert_eq!(verified.head(), plan.local_head());
    assert_eq!(verified.upstream(), Some("refs/remotes/origin/feature"));
    assert_eq!(git_text(&fixture.root, &["rev-parse", "HEAD"]), local);
    assert_eq!(fs::read(fixture.root.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(fixture.root.join(".git/config")).unwrap(), config);
    assert_eq!(
        fs::read(fixture.root.join("file")).unwrap(),
        b"staged local\n"
    );
    assert_eq!(
        fs::read(fixture.root.join("other")).unwrap(),
        b"unstaged local\n"
    );
    assert_eq!(
        fs::read(fixture.root.join("untracked")).unwrap(),
        b"untracked local\n"
    );
}

#[test]
fn explicit_preserve_local_admits_diverged_commits_while_strict_writer_still_refuses() {
    let fixture = Fixture::new(true);
    fs::write(fixture.root.join("file"), "local divergent commit\n").unwrap();
    run_git(&fixture.root, &["commit", "-qam", "local divergence"]);
    let local = git_text(&fixture.root, &["rev-parse", "HEAD"]);
    let reviewed =
        fixture.advance(|root| fs::write(root.join("file"), "PR divergent commit\n").unwrap());
    run_git(&fixture.root, &["fetch", "origin"]);
    let request = fixture.request(reviewed.clone());
    assert!(
        fixture
            .service
            .prepare_existing_pr_checkout(&request, &CancellationToken::default())
            .unwrap_err()
            .detail
            .contains("diverged")
    );
    let plan = fixture
        .service
        .prepare_existing_pr_checkout_preserving_local(&request, &CancellationToken::default())
        .unwrap();
    let verified = fixture.apply(&plan).unwrap();
    assert_eq!(verified.head().as_str(), local);
    assert_ne!(verified.head(), &reviewed);
    assert_eq!(
        verified.disposition(),
        PrCheckoutDisposition::OpenPreservingLocal
    );
    assert_eq!(
        fs::read(fixture.root.join("file")).unwrap(),
        b"local divergent commit\n"
    );
}

#[test]
fn preserve_local_never_repairs_missing_tracking_or_accepts_changed_confirmation() {
    let missing = Fixture::new(false);
    let request = missing.request(missing.original.clone());
    let config = fs::read(missing.root.join(".git/config")).unwrap();
    assert!(
        missing
            .service
            .prepare_existing_pr_checkout_preserving_local(&request, &CancellationToken::default())
            .unwrap_err()
            .detail
            .contains("no PR upstream")
    );
    assert_eq!(fs::read(missing.root.join(".git/config")).unwrap(), config);
    let fixture = Fixture::new(true);
    let plan = fixture
        .service
        .prepare_existing_pr_checkout_preserving_local(
            &fixture.request(fixture.original.clone()),
            &CancellationToken::default(),
        )
        .unwrap();
    run_git(
        &fixture.root,
        &["config", "branch.feature.merge", "refs/heads/different"],
    );
    assert_eq!(
        fixture.apply(&plan).unwrap_err().kind,
        PrCheckoutFailureKind::Refused
    );
    assert_eq!(
        git_text(&fixture.root, &["config", "branch.feature.merge"]),
        "refs/heads/different"
    );
}

#[derive(Clone)]
struct PrivateFetchLogSink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for PrivateFetchLogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn private_fetch_failure_never_logs_or_displays_credentials_in_argv_or_stderr() {
    const CHILD: &str = "CAGENTS_PRIVATE_FETCH_LOG_TEST";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap()).args(["--exact", "repository::pr_checkout::tests::private_fetch_failure_never_logs_or_displays_credentials_in_argv_or_stderr", "--nocapture"]).env(CHILD, "1").output().unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for timeout in [false, true] {
        let fixture = Fixture::new(false);
        let token = "synthetic-private-transport-token";
        run_git(
            &fixture.root,
            &[
                "config",
                "remote.origin.url",
                &format!("https://{token}@fixture.invalid/owner/repo.git"),
            ],
        );
        let request = fixture.request(fixture.original.clone());
        let plan = fixture.prepare(&request);
        let stderr =
            format!("fatal: transport failed for https://{token}@fixture.invalid/owner/repo.git\n")
                .into_bytes();
        *fixture.executor.fetch_failure.lock().unwrap() = Some(if timeout {
            Err(ProcessFailure {
                kind: ProcessFailureKind::TimedOut,
                program: "git".into(),
                exit_code: None,
                signal: Some(15),
                stdout: vec![],
                stderr: stderr.clone(),
                message: "synthetic private timeout".into(),
                started: true,
            })
        } else {
            Ok(ProcessOutput {
                exit_code: Some(128),
                signal: None,
                success: false,
                stdout: vec![],
                stderr: stderr.clone(),
            })
        });
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed_hook = observed.clone();
        let operation_root = fixture.root.clone();
        fixture.intercept(move |command| {
            if command.arguments.iter().any(|argument| argument == "fetch") {
                let operation = fs::read_dir(operation_root.join(".git"))
                    .unwrap()
                    .map(Result::unwrap)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("cagents-pr-checkout-")
                    })
                    .unwrap()
                    .path();
                assert_eq!(
                    fs::metadata(&operation).unwrap().mode() & 0o777,
                    0o700,
                    "operation must be private before transport or diagnostic capture"
                );
                assert!(
                    !command
                        .arguments
                        .iter()
                        .any(|argument| argument.to_string_lossy().contains(token))
                );
                assert!(
                    command.arguments.iter().any(|argument| argument
                        .to_string_lossy()
                        .starts_with("cagents-pr-fetch-"))
                );
                assert!(command.environment.iter().any(|(key, value)| {
                    key.to_string_lossy().starts_with("GIT_CONFIG_VALUE_")
                        && value.to_string_lossy().contains(token)
                }));
                observed_hook.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || PrivateFetchLogSink(writer.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let failure = tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("private_fetch_capture_ready");
            fixture.apply(&plan).unwrap_err()
        });
        let logs = String::from_utf8(sink.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("private_fetch_capture_ready"));
        assert!(
            !logs.contains(token),
            "transport secrets must not reach tracing"
        );
        assert!(!failure.detail.contains(token));
        assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
        let private = fs::read_dir(fixture.root.join(".git"))
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("cagents-pr-checkout-")
            })
            .unwrap()
            .path();
        assert_eq!(fs::metadata(&private).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::read(private.join("fetch.stderr")).unwrap(), stderr);
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
    }
}

#[test]
fn captured_raw_fetch_endpoint_applies_git_url_rewrites_exactly_once() {
    let fixture = Fixture::new(false);
    let missing = fixture.temporary.path().join("must-not-be-used");
    run_git(
        &fixture.root,
        &[
            "config",
            &format!("url.file://{}/.insteadOf", missing.display()),
            &format!("file://{}/", fixture.bare.display()),
        ],
    );
    let verified = fixture
        .apply(&fixture.prepare(&fixture.request(fixture.original.clone())))
        .unwrap();
    assert_eq!(verified.head(), &fixture.original);
    assert_eq!(verified.upstream(), Some("refs/remotes/origin/feature"));
    assert!(!missing.exists());
}

#[test]
fn explicit_tracking_repair_preserves_local_ahead_dirty_work_and_all_refs() {
    let fixture = Fixture::new(false);
    fs::write(fixture.root.join("file"), "local commit\n").unwrap();
    run_git(&fixture.root, &["commit", "-qam", "local work"]);
    let local = git_text(&fixture.root, &["rev-parse", "HEAD"]);
    fs::write(fixture.root.join("file"), "staged local\n").unwrap();
    run_git(&fixture.root, &["add", "file"]);
    fs::write(fixture.root.join("other"), "unstaged local\n").unwrap();
    fs::write(fixture.root.join("untracked"), "untracked local\n").unwrap();
    run_git(&fixture.root, &["config", "fixture.unrelated", "preserved"]);
    let index = fs::read(fixture.root.join(".git/index")).unwrap();
    let refs = run_git(&fixture.root, &["show-ref"]).stdout;
    let original_config = fs::read(fixture.root.join(".git/config")).unwrap();
    let request = fixture.request(fixture.original.clone());
    assert!(
        fixture
            .service
            .prepare_existing_pr_checkout(&request, &CancellationToken::default())
            .is_err()
    );
    assert!(
        fixture
            .service
            .prepare_existing_pr_checkout_preserving_local(&request, &CancellationToken::default())
            .is_err()
    );
    let plan = fixture
        .service
        .prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(
            &request,
            &CancellationToken::default(),
        )
        .unwrap();
    assert_eq!(
        plan.disposition(),
        PrCheckoutDisposition::RepairTrackingPreservingLocal
    );
    assert_eq!(plan.local_head().as_str(), local);
    assert_eq!(
        fs::read(fixture.root.join(".git/config")).unwrap(),
        original_config,
        "preparing the explicit choice must not configure tracking"
    );
    fixture.intercept(|command| {
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument == "fetch" || argument == "read-tree")
        );
        if command
            .arguments
            .iter()
            .any(|argument| argument == "update-ref")
        {
            assert_eq!(
                command.environment.get(OsStr::new("CAGENTS_PR_MODE")),
                Some(&OsString::from("probe"))
            );
            assert_eq!(
                command
                    .environment
                    .get(OsStr::new("CAGENTS_PR_UPDATE_REMOTE")),
                Some(&OsString::from("0"))
            );
        }
    });
    let verified = fixture.apply(&plan).unwrap();
    assert_eq!(
        verified.disposition(),
        PrCheckoutDisposition::RepairTrackingPreservingLocal
    );
    assert_eq!(verified.head().as_str(), local);
    assert_eq!(verified.upstream(), Some("refs/remotes/origin/feature"));
    assert_eq!(run_git(&fixture.root, &["show-ref"]).stdout, refs);
    assert_eq!(fs::read(fixture.root.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read(fixture.root.join("file")).unwrap(),
        b"staged local\n"
    );
    assert_eq!(
        fs::read(fixture.root.join("other")).unwrap(),
        b"unstaged local\n"
    );
    assert_eq!(
        fs::read(fixture.root.join("untracked")).unwrap(),
        b"untracked local\n"
    );
    assert_eq!(
        git_text(&fixture.root, &["config", "fixture.unrelated"]),
        "preserved"
    );
    assert_eq!(
        git_text(&fixture.root, &["config", "branch.feature.remote"]),
        "origin"
    );
    assert_eq!(
        git_text(&fixture.root, &["config", "branch.feature.merge"]),
        "refs/heads/feature"
    );
    run_git(&fixture.root, &["push"]);
    assert_eq!(
        git_text(&fixture.bare, &["rev-parse", "refs/heads/feature"]),
        local
    );
}

#[test]
fn tracking_repair_preserves_diverged_head_and_refuses_missing_or_stale_tracking_refs() {
    let fixture = Fixture::new(false);
    fs::write(fixture.root.join("file"), "local divergent\n").unwrap();
    run_git(&fixture.root, &["commit", "-qam", "local divergence"]);
    let local = git_text(&fixture.root, &["rev-parse", "HEAD"]);
    let reviewed = fixture.advance(|root| fs::write(root.join("file"), "PR divergent\n").unwrap());
    let request = fixture.request(reviewed.clone());
    let configuration = fs::read(fixture.root.join(".git/config")).unwrap();
    assert!(
        fixture
            .service
            .prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(
                &request,
                &CancellationToken::default()
            )
            .unwrap_err()
            .detail
            .contains("Fetch and review")
    );
    assert_eq!(
        fs::read(fixture.root.join(".git/config")).unwrap(),
        configuration
    );
    run_git(&fixture.root, &["fetch", "origin"]);
    let plan = fixture
        .service
        .prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(
            &request,
            &CancellationToken::default(),
        )
        .unwrap();
    let refs = run_git(&fixture.root, &["show-ref"]).stdout;
    let verified = fixture.apply(&plan).unwrap();
    assert_eq!(verified.head().as_str(), local);
    assert_eq!(run_git(&fixture.root, &["show-ref"]).stdout, refs);
    assert_eq!(
        fs::read(fixture.root.join("file")).unwrap(),
        b"local divergent\n"
    );

    let missing = Fixture::new(false);
    run_git(
        &missing.root,
        &["update-ref", "-d", "refs/remotes/origin/feature"],
    );
    let config = fs::read(missing.root.join(".git/config")).unwrap();
    assert!(
        missing
            .service
            .prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(
                &missing.request(missing.original.clone()),
                &CancellationToken::default()
            )
            .unwrap_err()
            .detail
            .contains("Fetch and review")
    );
    assert_eq!(fs::read(missing.root.join(".git/config")).unwrap(), config);
}

#[test]
fn tracking_only_preserve_requires_absent_pair_and_fresh_confirmation() {
    let fixture = Fixture::new(false);
    let request = fixture.request(fixture.original.clone());
    let plan = fixture
        .service
        .prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(
            &request,
            &CancellationToken::default(),
        )
        .unwrap();
    run_git(
        &fixture.root,
        &["config", "branch.feature.merge", "refs/heads/different"],
    );
    let config = fs::read(fixture.root.join(".git/config")).unwrap();
    assert!(
        fixture
            .service
            .prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(
                &request,
                &CancellationToken::default()
            )
            .is_err()
    );
    assert_eq!(
        fixture.apply(&plan).unwrap_err().kind,
        PrCheckoutFailureKind::Refused
    );
    assert_eq!(fs::read(fixture.root.join(".git/config")).unwrap(), config);
    let configured = Fixture::new(true);
    assert!(
        configured
            .service
            .prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(
                &configured.request(configured.original.clone()),
                &CancellationToken::default()
            )
            .unwrap_err()
            .detail
            .contains("wholly absent")
    );
}
