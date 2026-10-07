//! Compact, keyboard-accessible icon targets and the flat surface they share.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use crate::{
    design::{self, Tone, space},
    interaction::tooltip,
};

/// How large an icon target is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scale {
    /// A toolbar or row control.
    #[default]
    Regular,
    /// A control that belongs to a message rather than to a toolbar, drawn at
    /// caption scale.
    Compact,
}

impl Scale {
    fn target(self) -> f32 {
        match self {
            Self::Regular => design::PRIMARY_CONTROL_SIZE,
            Self::Compact => design::COMPACT_CONTROL_SIZE,
        }
    }

    fn glyph(self) -> u16 {
        match self {
            Self::Regular => design::ROW_ICON_SIZE,
            Self::Compact => design::COMPACT_ICON_SIZE,
        }
    }
}

/// A centered, fixed-size icon target with a quiet selected surface.
///
/// The label is both its accessible name and its tooltip. The native button
/// underneath keeps keyboard focus, accessibility and the enabled cursor.
pub struct IconButton<'a, Message> {
    icon: widget::icon::Handle,
    label: Cow<'a, str>,
    scale: Scale,
    selected: bool,
    tone: Option<Tone>,
    on_press: Option<Message>,
    id: Option<widget::Id>,
}

pub fn icon_button<'a, Message>(
    icon: impl Into<widget::icon::Handle>,
    label: impl Into<Cow<'a, str>>,
) -> IconButton<'a, Message> {
    IconButton {
        icon: icon.into(),
        label: label.into(),
        scale: Scale::default(),
        selected: false,
        tone: None,
        on_press: None,
        id: None,
    }
}

impl<'a, Message> IconButton<'a, Message> {
    /// A control without a press message is drawn and announced as disabled:
    /// whether it can be pressed and whether it looks pressable are one fact.
    pub fn on_press(mut self, message: Message) -> Self {
        self.on_press = Some(message);
        self
    }

    pub fn on_press_maybe(mut self, message: Option<Message>) -> Self {
        self.on_press = message;
        self
    }

    /// Marks the control as the current choice of its group.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Draws the control at caption scale, for a control that belongs to a
    /// message rather than to a toolbar.
    pub fn compact(mut self) -> Self {
        self.scale = Scale::Compact;
        self
    }

    /// Draws the glyph in `tone` instead of the neutral ink. A semantic tone
    /// is resolved from the drawing theme, so a row builder never locks the
    /// process-global theme to name a color.
    pub fn tone(mut self, tone: impl Into<Tone>) -> Self {
        self.tone = Some(tone.into());
        self
    }

    pub fn id(mut self, id: widget::Id) -> Self {
        self.id = Some(id);
        self
    }
}

impl<'a, Message: Clone + 'a> From<IconButton<'a, Message>> for Element<'a, Message> {
    fn from(control: IconButton<'a, Message>) -> Self {
        let IconButton {
            icon,
            label,
            scale,
            selected,
            tone,
            on_press,
            id,
        } = control;
        let enabled = on_press.is_some();
        let selected = selected && enabled;
        let content = widget::container(widget::icon(icon).size(scale.glyph()))
            .class(design::transparent())
            .center(Length::Fill);
        let mut button = widget::button::custom(content)
            .name(label.clone())
            .selected(selected)
            .class(control_style(selected, tone))
            .width(scale.target())
            .height(scale.target())
            .padding(0)
            .on_press_maybe(on_press);
        if let Some(id) = id {
            button = button.id(id);
        }
        let button: Element<'a, Message> = if enabled {
            crate::interaction::hover(button)
        } else {
            button.into()
        };
        tooltip(button, label)
    }
}

/// Independent toolbar commands, grouped by spacing without a selector surface.
pub fn toolbar_actions<'a, Message: 'a>(
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::container(widget::row::with_children(controls).align_y(iced::Alignment::Center))
        .padding(space::XXS)
        .into()
}

/// A connected mode selector using the same compact targets as toolbar actions.
/// Pass [`icon_button`] children; each retains its own focus and tooltip.
pub fn segmented_controls<'a, Message: 'a>(
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::container(widget::row::with_children(controls).align_y(iced::Alignment::Center))
        .padding(space::XXS)
        .class(cosmic::theme::Container::custom(|theme| {
            widget::container::Style {
                background: Some(iced::Background::Color(
                    theme.current_container().component.base.into(),
                )),
                border: iced::Border {
                    // Concentric with the controls it wraps: their radius plus
                    // the inset between the two edges.
                    radius: (design::RADIUS_S + space::XXS).into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }))
        .into()
}

/// The shell's flat control surface: no border, no fill until hovered.
///
/// Exposed so a surface that composes its own row - a transcript step, a
/// navigation entry - presses the same treatment as the icon controls beside
/// it instead of reaching for a stock button class.
pub fn control_style(selected: bool, tone: Option<Tone>) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| {
        let component = &theme.current_container().component;
        let raised = selected || focused;
        widget::button::Style {
            background: raised
                .then(|| iced::Background::Color(component.selected_state_color().into())),
            icon_color: Some(match tone {
                Some(tone) => tone.resolve(theme),
                None if raised => component.on.into(),
                None => design::muted_icon_color(theme),
            }),
            ..design::button_style()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| widget::button::Style {
            icon_color: Some(disabled_icon_color(theme)),
            background: None,
            ..appearance(false, theme)
        }),
        hovered: Box::new(move |focused, theme| {
            let component = &theme.current_container().component;
            widget::button::Style {
                background: Some(iced::Background::Color(component.hover.into())),
                icon_color: Some(tone.map_or(component.on.into(), |tone| tone.resolve(theme))),
                ..appearance(focused, theme)
            }
        }),
        pressed: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            ..appearance(focused, theme)
        }),
    }
}

/// Symbolic SVG recoloring ignores alpha, so a disabled glyph is mixed toward
/// its surface instead of relying on `on_disabled` transparency.
fn disabled_icon_color(theme: &cosmic::Theme) -> iced::Color {
    let component = &theme.current_container().component;
    design::mix(component.base.into(), component.on.into(), 0.38)
}
