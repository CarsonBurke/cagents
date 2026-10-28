//! Presentation of outstanding news and independently observed live activity.
//! A failed notice is an unread historical fact, not the current turn's fate.

use crate::agent::ActivityState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Unread,
    Working,
    Failed,
    Waiting,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub unread: usize,
    pub working: usize,
    pub failed: usize,
    pub waiting: usize,
}

impl Summary {
    pub fn card(activity: Option<ActivityState>, unread: bool, failed: bool) -> Self {
        Self {
            unread: usize::from(unread),
            working: usize::from(activity == Some(ActivityState::Working)),
            failed: usize::from(unread && failed),
            waiting: usize::from(activity == Some(ActivityState::Waiting)),
        }
    }

    pub fn state(self) -> Option<State> {
        if self.waiting != 0 {
            Some(State::Waiting)
        } else if self.failed != 0 {
            Some(State::Failed)
        } else if self.working != 0 {
            Some(State::Working)
        } else if self.unread != 0 {
            Some(State::Unread)
        } else {
            None
        }
    }

    pub fn activity(self) -> Option<ActivityState> {
        if self.waiting != 0 {
            Some(ActivityState::Waiting)
        } else if self.working != 0 {
            Some(ActivityState::Working)
        } else {
            None
        }
    }

    pub fn union(&mut self, other: Self) {
        self.unread = self.unread.max(other.unread);
        self.working = self.working.max(other.working);
        self.failed = self.failed.max(other.failed);
        self.waiting = self.waiting.max(other.waiting);
    }

    pub fn add(&mut self, other: Self) {
        self.unread += other.unread;
        self.working += other.working;
        self.failed += other.failed;
        self.waiting += other.waiting;
    }

    pub(super) fn remove(&mut self, other: Self) {
        self.unread -= other.unread;
        self.working -= other.working;
        self.failed -= other.failed;
        self.waiting -= other.waiting;
    }

    pub fn description(self) -> String {
        let mut parts = Vec::new();
        if self.waiting != 0 {
            parts.push(format!("{} waiting for input", self.waiting));
        }
        if self.failed != 0 {
            parts.push(format!(
                "{} conversation{} with unread failed updates",
                self.failed,
                if self.failed == 1 { "" } else { "s" }
            ));
        }
        if self.working != 0 {
            parts.push(format!("{} working", self.working));
        }
        if self.unread != 0 {
            parts.push(format!(
                "{} unread conversation{}",
                self.unread,
                if self.unread == 1 { "" } else { "s" }
            ));
        }
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_rank_preserves_unread_count_beside_live_activity_and_failure() {
        for waiting in [false, true] {
            for failed in [false, true] {
                for working in [false, true] {
                    let summary = Summary {
                        waiting: usize::from(waiting),
                        failed: usize::from(failed),
                        working: usize::from(working),
                        unread: 3,
                    };
                    assert_eq!(
                        summary.state(),
                        Some(if waiting {
                            State::Waiting
                        } else if failed {
                            State::Failed
                        } else if working {
                            State::Working
                        } else {
                            State::Unread
                        })
                    );
                    assert!(summary.description().contains("3 unread conversations"));
                }
            }
        }
        assert_eq!(Summary::card(None, false, true).state(), None);
    }
}
