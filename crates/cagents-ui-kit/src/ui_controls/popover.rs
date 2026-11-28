//! The anchored popup surface and its menu rows.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use super::control_style;
use crate::design::{self, space};

/// How an open popover closes, and where the keyboard goes when it does.
///
/// Escape, or a Close control in the popover, *dismisses* it with the
/// message built from its opener: wherever keyboard focus was when it
/// opened, usually the control that opened it. Its handler hands focus back
/// there with [`return_focus`], so a keyboard reader keeps their place.
///
/// A press outside *closes* it with `outside`, and focus stays wherever that
/// press put it: moving it to the opener would take the keyboard from a
/// field the press had just focused.
#[derive(Clone, Debug)]
pub struct Close<Message> {
    outside: Message,
    dismiss: Message,
    /// Whether Escape dismisses it wherever the keyboard is, rather than
    /// only from inside it.
    from_anywhere: bool,
}

impl<Message> Close<Message> {
    /// Closes with `outside` on a press outside, and with `dismiss`, told
    /// the `opener` to return focus to, on Escape.
    pub fn new(
        outside: Message,
        opener: widget::Id,
        dismiss: impl FnOnce(widget::Id) -> Message,
    ) -> Self {
        Self {
            outside,
            dismiss: dismiss(opener),
            from_anywhere: true,
        }
    }

    /// Answers Escape only while the keyboard is inside the popover, as
    /// one that opened by itself must: the keyboard is still wherever the
    /// reader was, such as a terminal whose program reads Escape.
    #[must_use]
    pub fn only_from_inside(mut self) -> Self {
        self.from_anywhere = false;
        self
    }

    /// The message a Close control in the popover sends: the one Escape
    /// sends.
    pub fn dismiss(&self) -> Message
    where
        Message: Clone,
    {
        self.dismiss.clone()
    }
}

/// Focus `opener` again, as a dismissed popover's handler does: see
/// [`Close`].
pub fn return_focus<T>(opener: widget::Id) -> iced::Task<T> {
    iced::widget::operation::focus(opener)
}

/// `content` answering Escape with `dismiss`, as a popover's surface does:
/// first when the keyboard is inside it, before a focused field can take
/// the key to blur itself, and otherwise when nothing in it claims the key.
///
/// A popover's surface is an overlay, so it sees the key before the window
/// beneath, such as a terminal whose program reads Escape. While a dialog
/// is open over it, the surface is not drawn, and the dialog answers.
pub fn escape_dismisses<'a, Message: Clone + 'a>(
    content: impl Into<Element<'a, Message>>,
    dismiss: Message,
) -> Element<'a, Message> {
    let unclaimed = dismiss.clone();
    crate::interaction::focused_escape(
        crate::interaction::unclaimed_key(content, move |event| {
            is_escape(event).then(|| unclaimed.clone())
        }),
        move || dismiss.clone(),
    )
}

fn is_escape(event: &cosmic::iced::keyboard::Event) -> bool {
    matches!(
        event,
        cosmic::iced::keyboard::Event::KeyPressed {
            key: cosmic::iced::keyboard::Key::Named(cosmic::iced::keyboard::key::Named::Escape),
            ..
        }
    )
}

/// Shared anchored popup surface. COSMIC owns placement, outside-click dismissal
/// and overlay hit testing; all application popovers share the same frame.
pub fn popover<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    content: Option<Element<'a, Message>>,
    position: widget::popover::Position,
    close: Close<Message>,
) -> Element<'a, Message> {
    framed(
        anchor,
        content,
        position,
        close,
        iced::Padding::new(space::XS),
        design::elevated(),
    )
}

/// A context menu that the application opens itself, at `position`, drawn as
/// COSMIC draws its native context menu: rows the menu's width that highlight
/// whole, in the menu's text colour, with a dimmed row for an action that is
/// unavailable. Build its rows with [`menu_item`].
///
/// Prefer [`crate::interaction::context_menu`], which opens the native menu
/// on a right click by itself. This is for a surface that decides whether a
/// right click is its own first, such as a terminal whose program may be
/// reading the mouse.
pub fn popover_menu<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    items: Option<Vec<MenuItem<'a, Message>>>,
    position: widget::popover::Position,
    close: Close<Message>,
) -> Element<'a, Message> {
    let content = items.map(menu_rows);
    framed(
        anchor,
        content,
        position,
        close,
        iced::Padding::new(design::BORDER_WIDTH),
        cosmic::theme::Container::custom(menu_surface),
    )
}

/// A [`popover_menu`]'s rows, sized as the native menu sizes them.
pub fn menu_rows<'a, Message: 'a>(items: Vec<MenuItem<'a, Message>>) -> Element<'a, Message> {
    // As wide as the native menu sizes itself for its longest row.
    let width = items
        .iter()
        .map(|item| item.width)
        .fold(MENU_MIN_WIDTH, f32::max);
    // The native menu's column, which rounds the first and last rows'
    // highlight into the surface's corners.
    widget::menu::menu_column::MenuColumn::from_vec(
        items.into_iter().map(|item| item.element).collect(),
    )
    .width(Length::Fixed(width))
    .into()
}

/// The narrowest a native menu draws, and the widest it grows for its
/// labels.
const MENU_MIN_WIDTH: f32 = 240.0;
const MENU_MAX_WIDTH: f32 = 480.0;

/// A row of a [`popover_menu`].
pub struct MenuItem<'a, Message> {
    element: Element<'a, Message>,
    width: f32,
}

/// A [`popover_menu`] row: `label`, with `hint` (such as the gesture or chord
/// that does the same) at its trailing edge, pressed for `on_press`, or
/// dimmed and inert without it. It is the native menu's own row.
pub fn menu_item<'a, Message: Clone + 'a>(
    label: impl Into<Cow<'a, str>>,
    hint: Option<&'a str>,
    on_press: Option<Message>,
) -> MenuItem<'a, Message> {
    let label = label.into();
    let hint = hint.unwrap_or_default();
    // The native menu's estimate of the row's width, so the two agree.
    let width = ((label.chars().count() + hint.chars().count()) as f32 * 8.5 + 80.0)
        .clamp(MENU_MIN_WIDTH, MENU_MAX_WIDTH)
        .trunc();
    let row = widget::menu::menu_button(vec![
        // The native rows keep an icon's column even without one.
        iced::widget::Space::new().width(14).into(),
        iced::widget::Space::new().width(space::XXS).into(),
        widget::text(label.clone()).into(),
        iced::widget::Space::new().width(Length::Fill).into(),
        widget::text(hint)
            .class(cosmic::theme::Text::Custom(menu_hint))
            .into(),
    ])
    .name(label)
    .on_press_maybe(on_press);
    MenuItem {
        element: row.into(),
        width,
    }
}

/// A [`popover_menu`] rule between groups of rows, as the native menu draws
/// one.
pub fn menu_divider<'a, Message: 'a>() -> MenuItem<'a, Message> {
    MenuItem {
        element: widget::divider::horizontal::light().into(),
        width: 0.0,
    }
}

/// The native menu's surface: the background's component, its divider for
/// an edge, and small corners. Opaque even under blur, as the native menu
/// is when it draws inside the window rather than as its own surface.
fn menu_surface(theme: &cosmic::Theme) -> widget::container::Style {
    let cosmic = theme.cosmic();
    let component = &cosmic.background(false).component;
    widget::container::Style {
        background: Some(iced::Background::Color(component.base.into())),
        text_color: Some(component.on.into()),
        icon_color: Some(component.on.into()),
        border: iced::Border {
            color: component.divider.into(),
            width: design::BORDER_WIDTH,
            radius: cosmic.corner_radii.radius_s.into(),
        },
        ..Default::default()
    }
}

/// A native menu row's shortcut, in its label's colour at three quarters.
fn menu_hint(theme: &cosmic::Theme) -> iced::widget::text::Style {
    let mut color = theme.cosmic().background(theme.transparent).component.on;
    color.alpha *= 0.75;
    iced::widget::text::Style {
        color: Some(color.into()),
        ..Default::default()
    }
}

/// A [`popover`] framing a [`popover_scroll`]: a body that scrolls as a whole.
///
/// The viewport runs to the frame's border, so content leaves through the
/// popover's edge and fades into it, rather than being cut off a padding's
/// width short of it. The sides keep the frame's inset, which holds the fade
/// and the scrollbar's thumb clear of the rounded corners.
pub fn scrolling_popover<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    content: Option<Element<'a, Message>>,
    position: widget::popover::Position,
    close: Close<Message>,
) -> Element<'a, Message> {
    framed(
        anchor,
        content,
        position,
        close,
        iced::Padding {
            top: design::BORDER_WIDTH,
            bottom: design::BORDER_WIDTH,
            left: SCROLLING_FRAME,
            right: SCROLLING_FRAME,
        },
        design::elevated(),
    )
}

/// The scrolling body of a [`scrolling_popover`], at most `max_height` tall,
/// with its content `inset` from every side of the popover's frame and the
/// scrollbar's rail besides on the trailing side.
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
/// the frame. A fade or a scrollbar thumb this far in stays inside the
/// rounded corners.
pub const SCROLLING_FRAME: f32 = space::S;

/// The width to give a [`scrolling_popover`]'s body so the popover is as
/// wide as a [`popover`] around content `width` wide: its frame is wider by
/// the difference in their side insets.
pub fn scrolling_width(width: f32) -> f32 {
    (width - 2.0 * (SCROLLING_FRAME - space::XS)).max(0.0)
}

fn framed<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    content: Option<Element<'a, Message>>,
    position: widget::popover::Position,
    close: Close<Message>,
    padding: iced::Padding,
    class: cosmic::theme::Container<'static>,
) -> Element<'a, Message> {
    let Close {
        outside,
        dismiss,
        from_anywhere,
    } = close;
    let mut popup = widget::popover(anchor).position(position).on_close(outside);
    if let Some(content) = content {
        // iced masks the cursor beneath an overlay only when its surface
        // reports a mouse interaction. Include the frame's padding and inert
        // content, which otherwise report None and expose controls beneath.
        let surface =
            crate::interaction::opaque(widget::container(content).padding(padding).class(class));
        popup = popup.popup(if from_anywhere {
            escape_dismisses(surface, dismiss)
        } else {
            crate::interaction::focused_escape(surface, move || dismiss.clone())
        });
    }
    popup.into()
}

/// A full-width menu action with a stable icon column and a secondary description.
pub fn popover_item<'a, Message: Clone + 'a>(
    icon: impl Into<widget::icon::Handle>,
    title: impl Into<Cow<'a, str>>,
    description: impl Into<Cow<'a, str>> + 'a,
    on_press: Message,
) -> widget::button::Button<'a, Message> {
    let title = title.into();
    widget::button::custom(
        widget::row::with_children(vec![
            widget::container(widget::icon(icon.into()).size(design::STATUS_ICON_SIZE))
                .class(design::transparent())
                .center_x(design::DENSE_ROW_HEIGHT)
                .center_y(design::ROW_HEIGHT + space::XS)
                .into(),
            widget::column::with_children(vec![
                design::body(title.clone()).into(),
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

/// Whether `task` moves the keyboard to `control`, and off every other
/// control, at once.
#[cfg(test)]
pub(crate) fn focuses<T>(task: iced::Task<T>, control: &widget::Id) -> bool {
    use cosmic::iced::advanced::widget::operation::{Focusable, Outcome};
    use cosmic::iced::futures::{FutureExt, StreamExt};

    struct Control(bool);
    impl Focusable for Control {
        fn is_focused(&self) -> bool {
            self.0
        }
        fn focus(&mut self) {
            self.0 = true;
        }
        fn unfocus(&mut self) {
            self.0 = false;
        }
    }

    let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) else {
        return false;
    };
    let (mut asked, mut other) = (Control(false), Control(true));
    while let Some(Some(action)) = stream.next().now_or_never() {
        let cosmic::iced::runtime::Action::Widget(mut operation) = action else {
            continue;
        };
        loop {
            let bounds = cosmic::iced::Rectangle::default();
            operation.focusable(Some(control), bounds, &mut asked);
            operation.focusable(Some(&widget::Id::new("elsewhere")), bounds, &mut other);
            match operation.finish() {
                Outcome::Chain(next) => operation = next,
                _ => break,
            }
        }
    }
    asked.0 && !other.0
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
                        Close::new((), widget::Id::new("opener"), |_| ()),
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
                Close::new(Action::Close, widget::Id::new("opener"), |_| Action::Close),
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
                Close::new(Action::Close, widget::Id::new("opener"), |_| Action::Close),
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
            Close::new((), widget::Id::new("opener"), |_| ()),
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

    #[derive(Clone, Debug, PartialEq)]
    enum Closed {
        Outside,
        Dismissed(widget::Id),
        Typed,
    }

    fn escape() -> cosmic::iced::Event {
        use cosmic::iced::keyboard;
        let key = keyboard::Key::Named(keyboard::key::Named::Escape);
        cosmic::iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })
    }

    /// The messages `events` raise in an open popover whose surface holds
    /// a search field, focused when `focused`, over a base that takes keys.
    fn closes(events: &[cosmic::iced::Event], focused: bool) -> Vec<Closed> {
        closes_with(events, focused, true)
    }

    fn closes_with(
        events: &[cosmic::iced::Event],
        focused: bool,
        from_anywhere: bool,
    ) -> Vec<Closed> {
        use cosmic::iced::advanced::clipboard;
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Point, Size, mouse};

        let mut renderer = headless_renderer();
        let field = widget::Id::new("popover.search");
        // The opener, which, like a terminal beneath, reads every key.
        let base = crate::interaction::unclaimed_key(
            iced::widget::Space::new().width(20).height(20),
            |_| Some(Closed::Typed),
        );
        let close = Close::new(
            Closed::Outside,
            widget::Id::new("opener"),
            Closed::Dismissed,
        );
        let element = popover(
            base,
            Some(
                widget::text_input("Search", "")
                    .id(field.clone())
                    .on_input(|_| Closed::Typed)
                    .width(100)
                    .into(),
            ),
            widget::popover::Position::Point(Point::new(40.0, 40.0)),
            if from_anywhere {
                close
            } else {
                close.only_from_inside()
            },
        );
        let mut ui = UserInterface::build(
            element,
            Size::new(200.0, 200.0),
            Cache::new(),
            &mut renderer,
        );
        if focused {
            let mut focus = cosmic::iced::advanced::widget::operation::focusable::focus(field);
            ui.operate(&renderer, &mut focus);
        }
        let mut messages = Vec::new();
        let _ = ui.update(
            events,
            mouse::Cursor::Available(Point::new(180.0, 160.0)),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        messages
    }

    /// Escape dismisses the popover, naming the opener to return to, from
    /// wherever the keyboard is: before a focused field in it can take the
    /// key to blur itself, and before the window beneath can read it.
    #[test]
    fn escape_dismisses_back_to_the_opener_before_the_field_or_window_takes_it() {
        let dismissed = vec![Closed::Dismissed(widget::Id::new("opener"))];
        assert_eq!(closes(&[escape()], false), dismissed, "nothing focused");
        assert_eq!(closes(&[escape()], true), dismissed, "its field focused");
    }

    /// A popover that opened by itself answers Escape only from inside it,
    /// leaving the key to the window the reader is still in.
    #[test]
    fn a_popover_open_by_itself_leaves_escape_outside_it_to_the_window() {
        assert_eq!(
            closes_with(&[escape()], false, false),
            vec![Closed::Typed],
            "the window beneath reads it"
        );
        assert_eq!(
            closes_with(&[escape()], true, false),
            vec![Closed::Dismissed(widget::Id::new("opener"))],
            "from inside, it dismisses"
        );
    }

    /// A press outside closes the popover without naming anywhere to
    /// return to: focus stays where the press put it.
    #[test]
    fn a_press_outside_closes_without_returning_focus() {
        use cosmic::iced::{Event, mouse};
        assert_eq!(
            closes(
                &[Event::Mouse(mouse::Event::ButtonPressed(
                    mouse::Button::Left
                ))],
                false
            ),
            vec![Closed::Outside]
        );
    }

    /// The task a dismissal's handler returns focuses the opener.
    #[test]
    fn return_focus_focuses_the_opener() {
        let opener = widget::Id::new("opener");
        assert!(focuses(return_focus::<()>(opener.clone()), &opener));
        assert!(!focuses(iced::Task::<()>::none(), &opener));
    }
}
