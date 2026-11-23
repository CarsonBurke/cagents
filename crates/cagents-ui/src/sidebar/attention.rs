//! Presentation of outstanding news and independently observed live activity.
//! A failed notice is an unread historical fact, not the current turn's fate.

use crate::{agent::ActivityState, ui_controls::Icon};

/// An agent's state as the shell shows it, least to most urgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Unread,
    Working,
    /// A provider usage limit stops the agent until it resets.
    Limited,
    Failed,
    /// Waiting on the user for something its provider does not name.
    Waiting,
    /// Waiting for the user to answer a question.
    Question,
    /// Waiting for the user to allow or refuse an action.
    Approval,
}

/// What stands for a state: a glyph, or for the two states that are not
/// asking anything of the user, a spinner or a dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Glyph(Icon),
    Spinner,
    Dot,
}

/// The palette a state is drawn in. Only what needs the user is yellow, and
/// only a failure red; work in progress is calm and news is the accent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Activity,
    News,
    Attention,
    Failure,
}

impl Tone {
    pub fn color(self) -> fn(&cosmic::Theme) -> cosmic::iced::Color {
        match self {
            Self::Activity => crate::design::activity_color,
            Self::News => crate::design::accent_color,
            Self::Attention => crate::design::warning_color,
            Self::Failure => crate::design::destructive_color,
        }
    }
}

/// How a state looks wherever an agent's state is shown: its own mark, so it
/// is told apart without its colour, its tone, and what it says on hover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    pub mark: Mark,
    pub tone: Tone,
    pub label: &'static str,
}

impl State {
    pub const ALL: [Self; 7] = [
        Self::Unread,
        Self::Working,
        Self::Limited,
        Self::Failed,
        Self::Waiting,
        Self::Question,
        Self::Approval,
    ];

    pub const fn look(self) -> Look {
        let (mark, tone, label) = match self {
            Self::Approval => (
                Mark::Glyph(Icon::Permission),
                Tone::Attention,
                "Waiting for your permission",
            ),
            Self::Question => (
                Mark::Glyph(Icon::Question),
                Tone::Attention,
                "Waiting for your answer",
            ),
            Self::Waiting => (
                Mark::Glyph(Icon::AgentWaiting),
                Tone::Attention,
                "Waiting for your input",
            ),
            Self::Limited => (
                Mark::Glyph(Icon::UsageLimit),
                Tone::Attention,
                "Stopped by a usage limit until it resets",
            ),
            Self::Failed => (
                Mark::Glyph(Icon::Error),
                Tone::Failure,
                "Unread failed update",
            ),
            Self::Working => (Mark::Spinner, Tone::Activity, "Working"),
            Self::Unread => (Mark::Dot, Tone::News, "Unread update"),
        };
        Look { mark, tone, label }
    }

    /// Whether the agent is held up until the user does something.
    pub const fn waits_on_user(self) -> bool {
        matches!(self, Self::Waiting | Self::Question | Self::Approval)
    }

    /// The state a status glyph stands for, where a surface names an agent's
    /// state by its glyph: the look's own glyph, or for work, the still
    /// glyph a spinner replaces.
    pub fn of_glyph(icon: Icon) -> Option<Self> {
        if icon == Icon::AgentWorking {
            return Some(Self::Working);
        }
        Self::ALL
            .into_iter()
            .find(|state| state.look().mark == Mark::Glyph(icon))
    }

    /// The glyph that names this state where a surface passes a state by its
    /// glyph; see [`Self::of_glyph`].
    pub const fn glyph(self) -> Option<Icon> {
        match self.look().mark {
            Mark::Glyph(icon) => Some(icon),
            Mark::Spinner => Some(Icon::AgentWorking),
            Mark::Dot => None,
        }
    }

    /// The state an agent's reported activity alone gives it, if any.
    pub const fn of_activity(activity: ActivityState) -> Option<Self> {
        match activity {
            ActivityState::Working => Some(Self::Working),
            ActivityState::Waiting => Some(Self::Waiting),
            ActivityState::Question => Some(Self::Question),
            ActivityState::Approval => Some(Self::Approval),
            ActivityState::Idle | ActivityState::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub unread: usize,
    pub working: usize,
    pub failed: usize,
    /// Agents waiting on the user, whatever for; `approval` and `question`
    /// count those whose provider said what for.
    pub waiting: usize,
    pub approval: usize,
    pub question: usize,
    pub limited: usize,
}

impl Summary {
    pub fn card(activity: Option<ActivityState>, unread: bool, failed: bool) -> Self {
        Self {
            unread: usize::from(unread),
            working: usize::from(activity == Some(ActivityState::Working)),
            failed: usize::from(unread && failed),
            waiting: usize::from(activity.is_some_and(ActivityState::waits_on_user)),
            approval: usize::from(activity == Some(ActivityState::Approval)),
            question: usize::from(activity == Some(ActivityState::Question)),
            limited: 0,
        }
    }

    /// A card whose provider a usage limit stops: whatever the provider last
    /// reported, it is not working, and it cannot carry on until the reset.
    /// The limit is judged from the provider's last failure, so an unread
    /// failure on the card is that limit: it reads as the limit, which only
    /// time or another account lifts, rather than as a red failure.
    pub fn limited(mut self, limited: bool) -> Self {
        if limited {
            self.working = 0;
            self.failed = 0;
            self.limited = 1;
        }
        self
    }

    pub fn state(self) -> Option<State> {
        if self.approval != 0 {
            Some(State::Approval)
        } else if self.question != 0 {
            Some(State::Question)
        } else if self.waiting != 0 {
            Some(State::Waiting)
        } else if self.failed != 0 {
            Some(State::Failed)
        } else if self.limited != 0 {
            Some(State::Limited)
        } else if self.working != 0 {
            Some(State::Working)
        } else if self.unread != 0 {
            Some(State::Unread)
        } else {
            None
        }
    }

    /// What the Attention list puts in front of the user: an agent waiting
    /// for input, an unread failure, a usage limit, or unread news. Work in
    /// progress alone is not; it waits in its own section.
    pub fn needs_attention(self) -> bool {
        self.waiting != 0 || self.failed != 0 || self.limited != 0 || self.unread != 0
    }

    /// The part of the summary that needs the user, without work in
    /// progress. Where a row is listed for what needs the user, a spinner
    /// would claim the row is listed for its work instead.
    pub fn without_work(self) -> Self {
        Self { working: 0, ..self }
    }

    /// Agents working, and nothing the user has to act on.
    pub fn only_working(self) -> bool {
        self.working != 0 && !self.needs_attention()
    }

    pub fn activity(self) -> Option<ActivityState> {
        if self.approval != 0 {
            Some(ActivityState::Approval)
        } else if self.question != 0 {
            Some(ActivityState::Question)
        } else if self.waiting != 0 {
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
        self.approval = self.approval.max(other.approval);
        self.question = self.question.max(other.question);
        self.limited = self.limited.max(other.limited);
    }

    pub fn add(&mut self, other: Self) {
        self.unread += other.unread;
        self.working += other.working;
        self.failed += other.failed;
        self.waiting += other.waiting;
        self.approval += other.approval;
        self.question += other.question;
        self.limited += other.limited;
    }

    /// What `self` counts beyond `other`: the news of a total that rows
    /// drawn on their own do not show. Never below nothing.
    /// What this summary says that `other` does not. The kinds of wait
    /// stay a part of the waits left, so a wait `other` names without its
    /// kind is not said again by its kind.
    pub(super) fn without(self, other: Self) -> Self {
        let waiting = self.waiting.saturating_sub(other.waiting);
        let approval = self.approval.saturating_sub(other.approval).min(waiting);
        Self {
            unread: self.unread.saturating_sub(other.unread),
            working: self.working.saturating_sub(other.working),
            failed: self.failed.saturating_sub(other.failed),
            waiting,
            approval,
            question: self
                .question
                .saturating_sub(other.question)
                .min(waiting - approval),
            limited: self.limited.saturating_sub(other.limited),
        }
    }

    pub(super) fn remove(&mut self, other: Self) {
        self.unread -= other.unread;
        self.working -= other.working;
        self.failed -= other.failed;
        self.waiting -= other.waiting;
        self.approval -= other.approval;
        self.question -= other.question;
        self.limited -= other.limited;
    }

    pub fn description(self) -> String {
        let mut parts = Vec::new();
        if self.approval != 0 {
            parts.push(format!("{} waiting for permission", self.approval));
        }
        if self.question != 0 {
            parts.push(format!("{} waiting for an answer", self.question));
        }
        let unnamed = self.waiting.saturating_sub(self.approval + self.question);
        if unnamed != 0 {
            parts.push(format!("{unnamed} waiting for input"));
        }
        if self.failed != 0 {
            parts.push(format!(
                "{} conversation{} with unread failed updates",
                self.failed,
                if self.failed == 1 { "" } else { "s" }
            ));
        }
        if self.limited != 0 {
            parts.push(format!("{} stopped by a usage limit", self.limited));
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
                        ..Summary::default()
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

    #[test]
    fn a_usage_limit_replaces_working_and_its_own_failure_and_yields_to_waiting() {
        let limited = Summary::card(Some(ActivityState::Working), false, false).limited(true);
        assert_eq!(limited.state(), Some(State::Limited));
        assert_eq!(limited.working, 0);
        assert_eq!(limited.activity(), None);
        assert_eq!(limited.description(), "1 stopped by a usage limit");
        assert_eq!(
            Summary::card(Some(ActivityState::Working), false, false)
                .limited(false)
                .state(),
            Some(State::Working)
        );
        assert_eq!(
            Summary::card(Some(ActivityState::Waiting), false, false)
                .limited(true)
                .state(),
            Some(State::Waiting)
        );
        let stopped = Summary::card(None, true, true).limited(true);
        assert_eq!(stopped.state(), Some(State::Limited));
        assert_eq!(stopped.unread, 1);
        let mut worktree = Summary::default();
        worktree.add(limited);
        worktree.add(Summary::card(Some(ActivityState::Working), false, false));
        assert_eq!(worktree.state(), Some(State::Limited));
        assert_eq!(
            worktree.description(),
            "1 stopped by a usage limit, 1 working"
        );
        worktree.remove(limited);
        assert_eq!(worktree.state(), Some(State::Working));
    }

    /// Every state has a mark of its own, so it is told apart without its
    /// colour, and only what needs the user takes the attention or failure
    /// tone: work and news never look like a problem.
    #[test]
    fn each_state_has_its_own_mark_and_only_what_needs_the_user_is_toned_as_such() {
        let expected = [
            (State::Unread, Mark::Dot, Tone::News),
            (State::Working, Mark::Spinner, Tone::Activity),
            (
                State::Limited,
                Mark::Glyph(Icon::UsageLimit),
                Tone::Attention,
            ),
            (State::Failed, Mark::Glyph(Icon::Error), Tone::Failure),
            (
                State::Waiting,
                Mark::Glyph(Icon::AgentWaiting),
                Tone::Attention,
            ),
            (
                State::Question,
                Mark::Glyph(Icon::Question),
                Tone::Attention,
            ),
            (
                State::Approval,
                Mark::Glyph(Icon::Permission),
                Tone::Attention,
            ),
        ];
        assert_eq!(expected.map(|(state, ..)| state), State::ALL);
        for (state, mark, tone) in expected {
            let look = state.look();
            assert_eq!((look.mark, look.tone), (mark, tone), "{state:?}");
        }
        let marks: std::collections::HashSet<_> = State::ALL
            .iter()
            .map(|state| format!("{:?}", state.look().mark))
            .collect();
        assert_eq!(marks.len(), State::ALL.len(), "two states share a mark");
        let labels: std::collections::HashSet<_> =
            State::ALL.iter().map(|state| state.look().label).collect();
        assert_eq!(labels.len(), State::ALL.len(), "two states share a label");
        for state in State::ALL {
            if let Some(glyph) = state.glyph() {
                assert_eq!(State::of_glyph(glyph), Some(state));
            }
        }
    }

    /// What a waiting agent waits for comes from its provider's report, and
    /// a permission outranks a question, which outranks a wait no one named.
    #[test]
    fn a_wait_keeps_its_kind_and_the_most_urgent_kind_wins() {
        for (activity, state) in [
            (ActivityState::Approval, State::Approval),
            (ActivityState::Question, State::Question),
            (ActivityState::Waiting, State::Waiting),
        ] {
            let card = Summary::card(Some(activity), true, true);
            assert_eq!(card.state(), Some(state));
            assert_eq!(card.activity(), Some(activity));
            assert_eq!(State::of_activity(activity), Some(state));
        }
        let mut worktree = Summary::default();
        worktree.add(Summary::card(Some(ActivityState::Waiting), false, false));
        assert_eq!(worktree.state(), Some(State::Waiting));
        worktree.add(Summary::card(Some(ActivityState::Question), false, false));
        assert_eq!(worktree.state(), Some(State::Question));
        let approval = Summary::card(Some(ActivityState::Approval), false, false);
        worktree.add(approval);
        assert_eq!(worktree.state(), Some(State::Approval));
        assert_eq!(
            worktree.description(),
            "1 waiting for permission, 1 waiting for an answer, 1 waiting for input"
        );
        worktree.remove(approval);
        assert_eq!(worktree.state(), Some(State::Question));
    }

    /// A wait said elsewhere without its kind is not said again by its kind.
    #[test]
    fn taking_out_waits_takes_their_kinds_with_them() {
        let mut channel = Summary::default();
        channel.add(Summary::card(Some(ActivityState::Approval), false, false));
        channel.add(Summary::card(Some(ActivityState::Question), false, false));
        let unnamed = Summary {
            waiting: 2,
            ..Summary::default()
        };
        assert_eq!(channel.without(unnamed).description(), "");
        let one = Summary {
            waiting: 1,
            ..Summary::default()
        };
        assert_eq!(
            channel.without(one).description(),
            "1 waiting for permission"
        );
        let named = Summary {
            waiting: 1,
            question: 1,
            ..Summary::default()
        };
        assert_eq!(
            channel.without(named).description(),
            "1 waiting for permission"
        );
    }
}
