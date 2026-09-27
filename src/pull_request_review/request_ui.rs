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
    .map_err(|error| error.to_string())
}

impl super::State {
    pub fn review_request_repository(&self) -> Option<&GitHubRepoKey> {
        self.source.as_ref().map(|source| &source.repository)
    }

    pub fn clear_external_review_requests(&mut self) {
        self.requests.reset();
    }

    pub fn apply_review_requests(&mut self, repository: &GitHubRepoKey, snapshot: &Arc<Snapshot>) {
        if self
            .source
            .as_ref()
            .is_some_and(|source| source.repository.same_repository(repository))
        {
            self.requests.snapshot = Some(Arc::clone(snapshot));
        }
    }
}
