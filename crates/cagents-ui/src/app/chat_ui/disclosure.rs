//! One scan line for transcript activity and content disclosures.

use super::*;
use std::rc::Rc;

pub(super) fn duration(millis: u64) -> String {
    let seconds = millis / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

/// A tool's own interval, never the time spent waiting for the next tool.
pub(super) fn item_duration(item: &chat::Item, now: Option<u64>) -> Option<String> {
    item_duration_ms(item, now).map(duration)
}

/// Unformatted timing for retained rows, hashed at the displayed second cadence.
pub(super) fn item_duration_ms(item: &chat::Item, now: Option<u64>) -> Option<u64> {
    let status = item.content.status();
    let running = status == Some(chat::ToolStatus::Running);
    if !running
        && let ItemContent::Command {
            duration_ms: Some(millis),
            ..
        } = item.content
    {
        return Some(millis);
    }
    let start = item.started_at?;
    let end = if running { now } else { item.at }?;
    Some(end.saturating_sub(start))
}

/// What a row's leading glyph reports: how a step stands, or how a group's
/// steps stand together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Status {
    Tool(chat::ToolStatus),
    /// Some of a group's steps failed and the others did not. Its mark is
    /// the error colour's, as a failure's is, so collapsing the group never
    /// hides one, but half filled rather than the full error mark, which
    /// stays for a group in which nothing worked.
    PartlyFailed,
}

impl From<chat::ToolStatus> for Status {
    fn from(status: chat::ToolStatus) -> Self {
        Self::Tool(status)
    }
}

fn status_glyph(kind: Icon, status: Option<Status>) -> Icon {
    let Some(status) = status else {
        return kind;
    };
    match status {
        Status::PartlyFailed => Icon::PartlyFailed,
        Status::Tool(chat::ToolStatus::Failed) => Icon::Error,
        Status::Tool(chat::ToolStatus::Denied) => Icon::Unavailable,
        // Work that was stopped, or that ended without saying how it went,
        // has not failed. Its mark is a still one, unlike the Stop control.
        Status::Tool(chat::ToolStatus::Stopped | chat::ToolStatus::Ended) => Icon::Ended,
        Status::Tool(chat::ToolStatus::Waiting) => Icon::AgentBackground,
        Status::Tool(chat::ToolStatus::Running | chat::ToolStatus::Succeeded) => kind,
    }
}

/// What a row says. A path is told apart from its neighbours by its last
/// components, so a row that names one keeps those in view and lets the
/// start of the path give way instead, behind a verb that always shows.
pub(super) enum Headline {
    Text(String),
    /// Who a row is about, such as the agent that reported: emphasised by
    /// weight, as an agent's name is wherever a row names one.
    Name(String),
    Path {
        verb: String,
        path: String,
    },
}

impl From<String> for Headline {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl Headline {
    fn view(self) -> Element<'static, Message> {
        let line = |text: String| {
            crate::ui_controls::FadeText::transcript(text)
                .class(cosmic::theme::Text::Custom(design::muted_text))
        };
        match self {
            // The fade is in the glyphs themselves, so it is right on the
            // row's resting, hovered, pressed and focused paint alike.
            Self::Text(text) => line(text).width(Length::Fill).into(),
            Self::Name(name) => crate::ui_controls::FadeText::transcript(name)
                .font(design::transcript_name_font())
                .width(Length::Fill)
                .into(),
            Self::Path { verb, path } => {
                let (anchor, rest) = path.split_at(anchor(&path));
                let mut path = vec![];
                if !anchor.is_empty() {
                    path.push(line(anchor.to_owned()).into());
                }
                path.push(
                    line(rest.to_owned())
                        .edge(crate::ui_controls::FadeEdge::Start)
                        .width(Length::Fill)
                        .into(),
                );
                let path = widget::row::with_children(path).width(Length::Fill);
                if verb.is_empty() {
                    return path.into();
                }
                // The gap stands for the space between the words, outside
                // the path, so the fade never takes it.
                widget::row::with_children(vec![line(verb).into(), path.into()])
                    .spacing(space::XS)
                    .width(Length::Fill)
                    .into()
            }
        }
    }
}

impl Headline {
    /// What the headline says, as one line of plain text.
    pub(super) fn plain(&self) -> String {
        match self {
            Self::Text(text) | Self::Name(text) => text.clone(),
            Self::Path { verb, path } => format!("{verb} {path}"),
        }
    }

    /// The headline's words at their natural width, never drawn: what a
    /// row's trailing state gives way to.
    fn measure(&self) -> Element<'static, Message> {
        let line = |text: &str| {
            design::transcript(text.to_owned()).wrapping(iced::widget::text::Wrapping::None)
        };
        match self {
            Self::Text(text) => line(text).into(),
            Self::Name(name) => line(name).font(design::transcript_name_font()).into(),
            Self::Path { verb, path } => {
                widget::row::with_children(vec![line(verb).into(), line(path).into()])
                    .spacing(space::XS)
                    .into()
            }
        }
    }
}

/// How much of a shown path, from its start, says where it lies outside
/// the working directory: `~/`, `/`, or a run of `../`. A row keeps it in
/// view, so the path still reads as outside once the rest of it fades.
fn anchor(path: &str) -> usize {
    let anchor = if path.starts_with("~/") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        path.len() - path.trim_start_matches("../").len()
    };
    // A path that is nothing but its anchor is shown whole.
    if anchor < path.len() { anchor } else { 0 }
}

/// What a row's trailing cell says.
pub(super) enum Trailing {
    /// How long the work took, which is secondary, as a timestamp is.
    Elapsed(String),
    /// What became of it, such as "finished": the row's state, read at the
    /// transcript's size as an agent's state is.
    Outcome(String),
}

/// One disclosure row. Its accessible name carries the trailing metadata,
/// which is drawn in a cell of its own.
pub(super) fn row(
    label: impl Into<Headline>,
    name: String,
    icon: Icon,
    status: Option<Status>,
    trailing: Option<String>,
    expanded: Option<bool>,
    toggle: Option<Action>,
) -> Element<'static, Message> {
    let name = match &trailing {
        Some(trailing) => format!("{name} · {trailing}"),
        None => name,
    };
    row_named(
        label,
        name,
        icon,
        status,
        trailing.map(Trailing::Elapsed),
        expanded,
        toggle,
    )
}

/// The still mark of a state, in a dot the size of a caption's lowercase
/// letters: the accent beside a live caption, or in its place, the state's
/// colour, where a narrow row has no room for its words.
pub(super) fn state_dot(color: fn(&cosmic::Theme) -> iced::Color) -> Element<'static, Message> {
    const SIZE: f32 = 6.0;
    widget::container(widget::Space::new())
        .width(Length::Fixed(SIZE))
        .height(Length::Fixed(SIZE))
        .class(cosmic::theme::Container::custom(move |theme| {
            widget::container::Style {
                background: Some(iced::Background::Color(color(theme))),
                border: iced::Border {
                    radius: (SIZE / 2.0).into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }))
        .into()
}

/// A row whose accessible name already includes its trailing metadata.
/// Delivered reports retain their canonical full heading even when the title
/// and outcome are drawn in separate cells.
pub(super) fn row_named(
    label: impl Into<Headline>,
    name: String,
    icon: Icon,
    status: Option<Status>,
    trailing: Option<Trailing>,
    expanded: Option<bool>,
    toggle: Option<Action>,
) -> Element<'static, Message> {
    let label = label.into();
    let indicator = match status {
        Some(Status::Tool(chat::ToolStatus::Running)) => crate::ui_controls::activity_spinner(
            f32::from(design::LEADING_ICON_SIZE),
            design::muted_color,
        ),
        Some(Status::Tool(chat::ToolStatus::Failed) | Status::PartlyFailed) => {
            widget::icon(status_glyph(icon, status).handle())
                .size(design::LEADING_ICON_SIZE)
                .class(cosmic::theme::Svg::Custom(Rc::new(|theme| {
                    iced::widget::svg::Style {
                        color: Some(design::destructive_color(theme)),
                    }
                })))
                .into()
        }
        _ => {
            crate::ui_controls::passive_icon(status_glyph(icon, status), design::LEADING_ICON_SIZE)
        }
    };
    let mut cells = vec![indicator];
    let trailing_text = |text: iced::widget::Text<'static, cosmic::Theme, cosmic::Renderer>| {
        text.class(cosmic::theme::Text::Custom(design::muted_text))
            .wrapping(iced::widget::text::Wrapping::None)
    };
    match trailing {
        // A duration is a caption, short enough to keep its place.
        Some(Trailing::Elapsed(elapsed)) => {
            cells.push(label.view());
            cells.push(trailing_text(design::caption(elapsed)).into());
        }
        // An outcome gives way to the headline where both no longer fit:
        // the row's name is what a reader needs. A failure is still told by
        // the leading glyph, and a stop by a dot in the warning colour, as a
        // cancelled agent's row marks it. The outcome stays in the row's
        // accessible name and tooltip.
        Some(Trailing::Outcome(outcome)) => {
            let measure = label.measure();
            let stopped = matches!(outcome.as_str(), "stopped" | "was stopped");
            cells.push(crate::ui_controls::yielding(
                label.view(),
                measure,
                trailing_text(design::transcript(outcome)),
                stopped.then(|| state_dot(design::warning_color)),
                step::GAP_S,
            ));
        }
        None => cells.push(label.view()),
    }
    if let Some(expanded) = expanded {
        cells.push(crate::ui_controls::passive_icon(
            if expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            },
            design::COMPACT_ICON_SIZE,
        ));
    } else {
        // Non-expandable thoughts keep the duration column aligned without
        // advertising a disclosure that has no content.
        cells.push(widget::Space::new().width(design::COMPACT_ICON_SIZE).into());
    }
    let line = widget::row::with_children(cells)
        .width(Length::Fill)
        .spacing(step::GAP_S)
        .align_y(Alignment::Center);
    if let Some(toggle) = toggle {
        crate::interaction::a11y_target(
            widget::button::custom(line)
                .name(name)
                .description(if expanded == Some(true) {
                    "Collapse details"
                } else {
                    "Expand details"
                })
                .class(crate::ui_controls::control_style(false, None))
                .width(Length::Fill)
                .padding([step::GAP_XS as u16, step::GAP_S as u16])
                .on_press(action(toggle)),
        )
    } else {
        widget::container(line)
            .width(Length::Fill)
            .padding([step::GAP_XS as u16, step::GAP_S as u16])
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_keeps_what_says_it_lies_outside_in_view() {
        assert_eq!(anchor("~/.claude/settings.json"), 2);
        assert_eq!(anchor("/etc/hosts"), 1);
        assert_eq!(anchor("../../other/a.rs"), 6);
        assert_eq!(anchor("src/a.rs"), 0);
        assert_eq!(anchor("/"), 0);
        assert_eq!(anchor("../"), 0);
    }

    #[test]
    fn transcript_duration_uses_whole_seconds_and_minutes() {
        for (ms, expected) in [(0, "0s"), (999, "0s"), (12_999, "12s"), (65_000, "1m 5s")] {
            assert_eq!(duration(ms), expected);
        }
    }

    #[test]
    fn non_success_states_stay_visible_when_the_summary_fades() {
        assert_eq!(
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Succeeded.into())),
            Icon::Terminal
        );
        assert_eq!(
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Denied.into())),
            Icon::Unavailable
        );
        // Stopping is not failing, and nor is ending unreported. Neither is
        // drawn as the Stop control, which reads as something to press.
        for status in [chat::ToolStatus::Stopped, chat::ToolStatus::Ended] {
            assert_eq!(
                status_glyph(Icon::Terminal, Some(status.into())),
                Icon::Ended
            );
        }
        assert_eq!(
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Waiting.into())),
            Icon::AgentBackground
        );
        assert_eq!(
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Failed.into())),
            Icon::Error
        );
        // Some steps failing among others that did not is its own mark.
        assert_eq!(
            status_glyph(Icon::Terminal, Some(Status::PartlyFailed)),
            Icon::PartlyFailed
        );
    }

    #[test]
    fn empty_reasoning_reserves_the_disclosure_column_for_aligned_time() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = crate::app::tests::headless_renderer();
        let mut times = Vec::new();
        for expanded in [None, Some(false), Some(true)] {
            let mut element = row(
                "Thinking".to_owned(),
                "Reasoning".into(),
                Icon::Reasoning,
                None,
                Some("12s".into()),
                expanded,
                None,
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(200.0, 80.0)),
            );
            let line = Layout::new(&node).children().next().unwrap();
            times.push(line.children().nth(2).unwrap().bounds());
        }
        assert_eq!(times[0], times[1]);
        assert_eq!(times[1], times[2]);
    }

    /// The glyph leads a line of body text, so it is drawn at the leading
    /// size in every state, and the words start in the same place whether
    /// the step is running, failed or done.
    #[test]
    fn the_leading_glyph_is_sized_to_the_line_in_every_state() {
        use iced::advanced::{Layout, layout, widget::Tree};
        let renderer = crate::app::tests::headless_renderer();
        let mut words = Vec::new();
        for status in [
            None,
            Some(chat::ToolStatus::Running),
            Some(chat::ToolStatus::Failed),
            Some(chat::ToolStatus::Succeeded),
            Some(chat::ToolStatus::Stopped),
        ]
        .map(|status| status.map(Status::from))
        .into_iter()
        .chain([Some(Status::PartlyFailed)])
        {
            let mut element = row(
                "Ran 2 commands".to_owned(),
                "Ran 2 commands".into(),
                Icon::Terminal,
                status,
                None,
                Some(false),
                None,
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(300.0, 80.0)),
            );
            let line = Layout::new(&node).children().next().unwrap();
            let glyph = line.children().next().unwrap().bounds();
            let size = f32::from(design::LEADING_ICON_SIZE);
            assert_eq!((glyph.width, glyph.height), (size, size), "{status:?}");
            words.push(line.children().nth(1).unwrap().bounds().x);
        }
        assert!(words.windows(2).all(|pair| pair[0] == pair[1]), "{words:?}");
    }

    /// Where an outcome gives way to the name, a stop keeps a warning dot
    /// in its place, as a cancelled agent's row does, while a finish leaves
    /// nothing: the two must not read alike in a narrow pane.
    #[test]
    fn a_narrow_report_marks_a_stop_but_not_a_finish() {
        use iced::advanced::{Layout, layout, widget::Tree};
        fn dots(layout: Layout<'_>) -> usize {
            let bounds = layout.bounds();
            usize::from(bounds.width == 6.0 && bounds.height == 6.0)
                + layout.children().map(dots).sum::<usize>()
        }
        let renderer = crate::app::tests::headless_renderer();
        let drawn = |outcome: &str, width: f32| {
            let mut element = row_named(
                Headline::Name("Consolidate the directions".into()),
                format!("Agent \"Consolidate the directions\" {outcome}"),
                Icon::Chat,
                None,
                Some(Trailing::Outcome(outcome.into())),
                Some(false),
                None,
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 80.0)),
            );
            dots(Layout::new(&node))
        };
        assert_eq!(drawn("was stopped", 160.0), 1);
        assert_eq!(drawn("stopped", 160.0), 1);
        assert_eq!(drawn("finished", 160.0), 0);
        assert_eq!(drawn("was stopped", 600.0), 0, "wide, the words show");
    }
}
