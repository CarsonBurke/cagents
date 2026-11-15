//! The anchored popup surface and its menu rows.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use super::control_style;
use crate::design::{self, space};

/// Shared anchored popup surface. COSMIC owns placement, outside-click dismissal
/// and overlay hit testing; all application popovers share the same frame.
pub fn popover<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    content: Option<Element<'a, Message>>,
    position: widget::popover::Position,
    on_close: Message,
) -> Element<'a, Message> {
    framed(
        anchor,
        content,
        position,
        on_close,
        iced::Padding::new(space::XS),
    )
}

/// A [`popover`] framing a [`popover_scroll`]: a body that scrolls as a whole.
///
/// The viewport runs to the frame's border, so content leaves through the
/// popover's edge and fades into it, rather than being cut off a padding's
/// width short of it. The sides keep the frame's inset, which holds the fade
/// clear of the rounded corners; the scrollbar takes its rail out of the
/// trailing side.
pub fn scrolling_popover<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    content: Option<Element<'a, Message>>,
    position: widget::popover::Position,
    on_close: Message,
) -> Element<'a, Message> {
    framed(
        anchor,
        content,
        position,
        on_close,
        iced::Padding {
            top: design::BORDER_WIDTH,
            bottom: design::BORDER_WIDTH,
            left: SCROLLING_FRAME,
            right: SCROLLING_FRAME - design::SCROLLBAR_GUTTER,
        },
    )
}

/// The scrolling body of a [`scrolling_popover`], at most `max_height` tall,
/// with its content `inset` from every side of the popover's frame.
///
/// The vertical inset scrolls with the content, so the first and last items
/// sit `inset` from the frame until it scrolls, and what passes an edge fades
/// into the popover. `inset` is at least [`SCROLLING_FRAME`].
pub fn popover_scroll<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    id: widget::Id,
    max_height: f32,
    inset: f32,
) -> Element<'a, Message> {
    let side = (inset - SCROLLING_FRAME).max(0.0);
    let end = inset - design::BORDER_WIDTH;
    widget::container(crate::interaction::overflow_fade_y(
        widget::scrollable(
            widget::container(content).padding(design::with_vertical_rail(iced::Padding {
                top: end,
                bottom: end,
                left: side,
                right: side,
            })),
        )
        .id(id)
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .height(Length::Shrink),
        design::component_base,
    ))
    .max_height(max_height)
    .into()
}

/// How far a [`scrolling_popover`]'s scroll region sits from each side of
/// the frame. A fade drawn this far in stays inside the rounded corners.
pub const SCROLLING_FRAME: f32 = space::S;

fn framed<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    content: Option<Element<'a, Message>>,
    position: widget::popover::Position,
    on_close: Message,
    padding: iced::Padding,
) -> Element<'a, Message> {
    let mut popup = widget::popover(anchor)
        .position(position)
        .on_close(on_close);
    if let Some(content) = content {
        // iced masks the cursor beneath an overlay only when its surface
        // reports a mouse interaction. Include the frame's padding and inert
        // content, which otherwise report None and expose controls beneath.
        popup = popup.popup(crate::interaction::opaque(
            widget::container(content)
                .padding(padding)
                .class(design::elevated()),
        ));
    }
    popup.into()
}

/// A full-width menu action with a stable icon column and a secondary description.
pub fn popover_item<'a, Message: Clone + 'a>(
    icon: impl Into<widget::icon::Handle>,
    title: &'a str,
    description: impl Into<Cow<'a, str>> + 'a,
    on_press: Message,
) -> widget::button::Button<'a, Message> {
    widget::button::custom(
        widget::row::with_children(vec![
            widget::container(widget::icon(icon.into()).size(design::STATUS_ICON_SIZE))
                .class(design::transparent())
                .center_x(design::DENSE_ROW_HEIGHT)
                .center_y(design::ROW_HEIGHT + space::XS)
                .into(),
            widget::column::with_children(vec![
                design::body(title).into(),
                design::caption(description).into(),
            ])
            .spacing(space::XXS)
            .width(Length::Fill)
            .into(),
        ])
        .spacing(space::S)
        .align_y(iced::Alignment::Center),
    )
    .name(title)
    .class(control_style(false, None))
    .padding([space::XS + space::XXS, space::S])
    .width(Length::Fill)
    .on_press(on_press)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;

    /// A Work card's actions menu is a popover inside the card's
    /// `button::custom`; it must open beneath its anchor, not displaced by
    /// the card's origin.
    #[test]
    fn popovers_inside_an_offset_button_open_beneath_their_anchor() {
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};
        use cosmic::iced::{Padding, Point, Rectangle, Size, Vector};

        let renderer = headless_renderer();
        let screen = Size::new(600.0, 400.0);
        let anchor = Size::new(24.0, 24.0);
        let mut element: Element<'_, ()> = widget::container(
            widget::button::custom(
                widget::row::with_children(vec![
                    iced::widget::Space::new().width(100).height(24).into(),
                    popover(
                        iced::widget::Space::new()
                            .width(anchor.width)
                            .height(anchor.height),
                        Some(iced::widget::Space::new().width(120).height(60).into()),
                        widget::popover::Position::Bottom,
                        (),
                    ),
                ])
                .spacing(8),
            )
            .padding(10)
            .on_press(()),
        )
        .padding(Padding {
            left: 140.0,
            top: 90.0,
            ..Padding::ZERO
        })
        .into();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, screen),
        );
        let anchor = Rectangle::new(Point::new(140.0 + 10.0 + 100.0 + 8.0, 90.0 + 10.0), anchor);
        let viewport = Rectangle::with_size(screen);
        for camera in [Vector::ZERO, Vector::new(-60.0, 35.0)] {
            let mut overlay = element
                .as_widget_mut()
                .overlay(&mut tree, Layout::new(&node), &renderer, &viewport, camera)
                .expect("an open popover raises its surface");
            // An overlay group spans the screen; the surface is its child.
            let surface = overlay
                .as_overlay_mut()
                .layout(&renderer, screen)
                .children()[0]
                .bounds();
            let anchor = anchor + camera;
            assert!(
                (surface.center_x() - anchor.center_x()).abs() <= 1.0
                    && (surface.y - (anchor.y + anchor.height)).abs() <= 1.0,
                "popover {surface:?} is not beneath its anchor {anchor:?}"
            );
        }
    }

    #[test]
    fn queued_overlay_clicks_use_the_position_before_later_motion() {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Size, mouse};

        #[derive(Clone, Debug, PartialEq)]
        enum Action {
            Press,
            Close,
        }

        let mut renderer = headless_renderer();
        let inside = Point::new(60.0, 60.0);
        let outside = Point::new(180.0, 160.0);
        for (start, finish, expected_press) in [(inside, outside, false), (outside, inside, true)] {
            let base = iced::widget::mouse_area(iced::widget::Space::new().width(200).height(200))
                .on_press(Action::Press);
            let element = popover(
                base,
                Some(iced::widget::Space::new().width(100).height(60).into()),
                widget::popover::Position::Point(Point::new(40.0, 40.0)),
                Action::Close,
            );
            let mut ui = UserInterface::build(
                element,
                Size::new(200.0, 200.0),
                Cache::new(),
                &mut renderer,
            );
            let mut messages = Vec::new();
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::CursorMoved { position: start }),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::CursorMoved { position: finish }),
                ],
                mouse::Cursor::Available(finish),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(
                messages.contains(&Action::Press),
                expected_press,
                "{start:?} to {finish:?}"
            );
        }
    }

    #[test]
    fn popover_inert_surface_blocks_underlying_hover_and_clicks() {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Size, mouse};

        #[derive(Clone, Debug, PartialEq)]
        enum Action {
            Hover,
            Press,
            Close,
        }

        let mut renderer = headless_renderer();
        // The popup covers x=40..148, y=40..108. Probe its padding,
        // disabled control and empty area, then the exposed background.
        for (point, covered) in [
            (Point::new(42.0, 42.0), true),
            (Point::new(60.0, 55.0), true),
            (Point::new(60.0, 95.0), true),
            (Point::new(180.0, 160.0), false),
        ] {
            let base = iced::widget::mouse_area(iced::widget::Space::new().width(200).height(200))
                .on_enter(Action::Hover)
                .on_press(Action::Press);
            let content = widget::column::with_children(vec![
                widget::button::text("Disabled")
                    .width(100)
                    .height(30)
                    .into(),
                iced::widget::Space::new().width(100).height(30).into(),
            ]);
            let element = popover(
                base,
                Some(content.into()),
                widget::popover::Position::Point(Point::new(40.0, 40.0)),
                Action::Close,
            );
            let mut ui = UserInterface::build(
                element,
                Size::new(200.0, 200.0),
                Cache::new(),
                &mut renderer,
            );
            let mut messages = Vec::new();
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::CursorMoved { position: point }),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                ],
                mouse::Cursor::Available(point),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(messages.contains(&Action::Hover), !covered, "{point:?}");
            assert_eq!(messages.contains(&Action::Press), !covered, "{point:?}");
            assert!(!messages.contains(&Action::Close));
        }
    }

    #[test]
    fn popover_dismisses_when_clicking_outside_its_anchor_and_surface() {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Size, mouse};

        let mut renderer = headless_renderer();
        let element = popover(
            iced::widget::Space::new().width(20).height(20),
            Some(iced::widget::Space::new().width(100).height(60).into()),
            widget::popover::Position::Bottom,
            (),
        );
        let mut ui = UserInterface::build(
            element,
            Size::new(200.0, 200.0),
            Cache::new(),
            &mut renderer,
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &[Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Left,
            ))],
            mouse::Cursor::Available(Point::new(180.0, 160.0)),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, vec![()]);
    }
}
