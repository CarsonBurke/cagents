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
    let mut popup = widget::popover(anchor)
        .position(position)
        .on_close(on_close);
    if let Some(content) = content {
        // iced masks the cursor beneath an overlay only when its surface
        // reports a mouse interaction. Include the frame's padding and inert
        // content, which otherwise report None and expose controls beneath.
        popup = popup.popup(crate::interaction::opaque(
            widget::container(content)
                .padding(space::XS)
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
