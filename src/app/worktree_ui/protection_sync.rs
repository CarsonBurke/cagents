//! Lease preparation is asynchronous; readiness is published only by commit.
use super::*;
use std::sync::Mutex;

#[derive(Default)]
pub(in crate::app) struct State {
    desired: Vec<PathBuf>,
    applied: Option<Vec<PathBuf>>,
    inflight: Option<(u64, Vec<PathBuf>, u64)>,
    revision: u64,
    generation: u64,
    failed: Option<(Vec<PathBuf>, Instant)>,
    last_failure: Option<String>,
    accepted: Option<(Vec<PathBuf>, u64)>,
    releases: Vec<Release>,
}

impl State {
    pub(in crate::app) fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.applied = None;
        self.accepted = None;
        self.failed = None;
    }
}

struct Release {
    paths: Vec<PathBuf>,
    complete: Box<dyn FnOnce(Result<(), String>) -> Message + Send>,
}

type Prepared = Result<
    (
        crate::worktree_cleanup::WindowProtections,
        Option<crate::instance::PreparedLock>,
    ),
    String,
>;
#[derive(Clone)]
pub(in crate::app) struct Receipt(Arc<Mutex<Option<Prepared>>>);
impl std::fmt::Debug for Receipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedWindowProtections")
    }
}

impl Cagents {
    /// Execute dependent removal only after the accepted replacement has
    /// retired every older lease snapshot and all selection receipts. The
    /// consumer must fence its own deletion identity before issuing removal.
    pub(in crate::app) fn sync_worktree_protections_then_released(
        &mut self,
        path: PathBuf,
        complete: impl FnOnce(Result<(), String>) -> Message + Send + 'static,
    ) -> Task<Message> {
        let mut paths = vec![path.clone()];
        if let Some(physical) = self.worktree_owner_paths.cached.get(&path)
            && physical != &path
        {
            paths.push(physical.clone());
        }
        self.worktree_protection_sync.releases.push(Release {
            paths,
            complete: Box::new(complete),
        });
        let sync = self.sync_worktree_protections();
        let finished = self.finish_worktree_protection_releases(None);
        Task::batch([sync, finished])
    }

    pub(super) fn finish_worktree_protection_releases(
        &mut self,
        failure: Option<String>,
    ) -> Task<Message> {
        let state = &mut self.worktree_protection_sync;
        if state.releases.is_empty() {
            return Task::none();
        }
        let failure = failure.or_else(|| {
            (state.inflight.is_none()
                && state
                    .failed
                    .as_ref()
                    .is_some_and(|(paths, _)| paths == &state.desired))
            .then(|| state.last_failure.clone())
            .flatten()
        });
        let accepted = state.inflight.is_none()
            && state.accepted.as_ref().is_some_and(|(paths, revision)| {
                paths == &state.desired && *revision == state.revision
            });
        let mut pending = Vec::new();
        let mut completed = Vec::new();
        for release in std::mem::take(&mut state.releases) {
            let desired = state.desired.iter().any(|path| {
                let physical = self.worktree_owner_paths.cached.get(path).unwrap_or(path);
                release
                    .paths
                    .iter()
                    .any(|excluded| path.starts_with(excluded) || physical.starts_with(excluded))
            });
            let result = if let Some(failure) = &failure {
                Some(Err(failure.clone()))
            } else if desired {
                Some(Err(
                    "Worktree remains selected or open in this window".into()
                ))
            } else if accepted && self.worktree_selection.inflight.is_empty() {
                Some(
                    if release
                        .paths
                        .iter()
                        .any(|path| self.worktree_protections.holds_within(path))
                    {
                        Err("Worktree directory protection is still held".into())
                    } else {
                        Ok(())
                    },
                )
            } else {
                None
            };
            if let Some(result) = result {
                completed.push((release.complete)(result));
            } else {
                pending.push(release);
            }
        }
        state.releases = pending;
        Task::batch(completed.into_iter().map(|message| self.update(message)))
    }

    pub(in crate::app) fn prepare_window_protections(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> Task<Message> {
        let desired = paths
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if desired != self.worktree_protection_sync.desired {
            self.worktree_protection_sync.last_failure = None;
        }
        self.worktree_protection_sync.desired = desired;
        let state = &mut self.worktree_protection_sync;
        if state.inflight.is_some()
            || state.applied.as_ref() == Some(&state.desired)
            || state
                .failed
                .as_ref()
                .is_some_and(|(paths, until)| paths == &state.desired && Instant::now() < *until)
        {
            return Task::none();
        }
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let paths = state.desired.clone();
        state.inflight = Some((generation, paths.clone(), state.revision));
        // Revocation performs only unlock on an already-open local descriptor.
        let snapshot = self.worktree_protections.synchronization_snapshot();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || snapshot.prepare_sync(paths))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            Message::WorktreeProtectionsPrepared {
                generation,
                receipt: Receipt(Arc::new(Mutex::new(Some(result)))),
            }
        })
    }

    pub(in crate::app) fn window_protections_prepared(
        &mut self,
        generation: u64,
        receipt: Receipt,
    ) -> Task<Message> {
        let Some(result) = receipt.0.lock().unwrap().take() else {
            return Task::none();
        };
        let state = &mut self.worktree_protection_sync;
        let Some((current, paths, revision)) = &state.inflight else {
            return Task::none();
        };
        if generation != *current {
            return Task::none();
        };
        let paths = paths.clone();
        let revision = *revision;
        state.inflight = None;
        if paths != state.desired || revision != state.revision {
            return self.sync_worktree_protections();
        }
        let result = result.and_then(|(mut prepared, ready)| {
            prepared
                .commit_readiness(ready)
                .map_err(|error| error.to_string())?;
            Ok(prepared)
        });
        match result {
            Ok(prepared) => {
                self.worktree_protections = prepared;
                state.accepted = Some((paths.clone(), state.revision));
                if self.worktree_protections.needs_retry() {
                    state.applied = None;
                    state.failed = Some((paths, Instant::now() + Duration::from_secs(30)));
                } else {
                    state.applied = Some(paths);
                    state.failed = None;
                }
                state.last_failure = None;
            }
            Err(error) => {
                state.accepted = None;
                let failure = error.clone();
                state.failed = Some((paths, Instant::now() + Duration::from_secs(30)));
                if state.last_failure.as_ref() != Some(&error) {
                    tracing::debug!(%error,"automatic worktree cleanup unavailable until window scopes are protected");
                    state.last_failure = Some(error);
                }
                return self.finish_worktree_protection_releases(Some(failure));
            }
        }
        self.finish_worktree_protection_releases(None)
    }

    pub(in crate::app) fn protection_retry_subscription(&self) -> Subscription<Message> {
        if self.worktree_protection_sync.failed.is_some() && self.refresh_ui.focused {
            iced::time::every(Duration::from_secs(30)).map(|_| Message::RetryWorktreeProtections)
        } else {
            Subscription::none()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_window() -> Cagents {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.active_workspace = None;
        app.durable_mut().cards.clear();
        app.cards.clear();
        app
    }

    #[test]
    fn camera_update_drives_the_protection_preparation_it_schedules() {
        use cosmic::Application as _;
        use cosmic::iced::futures::StreamExt as _;
        let mut app = empty_window();
        let width = app.workspace.viewport_width();
        let task = app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(width),
        });
        assert!(app.worktree_protection_sync.inflight.is_some());
        let mut stream = iced::runtime::task::into_stream(task).expect("camera update has tasks");
        let (generation, receipt) = tokio::runtime::Builder::new_current_thread()
            .build().unwrap().block_on(async move {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        Message::WorktreeProtectionsPrepared { generation, receipt },
                    )) = action {
                        return (generation, receipt);
                    }
                    // Viewport updates also emit a scroll widget operation.
                }
                panic!("camera update did not return its protection task")
            });
        drop(app.window_protections_prepared(generation, receipt));
        assert!(app.worktree_protection_sync.inflight.is_none());
        assert_eq!(app.worktree_protection_sync.applied, Some(Vec::new()));
    }

    #[test]
    fn release_continuation_waits_for_real_lease_retirement_and_selection_receipts() {
        let runtime = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut app = empty_window();
        app.worktree_protections.install_prepared(Some(
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), false)
                .unwrap(),
        ));
        let mut replacement = app.worktree_protections.synchronization_snapshot();
        replacement.fixture_release_all();
        app.worktree_protection_sync.inflight = Some((1, Vec::new(), 0));
        app.worktree_selection.inflight.insert(7);
        let (completed, received) = crossbeam_channel::bounded(1);
        drop(app.sync_worktree_protections_then_released(
            directory.path().to_owned(),
            move |result| {
                completed.send(result).unwrap();
                Message::SettingsSearchChanged("protection released".into())
            },
        ));
        assert!(received.try_recv().is_err());
        assert!(
            matches!(crate::worktree_cleanup::fixture_protection(runtime.path(),directory.path(),true),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
        let receipt = Receipt(Arc::new(Mutex::new(Some(Ok((replacement, None))))));
        drop(app.window_protections_prepared(1, receipt.clone()));
        assert!(
            received.try_recv().is_err(),
            "obsolete selection could still own a receipt lease"
        );
        crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true)
            .unwrap();
        app.worktree_selection.inflight.remove(&7);
        drop(app.finish_worktree_protection_releases(None));
        assert_eq!(received.recv().unwrap(), Ok(()));
        assert_eq!(app.settings_search, "protection released");
        drop(app.window_protections_prepared(1, receipt));
        drop(app.finish_worktree_protection_releases(None));
        assert!(
            received.try_recv().is_err(),
            "release continuation ran twice"
        );
    }

    #[test]
    fn stale_replacement_cannot_run_release_continuation() {
        let mut app = empty_window();
        app.worktree_protection_sync.inflight = Some((1, Vec::new(), 0));
        let (completed, received) = crossbeam_channel::bounded(1);
        drop(
            app.sync_worktree_protections_then_released("/obsolete".into(), move |result| {
                completed.send(result).unwrap();
                Message::SettingsSearchChanged("settled".into())
            }),
        );
        app.worktree_protection_sync.invalidate();
        let receipt = Receipt(Arc::new(Mutex::new(Some(Ok((Default::default(), None))))));
        drop(app.window_protections_prepared(1, receipt));
        assert!(received.try_recv().is_err());
        assert_eq!(app.worktree_protection_sync.releases.len(), 1);
    }

    #[test]
    fn preparation_failure_rejects_release_once_instead_of_waiting_for_a_retry() {
        let mut app = empty_window();
        app.worktree_protection_sync.inflight = Some((1, Vec::new(), 0));
        let (completed, received) = crossbeam_channel::bounded(1);
        drop(
            app.sync_worktree_protections_then_released("/target".into(), move |result| {
                completed.send(result).unwrap();
                Message::SettingsSearchChanged("refused".into())
            }),
        );
        let receipt = Receipt(Arc::new(Mutex::new(Some(Err(
            "fixture protection failure".into(),
        )))));
        drop(app.window_protections_prepared(1, receipt));
        assert_eq!(
            received.recv().unwrap(),
            Err("fixture protection failure".into())
        );
        assert!(app.worktree_protection_sync.releases.is_empty());
        drop(app.finish_worktree_protection_releases(None));
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn a_still_desired_scope_refuses_release_even_without_an_inflight_batch() {
        let mut app = empty_window();
        app.active_workspace = Some("/target/child".into());
        let (completed, received) = crossbeam_channel::bounded(1);
        drop(
            app.sync_worktree_protections_then_released("/target".into(), move |result| {
                completed.send(result).unwrap();
                Message::SettingsSearchChanged("still selected".into())
            }),
        );
        assert!(received.recv().unwrap().unwrap_err().contains("selected"));
        assert_eq!(
            app.active_workspace.as_deref(),
            Some(Path::new("/target/child"))
        );
    }
}
