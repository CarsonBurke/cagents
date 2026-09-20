//! Shell separators share a logical layout budget and a physical pixel grid.
use cosmic::{Element, Renderer, Theme, iced};
use iced::advanced::Renderer as _;
use iced::advanced::{Layout, Widget, layout, mouse, renderer, widget::Tree};
use iced::{Length, Rectangle, Size, Vector};

#[derive(Clone, Copy)]
pub(crate) enum Axis {
    Horizontal,
    Vertical,
}

/// Account for the renderer's scroll translation before snapping. Thickness
/// is rounded once, independently of position, so identical rules never
/// alternate between one and two pixels as a pane moves. Edge splitters snap
/// toward their pane interior to survive fractional viewport clipping.
pub(crate) fn physical_bounds(
    bounds: Rectangle,
    offset: Vector,
    scale: f32,
    axis: Axis,
    trailing: bool,
) -> Rectangle {
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Rectangle {
            width: 0.0,
            height: 0.0,
            ..bounds
        };
    }
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let thickness = (crate::design::BORDER_WIDTH * scale).round().max(1.0) / scale;
    let start = |value: f32, shift: f32| ((value - shift) * scale).floor() / scale + shift;
    let end = |value: f32, shift: f32| ((value - shift) * scale).ceil() / scale + shift;
    let edge = |value: f32, extent: f32, shift: f32| {
        if trailing {
            ((value + extent - shift) * scale).floor() / scale + shift - thickness
        } else {
            ((value - shift) * scale).ceil() / scale + shift
        }
    };
    match axis {
        Axis::Horizontal => {
            let x = start(bounds.x, offset.x);
            Rectangle {
                x,
                y: edge(bounds.y, bounds.height, offset.y),
                width: end(bounds.x + bounds.width, offset.x) - x,
                height: thickness,
            }
        }
        Axis::Vertical => {
            let y = start(bounds.y, offset.y);
            Rectangle {
                x: edge(bounds.x, bounds.width, offset.x),
                y,
                width: thickness,
                height: end(bounds.y + bounds.height, offset.y) - y,
            }
        }
    }
}

pub(crate) fn horizontal<'a, Message: 'a>() -> Element<'a, Message> {
    Element::new(Separator(Axis::Horizontal))
}

pub(crate) fn vertical<'a, Message: 'a>() -> Element<'a, Message> {
    Element::new(Separator(Axis::Vertical))
}

struct Separator(Axis);

impl<Message> Widget<Message, Theme, Renderer> for Separator {
    fn size(&self) -> Size<Length> {
        match self.0 {
            Axis::Horizontal => Size::new(Length::Fill, Length::Fixed(crate::design::BORDER_WIDTH)),
            Axis::Vertical => Size::new(Length::Fixed(crate::design::BORDER_WIDTH), Length::Fill),
        }
    }

    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        let size = <Self as Widget<Message, Theme, Renderer>>::size(self);
        layout::atomic(limits, size.width, size.height)
    }

    fn draw(
        &self,
        _: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _: mouse::Cursor,
        _: &Rectangle,
    ) {
        renderer.fill_quad(
            renderer::Quad {
                bounds: physical_bounds(
                    layout.bounds(),
                    layout.virtual_offset(),
                    style.scale_factor as f32,
                    self.0,
                    true,
                ),
                snap: false,
                ..Default::default()
            },
            iced::Color::from(theme.current_container().divider),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::splitter::{Edge, Splitter};
    use iced::advanced::renderer::Headless;
    use iced::{Color, Font, Pixels};

    #[test]
    fn scaled_translated_rules_and_splitters_have_equal_thickness_and_solid_junctions() {
        let mut renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(<Renderer as Headless>::new(
                Font::DEFAULT,
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let theme = Theme::default();
        let background = Color::from_rgb(1.0, 0.0, 1.0);
        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0] {
            for phase in [0.0, 0.125, 0.5, 0.875] {
                // Cards, overlay inspector, editor, and docked inspector.
                for (edge, left_width, right_x, clip_left, clip_right) in [
                    (Edge::Trailing, 101.0, 101.0, true, true),
                    (Edge::Leading, 100.0, 100.0, true, true),
                    (Edge::Trailing, 100.0, 101.0, true, false),
                    (Edge::Leading, 101.0, 101.0, false, true),
                ] {
                    let shift = Vector::new(phase, phase);
                    let extent = Size::new(201.0, 80.0);
                    renderer.reset(Rectangle::with_size(extent));
                    let mut parts: Vec<(Element<'_, ()>, Rectangle, Rectangle)> = vec![
                        (
                            horizontal(),
                            Rectangle {
                                x: 0.0,
                                y: 43.0,
                                // Cards overlay their edge splitter on the
                                // surface, including its horizontal rule.
                                width: left_width,
                                height: 1.0,
                            },
                            Rectangle {
                                width: left_width - phase,
                                ..Rectangle::with_size(extent)
                            },
                        ),
                        (
                            horizontal(),
                            Rectangle {
                                x: right_x,
                                y: 43.0,
                                width: 100.0,
                                height: 1.0,
                            },
                            Rectangle {
                                x: right_x - phase,
                                width: 100.0,
                                ..Rectangle::with_size(extent)
                            },
                        ),
                        (
                            Splitter::new(
                                iced::id::Id::new("border.test"),
                                "Test border",
                                100.0,
                                50.0,
                                150.0,
                                edge,
                                false,
                                |_| (),
                                || (),
                            )
                            .into(),
                            Rectangle {
                                x: 100.0,
                                y: 0.0,
                                width: 1.0,
                                height: 80.0,
                            },
                            match edge {
                                Edge::Trailing => Rectangle {
                                    width: 101.0 - phase,
                                    ..Rectangle::with_size(extent)
                                },
                                Edge::Leading => Rectangle {
                                    x: 100.0 - phase,
                                    width: 101.0,
                                    ..Rectangle::with_size(extent)
                                },
                            },
                        ),
                    ];
                    if !clip_left {
                        parts[0].2 = Rectangle::with_size(extent);
                    }
                    if !clip_right {
                        parts[1].2 = Rectangle::with_size(extent);
                    }
                    if !clip_left || !clip_right {
                        parts[2].2 = Rectangle::with_size(extent);
                    }
                    for (part, bounds, clip) in &mut parts {
                        let mut tree = Tree::new(part.as_widget());
                        let node = part
                            .as_widget_mut()
                            .layout(
                                &mut tree,
                                &renderer,
                                &layout::Limits::new(Size::ZERO, bounds.size()),
                            )
                            .move_to(bounds.position());
                        renderer.with_layer(*clip, |renderer| {
                            renderer.with_translation(-shift, |renderer| {
                                part.as_widget().draw(
                                    &tree,
                                    renderer,
                                    &theme,
                                    &renderer::Style {
                                        scale_factor: scale as f64,
                                        ..Default::default()
                                    },
                                    Layout::new(&node).with_virtual_offset(shift),
                                    mouse::Cursor::Unavailable,
                                    &Rectangle::with_size(extent),
                                );
                            });
                        });
                    }
                    let size = Size::new(
                        (extent.width * scale).ceil() as u32,
                        (extent.height * scale).ceil() as u32,
                    );
                    let pixels = renderer.screenshot(size, scale, background);
                    let rgb = |x: u32, y: u32| {
                        let index = ((y * size.width + x) * 4) as usize;
                        [pixels[index], pixels[index + 1], pixels[index + 2]]
                    };
                    let xs: Vec<_> = (0..size.width)
                        .filter(|&x| rgb(x, (60.0 * scale) as u32) != [255, 0, 255])
                        .collect();
                    let ys: Vec<_> = (0..size.height)
                        .filter(|&y| rgb((40.0 * scale) as u32, y) != [255, 0, 255])
                        .collect();
                    let thickness = scale.round() as usize;
                    assert_eq!(
                        xs.len(),
                        thickness,
                        "vertical: scale={scale}, phase={phase}, edge={edge:?}"
                    );
                    assert_eq!(
                        ys.len(),
                        thickness,
                        "horizontal: scale={scale}, phase={phase}, edge={edge:?}"
                    );
                    let color = rgb(xs[0], (60.0 * scale) as u32);
                    for &y in &ys {
                        for x in (95.0 * scale) as u32..(106.0 * scale) as u32 {
                            assert_eq!(
                                rgb(x, y),
                                color,
                                "junction: scale={scale}, phase={phase}, edge={edge:?}, x={x}, y={y}"
                            );
                        }
                    }
                }
            }
        }
    }
}
