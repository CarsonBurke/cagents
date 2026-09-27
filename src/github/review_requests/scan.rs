//! A resumable scan: one page per call, alternating repository discovery and
//! request history. A long timeline can never starve PRs beyond the first page.
use super::{Current, Entry, HistoryPage, Page, Viewer};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum History {
    #[default]
    Unknown,
    First,
    ReRequested,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::review_requests::{RequestedEvent, Reviewer};

    fn viewer(id: &str) -> Viewer {
        Viewer {
            id: id.into(),
            login: id.into(),
        }
    }
    fn request(id: &str) -> Current {
        Current {
            id: id.into(),
            as_code_owner: false,
            requested_reviewer: Some(Reviewer {
                kind: "Team".into(),
                id: Some("team".into()),
            }),
        }
    }
    fn entry(number: u64, current: Option<Current>) -> Entry {
        Entry {
            number,
            title: format!("PR {number}"),
            url: format!("https://github.com/o/r/pull/{number}")
                .parse()
                .unwrap(),
            viewer_latest_review_request: current,
        }
    }
    fn event(id: &str, reviewer: Option<Reviewer>) -> RequestedEvent {
        RequestedEvent {
            id: id.into(),
            created_at: "2026-09-26T01:00:00Z".into(),
            requested_reviewer: reviewer,
        }
    }
    fn page(scan: &mut Scan, work: &Work, entries: Vec<Entry>, next: Option<&str>) {
        scan.page(
            work,
            Page {
                viewer: viewer("reader"),
                entries,
                next_cursor: next.map(str::to_owned),
            },
        )
        .unwrap();
    }

    #[test]
    fn long_history_does_not_starve_requests_after_first_fifty() {
        let mut scan = Scan::default();
        let work = scan.next();
        page(
            &mut scan,
            &work,
            (1..=50)
                .map(|number| entry(number, (number == 1).then(|| request("first"))))
                .collect(),
            Some("page2"),
        );
        let history = scan.next();
        assert!(matches!(history, Work::History { number: 1, .. }));
        scan.history(
            &history,
            HistoryPage {
                viewer: viewer("reader"),
                current: Some(request("first")),
                events: Vec::new(),
                previous_cursor: Some("older".into()),
            },
        )
        .unwrap();
        let work = scan.next();
        assert_eq!(
            work,
            Work::Page {
                cursor: Some("page2".into())
            }
        );
        page(
            &mut scan,
            &work,
            vec![entry(51, Some(request("late")))],
            None,
        );
        assert!(scan.requested.contains_key(&51));
        assert_eq!(scan.requested[&1].history, History::Unknown);
    }

    #[test]
    fn rerequest_requires_two_distinct_events_for_exact_current_reviewer() {
        let mut scan = Scan::default();
        let current = request("active");
        let work = scan.next();
        page(
            &mut scan,
            &work,
            vec![entry(1, Some(current.clone()))],
            None,
        );
        let work = scan.next();
        scan.history(
            &work,
            HistoryPage {
                viewer: viewer("reader"),
                current: Some(current.clone()),
                events: vec![
                    event("current", current.requested_reviewer.clone()),
                    event(
                        "someone-else",
                        Some(Reviewer {
                            kind: "User".into(),
                            id: Some("team".into()),
                        }),
                    ),
                ],
                previous_cursor: Some("earlier".into()),
            },
        )
        .unwrap();
        assert_eq!(scan.requested[&1].history, History::Unknown);
        let work = scan.next();
        page(
            &mut scan,
            &work,
            vec![entry(1, Some(current.clone()))],
            None,
        );
        let work = scan.next();
        scan.history(
            &work,
            HistoryPage {
                viewer: viewer("reader"),
                current: Some(current.clone()),
                events: vec![
                    event("current", current.requested_reviewer.clone()),
                    event("older", current.requested_reviewer.clone()),
                ],
                previous_cursor: None,
            },
        )
        .unwrap();
        assert_eq!(scan.requested[&1].history, History::ReRequested);
    }

    #[test]
    fn changed_request_and_viewer_discard_old_history_authority() {
        let mut scan = Scan::default();
        let work = scan.next();
        page(
            &mut scan,
            &work,
            vec![entry(1, Some(request("old")))],
            Some("next"),
        );
        let work = scan.next();
        scan.history(
            &work,
            HistoryPage {
                viewer: viewer("reader"),
                current: Some(request("new")),
                events: vec![
                    event("one", request("old").requested_reviewer),
                    event("two", request("old").requested_reviewer),
                ],
                previous_cursor: None,
            },
        )
        .unwrap();
        assert_eq!(scan.requested[&1].current.id, "new");
        assert_eq!(scan.requested[&1].history, History::Unknown);
        let work = scan.next();
        scan.page(
            &work,
            Page {
                viewer: viewer("other"),
                entries: vec![entry(99, Some(request("foreign-page")))],
                next_cursor: None,
            },
        )
        .unwrap();
        assert!(scan.requested.is_empty());
        assert_eq!(scan.next(), Work::Page { cursor: None });
    }

    #[test]
    fn timeline_failure_preserves_request_and_completed_scan_retires_missing_requests() {
        let mut scan = Scan::default();
        let work = scan.next();
        page(
            &mut scan,
            &work,
            vec![entry(1, Some(request("active")))],
            None,
        );
        let history = scan.next();
        scan.failed_history(&history);
        assert_eq!(scan.requested[&1].current.id, "active");
        let work = scan.next();
        page(&mut scan, &work, Vec::new(), None);
        assert!(scan.requested.is_empty());
    }

    #[test]
    fn repeated_history_cursor_retries_and_can_later_classify() {
        let mut scan = Scan::default();
        let current = request("active");
        let discovery = scan.next();
        page(
            &mut scan,
            &discovery,
            vec![entry(1, Some(current.clone()))],
            None,
        );
        let first = scan.next();
        scan.history(
            &first,
            HistoryPage {
                viewer: viewer("reader"),
                current: Some(current.clone()),
                events: vec![event("one", current.requested_reviewer.clone())],
                previous_cursor: Some("repeated".into()),
            },
        )
        .unwrap();
        let repeated = Work::History {
            number: 1,
            request: current.clone(),
            before: Some("repeated".into()),
        };
        assert!(
            scan.history(
                &repeated,
                HistoryPage {
                    viewer: viewer("reader"),
                    current: Some(current.clone()),
                    events: Vec::new(),
                    previous_cursor: Some("repeated".into()),
                }
            )
            .is_err()
        );
        scan.failed_history(&repeated);
        assert_eq!(scan.requested[&1].history, History::Unknown);
        let discovery = scan.next();
        page(
            &mut scan,
            &discovery,
            vec![entry(1, Some(current.clone()))],
            None,
        );
        let retry = scan.next();
        assert_eq!(
            retry,
            Work::History {
                number: 1,
                request: current.clone(),
                before: None
            }
        );
        scan.history(
            &retry,
            HistoryPage {
                viewer: viewer("reader"),
                current: Some(current.clone()),
                events: vec![
                    event("one", current.requested_reviewer.clone()),
                    event("two", current.requested_reviewer),
                ],
                previous_cursor: None,
            },
        )
        .unwrap();
        assert_eq!(scan.requested[&1].history, History::ReRequested);
    }

    #[test]
    fn repeated_cursor_restarts_discovery_without_losing_current_requests() {
        let mut scan = Scan::default();
        let first = scan.next();
        page(
            &mut scan,
            &first,
            vec![entry(1, Some(request("active")))],
            Some("repeated"),
        );
        let history = scan.next();
        scan.failed_history(&history);
        let next = scan.next();
        assert!(
            scan.page(
                &next,
                Page {
                    viewer: viewer("reader"),
                    entries: Vec::new(),
                    next_cursor: Some("repeated".into()),
                }
            )
            .is_err()
        );
        assert!(scan.requested.contains_key(&1));
        let history = scan.next();
        scan.failed_history(&history);
        assert_eq!(scan.next(), Work::Page { cursor: None });
    }
}

#[derive(Debug, Clone)]
pub struct Requested {
    pub entry: Entry,
    pub current: Current,
    pub history: History,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    Page {
        cursor: Option<String>,
    },
    History {
        number: u64,
        request: Current,
        before: Option<String>,
    },
}

#[derive(Debug)]
struct PendingHistory {
    number: u64,
    request: Current,
    before: Option<String>,
    matching: BTreeSet<String>,
    cursors: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub viewer: Option<Viewer>,
    pub requested: BTreeMap<u64, Requested>,
    cursor: Option<String>,
    cursors: BTreeSet<String>,
    seen: BTreeSet<u64>,
    histories: VecDeque<PendingHistory>,
    history_next: bool,
}

impl Scan {
    pub fn next(&mut self) -> Work {
        if self.history_next {
            self.history_next = false;
            if let Some(history) = self.histories.front() {
                return Work::History {
                    number: history.number,
                    request: history.request.clone(),
                    before: history.before.clone(),
                };
            }
        }
        self.history_next = true;
        Work::Page {
            cursor: self.cursor.clone(),
        }
    }

    fn observe_viewer(&mut self, viewer: Viewer) -> bool {
        let changed = self.viewer.as_ref().is_some_and(|old| old.id != viewer.id);
        if changed {
            *self = Self::default();
        }
        self.viewer = Some(viewer);
        changed
    }

    fn observe(&mut self, entry: Entry) {
        let number = entry.number;
        let Some(current) = entry.viewer_latest_review_request.clone() else {
            self.requested.remove(&number);
            self.histories.retain(|history| history.number != number);
            return;
        };
        if let Some(previous) = self.requested.get_mut(&number)
            && previous.current == current
        {
            previous.entry = entry;
            return;
        }
        self.histories.retain(|history| history.number != number);
        // Unknown union members still confer current request authority, but
        // cannot be matched to historical identities we cannot represent.
        if current
            .requested_reviewer
            .as_ref()
            .and_then(|reviewer| reviewer.id.as_ref())
            .is_some()
        {
            self.histories.push_back(PendingHistory {
                number,
                request: current.clone(),
                before: None,
                matching: BTreeSet::new(),
                cursors: BTreeSet::new(),
            });
        }
        self.requested.insert(
            number,
            Requested {
                entry,
                current,
                history: History::Unknown,
            },
        );
    }

    pub fn page(&mut self, work: &Work, page: Page) -> Result<(), String> {
        let Work::Page { cursor } = work else {
            return Err("Wrong review scan response".into());
        };
        if self.observe_viewer(page.viewer) && cursor.is_some() {
            // The new account must begin at the first repository page; the
            // old account's continuation is not discovery authority for it.
            return Ok(());
        }
        if cursor != &self.cursor {
            return Err("Review scan cursor changed".into());
        }
        if let Some(next) = &page.next_cursor
            && (Some(next) == cursor.as_ref() || !self.cursors.insert(next.clone()))
        {
            // Restart discovery after malformed pagination, preserving the
            // last observed requests until a complete new census replaces it.
            self.cursor = None;
            self.cursors.clear();
            self.seen.clear();
            return Err("GitHub repeated a review scan cursor".into());
        }
        for entry in page.entries {
            self.seen.insert(entry.number);
            self.observe(entry);
        }
        self.cursor = page.next_cursor;
        if self.cursor.is_none() {
            self.requested
                .retain(|number, _| self.seen.contains(number));
            self.histories
                .retain(|history| self.seen.contains(&history.number));
            self.seen.clear();
            self.cursors.clear();
        }
        Ok(())
    }

    pub fn history(&mut self, work: &Work, page: HistoryPage) -> Result<(), String> {
        let Work::History {
            number,
            request,
            before,
        } = work
        else {
            return Err("Wrong review history response".into());
        };
        if self.observe_viewer(page.viewer) {
            return Ok(());
        }
        let Some(pending) = self.histories.front() else {
            return Ok(());
        };
        if pending.number != *number || pending.request != *request || pending.before != *before {
            return Ok(());
        }
        if page.current.as_ref() != Some(request) {
            self.histories.pop_front();
            if let Some(previous) = self.requested.get(number) {
                let mut entry = previous.entry.clone();
                entry.viewer_latest_review_request = page.current;
                self.observe(entry);
            }
            return Ok(());
        }
        let mut pending = self.histories.pop_front().unwrap();
        for event in page.events {
            if event.requested_reviewer == request.requested_reviewer {
                pending.matching.insert(event.id);
                if pending.matching.len() == 2 {
                    break;
                }
            }
        }
        let classification = if pending.matching.len() >= 2 {
            Some(History::ReRequested)
        } else if page.previous_cursor.is_none() {
            Some(if pending.matching.len() == 1 {
                History::First
            } else {
                History::Unknown
            })
        } else {
            None
        };
        if let Some(classification) = classification {
            if let Some(current) = self.requested.get_mut(number) {
                current.history = classification;
            }
        } else {
            let cursor = page.previous_cursor.unwrap();
            if !pending.cursors.insert(cursor.clone()) {
                // This response consumed the queue entry. Retain it for a
                // fresh traversal after the monitor's backoff; otherwise an
                // unchanged current request could remain Unknown forever.
                pending.before = None;
                pending.cursors.clear();
                pending.matching.clear();
                self.histories.push_back(pending);
                return Err("GitHub repeated a review history cursor".into());
            }
            pending.before = Some(cursor);
            // Round-robin historical requests as well as repository pages.
            self.histories.push_back(pending);
        }
        Ok(())
    }

    /// An unavailable timeline never revokes the authoritative current request.
    /// Retry it after other pending requests and discovery get a turn.
    pub fn failed_history(&mut self, work: &Work) {
        if let Work::History {
            number, request, ..
        } = work
            && self
                .histories
                .front()
                .is_some_and(|pending| pending.number == *number && pending.request == *request)
        {
            let history = self.histories.pop_front().unwrap();
            self.histories.push_back(history);
        }
    }
}
