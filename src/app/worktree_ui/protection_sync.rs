//! Lease preparation is asynchronous; readiness is published only by commit.
use super::*;
use std::sync::Mutex;

#[derive(Default)]
pub(in crate::app) struct State {
    desired: Vec<PathBuf>,
    applied: Option<Vec<PathBuf>>,
    inflight: Option<(u64,Vec<PathBuf>,u64)>,
    revision: u64,
    generation: u64,
    failed: Option<(Vec<PathBuf>,Instant)>,
}

impl State {
    pub(in crate::app) fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.applied = None;
        self.failed = None;
    }
}

type Prepared = Result<(crate::worktree_cleanup::WindowProtections, Option<crate::instance::PreparedLock>),String>;
#[derive(Clone)]
pub(in crate::app) struct Receipt(Arc<Mutex<Option<Prepared>>>);
impl std::fmt::Debug for Receipt {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { f.write_str("PreparedWindowProtections") }
}

impl Cagents {
    pub(in crate::app) fn prepare_window_protections(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        self.worktree_protection_sync.desired = paths.into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        let state = &mut self.worktree_protection_sync;
        if state.inflight.is_some() || state.applied.as_ref()==Some(&state.desired)
            || state.failed.as_ref().is_some_and(|(paths,until)|paths==&state.desired && Instant::now()<*until)
        { return Task::none(); }
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let paths = state.desired.clone();
        state.inflight = Some((generation,paths.clone(),state.revision));
        // Revocation performs only unlock on an already-open local descriptor.
        let snapshot = self.worktree_protections.synchronization_snapshot();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || snapshot.prepare_sync(paths))
                .await.map_err(|error|error.to_string())
                .and_then(|result| result.map_err(|error|error.to_string()));
            Message::WorktreeProtectionsPrepared {generation,receipt:Receipt(Arc::new(Mutex::new(Some(result))))}
        })
    }

    pub(in crate::app) fn window_protections_prepared(&mut self, generation: u64, receipt: Receipt) -> Task<Message> {
        let Some(result) = receipt.0.lock().unwrap().take() else {return Task::none()};
        let state = &mut self.worktree_protection_sync;
        let Some((current,paths,revision)) = &state.inflight else {return Task::none()};
        if generation != *current {return Task::none()};
        let paths = paths.clone();
        let revision = *revision;
        state.inflight = None;
        if paths != state.desired || revision != state.revision {return self.sync_worktree_protections();}
        let result = result.and_then(|(mut prepared,ready)| {
            prepared.commit_readiness(ready).map_err(|error|error.to_string())?;
            Ok(prepared)
        });
        match result {
            Ok(prepared) => {
                self.worktree_protections = prepared;
                state.applied = Some(paths);
                state.failed = None;
            }
            Err(error) => {
                state.failed = Some((paths,Instant::now()+Duration::from_secs(30)));
                tracing::debug!(%error,"automatic worktree cleanup unavailable until window scopes are protected");
            }
        }
        Task::none()
    }

    pub(in crate::app) fn protection_retry_subscription(&self) -> Subscription<Message> {
        if self.worktree_protection_sync.failed.is_some() && self.refresh_ui.focused {
            iced::time::every(Duration::from_secs(30)).map(|_|Message::RetryWorktreeProtections)
        } else {Subscription::none()}
    }
}
