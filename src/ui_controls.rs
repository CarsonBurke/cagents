//! Compact, keyboard-accessible icon tabs and toolbar actions.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use crate::interaction::tooltip;

const CONTROL_SIZE: u16 = 30;

/// Keep essential controls visible even when the desktop icon theme omits
/// these nonstandard names. Static handles retain renderer cache identity.
pub fn symbolic_icon(name: &str) -> widget::icon::Handle {
    use std::sync::LazyLock;
    static CHANGES: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round"><path d="M3 1.75h7l3 3v9.5H3zM10 2v3h3M5.5 8h5M8 5.5v5M5.5 12h5"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static TREE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M2.5 3h4M4 5.5v7h3M4 8h3M9 8h4M9 12.5h4"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static PREVIEW: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.5"><path d="M1 8s2.5-4.5 7-4.5S15 8 15 8s-2.5 4.5-7 4.5S1 8 1 8Z"/><circle cx="8" cy="8" r="2"/></g></svg>"#.as_slice()).symbolic(true)
    });
    match name {
        "document-edit-symbolic" => CHANGES.clone(),
        "view-list-tree-symbolic" => TREE.clone(),
        "view-reveal-symbolic" => PREVIEW.clone(),
        _ => widget::icon::from_name(name).handle(),
    }
}

pub struct IconControl<'a, Message> {
    button: widget::button::Button<'a, Message>,
    label: Cow<'a, str>,
}

/// A fixed-size icon target with transparent rectangular hover feedback.
/// Selection is rendered from application state, without a separate tab model.
pub fn icon_control<'a, Message: Clone + 'a>(
    icon: widget::icon::Handle,
    label: impl Into<Cow<'a, str>>,
    selected: bool,
    enabled: bool,
    on_press: Option<Message>,
) -> IconControl<'a, Message> {
    let label = label.into();
    let underline = widget::container(iced::widget::Space::new())
        .width(Length::Fill)
        .height(2)
        .class(cosmic::theme::Container::custom(move |theme| {
            iced::widget::container::Style {
                background: selected
                    .then(|| iced::Background::Color((*theme.cosmic().accent_color()).into())),
                ..Default::default()
            }
        }));
    let content = widget::column::with_children(vec![
        widget::container(widget::icon(icon).size(16))
            .center(Length::Fill)
            .into(),
        underline.into(),
    ])
    .spacing(0);
    IconControl {
        button: widget::button::custom(content)
            .name(label.clone())
            .selected(selected)
            .class(control_style(selected))
            .width(CONTROL_SIZE)
            .height(CONTROL_SIZE)
            .padding(0)
            .on_press_maybe(if enabled { on_press } else { None }),
        label,
    }
}

impl<'a, Message: Clone + 'a> IconControl<'a, Message> {
    pub fn id(mut self, id: widget::Id) -> Self {
        self.button = self.button.id(id);
        self
    }
}

impl<'a, Message: Clone + 'a> From<IconControl<'a, Message>> for Element<'a, Message> {
    fn from(control: IconControl<'a, Message>) -> Self {
        tooltip(control.button, control.label)
    }
}

fn control_style(selected: bool) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| widget::button::Style {
        border_radius: 3.0.into(),
        border_width: if focused { 1.0 } else { 0.0 },
        border_color: (*theme.cosmic().accent_color()).into(),
        icon_color: selected.then(|| (*theme.cosmic().accent_color()).into()),
        ..widget::button::Style::new()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| widget::button::Style {
            icon_color: Some(theme.current_container().component.on_disabled.into()),
            ..appearance(false, theme)
        }),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.hover.into(),
            )),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            ..appearance(focused, theme)
        }),
    }
}
