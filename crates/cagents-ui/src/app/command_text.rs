//! Messages that name a command to run, written as cagents writes them: in
//! backticks. Each command is shown whole on its own line, in monospace, with
//! a control that copies it, rather than as backticked text that wraps in
//! the middle of the command.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Segment<'a> {
    Prose(&'a str),
    Command(&'a str),
}

/// `text` as the prose and the backticked commands it alternates between.
/// An unpaired or empty pair of backticks stays in the prose, and prose that
/// is only punctuation, as the full stop after a closing command is, is left
/// out.
pub(super) fn segments(text: &str) -> Vec<Segment<'_>> {
    fn prose<'a>(segments: &mut Vec<Segment<'a>>, text: &'a str) {
        let text = text.trim();
        if text.chars().any(char::is_alphanumeric) {
            segments.push(Segment::Prose(text));
        }
    }
    let mut segments = Vec::new();
    let mut rest = text;
    let mut start = 0;
    while let Some(open) = rest[start..].find('`').map(|at| start + at) {
        let Some(length) = rest[open + 1..].find('`') else {
            break;
        };
        let command = &rest[open + 1..open + 1 + length];
        if command.trim().is_empty() {
            start = open + 2 + length;
            continue;
        }
        prose(&mut segments, &rest[..open]);
        segments.push(Segment::Command(command.trim()));
        rest = &rest[open + 2 + length..];
        start = 0;
    }
    prose(&mut segments, rest);
    segments
}

/// `text`, with each command on its own unwrapped monospace line and the
/// `copy` control after it. Prose is drawn by `prose`, as it was before.
pub(super) fn view<'a, M: Clone + 'static>(
    text: &'a str,
    prose: impl Fn(&'a str) -> Element<'a, M>,
    copy: impl Fn(&'a str) -> Element<'a, M>,
) -> Element<'a, M> {
    let segments = segments(text);
    if !segments
        .iter()
        .any(|segment| matches!(segment, Segment::Command(_)))
    {
        return prose(text);
    }
    widget::column::with_children(segments.into_iter().map(|segment| {
        match segment {
            Segment::Prose(text) => prose(text),
            Segment::Command(command) => widget::row::with_children(vec![
                crate::interaction::overflow_fade_x(
                    widget::scrollable(
                        widget::selectable_text(command)
                            .font(iced::Font::MONOSPACE)
                            .size(design::TEXT_BODY)
                            .wrapping(cosmic::iced::widget::text::Wrapping::None),
                    )
                    .width(Length::Fill)
                    .direction(
                        iced::widget::scrollable::Direction::Horizontal(
                            iced::widget::scrollable::Scrollbar::hidden(),
                        ),
                    ),
                ),
                copy(command),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .into(),
        }
    }))
    .spacing(space::XS)
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::Segment::{Command, Prose};
    use super::*;

    #[test]
    fn a_command_is_set_apart_from_the_sentence_around_it() {
        assert_eq!(
            segments(
                "slow was reached over SSH, but its cagents daemon did not answer within 30 seconds. It may be busy or stuck: try again, or check it with `ssh slow '~/cagents daemon status'`."
            ),
            [
                Prose(
                    "slow was reached over SSH, but its cagents daemon did not answer within 30 seconds. It may be busy or stuck: try again, or check it with"
                ),
                Command("ssh slow '~/cagents daemon status'"),
            ]
        );
        assert_eq!(
            segments(
                "Check that `ssh box` connects without asking for a password, then try again."
            ),
            [
                Prose("Check that"),
                Command("ssh box"),
                Prose("connects without asking for a password, then try again."),
            ]
        );
        assert_eq!(
            segments("Run `a` then `b`."),
            [Prose("Run"), Command("a"), Prose("then"), Command("b")]
        );
    }

    #[test]
    fn text_without_a_whole_command_is_left_as_written() {
        for text in [
            "No command here.",
            "An unpaired ` backtick.",
            "Empty `` backticks.",
        ] {
            assert_eq!(segments(text), [Prose(text)]);
        }
    }
}
