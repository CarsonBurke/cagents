//! How a pull request's state looks wherever one is listed: its glyph, its
//! tone and the word for it.
//!
//! The glyph alone carries the state, as it does on GitHub, so a row needs no
//! separate badge to say whether its pull request is open, a draft, merged or
//! closed. Every list draws it from here, so one state never reads two ways.

use cosmic::Element;
use cosmic::iced::{self, Color};
use cosmic::widget;

use super::Icon;
use crate::design;

/// Where a pull request stands, as people read it: a draft is its own state
/// rather than an open pull request with a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreePullRequestState {
    Open,
    Draft,
    Closed,
    Merged,
}

/// Merged has no COSMIC role, so it takes GitHub's purple, darker on a light
/// surface so it keeps its contrast there.
fn merged_color(theme: &cosmic::Theme) -> Color {
    if theme.cosmic().is_dark {
        Color::from_rgb8(163, 113, 247)
    } else {
        Color::from_rgb8(130, 80, 223)
    }
}

impl WorktreePullRequestState {
    pub fn from_work(pull_request: &crate::work::PullRequest) -> Self {
        match pull_request.state {
            crate::work::PullRequestState::Open if pull_request.draft => Self::Draft,
            crate::work::PullRequestState::Open => Self::Open,
            crate::work::PullRequestState::Closed => Self::Closed,
            crate::work::PullRequestState::Merged => Self::Merged,
        }
    }

    pub const fn icon(self) -> Icon {
        match self {
            Self::Open => Icon::PullRequest,
            Self::Draft => Icon::DraftPullRequest,
            Self::Merged => Icon::MergedPullRequest,
            Self::Closed => Icon::ClosedPullRequest,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Draft => "Draft",
            Self::Merged => "Merged",
            Self::Closed => "Closed",
        }
    }

    /// The state's tone, as the function a style resolves from the theme a
    /// widget is drawn with, so a row never reaches for the process-wide
    /// theme.
    pub const fn tone(self) -> fn(&cosmic::Theme) -> Color {
        match self {
            Self::Open => design::success_color,
            Self::Draft => design::muted_icon_color,
            Self::Merged => merged_color,
            Self::Closed => design::destructive_color,
        }
    }

    pub fn color(self, theme: &cosmic::Theme) -> Color {
        self.tone()(theme)
    }
}

/// A pull request's state as a tinted glyph of `size`; `None` when its state
/// is not known yet, which draws the open glyph without a tone.
pub fn pull_request_state_glyph<'a, Message: 'a>(
    state: Option<WorktreePullRequestState>,
    size: u16,
) -> Element<'a, Message> {
    let icon = state.map_or(Icon::PullRequest, WorktreePullRequestState::icon);
    widget::icon(icon.handle())
        .size(size)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
            iced::widget::svg::Style {
                color: Some(state.map_or_else(
                    || design::muted_icon_color(theme),
                    |state| state.color(theme),
                )),
            }
        })))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_state_draws_its_own_glyph() {
        let states = [
            WorktreePullRequestState::Open,
            WorktreePullRequestState::Draft,
            WorktreePullRequestState::Merged,
            WorktreePullRequestState::Closed,
        ];
        for (index, state) in states.iter().enumerate() {
            for other in &states[index + 1..] {
                assert_ne!(state.icon(), other.icon(), "{state:?} and {other:?}");
                assert_ne!(state.label(), other.label());
            }
        }
    }

    #[test]
    fn a_draft_is_its_own_state_not_an_open_one() {
        let mut pull_request = crate::work::PullRequest {
            number: 1,
            title: String::new(),
            url: String::new(),
            state: crate::work::PullRequestState::Open,
            draft: true,
            head_oid: String::new(),
            contains_local_head: false,
            checks: None,
            review: None,
            updated_at: String::new(),
        };
        assert_eq!(
            WorktreePullRequestState::from_work(&pull_request),
            WorktreePullRequestState::Draft
        );
        // GitHub keeps the draft flag on a merged pull request.
        pull_request.state = crate::work::PullRequestState::Merged;
        assert_eq!(
            WorktreePullRequestState::from_work(&pull_request),
            WorktreePullRequestState::Merged
        );
    }
}
