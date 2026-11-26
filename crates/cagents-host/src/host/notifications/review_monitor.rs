//! Negative review evidence needs a complete, exact GitHub census.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use uuid::Uuid;

use crate::{
    github_identity::GitHubRepoKey,
    notifications::{
        Event, Notice, Target,
        review::{MonitorUpdate, ScanPoint},
    },
};

use super::Journal;

const WATCH_GRACE: i64 = 7 * 24 * 60 * 60;
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const OBSERVATION_GRACE: i64 = 5 * 60;

#[derive(Default)]
pub(super) struct ReviewMonitor {
    sweep_after: Option<Instant>,
}

fn repository_key(repository: &GitHubRepoKey) -> String {
    repository.gh_selector().to_ascii_lowercase()
}

impl ReviewMonitor {
    pub fn apply(
        &mut self,
        update: MonitorUpdate,
        journal: &mut Journal,
        notices: &[Notice],
        seconds: i64,
    ) -> Result<Vec<Uuid>, String> {
        match update {
            MonitorUpdate::Observations(_) => {
                Err("Unexpected review observations in a census batch".into())
            }
            MonitorUpdate::Watching(repositories) => {
                for repository in repositories {
                    journal.remember_review_watch(&repository_key(&repository), seconds)?;
                }
                Ok(Vec::new())
            }
            MonitorUpdate::Completed(census) => {
                let key = repository_key(&census.repository);
                if let Some(previous) = journal.review_census_stamp(&key, &census.viewer)?
                    && (!previous.precedes_census(&census.start)
                        || census.start.precedes_census(&previous))
                {
                    return Ok(Vec::new());
                }
                let points = journal.review_points()?;
                let eligible = notices
                    .iter()
                    .filter_map(|notice| match &notice.event.target {
                        Target::PullRequest(request)
                            if !notice.acknowledged
                                && census.viewer == request.viewer
                                && census.repository.same_repository(&request.repository_key())
                                && census.current.get(&request.number)
                                    != Some(&request.request)
                                && points
                                    .get(&notice.id)
                                    .is_none_or(|point| point.precedes_census(&census.start)) =>
                        {
                            Some(notice.id)
                        }
                        _ => None,
                    })
                    .collect();
                journal.commit_review_reconciliation(Some((key.as_str(), &census)), eligible)
            }
        }
    }

    pub fn record(
        journal: &mut Journal,
        event: Event,
        point: Option<ScanPoint>,
        suppressed: bool,
        seconds: i64,
    ) -> Result<Option<Notice>, String> {
        let Target::PullRequest(request) = &event.target else {
            return Err("Review observation has no review request".into());
        };
        if let Some(point) = &point
            && i64::try_from(point.at)
                .map_or(true, |at| seconds.saturating_sub(at) >= OBSERVATION_GRACE)
        {
            return Ok(None);
        }
        let repository = repository_key(&request.repository_key());
        // Older clients report no scan point. Their accepted positives
        // participate in FIFO order, and a previously accepted census cannot
        // tell whether an unstamped observation represents a new request.
        if let Some(point) = &point {
            let is_current = match journal.review_census(&repository, &request.viewer)? {
                Some(census) => {
                    !point.precedes_census(&census.start)
                        || census.current.get(&request.number) == Some(&request.request)
                }
                // The ordering fence is still authoritative, but its evicted
                // payload can no longer prove that an older request was current.
                None => journal
                    .review_census_stamp(&repository, &request.viewer)?
                    .is_none_or(|stamp| !point.precedes_census(&stamp)),
            };
            if !is_current {
                return Ok(None);
            }
        }
        journal.remember_review_watch(&repository, seconds)?;
        match point {
            Some(point) => journal.record_review(event, &point, suppressed),
            None => journal.record(event, suppressed),
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.sweep_after
    }

    pub fn expire(
        &mut self,
        journal: &mut Journal,
        notices: &[Notice],
        now: Instant,
        seconds: i64,
    ) -> Result<Vec<Uuid>, String> {
        if self.sweep_after.is_some_and(|due| now < due) {
            return Ok(Vec::new());
        }
        self.sweep_after = Some(now + SWEEP_INTERVAL);
        let mut expired = Vec::new();
        let mut checked = HashMap::new();
        for notice in notices.iter().filter(|notice| !notice.acknowledged) {
            if let Target::PullRequest(request) = &notice.event.target {
                let key = repository_key(&request.repository_key());
                let expired_repository = match checked.get(&key) {
                    Some(&expired) => expired,
                    None => {
                        let expired = match journal.last_review_watch(&key)? {
                            Some(watched) => seconds.saturating_sub(watched) >= WATCH_GRACE,
                            None => {
                                // A notice written by an older daemon has no
                                // watch history. Give it the full grace period.
                                journal.remember_review_watch(&key, seconds)?;
                                false
                            }
                        };
                        checked.insert(key, expired);
                        expired
                    }
                };
                if expired_repository {
                    expired.push(notice.id);
                }
            }
        }
        self.sweep_after = (!checked.is_empty()).then_some(now + SWEEP_INTERVAL);
        Ok(expired)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::notifications::{
        Application, Destination,
        review::{Census, ObservedRequest, ReviewRequest, StampedRequests},
    };
    use std::collections::BTreeMap;

    pub(in crate::host::notifications) fn point(revision: u64, seconds: u64) -> ScanPoint {
        ScanPoint {
            incarnation: "de2c0839-573a-4ee7-9050-6f63372d9f0b".into(),
            revision,
            at: seconds,
        }
    }

    pub(in crate::host::notifications) fn observed(
        repository: &GitHubRepoKey,
        viewer: &str,
        request: &str,
        number: u64,
        point: ScanPoint,
    ) -> StampedRequests {
        StampedRequests {
            destination: Destination {
                application: Application::Cagents,
                window: "primary".into(),
            },
            requests: vec![ObservedRequest {
                request: ReviewRequest {
                    host: repository.host.clone(),
                    port: repository.port,
                    owner: repository.owner.clone(),
                    repository: repository.name.clone(),
                    number,
                    viewer: viewer.into(),
                    request: request.into(),
                },
                point,
            }],
        }
    }

    fn request(
        journal: &mut Journal,
        repository: &GitHubRepoKey,
        viewer: &str,
        request: &str,
        number: u64,
        at: ScanPoint,
    ) -> Option<Notice> {
        let (event, point) = observed(repository, viewer, request, number, at)
            .events()
            .next()
            .unwrap();
        let seconds = point.at as i64;
        ReviewMonitor::record(journal, event, Some(point), false, seconds).unwrap()
    }

    pub(in crate::host::notifications) fn repo(port: Option<u16>) -> GitHubRepoKey {
        GitHubRepoKey::new("review.example.com", port, "owner", "code").unwrap()
    }

    #[test]
    fn only_pre_census_observations_for_the_exact_repository_and_viewer_are_reconciled() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let repository = repo(None);
        let start = point(5, 100);
        let prior = point(4, 100); // The accepted cache order resolves the shared clock tick.
        let stale = request(
            &mut journal,
            &repository,
            "alice",
            "removed",
            10,
            prior.clone(),
        )
        .unwrap();
        request(
            &mut journal,
            &repository,
            "bob",
            "other_viewer",
            10,
            prior.clone(),
        )
        .unwrap();
        request(
            &mut journal,
            &repo(Some(8443)),
            "alice",
            "other_port",
            10,
            prior.clone(),
        )
        .unwrap();
        request(
            &mut journal,
            &repository,
            "alice",
            "still_open",
            11,
            prior.clone(),
        )
        .unwrap();
        request(
            &mut journal,
            &repository,
            "alice",
            "later_request",
            10,
            point(6, 100),
        )
        .unwrap();
        let notices = journal.list().unwrap();
        let mut monitor = ReviewMonitor::default();
        let complete = MonitorUpdate::Completed(Census {
            repository: repository.clone(),
            start,
            viewer: "alice".into(),
            current: BTreeMap::from([(11, "still_open".into()), (12, "confirmed".into())]),
        });
        assert_eq!(
            monitor
                .apply(complete, &mut journal, &notices, 101)
                .unwrap(),
            [stale.id]
        );
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(
            request(
                &mut journal,
                &repository,
                "alice",
                "delayed",
                12,
                prior.clone()
            )
            .is_none()
        );
        assert!(request(&mut journal, &repository, "alice", "confirmed", 12, prior).is_some());
    }

    #[test]
    fn a_failed_census_write_does_not_strand_a_legacy_notice_on_replay() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let repository = repo(None);
        let point = point(4, 100);
        let census = MonitorUpdate::Completed(Census {
            repository: repository.clone(),
            start: point.clone(),
            viewer: "alice".into(),
            current: BTreeMap::new(),
        });
        let legacy_event = |key, number| {
            observed(&repository, "alice", key, number, point.clone())
                .events()
                .next()
                .unwrap()
                .0
        };
        let mut journal = Journal::open(&path).unwrap();
        let original =
            ReviewMonitor::record(&mut journal, legacy_event("legacy", 10), None, false, 101)
                .unwrap()
                .unwrap();
        let fixture = rusqlite::Connection::open(&path).unwrap();
        fixture
            .execute_batch(
                "CREATE TRIGGER fail_acknowledgement BEFORE UPDATE OF acknowledged ON notices
                 BEGIN SELECT RAISE(ABORT, 'fixture rejected acknowledgement'); END;",
            )
            .unwrap();
        let mut monitor = ReviewMonitor::default();
        let error = monitor
            .apply(census.clone(), &mut journal, &[original.clone()], 101)
            .unwrap_err();
        assert!(error.contains("fixture rejected acknowledgement"));
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(
            journal
                .review_census(&repository_key(&repository), "alice")
                .unwrap()
                .is_none()
        );
        assert!(
            journal
                .review_census_stamp(&repository_key(&repository), "alice")
                .unwrap()
                .is_none()
        );
        assert_eq!(journal.list().unwrap(), [original.clone()]);
        assert!(journal.group_withdrawals().unwrap().is_empty());

        fixture
            .execute_batch("DROP TRIGGER fail_acknowledgement")
            .unwrap();
        assert_eq!(
            monitor
                .apply(census.clone(), &mut journal, &[original.clone()], 101)
                .unwrap(),
            [original.id]
        );
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(
            journal
                .review_census_stamp(&repository_key(&repository), "alice")
                .unwrap(),
            Some(point.clone())
        );
        assert!(journal.list().unwrap()[0].acknowledged);
        assert!(journal.group_withdrawals().unwrap().contains(&original.id));
        let later =
            ReviewMonitor::record(&mut journal, legacy_event("later", 11), None, false, 102)
                .unwrap()
                .unwrap();
        assert!(
            monitor
                .apply(census, &mut journal, &[later.clone()], 102)
                .unwrap()
                .is_empty()
        );
        assert!(journal.list().unwrap().contains(&later));
    }

    #[test]
    fn an_evicted_census_cannot_reconcile_new_legacy_notices_on_replay() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let repository = repo(None);
        let key = repository_key(&repository);
        let initial = Census {
            repository: repository.clone(),
            start: point(5, 100),
            viewer: "alice".into(),
            current: BTreeMap::from([(11, "still_open".into())]),
        };
        let mut monitor = ReviewMonitor::default();
        monitor
            .apply(
                MonitorUpdate::Completed(initial.clone()),
                &mut journal,
                &[],
                100,
            )
            .unwrap();
        let (legacy, _) = observed(&repository, "alice", "legacy_after", 12, point(5, 100))
            .events()
            .next()
            .unwrap();
        let legacy = ReviewMonitor::record(&mut journal, legacy, None, false, 100)
            .unwrap()
            .unwrap();

        // Fill the bounded membership cache once. Every row is a separate
        // viewer of the same repository, so the original exact scope ages out.
        for viewer in 0..256 {
            let census = Census {
                viewer: format!("other_{viewer}"),
                ..initial.clone()
            };
            journal
                .commit_review_reconciliation(Some((&key, &census)), Vec::new())
                .unwrap();
        }
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert!(journal.review_census(&key, "alice").unwrap().is_none());
        assert_eq!(
            journal.review_census_stamp(&key, "alice").unwrap(),
            Some(initial.start.clone())
        );
        assert!(
            monitor
                .apply(
                    MonitorUpdate::Completed(initial.clone()),
                    &mut journal,
                    &[legacy.clone()],
                    101
                )
                .unwrap()
                .is_empty()
        );
        assert!(journal.list().unwrap().contains(&legacy));
        assert!(journal.group_withdrawals().unwrap().is_empty());

        // Without the old payload, no earlier positive can prove membership.
        // Deferral must not burn the request identity in the seen ledger.
        assert!(
            request(
                &mut journal,
                &repository,
                "alice",
                "removed",
                10,
                point(4, 100)
            )
            .is_none()
        );
        assert!(
            request(
                &mut journal,
                &repository,
                "alice",
                "still_open",
                11,
                point(5, 100)
            )
            .is_none()
        );
        let fresh = request(
            &mut journal,
            &repository,
            "alice",
            "still_open",
            11,
            point(6, 100),
        )
        .unwrap();
        let latest = Census {
            start: point(7, 100),
            ..initial
        };
        let visible = journal.list().unwrap();
        assert_eq!(
            monitor
                .apply(
                    MonitorUpdate::Completed(latest),
                    &mut journal,
                    &visible,
                    101
                )
                .unwrap(),
            [legacy.id]
        );
        assert!(journal.list().unwrap().contains(&fresh));
    }

    #[test]
    fn a_fresh_observation_renews_a_stale_watch_before_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let repository = repo(None);
        journal
            .remember_review_watch(&repository_key(&repository), 1)
            .unwrap();
        let observed = WATCH_GRACE + 2;
        let notice = request(
            &mut journal,
            &repository,
            "viewer",
            "current",
            10,
            point(1, observed as u64),
        )
        .unwrap();
        let now = Instant::now();
        let mut monitor = ReviewMonitor::default();
        assert!(
            monitor
                .expire(&mut journal, &[notice.clone()], now, observed)
                .unwrap()
                .is_empty()
        );
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(
            monitor
                .expire(
                    &mut journal,
                    &[notice.clone()],
                    now + SWEEP_INTERVAL,
                    observed + WATCH_GRACE
                )
                .unwrap(),
            [notice.id]
        );
    }
}
