//! Messages that end by naming a command to run, written as cagents writes
//! them: in backticks, as in "check it with `ssh box cagents daemon status`".
//! The command is shown whole on its own line, in monospace, with a control
//! that copies it, rather than as backticked text that wraps in the middle.
//!
//! Only a closing command is set apart. Backticks elsewhere also quote paths,
//! flags and values inside a sentence, which read best where they are.

use super::*;

/// The command a message ends with, and the sentence that leads up to it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Trailing<'a> {
    /// What comes before the command, ending in a colon when it ended in
    /// nothing, or `None` when the command is all there is.
    pub prose: Option<String>,
    pub command: &'a str,
}

/// The command `text` closes with, if it does: a backticked span, followed
/// by nothing but punctuation, that reads as a command and its arguments
/// rather than a lone path, flag or value.
pub(super) fn trailing_command(text: &str) -> Option<Trailing<'_>> {
    let end = text
        .trim_end_matches(|character: char| character.is_whitespace() || ".;!".contains(character));
    let quoted = end.strip_suffix('`')?;
    let open = quoted.rfind('`')?;
    let command = quoted[open + 1..].trim();
    let mut words = command.split_whitespace();
    let program = words.next()?;
    if words.next().is_none() || !program.starts_with(|first: char| first.is_ascii_alphanumeric()) {
        return None;
    }
    let before = quoted[..open].trim_end();
    let prose = (!before.is_empty()).then(|| {
        if before.ends_with(|last: char| last.is_ascii_punctuation()) {
            before.to_owned()
        } else {
            format!("{before}:")
        }
    });
    Some(Trailing { prose, command })
}

/// `command` on one unwrapped monospace line with the `copy` control after
/// it. One too long for the line scrolls, its cut end faded into `surface`,
/// the colour it sits on; the copy control takes it whole.
pub(super) fn command_row<'a, M: Clone + 'static>(
    command: &'a str,
    copy: Element<'a, M>,
    surface: impl Fn(&cosmic::Theme) -> iced::Color + 'a,
) -> Element<'a, M> {
    widget::row::with_children(vec![
        crate::interaction::overflow_fade_x_over(
            widget::scrollable(
                widget::selectable_text(command)
                    .font(iced::Font::MONOSPACE)
                    .size(design::TEXT_CAPTION)
                    .wrapping(cosmic::iced::widget::text::Wrapping::None),
            )
            .width(Length::Fill)
            .direction(iced::widget::scrollable::Direction::Horizontal(
                iced::widget::scrollable::Scrollbar::hidden(),
            )),
            surface,
        ),
        copy,
    ])
    .spacing(space::XS)
    .align_y(Alignment::Center)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closing_command_is_set_apart_from_the_sentence_before_it() {
        assert_eq!(
            trailing_command(
                "slow was reached over SSH, but its cagents daemon did not answer within 30 seconds. It may be busy or stuck: try again, or check it with `ssh slow '~/cagents daemon status'`."
            ),
            Some(Trailing {
                prose: Some(
                    "slow was reached over SSH, but its cagents daemon did not answer within 30 seconds. It may be busy or stuck: try again, or check it with:".into()
                ),
                command: "ssh slow '~/cagents daemon status'",
            })
        );
        // Prose that already ends in punctuation keeps it.
        assert_eq!(
            trailing_command("Run this: `cagents daemon start`"),
            Some(Trailing {
                prose: Some("Run this:".into()),
                command: "cagents daemon start",
            })
        );
        assert_eq!(
            trailing_command("`cagents daemon start`"),
            Some(Trailing {
                prose: None,
                command: "cagents daemon start",
            })
        );
    }

    #[test]
    fn quoted_paths_values_and_commands_inside_a_sentence_stay_where_they_are() {
        for text in [
            "No command here.",
            "Check that `ssh box` connects without asking for a password, then try again.",
            "the checkout `/home/me/repo` is not a worktree of the repository `/home/me/other`",
            "SQLite journal mode is `wal2`",
            "Pass `--to CHANNEL` or `--new TITLE`",
            "An unpaired ` backtick.",
            "Empty `` backticks.",
            "Ends with a lone path `~/projects/my repo`.",
        ] {
            assert_eq!(trailing_command(text), None, "{text}");
        }
    }
}
