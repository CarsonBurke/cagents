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

#[derive(Debug, Clone)]
pub enum Action {
    Tick {
        generation: u64,
    },
    Loaded {
        generation: u64,
        work: Work,
        result: Result<Reply, String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub viewer: Option<crate::github::review_requests::Viewer>,
    pub requested: std::collections::BTreeMap<u64, crate::github::review_requests::scan::Requested>,
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct State {
    pub scan: Scan,
    external: bool,
    snapshot: Option<Arc<Snapshot>>,
    generation: u64,
    busy: bool,
    cancellation: CancellationToken,
    error: Option<String>,
}

impl State {
    pub fn reset(&mut self) {
        self.cancellation.cancel();
        self.scan = Scan::default();
        self.snapshot = None;
        self.generation = self.generation.wrapping_add(1);
        self.busy = false;
        self.cancellation = CancellationToken::new();
        self.error = None;
    }
    pub fn requested(
        &self,
    ) -> &std::collections::BTreeMap<u64, crate::github::review_requests::scan::Requested> {
        self.snapshot
            .as_ref()
            .map_or(&self.scan.requested, |snapshot| &snapshot.requested)
    }
    pub fn status(&self) -> Option<&str> {
        if let Some(snapshot) = &self.snapshot {
            return snapshot.error.as_deref().or_else(|| {
                snapshot
                    .viewer
                    .is_none()
                    .then_some("Checking review requests…")
            });
        }
        self.error.as_deref().or_else(|| {
            self.scan
                .viewer
                .is_none()
                .then_some("Checking review requests…")
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

impl Drop for State {
    fn drop(&mut self) {
        self.cancellation.cancel();
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
    /// Cagents shares one bounded scan per repository across every mounted pane.
    /// Standalone Ceditor retains the local polling owner.
    pub fn review_request_repository(&self) -> Option<&GitHubRepoKey> {
        self.source.as_ref().map(|source| &source.repository)
    }

    pub fn with_external_review_requests(mut self) -> Self {
        self.requests.external = true;
        self
    }

    pub fn clear_external_review_requests(&mut self) {
        if self.requests.external {
            self.requests.snapshot = None;
        }
    }

    pub fn apply_review_requests(&mut self, repository: &GitHubRepoKey, snapshot: &Arc<Snapshot>) {
        if self.requests.external
            && self
                .source
                .as_ref()
                .is_some_and(|source| source.repository.same_repository(repository))
        {
            self.requests.snapshot = Some(Arc::clone(snapshot));
        }
    }

    pub(super) fn poll_review_requests(&mut self) -> ActionTask {
        let Some(source) = &self.source else {
            return ActionTask::none();
        };
        if self.requests.external || self.requests.busy {
            return ActionTask::none();
        }
        self.requests.busy = true;
        let work = self.requests.scan.next();
        let repository = source.repository.clone();
        let api = self.api.clone();
        let generation = self.requests.generation;
        let cancellation = self.requests.cancellation.clone();
        ActionTask::perform(
            async move {
                let performed = work.clone();
                let result = tokio::task::spawn_blocking(move || {
                    execute(api.as_ref(), &repository, &performed, &cancellation)
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
                super::Action::ReviewRequests(Action::Loaded {
                    generation,
                    work,
                    result,
                })
            },
            |action| action,
        )
    }

    pub(super) fn update_review_requests(&mut self, action: Action) -> ActionTask {
        match action {
            Action::Tick { generation } if generation == self.requests.generation => {
                self.poll_review_requests()
            }
            Action::Loaded {
                generation,
                work,
                result,
            } if generation == self.requests.generation && self.requests.busy => {
                self.requests.busy = false;
                let result = result.and_then(|reply| match reply {
                    Reply::Page(page) => self.requests.scan.page(&work, page),
                    Reply::History(page) => self.requests.scan.history(&work, page),
                });
                if result.is_err() {
                    self.requests.scan.failed_history(&work);
                }
                self.requests.error = result.err();
                let delay = if self.requests.error.is_some() {
                    60
                } else {
                    15
                };
                ActionTask::perform(
                    async move {
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                        super::Action::ReviewRequests(Action::Tick { generation })
                    },
                    |action| action,
                )
            }
            _ => ActionTask::none(),
        }
    }
}
