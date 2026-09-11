//! Compact, keyboard-accessible icon tabs and toolbar actions.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use crate::interaction::tooltip;

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
    static SPLIT_PANEL: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linejoin="round"><rect x="1.5" y="2.5" width="13" height="11" rx="1.5"/><path d="M10 3v10"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static PULL_REQUEST: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="4" cy="3" r="1.5"/><circle cx="4" cy="13" r="1.5"/><circle cx="12" cy="13" r="1.5"/><path d="M4 4.5v7M12 11.5V6a3 3 0 0 0-3-3H7m2-2L7 3l2 2"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static MERGED_PULL_REQUEST: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><circle cx="4" cy="3" r="1.5"/><circle cx="4" cy="13" r="1.5"/><circle cx="12" cy="3" r="1.5"/><path d="M4 4.5v7M12 4.5V6a3 3 0 0 1-3 3H4"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static ATTENTION: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 11.5h10l-1.5-2V6a3.5 3.5 0 0 0-7 0v3.5zM6.5 13.5a1.7 1.7 0 0 0 3 0M8 1v1"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static WORKING: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round"><circle cx="8" cy="8" r="4.5"/><circle cx="8" cy="8" r="1.5"/><path d="M8 1v2M8 13v2M1 8h2M13 8h2M3 3l1.5 1.5M11.5 11.5 13 13M3 13l1.5-1.5M11.5 4.5 13 3"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static WAITING: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 2.5h10a1 1 0 0 1 1 1v7a1 1 0 0 1-1 1H7l-4 3v-3H2V3.5a1 1 0 0 1 1-1ZM8 5v2.5"/><path d="M8 9.5h0"/></g></svg>"#.as_slice()).symbolic(true)
    });
    static ADDED_FILE: LazyLock<widget::icon::Handle> = LazyLock::new(|| {
        widget::icon::from_svg_bytes(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><g fill="none" stroke="black" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M3 1.5h6l4 4v9H3zM9 1.5v4h4M5.5 10h5M8 7.5v5"/></g></svg>"#.as_slice()).symbolic(true)
    });
    match name {
        "document-edit-symbolic" => CHANGES.clone(),
        "view-list-tree-symbolic" => TREE.clone(),
        "view-reveal-symbolic" => PREVIEW.clone(),
        "view-split-right-symbolic" => SPLIT_PANEL.clone(),
        "pull-request-symbolic" => PULL_REQUEST.clone(),
        "merged-pull-request-symbolic" => MERGED_PULL_REQUEST.clone(),
        "notifications-symbolic" => ATTENTION.clone(),
        "agent-working-symbolic" => WORKING.clone(),
        "agent-waiting-symbolic" => WAITING.clone(),
        "document-new-symbolic" => ADDED_FILE.clone(),
        _ => widget::icon::from_name(name).handle(),
    }
}

pub struct IconControl<'a, Message> {
    button: widget::button::Button<'a, Message>,
    label: Cow<'a, str>,
}

/// A centered, fixed-size icon target with a quiet surface selected state.
/// Native buttons retain keyboard focus, accessibility, and enabled cursors.
pub fn icon_control<'a, Message: Clone + 'a>(
    icon: widget::icon::Handle,
    label: impl Into<Cow<'a, str>>,
    selected: bool,
    enabled: bool,
    on_press: Option<Message>,
) -> IconControl<'a, Message> {
    let label = label.into();
    let content = widget::container(widget::icon(icon).size(16)).center(Length::Fill);
    IconControl {
        button: widget::button::custom(content)
            .name(label.clone())
            .selected(selected)
            .class(control_style(selected))
            .width(crate::design::PRIMARY_CONTROL_SIZE)
            .height(crate::design::PRIMARY_CONTROL_SIZE)
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

/// A connected mode selector using the same compact targets as toolbar actions.
/// Pass `icon_control(...).into()` children; each retains its own focus and tooltip.
pub fn segmented_controls<'a, Message: 'a>(
    controls: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::container(
        widget::row::with_children(controls)
            .spacing(0)
            .align_y(iced::Alignment::Center),
    )
    .padding(2)
    .class(cosmic::theme::Container::custom(|theme| {
        iced::widget::container::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.base.into(),
            )),
            border: iced::Border {
                color: theme.current_container().component.divider.into(),
                width: crate::design::BORDER_WIDTH,
                radius: theme.cosmic().corner_radii.radius_m.into(),
            },
            ..Default::default()
        }
    }))
    .into()
}

fn control_style(selected: bool) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| widget::button::Style {
        background: selected.then(|| {
            iced::Background::Color(
                theme
                    .current_container()
                    .component
                    .selected_state_color()
                    .into(),
            )
        }),
        border_radius: theme.cosmic().corner_radii.radius_s.into(),
        border_width: if focused { 1.0 } else { 0.0 },
        border_color: theme.current_container().component.on_disabled.into(),
        icon_color: Some(theme.current_container().component.on.into()),
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
