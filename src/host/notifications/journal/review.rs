//! Durable review-monitor leases and settled census reconciliation.
use super::{Journal, notices, owners, seen};
use crate::{
    github_identity::GitHubRepoKey,
    notifications::{
        Event, Target,
        review::{Census, ReviewRequest, request_key},
    },
};
use diesel::{
    prelude::*,
    result::Error,
    sql_query,
    sql_types::{BigInt, Text},
    sqlite::SqliteConnection,
};
use std::collections::HashSet;
use uuid::Uuid;

const UNWATCHED: i64 = 7 * 24 * 60 * 60;
const CENSUS_FRESHNESS: u64 = 5 * 60;

// Additive bookkeeping keeps the existing attention and dedup schemas
// readable by an older daemon. Watch leases and evidence of request absence
// are independent: an expired lease only means nobody renewed the watch.
pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS review_watches (
        repository TEXT PRIMARY KEY, watched_at INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS review_routes (
        identity TEXT PRIMARY KEY, repository TEXT NOT NULL, viewer TEXT NOT NULL,
        first_observed_at INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS review_routes_scope ON review_routes (repository, viewer);
    CREATE TABLE IF NOT EXISTS review_censuses (
        repository TEXT NOT NULL, viewer TEXT NOT NULL, started_at INTEGER NOT NULL,
        PRIMARY KEY (repository, viewer));";

#[derive(diesel::QueryableByName)]
struct StoredNotice {
    #[diesel(sql_type = Text)]
    session: String,
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    event: String,
}

fn scope(repository: &GitHubRepoKey) -> String {
    format!(
        "{}:{}/{}/{}",
        repository.host,
        repository.port.unwrap_or(443),
        repository.owner,
        repository.name,
    )
}

fn unix_time() -> i64 {
    crate::github_rate_limit::now()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub(super) fn record_route(
    connection: &mut SqliteConnection,
    request: &ReviewRequest,
    identity: &str,
) -> QueryResult<()> {
    record_route_at(connection, request, identity, unix_time())
}

fn record_route_at(
    connection: &mut SqliteConnection,
    request: &ReviewRequest,
    identity: &str,
    first_observed_at: i64,
) -> QueryResult<()> {
    let repository = scope(&request.repository_key());
    sql_query(
        "INSERT INTO review_routes (identity, repository, viewer, first_observed_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (identity) DO UPDATE SET repository=excluded.repository,
             first_observed_at=excluded.first_observed_at
         WHERE review_routes.repository <> excluded.repository",
    )
    .bind::<Text, _>(identity)
    .bind::<Text, _>(&repository)
    .bind::<Text, _>(&request.viewer)
    .bind::<BigInt, _>(first_observed_at)
    .execute(connection)?;
    sql_query(
        "INSERT INTO review_watches (repository, watched_at) VALUES (?1, ?2)
         ON CONFLICT (repository) DO NOTHING",
    )
    .bind::<Text, _>(repository)
    .bind::<BigInt, _>(unix_time())
    .execute(connection)?;
    Ok(())
}

impl Journal {
    pub(super) fn seed_review_routes(&mut self) -> Result<(), String> {
        self.0
            .transaction::<_, Error, _>(|connection| {
                let rows = sql_query(
                    "SELECT session, id, event FROM notices
                     WHERE session LIKE 'review:%' AND session NOT IN (SELECT identity FROM review_routes)",
                )
                .load::<StoredNotice>(connection)?;
                for row in rows {
                    if let Ok(event) = Event::from_journal(&row.event)
                        && let Target::PullRequest(request) = event.target
                    {
                        // Old rows have no reliable last-watch time. Give
                        // their repositories the same grace as a fresh watch.
                        record_route_at(connection, &request, &row.session, 0)?;
                    }
                }
                Ok(())
            })
            .map_err(|error| error.to_string())
    }

    pub(in crate::host::notifications) fn renew_review_watch(
        &mut self,
        repository: &GitHubRepoKey,
        now: i64,
    ) -> Result<(), String> {
        sql_query(
            "INSERT INTO review_watches (repository, watched_at) VALUES (?1, ?2)
             ON CONFLICT (repository) DO UPDATE SET watched_at=excluded.watched_at
             WHERE review_watches.watched_at < excluded.watched_at",
        )
        .bind::<Text, _>(scope(repository))
        .bind::<BigInt, _>(now)
        .execute(&mut self.0)
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub(in crate::host::notifications) fn expire_review_watches(
        &mut self,
        now: i64,
    ) -> Result<(), String> {
        sql_query("DELETE FROM review_watches WHERE watched_at <= ?1")
            .bind::<BigInt, _>(now.saturating_sub(UNWATCHED))
            .execute(&mut self.0)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(in crate::host::notifications) fn reconcile_review_census(
        &mut self,
        repository: &GitHubRepoKey,
        census: Census,
        now: i64,
    ) -> Result<Vec<(Uuid, Target)>, String> {
        if (now as u64).abs_diff(census.completed_at) >= CENSUS_FRESHNESS {
            return Ok(Vec::new());
        }
        let active = census
            .requests
            .into_iter()
            .map(|(_, request)| {
                request_key(&repository.host, repository.port, &census.viewer, &request)
            })
            .collect::<HashSet<_>>();
        let repository = scope(repository);
        self.0
            .transaction::<_, Error, _>(|connection| {
                // Credential contexts can scan the same viewer concurrently.
                // A pass that began later has stronger absence evidence even
                // when an older pass takes longer to reach its last page.
                let updated = sql_query(
                    "INSERT INTO review_censuses (repository, viewer, started_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT (repository, viewer) DO UPDATE SET started_at=excluded.started_at
                     WHERE review_censuses.started_at < excluded.started_at",
                )
                .bind::<Text, _>(&repository)
                .bind::<Text, _>(&census.viewer)
                .bind::<BigInt, _>(census.started_at as i64)
                .execute(connection)?;
                if updated == 0 {
                    return Ok(Vec::new());
                }
                let rows = review_notices(
                    connection,
                    &repository,
                    &census.viewer,
                    census.started_at as i64,
                )?;
                retire_reviews(connection, rows.into_iter().filter(|row| !active.contains(&row.session)))
            })
            .map_err(|error| error.to_string())
    }
}

fn review_notices(
    connection: &mut SqliteConnection,
    repository: &str,
    viewer: &str,
    before: i64,
) -> QueryResult<Vec<StoredNotice>> {
    sql_query(
        "SELECT n.session, n.id, n.event FROM review_routes AS r
         JOIN notices AS n ON n.session = r.identity
         WHERE r.repository = ?1 AND r.viewer = ?2 AND r.first_observed_at < ?3",
    )
    .bind::<Text, _>(repository)
    .bind::<Text, _>(viewer)
    .bind::<BigInt, _>(before)
    .load(connection)
}

fn retire_reviews(
    connection: &mut SqliteConnection,
    rows: impl IntoIterator<Item = StoredNotice>,
) -> QueryResult<Vec<(Uuid, Target)>> {
    let mut retired = Vec::new();
    for row in rows {
        let id = row
            .id
            .parse()
            .map_err(|error| Error::DeserializationError(Box::new(error)))?;
        let target = Event::from_journal(&row.event)
            .map_err(|error| Error::DeserializationError(Box::new(std::io::Error::other(error))))?
            .target;
        diesel::delete(notices::table.find(&row.session)).execute(connection)?;
        diesel::delete(seen::table.filter(seen::session.eq(&row.session))).execute(connection)?;
        diesel::delete(owners::table.find(&row.session)).execute(connection)?;
        retired.push((id, target));
    }
    Ok(retired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::{
        Application, Destination,
        review::{ObservedRequests, WatchUpdate},
    };

    fn request() -> ReviewRequest {
        ReviewRequest {
            host: "github.example".into(),
            port: None,
            owner: "owner".into(),
            repository: "repo".into(),
            number: 51,
            viewer: "alice".into(),
            request: "first".into(),
        }
    }

    fn event(request: ReviewRequest) -> Event {
        ObservedRequests {
            destination: Destination {
                application: Application::Cagents,
                window: "primary".into(),
            },
            requests: vec![request],
        }
        .events()
        .next()
        .unwrap()
    }

    #[test]
    fn settled_census_retires_only_exact_scope_and_newer_absence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let retired = request();
        let active = ReviewRequest {
            request: "second".into(),
            ..retired.clone()
        };
        let other_viewer = ReviewRequest {
            viewer: "bob".into(),
            ..retired.clone()
        };
        let other_port = ReviewRequest {
            port: Some(8443),
            ..retired.clone()
        };
        let other_repo = ReviewRequest {
            repository: "another".into(),
            request: "another-request".into(),
            ..retired.clone()
        };
        for request in [&retired, &active, &other_viewer, &other_port, &other_repo] {
            assert!(
                journal
                    .record(event(request.clone()), false)
                    .unwrap()
                    .is_some()
            );
        }
        let now = unix_time() + 2;
        let repository = active.repository_key();
        let census = Census {
            viewer: active.viewer.clone(),
            started_at: now as u64,
            completed_at: now as u64,
            requests: vec![(active.number, active.request.clone())],
        };
        WatchUpdate {
            repository: repository.clone(),
            census: Some(census.clone()),
        }
        .validate()
        .unwrap();
        journal.renew_review_watch(&repository, now).unwrap();
        assert_eq!(
            journal.list().unwrap().len(),
            5,
            "a heartbeat has no absence authority"
        );
        let removed = journal
            .reconcile_review_census(&repository, census.clone(), now)
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].1, Target::PullRequest(retired.clone()));
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        let stale = Census {
            started_at: census.started_at - 1,
            completed_at: census.completed_at + 1,
            requests: vec![],
            ..census
        };
        assert!(
            journal
                .reconcile_review_census(&repository, stale, now)
                .unwrap()
                .is_empty()
        );
        assert_eq!(journal.list().unwrap().len(), 4);
        assert!(
            journal.record(event(retired), false).unwrap().is_none(),
            "census retirement keeps delivery dedup"
        );
    }

    #[test]
    fn a_slow_census_cannot_remove_a_request_observed_after_it_started() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&directory.path().join("attention.sqlite3")).unwrap();
        let old = request();
        let new = ReviewRequest {
            request: "new".into(),
            ..old.clone()
        };
        let at = unix_time();
        for request in [&old, &new] {
            journal
                .record(event(request.clone()), false)
                .unwrap()
                .unwrap();
        }
        sql_query("UPDATE review_routes SET first_observed_at = ?1 WHERE identity = ?2")
            .bind::<BigInt, _>(at - 2)
            .bind::<Text, _>(old.storage_key())
            .execute(&mut journal.0)
            .unwrap();
        let census = Census {
            viewer: old.viewer.clone(),
            started_at: (at - 1) as u64,
            completed_at: (at + 2) as u64,
            requests: Vec::new(),
        };
        let removed = journal
            .reconcile_review_census(&old.repository_key(), census.clone(), at + 2)
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].1, Target::PullRequest(old));
        assert_eq!(
            journal.list().unwrap()[0].event.target,
            Target::PullRequest(new.clone())
        );
        let fresh_at = unix_time().max(at + 2) + 2;
        let fresh = Census {
            started_at: fresh_at as u64,
            completed_at: fresh_at as u64,
            ..census
        };
        let removed = journal
            .reconcile_review_census(&new.repository_key(), fresh, fresh_at)
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].1, Target::PullRequest(new));
    }

    #[test]
    fn expired_watch_preserves_unread_notices_and_dedup_until_settled_absence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let request = request();
        let repo = request.repository_key();
        let watched = ReviewRequest {
            repository: "watched".into(),
            request: "watched-request".into(),
            ..request.clone()
        };
        let unknown = ReviewRequest {
            repository: "unknown".into(),
            request: "unknown-request".into(),
            ..request.clone()
        };
        let original = journal
            .record(event(request.clone()), false)
            .unwrap()
            .unwrap();
        let uncertain = journal
            .record(event(unknown.clone()), false)
            .unwrap()
            .unwrap();
        journal
            .record(event(watched.clone()), false)
            .unwrap()
            .unwrap();

        let at = unix_time() + 2;
        journal.renew_review_watch(&repo, at).unwrap();
        journal
            .renew_review_watch(&watched.repository_key(), at + UNWATCHED / 2)
            .unwrap();
        let live = Census {
            viewer: request.viewer.clone(),
            started_at: at as u64,
            completed_at: at as u64,
            requests: vec![(request.number, request.request.clone())],
        };
        assert!(
            journal
                .reconcile_review_census(&repo, live.clone(), at)
                .unwrap()
                .is_empty()
        );
        let expired_at = at + UNWATCHED;
        journal.expire_review_watches(expired_at).unwrap();
        for repository in [&repo, &unknown.repository_key()] {
            assert_eq!(
                sql_query(
                    "UPDATE review_watches SET watched_at = watched_at WHERE repository = ?1"
                )
                .bind::<Text, _>(scope(repository))
                .execute(&mut journal.0)
                .unwrap(),
                0
            );
        }
        // A heartbeat after expiry has no new evidence about the request.
        journal
            .renew_review_watch(&repo, expired_at + UNWATCHED)
            .unwrap();
        drop(journal);
        let mut journal = Journal::open(&path).unwrap();
        let notices = journal.list().unwrap();
        assert_eq!(notices.len(), 3);
        for id in [original.id, uncertain.id] {
            assert!(
                notices
                    .iter()
                    .any(|notice| notice.id == id && !notice.acknowledged)
            );
        }
        assert!(
            journal
                .record(event(request.clone()), false)
                .unwrap()
                .is_none()
        );
        assert!(journal.record(event(unknown), false).unwrap().is_none());
        let absent = Census {
            started_at: expired_at as u64,
            completed_at: expired_at as u64,
            requests: Vec::new(),
            ..live
        };
        assert_eq!(
            journal
                .reconcile_review_census(&repo, absent, expired_at)
                .unwrap(),
            vec![(original.id, Target::PullRequest(request))]
        );
        assert_eq!(journal.list().unwrap().len(), 2);
    }
}
