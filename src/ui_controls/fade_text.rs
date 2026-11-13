//! One line of text that fades out where it runs out of room.

use std::borrow::Cow;

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, renderer,
    text::Paragraph as _,
    widget::{Id, Operation, Tree, tree},
};
use cosmic::iced::widget::text::{Catalog as _, LineHeight, Wrapping};
use cosmic::iced::{Event, Font, Length, Pixels, Rectangle, Size, mouse};
use cosmic::{Element, Renderer, Theme};

use crate::design;

/// How far into the text the fade reaches.
///
/// About two narrow glyphs at body size: enough to read as the text running
/// on past the edge, short enough that the visible words stay legible.
const FADE: f32 = 24.0;

/// How many steps the fade is drawn in.
///
/// The renderer has no alpha mask, so the fading stretch is drawn as narrow
/// clipped copies of the same paragraph, each a step fainter. Two logical
/// pixels a step is finer than a glyph stroke, so the steps read as a ramp.
const STEPS: usize = 12;

/// Which end of the line gives way when the text does not fit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FadeEdge {
    /// The text starts at the leading edge and fades at the trailing one:
    /// names and titles, read from their start.
    #[default]
    End,
    /// The text ends at the trailing edge and fades at the leading one:
    /// paths, which are told apart by their last components.
    Start,
}

/// One line of UI text that fades at its edge when it overflows, instead of
/// being cut short with an ellipsis.
///
/// The fade is an alpha ramp on the glyphs themselves, not a gradient painted
/// over them, so it is right on any surface, in every hover, selection and
/// theme state, without knowing what is behind it. Text that fits is drawn
/// once, untouched. The fade stays inside the text's own bounds, so a status
/// icon or control after it in the row is never faded.
///
/// Accessibility and any tooltip see the whole text: only the drawing is cut.
///
/// The colour comes from [`FadeText::class`], never from the text itself, so
/// the faded steps always match the unfaded part.
pub struct FadeText<'a> {
    content: Cow<'a, str>,
    role: Role,
    size: Option<f32>,
    line_height: Option<LineHeight>,
    font: Option<Font>,
    class: cosmic::theme::Text,
    width: Length,
    edge: FadeEdge,
    limit: Option<usize>,
}

/// The [`design`] text a line is set as, whose size, line box and face it
/// takes.
#[derive(Clone, Copy)]
enum Role {
    Body,
    Caption,
    Heading,
}

/// One line of body text; see [`FadeText`].
pub fn fade_text<'a>(content: impl Into<Cow<'a, str>>) -> FadeText<'a> {
    FadeText {
        content: content.into(),
        role: Role::Body,
        size: None,
        line_height: None,
        font: None,
        class: cosmic::theme::Text::Default,
        width: Length::Shrink,
        edge: FadeEdge::End,
        limit: None,
    }
}

impl<'a> FadeText<'a> {
    /// A [`design::caption`], in the muted caption colour.
    pub fn caption(content: impl Into<Cow<'a, str>>) -> Self {
        Self {
            role: Role::Caption,
            class: cosmic::theme::Text::Custom(design::muted_text),
            ..fade_text(content)
        }
    }

    /// A [`design::heading`].
    pub fn heading(content: impl Into<Cow<'a, str>>) -> Self {
        Self {
            role: Role::Heading,
            ..fade_text(content)
        }
    }

    pub fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.size = Some(size.into().0);
        self
    }

    pub fn line_height(mut self, line_height: impl Into<LineHeight>) -> Self {
        self.line_height = Some(line_height.into());
        self
    }

    pub fn font(mut self, font: Font) -> Self {
        self.font = Some(font);
        self
    }

    pub fn class(mut self, class: cosmic::theme::Text) -> Self {
        self.class = class;
        self
    }

    /// How wide the line may be. Like any text it shrinks to fit by default,
    /// and is held to the room its row leaves it; [`Length::Fill`] takes all
    /// of that room, so a trailing sibling stays at the row's end.
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn edge(mut self, edge: FadeEdge) -> Self {
        self.edge = edge;
        self
    }

    /// Shows at most `chars` characters, fading out the last of them when
    /// there were more, for a label held to a budget of its own rather than
    /// to the room its row has left.
    pub fn limit(mut self, chars: usize) -> Self {
        self.limit = Some(chars);
        self
    }
}

impl<'a, Message: 'a> From<FadeText<'a>> for Element<'a, Message> {
    fn from(text: FadeText<'a>) -> Self {
        let (content, cut) = within(one_line(text.content), text.limit);
        let mut line = match text.role {
            Role::Body => design::body(content),
            Role::Caption => design::caption(content),
            Role::Heading => design::heading(content),
        }
        .wrapping(Wrapping::None)
        .ellipsize(cosmic::iced::widget::text::Ellipsize::None)
        .width(text.width)
        .align_x(match text.edge {
            FadeEdge::End => cosmic::iced::alignment::Horizontal::Left,
            FadeEdge::Start => cosmic::iced::alignment::Horizontal::Right,
        })
        // The colour is applied by the fade, step by step.
        .class(cosmic::theme::Text::Default);
        if let Some(size) = text.size {
            line = line.size(size);
        }
        if let Some(line_height) = text.line_height {
            line = line.line_height(line_height);
        }
        if let Some(font) = text.font {
            line = line.font(font);
        }
        Element::new(Faded {
            line: line.into(),
            class: Some(text.class),
            edge: text.edge,
            cut,
        })
    }
}

/// Fades any one line of text that overflows, such as a [`rich_text`] label
/// with highlighted matches, as [`FadeText`] fades plain text.
///
/// The line must not wrap or ellipsize, and must keep its default shrinking
/// width, which is how its full width is measured; size the row around it
/// instead. Spans keep the colour the line is
/// drawn with unless they set their own: a span with its own colour is drawn
/// at full strength even where the line fades, so keep coloured spans to the
/// line's start, where a fade rarely reaches.
///
/// [`rich_text`]: cosmic::iced::widget::rich_text
pub fn fade_overflow<'a, Message: 'a>(
    line: impl Into<Element<'a, Message>>,
    edge: FadeEdge,
) -> Element<'a, Message> {
    Element::new(Faded {
        line: line.into(),
        class: None,
        edge,
        cut: false,
    })
}

/// `content` on a single line: a line break would make the label a line
/// taller and drop what follows it from view, so the lines run on instead,
/// a space apart, and the fade says where they go on past the edge.
fn one_line(content: Cow<'_, str>) -> Cow<'_, str> {
    if !content.contains(['\n', '\r']) {
        return content;
    }
    Cow::Owned(
        content
            .split(['\n', '\r'])
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// `content` held to `limit` characters, and whether any were cut. A cut
/// leaves no trailing space for the fade to spend itself on.
fn within(content: Cow<'_, str>, limit: Option<usize>) -> (Cow<'_, str>, bool) {
    let Some((end, _)) = limit.and_then(|limit| content.char_indices().nth(limit)) else {
        return (content, false);
    };
    let kept = content[..end].trim_end();
    (Cow::Owned(kept.to_owned()), true)
}

struct Faded<'a, Message> {
    line: Element<'a, Message>,
    /// The text colour, or the inherited one when the line brings its own.
    class: Option<cosmic::theme::Text>,
    edge: FadeEdge,
    /// The text was cut to a budget, so it fades even where it fits.
    cut: bool,
}

/// The line's natural width, measured at layout.
#[derive(Clone, Copy, Default)]
struct Natural(f32);

type TextState = cosmic::iced::advanced::widget::text::State<
    <Renderer as cosmic::iced::advanced::text::Renderer>::Paragraph,
>;

/// The clip of each fading step across `fade`, faintest last, with the share
/// of the colour's alpha it keeps.
///
/// Step edges are computed from the fade's ends rather than accumulated, so
/// neighbouring steps share an edge exactly and the renderer's rounding of
/// each clip to whole pixels never leaves a gap or a doubled column.
fn steps(fade: Rectangle, edge: FadeEdge) -> impl Iterator<Item = (Rectangle, f32)> {
    let at = move |index: usize| fade.x + fade.width * index as f32 / STEPS as f32;
    (0..STEPS).map(move |index| {
        let clip = Rectangle {
            x: at(index),
            width: at(index + 1) - at(index),
            ..fade
        };
        // Toward the edge the text gives way at.
        let position = (index as f32 + 0.5) / STEPS as f32;
        let alpha = match edge {
            FadeEdge::End => 1.0 - position,
            FadeEdge::Start => position,
        };
        (clip, alpha)
    })
}

impl<Message> Widget<Message, Theme, Renderer> for Faded<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Natural>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Natural::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.line)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.line));
    }

    fn size(&self) -> Size<Length> {
        self.line.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.line.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let line = &mut tree.children[0];
        // Plain text keeps its shaped paragraph, which knows its full width
        // whatever room it is laid out in. Anything else is measured by
        // laying it out with all the room it wants first; unwrapped text
        // only re-flows for that, it is not shaped again.
        let plain = line.tag == tree::Tag::of::<TextState>();
        let unbounded = (!plain).then(|| {
            let room =
                layout::Limits::new(limits.min(), Size::new(f32::INFINITY, limits.max().height));
            self.line
                .as_widget_mut()
                .layout(line, renderer, &room)
                .size()
                .width
        });
        let node = self.line.as_widget_mut().layout(line, renderer, limits);
        let natural = unbounded.unwrap_or_else(|| {
            line.state
                .downcast_ref::<TextState>()
                .paragraph
                .min_bounds()
                .width
        });
        *tree.state.downcast_mut::<Natural>() = Natural(natural);
        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.line
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.line.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.line.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
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
        let bounds = layout.bounds();
        // Text clips to the viewport it is handed, not its own bounds, so
        // without this an overflowing line would run over its siblings.
        let Some(visible) = bounds.intersection(viewport) else {
            return;
        };
        let color = self
            .class
            .as_ref()
            .and_then(|class| theme.style(class).color)
            .unwrap_or(style.text_color);
        let draw = |renderer: &mut Renderer, clip: &Rectangle, alpha: f32| {
            let style = renderer::Style {
                text_color: cosmic::iced::Color {
                    a: color.a * alpha,
                    ..color
                },
                ..*style
            };
            self.line.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                &style,
                layout,
                cursor,
                clip,
            );
        };
        let Natural(natural) = *tree.state.downcast_ref::<Natural>();
        if !self.cut && natural <= bounds.width + 0.5 {
            draw(renderer, &visible, 1.0);
            return;
        }
        let depth = FADE.min(bounds.width);
        let (solid, fade) = match self.edge {
            FadeEdge::End => (
                Rectangle {
                    width: bounds.width - depth,
                    ..bounds
                },
                Rectangle {
                    x: bounds.x + bounds.width - depth,
                    width: depth,
                    ..bounds
                },
            ),
            FadeEdge::Start => (
                Rectangle {
                    x: bounds.x + depth,
                    width: bounds.width - depth,
                    ..bounds
                },
                Rectangle {
                    width: depth,
                    ..bounds
                },
            ),
        };
        if let Some(clip) = solid.intersection(viewport) {
            draw(renderer, &clip, 1.0);
        }
        for (step, alpha) in steps(fade, self.edge) {
            if let Some(clip) = step.intersection(viewport) {
                draw(renderer, &clip, alpha);
            }
        }
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.line
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }

    fn id(&self) -> Option<Id> {
        self.line.as_widget().id()
    }

    fn set_id(&mut self, id: Id) {
        self.line.as_widget_mut().set_id(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;

    /// The natural width a line measured, and the width it was given.
    fn measure(mut line: Element<'_, ()>, room: f32) -> (f32, f32) {
        let renderer = headless_renderer();
        let mut tree = Tree::new(line.as_widget());
        let node = line.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(room, 100.0)),
        );
        (tree.state.downcast_ref::<Natural>().0, node.size().width)
    }

    #[test]
    fn line_breaks_run_on_as_one_line() {
        assert!(matches!(one_line("plain".into()), Cow::Borrowed("plain")));
        assert_eq!(
            one_line("Fix the build\r\n\n  then  rerun\n".into()),
            "Fix the build then  rerun"
        );
    }

    #[test]
    fn a_budget_cuts_by_character_and_says_so() {
        assert!(matches!(
            within("Short".into(), Some(8)),
            (Cow::Borrowed("Short"), false)
        ));
        assert!(matches!(within("Exactly8".into(), Some(8)), (_, false)));
        let (kept, cut) = within("feature/a much longer branch".into(), Some(10));
        assert_eq!((kept.as_ref(), cut), ("feature/a", true));
        assert!(matches!(within("Unbounded".into(), None), (_, false)));
    }

    #[test]
    fn a_line_knows_when_it_overflows_the_room_it_was_given() {
        let title = "A conversation title long enough to run out of room";
        let (natural, width) = measure(fade_text(title).into(), 600.0);
        assert!(
            natural > 0.0 && natural <= width + 0.5,
            "{natural} in {width}"
        );
        let (narrow, width) = measure(fade_text(title).into(), 80.0);
        assert!((narrow - natural).abs() < 0.5, "measured whatever the room");
        assert!(width <= 80.0 && narrow > width + 0.5, "{narrow} in {width}");
    }

    #[test]
    fn rich_lines_are_measured_as_plain_ones_are() {
        let title = "A conversation title long enough to run out of room";
        let (plain, _) = measure(fade_text(title).into(), 80.0);
        let rich = cosmic::iced::widget::rich_text(vec![
            cosmic::iced::widget::text::Span::<'_, (), Font>::new("A conversation")
                .color(cosmic::iced::Color::from_rgb(0.9, 0.2, 0.2)),
            cosmic::iced::widget::text::Span::new(" title long enough to run out of room"),
        ])
        .size(design::TEXT_BODY)
        .wrapping(Wrapping::None);
        let (natural, width) = measure(fade_overflow(rich, FadeEdge::End), 80.0);
        assert!((natural - plain).abs() < 1.0, "{natural} vs {plain}");
        assert!(width <= 80.0);
    }

    #[test]
    fn steps_tile_the_fade_and_ramp_toward_the_edge_it_gives_way_at() {
        let fade = Rectangle {
            x: 100.25,
            y: 4.0,
            width: 24.0,
            height: 18.0,
        };
        let end: Vec<_> = steps(fade, FadeEdge::End).collect();
        assert_eq!(end.len(), STEPS);
        assert_eq!(end[0].0.x, fade.x);
        for pair in end.windows(2) {
            assert_eq!(pair[0].0.x + pair[0].0.width, pair[1].0.x);
            assert!(pair[0].1 > pair[1].1);
        }
        let last = end[STEPS - 1].0;
        assert!((last.x + last.width - (fade.x + fade.width)).abs() < 1e-3);
        assert!(end[0].1 < 1.0 && end[STEPS - 1].1 > 0.0);

        let start: Vec<_> = steps(fade, FadeEdge::Start).collect();
        for pair in start.windows(2) {
            assert!(pair[0].1 < pair[1].1);
        }
    }
}
