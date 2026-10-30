//! Both shells consume the same per-viewer discovery and history authority.
pub mod monitor;
use super::*;
use crate::github::review_requests::{
    HistoryPage, Page,
    scan::{History, Scan, Work},
};

#[derive(Debug, Clone)]
pub enum Reply {
    Page(Page),
    History(HistoryPage),
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub viewer: Option<crate::github::review_requests::Viewer>,
    pub requested: std::collections::BTreeMap<u64, crate::github::review_requests::scan::Requested>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct State {
    snapshot: Option<Arc<Snapshot>>,
}

impl State {
    pub fn reset(&mut self) {
        self.snapshot = None;
    }
    pub fn requested(
        &self,
    ) -> &std::collections::BTreeMap<u64, crate::github::review_requests::scan::Requested> {
        static EMPTY: std::sync::LazyLock<
            std::collections::BTreeMap<u64, crate::github::review_requests::scan::Requested>,
        > = std::sync::LazyLock::new(Default::default);
        self.snapshot
            .as_ref()
            .map_or(&EMPTY, |snapshot| &snapshot.requested)
    }
    pub fn status(&self) -> Option<&str> {
        self.snapshot
            .as_ref()
            .map_or(Some("Checking review requests…"), |snapshot| {
                snapshot.error.as_deref().or_else(|| {
                    snapshot
                        .viewer
                        .is_none()
                        .then_some("Checking review requests…")
                })
            })
    }
    pub fn label(&self, number: u64) -> Option<&'static str> {
        self.requested().get(&number).map(|request| {
            if request.history == History::ReRequested {
                "Review re-requested"
            } else {
                "Needs your review"
            }
        })
    }
}

pub(crate) fn execute(
    api: &dyn GitHubReviewApi,
    repository: &GitHubRepoKey,
    work: &Work,
    cancellation: &CancellationToken,
) -> Result<Reply, String> {
    match work {
        Work::Page { cursor } => api
            .review_request_page(repository, cursor.as_deref(), cancellation)
            .map(Reply::Page),
        Work::History { number, before, .. } => api
            .review_request_history_page(
                &PullRequestKey {
                    repository: repository.clone(),
                    number: *number,
                },
                before.as_deref(),
                cancellation,
            )
            .map(Reply::History),
    }
    .map_err(|error| {
        crate::github::polling::failed(&repository.host, &error);
        error.to_string()
    })
}

impl super::State {
    pub fn review_request_repository(&self) -> Option<&GitHubRepoKey> {
        self.source.as_ref().map(|source| &source.repository)
    }

    /// Shows the monitor's snapshot for this pane's repository. A pane whose
    /// source just changed has none until the next pass installs it, so this
    /// runs on every pass, and it replaces nothing that is already current.
    pub fn sync_review_requests<'a>(
        &mut self,
        snapshots: impl IntoIterator<Item = (&'a GitHubRepoKey, &'a Arc<Snapshot>)>,
    ) {
        let snapshot = self.source.as_ref().and_then(|source| {
            snapshots.into_iter().find_map(|(key, snapshot)| {
                key.same_repository(&source.repository).then_some(snapshot)
            })
        });
        let current = match (&self.requests.snapshot, snapshot) {
            (Some(shown), Some(snapshot)) => Arc::ptr_eq(shown, snapshot),
            (shown, snapshot) => shown.is_none() && snapshot.is_none(),
        };
        if !current {
            self.requests.snapshot = snapshot.cloned();
        }
    }
}
