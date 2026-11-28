//! Shortcuts drawn as key caps: one outlined cap per key, named as the
//! platform's keyboards print it.
//!
//! Every surface that shows a shortcut, from a tooltip to the Keyboard
//! settings page and the command palette, draws it here, so a chord reads the
//! same wherever it appears. A chord is read into its keys by the grammar
//! shortcuts are written in ([`ShownChord`]), not split as text, so `Control`
//! and `ctrl` read alike and macOS shows `⇧` `⌘` `P` where Linux shows
//! `Ctrl` `Shift` `P`.
//!
//! The caps take their colors from the ink they are given rather than naming
//! one, so a cap fades in with the tooltip that holds it and follows the
//! theme of whatever surface it sits on.

use std::borrow::Cow;

use cagents::settings::{ShortcutBinding, ShortcutPlatform, ShownChord};
use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::{
    Layout, Widget, layout, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Alignment, Color, Length, Point, Rectangle, Size, mouse};
use cosmic::{Element, Renderer, Theme, widget};

use crate::design::{self, space};

/// A chord to show: the keys it was read into, or its text as written when
/// it is not one the shortcut grammar reads, such as a setting mistyped by
/// hand, which still has to be shown to be fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyChord<'a> {
    Parsed(ShownChord),
    Written(Cow<'a, str>),
}

impl<'a> KeyChord<'a> {
    pub fn new(raw: impl Into<Cow<'a, str>>) -> Self {
        let raw = raw.into();
        match ShownChord::parse(&raw) {
            Some(chord) => Self::Parsed(chord),
            None => Self::Written(raw),
        }
    }

    /// Each key cap's text, as this build's platform prints it.
    pub fn keys(&self) -> Vec<&str> {
        match self {
            Self::Parsed(chord) => chord.keys(ShortcutPlatform::CURRENT),
            Self::Written(raw) => vec![raw.as_ref()],
        }
    }

    /// The chord as one line of text, as a menu or a screen reader gives it.
    pub fn label(&self) -> Cow<'_, str> {
        match self {
            Self::Parsed(chord) => Cow::Owned(chord.label(ShortcutPlatform::CURRENT)),
            Self::Written(raw) => Cow::Borrowed(raw),
        }
    }

    pub fn into_owned(self) -> KeyChord<'static> {
        match self {
            Self::Parsed(chord) => KeyChord::Parsed(chord),
            Self::Written(raw) => KeyChord::Written(Cow::Owned(raw.into_owned())),
        }
    }
}

impl<'a> From<&'a str> for KeyChord<'a> {
    fn from(raw: &'a str) -> Self {
        Self::new(raw)
    }
}

impl<'a> From<&'a String> for KeyChord<'a> {
    fn from(raw: &'a String) -> Self {
        Self::new(raw.as_str())
    }
}

impl From<String> for KeyChord<'_> {
    fn from(raw: String) -> Self {
        Self::new(raw)
    }
}

impl<'a> From<Cow<'a, str>> for KeyChord<'a> {
    fn from(raw: Cow<'a, str>) -> Self {
        Self::new(raw)
    }
}

impl From<ShownChord> for KeyChord<'_> {
    fn from(chord: ShownChord) -> Self {
        Self::Parsed(chord)
    }
}

impl From<ShortcutBinding> for KeyChord<'_> {
    fn from(binding: ShortcutBinding) -> Self {
        Self::Parsed(binding.into())
    }
}

impl std::fmt::Display for KeyChord<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.label())
    }
}

/// The share of the given ink a cap's key is drawn in: close to the app's
/// faint text, so the caps recede behind the label they sit beside.
const KEY_INK: f32 = 0.64;
/// The share of the given ink a cap's outline is drawn in.
const OUTLINE_INK: f32 = 0.28;
/// The share of the given ink a cap is filled with.
const FILL_INK: f32 = 0.06;
/// A cap's inset either side of its key.
const CAP_PADDING: f32 = space::XS + 1.0;
/// The narrowest a cap is: a little wider than it is tall, so a cap holding
/// one letter is not a sliver beside `Ctrl` and a row of them reads as keys.
pub const MIN_CAP_WIDTH: f32 = design::LINE_CAPTION + space::XS;
/// Between the caps of one chord.
pub const CAP_GAP: f32 = space::XXS;
/// Between chords shown side by side, such as the several bound to one
/// action: wider than the gap inside a chord, so they read as alternatives.
pub const CHORD_GAP: f32 = space::S;

/// `chord` as a row of key caps.
pub fn key_caps<'a, Message: 'a>(chord: &KeyChord<'_>) -> Element<'a, Message> {
    widget::row::with_children(
        chord
            .keys()
            .into_iter()
            .map(|key| cap(key.to_owned()))
            .collect::<Vec<_>>(),
    )
    .spacing(CAP_GAP)
    .align_y(Alignment::Center)
    .into()
}

/// Several chords, such as every one bound to an action, as rows of key caps
/// side by side, wrapping a whole chord at a time where they run out of room.
pub fn chords_caps<'a, 'c, Message: 'a>(
    chords: impl IntoIterator<Item = KeyChord<'c>>,
) -> Element<'a, Message> {
    cosmic::iced::widget::Row::with_children(
        chords
            .into_iter()
            .map(|chord| key_caps(&chord))
            .collect::<Vec<_>>(),
    )
    .spacing(CHORD_GAP)
    .align_y(Alignment::Center)
    .wrap()
    .vertical_spacing(space::XS)
    .into()
}

/// A line of keyboard hints, such as a list's footer: each the keys that act
/// and, after them, what they do, as `↑` `↓` choose, `Enter` run.
pub fn key_hints<'a, 'c, Message: 'a>(
    hints: impl IntoIterator<Item = (&'c [&'c str], &'a str)>,
) -> Element<'a, Message> {
    cosmic::iced::widget::Row::with_children(
        hints
            .into_iter()
            .map(|(chords, action)| {
                let mut hint: Vec<Element<'a, Message>> = chords
                    .iter()
                    .map(|chord| key_caps(&KeyChord::new(*chord)))
                    .collect();
                hint.push(design::caption(action).into());
                widget::row::with_children(hint)
                    .spacing(space::XS)
                    .align_y(Alignment::Center)
                    .into()
            })
            .collect::<Vec<_>>(),
    )
    .spacing(space::M)
    .align_y(Alignment::Center)
    .wrap()
    .vertical_spacing(space::XS)
    .into()
}

fn cap<'a, Message: 'a>(key: String) -> Element<'a, Message> {
    Element::new(Cap {
        key: widget::text::caption(key)
            .size(design::TEXT_CAPTION)
            .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
                design::LINE_CAPTION.into(),
            ))
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .into(),
    })
}

/// One key cap: its key centered in an outline at least [`MIN_CAP_WIDTH`]
/// wide.
struct Cap<'a, Message> {
    key: Element<'a, Message>,
}

impl<Message> Widget<Message, Theme, Renderer> for Cap<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.key)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.key));
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let key = self.key.as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::new(Size::ZERO, limits.max()),
        );
        let inner = key.size();
        let size = Size::new(
            (inner.width + 2.0 * CAP_PADDING).max(MIN_CAP_WIDTH),
            inner.height,
        );
        let key = key.move_to(Point::new((size.width - inner.width) / 2.0, 0.0));
        layout::Node::with_children(
            limits.resolve(Length::Shrink, Length::Shrink, size),
            vec![key],
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        if let Some(key) = layout.children().next() {
            self.key
                .as_widget_mut()
                .operate(&mut tree.children[0], key, renderer, operation);
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let given = style.text_color;
        renderer.fill_quad(
            renderer::Quad {
                bounds: layout.bounds(),
                border: cosmic::iced::Border {
                    color: scaled(given, OUTLINE_INK),
                    width: design::BORDER_WIDTH,
                    radius: design::RADIUS_XS.into(),
                },
                ..Default::default()
            },
            scaled(given, FILL_INK),
        );
        let Some(key) = layout.children().next() else {
            return;
        };
        let mut style = *style;
        style.text_color = scaled(given, KEY_INK);
        self.key.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            &style,
            key.with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        );
    }
}

pub(super) fn scaled(color: Color, share: f32) -> Color {
    Color {
        a: color.a * share,
        ..color
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_of(chord: &str) -> layout::Node {
        let renderer = crate::workspace_widget::forwarding_test::renderer();
        let mut element: Element<'_, ()> = key_caps(&KeyChord::new(chord));
        let mut tree = Tree::new(&element);
        element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 100.0)),
        )
    }

    #[test]
    fn a_chord_is_read_into_keys_and_shown_as_written_when_it_cannot_be() {
        let linux = cfg!(not(target_os = "macos"));
        let keys = |raw: &str| KeyChord::new(raw).keys().join(" ");
        if linux {
            assert_eq!(keys("control+shift+t"), "Ctrl Shift T");
            assert_eq!(KeyChord::new("Alt+i").label(), "Alt+I");
            assert_eq!(keys("Escape"), "Esc");
        } else {
            assert_eq!(keys("cmd+shift+t"), "⇧ ⌘ T");
            assert_eq!(KeyChord::new("Cmd+I").label(), "⌘I");
        }
        assert_eq!(keys("Hyper+X"), "Hyper+X");
    }

    /// A one-letter cap is as wide as the narrowest cap, not its letter, and
    /// every cap sits on one line.
    #[test]
    fn single_letter_caps_keep_a_minimum_width() {
        let node = layout_of("Alt+I");
        let caps: Vec<Rectangle> = node.children().iter().map(|cap| cap.bounds()).collect();
        assert_eq!(caps.len(), 2);
        assert!(caps[1].width >= MIN_CAP_WIDTH, "{caps:?}");
        assert!(caps[0].width > caps[1].width || caps[0].width >= MIN_CAP_WIDTH);
        assert_eq!(caps[0].y, caps[1].y);
        // The letter is centered in its cap.
        let letter = node.children()[1].children()[0].bounds();
        let cap = caps[1];
        let left = letter.x;
        let right = cap.width - (letter.x + letter.width);
        assert!((left - right).abs() < 1.0, "{letter:?} in {cap:?}");
    }
}
