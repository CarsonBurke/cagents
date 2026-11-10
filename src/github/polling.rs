//! One host budget shared by review, inspector and review-request polling.
use super::*;
use std::cell::RefCell;

thread_local! {
    /// Shared workers already captured their credential context off the UI
    /// thread. Keep that context through all service-level pacing decisions.
    static SCOPE: RefCell<Option<String>> = const { RefCell::new(None) };
    static RESOURCE: RefCell<String> = RefCell::new("graphql".into());
}

pub(crate) fn with_scope<T>(scope: &str, read: impl FnOnce() -> T) -> T {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCOPE.with(|current| *current.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SCOPE.with(|current| current.replace(Some(scope.to_owned()))));
    read()
}

pub(crate) fn current_scope() -> Option<String> {
    SCOPE.with(|scope| scope.borrow().clone())
}

/// Foreground admission defaults to GraphQL, which owns the app's polling
/// queries. Transport workers select their actual resource without UI file IO.
pub(crate) fn with_resource<T>(resource: &str, read: impl FnOnce() -> T) -> T {
    struct Restore(String);
    impl Drop for Restore {
        fn drop(&mut self) {
            RESOURCE.with(|current| *current.borrow_mut() = std::mem::take(&mut self.0));
        }
    }
    let _restore = Restore(RESOURCE.with(|current| current.replace(resource.to_owned())));
    read()
}

pub(crate) fn resource() -> String {
    RESOURCE.with(|current| current.borrow().clone())
}

pub(crate) fn classify(text: &str, now: u64) -> Option<RateLimit> {
    RateLimit::classify_for(text, now, &resource())
}

static ACTIVE: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

/// Workers capture credentials before starting a read. UI admission follows
/// that context without reading credential files on the UI thread. Completion
/// of an older scoped request must never change this binding.
pub(crate) fn bind_scope(host: &str, scope: &str) {
    let host = host.to_ascii_lowercase();
    let first = ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(host.clone(), scope.into())
        .is_none();
    if first {
        let mut budgets = BUDGETS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(budget) = budgets.remove(&host) {
            budgets.insert(format!("{host}:{scope}"), budget);
        }
    }
}

fn budget_key(host: &str) -> String {
    SCOPE.with(|scope| match scope.borrow().as_deref() {
        Some(scope) => format!("{}:{scope}", host.to_ascii_lowercase()),
        None => {
            let host = host.to_ascii_lowercase();
            ACTIVE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&host)
                .map_or_else(|| host.clone(), |scope| format!("{host}:{scope}"))
        }
    })
}

pub use crate::github_rate_limit::{RateLimit, now};

#[derive(Default)]
struct Budget {
    failures: u32,
    due: u64,
    rate_limited: bool,
    // Resource-local pauses include ambiguous rate errors, not only confirmed
    // primary exhaustion. Only affirmative secondary evidence sets shared due.
    primary: HashMap<String, u64>,
}
static BUDGETS: LazyLock<Mutex<HashMap<String, Budget>>> = LazyLock::new(Default::default);
impl Budget {
    fn limit(&self, resource: &str, at: u64) -> Option<RateLimit> {
        let secondary = (self.rate_limited && self.due > at).then_some(self.due);
        if let Some(&due) = self.primary.get(resource).filter(|&&due| due > at) {
            Some(RateLimit {
                retry_at: due.max(secondary.unwrap_or(0)),
                resource: Some(resource.into()),
                secondary_retry_at: secondary,
            })
        } else {
            secondary.map(RateLimit::secondary)
        }
    }
}

/// Manual refresh bypasses transient backoff, never applicable server pacing.
/// UI callers inspect the GraphQL budget; REST transports select core/search.
pub fn ready(host: &str, manual: bool) -> bool {
    let at = now();
    BUDGETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&budget_key(host))
        .is_none_or(|b| b.limit(&resource(), at).is_none() && (b.due <= at || manual))
}

pub fn rate_limit(host: &str) -> Option<RateLimit> {
    BUDGETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&budget_key(host))
        .and_then(|budget| budget.limit(&resource(), now()))
}

pub(crate) fn rate_limit_for(host: &str, resource: &str) -> Option<RateLimit> {
    with_resource(resource, || rate_limit(host))
}

pub fn failed(host: &str, error: &GitHubError) {
    let rate_limit = error
        .rate_limit()
        .map(|limit| limit.with_resource(&resource()));
    // Closing or switching a review cancels reads without a host outage. Any
    // server pacing already observed still applies, including partial output.
    if rate_limit.is_none()
        && matches!(error, GitHubError::Process(failure) if failure.kind == ProcessFailureKind::Cancelled)
    {
        return;
    }
    let mut budgets = BUDGETS.lock().unwrap_or_else(|e| e.into_inner());
    let budget = budgets.entry(budget_key(host)).or_default();
    budget.failures = budget.failures.saturating_add(1);
    if let Some(limit) = rate_limit {
        if let Some(resource) = limit.resource {
            let due = budget.primary.entry(resource).or_default();
            *due = (*due).max(limit.retry_at);
            if let Some(secondary) = limit.secondary_retry_at {
                budget.due = budget.due.max(secondary);
                budget.rate_limited = true;
            }
        } else {
            budget.due = budget.due.max(limit.retry_at);
            budget.rate_limited = true;
        }
    } else if !budget.rate_limited || budget.due <= now() {
        let delay = 30u64.saturating_mul(1u64 << budget.failures.saturating_sub(1).min(6));
        // A per-host/time offset staggers windows recovering together.
        let jitter = host
            .bytes()
            .fold(now(), |n, b| n.wrapping_mul(31).wrapping_add(b as u64))
            % 11;
        budget.due = now().saturating_add(delay.min(1800) + jitter);
        budget.rate_limited = false;
    }
}
pub fn succeeded(host: &str) {
    let mut budgets = BUDGETS.lock().unwrap_or_else(|e| e.into_inner());
    let key = budget_key(host);
    if let Some(budget) = budgets.get_mut(&key) {
        budget.primary.retain(|_, due| *due > now());
        if !budget.rate_limited || budget.due <= now() {
            budget.failures = 0;
            budget.due = 0;
            budget.rate_limited = false;
        }
        if budget.primary.is_empty() && !budget.rate_limited {
            budgets.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headerless_plain_cli_graphql_failure_leaves_rest_reads_and_comments_available() {
        struct Headerless {
            commands: Mutex<Vec<CommandSpec>>,
        }
        impl ProcessExecutor for Headerless {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.commands.lock().unwrap().push(command.clone());
                let graphql = command_resource(command) == "graphql";
                if graphql {
                    assert_eq!(command.arguments[0], "pr");
                    assert_eq!(command.arguments[1], "view");
                    assert!(!command.arguments.iter().any(|arg| arg == "--include"));
                }
                Ok(ProcessOutput {
                    success: !graphql,
                    exit_code: Some(if graphql { 1 } else { 0 }),
                    signal: None,
                    stdout: if graphql {
                        vec![]
                    } else {
                        br#"{"id":1,"ok":true}"#.to_vec()
                    },
                    stderr: if graphql {
                        b"GraphQL: API rate limit already exceeded\n".to_vec()
                    } else {
                        vec![]
                    },
                })
            }
        }
        let repository =
            GitHubRepoKey::new("headerless-rate-scope.invalid", None, "owner", "repo").unwrap();
        let key = PullRequestKey::new(repository.clone(), 1).unwrap();
        let executor = Arc::new(Headerless {
            commands: Mutex::new(vec![]),
        });
        let service = GitHubService::new("gh", executor.clone());
        let cancellation = CancellationToken::new();
        with_scope("headerless-account", || {
            let error = service.view(&key, &cancellation).unwrap_err();
            let limit = error.rate_limit().unwrap();
            assert_eq!(limit.resource.as_deref(), Some("graphql"));
            assert_eq!(limit.secondary_retry_at, None);
            assert!(!ready(&repository.authority(), false));
            assert!(with_resource("core", || ready(
                &repository.authority(),
                false
            )));
            let rest = service.gh_command(["api", "--include", "repos/owner/repo"]);
            service
                .run_checked(&rest, &repository, &cancellation)
                .unwrap();
            assert!(matches!(
                service.post_single_review_comment(
                    &key,
                    &"a".repeat(40),
                    &AddReviewThreadRequest {
                        review_id: "unused".into(),
                        path: "file.rs".into(),
                        subject: ReviewThreadSubject::File,
                        body: "A comment".into(),
                    },
                    &cancellation,
                ),
                Ok(ReviewMutationOutcome::Applied(()))
            ));
            assert!(service.view(&key, &cancellation).is_err());
            assert!(
                service
                    .graphql(&repository, "query{viewer{login}}", vec![], &cancellation)
                    .is_err()
            );
            assert_eq!(executor.commands.lock().unwrap().len(), 3);
            BUDGETS
                .lock()
                .unwrap()
                .remove(&budget_key(&repository.authority()));
        });
    }

    #[test]
    fn primary_resources_do_not_block_other_transports_but_secondary_pacing_does() {
        struct Healthy {
            commands: Mutex<Vec<CommandSpec>>,
            exhausted: Mutex<Option<&'static str>>,
        }
        impl ProcessExecutor for Healthy {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.commands.lock().unwrap().push(command.clone());
                let resource = command_resource(command);
                let mut exhausted = self.exhausted.lock().unwrap();
                if *exhausted == Some(resource) {
                    *exhausted = None;
                    return Ok(ProcessOutput { success: false, exit_code: Some(1), signal: None,
                        stdout: format!("HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Resource: {resource}\r\nX-RateLimit-Reset: {}\r\n\r\n{{\"message\":\"API rate limit exceeded\"}}", now() + 3600).into_bytes(), stderr: vec![] });
                }
                Ok(ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    signal: None,
                    stdout: if command_resource(command) == "graphql" {
                        br#"{"data":{"viewer":{"login":"reader"}}}"#.to_vec()
                    } else {
                        br#"{"id":1,"ok":true}"#.to_vec()
                    },
                    stderr: vec![],
                })
            }
        }
        let host = "resource-isolation.invalid";
        let repository = GitHubRepoKey::new(host, None, "owner", "repo").unwrap();
        let executor = Arc::new(Healthy {
            commands: Mutex::new(vec![]),
            exhausted: Mutex::new(Some("graphql")),
        });
        let service = GitHubService::new("gh", executor.clone());
        let cancellation = CancellationToken::new();
        with_scope("resource-account", || {
            assert!(
                service
                    .graphql(&repository, "query{viewer{login}}", vec![], &cancellation)
                    .is_err()
            );
            assert!(!ready(host, false));
            let rest = service.gh_command(["api", "--include", "repos/owner/repo"]);
            assert!(
                service
                    .run_checked(&rest, &repository, &cancellation)
                    .is_ok()
            );
            assert!(matches!(
                service.post_single_review_comment(
                    &PullRequestKey::new(repository.clone(), 1).unwrap(),
                    &"a".repeat(40),
                    &AddReviewThreadRequest {
                        review_id: "unused".into(),
                        path: "file.rs".into(),
                        subject: ReviewThreadSubject::File,
                        body: "A comment".into()
                    },
                    &cancellation,
                ),
                Ok(ReviewMutationOutcome::Applied(()))
            ));
            assert!(
                service
                    .graphql(&repository, "query{viewer{login}}", vec![], &cancellation)
                    .is_err()
            );
            assert_eq!(executor.commands.lock().unwrap().len(), 3);
            BUDGETS.lock().unwrap().remove(&budget_key(host));
            *executor.exhausted.lock().unwrap() = Some("core");
            assert!(
                service
                    .run_checked(&rest, &repository, &cancellation)
                    .is_err()
            );
            assert!(
                ready(host, false),
                "UI GraphQL admission ignores core exhaustion"
            );
            service
                .graphql(&repository, "query{viewer{login}}", vec![], &cancellation)
                .unwrap();
            assert!(
                service
                    .run_checked(&rest, &repository, &cancellation)
                    .is_err()
            );
            assert_eq!(executor.commands.lock().unwrap().len(), 5);
            let secondary = classify("secondary rate limit\nRetry-After: 60", now()).unwrap();
            failed(host, &GitHubError::RateLimited { limit: secondary });
            assert!(!ready(host, true));
            assert!(
                service
                    .graphql(&repository, "query{viewer{login}}", vec![], &cancellation)
                    .is_err()
            );
            assert!(
                service
                    .run_checked(&rest, &repository, &cancellation)
                    .is_err()
            );
            assert_eq!(executor.commands.lock().unwrap().len(), 5);
            with_scope("another-account", || {
                assert!(ready(host, false));
                service
                    .run_checked(&rest, &repository, &cancellation)
                    .unwrap();
            });
            BUDGETS.lock().unwrap().remove(&budget_key(host));
        });
    }

    #[test]
    fn enterprise_ports_keep_primary_and_secondary_worker_and_ui_budgets_separate() {
        struct Enterprise {
            headers: String,
            commands: Mutex<Vec<CommandSpec>>,
        }
        impl ProcessExecutor for Enterprise {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.commands.lock().unwrap().push(command.clone());
                let limited = command
                    .arguments
                    .iter()
                    .any(|arg| arg.to_string_lossy().ends_with(":8443"));
                Ok(ProcessOutput {
                    success: !limited,
                    exit_code: Some(if limited { 1 } else { 0 }),
                    signal: None,
                    stdout: if limited {
                        format!("HTTP/2.0 403 Forbidden\r\n{}\r\n\r\n{{\"message\":\"API rate limit exceeded\"}}", self.headers).into_bytes()
                    } else {
                        br#"{"data":{"viewer":{"login":"reader"}}}"#.to_vec()
                    },
                    stderr: vec![],
                })
            }
        }
        for (suffix, headers) in [
            (
                "primary",
                format!(
                    "X-RateLimit-Remaining: 0\r\nX-RateLimit-Resource: graphql\r\nX-RateLimit-Reset: {}",
                    now() + 3600
                ),
            ),
            ("secondary", "Retry-After: 60".into()),
        ] {
            let host = format!("ports-{suffix}.invalid");
            let first = GitHubRepoKey::new(&host, Some(8443), "owner", "repo").unwrap();
            let second = GitHubRepoKey::new(&host, Some(9443), "owner", "repo").unwrap();
            bind_scope(&first.authority(), "same-account");
            bind_scope(&second.authority(), "same-account");
            let executor = Arc::new(Enterprise {
                headers,
                commands: Mutex::new(vec![]),
            });
            let service = GitHubService::new("gh", executor.clone());
            with_scope("same-account", || {
                assert!(
                    service
                        .graphql(
                            &first,
                            "query{viewer{login}}",
                            vec![],
                            &CancellationToken::new()
                        )
                        .is_err()
                );
                service
                    .graphql(
                        &second,
                        "query{viewer{login}}",
                        vec![],
                        &CancellationToken::new(),
                    )
                    .unwrap();
                assert!(
                    service
                        .graphql(
                            &first,
                            "query{viewer{login}}",
                            vec![],
                            &CancellationToken::new()
                        )
                        .is_err()
                );
            });
            // UI admission uses the worker's authority binding without touching
            // credential files or the shared cache on its thread.
            assert!(!ready(&first.authority(), false));
            assert!(ready(&second.authority(), false));
            assert_eq!(executor.commands.lock().unwrap().len(), 2);
            BUDGETS
                .lock()
                .unwrap()
                .retain(|key, _| !key.starts_with(&host));
            ACTIVE.lock().unwrap().remove(&first.authority());
            ACTIVE.lock().unwrap().remove(&second.authority());
        }
    }

    #[test]
    fn local_coordination_refusal_proves_no_mutation_was_submitted() {
        struct Deferred;
        impl ProcessExecutor for Deferred {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                assert_eq!(
                    command
                        .environment
                        .get(&OsString::from("CAGENTS_GH_NONBLOCKING")),
                    Some(&OsString::from("1"))
                );
                Ok(ProcessOutput { success: false, exit_code: Some(75), signal: None, stdout: vec![],
                    stderr: b"cagents-gh: deferred GitHub requests are paused by the shared rate limit.\nRetry-After: 3600\n".to_vec() })
            }
        }
        let repository =
            GitHubRepoKey::new("local-deferral.invalid", None, "owner", "repo").unwrap();
        let service = GitHubService::new("gh", Arc::new(Deferred));
        let outcome = service
            .mutate(
                &repository,
                "mutation{test}",
                vec![],
                "/test",
                &CancellationToken::new(),
            )
            .unwrap();
        let ReviewMutationOutcome::NotApplied { failure } = outcome else {
            panic!("local refusal is never an unknown write outcome")
        };
        assert!(!failure.started);
        assert!(
            failure
                .to_string()
                .contains("paused by the shared rate limit")
        );
        assert!(GitHubError::Process(failure).rate_limit().unwrap().retry_at >= now() + 3599);
        BUDGETS
            .lock()
            .unwrap()
            .retain(|key, _| !key.starts_with(&repository.host));
        ACTIVE.lock().unwrap().remove(&repository.host);
    }

    #[test]
    fn late_old_account_error_does_not_poison_the_current_account_budget() {
        struct Delayed {
            started: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl ProcessExecutor for Delayed {
            fn execute(
                &self,
                _: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.started.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Ok(ProcessOutput {
                    success: false,
                    exit_code: Some(1),
                    signal: None,
                    stdout: br#"HTTP/2.0 200 OK
Retry-After: 120

{"data":null,"errors":[{"type":"RATE_LIMITED","message":"secondary rate limit"}]}"#
                        .to_vec(),
                    stderr: vec![],
                })
            }
        }
        struct Healthy;
        impl ProcessExecutor for Healthy {
            fn execute(
                &self,
                _: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                Ok(ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    signal: None,
                    stdout: br#"{"data":{"viewer":{"login":"current"}}}"#.to_vec(),
                    stderr: vec![],
                })
            }
        }
        let host = "late-account-error.invalid";
        let repository = GitHubRepoKey::new(host, None, "owner", "repo").unwrap();
        let (started, wait_started) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        bind_scope(host, "old-account");
        let old_repository = repository.clone();
        let old = thread::spawn(move || {
            with_scope("old-account", || {
                GitHubService::new(
                    "gh",
                    Arc::new(Delayed {
                        started,
                        release: Mutex::new(wait_release),
                    }),
                )
                .graphql(
                    &old_repository,
                    "query{viewer{login}}",
                    vec![],
                    &CancellationToken::new(),
                )
            })
        });
        wait_started.recv_timeout(Duration::from_secs(5)).unwrap();
        let current = GitHubService::new("gh", Arc::new(Healthy));
        current
            .graphql(
                &repository,
                "query{viewer{login}}",
                vec![],
                &CancellationToken::new(),
            )
            .unwrap();
        release.send(()).unwrap();
        assert!(old.join().unwrap().is_err());
        assert!(
            ready(host, false),
            "an old in-flight response cannot pause the new account"
        );
        with_scope("old-account", || assert!(!ready(host, false)));
        current
            .graphql(
                &repository,
                "query{viewer{login}}",
                vec![],
                &CancellationToken::new(),
            )
            .unwrap();
        let current_scope = crate::github_status::credential_fingerprint().unwrap();
        with_scope(&current_scope, || {
            failed(
                host,
                &GitHubError::RateLimited {
                    limit: RateLimit::secondary(now() + 120),
                },
            )
        });
        assert!(
            !ready(host, false),
            "shared scan limits still pause ordinary app reads"
        );
        assert!(
            current
                .graphql(
                    &repository,
                    "query{viewer{login}}",
                    vec![],
                    &CancellationToken::new()
                )
                .is_err()
        );
        BUDGETS
            .lock()
            .unwrap()
            .retain(|key, _| !key.starts_with(host));
        ACTIVE.lock().unwrap().remove(host);
    }

    #[test]
    fn captured_credentials_isolate_pacing_and_restore_scope_after_unwind() {
        let host = "credential-scope.invalid";
        let limit = GitHubError::RateLimited {
            limit: RateLimit::secondary(now() + 120),
        };
        failed(host, &limit);
        assert!(!ready(host, false));
        with_scope("first", || {
            assert!(ready(host, false));
            failed(host, &limit);
            assert!(!ready(host, false));
            with_scope("second", || assert!(ready(host, false)));
            assert!(!ready(host, false));
        });
        let _ = std::panic::catch_unwind(|| with_scope("panic", || panic!("scope test")));
        assert_eq!(budget_key(host), host);
        with_scope("first", || {
            assert!(!ready(host, false));
            BUDGETS.lock().unwrap().remove(&budget_key(host));
        });
        BUDGETS.lock().unwrap().remove(host);
    }
    #[test]
    fn classifies_primary_secondary_graphql_and_abuse_limits() {
        for text in [
            "API rate limit exceeded\nX-RateLimit-Reset: 200",
            "X-RateLimit-Remaining: 0\nX-RateLimit-Reset: 200",
            "secondary rate limit\nRetry-After: 100",
            "GraphQL RATE_LIMITED\nRetry-After: 100",
            "abuse detection\nRetry-After: 100",
        ] {
            assert_eq!(
                RateLimit::classify(text, 100).unwrap().retry_at,
                200,
                "{text}"
            );
        }
        assert_eq!(RateLimit::classify("secondary rate limit\nRetry-After: 100\nX-RateLimit-Remaining: 4000\nX-RateLimit-Reset: 3600", 100).unwrap().retry_at, 200);
        assert_eq!(
            RateLimit::classify(
                "X-RateLimit-Remaining: 0\nX-RateLimit-Reset: 105\nRetry-After: 20",
                100
            )
            .unwrap()
            .retry_at,
            120
        );
        assert_eq!(
            RateLimit::classify(
                "X-RateLimit-Remaining: 0\nX-RateLimit-Reset: 150\nRetry-After: 20",
                100
            )
            .unwrap()
            .retry_at,
            150
        );
        assert!(RateLimit::classify("network disconnected", 100).is_none());
        assert!(RateLimit::classify("Access-Control-Expose-Headers: Retry-After, X-RateLimit-Remaining, X-RateLimit-Reset\nX-RateLimit-Remaining: 4000\nX-RateLimit-Reset: 200", 100).is_none());
    }
    #[test]
    fn one_host_budget_pauses_all_pollers_and_manual_bypasses_only_transient_failures() {
        let host = "polling-test.invalid";
        failed(host, &GitHubError::Inspection("offline".into()));
        assert!(!ready(host, false));
        assert!(ready(host, true));
        let first = BUDGETS.lock().unwrap().get(host).unwrap().due;
        failed(host, &GitHubError::Inspection("offline".into()));
        assert!(BUDGETS.lock().unwrap().get(host).unwrap().due > first);
        failed(
            host,
            &GitHubError::RateLimited {
                limit: RateLimit::secondary(now() + 120),
            },
        );
        assert!(!ready(host, false));
        assert!(!ready(host, true));
        succeeded(host);
        assert!(!ready(host, false));
        BUDGETS.lock().unwrap().remove(host);
    }

    #[test]
    fn cancelled_process_leaves_new_and_existing_host_budgets_unchanged() {
        let host = "polling-cancelled-process-test.invalid";
        let mut failure = ProcessFailure {
            kind: ProcessFailureKind::Cancelled,
            program: "gh".into(),
            exit_code: None,
            signal: None,
            stdout: vec![],
            stderr: vec![],
            message: "operation cancelled".into(),
            started: false,
        };
        for started in [false, true] {
            failure.started = started;
            failed(host, &GitHubError::Process(failure.clone()));
            assert!(ready(host, false));
            assert!(!BUDGETS.lock().unwrap().contains_key(host));
        }
        // A timeout remains a transient failure even with the same message.
        let cancelled = GitHubError::Process(failure.clone());
        failure.kind = ProcessFailureKind::TimedOut;
        failed(host, &GitHubError::Process(failure));
        assert!(!ready(host, false));
        assert!(ready(host, true));
        let snapshot = || {
            let budgets = BUDGETS.lock().unwrap();
            let budget = budgets.get(host).unwrap();
            (budget.failures, budget.due, budget.rate_limited)
        };
        let transient = snapshot();
        failed(host, &cancelled);
        assert_eq!(snapshot(), transient);

        let limit = RateLimit::secondary(now() + 120);
        failed(
            host,
            &GitHubError::RateLimited {
                limit: limit.clone(),
            },
        );
        let rate_limited = snapshot();
        failed(host, &cancelled);
        assert_eq!(snapshot(), rate_limited);
        assert_eq!(rate_limit(host), Some(limit));
        assert!(!ready(host, true));
        BUDGETS.lock().unwrap().remove(host);
    }

    #[test]
    fn cancelled_process_preserves_server_pacing_from_partial_output() {
        let host = "polling-cancelled-rate-limit-test.invalid";
        let retry_at = now() + 120;
        failed(
            host,
            &GitHubError::Process(ProcessFailure {
                kind: ProcessFailureKind::Cancelled,
                program: "gh".into(),
                exit_code: None,
                signal: None,
                stdout: format!(
                    "HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {retry_at}\r\n\r\n"
                )
                .into_bytes(),
                stderr: vec![],
                message: "operation cancelled".into(),
                started: true,
            }),
        );
        assert_eq!(
            rate_limit(host),
            Some(RateLimit::primary("graphql", retry_at))
        );
        assert!(!ready(host, false));
        assert!(!ready(host, true));
        BUDGETS.lock().unwrap().remove(host);
    }

    #[test]
    fn any_completed_request_clears_a_transient_backoff() {
        struct Healthy;
        impl ProcessExecutor for Healthy {
            fn execute(
                &self,
                _: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                Ok(ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    signal: None,
                    stdout: br#"{"data":{"viewer":{"login":"octo"}}}"#.to_vec(),
                    stderr: Vec::new(),
                })
            }
        }
        let repository =
            GitHubRepoKey::new("polling-recover.invalid", None, "owner", "repo").unwrap();
        failed(&repository.host, &GitHubError::Inspection("offline".into()));
        assert!(!ready(&repository.host, false));
        // A surface other than the one that backed off completes a request.
        GitHubService::new("gh", Arc::new(Healthy))
            .graphql(
                &repository,
                "query{viewer{login}}",
                Vec::new(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(ready(&repository.host, false));
    }
}
