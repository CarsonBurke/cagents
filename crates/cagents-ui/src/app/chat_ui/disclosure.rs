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

/// How a row's work stands, or how a group's steps stand together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Status {
    Tool(chat::ToolStatus),
    /// Some of a group's steps failed and the others did not. Its mark is
    /// the error colour's, as a failure's is, so collapsing the group never
    /// hides one, but half filled rather than the full error mark, which
    /// stays for a group in which nothing worked.
    PartlyFailed,
    /// Under way, where another row carries the transcript's one activity
    /// indicator: a still mark rather than a second spinner.
    Working,
}

impl From<chat::ToolStatus> for Status {
    fn from(status: chat::ToolStatus) -> Self {
        Self::Tool(status)
    }
}

/// A row's leading glyph: what kind of work it is, or, where it failed, the
/// failure, in the error colour. A failure is marked there and only there,
/// so the eye finds every one down the row's leading edge.
fn status_glyph(kind: Icon, status: Option<Status>) -> Icon {
    match status {
        Some(Status::PartlyFailed) => Icon::PartlyFailed,
        Some(Status::Tool(chat::ToolStatus::Failed)) => Icon::Error,
        _ => kind,
    }
}

fn failed(status: Option<Status>) -> bool {
    matches!(
        status,
        Some(Status::Tool(chat::ToolStatus::Failed) | Status::PartlyFailed)
    )
}

/// How a row's work stands, marked small in its trailing column, just
/// before its time, so every row says it in the same place and at the
/// same weight. A failure has no mark here: the leading glyph tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Mark {
    /// The transcript's one activity indicator.
    Spinning,
    /// Under way, without it: a still dot in the accent colour.
    Working,
    Finished,
    /// Stopped, or ended without saying how it went. Neither has failed.
    Stopped,
    WaitingOnBackgroundWork,
    Denied,
    /// Waiting on the person: a still dot in the warning colour.
    Attention,
}

impl Mark {
    pub(super) fn of(status: Status) -> Option<Self> {
        Some(match status {
            Status::Tool(chat::ToolStatus::Running) => Self::Spinning,
            Status::Working => Self::Working,
            Status::Tool(chat::ToolStatus::Succeeded) => Self::Finished,
            Status::Tool(chat::ToolStatus::Stopped | chat::ToolStatus::Ended) => Self::Stopped,
            Status::Tool(chat::ToolStatus::Waiting) => Self::WaitingOnBackgroundWork,
            Status::Tool(chat::ToolStatus::Denied) => Self::Denied,
            Status::Tool(chat::ToolStatus::Failed) | Status::PartlyFailed => return None,
        })
    }

    /// The mark in words, for the row's accessible name and tooltip.
    pub(super) fn word(self) -> &'static str {
        match self {
            Self::Spinning | Self::Working => "working",
            Self::Finished => "finished",
            Self::Stopped => "stopped",
            Self::WaitingOnBackgroundWork => "waiting on background work",
            Self::Denied => "denied",
            Self::Attention => "waiting",
        }
    }

    /// The mark, named in words for assistive technology, where a row with
    /// nothing to open has no accessible name of its own to carry them.
    pub(super) fn view(self) -> Element<'static, Message> {
        crate::ui_controls::labelled(self.glyph(), self.word())
    }

    fn glyph(self) -> Element<'static, Message> {
        let size = design::COMPACT_ICON_SIZE;
        let centred = |dot: Element<'static, Message>| {
            widget::container(dot)
                .width(Length::Fixed(f32::from(size)))
                .height(Length::Fixed(f32::from(size)))
                .align_x(Alignment::Center)
                .align_y(Alignment::Center)
                .into()
        };
        match self {
            Self::Spinning => {
                crate::ui_controls::activity_spinner(f32::from(size), design::muted_color)
            }
            Self::Working => centred(state_dot(design::accent_color)),
            Self::Attention => centred(state_dot(design::warning_color)),
            Self::Finished => crate::ui_controls::passive_icon(Icon::Check, size),
            Self::Stopped => crate::ui_controls::passive_icon(Icon::Ended, size),
            Self::WaitingOnBackgroundWork => {
                crate::ui_controls::passive_icon(Icon::AgentBackground, size)
            }
            Self::Denied => crate::ui_controls::passive_icon(Icon::Unavailable, size),
        }
    }
}

/// A row's trailing column: any words its state still needs, such as what
/// an agent waits on, then the state's mark, then how long the work took.
/// All of it is a muted caption, so it never outweighs the row's own words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Trailing {
    pub(super) detail: Option<String>,
    pub(super) mark: Option<Mark>,
    pub(super) elapsed: Option<String>,
}

impl Trailing {
    pub(super) fn is_empty(&self) -> bool {
        self.detail.is_none() && self.mark.is_none() && self.elapsed.is_none()
    }

    /// What the column says, after the row's name: " · finished · 3m 2s".
    pub(super) fn described(&self) -> String {
        let mut words = String::new();
        if let Some(detail) = &self.detail {
            words.push_str(&format!(" · {detail}"));
        } else if let Some(mark) = self.mark {
            words.push_str(&format!(" · {}", mark.word()));
        }
        if let Some(elapsed) = &self.elapsed {
            words.push_str(&format!(" · {elapsed}"));
        }
        words
    }

    /// The column, and what of it stays where the row's name needs the
    /// room: its mark, but for a finish, without the words or the time.
    pub(super) fn view(self) -> (Element<'static, Message>, Option<Element<'static, Message>>) {
        let caption = |text: String| {
            design::caption(text)
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .wrapping(iced::widget::text::Wrapping::None)
        };
        let mut cells: Vec<Element<'static, Message>> = Vec::new();
        // Narrow, a finish gives way with the rest: it is how work most
        // often stands, so a row without a mark reads as finished, and the
        // name keeps the room. Every other mark stays.
        let compact = self
            .mark
            .filter(|mark| *mark != Mark::Finished)
            .map(Mark::view);
        if let Some(detail) = self.detail {
            cells.push(
                crate::ui_controls::FadeText::caption(detail)
                    .limit(STATUS_CHARS)
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
        }
        if let Some(mark) = self.mark {
            cells.push(mark.view());
        }
        if let Some(elapsed) = self.elapsed {
            cells.push(caption(elapsed).into());
        }
        (
            widget::row::with_children(cells)
                .spacing(space::XS)
                .align_y(Alignment::Center)
                .into(),
            compact,
        )
    }
}

/// How many characters of a state's words a row shows: as many as its tab's
/// caption, so the row says what the tab says.
pub(super) const STATUS_CHARS: usize = super::view::AGENT_CAPTION_CHARS;

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

/// One disclosure row. Its accessible name carries what its trailing
/// column marks, which is drawn in a cell of its own.
pub(super) fn row(
    label: impl Into<Headline>,
    name: String,
    icon: Icon,
    status: Option<Status>,
    elapsed: Option<String>,
    expanded: Option<bool>,
    toggle: Option<Action>,
) -> Element<'static, Message> {
    let trailing = Trailing {
        detail: None,
        mark: status.and_then(Mark::of),
        elapsed,
    };
    // A failure is marked by the leading glyph alone, so its words are the
    // accessible name's to carry. A group failing in part says how many of
    // its steps failed in its own words.
    let failure = match status {
        Some(Status::Tool(chat::ToolStatus::Failed)) => " · failed",
        _ => "",
    };
    let name = format!("{name}{failure}{}", trailing.described());
    row_named(label, name, icon, status, trailing, expanded, toggle)
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
    trailing: Trailing,
    expanded: Option<bool>,
    toggle: Option<Action>,
) -> Element<'static, Message> {
    let label = label.into();
    let glyph = status_glyph(icon, status);
    let indicator = if failed(status) {
        widget::icon(glyph.handle())
            .size(design::LEADING_ICON_SIZE)
            .class(cosmic::theme::Svg::Custom(Rc::new(|theme| {
                iced::widget::svg::Style {
                    color: Some(design::destructive_color(theme)),
                }
            })))
            .into()
    } else {
        crate::ui_controls::passive_icon(glyph, design::LEADING_ICON_SIZE)
    };
    let mut cells = vec![indicator];
    if trailing.is_empty() {
        cells.push(label.view());
    } else if trailing.detail.is_some() || matches!(label, Headline::Name(_)) {
        // Words about the state, and the time beside an agent's name, give
        // way to the headline where both no longer fit: the row's name is
        // what a reader needs. Its mark stays, and the words stay in the
        // row's accessible name and tooltip.
        let measure = label.measure();
        let (column, compact) = trailing.view();
        cells.push(crate::ui_controls::yielding(
            label.view(),
            measure,
            column,
            compact,
            step::GAP_S,
        ));
    } else {
        // A mark and a time are short enough to keep their place, beside
        // words that fade where they run short.
        cells.push(label.view());
        cells.push(trailing.view().0);
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
    fn a_failure_is_marked_at_the_leading_edge_and_every_other_state_beside_the_time() {
        use chat::ToolStatus::*;
        for status in [Running, Succeeded, Stopped, Ended, Waiting, Denied] {
            assert_eq!(
                status_glyph(Icon::Terminal, Some(status.into())),
                Icon::Terminal,
                "{status:?} keeps the kind glyph"
            );
        }
        assert_eq!(
            status_glyph(Icon::Terminal, Some(Failed.into())),
            Icon::Error
        );
        // Some steps failing among others that did not is its own mark.
        assert_eq!(
            status_glyph(Icon::Terminal, Some(Status::PartlyFailed)),
            Icon::PartlyFailed
        );
        // A failure is marked once, so not again in the trailing column.
        assert_eq!(Mark::of(Failed.into()), None);
        assert_eq!(Mark::of(Status::PartlyFailed), None);
        assert_eq!(Mark::of(Running.into()), Some(Mark::Spinning));
        assert_eq!(Mark::of(Status::Working), Some(Mark::Working));
        assert_eq!(Mark::of(Succeeded.into()), Some(Mark::Finished));
        // Stopping is not failing, and nor is ending unreported.
        assert_eq!(Mark::of(Stopped.into()), Some(Mark::Stopped));
        assert_eq!(Mark::of(Ended.into()), Some(Mark::Stopped));
        assert_eq!(
            Mark::of(Waiting.into()),
            Some(Mark::WaitingOnBackgroundWork)
        );
        assert_eq!(Mark::of(Denied.into()), Some(Mark::Denied));
    }

    /// The column's words reach the row's accessible name, so the mark is
    /// never the only way the state is told.
    #[test]
    fn a_rows_name_says_what_its_trailing_column_marks() {
        let trailing = Trailing {
            detail: None,
            mark: Some(Mark::Finished),
            elapsed: Some("3m 2s".into()),
        };
        assert_eq!(trailing.described(), " · finished · 3m 2s");
        let waiting = Trailing {
            detail: Some("waiting on scripts/restart-check".into()),
            mark: Some(Mark::WaitingOnBackgroundWork),
            elapsed: None,
        };
        assert_eq!(waiting.described(), " · waiting on scripts/restart-check");
        assert_eq!(Trailing::default().described(), "");
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

    /// Where a state's words give way to the name, its mark stays in
    /// their place, so a narrow pane still tells a finish from a stop.
    #[test]
    fn a_narrow_row_keeps_its_mark_when_its_words_give_way() {
        use iced::advanced::{Layout, layout, widget::Tree};
        fn marks(layout: Layout<'_>) -> usize {
            let bounds = layout.bounds();
            let size = f32::from(design::COMPACT_ICON_SIZE);
            usize::from(
                bounds.width == size && bounds.height == size && layout.children().count() > 0,
            ) + layout.children().map(marks).sum::<usize>()
        }
        let renderer = crate::app::tests::headless_renderer();
        let drawn = |width: f32| {
            let mut element = row_named(
                Headline::Name("Consolidate the directions".into()),
                "Agent \"Consolidate the directions\" waiting on scripts/check".into(),
                Icon::Chat,
                None,
                Trailing {
                    detail: Some("waiting on scripts/check".into()),
                    mark: Some(Mark::WaitingOnBackgroundWork),
                    elapsed: Some("6m 12s".into()),
                },
                // No disclosure arrow, whose glyph is the mark's size.
                None,
                None,
            );
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 80.0)),
            );
            marks(Layout::new(&node))
        };
        assert_eq!(drawn(160.0), 1, "narrow, the mark stays");
        assert_eq!(drawn(600.0), 1, "wide, the mark shows with its words");
    }
}
