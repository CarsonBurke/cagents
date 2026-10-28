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
        matches(
            &format!("{context} {} {} {}", self.label, self.detail, self.keywords),
            query,
        )
    }

    fn view(self, query: &str) -> Element<'a, Message> {
        let mut children = Vec::new();
        if !self.label_in_control {
            children.push(if query.trim().is_empty() {
                design::heading(self.label).into()
            } else {
                highlighted(
                    self.label.to_owned(),
                    query,
                    design::heading_font(),
                    design::interface_size(),
                    design::interface_size() + 5.0,
                )
            });
        }
        children.push(self.control);
        if let Some(error) = self.error {
            children.push(
                design::caption(error)
                    .class(cosmic::theme::Text::Custom(design::destructive_text))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            );
        }
        if !self.detail.is_empty() {
            children.push(
                design::caption(self.detail)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .width(Length::Fill)
                    .into(),
            );
        }
        widget::column::with_children(children)
            .spacing(space::XS)
            .width(Length::Fill)
            .into()
    }
}

/// Build spans at original UTF-8 boundaries, including non-ASCII text.
fn highlighted<'a, Message: 'a>(
    text: String,
    query: &str,
    font: iced::Font,
    size: f32,
    line_height: f32,
) -> Element<'a, Message> {
    iced::widget::rich_text(highlighted_spans(&text, query, font))
        .size(size)
        .font(font)
        .line_height(iced::widget::text::LineHeight::Absolute(line_height.into()))
        .width(Length::Fill)
        .into()
}

/// The native rich-text builder consumes these exact render properties.
fn highlighted_spans(
    text: &str,
    query: &str,
    font: iced::Font,
) -> Vec<iced::widget::text::Span<'static, ()>> {
    let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut spans = Vec::new();
    let boundaries: Vec<_> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let mut highlighted = vec![false; text.len()];
    for (offset, &start) in boundaries.iter().enumerate() {
        for &end in &boundaries[offset + 1..] {
            if words
                .iter()
                .any(|word| text[start..end].to_lowercase() == *word)
            {
                highlighted[start..end].fill(true);
            }
        }
    }
    for range in boundaries.windows(2) {
        let span: iced::widget::text::Span<'_, ()> =
            iced::widget::text::Span::new(text[range[0]..range[1]].to_owned());
        spans.push(if highlighted[range[0]] {
            // Title roles already use Bold. Underline provides an actual
            // native paint distinction without changing the role's metrics.
            span.font(iced::Font {
                weight: iced::font::Weight::Bold,
                ..font
            })
            .underline(true)
        } else {
            span.font(font)
        });
    }
    spans
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
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

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
        let mut heading: Vec<Element<'a, Message>> = vec![if query.trim().is_empty() {
            design::title(self.title).into()
        } else {
            highlighted(
                breadcrumb,
                query,
                design::title_font(),
                design::scaled_interface_size(design::TEXT_TITLE),
                design::scaled_interface_size(design::LINE_TITLE),
            )
        }];
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
    fn labels_and_search_highlights_follow_interface_type_size() {
        use iced::advanced::{layout, widget::Tree};
        let _restore = design::typography_test_guard();
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let height = |query| {
            let mut label: Element<'_, ()> = highlighted(
                "Interface font".into(),
                query,
                design::heading_font(),
                design::interface_size(),
                design::interface_size() + 5.0,
            );
            let mut tree = Tree::new(label.as_widget());
            label
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(300.0, 100.0)),
                )
                .size()
                .height
        };
        design::configure_typography("", 13.0, "", 14.0);
        let small = [height(""), height("font")];
        design::configure_typography("", 24.0, "", 14.0);
        for (small, large) in small.into_iter().zip([height(""), height("font")]) {
            assert!(large > small);
            assert_eq!(large, 29.0);
        }
        design::configure_typography("", 13.0, "", 14.0);
    }

    #[test]
    fn settings_search_keeps_dynamic_heading_and_title_roles() {
        use iced::advanced::{layout, widget::Tree};
        let _restore = design::typography_test_guard();
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 600.0));
        let measure = |mut element: Element<'_, ()>| {
            let mut tree = Tree::new(element.as_widget());
            element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits)
                .size()
                .height
        };
        for size in [13.0, 24.0] {
            design::configure_typography("", size, "", 14.0);
            // Compare actual stock role widgets rather than duplicating their
            // scale formulas in the expected geometry.
            let heading_height = measure(design::heading("Font").into());
            let title_height = measure(design::title("Editor > Font").into());
            assert!(title_height > heading_height);
            for query in ["", "Font"] {
                let mut setting =
                    field::<()>("Font", "", widget::Space::new().width(1).height(1).into())
                        .view(query);
                let mut tree = Tree::new(setting.as_widget());
                let node = setting
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                assert_eq!(
                    node.children()[0].size().height,
                    heading_height,
                    "field label keeps its heading role at {size}px/query={query}"
                );
                let mut element = group(
                    "Font",
                    "",
                    vec![field::<()>(
                        "Font",
                        "",
                        widget::Space::new().width(1).height(1).into(),
                    )],
                )
                .view("Editor", query)
                .unwrap();
                let mut tree = Tree::new(element.as_widget());
                let node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                assert_eq!(
                    node.children()[0].children()[0].size().height,
                    title_height,
                    "group breadcrumb keeps its title role at {size}px/query={query}"
                );
            }
        }
    }

    #[test]
    fn search_matches_remain_visible_when_the_title_is_already_bold() {
        for (label, query, matched) in [
            ("Editor > Font", "editor", "Editor"),
            ("Editor > naïve fonts", "NAÏVE", "naïve"),
        ] {
            let font = design::title_font();
            assert_eq!(font.weight, iced::font::Weight::Bold);
            let spans = highlighted_spans(label, query, font);
            assert_eq!(
                spans
                    .iter()
                    .map(|span| span.text.as_ref())
                    .collect::<String>(),
                label
            );
            assert_eq!(
                spans
                    .iter()
                    .filter(|span| span.underline)
                    .map(|span| span.text.as_ref())
                    .collect::<String>(),
                matched
            );
            assert!(
                spans.iter().any(|span| !span.underline),
                "context outside the match keeps its ordinary role"
            );
            // Weight alone was identical for both sets of spans; the native
            // Rich widget draws underline quads from this actual property.
            assert!(spans.iter().all(|span| span.font == Some(font)));
            assert!(
                highlighted_spans(label, "", font)
                    .iter()
                    .all(|span| !span.underline)
            );
        }
    }

    #[test]
    fn search_indexes_keys_values_captions_and_current_shortcuts() {
        let cases = [
            (
                "Pane activation",
                "focus_follows_mouse hover",
                "focus_follows_mouse",
            ),
            ("Color theme", "dracula nord", "dracula"),
            ("Pane activation", "focus_follows_mouse hover", "hover"),
            (
                "New chat",
                "keyboard_shortcuts.new_native_thread Ctrl+T",
                "ctrl+t",
            ),
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
