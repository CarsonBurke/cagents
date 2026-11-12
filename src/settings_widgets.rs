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

    /// Label, control, error and description, at the fixed distances
    /// [`design::rhythm`] sets between those roles.
    fn view(self, query: &str) -> Element<'a, Message> {
        use design::rhythm;
        let gap = |height: f32| -> Element<'a, Message> {
            iced::widget::Space::new().height(height).into()
        };
        let mut children = Vec::new();
        if !self.label_in_control {
            children.push(if query.trim().is_empty() {
                design::heading(self.label).width(Length::Fill).into()
            } else {
                highlighted(
                    self.label.to_owned(),
                    query,
                    design::heading_font(),
                    design::TEXT_HEADING,
                    design::LINE_BODY,
                )
            });
            children.push(gap(rhythm::LABEL_TO_CONTROL));
        }
        children.push(self.control);
        // A switch carries its label on its own line, so what follows reads
        // as the label's description; under a control below its label it
        // reads as the control's.
        let after_control = if self.label_in_control {
            rhythm::LABEL_TO_DESCRIPTION
        } else {
            rhythm::LABEL_TO_CONTROL
        };
        if let Some(error) = self.error {
            children.push(gap(after_control));
            children.push(
                design::description(error)
                    .class(cosmic::theme::Text::Custom(design::destructive_text))
                    .into(),
            );
        }
        if !self.detail.is_empty() {
            children.push(gap(after_control));
            children.push(design::description(self.detail).into());
        }
        widget::column::with_children(children)
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
            // Title roles already use Bold. Underline distinguishes a match
            // without changing the role's font size or line box.
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
    untitled: bool,
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
        untitled: false,
    }
}

impl<'a, Message: Clone + 'a> Group<'a, Message> {
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    /// Where a search result sits: the category, then the group unless the
    /// group is named after it, which would read "Editor > Editor".
    pub fn breadcrumb(&self, category: &str) -> String {
        if self.title == category {
            category.to_owned()
        } else {
            format!("{category} > {}", self.title)
        }
    }

    /// Leaves the title and description to the page, for a page whose own
    /// title already names this, its only group. Returns the description,
    /// which the page sets under its title.
    pub fn untitled(&mut self) -> &'static str {
        self.untitled = true;
        self.detail
    }

    pub fn title(&self) -> &'static str {
        self.title
    }

    pub fn view(self, category: &str, query: &str) -> Option<Element<'a, Message>> {
        use design::rhythm;
        let breadcrumb = self.breadcrumb(category);
        let context = format!("{category} {} {}", self.title, self.detail);
        let settings: Vec<_> = self
            .settings
            .into_iter()
            .filter(|setting| setting.matches(&context, query))
            .map(|setting| setting.view(query))
            .collect();
        if settings.is_empty() {
            return None;
        }
        let searching = !query.trim().is_empty();
        let mut heading: Vec<Element<'a, Message>> = Vec::new();
        if searching {
            heading.push(highlighted(
                breadcrumb,
                query,
                design::title_font(),
                design::TEXT_TITLE,
                design::LINE_TITLE,
            ));
        } else if !self.untitled {
            heading.push(design::title(self.title).width(Length::Fill).into());
        }
        if !self.detail.is_empty() && (searching || !self.untitled) {
            if !heading.is_empty() {
                heading.push(
                    iced::widget::Space::new()
                        .height(rhythm::TITLE_TO_DESCRIPTION)
                        .into(),
                );
            }
            heading.push(design::description(self.detail).into());
        }
        let mut children = Vec::with_capacity(settings.len() + 1);
        if !heading.is_empty() {
            children.push(
                widget::column::with_children(heading)
                    .width(Length::Fill)
                    .into(),
            );
        }
        children.extend(settings);
        Some(
            widget::column::with_children(children)
                .spacing(if self.compact {
                    space::XS
                } else {
                    rhythm::ITEM_TO_ITEM
                })
                .width(Length::Fill)
                .into(),
        )
    }
}

/// A setting with one label beside its switch and supporting detail below.
pub fn toggle<'a, Message: Clone + 'static>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>>,
    value: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
) -> Setting<'a, Message> {
    toggle_enabled(label, detail, value, true, on_toggle)
}

pub fn toggle_enabled<'a, Message: Clone + 'static>(
    label: &'static str,
    detail: impl Into<Cow<'a, str>>,
    value: bool,
    enabled: bool,
    on_toggle: impl Fn(bool) -> Message + 'a,
) -> Setting<'a, Message> {
    // The switch's label is a setting label like any other, so it takes the
    // label role rather than the toggler's regular-weight default.
    let control = crate::ui_controls::field::toggle(value)
        .label(label.to_owned())
        .font(design::heading_font())
        .text_line_height(iced::widget::text::LineHeight::Absolute(
            design::LINE_BODY.into(),
        ))
        .spacing(space::M)
        .width(Length::Fill)
        .on_toggle_maybe(enabled.then_some(on_toggle))
        .into();
    field(label, detail, control).label_in_control()
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
        toggle(
            "Save files automatically",
            "Save files after a pause. Conflicting disk changes still require a decision.",
            auto_save,
            on_save,
        )
        .keywords("Auto-save auto_save"),
        toggle(
            "Wrap long lines",
            "Wrap long editor lines to the available width.",
            wrap,
            on_wrap,
        )
        .keywords("Word wrap word_wrap"),
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
    fn switch_settings_have_one_named_control_and_detail_at_both_widths() {
        use iced::advanced::{Layout, layout, mouse, widget::Tree};
        use iced_accessibility::accesskit::{Role, Toggled};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        for width in [220.0, 600.0] {
            for query in ["", "status"] {
                let mut setting = toggle(
                    "Show the status bar",
                    "See each signed-in account's remaining usage at the bottom of the window.",
                    true,
                    |_| (),
                )
                .view(query);
                let mut tree = Tree::new(&setting);
                let node = setting.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
                );
                assert_eq!(
                    node.children().len(),
                    3,
                    "one control row, the rhythm gap and its detail"
                );
                assert_eq!(
                    node.children()[1].size().height,
                    design::rhythm::LABEL_TO_DESCRIPTION
                );
                assert_eq!(node.size().width, width);
                let switch = &node.children()[0];
                let label = &switch.children()[0];
                let control = &switch.children()[1];
                assert!(label.bounds().x + label.size().width <= control.bounds().x);
                assert!(control.bounds().x + control.size().width <= width);

                let accessible = setting.as_widget().a11y_nodes(
                    Layout::new(&node),
                    &tree,
                    mouse::Cursor::Unavailable,
                );
                let named: Vec<_> = accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .filter(|node| node.node().label() == Some("Show the status bar"))
                    .collect();
                assert_eq!(named.len(), 1, "the label is announced once");
                assert_eq!(named[0].node().role(), Role::Switch);
                assert_eq!(named[0].node().toggled(), Some(Toggled::True));
            }
        }
    }

    #[test]
    fn combined_editor_switches_keep_familiar_search_terms() {
        let settings = editor_options(
            false,
            false,
            EditorSyntaxTheme::default(),
            |_| (),
            |_| (),
            |_| (),
        );
        for (index, queries) in [
            (0, ["auto-save", "auto_save", "Save files automatically"]),
            (1, ["word wrap", "word_wrap", "Wrap long lines"]),
        ] {
            for query in queries {
                assert!(settings[index].matches("Editor", query), "{query}");
            }
        }
    }

    #[test]
    fn settings_search_preserves_label_and_group_title_line_boxes() {
        use iced::advanced::{layout, widget::Tree};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        for query in ["", "Font"] {
            let mut setting =
                field::<()>("Font", "", widget::Space::new().width(1).height(1).into()).view(query);
            let mut tree = Tree::new(&setting);
            let node = setting.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 600.0)),
            );
            assert_eq!(node.children()[0].size().height, design::LINE_BODY);

            let mut group = group(
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
            let mut tree = Tree::new(&group);
            let node = group.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 600.0)),
            );
            assert_eq!(
                node.children()[0].children()[0].size().height,
                design::LINE_TITLE,
            );
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
                label,
            );
            assert_eq!(
                spans
                    .iter()
                    .filter(|span| span.underline)
                    .map(|span| span.text.as_ref())
                    .collect::<String>(),
                matched,
            );
            assert!(
                spans.iter().any(|span| !span.underline),
                "context outside the match keeps its ordinary role",
            );
            assert!(spans.iter().all(|span| span.font == Some(font)));
            assert!(
                highlighted_spans(label, "", font)
                    .iter()
                    .all(|span| !span.underline),
            );
        }
    }

    #[test]
    fn search_indexes_keys_values_captions_and_current_shortcuts() {
        let cases = [
            (
                "Pane focus",
                "focus_follows_mouse hover",
                "focus_follows_mouse",
            ),
            ("Color theme", "dracula nord", "dracula"),
            ("Pane focus", "focus_follows_mouse hover", "hover"),
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
        let font = group::<()>("Font", "", vec![]);
        assert_eq!(font.breadcrumb("Editor"), "Editor > Font");
        // A group named after its category is not named twice.
        let named_after = group::<()>("Editor", "", vec![]);
        assert_eq!(named_after.breadcrumb("Editor"), "Editor");
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
