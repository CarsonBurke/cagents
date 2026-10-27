//! The shell's labelled buttons.
//!
//! A command with a name is a compact, squared control at the shell's own
//! type size, in one of four weights. Stock COSMIC buttons are tall pills at
//! the desktop's body size; beside the workbench's 13px rows they read as a
//! second application, and every dialog that mixed the two had no single
//! control height to align to.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Alignment, Length},
    widget,
};

use crate::design::{self, space};

/// How much of the surface's attention a button asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weight {
    /// The one action a surface exists to offer.
    Primary,
    /// An ordinary command.
    Secondary,
    /// The action that discards or deletes.
    Destructive,
    /// A command that should stay out of the way until it is wanted.
    Quiet,
}

pub struct Button<'a, Message> {
    label: Cow<'a, str>,
    weight: Weight,
    name: Option<Cow<'a, str>>,
    icon: Option<widget::icon::Handle>,
    on_press: Option<Message>,
    width: Length,
    id: Option<widget::Id>,
}

/// The one action a surface exists to offer. At most one per surface.
pub fn primary<'a, Message>(label: impl Into<Cow<'a, str>>) -> Button<'a, Message> {
    Button::new(label, Weight::Primary)
}

/// An ordinary command.
pub fn secondary<'a, Message>(label: impl Into<Cow<'a, str>>) -> Button<'a, Message> {
    Button::new(label, Weight::Secondary)
}

/// The action that discards or deletes something.
pub fn destructive<'a, Message>(label: impl Into<Cow<'a, str>>) -> Button<'a, Message> {
    Button::new(label, Weight::Destructive)
}

/// A command without a surface of its own until it is hovered.
pub fn quiet<'a, Message>(label: impl Into<Cow<'a, str>>) -> Button<'a, Message> {
    Button::new(label, Weight::Quiet)
}

impl<'a, Message> Button<'a, Message> {
    fn new(label: impl Into<Cow<'a, str>>, weight: Weight) -> Self {
        Self {
            label: label.into(),
            weight,
            name: None,
            icon: None,
            on_press: None,
            width: Length::Shrink,
            id: None,
        }
    }

    /// A button without a press message is drawn and announced as disabled.
    pub fn on_press(mut self, message: Message) -> Self {
        self.on_press = Some(message);
        self
    }

    pub fn on_press_maybe(mut self, message: Option<Message>) -> Self {
        self.on_press = message;
        self
    }

    /// The accessible name, where the visible label alone would not say what
    /// the button acts on. Defaults to the label.
    pub fn name(mut self, name: impl Into<Cow<'a, str>>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// A glyph ahead of the label.
    pub fn icon(mut self, icon: impl Into<widget::icon::Handle>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// A button wider than its label centers the label.
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn id(mut self, id: widget::Id) -> Self {
        self.id = Some(id);
        self
    }
}

impl<'a, Message: Clone + 'a> From<Button<'a, Message>> for Element<'a, Message> {
    fn from(button: Button<'a, Message>) -> Self {
        let Button {
            label,
            weight,
            name,
            icon,
            on_press,
            width,
            id,
        } = button;
        let mut content = widget::row::with_capacity(2)
            .spacing(space::XS + space::XXS)
            .align_y(Alignment::Center);
        if let Some(icon) = icon {
            content = content.push(widget::icon(icon).size(design::ROW_ICON_SIZE));
        }
        content = content
            .push(design::heading(label.clone()).wrapping(iced::widget::text::Wrapping::None));
        let mut button = widget::button::custom(
            widget::container(content)
                // COSMIC's transparent container resets the inherited ink.
                // This wrapper only aligns, so it must pass the button's
                // status colors through to the label and glyph.
                .class(design::transparent())
                .center_x(width)
                .center_y(Length::Fill),
        )
        .name(name.unwrap_or(label))
        .class(style(weight))
        .padding([0.0, space::M])
        .width(width)
        .height(design::DENSE_ROW_HEIGHT)
        .on_press_maybe(on_press);
        if let Some(id) = id {
            button = button.id(id);
        }
        crate::interaction::non_repeating_activation(button)
    }
}

/// The fill, edge and ink of one button weight in one interaction state.
struct Paint {
    fill: Option<iced::Color>,
    edge: Option<iced::Color>,
    ink: iced::Color,
}

#[derive(Clone, Copy)]
enum Status {
    Active,
    Hovered,
    Pressed,
    Disabled,
}

fn paint(weight: Weight, status: Status, theme: &cosmic::Theme) -> Paint {
    let cosmic = theme.cosmic();
    let container = theme.current_container();
    let surface: iced::Color = container.base.into();
    let on: iced::Color = container.on.into();
    // How far a neutral fill lifts off the surface in each state.
    let lift = |amount| design::mix(surface, on, amount);
    if matches!(status, Status::Disabled) {
        return Paint {
            fill: (weight != Weight::Quiet).then(|| lift(0.05)),
            edge: None,
            ink: design::disabled_color(theme),
        };
    }
    let filled = |component: &cosmic::cosmic_theme::Component| Paint {
        fill: Some(
            match status {
                Status::Hovered => component.hover,
                Status::Pressed => component.pressed,
                Status::Active | Status::Disabled => component.base,
            }
            .into(),
        ),
        edge: None,
        ink: component.on.into(),
    };
    match weight {
        Weight::Primary => filled(&cosmic.accent_button),
        Weight::Destructive => filled(&cosmic.destructive_button),
        Weight::Secondary => Paint {
            fill: Some(lift(match status {
                Status::Hovered => 0.11,
                Status::Pressed => 0.15,
                Status::Active | Status::Disabled => 0.07,
            })),
            edge: Some(design::hairline(theme)),
            ink: on,
        },
        Weight::Quiet => Paint {
            fill: match status {
                Status::Hovered => Some(lift(0.08)),
                Status::Pressed => Some(lift(0.12)),
                Status::Active | Status::Disabled => None,
            },
            edge: None,
            ink: on,
        },
    }
}

fn style(weight: Weight) -> cosmic::theme::Button {
    let appearance = move |status: Status, focused: bool, theme: &cosmic::Theme| {
        let Paint { fill, edge, ink } = paint(weight, status, theme);
        widget::button::Style {
            background: fill.map(iced::Background::Color),
            border_radius: design::RADIUS_S.into(),
            border_width: if edge.is_some() {
                design::BORDER_WIDTH
            } else {
                0.0
            },
            border_color: edge.unwrap_or(iced::Color::TRANSPARENT),
            // Keyboard focus is a ring outside the control, so it reads the
            // same on a filled button as on a quiet one.
            outline_width: if focused { design::BORDER_WIDTH } else { 0.0 },
            outline_color: if focused {
                design::accent_color(theme)
            } else {
                iced::Color::TRANSPARENT
            },
            icon_color: Some(ink),
            text_color: Some(ink),
            ..design::button_style()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(move |focused, theme| appearance(Status::Active, focused, theme)),
        disabled: Box::new(move |theme| appearance(Status::Disabled, false, theme)),
        hovered: Box::new(move |focused, theme| appearance(Status::Hovered, focused, theme)),
        pressed: Box::new(move |focused, theme| appearance(Status::Pressed, focused, theme)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_share_the_dense_control_height_and_disable_without_a_message() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(400.0, 80.0));
        for (constructor, message) in [
            (primary as fn(&'static str) -> Button<'static, ()>, Some(())),
            (secondary, Some(())),
            (destructive, None),
            (quiet, Some(())),
        ] {
            let mut element: Element<'_, ()> = constructor("Apply").on_press_maybe(message).into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            assert_eq!(node.size().height, design::DENSE_ROW_HEIGHT);
            let cursor = mouse::Cursor::Available(node.bounds().center());
            let mut messages = Vec::new();
            for event in [
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(event),
                    Layout::new(&node),
                    cursor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            assert_eq!(messages.len(), usize::from(message.is_some()));
        }
    }

    fn enter_event(repeat: bool) -> iced::Event {
        use iced::keyboard;
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat,
        })
    }

    #[test]
    fn labelled_buttons_keep_fresh_keyboard_pointer_and_assistive_activations() {
        use iced::{
            Event, Size,
            advanced::{clipboard, widget::operation},
            mouse,
            runtime::{UserInterface, user_interface::Cache},
        };
        use iced_accessibility::{
            A11yId,
            accesskit::{Action, ActionRequest, TreeId},
        };
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        for constructor in [
            primary as fn(&'static str) -> Button<'static, u8>,
            secondary,
            destructive,
            quiet,
        ] {
            for enabled in [false, true] {
                let id = widget::Id::new("activation.primary");
                let mut ui = UserInterface::build(
                    constructor("Apply")
                        .id(id.clone())
                        .on_press_maybe(enabled.then_some(1)),
                    Size::new(320.0, 80.0),
                    Cache::new(),
                    &mut renderer,
                );
                ui.operate(
                    &renderer,
                    &mut operation::focusable::focus::<()>(id.clone()),
                );
                let mut messages = Vec::new();
                ui.update(
                    &[enter_event(true)],
                    mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert!(
                    messages.is_empty(),
                    "a held Enter cannot activate a labelled button"
                );
                for key in [
                    iced::keyboard::key::Named::Tab,
                    iced::keyboard::key::Named::Escape,
                ] {
                    let mut event = enter_event(false);
                    if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        key: native,
                        modified_key,
                        ..
                    }) = &mut event
                    {
                        *native = iced::keyboard::Key::Named(key.clone());
                        *modified_key = iced::keyboard::Key::Named(key);
                    }
                    let (_, statuses) = ui.update(
                        &[event],
                        mouse::Cursor::Unavailable,
                        &mut renderer,
                        &mut clipboard::Null,
                        &mut messages,
                    );
                    assert_eq!(statuses, [iced::event::Status::Ignored]);
                    assert!(
                        messages.is_empty(),
                        "button filtering must leave Tab and Escape to their owners"
                    );
                }
                ui.update(
                    &[enter_event(false)],
                    mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert_eq!(messages, if enabled { vec![1] } else { vec![] });
                messages.clear();
                // Assistive Click remains a real activation and clears the native press.
                let event = Event::A11y(
                    id.clone(),
                    ActionRequest {
                        action: Action::Click,
                        target_tree: TreeId::ROOT,
                        target_node: A11yId::from(id).into(),
                        data: None,
                    },
                );
                ui.update(
                    &[event],
                    mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert_eq!(messages, if enabled { vec![1] } else { vec![] });
                messages.clear();
                let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
                let bounds = nodes
                    .root()
                    .iter()
                    .chain(nodes.children())
                    .find(|node| {
                        node.node().role() == iced_accessibility::accesskit::Role::Button
                            && node.node().label() == Some("Apply")
                    })
                    .unwrap()
                    .node()
                    .bounds()
                    .unwrap();
                let cursor = mouse::Cursor::Available(iced::Point::new(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                ));
                ui.update(
                    &[
                        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                    ],
                    cursor,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert_eq!(messages, if enabled { vec![1] } else { vec![] });
            }
        }
    }

    #[test]
    fn opening_native_batch_does_not_commit_replacement_and_held_enter_repeat_is_inert() {
        use iced::{
            Event, Size,
            advanced::{clipboard, widget::operation},
            keyboard, mouse,
            runtime::{UserInterface, user_interface::Cache},
        };
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let size = Size::new(320.0, 80.0);
        for pointer_down_opener in [false, true] {
            let open_id = widget::Id::new("activation.open");
            let original: Element<'_, u8> = if pointer_down_opener {
                widget::button::custom(crate::design::body("Open"))
                    .id(open_id.clone())
                    .on_press_down(1)
                    .into()
            } else {
                primary("Open").id(open_id.clone()).on_press(1).into()
            };
            let mut ui = UserInterface::build(original, size, Cache::new(), &mut renderer);
            let cursor = mouse::Cursor::Available(iced::Point::new(8.0, 8.0));
            let opening = if pointer_down_opener {
                vec![Event::Mouse(mouse::Event::ButtonPressed(
                    mouse::Button::Left,
                ))]
            } else {
                ui.operate(&renderer, &mut operation::focusable::focus::<()>(open_id));
                vec![enter_event(false)]
            };
            let mut messages = Vec::new();
            ui.update(
                &opening,
                cursor,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(
                messages,
                [1],
                "only the old widget emits the genuine opening action"
            );
            // Like iced-winit, finish the old input batch, consume its message,
            // and then rebuild the application view from the real native cache.
            let cache = ui.into_cache();
            let confirm = widget::Id::new("activation.confirm");
            let replacement = crate::ui_controls::owned(
                "confirmation",
                primary("Confirm").id(confirm.clone()).on_press(2),
            );
            let mut ui = UserInterface::build(replacement, size, cache, &mut renderer);
            ui.operate(&renderer, &mut operation::focusable::focus::<()>(confirm));
            messages.clear();
            ui.update(
                &[Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                ))],
                cursor,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(
                messages.is_empty(),
                "the actual redraw second pass is not an input replay"
            );
            let release = if pointer_down_opener {
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            } else {
                ui.update(
                    &[enter_event(true)],
                    cursor,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                Event::Keyboard(keyboard::Event::KeyReleased {
                    key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                })
            };
            ui.update(
                &[release],
                cursor,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(
                messages.is_empty(),
                "the opener's held input cannot confirm the replacement"
            );
            ui.update(
                &[enter_event(false)],
                cursor,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(
                messages,
                [2],
                "a distinct fresh Enter completes the newly focused task"
            );
        }
    }
}
