//! One line of body text with highlighted stretches, fading where it runs
//! out of room, highlights included.

use std::ops::Range;

use cosmic::iced::advanced::text::{
    self as core_text, Paragraph as _, Renderer as _, Shaping, Span,
};
use cosmic::iced::advanced::{
    Layout, Widget, layout, renderer,
    widget::{Tree, tree},
};
use cosmic::iced::alignment;
use cosmic::iced::widget::text::{Catalog as _, LineHeight, Wrapping};
use cosmic::iced::{Color, Font, Length, Pixels, Point, Rectangle, Size, mouse};
use cosmic::{Element, Renderer, Theme};

use super::{FadeEdge, split, steps};
use crate::design;

type Paragraph = <Renderer as core_text::Renderer>::Paragraph;

/// One line of body text, which must not break, in which the `highlighted`
/// byte ranges, in order, are set in bold and drawn in the `highlight`
/// colour. It fades at its end where it overflows, as
/// [`FadeText`](super::FadeText) does, highlights included.
///
/// The renderer has no alpha mask, so a fade is drawn by fading a line's
/// colour across narrow clips. A span with a colour of its own would keep
/// full strength through that, so the spans here carry none: the line is
/// shaped once and drawn in its own colour everywhere but over the
/// highlights, and in the highlight colour over them, each faded alike.
pub fn fade_highlighted<'a, Message: 'a>(
    content: &str,
    highlighted: &[Range<usize>],
    highlight: cosmic::theme::Text,
) -> Element<'a, Message> {
    let stretches = stretches(content, highlighted);
    let font = cosmic::font::default();
    let bold = Font {
        weight: cosmic::iced::font::Weight::Bold,
        ..font
    };
    Element::new(Highlighted {
        spans: stretches
            .iter()
            .map(|(range, highlighted)| {
                Span::new(content[range.clone()].to_owned()).font(if *highlighted {
                    bold
                } else {
                    font
                })
            })
            .collect(),
        highlighted: stretches
            .iter()
            .enumerate()
            .filter_map(|(index, (_, highlighted))| highlighted.then_some(index))
            .collect(),
        font,
        highlight,
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

struct Highlighted {
    spans: Vec<Span<'static, (), Font>>,
    /// The indices of the highlighted spans.
    highlighted: Vec<usize>,
    font: Font,
    highlight: cosmic::theme::Text,
}

#[derive(Default)]
struct State {
    spans: Vec<Span<'static, (), Font>>,
    paragraph: Paragraph,
    /// Where the highlighted spans lie across the line, from its start.
    marks: Vec<Range<f32>>,
}

impl Highlighted {
    fn text<'a>(
        &self,
        content: &'a [Span<'static, (), Font>],
    ) -> core_text::Text<&'a [Span<'static, (), Font>], Font> {
        core_text::Text {
            content,
            // Laid out unbounded across, so the paragraph keeps its
            // natural width whatever room the line is given.
            bounds: Size::new(f32::INFINITY, f32::INFINITY),
            size: Pixels(design::TEXT_BODY),
            line_height: LineHeight::default(),
            font: self.font,
            align_x: core_text::Alignment::Default,
            align_y: alignment::Vertical::Top,
            shaping: Shaping::Advanced,
            wrapping: Wrapping::None,
            ellipsize: core_text::Ellipsize::None,
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Highlighted {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        if state.spans != self.spans {
            state.paragraph = Paragraph::with_spans(self.text(&self.spans));
            state.marks = self
                .highlighted
                .iter()
                .flat_map(|index| state.paragraph.span_bounds(*index))
                .map(|bounds| bounds.x..bounds.x + bounds.width)
                .collect();
            state.spans = self.spans.clone();
        }
        layout::Node::new(limits.resolve(
            Length::Shrink,
            Length::Shrink,
            state.paragraph.min_bounds(),
        ))
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let state = tree.state.downcast_ref::<State>();
        let base = style.text_color;
        let accent = theme.style(&self.highlight).color.unwrap_or(base);
        let natural = state.paragraph.min_bounds().width;
        // Text that fits is drawn whole; up and down the viewport decides.
        let regions: Vec<(Rectangle, f32)> = if natural <= bounds.width + 0.5 {
            vec![(bounds, 1.0)]
        } else {
            let (solid, fade) = split(bounds, FadeEdge::End);
            std::iter::once((solid, 1.0))
                .chain(steps(fade, FadeEdge::End))
                .collect()
        };
        let faded = |color: Color, alpha: f32| Color {
            a: color.a * alpha,
            ..color
        };
        for (region, alpha) in regions {
            // Each stretch across is drawn once, in the colour it is set
            // in, so no glyph is drawn twice.
            for (across, highlighted) in
                pieces(region.x..region.x + region.width, bounds.x, &state.marks)
            {
                let Some(clip) = Rectangle {
                    x: across.start,
                    width: across.end - across.start,
                    y: viewport.y,
                    height: viewport.height,
                }
                .intersection(viewport) else {
                    continue;
                };
                let color = if highlighted { accent } else { base };
                renderer.fill_paragraph(
                    &state.paragraph,
                    Point::new(bounds.x, bounds.y),
                    faded(color, alpha),
                    clip,
                );
            }
        }
    }
}

/// `across` cut where the highlights, measured from `origin`, begin and
/// end, each piece with whether it lies over a highlight.
fn pieces(across: Range<f32>, origin: f32, marks: &[Range<f32>]) -> Vec<(Range<f32>, bool)> {
    let mut pieces = Vec::new();
    let mut at = across.start;
    for mark in marks {
        let mark = (origin + mark.start).max(at)..(origin + mark.end).min(across.end);
        if mark.start >= mark.end {
            continue;
        }
        if at < mark.start {
            pieces.push((at..mark.start, false));
        }
        pieces.push((mark.clone(), true));
        at = mark.end;
    }
    if at < across.end {
        pieces.push((at..across.end, false));
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;

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
    fn a_clip_is_cut_where_the_highlights_lie() {
        let marks = [10.0..20.0, 30.0..40.0];
        assert_eq!(
            pieces(100.0..150.0, 100.0, &marks),
            vec![
                (100.0..110.0, false),
                (110.0..120.0, true),
                (120.0..130.0, false),
                (130.0..140.0, true),
                (140.0..150.0, false),
            ]
        );
        // A fade step inside one highlight is that highlight alone.
        assert_eq!(
            pieces(112.0..114.0, 100.0, &marks),
            vec![(112.0..114.0, true)]
        );
        assert_eq!(
            pieces(100.0..105.0, 100.0, &marks),
            vec![(100.0..105.0, false)]
        );
        assert_eq!(
            pieces(100.0..150.0, 100.0, &[]),
            vec![(100.0..150.0, false)]
        );
    }

    #[test]
    fn a_highlighted_line_is_measured_and_marked_where_its_matches_lie() {
        let title = "A conversation title long enough to run out of room";
        let measure = |highlighted: &[Range<usize>]| {
            let mut line: Element<'_, ()> =
                fade_highlighted(title, highlighted, cosmic::theme::Text::Accent);
            let renderer = headless_renderer();
            let mut tree = Tree::new(line.as_widget());
            let node = line.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(80.0, 100.0)),
            );
            let state = tree.state.downcast_ref::<State>();
            (
                state.paragraph.min_bounds().width,
                node.size().width,
                state.marks.clone(),
            )
        };
        let (plain, width, marks) = measure(&[]);
        assert!(width <= 80.0 && plain > width + 0.5, "{plain} in {width}");
        assert!(marks.is_empty());
        let (marked, _, marks) = measure(&[2..14, 40..44]);
        assert!(marked >= plain);
        assert_eq!(marks.len(), 2);
        assert!(marks[0].start > 0.0 && marks[0].end < marks[1].start);
        assert!(marks[1].end <= marked + 0.5);
    }
}
