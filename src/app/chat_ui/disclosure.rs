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
        Some(chat::ToolStatus::Stopped | chat::ToolStatus::Ended) => Icon::Cancel,
        Some(chat::ToolStatus::Waiting) => Icon::AgentBackground,
        _ => kind,
    }
}

/// One disclosure row. Its accessible name carries the trailing metadata,
/// which is drawn in a cell of its own.
pub(super) fn row(
    label: String,
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
    label: String,
    name: String,
    icon: Icon,
    status: Option<chat::ToolStatus>,
    trailing: Option<String>,
    expanded: Option<bool>,
    toggle: Option<Action>,
) -> Element<'static, Message> {
    // The fade is in the glyphs themselves, so it is right on the row's
    // resting, hovered, pressed and focused paint alike.
    let text = crate::ui_controls::fade_text(label)
        .class(cosmic::theme::Text::Custom(design::muted_text))
        .width(Length::Fill)
        .into();
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
        assert_eq!(
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Stopped)),
            Icon::Cancel
        );
        assert_eq!(
            status_glyph(Icon::Terminal, Some(chat::ToolStatus::Ended)),
            Icon::Cancel
        );
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
                "Thinking".into(),
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
