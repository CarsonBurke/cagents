//! One line of text with stretches set apart in colours of their own, or in
//! bold, fading where it runs out of room, those stretches included.

use std::ops::Range;

use cosmic::iced::advanced::text::{
    self as core_text, Paragraph as _, Renderer as _, Shaping, Span,
};
use cosmic::iced::advanced::{
    Layout, Widget, layout, renderer,
    widget::{Id, Operation, Tree, tree},
};
use cosmic::iced::alignment;
use cosmic::iced::widget::text::{Catalog as _, LineHeight, Wrapping};
use cosmic::iced::{Color, Font, Length, Pixels, Point, Rectangle, Size, mouse};
use cosmic::{Element, Renderer, Theme};

use super::{Drawn, FadeText, Mark, Region, regions, within};

type Paragraph = <Renderer as core_text::Renderer>::Paragraph;

/// `text`, whose marks the caller has checked it has, as one line in which
/// each marked stretch is drawn in its mark's colour, and in bold where the
/// mark says so. It fades where it overflows, as any [`FadeText`] does.
///
/// The renderer has no alpha mask, so a fade is drawn by fading a line's
/// colour across narrow clips. A span with a colour of its own would keep
/// full strength through that, so the spans here carry none: the line is
/// shaped once and drawn in its own colour everywhere but over the marks,
/// and in each mark's colour over it, each faded alike.
pub(super) fn element<'a, Message: 'a>(text: FadeText<'a>) -> Element<'a, Message> {
    // A line break, one byte, runs on as one space, so the marks' byte
    // ranges still hold.
    let content = match text.content {
        content if content.contains(['\n', '\r']) => content.replace(['\n', '\r'], " ").into(),
        content => content,
    };
    let (content, cut) = within(content, text.limit);
    let content = content.into_owned();
    let (size, line_height, font) = text.role.format();
    let font = text.font.unwrap_or(font);
    let stretches = stretches(&content, &text.marks);
    let bold = |bold: bool| {
        if bold {
            Font {
                weight: cosmic::iced::font::Weight::Bold,
                ..font
            }
        } else {
            font
        }
    };
    Element::new(Highlighted {
        spans: stretches
            .iter()
            .map(|(range, mark)| {
                Span::new(content[range.clone()].to_owned())
                    .font(bold(mark.is_some_and(|mark| text.marks[mark].bold)))
            })
            .collect(),
        marked: stretches
            .iter()
            .enumerate()
            .filter_map(|(index, (_, mark))| mark.map(|mark| (index, mark)))
            .collect(),
        classes: text.marks.iter().map(|mark| mark.class).collect(),
        content,
        size: text.size.unwrap_or(size),
        line_height: text.line_height.unwrap_or(line_height),
        font,
        class: text.class,
        width: text.width,
        edge: text.edge,
        cut,
        whole: text.whole,
        id: Id::unique(),
    })
}

/// `content` cut into stretches in order, each with the mark it is under,
/// by index. A mark is kept only where it follows the last one and lies
/// within the content on character boundaries.
fn stretches(content: &str, marks: &[Mark]) -> Vec<(Range<usize>, Option<usize>)> {
    let mut stretches = Vec::new();
    let mut start = 0;
    for (index, mark) in marks.iter().enumerate() {
        let range = mark.range.start.max(start)..mark.range.end.min(content.len());
        if range.is_empty()
            || !content.is_char_boundary(range.start)
            || !content.is_char_boundary(range.end)
        {
            continue;
        }
        if start < range.start {
            stretches.push((start..range.start, None));
        }
        stretches.push((range.clone(), Some(index)));
        start = range.end;
    }
    if start < content.len() || stretches.is_empty() {
        stretches.push((start..content.len(), None));
    }
    stretches
}

struct Highlighted {
    content: String,
    spans: Vec<Span<'static, (), Font>>,
    /// Each marked span, by index, with its mark's index.
    marked: Vec<(usize, usize)>,
    /// Each mark's colour.
    classes: Vec<cosmic::theme::Text>,
    size: f32,
    line_height: LineHeight,
    font: Font,
    /// The colour of the unmarked text.
    class: cosmic::theme::Text,
    width: Length,
    edge: super::FadeEdge,
    cut: bool,
    whole: bool,
    id: Id,
}

#[derive(Default)]
struct State {
    spans: Vec<Span<'static, (), Font>>,
    format: Option<(u32, LineHeight)>,
    /// The font system's version the paragraph was shaped against: a font
    /// loaded since may shape it differently.
    fonts: Option<cosmic::iced::advanced::graphics::text::Version>,
    paragraph: Paragraph,
    /// Where the marked spans lie across the line, from its start, with
    /// the mark each is under.
    marks: Vec<(Range<f32>, usize)>,
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
            size: Pixels(self.size),
            line_height: self.line_height,
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
        Size::new(self.width, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<State>();
        let fonts = cosmic::iced::advanced::graphics::text::font_system()
            .read()
            .map_or(None, |fonts| Some(fonts.version()));
        let format = Some((self.size.to_bits(), self.line_height));
        if state.spans != self.spans || state.format != format || state.fonts != fonts {
            state.paragraph = Paragraph::with_spans(self.text(&self.spans));
            // Set right to left, marks lie across the line in the opposite
            // order to the text's; drawing walks them left to right.
            state.marks = self
                .marked
                .iter()
                .flat_map(|&(span, mark)| {
                    state
                        .paragraph
                        .span_bounds(span)
                        .into_iter()
                        .map(move |bounds| (bounds.x..bounds.x + bounds.width, mark))
                })
                .collect();
            state.marks.sort_by(|a, b| a.0.start.total_cmp(&b.0.start));
            state.spans = self.spans.clone();
            state.format = format;
            state.fonts = fonts;
        }
        layout::Node::new(limits.resolve(self.width, Length::Shrink, state.paragraph.min_bounds()))
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
        if bounds.intersection(viewport).is_none() {
            return;
        }
        let state = tree.state.downcast_ref::<State>();
        let base = theme.style(&self.class).color.unwrap_or(style.text_color);
        let colors: Vec<Color> = self
            .classes
            .iter()
            .map(|class| theme.style(class).color.unwrap_or(base))
            .collect();
        let natural = state.paragraph.min_bounds().width;
        let drawn = Drawn::of(natural, bounds.width, self.cut, self.whole);
        // Across, only the fade and the cuts between colours clip: ink that
        // overhangs a line that fits, such as a bold glyph's, is drawn as
        // plain text draws it. Up and down the viewport decides.
        let regions = match drawn {
            Drawn::Nothing => return,
            Drawn::Whole => vec![Region {
                clip: Rectangle {
                    x: viewport.x.min(bounds.x),
                    width: (viewport.x + viewport.width).max(bounds.x + bounds.width)
                        - viewport.x.min(bounds.x),
                    ..bounds
                },
                alpha: 1.0,
                shift: 0.0,
            }],
            Drawn::Faded => regions(bounds, self.edge, natural),
        };
        let faded = |color: Color, alpha: f32| Color {
            a: color.a * alpha,
            ..color
        };
        for region in regions {
            let origin = bounds.x - region.shift;
            // Each stretch across is drawn once, in the colour it is set
            // in, so no glyph is drawn twice.
            for (across, mark) in pieces(
                region.clip.x..region.clip.x + region.clip.width,
                origin,
                &state.marks,
            ) {
                let Some(clip) = Rectangle {
                    x: across.start,
                    width: across.end - across.start,
                    y: viewport.y,
                    height: viewport.height,
                }
                .intersection(viewport) else {
                    continue;
                };
                let color = mark.map_or(base, |mark| colors[mark]);
                renderer.fill_paragraph(
                    &state.paragraph,
                    Point::new(origin, bounds.y),
                    faded(color, region.alpha),
                    clip,
                );
            }
        }
    }

    fn operate(
        &mut self,
        _tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.text(Some(&self.id), layout.bounds(), &self.content);
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        _tree: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use iced_accessibility::{
            A11yTree,
            accesskit::{Live, Node, Rect, Role},
        };
        // Read as plain text is: the whole of it, whatever is drawn.
        let bounds = layout.bounds();
        let mut node = Node::new(Role::Paragraph);
        node.set_label(self.content.clone().into_boxed_str());
        node.set_bounds(Rect::new(
            f64::from(bounds.x),
            f64::from(bounds.y),
            f64::from(bounds.x + bounds.width),
            f64::from(bounds.y + bounds.height),
        ));
        node.set_live(Live::Polite);
        A11yTree::leaf(node, self.id.clone())
    }

    fn id(&self) -> Option<Id> {
        Some(self.id.clone())
    }

    fn set_id(&mut self, id: Id) {
        self.id = id;
    }
}

/// `across` cut where the marks, measured from `origin`, begin and end,
/// each piece with the mark it lies under.
fn pieces(
    across: Range<f32>,
    origin: f32,
    marks: &[(Range<f32>, usize)],
) -> Vec<(Range<f32>, Option<usize>)> {
    let mut pieces = Vec::new();
    let mut at = across.start;
    for (mark, index) in marks {
        let mark = (origin + mark.start).max(at)..(origin + mark.end).min(across.end);
        if mark.start >= mark.end {
            continue;
        }
        if at < mark.start {
            pieces.push((at..mark.start, None));
        }
        pieces.push((mark.clone(), Some(*index)));
        at = mark.end;
    }
    if at < across.end {
        pieces.push((at..across.end, None));
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;
    use crate::ui_controls::fade_text;

    fn matches(ranges: &[Range<usize>]) -> Vec<Mark> {
        ranges
            .iter()
            .map(|range| Mark {
                range: range.clone(),
                class: cosmic::theme::Text::Accent,
                bold: true,
            })
            .collect()
    }

    /// Lays `line` out in `room` across.
    fn laid_out(mut line: Element<'_, ()>, room: f32) -> (Tree, layout::Node) {
        let renderer = headless_renderer();
        let mut tree = Tree::new(line.as_widget());
        let node = line.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(room, 100.0)),
        );
        (tree, node)
    }

    #[test]
    fn marks_cut_the_line_into_ordered_stretches() {
        let line = "Open the conversation";
        assert_eq!(
            stretches(line, &matches(&[5..8, 9..21])),
            vec![
                (0..5, None),
                (5..8, Some(0)),
                (8..9, None),
                (9..21, Some(1))
            ]
        );
        // Overlapping, backward and out-of-range marks keep only the part
        // that follows the last stretch and lies within the line.
        assert_eq!(
            stretches(line, &matches(&[5..8, 6..12, 2..4, 18..400])),
            vec![
                (0..5, None),
                (5..8, Some(0)),
                (8..12, Some(1)),
                (12..18, None),
                (18..21, Some(3)),
            ]
        );
        assert_eq!(stretches("", &[]), vec![(0..0, None)]);
        // A range inside a character is passed over rather than split it.
        assert_eq!(stretches("é", &matches(&[0..1])), vec![(0..2, None)]);
    }

    #[test]
    fn a_clip_is_cut_where_the_marks_lie() {
        let marks = [(10.0..20.0, 0), (30.0..40.0, 1)];
        assert_eq!(
            pieces(100.0..150.0, 100.0, &marks),
            vec![
                (100.0..110.0, None),
                (110.0..120.0, Some(0)),
                (120.0..130.0, None),
                (130.0..140.0, Some(1)),
                (140.0..150.0, None),
            ]
        );
        // A fade step inside one mark is that mark alone.
        assert_eq!(
            pieces(112.0..114.0, 100.0, &marks),
            vec![(112.0..114.0, Some(0))]
        );
        assert_eq!(
            pieces(100.0..105.0, 100.0, &marks),
            vec![(100.0..105.0, None)]
        );
        assert_eq!(pieces(100.0..150.0, 100.0, &[]), vec![(100.0..150.0, None)]);
    }

    #[test]
    fn a_highlighted_line_is_measured_and_marked_where_its_matches_lie() {
        let title = "A conversation title long enough to run out of room";
        let measure = |highlighted: &[Range<usize>]| {
            let (tree, node) = laid_out(
                fade_text(title)
                    .highlight(highlighted, cosmic::theme::Text::Accent)
                    .tint(0..0, cosmic::theme::Text::Accent)
                    .into(),
                80.0,
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
        assert!(marks[0].0.start > 0.0 && marks[0].0.end < marks[1].0.start);
        assert!(marks[1].0.end <= marked + 0.5);
    }

    #[test]
    fn a_tinted_line_is_set_like_the_plain_one() {
        // A tint changes only colour, so the line keeps the plain line's
        // size, line box and face in every role.
        let words = "octocat · main ← feature/fade-every-truncation";
        let tint = || words.find('·').unwrap()..words.len();
        let cases: [(super::super::FadeText<'_>, super::super::FadeText<'_>); 4] = [
            (fade_text(words), fade_text(words)),
            (
                super::super::FadeText::caption(words),
                super::super::FadeText::caption(words),
            ),
            (
                super::super::FadeText::heading(words),
                super::super::FadeText::heading(words),
            ),
            (
                super::super::FadeText::transcript(words),
                super::super::FadeText::transcript(words),
            ),
        ];
        for (plain, tinted) in cases {
            let (_, plain) = laid_out(plain.into(), 1000.0);
            let (_, tinted) = laid_out(
                tinted.tint(tint(), cosmic::theme::Text::Accent).into(),
                1000.0,
            );
            let (plain, tinted) = (plain.size(), tinted.size());
            assert!(
                (plain.width - tinted.width).abs() < 0.5 && plain.height == tinted.height,
                "{plain:?} against {tinted:?}"
            );
        }
    }

    #[test]
    fn matches_set_right_to_left_are_marked_left_to_right() {
        // Two matches in a Hebrew title: the first lies to the right.
        let title = "שלום עולם ושלום";
        let first = 0..title.find(' ').unwrap();
        let last = title.rfind(' ').unwrap() + 1..title.len();
        let (tree, _) = laid_out(
            fade_text(title)
                .highlight(&[first, last], cosmic::theme::Text::Accent)
                .into(),
            400.0,
        );
        let marks = &tree.state.downcast_ref::<State>().marks;
        assert_eq!(marks.len(), 2, "{marks:?}");
        assert!(marks[0].0.end <= marks[1].0.start + 0.5, "{marks:?}");
        // Both matches are drawn in the highlight across a clip of the line.
        let highlighted = pieces(0.0..400.0, 0.0, marks)
            .into_iter()
            .filter(|(_, mark)| mark.is_some())
            .count();
        assert_eq!(highlighted, 2);
    }
}
