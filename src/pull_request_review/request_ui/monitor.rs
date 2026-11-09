//! Each shell consumes the repository scan shared across local windows.
use super::*;
use crate::github::review_requests::{scan::Scan, shared};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// A failed local cache operation is retried once a minute. Network backoff
/// and server pacing live in the shared repository state.
const RETRY: Duration = Duration::from_secs(60);
/// Across repositories, a shell starts one shared-cache read per `PACE` after
/// spending its initial burst. The shared repository state decides whether a
/// read needs GitHub; several windows cannot each spend a repository's budget.
const PACE: Duration = Duration::from_secs(6);
const BURST: u32 = 10;

/// How long a shell's monitor may wait to start a read that has fallen due
/// while its window is focused. A settled repository falls due once a minute
/// however often this ticks, and a tick with nothing due changes
/// nothing.
const FOCUSED_TICK: Duration = Duration::from_secs(10);
/// Out of focus, discovery mostly serves the desktop alerts, which exist for
/// someone looking elsewhere. Two minutes keeps those on a human timescale,
/// while waking the window a twelfth as often. A shell
/// ticks once more when its window regains focus, so whatever fell due in
/// the meantime is read at once.
const BACKGROUND_TICK: Duration = Duration::from_secs(2 * 60);

/// How often a shell should tick its monitor.
pub fn tick_interval(focused: bool) -> Duration {
    if focused {
        FOCUSED_TICK
    } else {
        BACKGROUND_TICK
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Tick,
    SharedLoaded {
        repository: GitHubRepoKey,
        generation: u64,
        result: Result<shared::PollResult, String>,
    },
}

struct Entry {
    scan: Scan,
    snapshot: Arc<Snapshot>,
    generation: u64,
    cancellation: CancellationToken,
    due: Instant,
    last_shared_observation: Option<(String, u64)>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[derive(Default)]
pub struct Monitor {
    entries: HashMap<GitHubRepoKey, Entry>,
    generation: u64,
    // Keep the global slot until even a cancelled worker acknowledges completion.
    busy: Option<(GitHubRepoKey, u64)>,
    observed: Vec<crate::notifications::review::ReviewRequest>,
    // Followers can catch up several pages at once, but each report RPC accepts
    // one page. Drain the remaining observations through local messages.
    pending_observed: VecDeque<(u64, crate::notifications::review::ReviewRequest)>,
    /// When the next read would be on schedule were reads started at exactly
    /// one per [`PACE`]; a read may start while this is less than a burst
    /// ahead of now (the generic cell rate algorithm).
    paced: Option<Instant>,
    /// Advances whenever [`Monitor::snapshots`] would yield something else.
    revision: u64,
}

impl Monitor {
    /// Spends one read of the budget if any is left at `now`.
    fn admit(&mut self, now: Instant) -> bool {
        let paced = self.paced.map_or(now, |paced| paced.max(now));
        if paced > now + PACE * (BURST - 1) {
            return false;
        }
        self.paced = Some(paced + PACE);
        true
    }

    /// A bounded batch of newly consumed shared observations. Displayed rows
    /// alone never grant an alert, and ordinary ticks never replay a page.
    pub fn take_observed(&mut self) -> Vec<crate::notifications::review::ReviewRequest> {
        std::mem::take(&mut self.observed)
    }

    pub fn target(
        &self,
        repository: &GitHubRepoKey,
        number: u64,
    ) -> Option<crate::notifications::Target> {
        let (repository, entry) = self
            .entries
            .iter()
            .find(|(key, _)| key.same_repository(repository))?;
        let viewer = entry.scan.viewer.as_ref()?;
        let request = entry.scan.requested.get(&number)?;
        Some(crate::notifications::Target::PullRequest(
            crate::notifications::review::ReviewRequest {
                host: repository.host.clone(),
                port: repository.port,
                owner: repository.owner.clone(),
                repository: repository.name.clone(),
                number,
                viewer: viewer.id.clone(),
                request: request.current.id.clone(),
            },
        ))
    }

    /// Unchanged while every snapshot, and the set of repositories they
    /// belong to, stays the same, so a shell can skip republishing them.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    #[cfg(test)]
    pub(crate) fn in_flight_for_test(&self) -> Option<&(GitHubRepoKey, u64)> {
        self.busy.as_ref()
    }

    /// Makes every repository due, with the request budget refilled.
    #[cfg(test)]
    pub(crate) fn make_due_for_test(&mut self) {
        let now = Instant::now();
        for entry in self.entries.values_mut() {
            entry.due = now;
        }
        self.paced = None;
    }

    pub fn snapshots(&self) -> impl Iterator<Item = (&GitHubRepoKey, &Arc<Snapshot>)> {
        self.entries
            .iter()
            .map(|(key, entry)| (key, &entry.snapshot))
    }

    pub fn update(
        &mut self,
        action: Action,
        repositories: impl IntoIterator<Item = GitHubRepoKey>,
    ) -> iced::Task<Action> {
        let repositories = repositories
            .into_iter()
            .map(|mut key| {
                key.host.make_ascii_lowercase();
                key.owner.make_ascii_lowercase();
                key.name.make_ascii_lowercase();
                key
            })
            .collect::<std::collections::HashSet<_>>();
        let state = self;
        state.observed.clear();
        let watched = state.entries.len();
        state
            .entries
            .retain(|repository, _| repositories.iter().any(|key| key == repository));
        let mut revised = state.entries.len() != watched;
        for repository in &repositories {
            if !state.entries.contains_key(repository) {
                revised = true;
                state.generation = state.generation.wrapping_add(1);
                state.entries.insert(
                    repository.clone(),
                    Entry {
                        scan: Scan::default(),
                        snapshot: Arc::new(Snapshot::default()),
                        generation: state.generation,
                        cancellation: CancellationToken::new(),
                        due: Instant::now(),
                        last_shared_observation: None,
                    },
                );
            }
        }
        if revised {
            state.revision = state.revision.wrapping_add(1);
        }
        match action {
            Action::SharedLoaded {
                repository,
                generation,
                result,
            } => {
                if state.busy.as_ref() != Some(&(repository.clone(), generation)) {
                    return iced::Task::none();
                }
                state.busy = None;
                if let Some(entry) = state
                    .entries
                    .get_mut(&repository)
                    .filter(|entry| entry.generation == generation)
                {
                    let error = match result {
                        Ok(result) => {
                            entry.scan = result.scan;
                            // The shared state carries server pacing. Recheck
                            // its credential key within one minute, including
                            // during an old account's transient backoff.
                            entry.due = Instant::now()
                                + Duration::from_secs(
                                    result
                                        .due
                                        .saturating_sub(crate::github::polling::now())
                                        .clamp(1, 60),
                                );
                            if entry.last_shared_observation.as_ref() != Some(&result.observation) {
                                let since = entry
                                    .last_shared_observation
                                    .as_ref()
                                    .filter(|(incarnation, _)| incarnation == &result.observation.0)
                                    .map_or(0, |(_, revision)| *revision);
                                entry.last_shared_observation = Some(result.observation);
                                if let Some(viewer) = &entry.scan.viewer {
                                    let mut queued = state
                                        .pending_observed
                                        .iter()
                                        .enumerate()
                                        .map(|(index, (_, request))| (request.clone(), index))
                                        .collect::<HashMap<_, _>>();
                                    for page in result.observed {
                                        if page.revision <= since || page.viewer != viewer.id {
                                            continue;
                                        }
                                        for (number, request_id) in page.requests {
                                            if entry.scan.requested.get(&number).is_some_and(
                                                |request| request.current.id == request_id,
                                            ) {
                                                let request =
                                                    crate::notifications::review::ReviewRequest {
                                                        host: repository.host.clone(),
                                                        port: repository.port,
                                                        owner: repository.owner.clone(),
                                                        repository: repository.name.clone(),
                                                        number,
                                                        viewer: viewer.id.clone(),
                                                        request: request_id,
                                                    };
                                                if let Some(&index) = queued.get(&request) {
                                                    state.pending_observed[index].0 = state
                                                        .pending_observed[index]
                                                        .0
                                                        .max(page.at);
                                                } else {
                                                    queued.insert(
                                                        request.clone(),
                                                        state.pending_observed.len(),
                                                    );
                                                    state
                                                        .pending_observed
                                                        .push_back((page.at, request));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            result.error
                        }
                        Err(error) => {
                            entry.due = Instant::now() + RETRY;
                            Some(error)
                        }
                    };
                    if entry.snapshot.viewer != entry.scan.viewer
                        || entry.snapshot.requested != entry.scan.requested
                        || entry.snapshot.error != error
                    {
                        entry.snapshot = Arc::new(Snapshot {
                            viewer: entry.scan.viewer.clone(),
                            requested: entry.scan.requested.clone(),
                            error,
                        });
                        state.revision = state.revision.wrapping_add(1);
                    }
                }
            }
            Action::Tick => {}
        };
        // A new viewer, closed request or removed repository revokes queued
        // authority before delivery. Unchanged cached pages are never replayed.
        let observed_at = crate::github::polling::now();
        state.pending_observed.retain(|(at, request)| {
            observed_at.saturating_sub(*at) < 5 * 60
                && state.entries.iter().any(|(repository, entry)| {
                    repository.host == request.host
                        && repository.port == request.port
                        && repository.owner == request.owner
                        && repository.name == request.repository
                        && entry.snapshot.error.is_none()
                        && entry
                            .scan
                            .viewer
                            .as_ref()
                            .is_some_and(|viewer| viewer.id == request.viewer)
                        && entry
                            .scan
                            .requested
                            .get(&request.number)
                            .is_some_and(|current| current.current.id == request.request)
                })
        });
        let mut numbers = std::collections::HashSet::new();
        while state.observed.len() < 50 {
            let Some((_, request)) = state.pending_observed.pop_front() else {
                break;
            };
            if numbers.insert(request.clone()) {
                state.observed.push(request);
            }
        }
        if !state.pending_observed.is_empty() {
            // Notification RPCs accept fifty observations. Drain a follower's
            // backlog through bounded local messages now, rather than waiting
            // another background interval or spending another GitHub request.
            return iced::Task::done(Action::Tick);
        }
        if state.busy.is_some() {
            return iced::Task::none();
        }
        let now = Instant::now();
        let Some(repository) = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.due <= now)
            .min_by_key(|(_, entry)| entry.due)
            .map(|(key, _)| key.clone())
        else {
            return iced::Task::none();
        };
        if !state.admit(now) {
            return iced::Task::none();
        }
        let entry = state.entries.get_mut(&repository).expect("selected entry");
        let generation = entry.generation;
        let cancellation = entry.cancellation.clone();
        state.busy = Some((repository.clone(), generation));
        iced::Task::perform(
            async move {
                let target = repository.clone();
                let result =
                    tokio::task::spawn_blocking(move || shared::poll(&target, &cancellation))
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                Action::SharedLoaded {
                    repository,
                    generation,
                    result,
                }
            },
            |action| action,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::review_requests::{
        Current, Entry as RequestEntry, Page, Viewer, scan::Work,
    };

    fn repository() -> GitHubRepoKey {
        GitHubRepoKey::new("github.com", None, "o", "r").unwrap()
    }

    fn result(viewer: &str, requested: &[u64], revision: u64) -> shared::PollResult {
        let mut scan = Scan::default();
        scan.page(
            &Work::Page { cursor: None },
            Page {
                viewer: Viewer {
                    id: viewer.into(),
                    login: viewer.into(),
                },
                entries: requested
                    .iter()
                    .map(|&number| RequestEntry {
                        number,
                        title: format!("PR {number}"),
                        url: format!("https://github.com/o/r/pull/{number}")
                            .parse()
                            .unwrap(),
                        viewer_latest_review_request: Some(Current {
                            id: format!("request-{number}"),
                            as_code_owner: false,
                            requested_reviewer: None,
                        }),
                    })
                    .collect(),
                next_cursor: None,
            },
        )
        .unwrap();
        shared::PollResult {
            scan,
            error: None,
            due: crate::github::polling::now() + 60,
            observation: ("incarnation".into(), revision),
            observed: vec![shared::Observation {
                revision,
                at: crate::github::polling::now(),
                viewer: viewer.into(),
                requests: requested
                    .iter()
                    .map(|&number| (number, format!("request-{number}")))
                    .collect(),
            }],
        }
    }

    fn load(monitor: &mut Monitor, result: Result<shared::PollResult, String>) {
        monitor.make_due_for_test();
        drop(monitor.update(Action::Tick, [repository()]));
        let (_, generation) = monitor.busy.clone().unwrap();
        drop(monitor.update(
            Action::SharedLoaded {
                repository: repository(),
                generation,
                result,
            },
            [repository()],
        ));
    }

    #[test]
    fn only_a_changed_snapshot_or_repository_set_revises_the_monitor() {
        let mut monitor = Monitor::default();
        let empty = monitor.revision();
        drop(monitor.update(Action::Tick, [repository()]));
        assert_ne!(monitor.revision(), empty);
        load(&mut monitor, Ok(result("viewer", &[51], 1)));
        let revision = monitor.revision();
        let shown = monitor.entries[&repository()].snapshot.clone();
        load(&mut monitor, Ok(result("viewer", &[51], 2)));
        assert_eq!(monitor.revision(), revision);
        assert!(Arc::ptr_eq(
            &shown,
            &monitor.entries[&repository()].snapshot
        ));
        load(&mut monitor, Ok(result("viewer", &[52], 3)));
        assert_ne!(monitor.revision(), revision);
        let revision = monitor.revision();
        load(&mut monitor, Err("cache unavailable".into()));
        assert_ne!(monitor.revision(), revision);
        assert!(
            monitor.entries[&repository()]
                .snapshot
                .requested
                .contains_key(&52)
        );
        let revision = monitor.revision();
        drop(monitor.update(Action::Tick, []));
        assert_ne!(monitor.revision(), revision);
    }

    #[test]
    fn duplicate_panes_share_a_worker_and_viewer_switch_revokes_old_authority() {
        let mut monitor = Monitor::default();
        let equivalent = GitHubRepoKey::new("github.com", None, "O", "R").unwrap();
        drop(monitor.update(Action::Tick, [repository(), equivalent]));
        assert_eq!(monitor.entries.len(), 1);
        load(&mut monitor, Ok(result("first", &[51], 1)));
        assert_eq!(monitor.take_observed()[0].viewer, "first");
        load(&mut monitor, Ok(result("second", &[], 1)));
        assert!(monitor.take_observed().is_empty());
        assert!(monitor.entries[&repository()].snapshot.requested.is_empty());
        assert_eq!(
            monitor.entries[&repository()]
                .snapshot
                .viewer
                .as_ref()
                .unwrap()
                .id,
            "second"
        );
    }

    #[test]
    fn stale_repository_reply_cannot_grant_requests_to_a_readded_binding() {
        let mut monitor = Monitor::default();
        drop(monitor.update(Action::Tick, [repository()]));
        let old = monitor.entries[&repository()].generation;
        drop(monitor.update(Action::Tick, []));
        drop(monitor.update(Action::Tick, [repository()]));
        let current = monitor.entries[&repository()].generation;
        assert_ne!(old, current);
        drop(monitor.update(
            Action::SharedLoaded {
                repository: repository(),
                generation: old,
                result: Ok(result("old", &[1], 1)),
            },
            [repository()],
        ));
        assert!(monitor.entries[&repository()].snapshot.requested.is_empty());
        assert_eq!(monitor.busy, Some((repository(), current)));
    }

    #[test]
    fn local_ticks_do_not_replay_a_cached_observation_but_a_new_read_can_offer_it_again() {
        let mut monitor = Monitor::default();
        load(&mut monitor, Ok(result("viewer", &[1], 1)));
        let observed = monitor.take_observed();
        assert_eq!(observed.len(), 1);
        drop(monitor.update(Action::Tick, [repository()]));
        assert!(monitor.take_observed().is_empty());
        load(&mut monitor, Ok(result("viewer", &[1], 1)));
        assert!(monitor.take_observed().is_empty());
        load(&mut monitor, Ok(result("viewer", &[1], 2)));
        assert_eq!(monitor.take_observed(), observed);
    }

    #[test]
    fn reads_spend_a_burst_then_one_per_pace() {
        let mut monitor = Monitor::default();
        let start = Instant::now();
        for _ in 0..BURST {
            assert!(monitor.admit(start));
        }
        assert!(!monitor.admit(start));
        assert!(!monitor.admit(start + PACE - Duration::from_millis(1)));
        assert!(monitor.admit(start + PACE));
        let idle = start + PACE * (2 * BURST);
        for _ in 0..BURST {
            assert!(monitor.admit(idle));
        }
        assert!(!monitor.admit(idle));
    }

    #[test]
    fn background_follower_delivers_two_hundred_requests_before_the_journal_expires() {
        use cosmic::iced::futures::{FutureExt, StreamExt};
        let mut monitor = Monitor::default();
        let repository = GitHubRepoKey::new("github.com", None, "owner", "repository").unwrap();
        drop(monitor.update(Action::Tick, [repository.clone()]));
        let generation = monitor.entries[&repository].generation;
        let root = tempfile::tempdir().unwrap();
        let now = crate::github::polling::now();
        // Separate publisher processes advance every page without an alert
        // consumer. The background follower arrives four minutes later, so
        // another 120-second subscription interval would lose the observations.
        let mut result = shared::tests::published_fixture(root.path(), now - 240, 200);
        result.due = now + 60;
        let mut task = monitor.update(
            Action::SharedLoaded {
                repository: repository.clone(),
                generation,
                result: Ok(result.clone()),
            },
            [repository.clone()],
        );
        let mut delivered = monitor.take_observed();
        assert_eq!(delivered.len(), 50);
        for _ in 0..3 {
            let mut stream = iced::runtime::task::into_stream(task)
                .expect("a local continuation must drain the backlog now");
            let action = stream
                .next()
                .now_or_never()
                .expect("a local notification continuation must be immediately ready")
                .unwrap();
            let iced::runtime::Action::Output(action @ Action::Tick) = action else {
                panic!("draining observations must not fetch GitHub");
            };
            task = monitor.update(action, [repository.clone()]);
            let batch = monitor.take_observed();
            assert_eq!(batch.len(), 50);
            delivered.extend(batch);
        }
        assert!(iced::runtime::task::into_stream(task).is_none());
        assert_eq!(
            delivered
                .iter()
                .map(|request| request.number)
                .collect::<std::collections::BTreeSet<_>>(),
            (1..=200).collect()
        );
        assert!(monitor.busy.is_none());
        assert_eq!(
            std::fs::read_to_string(root.path().join("calls"))
                .unwrap()
                .lines()
                .count(),
            4
        );
        monitor.make_due_for_test();
        drop(monitor.update(Action::Tick, [repository.clone()]));
        drop(monitor.update(
            Action::SharedLoaded {
                repository: repository.clone(),
                generation,
                result: Ok(result),
            },
            [repository],
        ));
        assert!(
            monitor.take_observed().is_empty(),
            "the same journal is consumed only once by each window"
        );
    }

    #[test]
    fn shared_poll_admission_is_not_blocked_by_an_old_unscoped_host_budget() {
        let mut monitor = Monitor::default();
        let repository =
            GitHubRepoKey::new("old-review-credential-budget.invalid", None, "o", "r").unwrap();
        crate::github::polling::failed(
            &repository.host,
            &crate::github::GitHubError::RateLimited {
                limit: crate::github::polling::RateLimit {
                    retry_at: crate::github::polling::now() + 120,
                },
            },
        );
        assert!(!crate::github::polling::ready(&repository.host, false));
        drop(monitor.update(Action::Tick, [repository.clone()]));
        assert_eq!(
            monitor.busy.as_ref().map(|(key, _)| key),
            Some(&repository),
            "the shared worker must resolve its own credential-scoped cooldown"
        );
    }
}
