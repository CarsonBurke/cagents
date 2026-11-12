//! How a pull request's state, checks and review look wherever one is listed:
//! a glyph and its tone, with the words for it on hover and for assistive
//! technology.
//!
//! The glyph alone carries the state, as it does on GitHub, so a row needs no
//! separate badge to say whether its pull request is open, a draft, merged or
//! closed. Checks and review read the same way: a row says at a glance whether
//! they pass and whether anyone has approved, and leaves the counts and words
//! to its tooltip rather than spending a pill of text on each. Every list draws
//! them from here, so one state never reads two ways.

use cosmic::Element;
use cosmic::iced::{self, Color};
use cosmic::widget;

use super::Icon;
use crate::design;
use crate::work::{CheckState, Review};

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

/// A tinted glyph that says `label` on hover and to assistive technology.
fn named_glyph<'a, Message: 'a>(
    icon: Icon,
    tone: fn(&cosmic::Theme) -> Color,
    size: u16,
    label: String,
) -> Element<'a, Message> {
    let glyph = widget::icon(icon.handle())
        .size(size)
        .class(cosmic::theme::Svg::Custom(std::rc::Rc::new(move |theme| {
            iced::widget::svg::Style {
                color: Some(tone(theme)),
            }
        })));
    crate::interaction::tooltip(super::labelled(glyph, label.clone()), label)
}

/// The glyph and tone of a check, or of a set of checks by the worst of them.
pub const fn check_state_look(state: CheckState) -> (Icon, fn(&cosmic::Theme) -> Color) {
    match state {
        CheckState::Failed => (Icon::Error, design::destructive_color),
        CheckState::Pending => (Icon::CheckRunning, design::warning_color),
        CheckState::Passed => (Icon::Check, design::success_color),
        CheckState::Cancelled => (Icon::Cancel, design::muted_color),
        CheckState::Skipped => (Icon::Minus, design::muted_color),
    }
}

/// What a set of checks comes to: the state most in need of attention, and
/// how many are in each state, in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksSummary {
    pub worst: CheckState,
    /// Every state some check is in, worst first: `2 failing, 10 passed`.
    pub label: String,
}

impl ChecksSummary {
    /// The states in the order a summary names them, worst first, with
    /// their words.
    const ORDER: [(CheckState, &'static str); 5] = [
        (CheckState::Failed, "failing"),
        (CheckState::Pending, "running"),
        (CheckState::Cancelled, "cancelled"),
        (CheckState::Passed, "passed"),
        (CheckState::Skipped, "skipped"),
    ];

    /// Summarises the states of a pull request's checks; `None` when it has
    /// none.
    pub fn of(states: impl IntoIterator<Item = CheckState>) -> Option<Self> {
        let mut counts = [0_usize; Self::ORDER.len()];
        for state in states {
            let index = match state {
                CheckState::Failed => 0,
                CheckState::Pending => 1,
                CheckState::Cancelled => 2,
                CheckState::Passed => 3,
                CheckState::Skipped => 4,
            };
            counts[index] += 1;
        }
        Self::of_each(counts)
    }

    /// Summarises the counts a work item keeps of its pull request's checks,
    /// in constant time however many checks there are.
    pub fn of_counts(checks: crate::work::Checks) -> Option<Self> {
        let count = |n: u32| usize::try_from(n).unwrap_or(usize::MAX);
        Self::of_each([
            count(checks.failed),
            count(checks.pending),
            0,
            count(checks.passed),
            0,
        ])
    }

    fn of_each(counts: [usize; Self::ORDER.len()]) -> Option<Self> {
        let worst = Self::ORDER[counts.iter().position(|count| *count > 0)?].0;
        let label = Self::ORDER
            .iter()
            .zip(counts)
            .filter(|(_, count)| *count > 0)
            .map(|((_, word), count)| format!("{count} {word}"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(Self { worst, label })
    }

    /// The checks' name on hover and to assistive technology:
    /// `Checks: 2 failing, 10 passed`.
    pub fn name(&self) -> String {
        format!("Checks: {}", self.label)
    }
}

/// A check's state as a tinted glyph of `size`, named `label` on hover and to
/// assistive technology.
pub fn check_state_glyph<'a, Message: 'a>(
    state: CheckState,
    label: impl Into<String>,
    size: u16,
) -> Element<'a, Message> {
    let (icon, tone) = check_state_look(state);
    named_glyph(icon, tone, size, label.into())
}

/// A pull request's checks as the glyph of the worst of them, named with
/// every count.
pub fn checks_glyph<'a, Message: 'a>(summary: &ChecksSummary, size: u16) -> Element<'a, Message> {
    check_state_glyph(summary.worst, summary.name(), size)
}

/// How a review decision is said, on hover and to assistive technology.
pub const fn review_label(review: Review) -> &'static str {
    match review {
        Review::Approved => "Approved",
        Review::ChangesRequested => "Changes requested",
        Review::Required => "Review required",
    }
}

/// How a review decision looks: a comment bubble, so the three read as one
/// family apart from the checks' marks. A decision fills the bubble and cuts
/// its mark out, a check or a cross, heavy enough to read at the compact size;
/// a review still required leaves the bubble hollow.
pub const fn review_look(review: Review) -> (Icon, fn(&cosmic::Theme) -> Color) {
    match review {
        Review::Approved => (Icon::ReviewApproved, design::success_color),
        Review::ChangesRequested => (Icon::ReviewChanges, design::destructive_color),
        Review::Required => (Icon::ReviewRequired, design::muted_icon_color),
    }
}

/// A review decision as a tinted glyph of `size`, named on hover and to
/// assistive technology.
pub fn pull_request_review_glyph<'a, Message: 'a>(
    review: Review,
    size: u16,
) -> Element<'a, Message> {
    let (icon, tone) = review_look(review);
    named_glyph(icon, tone, size, review_label(review).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which pixels a bundled glyph covers when drawn at `size`.
    fn silhouette(svg: &[u8], size: u32) -> Vec<bool> {
        let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default())
            .expect("a bundled glyph parses");
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
        let scale = size as f32 / tree.size().width();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        pixmap
            .pixels()
            .iter()
            .map(|pixel| pixel.alpha() >= 128)
            .collect()
    }

    /// Tone alone must not tell these glyphs apart: someone who cannot see
    /// red from green still reads the decision from the shape, at the
    /// compact size rows draw them.
    #[test]
    fn review_and_running_glyphs_differ_in_shape_at_the_compact_size() {
        let size = u32::from(design::COMPACT_ICON_SIZE);
        let glyphs = [
            (
                "approved",
                &include_bytes!("../icons/review-approved.svg")[..],
            ),
            (
                "changes",
                &include_bytes!("../icons/review-changes.svg")[..],
            ),
            (
                "required",
                &include_bytes!("../icons/review-required.svg")[..],
            ),
            ("running", &include_bytes!("../icons/check-running.svg")[..]),
        ]
        .map(|(name, svg)| (name, silhouette(svg, size)));
        for (index, (name, pixels)) in glyphs.iter().enumerate() {
            let covered = pixels.iter().filter(|covered| **covered).count();
            assert!(covered >= 20, "{name} is too faint to read: {covered} px");
            for (other, other_pixels) in &glyphs[index + 1..] {
                let differing = pixels
                    .iter()
                    .zip(other_pixels)
                    .filter(|(a, b)| a != b)
                    .count();
                assert!(
                    differing >= 12,
                    "{name} and {other} differ in only {differing} px at {size} px"
                );
            }
        }
    }

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
    fn checks_name_every_count_worst_first_and_take_the_worst_glyph() {
        let summary = ChecksSummary::of([
            CheckState::Passed,
            CheckState::Skipped,
            CheckState::Failed,
            CheckState::Pending,
            CheckState::Failed,
        ])
        .unwrap();
        assert_eq!(summary.worst, CheckState::Failed);
        assert_eq!(summary.label, "2 failing, 1 running, 1 passed, 1 skipped");
        assert_eq!(
            summary.name(),
            "Checks: 2 failing, 1 running, 1 passed, 1 skipped"
        );
        let counts = |failed, pending, passed| crate::work::Checks {
            passed,
            failed,
            pending,
        };
        let summary = ChecksSummary::of_counts(counts(0, 3, 4)).unwrap();
        assert_eq!(summary.worst, CheckState::Pending);
        assert_eq!(summary.label, "3 running, 4 passed");
        assert_eq!(
            ChecksSummary::of_counts(counts(0, 0, 4)).unwrap().label,
            "4 passed"
        );
        assert!(ChecksSummary::of_counts(counts(0, 0, 0)).is_none());
        assert!(ChecksSummary::of([]).is_none());
    }

    #[test]
    fn each_review_decision_draws_its_own_glyph_apart_from_the_checks() {
        let reviews = [Review::Approved, Review::ChangesRequested, Review::Required];
        let checks = [
            CheckState::Failed,
            CheckState::Pending,
            CheckState::Cancelled,
            CheckState::Passed,
            CheckState::Skipped,
        ]
        .map(|state| check_state_look(state).0);
        for (index, review) in reviews.iter().enumerate() {
            let icon = review_look(*review).0;
            assert!(!checks.contains(&icon), "{review:?}");
            for other in &reviews[index + 1..] {
                assert_ne!(icon, review_look(*other).0);
                assert_ne!(review_label(*review), review_label(*other));
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
