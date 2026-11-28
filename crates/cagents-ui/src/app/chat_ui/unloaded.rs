//! The space that stands for history outside the loaded transcript window.
//!
//! A long conversation is loaded a window at a time. Giving the history
//! before and after the window an estimated height lets the scrollbar span
//! the whole conversation, so its thumb says where the reader is in it and a
//! track click or drag can reach any part of it. While that part loads, the
//! space shows the outline of messages rather than an empty pane.
//!
//! The space is a whole number of outlined messages tall, so the outline
//! lines up with both of its edges. The reader is kept still by the loaded
//! rows next to the space, or by the conversation's start while they see
//! only the space, and either way the outline does not move under them as
//! the space changes height.

use cosmic::iced::advanced::{
    Layout, Widget, layout, renderer,
    widget::{Tree, tree},
};
use cosmic::iced::{Background, Border, Color, Length, Rectangle, Shadow, Size, mouse};
use cosmic::{Element, Renderer, Theme};

/// How tall one outlined message is, with the gap after it.
const PERIOD: f32 = 132.0;
/// Each outlined message's lines, as fractions of the row's width.
const LINES: [f32; 4] = [0.94, 0.88, 0.72, 0.46];
const LINE_HEIGHT: f32 = 10.0;
const LINE_GAP: f32 = 14.0;
/// Where an outlined message starts within its period.
const INSET: f32 = 24.0;

/// The height of the space standing for `items` unloaded items, at an
/// estimated `per_item` each.
pub(super) fn height(items: u64, per_item: f32) -> f32 {
    if items == 0 {
        return 0.0;
    }
    (items as f32 * per_item / PERIOD).round().max(1.0) * PERIOD
}

pub(super) fn space<'a, Message: 'a>(height: f32) -> Element<'a, Message> {
    Element::new(Unloaded { height })
}

struct Unloaded {
    height: f32,
}

impl<Message> Widget<Message, Theme, Renderer> for Unloaded {
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fixed(self.height))
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(Size::new(limits.max().width, self.height))
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(visible) = bounds.intersection(viewport) else {
            return;
        };
        let color = Color {
            a: 0.07,
            ..crate::design::muted_color(theme)
        };
        let origin = bounds.y;
        let period = |y: f32| ((y - origin) / PERIOD).floor() as i64;
        for index in period(visible.y)..=period(visible.y + visible.height) {
            let start = origin + index as f32 * PERIOD;
            for (line, fraction) in LINES.iter().enumerate() {
                let y = start + INSET + line as f32 * (LINE_HEIGHT + LINE_GAP);
                let bar = Rectangle {
                    x: bounds.x,
                    y,
                    width: bounds.width * fraction,
                    height: LINE_HEIGHT,
                };
                if bar.y < bounds.y || bar.y + bar.height > bounds.y + bounds.height {
                    continue;
                }
                if bar.intersection(&visible).is_none() {
                    continue;
                }
                renderer::Renderer::fill_quad(
                    renderer,
                    renderer::Quad {
                        bounds: bar,
                        border: Border {
                            radius: (LINE_HEIGHT / 2.0).into(),
                            ..Border::default()
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    },
                    Background::Color(color),
                );
            }
        }
    }
}
