//! Sidebar history needs identity and terminal state, not live check trees.
use super::*;

/// Checks GitHub doesn't date, such as a rerun of a finished one, are read
/// again at least this often.
const RELIST_AFTER: Duration = Duration::from_secs(5 * 60);
/// A probe GitHub refused, such as for a repository with issues disabled, is
/// asked again after this.
const REFUSED_FOR: Duration = Duration::from_secs(10 * 60);
/// A repository's name and owner rarely change under its remotes, and `gh
/// repo view` costs a GraphQL request.
const REPOSITORY_FOR: Duration = Duration::from_secs(60 * 60);

/// Whose credentials read which repository.
type ListingKey = (Option<String>, GitHubRepoKey);

/// What the sidebar last read of a repository, and the validator taken
/// before reading it.
struct Listing {
    at: Instant,
    etag: Option<String>,
    settling: bool,
    pull_requests: Vec<PullRequest>,
}

#[derive(Default)]
struct Listings {
    read: HashMap<ListingKey, Listing>,
    refused: HashMap<ListingKey, Instant>,
}

static LISTINGS: LazyLock<Mutex<Listings>> = LazyLock::new(Default::default);
/// What each checkout's remotes resolved to, and when.
type Resolution = (Instant, Vec<crate::repository::RemoteRecord>, GitHubRepoKey);
static REPOSITORIES: LazyLock<Mutex<HashMap<PathBuf, Resolution>>> =
    LazyLock::new(Default::default);

/// Whether the sidebar's listing of a repository is still current.
pub enum SidebarProbe {
    /// GitHub says nothing changed and nothing is settling.
    Unchanged(Vec<PullRequest>),
    /// Read it, and keep the validator with what was read.
    Read(SidebarValidator),
}

/// The probe's validator, taken before a listing is read so that a change
/// landing between the two shows as a change next time.
pub struct SidebarValidator {
    key: ListingKey,
    etag: Option<String>,
}

const HISTORY_FIELDS: &str = "id number title state isDraft headRefName headRefOid baseRefName baseRefOid author{login} updatedAt url headRepository{name owner{login}}";

impl<E: ProcessExecutor> GitHubService<E> {
    /// [`Self::resolve_repository`], remembered for the checkout's remotes
    /// and, with `reuse`, answered from that.
    pub fn resolve_repository_cached(
        &self,
        repository: &Path,
        remotes: &[crate::repository::RemoteRecord],
        reuse: bool,
        cancellation: &CancellationToken,
    ) -> Result<GitHubRepoKey, GitHubError> {
        if reuse
            && let Some((at, resolved_from, resolved)) = lock(&REPOSITORIES).get(repository)
            && resolved_from == remotes
            && at.elapsed() < REPOSITORY_FOR
        {
            return Ok(resolved.clone());
        }
        let resolved = self.resolve_repository(repository, remotes, cancellation)?;
        let mut repositories = lock(&REPOSITORIES);
        repositories.retain(|_, (at, ..)| at.elapsed() < REPOSITORY_FOR);
        repositories.insert(
            repository.to_path_buf(),
            (Instant::now(), remotes.to_vec(), resolved.clone()),
        );
        Ok(resolved)
    }

    /// Asks GitHub, for free when nothing changed, whether any of the
    /// repository's issues and pull requests changed since the sidebar last
    /// read it. A pull request's `updatedAt` moves with pushes, reviews,
    /// review requests, comments, edits, merges and closes, but not with its
    /// checks, so a listing whose checks are settling is read again anyway,
    /// as is one `RELIST_AFTER` old. Without `reuse` the listing is read
    /// regardless, and the probe supplies the validator the next compares
    /// with.
    pub fn sidebar_probe(
        &self,
        repository: &GitHubRepoKey,
        reuse: bool,
        cancellation: &CancellationToken,
    ) -> SidebarProbe {
        let key = (
            crate::github_status::credential_fingerprint().ok(),
            repository.clone(),
        );
        let (validator, current) = {
            let listings = lock(&LISTINGS);
            if listings
                .refused
                .get(&key)
                .is_some_and(|at| at.elapsed() < REFUSED_FOR)
            {
                return SidebarProbe::Read(SidebarValidator { key, etag: None });
            }
            match listings.read.get(&key) {
                Some(listing) => (
                    listing.etag.clone(),
                    (reuse && listing.at.elapsed() < RELIST_AFTER && !listing.settling)
                        .then(|| listing.pull_requests.clone()),
                ),
                None => (None, None),
            }
        };
        let path = repository_probe_path(repository);
        let etag = match self.revalidate(repository, &path, validator.as_deref(), cancellation) {
            Ok(Revalidation::NotModified) => match current {
                Some(pull_requests) => return SidebarProbe::Unchanged(pull_requests),
                None => validator,
            },
            Ok(Revalidation::Modified { etag, .. }) => etag,
            Ok(Revalidation::Refused) => {
                lock(&LISTINGS).refused.insert(key.clone(), Instant::now());
                None
            }
            // Out of quota, deferred or unreachable: the validator is older
            // than the listing about to be read, so it still serves.
            Err(_) => validator,
        };
        SidebarProbe::Read(SidebarValidator { key, etag })
    }

    /// The open page and recent history, kept with `validator` when both
    /// were read.
    pub fn sidebar_listing(
        &self,
        repository: &GitHubRepoKey,
        validator: SidebarValidator,
        limit: u16,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        let at = Instant::now();
        let mut pull_requests = self.sidebar_open(repository, limit, cancellation)?;
        let settling = checks_settling(&pull_requests);
        // Closed history must not consume the open-PR page. Its failure also
        // must not discard successfully refreshed open conversation authority,
        // but leaves nothing new to keep: what was kept before is still older
        // than its validator, so the next probe still sees what changed since.
        let history = self.recent_history(repository, limit, cancellation);
        // Read as another account than it would be kept for.
        let switched = crate::github_status::credential_fingerprint().ok() != validator.key.0;
        let mut listings = lock(&LISTINGS);
        listings
            .read
            .retain(|_, listing| listing.at.elapsed() < REPOSITORY_FOR);
        listings.refused.retain(|_, at| at.elapsed() < REFUSED_FOR);
        if let Ok(history) = history {
            append_recent_history(&mut pull_requests, history);
            if !switched {
                listings.read.insert(
                    validator.key,
                    Listing {
                        at,
                        etag: validator.etag,
                        settling,
                        pull_requests: pull_requests.clone(),
                    },
                );
            }
        }
        Ok(pull_requests)
    }

    /// The sidebar shows aggregate check state. Individual check runs are
    /// loaded by the inspector when the person opens the pull request.
    pub fn sidebar_open(
        &self,
        repository: &GitHubRepoKey,
        limit: u16,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        self.sidebar_page(repository, limit, false, cancellation)
    }

    fn sidebar_page(
        &self,
        repository: &GitHubRepoKey,
        limit: u16,
        history: bool,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        let (operation, state, fields) = if history {
            ("SidebarHistory", "", HISTORY_FIELDS)
        } else {
            (
                "SidebarPullRequests",
                "states:OPEN,",
                BROWSER_PULL_REQUEST_FIELDS,
            )
        };
        let query = format!(
            "query {operation}($owner:String!,$name:String!,$limit:Int!){{rateLimit{{cost}} repository(owner:$owner,name:$name){{pullRequests(first:$limit,{state}orderBy:{{field:CREATED_AT,direction:DESC}}){{nodes{{{fields}}}}}}}}}"
        );
        let data = self.graphql(
            repository,
            &query,
            vec![
                ("owner", repository.owner.as_str().into()),
                ("name", repository.name.as_str().into()),
                ("limit", GraphQlValue::Int(limit.clamp(1, 100).into())),
            ],
            cancellation,
        )?;
        let nodes = data
            .pointer("/repository/pullRequests/nodes")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                GitHubError::Inspection("GitHub returned no sidebar pull requests".into())
            })?;
        nodes
            .iter()
            .cloned()
            .map(|node| decode_browser_pull_request(node, repository))
            .collect()
    }

    /// Keep the same recent-history membership as the full listing, while
    /// excluding expensive checks, labels and mergeability calculations.
    /// Open rows from this read must not replace the full open-page rows.
    pub fn recent_history(
        &self,
        repository: &GitHubRepoKey,
        limit: u16,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PullRequest>, GitHubError> {
        self.sidebar_page(repository, limit, true, cancellation)
    }
}

/// The repository's most recently updated issue or pull request, whose
/// validator moves with a change to any of them.
pub(super) fn repository_probe_path(repository: &GitHubRepoKey) -> String {
    format!(
        "repos/{}/{}/issues?state=all&sort=updated&direction=desc&per_page=1",
        repository.owner, repository.name
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The open page owns live checks. A history read also contains open rows, so
/// replacing duplicates would erase their check and review state.
pub fn append_recent_history(open: &mut Vec<PullRequest>, history: Vec<PullRequest>) {
    for entry in history {
        if !open.iter().any(|current| current.number == entry.number) {
            open.push(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers the repository probe by validator and both sidebar pages,
    /// naming each call.
    struct Forge {
        etag: Mutex<&'static str>,
        checks: Mutex<&'static str>,
        /// How the probe fails, if it does: a status line.
        fails: Mutex<Option<&'static str>>,
        history_fails: Mutex<bool>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl Forge {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                etag: Mutex::new("W/\"1\""),
                checks: Mutex::new("SUCCESS"),
                fails: Mutex::default(),
                history_fails: Mutex::default(),
                calls: Mutex::default(),
            })
        }
    }

    impl ProcessExecutor for Forge {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            let arguments = command
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let output = |success, stdout: String| {
                Ok(ProcessOutput {
                    success,
                    exit_code: Some(if success { 0 } else { 1 }),
                    signal: None,
                    stdout: stdout.into_bytes(),
                    stderr: vec![],
                })
            };
            if arguments.iter().any(|argument| argument == "view") {
                self.calls.lock().unwrap().push("resolve");
                return output(
                    true,
                    r#"{"nameWithOwner":"sidebar-probe/repo","url":"https://github.com/sidebar-probe/repo"}"#
                        .into(),
                );
            }
            if arguments.iter().any(|argument| argument == "graphql") {
                let history = arguments
                    .iter()
                    .any(|argument| argument.contains("SidebarHistory"));
                self.calls
                    .lock()
                    .unwrap()
                    .push(if history { "history" } else { "open" });
                if history && *self.history_fails.lock().unwrap() {
                    return output(false, String::new());
                }
                let nodes = if history {
                    serde_json::json!([])
                } else {
                    serde_json::json!([{
                        "number": 1, "title": "t", "state": "OPEN", "isDraft": false,
                        "headRefName": "b", "headRefOid": "a", "baseRefName": "main",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "url": "https://github.com/sidebar-probe/repo/pull/1",
                        "headRepository": {"name": "repo", "owner": {"login": "sidebar-probe"}},
                        "commits": {"nodes": [{"commit": {"statusCheckRollup": {
                            "state": *self.checks.lock().unwrap()
                        }}}]},
                    }])
                };
                let body =
                    serde_json::json!({"data": {"repository": {"pullRequests": {"nodes": nodes}}}});
                return output(true, body.to_string());
            }
            self.calls.lock().unwrap().push("probe");
            assert_eq!(
                arguments.last().unwrap(),
                "repos/sidebar-probe/repo/issues?state=all&sort=updated&direction=desc&per_page=1"
            );
            if let Some(status) = *self.fails.lock().unwrap() {
                return output(false, format!("HTTP/2.0 {status}\r\n\r\n{{}}"));
            }
            let etag = *self.etag.lock().unwrap();
            if arguments.contains(&format!("If-None-Match: {etag}")) {
                return output(false, "HTTP/2.0 304 Not Modified\r\n\r\n".into());
            }
            output(true, format!("HTTP/2.0 200 OK\r\nEtag: {etag}\r\n\r\n[]"))
        }
    }

    #[test]
    fn an_unchanged_sidebar_listing_costs_a_free_probe_until_its_checks_or_age_say_otherwise() {
        let forge = Forge::new();
        let service = GitHubService::new("gh", forge.clone());
        let repository = GitHubRepoKey::new("github.com", None, "sidebar-probe", "repo").unwrap();
        let cancellation = CancellationToken::new();
        let read = |reuse| {
            let listed = match service.sidebar_probe(&repository, reuse, &cancellation) {
                SidebarProbe::Unchanged(pull_requests) => pull_requests,
                SidebarProbe::Read(validator) => service
                    .sidebar_listing(&repository, validator, 50, &cancellation)
                    .unwrap(),
            };
            assert_eq!(listed.len(), 1);
            std::mem::take(&mut *forge.calls.lock().unwrap())
        };

        // Opening a project reads it, and its probe's validator serves next.
        assert_eq!(read(false), ["probe", "open", "history"]);
        assert_eq!(read(true), ["probe"]);
        assert_eq!(read(false), ["probe", "open", "history"]);
        // A change GitHub dates.
        *forge.etag.lock().unwrap() = "W/\"2\"";
        assert_eq!(read(true), ["probe", "open", "history"]);
        assert_eq!(read(true), ["probe"]);
        // Pending checks change without the probe noticing.
        *forge.checks.lock().unwrap() = "PENDING";
        assert_eq!(read(false), ["probe", "open", "history"]);
        *forge.checks.lock().unwrap() = "SUCCESS";
        assert_eq!(read(true), ["probe", "open", "history"]);
        assert_eq!(read(true), ["probe"]);
        // An old listing is read again.
        let key = (
            crate::github_status::credential_fingerprint().ok(),
            repository.clone(),
        );
        lock(&LISTINGS).read.get_mut(&key).unwrap().at -= RELIST_AFTER;
        assert_eq!(read(true), ["probe", "open", "history"]);
        // A probe that fails in passing keeps its validator for the next.
        *forge.fails.lock().unwrap() = Some("502 Bad Gateway");
        assert_eq!(read(true), ["probe", "open", "history"]);
        *forge.fails.lock().unwrap() = None;
        assert_eq!(read(true), ["probe"]);
        // A repository GitHub refuses to probe is read every time, and asked
        // again only after a while.
        *forge.fails.lock().unwrap() = Some("410 Gone");
        assert_eq!(read(true), ["probe", "open", "history"]);
        assert_eq!(read(true), ["open", "history"]);
        *forge.fails.lock().unwrap() = None;
        let refused_at = lock(&LISTINGS).refused.get(&key).copied().unwrap();
        lock(&LISTINGS)
            .refused
            .insert(key.clone(), refused_at - REFUSED_FOR);
        assert_eq!(read(true), ["probe", "open", "history"]);
        assert_eq!(read(true), ["probe"]);

        // A failed history read still shows the open page, and keeps what
        // was kept before, whose validator still sees the change.
        *forge.history_fails.lock().unwrap() = true;
        *forge.etag.lock().unwrap() = "W/\"3\"";
        assert_eq!(read(true), ["probe", "open", "history"]);
        *forge.history_fails.lock().unwrap() = false;
        assert_eq!(read(true), ["probe", "open", "history"]);
        assert_eq!(read(true), ["probe"]);

        // A listing read after the account changed is not kept for the
        // account it started as.
        let other = (Some("another account".to_owned()), repository.clone());
        let validator = SidebarValidator {
            key: other.clone(),
            etag: Some("W/\"3\"".into()),
        };
        service
            .sidebar_listing(&repository, validator, 50, &cancellation)
            .unwrap();
        assert!(!lock(&LISTINGS).read.contains_key(&other));
    }

    #[test]
    fn a_checkout_resolves_again_when_its_remotes_or_gh_default_change() {
        let forge = Forge::new();
        let service = GitHubService::new("gh", forge.clone());
        let cancellation = CancellationToken::new();
        let checkout = Path::new("/sidebar-probe/resolution");
        let origin = crate::repository::RemoteRecord {
            name: "origin".into(),
            fetch_url: Some("https://github.com/sidebar-probe/repo.git".into()),
            push_url: None,
            gh_resolved: None,
        };
        let resolve = |remotes: &[_], reuse| {
            let key = service
                .resolve_repository_cached(checkout, remotes, reuse, &cancellation)
                .unwrap();
            assert_eq!(key.name, "repo");
            std::mem::take(&mut *forge.calls.lock().unwrap()).len()
        };
        let remotes = [origin.clone()];
        assert_eq!(resolve(&remotes, false), 1);
        assert_eq!(resolve(&remotes, true), 0);
        // `gh repo set-default` changes what gh resolves, but no URL.
        let defaulted = [crate::repository::RemoteRecord {
            gh_resolved: Some("base".into()),
            ..origin
        }];
        assert_eq!(resolve(&defaulted, true), 1);
        assert_eq!(resolve(&defaulted, true), 0);
        // Opening or retrying the project always resolves.
        assert_eq!(resolve(&defaulted, false), 1);
    }

    #[test]
    fn history_keeps_the_same_page_without_live_check_connections() {
        #[derive(Default)]
        struct Empty(Mutex<Vec<CommandSpec>>);
        impl ProcessExecutor for Empty {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                self.0.lock().unwrap().push(command.clone());
                Ok(ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    signal: None,
                    stdout: br#"{"data":{"repository":{"pullRequests":{"nodes":[]}}}}"#.to_vec(),
                    stderr: vec![],
                })
            }
        }
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let executor = Arc::new(Empty::default());
        let service = GitHubService::new("gh", executor.clone());
        let cancellation = CancellationToken::new();
        assert!(
            service
                .recent_history(&repository, 50, &cancellation)
                .unwrap()
                .is_empty()
        );
        assert!(
            service
                .sidebar_open(&repository, 50, &cancellation)
                .unwrap()
                .is_empty()
        );
        let calls = executor.0.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "one request per page, with no gh CLI discovery queries"
        );
        let query = |command: &CommandSpec| {
            command
                .arguments
                .iter()
                .find_map(|arg| arg.to_str().and_then(|s| s.strip_prefix("query=")))
                .unwrap()
                .to_owned()
        };
        let history = query(&calls[0]);
        assert!(history.contains("field:CREATED_AT,direction:DESC"));
        assert!(!history.contains("states:OPEN"));
        for expensive in [
            "statusCheckRollup",
            "mergeable",
            "mergeStateStatus",
            "labels",
        ] {
            assert!(!history.contains(expensive));
        }
        let open = query(&calls[1]);
        assert!(open.contains("states:OPEN"));
        assert!(open.contains("statusCheckRollup{state}"));
        assert!(!open.contains("contexts("));
        assert!(open.contains("rateLimit{cost}"));
    }

    #[test]
    fn history_never_erases_live_check_state() {
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let live = parse_pull_request(br#"{"number":7,"title":"Live","state":"OPEN","headRefName":"feature","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7","statusCheckRollup":[{"__typename":"StatusContext","context":"ci","state":"FAILURE"}]}"#, &repository).unwrap();
        let history = parse_pull_request(br#"{"number":7,"title":"History","state":"OPEN","headRefName":"feature","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/7"}"#, &repository).unwrap();
        let closed = parse_pull_request(br#"{"number":8,"title":"Closed","state":"MERGED","headRefName":"old","baseRefName":"main","updatedAt":"now","url":"https://github.com/owner/repo/pull/8"}"#, &repository).unwrap();
        let mut rows = vec![live.clone()];
        append_recent_history(&mut rows, vec![history, closed.clone()]);
        assert_eq!(rows, vec![live, closed]);
        assert!(!rows[0].check_summaries().is_empty());
    }
}
