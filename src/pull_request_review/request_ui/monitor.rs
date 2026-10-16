//! A shell owns one worker slot and one resumable scan per repository.
use super::*;
use crate::github::GitHubService;
use std::collections::HashMap;
use std::time::Instant;

/// How long a repository's requests may go unread between passes, each of
/// which reads every page and new request's history. Review requests change on
/// a human timescale, and every read spends the account's GraphQL budget,
/// which it shares with every other `gh` client on the machine.
const REFRESH: Duration = Duration::from_secs(60);
/// A failed read waits this long, then twice as long after each further
/// failure in a row, up to [`MAX_BACKOFF`]. A rate limit is the failure this
/// matters for: retrying it sooner only spends the budget it is waiting for.
const RETRY: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// Across every repository, one shell starts at most one read per `PACE` once
/// it has spent its `BURST`: at most 600 of GitHub's 5,000 hourly GraphQL
/// points, however many repositories are open. Past ten repositories this,
/// not [`REFRESH`], sets how often each is read. The burst lets a shell that
/// just opened discover its first repositories without waiting a pace apiece.
const PACE: Duration = Duration::from_secs(6);
const BURST: u32 = 10;

#[derive(Debug, Clone)]
pub enum Action {
    Tick,
    Loaded {
        repository: GitHubRepoKey,
        generation: u64,
        work: Work,
        result: Result<Reply, String>,
    },
}

struct Entry {
    scan: Scan,
    snapshot: Arc<Snapshot>,
    generation: u64,
    cancellation: CancellationToken,
    due: Instant,
    failures: u32,
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
    /// When the next read would be on schedule were reads started at exactly
    /// one per [`PACE`]; a read may start while this is less than a burst
    /// ahead of now (the generic cell rate algorithm).
    paced: Option<Instant>,
    /// Reads in a row, across every repository, that failed.
    failing: usize,
}

fn backoff(failures: u32) -> Duration {
    RETRY
        .saturating_mul(1 << (failures - 1).min(16))
        .min(MAX_BACKOFF)
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

    /// Only the bounded page just validated by this coordinator may report
    /// alerts. Historical snapshot rows are not replayed on every UI tick.
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
        state
            .entries
            .retain(|repository, _| repositories.iter().any(|key| key == repository));
        for repository in &repositories {
            if !state.entries.contains_key(repository) {
                state.generation = state.generation.wrapping_add(1);
                state.entries.insert(
                    repository.clone(),
                    Entry {
                        scan: Scan::default(),
                        snapshot: Arc::new(Snapshot::default()),
                        generation: state.generation,
                        cancellation: CancellationToken::new(),
                        due: Instant::now(),
                        failures: 0,
                    },
                );
            }
        }
        let failing = state.failing;
        if let Action::Loaded {
            repository,
            generation,
            work,
            result,
        } = action
        {
            if state.busy.as_ref() != Some(&(repository.clone(), generation)) {
                return iced::Task::none();
            }
            state.busy = None;
            if let Some(entry) = state
                .entries
                .get_mut(&repository)
                .filter(|entry| entry.generation == generation)
            {
                let mut observed_numbers = Vec::new();
                let applied = result.and_then(|reply| {
                    let viewer = match &reply {
                        Reply::Page(page) => &page.viewer,
                        Reply::History(page) => &page.viewer,
                    };
                    let observed_viewer = viewer.id.clone();
                    observed_numbers = match &reply {
                        Reply::Page(page) => {
                            page.entries.iter().map(|entry| entry.number).collect()
                        }
                        Reply::History(_) => match &work {
                            Work::History { number, .. } => vec![*number],
                            _ => Vec::new(),
                        },
                    };
                    let result = match reply {
                        Reply::Page(page) => entry.scan.page(&work, page),
                        Reply::History(page) => entry.scan.history(&work, page),
                    };
                    if entry
                        .scan
                        .viewer
                        .as_ref()
                        .is_none_or(|viewer| viewer.id != observed_viewer)
                    {
                        observed_numbers.clear();
                    }
                    result
                });
                if applied.is_ok()
                    && let Some(viewer) = &entry.scan.viewer
                {
                    state
                        .observed
                        .extend(observed_numbers.into_iter().filter_map(|number| {
                            entry.scan.requested.get(&number).map(|request| {
                                crate::notifications::review::ReviewRequest {
                                    host: repository.host.clone(),
                                    port: repository.port,
                                    owner: repository.owner.clone(),
                                    repository: repository.name.clone(),
                                    number,
                                    viewer: viewer.id.clone(),
                                    request: request.current.id.clone(),
                                }
                            })
                        }));
                }
                if applied.is_err() {
                    entry.scan.failed_history(&work);
                }
                entry.snapshot = Arc::new(Snapshot {
                    viewer: entry.scan.viewer.clone(),
                    requested: entry.scan.requested.clone(),
                    error: applied.err(),
                });
                // A pass that met a failure keeps backing off until it
                // settles, so one timeline that always fails cannot reset the
                // backoff through the pages read between its attempts.
                let failed = entry.snapshot.error.is_some();
                if failed {
                    entry.failures = entry.failures.saturating_add(1);
                } else if entry.scan.settled() {
                    entry.failures = 0;
                }
                entry.due = Instant::now()
                    + if failed || entry.failures > 0 {
                        backoff(entry.failures.max(1))
                    } else if entry.scan.settled() {
                        REFRESH
                    } else {
                        Duration::ZERO
                    };
                state.failing = if failed { state.failing + 1 } else { 0 };
                if !failed && failing >= state.entries.len().max(2) {
                    // Every repository failed in a row, then one read worked:
                    // the account or network is back, so none should wait
                    // out a backoff it earned while everything was down.
                    let now = Instant::now();
                    for entry in state.entries.values_mut() {
                        if entry.failures > 0 {
                            entry.failures = 0;
                            entry.due = entry.due.min(now);
                        }
                    }
                }
            }
        }
        if state.busy.is_some() {
            return iced::Task::none();
        }
        let now = Instant::now();
        let Some(repository) = state
            .entries
            .iter()
            .filter(|(repository, entry)| {
                entry.due <= now && crate::github::polling::ready(&repository.host, false)
            })
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
        let work = entry.scan.next();
        let cancellation = entry.cancellation.clone();
        state.busy = Some((repository.clone(), generation));
        iced::Task::perform(
            async move {
                let target = repository.clone();
                let requested = work.clone();
                let result = tokio::task::spawn_blocking(move || {
                    execute(
                        &GitHubService::system("gh"),
                        &target,
                        &requested,
                        &cancellation,
                    )
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
                Action::Loaded {
                    repository,
                    generation,
                    work,
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
    use crate::github::review_requests::{Current, Entry as RequestEntry, Page, Viewer};

    fn reply(viewer: &str, request: Option<&str>) -> Reply {
        Reply::Page(Page {
            viewer: Viewer {
                id: viewer.into(),
                login: viewer.into(),
            },
            entries: vec![RequestEntry {
                number: 51,
                title: "Late request".into(),
                url: "https://github.com/o/r/pull/51".parse().unwrap(),
                viewer_latest_review_request: request.map(|id| Current {
                    id: id.into(),
                    as_code_owner: false,
                    requested_reviewer: None,
                }),
            }],
            next_cursor: None,
        })
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
        assert!(!monitor.admit(start + PACE));
        // An idle shell earns its burst back, and no more than it.
        let idle = start + PACE * (2 * BURST);
        for _ in 0..BURST {
            assert!(monitor.admit(idle));
        }
        assert!(!monitor.admit(idle));
    }

    #[test]
    fn failures_back_off_to_a_ceiling_and_recovery_continues_the_pass_at_once() {
        let mut monitor = Monitor::default();
        let repository = GitHubRepoKey::new("github.com", None, "o", "r").unwrap();
        let load = |monitor: &mut Monitor, cursor: Option<&str>, result: Result<Reply, String>| {
            monitor.entries.get_mut(&repository).unwrap().due = Instant::now();
            monitor.paced = None;
            drop(monitor.update(Action::Tick, [repository.clone()]));
            let (_, generation) = monitor.busy.clone().expect("a read started");
            let work = Work::Page {
                cursor: cursor.map(str::to_owned),
            };
            let before = Instant::now();
            drop(monitor.update(
                Action::Loaded {
                    repository: repository.clone(),
                    generation,
                    work,
                    result,
                },
                [repository.clone()],
            ));
            monitor.entries[&repository]
                .due
                .saturating_duration_since(before)
        };
        drop(monitor.update(Action::Tick, [repository.clone()]));
        monitor.busy = None;
        let waits = (0..8)
            .map(|_| load(&mut monitor, None, Err("rate limited".into())))
            .collect::<Vec<_>>();
        for (wait, expected) in waits.iter().zip([1, 2, 4, 8, 16, 30, 30, 30]) {
            assert!(wait.abs_diff(Duration::from_secs(60 * expected)) < Duration::from_secs(1));
        }
        let mut more = reply("viewer", None);
        if let Reply::Page(page) = &mut more {
            page.next_cursor = Some("page2".into());
        }
        assert!(load(&mut monitor, None, Ok(more)) < Duration::from_secs(1));
        assert_eq!(monitor.entries[&repository].failures, 0);
        let refreshed = load(&mut monitor, Some("page2"), Ok(reply("viewer", None)));
        assert!(refreshed.abs_diff(REFRESH) < Duration::from_secs(1));
    }

    #[test]
    fn a_spent_budget_holds_due_repositories_until_it_refills() {
        let mut monitor = Monitor::default();
        let repository = GitHubRepoKey::new("github.com", None, "o", "r").unwrap();
        monitor.paced = Some(Instant::now() + PACE * BURST);
        drop(monitor.update(Action::Tick, [repository.clone()]));
        assert!(monitor.busy.is_none());
        monitor.paced = None;
        drop(monitor.update(Action::Tick, [repository]));
        assert!(monitor.busy.is_some());
    }

    #[test]
    fn a_timeline_that_always_fails_keeps_backing_off_between_pages() {
        let mut monitor = Monitor::default();
        let repository = GitHubRepoKey::new("github.com", None, "o", "r").unwrap();
        let current = Current {
            id: "request".into(),
            as_code_owner: false,
            requested_reviewer: Some(crate::github::review_requests::Reviewer {
                kind: "User".into(),
                id: Some("reader".into()),
            }),
        };
        let page = Reply::Page(Page {
            viewer: Viewer {
                id: "reader".into(),
                login: "reader".into(),
            },
            entries: vec![RequestEntry {
                number: 7,
                title: "Needs history".into(),
                url: "https://github.com/o/r/pull/7".parse().unwrap(),
                viewer_latest_review_request: Some(current.clone()),
            }],
            next_cursor: None,
        });
        let history = Work::History {
            number: 7,
            request: current,
            before: None,
        };
        drop(monitor.update(Action::Tick, [repository.clone()]));
        monitor.busy = None;
        let mut load = |work: Work, result: Result<Reply, String>| {
            monitor.entries.get_mut(&repository).unwrap().due = Instant::now();
            monitor.paced = None;
            drop(monitor.update(Action::Tick, [repository.clone()]));
            let (_, generation) = monitor.busy.clone().expect("a read started");
            let before = Instant::now();
            drop(monitor.update(
                Action::Loaded {
                    repository: repository.clone(),
                    generation,
                    work,
                    result,
                },
                [repository.clone()],
            ));
            let wait = monitor.entries[&repository]
                .due
                .saturating_duration_since(before);
            (wait.as_secs_f64() / 60.0).round() as u64
        };
        let first = Work::Page { cursor: None };
        assert_eq!(load(first.clone(), Ok(page.clone())), 0);
        let waits = [1, 2, 4]
            .iter()
            .flat_map(|_| {
                [
                    load(history.clone(), Err("timeline unavailable".into())),
                    load(first.clone(), Ok(page.clone())),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(waits, [1, 1, 2, 2, 4, 4]);
    }

    #[test]
    fn a_read_that_ends_an_outage_clears_every_backoff() {
        let mut monitor = Monitor::default();
        let repositories =
            ["a", "b", "c"].map(|name| GitHubRepoKey::new("github.com", None, "o", name).unwrap());
        drop(monitor.update(Action::Tick, repositories.clone()));
        monitor.busy = None;
        let load = |monitor: &mut Monitor, index: usize, result: Result<Reply, String>| {
            for entry in monitor.entries.values_mut() {
                entry.due = Instant::now() + Duration::from_secs(3600);
            }
            monitor.entries.get_mut(&repositories[index]).unwrap().due = Instant::now();
            monitor.paced = None;
            drop(monitor.update(Action::Tick, repositories.clone()));
            let (repository, generation) = monitor.busy.clone().expect("a read started");
            drop(monitor.update(
                Action::Loaded {
                    repository,
                    generation,
                    work: Work::Page { cursor: None },
                    result,
                },
                repositories.clone(),
            ));
        };
        for round in 0..3 {
            for index in 0..3 {
                load(&mut monitor, index, Err(format!("offline {round}")));
            }
        }
        assert!(monitor.entries.values().all(|entry| entry.failures == 3));
        load(&mut monitor, 0, Ok(reply("viewer", None)));
        let now = Instant::now();
        assert!(
            monitor
                .entries
                .values()
                .all(|entry| entry.failures == 0 && entry.due <= now + REFRESH)
        );
    }

    #[test]
    fn duplicate_panes_share_worker_and_viewer_switch_revokes_prior_authority() {
        let mut monitor = Monitor::default();
        let repository = GitHubRepoKey::new("github.com", None, "o", "r").unwrap();
        let equivalent = GitHubRepoKey::new("github.com", None, "O", "R").unwrap();
        drop(monitor.update(Action::Tick, [repository.clone(), equivalent]));
        assert_eq!(monitor.entries.len(), 1);
        let generation = monitor.entries[&repository].generation;
        drop(monitor.update(
            Action::Loaded {
                repository: repository.clone(),
                generation,
                work: Work::Page { cursor: None },
                result: Ok(reply("first", Some("request51"))),
            },
            [repository.clone()],
        ));
        assert!(
            monitor.entries[&repository]
                .snapshot
                .requested
                .contains_key(&51)
        );
        monitor.entries.get_mut(&repository).unwrap().due = Instant::now();
        drop(monitor.update(Action::Tick, [repository.clone()]));
        drop(monitor.update(
            Action::Loaded {
                repository: repository.clone(),
                generation,
                work: Work::Page { cursor: None },
                result: Ok(reply("second", None)),
            },
            [repository.clone()],
        ));
        assert!(monitor.entries[&repository].snapshot.requested.is_empty());
        assert_eq!(
            monitor.entries[&repository]
                .snapshot
                .viewer
                .as_ref()
                .unwrap()
                .id,
            "second"
        );
    }
    #[test]
    fn a_failed_delivery_is_offered_again_only_after_fresh_validated_observation() {
        let mut monitor = Monitor::default();
        let repository = GitHubRepoKey::new("github.com", None, "o", "r").unwrap();
        drop(monitor.update(Action::Tick, [repository.clone()]));
        let generation = monitor.entries[&repository].generation;
        let loaded = || Action::Loaded {
            repository: repository.clone(),
            generation,
            work: Work::Page { cursor: None },
            result: Ok(reply("viewer", Some("request51"))),
        };
        drop(monitor.update(loaded(), [repository.clone()]));
        let lost = monitor.take_observed();
        assert_eq!(lost.len(), 1);
        // The report RPC failed after this page; an ordinary UI tick must not
        // replay stale account authority, nor permanently mark the ID delivered.
        drop(monitor.update(Action::Tick, [repository.clone()]));
        assert!(monitor.take_observed().is_empty());
        monitor.entries.get_mut(&repository).unwrap().due = Instant::now();
        drop(monitor.update(Action::Tick, [repository.clone()]));
        drop(monitor.update(loaded(), [repository.clone()]));
        assert_eq!(monitor.take_observed(), lost);
        monitor.entries.get_mut(&repository).unwrap().due = Instant::now();
        drop(monitor.update(Action::Tick, [repository.clone()]));
        drop(monitor.update(
            Action::Loaded {
                repository: repository.clone(),
                generation,
                work: Work::Page { cursor: None },
                result: Ok(reply("changed-viewer", None)),
            },
            [repository],
        ));
        assert!(monitor.take_observed().is_empty());
    }

    #[test]
    fn stale_repository_reply_cannot_grant_requests_to_readded_binding() {
        let mut monitor = Monitor::default();
        let repository = GitHubRepoKey::new("github.com", None, "o", "r").unwrap();
        drop(monitor.update(Action::Tick, [repository.clone()]));
        let old = monitor.entries[&repository].generation;
        drop(monitor.update(Action::Tick, []));
        drop(monitor.update(Action::Tick, [repository.clone()]));
        let current = monitor.entries[&repository].generation;
        assert_ne!(old, current);
        drop(monitor.update(
            Action::Loaded {
                repository: repository.clone(),
                generation: old,
                work: Work::Page { cursor: None },
                result: Ok(reply("stale", Some("request51"))),
            },
            [repository.clone()],
        ));
        assert!(monitor.entries[&repository].snapshot.requested.is_empty());
        assert_eq!(monitor.busy, Some((repository, current)));
    }
}
