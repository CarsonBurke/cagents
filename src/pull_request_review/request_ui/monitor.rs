//! A shell owns one worker slot and one resumable scan per repository.
use super::*;
use crate::github::GitHubService;
use std::collections::HashMap;
use std::time::Instant;

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
}

impl Monitor {
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
                    },
                );
            }
        }
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
                let applied = result.and_then(|reply| match reply {
                    Reply::Page(page) => entry.scan.page(&work, page),
                    Reply::History(page) => entry.scan.history(&work, page),
                });
                if applied.is_err() {
                    entry.scan.failed_history(&work);
                }
                entry.snapshot = Arc::new(Snapshot {
                    viewer: entry.scan.viewer.clone(),
                    requested: entry.scan.requested.clone(),
                    error: applied.err(),
                });
                entry.due = Instant::now()
                    + Duration::from_secs(if entry.snapshot.error.is_some() {
                        60
                    } else {
                        10
                    });
            }
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
