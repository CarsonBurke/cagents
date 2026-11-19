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

fn status_glyph(kind: Icon, status: Option<chat::ToolStatus>) -> Icon {
    match status {
        Some(chat::ToolStatus::Failed) => Icon::Error,
        Some(chat::ToolStatus::Denied) => Icon::Unavailable,
        // Work that was stopped, or that ended without saying how it went,
        // has not failed. Its mark is a still one, unlike the Stop control.
        Some(chat::ToolStatus::Stopped | chat::ToolStatus::Ended) => Icon::Ended,
        Some(chat::ToolStatus::Waiting) => Icon::AgentBackground,
        _ => kind,
    }
}

/// What a row says. A path is told apart from its neighbours by its last
/// components, so a row that names one keeps those in view and lets the
/// start of the path give way instead, behind a verb that always shows.
pub(super) enum Headline {
    Text(String),
    Path { verb: String, path: String },
}

impl From<String> for Headline {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl Headline {
    fn view(self) -> Element<'static, Message> {
        let line = |text: String| {
            crate::ui_controls::fade_text(text)
                .class(cosmic::theme::Text::Custom(design::muted_text))
        };
        match self {
            // The fade is in the glyphs themselves, so it is right on the
            // row's resting, hovered, pressed and focused paint alike.
            Self::Text(text) => line(text).width(Length::Fill).into(),
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

/// One disclosure row. Its accessible name carries the trailing metadata,
/// which is drawn in a cell of its own.
pub(super) fn row(
    label: impl Into<Headline>,
    name: String,
    icon: Icon,
    status: Option<chat::ToolStatus>,
    trailing: Option<String>,
    expanded: Option<bool>,
    toggle: Option<Action>,
) -> Element<'static, Message> {
    let name = match &trailing {
        Some(trailing) => format!("{name} · {trailing}"),
        None => name,
    };
    row_named(label, name, icon, status, trailing, expanded, toggle)
}

/// A row whose accessible name already includes its trailing metadata.
/// Delivered reports retain their canonical full heading even when the title
/// and outcome are drawn in separate cells.
pub(super) fn row_named(
    label: impl Into<Headline>,
    name: String,
    icon: Icon,
    status: Option<chat::ToolStatus>,
    trailing: Option<String>,
    expanded: Option<bool>,
    toggle: Option<Action>,
) -> Element<'static, Message> {
    let text = label.into().view();
    let indicator = match status {
        Some(chat::ToolStatus::Running) => crate::ui_controls::activity_spinner(
            f32::from(design::COMPACT_ICON_SIZE),
            design::muted_color,
        ),
        Some(chat::ToolStatus::Failed) => widget::icon(Icon::Error.handle())
            .size(design::COMPACT_ICON_SIZE)
            .class(cosmic::theme::Svg::Custom(Rc::new(|theme| {
                iced::widget::svg::Style {
                    color: Some(design::destructive_color(theme)),
                }
            })))
            .into(),
        _ => {
            crate::ui_controls::passive_icon(status_glyph(icon, status), design::COMPACT_ICON_SIZE)
        }
    };
    let mut cells = vec![indicator, text];
    if let Some(trailing) = trailing {
        cells.push(
            design::caption(trailing)
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        );
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
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Succeeded)),
            Icon::Terminal
        );
        assert_eq!(
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Denied)),
            Icon::Unavailable
        );
        // Stopping is not failing, and nor is ending unreported. Neither is
        // drawn as the Stop control, which reads as something to press.
        for status in [chat::ToolStatus::Stopped, chat::ToolStatus::Ended] {
            assert_eq!(status_glyph(Icon::Terminal, Some(status)), Icon::Ended);
        }
        assert_eq!(
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Waiting)),
            Icon::AgentBackground
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
                Icon::Agent,
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
}
