//! One line of text that fades out where it runs out of room.

use std::borrow::Cow;
use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout,
    renderer::{self, Renderer as _},
    widget::{Id, Operation, Tree, tree},
};
use cosmic::iced::widget::text::{Catalog as _, LineHeight, Wrapping};
use cosmic::iced::{Event, Font, Length, Pixels, Rectangle, Size, Vector, mouse};
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
    Title,
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

    /// A [`design::title`].
    pub fn title(content: impl Into<Cow<'a, str>>) -> Self {
        Self {
            role: Role::Title,
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

    /// Shows at most `chars` characters (grapheme clusters, so an emoji or
    /// an accented letter is never split), fading out the last of them when
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
            Role::Title => design::title(content),
        }
        .wrapping(Wrapping::None)
        .ellipsize(cosmic::iced::widget::text::Ellipsize::None)
        .width(text.width)
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
            over: None,
            edge: text.edge,
            cut,
        })
    }
}

/// One line of body text, which must not break, in which the `highlighted`
/// byte ranges, in order, are set in
/// bold and drawn in the `highlight` colour, fading where it overflows as
/// [`FadeText`] does, highlights included.
///
/// The renderer has no alpha mask, so a fade is drawn by fading a line's
/// own colour; a span with a colour of its own would keep full strength.
/// The line is set twice instead, identically shaped: once with the
/// highlights clear and once with everything else clear, each in one colour
/// the fade reaches. Only the end fades, as only plain text can be drawn
/// moved back to show its end.
pub fn fade_highlighted<'a, Message: 'a>(
    content: &str,
    highlighted: &[Range<usize>],
    highlight: cosmic::theme::Text,
) -> Element<'a, Message> {
    use cosmic::iced::widget::{rich_text, text::Span};
    let font = cosmic::font::default();
    let bold = Font {
        weight: cosmic::iced::font::Weight::Bold,
        ..font
    };
    let stretches = stretches(content, highlighted);
    let layer = |shows_highlights: bool| -> Element<'a, Message> {
        rich_text(
            stretches
                .iter()
                .map(|(range, highlighted)| {
                    let span = Span::<'a, (), Font>::new(content[range.clone()].to_owned())
                        .font(if *highlighted { bold } else { font });
                    if *highlighted == shows_highlights {
                        span
                    } else {
                        span.color(cosmic::iced::Color::TRANSPARENT)
                    }
                })
                .collect::<Vec<_>>(),
        )
        .size(design::TEXT_BODY)
        .font(font)
        .wrapping(Wrapping::None)
        .into()
    };
    Element::new(Faded {
        line: layer(false),
        class: None,
        over: Some((layer(true), highlight)),
        edge: FadeEdge::End,
        cut: false,
    })
}

/// `content` cut into stretches in order, each with whether it is
/// highlighted. A range is kept only where it follows the last one and lies
/// within the content on character boundaries.
fn stretches(content: &str, highlighted: &[Range<usize>]) -> Vec<(Range<usize>, bool)> {
    let mut stretches = Vec::new();
    let mut start = 0;
    for range in highlighted {
        let range = range.start.max(start)..range.end.min(content.len());
        if range.is_empty()
            || !content.is_char_boundary(range.start)
            || !content.is_char_boundary(range.end)
        {
            continue;
        }
        if start < range.start {
            stretches.push((start..range.start, false));
        }
        stretches.push((range.clone(), true));
        start = range.end;
    }
    if start < content.len() || stretches.is_empty() {
        stretches.push((start..content.len(), false));
    }
    stretches
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

/// `content` held to `limit` grapheme clusters, and whether any were cut. A cut
/// leaves no trailing space for the fade to spend itself on.
fn within(content: Cow<'_, str>, limit: Option<usize>) -> (Cow<'_, str>, bool) {
    let Some((end, _)) = limit.and_then(|limit| content.grapheme_indices(true).nth(limit)) else {
        return (content, false);
    };
    let kept = content[..end].trim_end();
    (Cow::Owned(kept.to_owned()), true)
}

struct Faded<'a, Message> {
    line: Element<'a, Message>,
    /// The text colour, or the inherited one when the line brings its own.
    class: Option<cosmic::theme::Text>,
    /// A second setting of the same line drawn over it in a colour of its
    /// own, shaped and faded identically; see [`fade_highlighted`].
    over: Option<(Element<'a, Message>, cosmic::theme::Text)>,
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

/// The part of `bounds` a line overflowing it is drawn whole in, and the
/// part it fades over at `edge`.
///
/// The fade reaches [`FADE`] into the line, but never past half its width,
/// so even a very narrow slot keeps its first glyphs at full strength.
fn split(bounds: Rectangle, edge: FadeEdge) -> (Rectangle, Rectangle) {
    let depth = FADE.min(bounds.width / 2.0);
    let solid = Rectangle {
        width: bounds.width - depth,
        ..bounds
    };
    let fade = Rectangle {
        width: depth,
        ..bounds
    };
    match edge {
        FadeEdge::End => (
            solid,
            Rectangle {
                x: bounds.x + bounds.width - depth,
                ..fade
            },
        ),
        FadeEdge::Start => (
            Rectangle {
                x: bounds.x + depth,
                ..solid
            },
            fade,
        ),
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Faded<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Natural>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Natural::default())
    }

    fn children(&self) -> Vec<Tree> {
        std::iter::once(&self.line)
            .chain(self.over.as_ref().map(|(over, _)| over))
            .map(Tree::new)
            .collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        let mut lines: Vec<&mut Element<'_, Message>> = std::iter::once(&mut self.line)
            .chain(self.over.as_mut().map(|(over, _)| over))
            .collect();
        tree.diff_children(&mut lines);
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
        if let Some((over, _)) = &mut self.over {
            // Shaped from the same text and fonts, it takes the same room.
            over.as_widget_mut()
                .layout(&mut tree.children[1], renderer, limits);
        }
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
        if bounds.intersection(viewport).is_none() {
            return;
        }
        let base = self
            .class
            .as_ref()
            .and_then(|class| theme.style(class).color)
            .unwrap_or(style.text_color);
        let lines: Vec<_> = std::iter::once((&self.line, base))
            .chain(
                self.over
                    .as_ref()
                    .map(|(over, class)| (over, theme.style(class).color.unwrap_or(base))),
            )
            .zip(&tree.children)
            .collect();
        let Natural(natural) = *tree.state.downcast_ref::<Natural>();
        // Text that fits is drawn as any text is, clipped only to the
        // viewport, so ink above or below its line box is kept.
        if !self.cut && natural <= bounds.width + 0.5 {
            for ((line, color), tree) in &lines {
                let style = renderer::Style {
                    text_color: *color,
                    ..*style
                };
                line.as_widget()
                    .draw(tree, renderer, theme, &style, layout, cursor, viewport);
            }
            return;
        }
        // A line that fades at its start is laid out from its start like any
        // other, and drawn moved back so that its end meets the far edge.
        let shift = match self.edge {
            FadeEdge::End => 0.0,
            FadeEdge::Start => (natural - bounds.width).max(0.0),
        };
        // Each clip bounds the line across only; up and down the viewport
        // decides, as it does for text that fits.
        let draw = |renderer: &mut Renderer, across: Rectangle, alpha: f32| {
            let Some(clip) = Rectangle {
                y: viewport.y,
                height: viewport.height,
                ..across
            }
            .intersection(viewport) else {
                return;
            };
            renderer.with_translation(Vector::new(-shift, 0.0), |renderer| {
                // The clip is given before the move, which applies to it too.
                let clip = clip + Vector::new(shift, 0.0);
                for ((line, color), tree) in &lines {
                    let style = renderer::Style {
                        text_color: cosmic::iced::Color {
                            a: color.a * alpha,
                            ..*color
                        },
                        ..*style
                    };
                    line.as_widget()
                        .draw(tree, renderer, theme, &style, layout, cursor, &clip);
                }
            });
        };
        let (solid, fade) = split(bounds, self.edge);
        draw(renderer, solid, 1.0);
        for (step, alpha) in steps(fade, self.edge) {
            draw(renderer, step, alpha);
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
        // A flag is one character to the reader, two to Rust.
        let (kept, cut) = within("ab\u{1F1EB}\u{1F1F7}cd".into(), Some(3));
        assert_eq!((kept.as_ref(), cut), ("ab\u{1F1EB}\u{1F1F7}", true));
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
    fn highlighted_lines_are_measured_as_plain_ones_are() {
        let title = "A conversation title long enough to run out of room";
        let accent = cosmic::theme::Text::Accent;
        let (plain, _) = measure(fade_text(title).into(), 80.0);
        let (unmarked, width) = measure(fade_highlighted(title, &[], accent.clone()), 80.0);
        assert!((unmarked - plain).abs() < 1.0, "{unmarked} vs {plain}");
        assert!(width <= 80.0);
        let (marked, width) = measure(fade_highlighted(title, &[2..6, 40..44], accent), 80.0);
        assert!(marked >= unmarked && width <= 80.0 && marked > width + 0.5);
    }

    #[test]
    fn highlights_cut_the_line_into_ordered_stretches() {
        let line = "Open the conversation";
        assert_eq!(
            stretches(line, &[5..8, 9..21]),
            vec![(0..5, false), (5..8, true), (8..9, false), (9..21, true)]
        );
        // Overlapping, backward and out-of-range ranges keep only the part
        // that follows the last stretch and lies within the line.
        assert_eq!(
            stretches(line, &[5..8, 6..12, 2..4, 18..400]),
            vec![
                (0..5, false),
                (5..8, true),
                (8..12, true),
                (12..18, false),
                (18..21, true),
            ]
        );
        assert_eq!(stretches("", &[]), vec![(0..0, false)]);
        // A range inside a character is passed over rather than split it.
        assert_eq!(stretches("é", &[0..1]), vec![(0..2, false)]);
    }

    #[test]
    fn a_narrow_slot_keeps_its_first_half_solid() {
        let bounds = Rectangle {
            x: 10.0,
            y: 0.0,
            width: 20.0,
            height: 18.0,
        };
        let (solid, fade) = split(bounds, FadeEdge::End);
        assert_eq!((solid.x, solid.width), (10.0, 10.0));
        assert_eq!((fade.x, fade.width), (20.0, 10.0));
        let (solid, fade) = split(bounds, FadeEdge::Start);
        assert_eq!((fade.x, fade.width), (10.0, 10.0));
        assert_eq!((solid.x, solid.width), (20.0, 10.0));
        let wide = Rectangle {
            width: 200.0,
            ..bounds
        };
        let (solid, fade) = split(wide, FadeEdge::End);
        assert_eq!(
            (solid.width, fade.x, fade.width),
            (200.0 - FADE, 210.0 - FADE, FADE)
        );
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
