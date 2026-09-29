//! Shared settings rows and search metadata for the workbench and editor.
use crate::{design, settings::EditorSyntaxTheme};
use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};
use std::borrow::Cow;

pub struct Setting<'a, Message> {
    label: &'static str,
    detail: Cow<'a, str>,
    control: Element<'a, Message>,
    error: Option<&'a str>,
}

impl<'a, Message: Clone + 'a> Setting<'a, Message> {
    /// Reports why the value last entered was not saved, directly under the
    /// control that holds it.
    pub fn error(mut self, error: Option<&'a str>) -> Self {
        self.error = error;
        self
    }

    fn matches(&self, context: &str, query: &str) -> bool {
        matches(&format!("{context} {} {}", self.label, self.detail), query)
    }

    fn view(self) -> Element<'a, Message> {
        let mut children = vec![
            widget::text::caption_heading(self.label).into(),
            self.control,
        ];
        if let Some(error) = self.error {
            children.push(
                design::caption(error)
                    .class(cosmic::theme::Text::Custom(design::destructive_text))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            );
        }
        children.push(
            design::caption(self.detail)
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into(),
        );
        widget::column::with_children(children)
            .spacing(design::GRID_UNIT)
            .width(Length::Fill)
            .into()
    }
}

pub fn matches(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query
        .split_whitespace()
        .all(|word| text.contains(&word.to_lowercase()))
}

pub fn field<'a, Message: Clone + 'a>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>>,
    control: Element<'a, Message>,
) -> Setting<'a, Message> {
    Setting {
        label,
        detail: detail.into(),
        control,
        error: None,
    }
}

pub struct Group<'a, Message> {
    title: &'static str,
    detail: &'static str,
    settings: Vec<Setting<'a, Message>>,
}

pub fn group<'a, Message>(
    title: &'static str,
    detail: &'static str,
    settings: Vec<Setting<'a, Message>>,
) -> Group<'a, Message> {
    Group {
        title,
        detail,
        settings,
    }
}

impl<'a, Message: Clone + 'a> Group<'a, Message> {
    pub fn view(self, category: &str, query: &str) -> Option<Element<'a, Message>> {
        let context = format!("{category} {} {}", self.title, self.detail);
        let mut settings: Vec<_> = self
            .settings
            .into_iter()
            .filter(|setting| setting.matches(&context, query))
            .map(Setting::view)
            .collect();
        if settings.is_empty() {
            return None;
        }
        let mut children = vec![
            widget::column::with_children(vec![
                widget::text::title4(self.title).into(),
                design::caption(self.detail)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(design::GRID_UNIT)
            .into(),
        ];
        children.append(&mut settings);
        Some(
            widget::column::with_children(children)
                .spacing(4.0 * design::GRID_UNIT)
                .width(Length::Fill)
                .into(),
        )
    }
}

/// A labelled switch that spans its setting row: the label reads from the
/// left edge and the switch sits at the right, clear of any label length.
pub fn toggle<'a, Message: 'static>(
    label: &str,
    value: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    widget::toggler(value)
        .label(label.to_owned())
        .spacing(3.0 * design::GRID_UNIT)
        .width(Length::Fill)
        .on_toggle(on_toggle)
        .into()
}

pub fn editor_options<'a, Message: Clone + 'static>(
    auto_save: bool,
    wrap: bool,
    syntax: EditorSyntaxTheme,
    on_save: impl Fn(bool) -> Message + 'a,
    on_wrap: impl Fn(bool) -> Message + 'a,
    on_syntax: impl Fn(EditorSyntaxTheme) -> Message + Send + Sync + 'static,
) -> Vec<Setting<'a, Message>> {
    vec![
        field(
            "Auto-save",
            "Save files after a pause. Conflicting disk changes still require a decision.",
            toggle("Save files automatically", auto_save, on_save),
        ),
        field(
            "Word wrap",
            "Wrap long editor lines to the available width.",
            toggle("Wrap long lines", wrap, on_wrap),
        ),
        field(
            "Syntax theme",
            "Syntax highlighting in editors and diffs. Match app follows the application appearance theme.",
            widget::dropdown(
                EditorSyntaxTheme::ALL
                    .iter()
                    .map(|theme| theme.label().to_owned())
                    .collect::<Vec<_>>(),
                EditorSyntaxTheme::ALL
                    .iter()
                    .position(|theme| *theme == syntax),
                move |index| on_syntax(EditorSyntaxTheme::ALL[index]),
            )
            .into(),
        ),
    ]
}

pub fn search_id() -> widget::Id {
    widget::Id::new("settings.search")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_matches_all_words_without_case_sensitivity() {
        assert!(matches("Terminal font family", "FONT terminal"));
        assert!(!matches("Terminal font family", "font syntax"));
    }
    #[test]
    fn search_combines_category_and_setting_terms() {
        let controls = editor_options(
            false,
            false,
            EditorSyntaxTheme::default(),
            |_| (),
            |_| (),
            |_| (),
        );
        assert!(
            group("Editing", "Files", controls)
                .view("Editor", "editor save")
                .is_some()
        );
        let controls = editor_options(
            false,
            false,
            EditorSyntaxTheme::default(),
            |_| (),
            |_| (),
            |_| (),
        );
        assert!(
            group("Editing", "Files", controls)
                .view("Editor", "editor terminal")
                .is_none()
        );
    }
}
