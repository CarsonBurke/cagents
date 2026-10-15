//! Shared settings rows and search metadata for the workbench and editor.
use crate::{
    design::{self, space},
    settings::EditorSyntaxTheme,
};
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
    label_in_control: bool,
    keywords: String,
}

impl<'a, Message: Clone + 'a> Setting<'a, Message> {
    /// Keep the label searchable when the native control already displays it.
    pub fn label_in_control(mut self) -> Self {
        self.label_in_control = true;
        self
    }

    /// Reports why the value last entered was not saved, directly under the
    /// control that holds it.
    pub fn error(mut self, error: Option<&'a str>) -> Self {
        self.error = error;
        self
    }

    /// Searchable TOML keys, choice labels, captions and current values.
    pub fn keywords(mut self, keywords: impl AsRef<str>) -> Self {
        self.keywords.push(' ');
        self.keywords.push_str(keywords.as_ref());
        self
    }

    fn matches(&self, context: &str, query: &str) -> bool {
        matches(&format!("{context} {} {} {}", self.label, self.detail, self.keywords), query)
    }

    fn view(self, query: &str) -> Element<'a, Message> {
        let mut children = Vec::new();
        if !self.label_in_control {
            children.push(highlighted(self.label.to_owned(), query, design::heading_font()));
        }
        children.push(self.control);
        if !query.trim().is_empty() {
            let terms: Vec<_> = self.keywords.split_whitespace().filter(|term| {
                query.split_whitespace().any(|word| term.to_lowercase().contains(&word.to_lowercase()))
            }).collect();
            if !terms.is_empty() {
                children.push(highlighted(format!("Matches: {}", terms.join(" · ")), query, cosmic::font::default()));
            }
        }
        if let Some(error) = self.error {
            children.push(
                design::caption(error)
                    .class(cosmic::theme::Text::Custom(design::destructive_text))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            );
        }
        if !self.detail.is_empty() { children.push(
            highlighted(self.detail.into_owned(), query, cosmic::font::default()),
        );
        }
        widget::column::with_children(children)
            .spacing(space::XS)
            .width(Length::Fill)
            .into()
    }
}

/// Build spans at original UTF-8 boundaries, including non-ASCII text.
fn highlighted<'a, Message: 'a>(text: String, query: &str, font: iced::Font) -> Element<'a, Message> {
    let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    let max_chars = words.iter().map(|word| word.chars().count()).max().unwrap_or(0);
    // Ordinary settings views need a single shaping run, not one rich span per
    // character. Search results also coalesce adjacent equal emphasis below.
    if words.is_empty() {
        let span: iced::widget::text::Span<'_, ()> = iced::widget::text::Span::new(text).font(font);
        return iced::widget::rich_text([span])
            .size(design::TEXT_BODY)
            .line_height(iced::widget::text::LineHeight::Absolute(design::LINE_BODY.into()))
            .width(Length::Fill).into();
    }
    let mut spans = Vec::new();
    let boundaries: Vec<_> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
    let mut highlighted = vec![false; text.len()];
    for (offset, &start) in boundaries.iter().enumerate() {
        for &end in boundaries[offset + 1..].iter().take(max_chars) {
            if words.iter().any(|word| text[start..end].to_lowercase() == *word) {
                highlighted[start..end].fill(true);
            }
        }
    }
    let mut run_start = 0;
    for &end in boundaries.iter().skip(1) {
        if end != text.len() && highlighted[end] == highlighted[run_start] { continue; }
        let span: iced::widget::text::Span<'_, ()> = iced::widget::text::Span::new(text[run_start..end].to_owned());
        spans.push(if highlighted[run_start] {
            span.font(iced::Font { weight: iced::font::Weight::Bold, ..font })
        } else { span.font(font) });
        run_start = end;
    }
    iced::widget::rich_text(spans)
        .size(design::TEXT_BODY)
        .line_height(iced::widget::text::LineHeight::Absolute(design::LINE_BODY.into()))
        .width(Length::Fill).into()
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
        label_in_control: false,
        keywords: label.to_lowercase().replace([' ', '-'], "_"),
    }
}

pub struct Group<'a, Message> {
    title: &'static str,
    detail: &'static str,
    settings: Vec<Setting<'a, Message>>,
    compact: bool,
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
        compact: false,
    }
}

impl<'a, Message: Clone + 'a> Group<'a, Message> {
    pub fn compact(mut self) -> Self { self.compact = true; self }

    pub fn breadcrumb(&self, category: &str) -> String {
        format!("{category} > {}", self.title)
    }

    pub fn view(self, category: &str, query: &str) -> Option<Element<'a, Message>> {
        let breadcrumb = self.breadcrumb(category);
        let context = format!("{category} {} {}", self.title, self.detail);
        let mut settings: Vec<_> = self
            .settings
            .into_iter()
            .filter(|setting| setting.matches(&context, query))
            .map(|setting| setting.view(query))
            .collect();
        if settings.is_empty() {
            return None;
        }
        let mut heading: Vec<Element<'a, Message>> = vec![
            if query.trim().is_empty() { design::title(self.title).into() }
            else { highlighted(breadcrumb, query, design::heading_font()) }
        ];
        if !self.detail.is_empty() {
            heading.push(
                design::caption(self.detail)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            );
        }
        let mut children = vec![
            widget::column::with_children(heading)
                .spacing(space::XS)
                .into(),
        ];
        children.append(&mut settings);
        Some(
            widget::column::with_children(children)
                .spacing(if self.compact { space::XS } else { space::L })
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
    toggle_enabled(label, value, true, on_toggle)
}

pub fn toggle_enabled<'a, Message: 'static>(
    label: &str,
    value: bool,
    enabled: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
) -> Element<'a, Message> {
    crate::ui_controls::field::toggle(value)
        .label(label.to_owned())
        .spacing(space::M)
        .width(Length::Fill)
        .on_toggle_maybe(enabled.then_some(on_toggle))
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
            crate::ui_controls::field::select(
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

/// Content identity shared by the ADE settings pane and standalone editor.
pub fn content_id() -> widget::Id {
    widget::Id::new("settings.content")
}

/// Category and query changes reveal the beginning of their new results.
pub fn reveal_content_start<Message: Send + 'static>() -> iced::Task<Message> {
    iced::widget::operation::scroll_to(
        content_id(),
        iced::widget::operation::AbsoluteOffset { x: 0.0, y: 0.0 },
    )
}

pub fn search_id() -> widget::Id {
    widget::Id::new("settings.search")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_indexes_keys_values_captions_and_current_shortcuts() {
        let cases = [
            ("Pane activation", "focus_follows_mouse hover", "focus_follows_mouse"),
            ("Color theme", "dracula nord", "dracula"),
            ("Pane activation", "focus_follows_mouse hover", "hover"),
            ("New chat", "keyboard_shortcuts.new_native_thread Ctrl+T", "ctrl+t"),
        ];
        for (label, keywords, query) in cases {
            let setting = field::<()>(label, "", widget::text("").into()).keywords(keywords);
            assert!(setting.matches("General > Workspace", query), "{query}");
        }
    }

    #[test]
    fn results_carry_category_and_group() {
        let group = group::<()>("Font", "", vec![]);
        assert_eq!(group.breadcrumb("Editor"), "Editor > Font");
    }

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
