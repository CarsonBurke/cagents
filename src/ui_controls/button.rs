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
        .height(design::DENSE_ROW_HEIGHT)
        .on_press_maybe(on_press);
        if let Some(id) = id {
            button = button.id(id);
        }
        button.into()
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
            ink: design::mix(surface, on, 0.42),
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
}
