//! Single-line text entry in the shell's own shape.
//!
//! COSMIC's inputs are tall, heavily bordered and, for search, fully rounded.
//! A field here is one dense row high with the same corner as the buttons
//! beside it and a hairline edge, so a form reads as one column of controls.

#[cfg(any(test, feature = "test-support"))]
pub use super::editable::padding_fade_geometry;
pub use super::editable::{TextEditor, TextInput};
use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use super::Icon;
use crate::design::{self, space};

/// The line box of a field's text, which with [`INSET_Y`] makes the field one
/// dense row high.
const LINE_HEIGHT: f32 = 18.0;
const INSET_Y: f32 = (design::DENSE_ROW_HEIGHT - LINE_HEIGHT) / 2.0;

/// A single-line text field.
///
/// Wraps COSMIC's input with editable accessibility semantics while retaining
/// its normal input behavior and the shell's field builder methods.
pub fn text<'a, Message: Clone + 'static>(
    placeholder: impl Into<Cow<'a, str>>,
    value: impl Into<Cow<'a, str>>,
) -> TextInput<'a, Message> {
    TextInput::new(placeholder, value)
        .size(design::TEXT_BODY)
        .line_height(iced::widget::text::LineHeight::Absolute(LINE_HEIGHT.into()))
        .padding([INSET_Y, space::S])
        .style(style())
}

/// A field that filters or finds: a [`text`] field led by the search glyph.
///
/// The glyph sits in the list grid's icon column and the text starts on its
/// label column, so a search field above a list lines up with the rows it
/// filters; see [`design::list`].
pub fn search<'a, Message: Clone + 'static>(
    placeholder: impl Into<Cow<'a, str>>,
    value: impl Into<Cow<'a, str>>,
) -> TextInput<'a, Message> {
    text(placeholder, value)
        .padding([INSET_Y, design::list::INSET])
        .leading_icon(
            widget::container(widget::icon(Icon::Search.handle()).size(design::ROW_ICON_SIZE))
                .class(design::transparent())
                .center_x(Length::Fixed(design::list::ICON))
                .into(),
        )
}

/// The trailing control that empties a field, for `.trailing_icon(..)`.
///
/// COSMIC's own `on_clear` button is taller than a dense row and a field
/// grows to fit its trailing icon, so attaching it made a search field jump
/// in height on the first keystroke. This one fits inside the line box.
pub fn clear<'a, Message: Clone + 'a>(on_clear: Message) -> Element<'a, Message> {
    widget::button::custom(
        widget::container(widget::icon(Icon::Clear.handle()).size(design::COMPACT_ICON_SIZE))
            .class(design::transparent())
            .center(Length::Fill),
    )
    .name("Clear")
    .class(super::control_style(false, None))
    .width(LINE_HEIGHT)
    .height(LINE_HEIGHT)
    .padding(0)
    .on_press(on_clear)
    .into()
}

/// A choice among a few named options, at the shell's type size.
pub fn select<'a, S, Message>(
    options: impl Into<Cow<'a, [S]>>,
    selected: Option<usize>,
    on_select: impl Fn(usize) -> Message + Send + Sync + 'static,
) -> widget::Dropdown<'a, S, Message, Message>
where
    S: AsRef<str> + Clone + Send + Sync + 'static,
    [S]: ToOwned,
    Message: Clone + 'static,
{
    widget::dropdown(options, selected, on_select)
        .text_size(design::TEXT_BODY)
        .padding([INSET_Y, space::S])
}

/// A labelled yes/no choice. Add the label with `.label(..)`.
pub fn checkbox<'a, Message>(checked: bool) -> widget::Checkbox<'a, Message, cosmic::Theme> {
    widget::checkbox(checked)
        .size(design::ROW_ICON_SIZE)
        .text_size(design::TEXT_BODY)
        .spacing(space::S)
}

/// A setting that is on or off and takes effect at once. Add the label with
/// `.label(..)`.
pub fn toggle<'a, Message>(on: bool) -> widget::Toggler<'a, Message> {
    widget::toggler(on)
        .size(design::STATUS_ICON_SIZE)
        .text_size(design::TEXT_BODY)
        .spacing(space::M)
}

/// What a field is doing, which sets its edge and fill.
#[derive(Clone, Copy)]
enum Status {
    Active,
    Hovered,
    Focused,
    Error,
    Disabled,
}

/// A field's look in `status`: a well in the surface with a hairline edge.
fn appearance(status: Status, theme: &cosmic::Theme) -> cosmic::widget::text_input::Appearance {
    let container = theme.current_container();
    let surface: iced::Color = container.base.into();
    let on: iced::Color = container.on.into();
    let cosmic = theme.cosmic();
    cosmic::widget::text_input::Appearance {
        // A field is a well in the surface rather than a raised control:
        // it holds the user's text, it is not pressed.
        background: iced::Background::Color(match status {
            Status::Disabled => surface,
            _ => design::mix(surface, on, 0.04),
        }),
        border_radius: design::RADIUS_S.into(),
        border_offset: None,
        border_width: design::BORDER_WIDTH,
        border_color: match status {
            Status::Focused => design::accent_color(theme),
            Status::Error => design::destructive_color(theme),
            Status::Hovered => design::mix(surface, on, 0.24),
            Status::Active | Status::Disabled => design::hairline(theme),
        },
        label_color: on,
        placeholder_color: design::field_placeholder_color(theme),
        icon_color: Some(design::muted_icon_color(theme)),
        text_color: Some(match status {
            Status::Disabled => design::disabled_color(theme),
            _ => design::field_text_color(theme),
        }),
        selected_text_color: cosmic.on_accent_color().into(),
        selected_fill: cosmic.accent_color().into(),
    }
}

fn style() -> cosmic::theme::TextInput {
    cosmic::theme::TextInput::Custom {
        active: Box::new(|theme| appearance(Status::Active, theme)),
        error: Box::new(|theme| appearance(Status::Error, theme)),
        hovered: Box::new(|theme| appearance(Status::Hovered, theme)),
        focused: Box::new(|theme| appearance(Status::Focused, theme)),
        disabled: Box::new(|theme| appearance(Status::Disabled, theme)),
    }
}

/// A multiline field in the same shape as a single-line [`text`] field.
fn editor_style(
    theme: &cosmic::Theme,
    status: iced::widget::text_editor::Status,
) -> iced::widget::text_editor::Style {
    use iced::widget::text_editor::Status as Editor;
    let field = appearance(
        match status {
            Editor::Active => Status::Active,
            Editor::Hovered => Status::Hovered,
            Editor::Focused { .. } => Status::Focused,
            Editor::Disabled => Status::Disabled,
        },
        theme,
    );
    iced::widget::text_editor::Style {
        background: field.background,
        border: iced::Border {
            radius: field.border_radius,
            width: field.border_width,
            color: field.border_color,
        },
        placeholder: field.placeholder_color,
        value: field.text_color.unwrap_or(field.label_color),
        // As COSMIC's own editor selects.
        selection: theme.cosmic().accent.base.into(),
    }
}

/// A named multiline form field with the native editor's ordinary behavior,
/// in the shape of a single-line [`text`] field: a well with a hairline edge
/// and the same insets, rather than COSMIC's heavy border.
///
/// Text scrolled past either end of a fixed height runs on through the
/// vertical inset to the edge rather than being cut off at the inset, and
/// fades into the field across the inset and up to a line into the text, so
/// the edge line shows there is more at every scroll position (#3044). A
/// caller that restyles the editor keeps the fade into its own background,
/// which must be opaque; a transparent editor on another surface names that
/// surface with [`TextEditor::fade_into_surface`].
pub fn editor<'a, Message: Clone + 'static>(
    name: impl Into<Cow<'a, str>>,
    content: &'a cosmic::iced::widget::text_editor::Content<cosmic::Renderer>,
) -> TextEditor<'a, Message> {
    TextEditor::new(name, content)
        .padding([INSET_Y, space::S])
        .style(editor_style)
        .fade_into_padding()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multiline form field is drawn in the field's shape and insets, and
    /// fades scrolled text into its own background unless a caller says
    /// otherwise (#3044).
    #[test]
    fn a_multiline_field_fades_into_its_own_padding_by_default() {
        let content = cosmic::iced::widget::text_editor::Content::with_text("notes");
        for theme in [cosmic::Theme::dark(), cosmic::Theme::light()] {
            let (padding, fade, style) =
                editor::<()>("Notes", &content).padding_fade_for_test(&theme);
            assert_eq!(padding, iced::Padding::from([INSET_Y, space::S]));
            assert_eq!(fade, Some((f32::INFINITY, f32::INFINITY, true)));
            assert_eq!(
                style,
                editor_style(&theme, iced::widget::text_editor::Status::Active)
            );
            // A caller's own style and padding keep the fade into its own
            // background; a surface replaces it.
            let restyled = editor::<()>("Notes", &content)
                .padding(4)
                .style(|theme, status| {
                    let mut style = editor_style(theme, status);
                    style.border.width = 2.0;
                    style
                });
            let (padding, fade, style) = restyled.padding_fade_for_test(&theme);
            assert_eq!(padding, iced::Padding::new(4.0));
            assert_eq!(fade, Some((f32::INFINITY, f32::INFINITY, true)));
            assert_eq!(style.border.width, 2.0);
            let (_, fade, _) = editor::<()>("Notes", &content)
                .fade_into_surface(3.0, 2.0, |_| iced::Color::BLACK)
                .padding_fade_for_test(&theme);
            assert_eq!(fade, Some((3.0, 2.0, false)));
        }
    }

    /// A multiline field has a single-line field's hairline edge, corner and
    /// fill in every status, and an opaque fill, which is what its scrolled
    /// text fades into, inside a band left between its inset and its edge
    /// (#3044).
    #[test]
    fn a_multiline_field_takes_the_single_line_shape_and_an_opaque_fill_to_fade_into() {
        use iced::widget::text_editor::Status as Editor;
        const { assert!(INSET_Y - design::BORDER_WIDTH >= 3.0, "a band to fade in") };
        for theme in [cosmic::Theme::dark(), cosmic::Theme::light()] {
            for (editor, field) in [
                (Editor::Active, Status::Active),
                (Editor::Hovered, Status::Hovered),
                (Editor::Focused { is_hovered: false }, Status::Focused),
                (Editor::Focused { is_hovered: true }, Status::Focused),
                (Editor::Disabled, Status::Disabled),
            ] {
                let style = editor_style(&theme, editor);
                let single = appearance(field, &theme);
                assert_eq!(style.background, single.background);
                assert_eq!(style.border.color, single.border_color);
                assert_eq!(style.border.width, design::BORDER_WIDTH);
                assert_eq!(style.border.radius, design::RADIUS_S.into());
                assert!(
                    matches!(style.background, iced::Background::Color(fill) if fill.a == 1.0),
                    "{:?}",
                    style.background
                );
            }
        }
    }

    #[test]
    fn a_search_field_stays_one_dense_row_high_with_its_clear_control() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{layout, widget::Tree};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        for query in ["", "query"] {
            let mut field = search::<()>("Search", query);
            if !query.is_empty() {
                field = field.trailing_icon(clear(()));
            }
            let mut element: Element<'_, ()> = field.into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(240.0, 200.0)),
            );
            assert_eq!(node.size().height, design::DENSE_ROW_HEIGHT, "{query:?}");
        }
    }
}
