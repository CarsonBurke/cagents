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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    fn replace_snapshot(&mut self, snapshot: Option<&Arc<Snapshot>>) -> bool {
        let unchanged = match (&self.snapshot, snapshot) {
            (None, None) => true,
            (Some(current), Some(next)) => Arc::ptr_eq(current, next) || current == next,
            _ => false,
        };
        if unchanged { return false; }
        self.snapshot = snapshot.cloned();
        true
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

    /// Install the repository's current authority without resetting unchanged
    /// state on the shell's discovery ticks.
    pub fn sync_review_requests<'a>(
        &mut self,
        snapshots: impl IntoIterator<Item = (&'a GitHubRepoKey, &'a Arc<Snapshot>)>,
    ) -> bool {
        let snapshot = matching_snapshot(self.review_request_repository(), snapshots);
        self.requests.replace_snapshot(snapshot)
    }
}

fn matching_snapshot<'a>(
    repository: Option<&GitHubRepoKey>,
    snapshots: impl IntoIterator<Item = (&'a GitHubRepoKey, &'a Arc<Snapshot>)>,
) -> Option<&'a Arc<Snapshot>> {
    let repository = repository?;
    snapshots.into_iter().find_map(|(key, snapshot)| key.same_repository(repository).then_some(snapshot))
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn unchanged_review_snapshot_preserves_the_installed_authority() {
        let original = Arc::new(Snapshot::default());
        let equal = Arc::new((*original).clone());
        let mut state = State::default();
        assert!(state.replace_snapshot(Some(&original)));
        assert!(!state.replace_snapshot(Some(&original)));
        assert!(!state.replace_snapshot(Some(&equal)));
        assert!(Arc::ptr_eq(state.snapshot.as_ref().unwrap(), &original));
        assert_eq!(Arc::strong_count(&equal), 1);
    }

    #[test]
    fn review_snapshot_changes_and_removal_are_applied_once() {
        let original = Arc::new(Snapshot::default());
        let changed = Arc::new(Snapshot {
            viewer: Some(crate::github::review_requests::Viewer { id: "viewer".into(), login: "viewer".into() }),
            error: Some("temporarily unavailable".into()),
            ..Default::default()
        });
        let mut state = State::default();
        assert!(state.replace_snapshot(Some(&original)));
        assert!(state.replace_snapshot(Some(&changed)));
        assert!(!state.replace_snapshot(Some(&changed)));
        assert_eq!(state.status(), Some("temporarily unavailable"));
        assert!(state.replace_snapshot(None));
        assert!(!state.replace_snapshot(None));
        assert_eq!(state.status(), Some("Checking review requests…"));
    }

    #[test]
    fn changed_request_history_updates_the_label_and_identity() {
        let request = crate::github::review_requests::Current {
            id: "request-1".into(), as_code_owner: false, requested_reviewer: None,
        };
        let mut original = Snapshot::default();
        original.requested.insert(7, crate::github::review_requests::scan::Requested {
            entry: crate::github::review_requests::Entry {
                number: 7, title: "Review".into(), url: "https://github.com/o/r/pull/7".parse().unwrap(),
                viewer_latest_review_request: Some(request.clone()),
            }, current: request, history: History::First,
        });
        let original = Arc::new(original);
        let mut state = State::default();
        assert!(state.replace_snapshot(Some(&original)));
        assert_eq!(state.label(7), Some("Needs your review"));
        let mut changed = (*original).clone();
        let requested = changed.requested.get_mut(&7).unwrap();
        requested.history = History::ReRequested;
        requested.current.id = "request-2".into();
        let changed = Arc::new(changed);
        assert!(state.replace_snapshot(Some(&changed)));
        assert_eq!(state.label(7), Some("Review re-requested"));
        assert_eq!(state.requested()[&7].current.id, "request-2");
        assert!(!state.replace_snapshot(Some(&Arc::new((*changed).clone()))));
    }

    #[test]
    fn review_snapshot_selection_tracks_the_exact_repository_and_absence() {
        let requested = GitHubRepoKey::new("github.com", None, "owner", "project").unwrap();
        let same = GitHubRepoKey::new("github.com", None, "OWNER", "PROJECT").unwrap();
        let other = GitHubRepoKey::new("github.com", None, "owner", "other").unwrap();
        let first = Arc::new(Snapshot::default());
        let second = Arc::new(Snapshot { error: Some("other".into()), ..Default::default() });
        let snapshots = [(&other, &second), (&same, &first)];
        assert!(Arc::ptr_eq(matching_snapshot(Some(&requested), snapshots).unwrap(), &first));
        assert!(matching_snapshot(Some(&requested), [(&other, &second)]).is_none());
        assert!(matching_snapshot(None, snapshots).is_none());
    }
}
