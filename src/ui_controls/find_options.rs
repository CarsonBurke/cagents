//! Shared matching-option controls for editor find and worktree search.
use crate::design::space;
use cosmic::{Element, iced::Alignment, widget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindOption {
    CaseSensitive,
    Regex,
    WholeWord,
}

pub fn find_options<'a, Message: Clone + 'static>(
    case_sensitive: bool,
    regex: bool,
    whole_word: bool,
    enabled: bool,
    on_toggle: impl Fn(FindOption, bool) -> Message + Copy + 'a,
) -> Element<'a, Message> {
    widget::row::with_children(
        [
            (
                FindOption::CaseSensitive,
                case_sensitive,
                "Aa",
                "Match case",
            ),
            (
                FindOption::WholeWord,
                whole_word,
                "Word",
                "Match whole word",
            ),
            (FindOption::Regex, regex, ".*", "Use regular expression"),
        ]
        .into_iter()
        .map(|(option, selected, label, description)| {
            let mut control = super::field::checkbox(selected).label(label);
            if enabled {
                control = control.on_toggle(move |value| on_toggle(option, value));
            }
            crate::interaction::tooltip(control, description).into()
        }),
    )
    .spacing(space::S)
    .align_y(Alignment::Center)
    .into()
}
