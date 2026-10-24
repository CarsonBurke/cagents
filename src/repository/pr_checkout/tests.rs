use super::*;
use std::{
    process::{Command, Output},
    sync::Mutex,
};

type Interceptor = Box<dyn FnMut(&CommandSpec) + Send>;

use crate::repository::test_fixture::isolated_fixture_case;

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
        let mut isolated = command.clone();
        for (name, _) in std::env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"GIT_") {
                isolated = isolated.env_remove(name);
            }
        }
        // The controlled map is applied after removals by CommandSpec. Keep
        // the synthetic fetch endpoint/count and original retained FD scopes.
        let command = isolated
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
    let mut command = Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    let output = command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "init.templateDir=",
            "-c",
            "commit.gpgsign=false",
        ])
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
        fs::create_dir_all(root.join(".git/hooks")).unwrap();
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

/// Wrap only the synthetic operation's hook, preserving its actual native
/// phase argument and stdin before invoking the production script unchanged.
fn observe_transaction_phases(fixture: &Fixture) {
    let mut wrapped = false;
    fixture.intercept(move |command| {
        if !wrapped
            && command
                .environment
                .contains_key(OsStr::new("CAGENTS_PR_MODE"))
        {
            wrapped = true;
            let private = Path::new(&command.environment[OsStr::new("CAGENTS_PR_PRIVATE")]);
            let hook = private.join("reference-transaction");
            fs::rename(&hook, private.join("observed-transaction")).unwrap();
            fs::write(
                &hook,
                r#"#!/bin/sh
set -eu
evidence="$CAGENTS_PR_WORKTREE/.git/observed-$CAGENTS_PR_MODE-$1"
/bin/cat > "$evidence"
printf '%s:%s\n' "$CAGENTS_PR_MODE" "$1" >> "$CAGENTS_PR_WORKTREE/.git/observed-phases"
exec /bin/sh "$CAGENTS_PR_PRIVATE/observed-transaction" "$@" < "$evidence"
"#,
            )
            .unwrap();
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        }
    });
}

#[test]
fn native_transaction_phases_preserve_current_and_fast_forward_checkouts() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(native_transaction_phases_preserve_current_and_fast_forward_checkouts)
    )) {
        return;
    }
    for advance in [false, true] {
        let fixture = Fixture::new(true);
        let expected = if advance {
            fixture.advance(|root| fs::write(root.join("file"), "new\n").unwrap())
        } else {
            fixture.original.clone()
        };
        if !advance {
            fs::write(fixture.root.join("other"), "local draft\n").unwrap();
        }
        let plan = fixture.prepare(&fixture.request(expected.clone()));
        observe_transaction_phases(&fixture);
        let verified = fixture.apply(&plan).unwrap();
        assert_eq!(verified.head(), &expected);
        let phases = fs::read_to_string(fixture.root.join(".git/observed-phases")).unwrap();
        eprintln!("actual native transaction phases (advance={advance}): {phases}");
        assert!(phases.lines().any(|phase| phase == "probe:prepared"));
        // Earlier Git versions emit no preparing notification. When native
        // Git emits it, retain proof that symbolic HEAD was still unresolved.
        if phases.lines().any(|phase| phase == "probe:preparing") {
            let preparing =
                fs::read_to_string(fixture.root.join(".git/observed-probe-preparing")).unwrap();
            let prepared =
                fs::read_to_string(fixture.root.join(".git/observed-probe-prepared")).unwrap();
            assert!(preparing.lines().any(|line| line.ends_with(" HEAD")));
            assert!(
                prepared
                    .lines()
                    .any(|line| line.ends_with(" refs/heads/feature"))
            );
            assert!(
                phases.find("probe:preparing").unwrap() < phases.find("probe:prepared").unwrap()
            );
        }
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            expected.as_str()
        );
        assert_eq!(
            fs::read(fixture.root.join("file")).unwrap(),
            if advance {
                b"new\n".as_slice()
            } else {
                b"old\n".as_slice()
            }
        );
        if !advance {
            assert_eq!(
                fs::read(fixture.root.join("other")).unwrap(),
                b"local draft\n"
            );
        }
    }
}

#[test]
fn preparing_notifications_preserve_policy_refusal_without_prepared_proof() {
    let temporary = tempfile::tempdir().unwrap();
    let hook = temporary.path().join("transaction");
    fs::write(&hook, include_bytes!("transaction.sh")).unwrap();
    let policy = temporary.path().join("policy");
    fs::write(&policy, "#!/bin/sh\n[ \"$1\" = preparing ] || exit 2\n/bin/cat > \"$CAGENTS_PR_PRIVATE/policy-input\"\nexit 7\n").unwrap();
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o700)).unwrap();
    let input = format!("{} {} HEAD\n", "0".repeat(40), "1".repeat(40));
    for (mode, success) in [("probe", true), ("effect", false)] {
        let mut child = Command::new("/bin/sh")
            .arg(&hook)
            .arg("preparing")
            .env("CAGENTS_PR_PRIVATE", temporary.path())
            .env("CAGENTS_PR_MODE", mode)
            .env("CAGENTS_PR_PREVIOUS_HOOK", &policy)
            .env("CAGENTS_PR_PREVIOUS_CONFIG", "")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.success(), success);
        assert!(!temporary.path().join("probe").exists());
        assert!(!temporary.path().join("effect-prepared").exists());
        assert_eq!(
            temporary.path().join("policy-input").exists(),
            mode == "effect"
        );
        assert_eq!(
            temporary.path().join("policy-failed").exists(),
            mode == "effect"
        );
        if mode == "effect" {
            assert_eq!(
                fs::read(temporary.path().join("policy-input")).unwrap(),
                input.as_bytes()
            );
            assert_eq!(
                fs::read(temporary.path().join("policy-failed")).unwrap(),
                b"preparing\n"
            );
        }
    }
    let unknown = Command::new("/bin/sh")
        .arg(&hook)
        .arg("unknown-phase")
        .output()
        .unwrap();
    assert!(
        !unknown.status.success(),
        "unknown phases must remain fail-closed"
    );
}

#[test]
fn native_prepared_config_guard_still_refuses_after_policy_edits() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(native_prepared_config_guard_still_refuses_after_policy_edits)
    )) {
        return;
    }
    let fixture = Fixture::new(true);
    let expected = fixture.advance(|root| fs::write(root.join("file"), "new\n").unwrap());
    let policy = fixture.root.join(".git/hooks/reference-transaction");
    fs::write(&policy, "#!/bin/sh\n/bin/cat >/dev/null\nif [ \"${CAGENTS_PR_MODE:-}\" = effect ] && [ \"$1\" = prepared ]; then printf '\\n[fixture]\\n concurrent = retained\\n' >> \"$CAGENTS_PR_WORKTREE/.git/config\"; fi\n").unwrap();
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o700)).unwrap();
    let plan = fixture.prepare(&fixture.request(expected));
    observe_transaction_phases(&fixture);
    let failure = fixture.apply(&plan).unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::Refused);
    let phases = fs::read_to_string(fixture.root.join(".git/observed-phases")).unwrap();
    assert!(phases.lines().any(|phase| phase == "effect:prepared"));
    assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"old\n");
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
    assert_eq!(
        git_text(&fixture.root, &["config", "fixture.concurrent"]),
        "retained"
    );
}

#[test]
fn missing_tracking_is_confirmed_atomically_and_bare_push_updates_pr() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(missing_tracking_is_confirmed_atomically_and_bare_push_updates_pr)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            advanced_pr_fast_forwards_existing_worktree_and_preserves_unrelated_ignored_tree
        )
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(repeated_current_checkout_is_verified_without_fetch_or_losing_local_edits)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(readonly_pr_branch_updates_exact_head_without_configuring_upstream)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(alias_branch_requires_explicit_upstream_push_mode_and_never_rewrites_policy)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(partial_conflicting_and_push_override_configuration_refuses_without_mutation)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(dirty_or_locally_ahead_checkout_refuses_and_preserves_every_edit)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(same_oid_branch_switch_at_effect_is_refused_under_native_ref_locks)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(concurrent_config_edit_and_dirty_write_at_effect_are_preserved)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(ignored_collision_created_after_census_is_preserved_at_checkout_effect)
    )) {
        return;
    }
    let mut fixture = Fixture::new(true);
    let new = fixture.advance(|root| {
        fs::write(root.join("secret"), "PR secret\n").unwrap();
        run_git(root, &["add", "--force", "secret"]);
    });
    let wrapper = fixture.temporary.path().join("late-collision-git");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
set -eu
if [ "${CAGENTS_PR_MODE:-}" = effect ]; then
    for argument do
        if [ "$argument" = read-tree ] || [ "$argument" = checkout ]; then
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
"#,
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.service = RepositoryService::new(&wrapper, fixture.executor.clone());
    let plan = fixture.prepare(&fixture.request(new));
    let before_index = fs::read(fixture.root.join(".git/index")).unwrap();
    let before_config = fs::read(fixture.root.join(".git/config")).unwrap();
    let result = fixture.apply(&plan);
    assert_eq!(
        fs::read(wrapper.with_extension("entered")).unwrap(),
        b"injected"
    );
    assert_eq!(
        fs::read(fixture.root.join("secret")).unwrap(),
        b"late ignored local data\n",
        "a clean census must not authorize overwriting data created before checkout"
    );
    assert!(
        result.is_err(),
        "the late collision must withhold verified success"
    );
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
    assert_eq!(
        fs::read(fixture.root.join(".git/index")).unwrap(),
        before_index
    );
    assert_eq!(
        fs::read(fixture.root.join(".git/config")).unwrap(),
        before_config
    );
}

#[test]
fn native_preserving_checkout_keeps_original_head_and_index_policy() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(native_preserving_checkout_keeps_original_head_and_index_policy)
    )) {
        return;
    }
    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| fs::write(root.join("file"), "native new\n").unwrap());
    let policy = fixture.root.join(".git/index policy");
    fs::create_dir(&policy).unwrap();
    run_git(
        &fixture.root,
        &["config", "core.hooksPath", policy.to_str().unwrap()],
    );
    let hook = policy.join("post-index-change");
    fs::write(&hook, "#!/bin/sh\nset -eu\n[ \"${CAGENTS_PR_MODE:-}\" = effect ] || exit 0\nprintf '%s:%s:%s\\n' \"$1\" \"$2\" \"$(git config core.hooksPath)\" >> \"$CAGENTS_PR_GITDIR_FD/index-policy-events\"\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let checkout_hook = policy.join("post-checkout");
    fs::write(
        &checkout_hook,
        "#!/bin/sh\nprintf unexpected > \"$CAGENTS_PR_GITDIR_FD/post-checkout-ran\"\n",
    )
    .unwrap();
    fs::set_permissions(&checkout_hook, fs::Permissions::from_mode(0o700)).unwrap();
    let original_orig_head = fs::read(fixture.root.join(".git/ORIG_HEAD")).ok();
    let old_reflog = git_text(&fixture.root, &["reflog", "show", "--format=%H", "HEAD"]);
    let verified = fixture
        .apply(&fixture.prepare(&fixture.request(new.clone())))
        .unwrap();
    assert_eq!(verified.head(), &new);
    assert_eq!(
        git_text(&fixture.root, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature"
    );
    assert_eq!(
        git_text(&fixture.root, &["write-tree"]),
        git_text(&fixture.root, &["rev-parse", &format!("{new}^{{tree}}")])
    );
    assert_eq!(
        fs::read(fixture.root.join(".git/ORIG_HEAD")).ok(),
        original_orig_head
    );
    assert!(!fixture.root.join(".git/SQUASH_MSG").exists());
    assert!(!fixture.root.join(".git/post-checkout-ran").exists());
    let events = fs::read_to_string(fixture.root.join(".git/index-policy-events")).unwrap();
    assert_eq!(
        events.lines().count(),
        1,
        "one native index publication: {events}"
    );
    assert!(
        events.trim_end().ends_with(policy.to_str().unwrap()),
        "original hooksPath remains active: {events}"
    );
    let reflog = git_text(&fixture.root, &["reflog", "show", "--format=%H", "HEAD"]);
    assert_eq!(reflog.lines().count(), old_reflog.lines().count() + 1);
    assert_eq!(reflog.lines().next(), Some(new.as_str()));
}

#[test]
fn preserving_checkout_can_advance_to_a_new_commit_with_the_same_tree() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(preserving_checkout_can_advance_to_a_new_commit_with_the_same_tree)
    )) {
        return;
    }
    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| {
        fs::write(root.join("file"), "intermediate\n").unwrap();
        run_git(root, &["commit", "-qam", "intermediate"]);
        fs::write(root.join("file"), "old\n").unwrap();
    });
    assert_ne!(new, fixture.original);
    assert_eq!(
        git_text(&fixture.bare, &["rev-parse", &format!("{new}^{{tree}}")]),
        git_text(
            &fixture.root,
            &["rev-parse", &format!("{}^{{tree}}", fixture.original)]
        )
    );
    let verified = fixture
        .apply(&fixture.prepare(&fixture.request(new.clone())))
        .unwrap();
    assert_eq!(verified.head(), &new);
    assert_eq!(
        git_text(&fixture.root, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature"
    );
    assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"old\n");
}

#[test]
fn unexpected_head_lock_probe_success_is_uncertain_and_never_runs_checkout() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(unexpected_head_lock_probe_success_is_uncertain_and_never_runs_checkout)
    )) {
        return;
    }
    let mut fixture = Fixture::new(true);
    let new = fixture.advance(|root| fs::write(root.join("file"), "native new\n").unwrap());
    let wrapper = fixture.temporary.path().join("unexpected-probe-git");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
set -eu
if [ "${CAGENTS_PR_MODE:-}" = effect ]; then
    for argument do
        if [ "$argument" = --no-deref ]; then
            printf probe > "$0.entered"
            exit 0
        fi
        if [ "$argument" = checkout ]; then
            printf unexpected > "$0.checkout"
        fi
    done
fi
exec /usr/bin/git "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.service = RepositoryService::new(&wrapper, fixture.executor.clone());
    let failure = fixture
        .apply(&fixture.prepare(&fixture.request(new)))
        .unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::PartialOrUnknown);
    assert_eq!(
        fs::read(wrapper.with_extension("entered")).unwrap(),
        b"probe"
    );
    assert!(!wrapper.with_extension("checkout").exists());
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
    assert_eq!(
        git_text(&fixture.root, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature"
    );
    assert_eq!(fs::read(fixture.root.join("file")).unwrap(), b"old\n");
}

#[test]
fn a_different_checkout_failure_cannot_publish_the_parent_reference() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(a_different_checkout_failure_cannot_publish_the_parent_reference)
    )) {
        return;
    }
    let mut fixture = Fixture::new(true);
    let new = fixture.advance(|root| fs::write(root.join("file"), "native new\n").unwrap());
    let wrapper = fixture.temporary.path().join("failed-after-checkout-git");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
set -eu
if [ "${CAGENTS_PR_MODE:-}" = effect ]; then
    for argument do
        if [ "$argument" = checkout ]; then
            code=0
            /usr/bin/git "$@" || code=$?
            printf 'different post-checkout command failure\n' >&2
            printf finished > "$0.finished"
            exit "$code"
        fi
    done
fi
exec /usr/bin/git "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.service = RepositoryService::new(&wrapper, fixture.executor.clone());
    let failure = fixture
        .apply(&fixture.prepare(&fixture.request(new.clone())))
        .unwrap_err();
    assert_eq!(failure.kind, PrCheckoutFailureKind::PartialOrUnknown);
    assert_eq!(
        fs::read(wrapper.with_extension("finished")).unwrap(),
        b"finished"
    );
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
    assert_eq!(
        git_text(&fixture.root, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature"
    );
    assert_eq!(
        git_text(&fixture.root, &["write-tree"]),
        git_text(&fixture.root, &["rev-parse", &format!("{new}^{{tree}}")])
    );
    assert_eq!(
        fs::read(fixture.root.join("file")).unwrap(),
        b"native new\n",
        "retain known partial checkout; never reset/replay"
    );
}

#[test]
fn cancellation_after_native_index_publication_retains_partial_state() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(cancellation_after_native_index_publication_retains_partial_state)
    )) {
        return;
    }
    let fixture = Fixture::new(true);
    let new = fixture.advance(|root| fs::write(root.join("file"), "native new\n").unwrap());
    let hooks = fixture.root.join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-index-change");
    fs::write(&hook, r#"#!/bin/sh
set -eu
[ "${CAGENTS_PR_MODE:-}" = effect ] || exit 0
printf paused > "$CAGENTS_PR_GITDIR_FD/checkout-paused"
/usr/bin/timeout 5 /bin/sh -c 'while [ ! -e "$CAGENTS_PR_GITDIR_FD/release-checkout" ]; do /bin/sleep 0.02; done'
"#).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let plan = fixture.prepare(&fixture.request(new.clone()));
    let cancel = CancellationToken::default();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            fixture
                .service
                .reconcile_existing_pr_checkout(&plan, &cancel)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fixture.root.join(".git/checkout-paused").exists() {
            assert!(
                Instant::now() < deadline,
                "native index hook must be reached"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
        cancel.cancel();
        fs::write(fixture.root.join(".git/release-checkout"), b"release").unwrap();
        let failure = worker.join().unwrap().unwrap_err();
        assert_eq!(failure.kind, PrCheckoutFailureKind::PartialOrUnknown);
    });
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "HEAD"]),
        fixture.original.as_str()
    );
    assert_eq!(
        git_text(&fixture.root, &["symbolic-ref", "HEAD"]),
        "refs/heads/feature"
    );
    assert_eq!(
        fs::read(fixture.root.join("file")).unwrap(),
        b"native new\n"
    );
}

#[test]
fn ignored_collision_refuses_and_preserves_original_secret() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(ignored_collision_refuses_and_preserves_original_secret)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(ignored_symlink_ancestor_refuses_without_touching_its_external_target)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(plan_debug_never_formats_configuration_bytes_or_credential_urls)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(replacing_worktree_path_at_effect_never_updates_replacement_or_wrong_inode)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(direct_config_edit_during_staging_is_preserved_and_pair_is_not_published)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            concurrent_private_config_snapshot_never_widens_the_named_lock_or_exposes_new_bytes
        )
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(foreign_index_lock_is_preserved_without_publishing_tracking)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            original_policy_is_chained_with_its_hook_configuration_and_aborted_effects_are_honest
        )
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            native_addition_guard_preserves_non_utf8_ignored_data_and_refuses_file_to_directory
        )
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(new_paths_with_newlines_and_globs_are_checked_and_installed_literally)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(truncated_or_over_limit_addition_census_refuses_before_checkout)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(unsupported_native_transaction_refuses_before_any_checkout_effect)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            detached_policy_descendant_keeps_operation_running_and_protected_through_cancellation
        )
    )) {
        return;
    }
    detached_policy_descendant_is_retained_through_settlement(true);
}

#[test]
fn retained_checkout_lease_refuses_success_without_claiming_a_known_holder() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(retained_checkout_lease_refuses_success_without_claiming_a_known_holder)
    )) {
        return;
    }
    detached_policy_descendant_is_retained_through_settlement(false);
}

fn detached_policy_descendant_is_retained_through_settlement(cancelled: bool) {
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
        let (observed_sender, observed_receiver) = std::sync::mpsc::sync_channel(1);
        let service = &fixture.service;
        let plan = &plan;
        let worker_cancel = &cancel;
        let worker = scope.spawn(move || {
            effect::RECONCILIATION_TEST_HOOK.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move |phase| {
                    if phase == "retained_lease_observed" {
                        observed_sender.send(()).unwrap();
                    }
                }));
            });
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
        if cancelled {
            cancel.cancel();
        }
        let lease = fs::File::open(operation.join("lease")).unwrap();
        // SAFETY: owned independent file remains live for this probe.
        assert_ne!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(
            receiver.recv_timeout(Duration::from_millis(100)).is_err(),
            "an inherited lease must prevent success until its holder exits"
        );
        observed_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the original lease must be observed before releasing its fixture holder");
        fs::write(operation.join("release-descendant"), b"release").unwrap();
        let failure = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert_eq!(failure.kind, PrCheckoutFailureKind::PartialOrUnknown);
        if !cancelled {
            assert!(
                failure
                    .detail
                    .contains("Its holder could not be identified")
            );
            assert!(failure.detail.contains("unrelated child before exec"));
            assert!(
                !failure
                    .detail
                    .contains("A policy or Git descendant outlived")
            );
        }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            policy_child_finishing_before_first_lease_probe_cannot_authorize_a_stale_receipt
        )
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(packed_nested_branches_repair_absent_tracking_and_can_fast_forward)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(missing_ref_namespace_never_follows_a_foreign_symlink_parent)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            explicit_preserve_local_keeps_ahead_commits_and_all_dirty_bytes_without_fetch_or_repair
        )
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            explicit_preserve_local_admits_diverged_commits_while_strict_writer_still_refuses
        )
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(preserve_local_never_repairs_missing_tracking_or_accepts_changed_confirmation)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(private_fetch_failure_never_logs_or_displays_credentials_in_argv_or_stderr)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(captured_raw_fetch_endpoint_applies_git_url_rewrites_exactly_once)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(explicit_tracking_repair_preserves_local_ahead_dirty_work_and_all_refs)
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(
            tracking_repair_preserves_diverged_head_and_refuses_missing_or_stale_tracking_refs
        )
    )) {
        return;
    }
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
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(tracking_only_preserve_requires_absent_pair_and_fresh_confirmation)
    )) {
        return;
    }
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

#[cfg(feature = "ui")]
fn creation_request(
    fixture: &Fixture,
    track: bool,
) -> (CreateWorktreeRequest, PrWorktreeCreationConsent, StateStore) {
    let root = fixture.temporary.path().join("managed");
    if track {
        run_git(&fixture.root, &["config", "push.default", "upstream"]);
    }
    let consent = PrWorktreeCreationConsent::new(
        GitHubRepoKey::new("fixture.invalid", None, "owner", "repo").unwrap(),
        ValidatedBranchName::parse("feature").unwrap(),
        fixture.original.clone(),
        "origin".into(),
        track,
    )
    .unwrap();
    let request = CreateWorktreeRequest {
        repository: fixture.root.clone(),
        managed_root: root.clone(),
        managed_ignore_root: None,
        destination: root.join("reviewed"),
        branch: ValidatedBranchName::parse("reviewed").unwrap(),
        base: if track {
            BaseRevision::remote_ref("refs/remotes/origin/feature").unwrap()
        } else {
            BaseRevision::object_id(fixture.original.as_str()).unwrap()
        },
        track,
        operation_id: OperationId::new(),
        setup_intent: Some(WorktreeSetupIntent::default()),
    };
    let store = StateStore::open(
        fixture.temporary.path().join("creation.sqlite3"),
        crate::model::BootId::new("pr-creation-fixture"),
    )
    .unwrap();
    (request, consent, store)
}

#[cfg(feature = "ui")]
fn create_reviewed(
    fixture: &Fixture,
    request: &CreateWorktreeRequest,
    consent: &PrWorktreeCreationConsent,
    store: &StateStore,
) -> WorktreeOperationReport {
    let (progress, _receiver) = async_channel::bounded(8);
    fixture.service.create_worktree_journaled_for_pr(
        store,
        request,
        consent,
        7,
        &CancellationToken::new(),
        &progress,
    )
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_keeps_exact_reviewed_commit_tracking_and_setup_on_restart() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_keeps_exact_reviewed_commit_tracking_and_setup_on_restart"
    )) {
        return;
    }
    for track in [true, false] {
        let fixture = Fixture::new(false);
        let (request, consent, store) = creation_request(&fixture, track);
        let report = create_reviewed(&fixture, &request, &consent, &store);
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert!(
            matches!(&report.outcome, Some(WorktreeCreationOutcome::Created {worktree, ..}) if worktree.head_oid.as_deref() == Some(fixture.original.as_str()))
        );
        assert_eq!(
            git_text(&request.destination, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
        let upstream = fixture
            .service
            .pr_values(
                &request.destination,
                "branch.reviewed.remote",
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            upstream,
            if track {
                vec!["origin".to_owned()]
            } else {
                vec![]
            }
        );
        let saved = store
            .journaled_worktree_operation(request.operation_id)
            .unwrap()
            .unwrap();
        let journal: WorktreeJournal = serde_json::from_value(saved.payload.clone()).unwrap();
        assert_eq!(journal.pr_checkout.as_ref(), Some(&consent));
        assert_eq!(journal.request.setup_intent, request.setup_intent);
        assert!(journal.valid_pr_checkout_evidence());
        let reloaded = fixture
            .service
            .rehydrate_worktree_operations(&store)
            .unwrap();
        assert_eq!(reloaded[0].state, WorktreeOperationState::Created);
        assert!(matches!(
            reloaded[0].outcome,
            Some(WorktreeCreationOutcome::Created { .. })
        ));
        assert_eq!(
            fixture
                .service
                .worktree_setup_creation(&store, request.operation_id)
                .unwrap()
                .unwrap()
                .state,
            WorktreeOperationState::Created
        );
        // Removing the optional consent keeps the old v1 JSON shape readable.
        let mut legacy = saved.payload;
        legacy.as_object_mut().unwrap().remove("pr_checkout");
        let legacy: WorktreeJournal = serde_json::from_value(legacy).unwrap();
        assert!(legacy.pr_checkout.is_none());
        assert!(
            !serde_json::to_value(legacy)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("pr_checkout")
        );
    }
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_refuses_remote_advance_before_any_worktree_effect() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_refuses_remote_advance_before_any_worktree_effect"
    )) {
        return;
    }
    for track in [true, false] {
        let fixture = Fixture::new(false);
        let (request, consent, store) = creation_request(&fixture, track);
        let advanced =
            fixture.advance(|producer| fs::write(producer.join("file"), "new PR head\n").unwrap());
        assert_ne!(advanced, fixture.original);
        let report = create_reviewed(&fixture, &request, &consent, &store);
        assert_eq!(
            report.state,
            WorktreeOperationState::NotCreated,
            "{report:?}"
        );
        assert!(matches!(
            report.outcome,
            Some(WorktreeCreationOutcome::NotCreated { .. })
        ));
        assert!(!request.managed_root.exists());
        assert!(
            fixture
                .service
                .resolve_optional_revision(
                    &fixture.root,
                    "refs/heads/reviewed",
                    &CancellationToken::new(),
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
        assert_eq!(
            fixture
                .service
                .rehydrate_worktree_operations(&store)
                .unwrap()[0]
                .state,
            WorktreeOperationState::NotCreated
        );
    }
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_refuses_unmounted_local_ahead_without_implicit_preserve() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_refuses_unmounted_local_ahead_without_implicit_preserve"
    )) {
        return;
    }
    let fixture = Fixture::new(false);
    let (request, consent, store) = creation_request(&fixture, true);
    run_git(&fixture.root, &["switch", "-c", "reviewed"]);
    fs::write(fixture.root.join("file"), "local work\n").unwrap();
    run_git(&fixture.root, &["commit", "-qam", "local ahead"]);
    let local = git_text(&fixture.root, &["rev-parse", "HEAD"]);
    run_git(&fixture.root, &["switch", "feature"]);
    fixture.intercept(|command| {
        assert!(
            !command.arguments.iter().any(|arg| arg == "fetch"),
            "local history refusal precedes fetch"
        )
    });
    let report = create_reviewed(&fixture, &request, &consent, &store);
    assert_eq!(
        report.state,
        WorktreeOperationState::NotCreated,
        "{report:?}"
    );
    assert!(
        report.detail.contains("existing local branch"),
        "{}",
        report.detail
    );
    assert!(!request.managed_root.exists());
    assert_eq!(
        git_text(&fixture.root, &["rev-parse", "refs/heads/reviewed"]),
        local
    );
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_retains_unknown_after_delayed_source_change() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_retains_unknown_after_delayed_source_change"
    )) {
        return;
    }
    for phase in ["fetch", "unlock"] {
        let fixture = Fixture::new(false);
        let (request, consent, store) = creation_request(&fixture, true);
        let root = fixture.root.clone();
        let mut swapped = false;
        fixture.intercept(move |command| {
            if !swapped && command.arguments.iter().any(|arg| arg == phase) {
                swapped = true;
                if phase == "fetch" {
                    assert!(
                        command
                            .environment
                            .values()
                            .any(|value| value == "https://fixture.invalid/owner/repo.git")
                    );
                }
                run_git(
                    &root,
                    &[
                        "remote",
                        "set-url",
                        "origin",
                        "https://fixture.invalid/other/repo.git",
                    ],
                );
            }
        });
        let report = create_reviewed(&fixture, &request, &consent, &store);
        assert_eq!(
            report.state,
            WorktreeOperationState::OutcomeUnknown,
            "{report:?}"
        );
        assert!(!matches!(
            report.outcome,
            Some(WorktreeCreationOutcome::Created { .. })
        ));
        assert_eq!(
            git_text(&request.destination, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
        assert_eq!(
            git_text(&fixture.root, &["config", "remote.origin.url"]),
            "https://fixture.invalid/other/repo.git"
        );
        let saved = store
            .journaled_worktree_operation(request.operation_id)
            .unwrap()
            .unwrap();
        let journal: WorktreeJournal = serde_json::from_value(saved.payload).unwrap();
        assert_eq!(journal.pr_checkout, Some(consent));
        assert_eq!(
            fixture
                .service
                .rehydrate_worktree_operations(&store)
                .unwrap()[0]
                .state,
            WorktreeOperationState::OutcomeUnknown
        );
    }
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_restart_rejects_changed_source_head_and_forged_consent() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_restart_rejects_changed_source_head_and_forged_consent"
    )) {
        return;
    }
    for change in ["source", "head", "payload"] {
        let fixture = Fixture::new(false);
        let (request, consent, store) = creation_request(&fixture, true);
        assert_eq!(
            create_reviewed(&fixture, &request, &consent, &store).state,
            WorktreeOperationState::Created
        );
        match change {
            "source" => {
                run_git(
                    &fixture.root,
                    &[
                        "remote",
                        "set-url",
                        "origin",
                        "https://fixture.invalid/other/repo.git",
                    ],
                );
            }
            "head" => {
                fs::write(
                    request.destination.join("file"),
                    "changed after completion\n",
                )
                .unwrap();
                run_git(&request.destination, &["commit", "-qam", "later work"]);
            }
            _ => {
                let saved = store
                    .journaled_worktree_operation(request.operation_id)
                    .unwrap()
                    .unwrap();
                let mut changed = saved.payload.clone();
                changed["pr_checkout"]["expected_head"] = serde_json::json!("f".repeat(40));
                store
                    .update_operation(
                        request.operation_id,
                        &saved.payload,
                        &changed,
                        PendingOperationStatus::Applied,
                    )
                    .unwrap();
            }
        }
        let before = git_text(&request.destination, &["rev-parse", "HEAD"]);
        let report = fixture
            .service
            .rehydrate_worktree_operations(&store)
            .unwrap()
            .remove(0);
        assert!(
            !matches!(
                report.outcome,
                Some(WorktreeCreationOutcome::Created { .. })
            ),
            "{change}: {report:?}"
        );
        assert_eq!(
            report.state,
            if change == "payload" {
                WorktreeOperationState::MalformedPayload
            } else {
                WorktreeOperationState::OutcomeUnknown
            }
        );
        assert!(
            fixture
                .service
                .worktree_setup_creation(&store, request.operation_id)
                .is_err()
        );
        assert_eq!(
            git_text(&request.destination, &["rev-parse", "HEAD"]),
            before
        );
    }
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_reuses_only_an_exact_matching_unmounted_read_only_branch() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_reuses_only_an_exact_matching_unmounted_read_only_branch"
    )) {
        return;
    }
    for resolve in [false, true] {
        let fixture = Fixture::new(false);
        let (mut request, consent, store) = creation_request(&fixture, false);
        run_git(
            &fixture.root,
            &["branch", "reviewed", fixture.original.as_str()],
        );
        if resolve {
            let input = WorktreeInput {
                repository: fixture.root.clone(),
                input: "reviewed".into(),
                base: fixture.original.as_str().into(),
                managed_directory: request.managed_root.to_str().unwrap().into(),
                destination: request.destination.to_str().unwrap().into(),
                operation_id: request.operation_id,
            };
            assert!(
                fixture
                    .service
                    .resolve_worktree_request(&input, &CancellationToken::new())
                    .is_err()
            );
            let resolved = fixture
                .service
                .resolve_worktree_request_for_pr(&input, &consent, &CancellationToken::new())
                .unwrap();
            assert_eq!(
                resolved.base,
                BaseRevision::LocalBranch(request.branch.clone())
            );
            assert!(!resolved.track);
            request.base = resolved.base;
        }
        let report = create_reviewed(&fixture, &request, &consent, &store);
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        assert_eq!(
            git_text(&request.destination, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
        assert!(
            fixture
                .service
                .pr_values(
                    &request.destination,
                    "branch.reviewed.remote",
                    &CancellationToken::new()
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fixture
                .service
                .rehydrate_worktree_operations(&store)
                .unwrap()[0]
                .state,
            WorktreeOperationState::Created
        );
    }
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_preserves_tracking_and_refuses_late_read_only_upstream() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_preserves_tracking_and_refuses_late_read_only_upstream"
    )) {
        return;
    }
    for phase in ["before", "fetch", "unlock", "worktree"] {
        let fixture = Fixture::new(false);
        let (request, consent, store) = creation_request(&fixture, false);
        run_git(
            &fixture.root,
            &["branch", "reviewed", fixture.original.as_str()],
        );
        if phase == "worktree" {
            run_git(
                &fixture.root,
                &["config", "extensions.worktreeConfig", "true"],
            );
        }
        let configure = move |root: &Path| {
            if phase == "worktree" {
                run_git(
                    root,
                    &["config", "--worktree", "branch.reviewed.remote", "origin"],
                );
                run_git(
                    root,
                    &[
                        "config",
                        "--worktree",
                        "branch.reviewed.merge",
                        "refs/heads/feature",
                    ],
                );
            } else {
                run_git(root, &["config", "branch.reviewed.remote", "origin"]);
                run_git(
                    root,
                    &["config", "branch.reviewed.merge", "refs/heads/feature"],
                );
            }
        };
        if phase == "before" {
            configure(&fixture.root);
        }
        let scope = if phase == "worktree" {
            request.destination.clone()
        } else {
            fixture.root.clone()
        };
        let root = scope.clone();
        let gate = if phase == "worktree" { "unlock" } else { phase };
        let mut changed = false;
        fixture.intercept(move |command| {
            if phase == "before" {
                assert!(!command.arguments.iter().any(|arg| arg == "fetch"));
            } else if !changed && command.arguments.iter().any(|arg| arg == gate) {
                changed = true;
                configure(&root);
            }
        });
        let report = create_reviewed(&fixture, &request, &consent, &store);
        assert_eq!(
            report.state,
            if phase == "before" {
                WorktreeOperationState::NotCreated
            } else {
                WorktreeOperationState::OutcomeUnknown
            },
            "{phase}: {report:?}"
        );
        assert!(!matches!(
            report.outcome,
            Some(WorktreeCreationOutcome::Created { .. })
        ));
        assert_eq!(
            git_text(&scope, &["config", "branch.reviewed.remote"]),
            "origin"
        );
        assert_eq!(
            git_text(&scope, &["config", "branch.reviewed.merge"]),
            "refs/heads/feature"
        );
        assert_eq!(
            git_text(&fixture.root, &["rev-parse", "refs/heads/reviewed"]),
            fixture.original.as_str()
        );
        if phase == "before" {
            assert!(!request.managed_root.exists());
        } else {
            assert_eq!(
                git_text(&request.destination, &["rev-parse", "HEAD"]),
                fixture.original.as_str()
            );
            assert!(!matches!(
                fixture
                    .service
                    .rehydrate_worktree_operations(&store)
                    .unwrap()[0]
                    .outcome,
                Some(WorktreeCreationOutcome::Created { .. })
            ));
        }
    }
    let fixture = Fixture::new(false);
    let (request, consent, store) = creation_request(&fixture, true);
    run_git(
        &fixture.root,
        &["branch", "reviewed", fixture.original.as_str()],
    );
    run_git(
        &fixture.root,
        &["config", "branch.reviewed.remote", "origin"],
    );
    run_git(
        &fixture.root,
        &["config", "branch.reviewed.merge", "refs/heads/other"],
    );
    let report = create_reviewed(&fixture, &request, &consent, &store);
    assert_eq!(
        report.state,
        WorktreeOperationState::NotCreated,
        "{report:?}"
    );
    assert_eq!(
        git_text(&fixture.root, &["config", "branch.reviewed.merge"]),
        "refs/heads/other"
    );
    assert!(!request.managed_root.exists());
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_preserves_writable_upstream_selected_after_fetch_or_by_hook() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_preserves_writable_upstream_selected_after_fetch_or_by_hook"
    )) {
        return;
    }
    for phase in ["fetch", "post-checkout"] {
        let fixture = Fixture::new(false);
        let (request, consent, store) = creation_request(&fixture, true);
        run_git(
            &fixture.root,
            &["branch", "reviewed", fixture.original.as_str()],
        );
        if phase == "post-checkout" {
            let config = fixture.root.join(".git/config");
            let quoted = format!("'{}'", config.to_str().unwrap().replace('\'', "'\\''"));
            let hook = fixture.root.join(".git/hooks/post-checkout");
            fs::write(&hook, format!("#!/bin/sh\ncat >> {quoted} <<'CAGENTS_UPSTREAM'\n[branch \"reviewed\"]\nremote = origin\nmerge = refs/heads/other\nCAGENTS_UPSTREAM\n")).unwrap();
            fs::set_permissions(hook, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root = fixture.root.clone();
        let mut changed = false;
        fixture.intercept(move |command| {
            assert!(
                !command
                    .arguments
                    .iter()
                    .any(|arg| arg.as_encoded_bytes().starts_with(b"--set-upstream-to=")),
                "a differing user upstream must never be rewritten"
            );
            if phase == "fetch" && !changed && command.arguments.iter().any(|arg| arg == "fetch") {
                changed = true;
                run_git(&root, &["config", "branch.reviewed.remote", "origin"]);
                run_git(
                    &root,
                    &["config", "branch.reviewed.merge", "refs/heads/other"],
                );
            }
        });
        let report = create_reviewed(&fixture, &request, &consent, &store);
        assert_eq!(
            report.state,
            WorktreeOperationState::OutcomeUnknown,
            "{phase}: {report:?}"
        );
        assert!(!matches!(
            report.outcome,
            Some(WorktreeCreationOutcome::Created { .. })
        ));
        assert_eq!(
            git_text(&request.destination, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
        assert_eq!(
            git_text(&request.destination, &["config", "branch.reviewed.merge"]),
            "refs/heads/other"
        );
        assert_eq!(
            git_text(&request.destination, &["config", "branch.reviewed.remote"]),
            "origin"
        );
        assert!(!matches!(
            fixture
                .service
                .rehydrate_worktree_operations(&store)
                .unwrap()[0]
                .outcome,
            Some(WorktreeCreationOutcome::Created { .. })
        ));
    }
}

#[test]
#[cfg(feature = "ui")]
fn new_pr_creation_refuses_redirected_or_broad_push_policy() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::new_pr_creation_refuses_redirected_or_broad_push_policy"
    )) {
        return;
    }
    for (key, value) in [
        ("branch.reviewed.pushremote", "other"),
        ("remote.pushdefault", "other"),
        ("remote.origin.push", "refs/heads/reviewed:refs/heads/main"),
        ("push.default", "matching"),
        ("push.default", "simple"),
    ] {
        let fixture = Fixture::new(false);
        let (request, consent, store) = creation_request(&fixture, true);
        run_git(&fixture.root, &["config", key, value]);
        let report = create_reviewed(&fixture, &request, &consent, &store);
        assert_eq!(
            report.state,
            WorktreeOperationState::NotCreated,
            "{key}: {report:?}"
        );
        assert!(!request.managed_root.exists());
        assert_eq!(git_text(&fixture.root, &["config", key]), value);
        assert!(
            fixture
                .service
                .resolve_optional_revision(
                    &fixture.root,
                    "refs/heads/reviewed",
                    &CancellationToken::new(),
                )
                .unwrap()
                .is_none()
        );
    }
    for phase in ["fetch", "unlock"] {
        let fixture = Fixture::new(false);
        let (request, consent, store) = creation_request(&fixture, true);
        let root = fixture.root.clone();
        let mut changed = false;
        fixture.intercept(move |command| {
            if !changed && command.arguments.iter().any(|arg| arg == phase) {
                changed = true;
                run_git(&root, &["config", "branch.reviewed.pushremote", "other"]);
            }
        });
        let report = create_reviewed(&fixture, &request, &consent, &store);
        assert_eq!(
            report.state,
            WorktreeOperationState::OutcomeUnknown,
            "{phase}: {report:?}"
        );
        assert!(!matches!(
            report.outcome,
            Some(WorktreeCreationOutcome::Created { .. })
        ));
        assert_eq!(
            git_text(
                &request.destination,
                &["config", "branch.reviewed.pushremote"]
            ),
            "other"
        );
        assert_eq!(
            git_text(&request.destination, &["rev-parse", "HEAD"]),
            fixture.original.as_str()
        );
        assert!(!matches!(
            fixture
                .service
                .rehydrate_worktree_operations(&store)
                .unwrap()[0]
                .outcome,
            Some(WorktreeCreationOutcome::Created { .. })
        ));
    }
}
