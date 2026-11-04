//! A pane's row of text tabs.

use std::borrow::Cow;

use cosmic::{
    Element,
    iced::{self, Length},
    widget,
};

use crate::{
    design::{self, space},
    interaction::tooltip,
};

/// One entry of a [`view_tabs`] bar.
pub struct ViewTab<'a, Message> {
    /// The short visible name.
    pub label: Cow<'a, str>,
    /// The accessible name and tooltip, which say what the label cannot fit.
    pub description: Cow<'a, str>,
    /// A quiet trailing figure, such as how many rows the view holds, and the
    /// text style it draws in, e.g. [`crate::design::muted_text`].
    pub count: Option<(
        Cow<'a, str>,
        fn(&cosmic::Theme) -> iced::widget::text::Style,
    )>,
    pub selected: bool,
    pub id: Option<widget::Id>,
    /// What choosing the tab raises. The selected tab publishes nothing:
    /// choosing the view already on screen is not an intent. In
    /// [`nested_view_tabs`] it returns from a page open beneath the view.
    pub on_press: Option<Message>,
}

/// A pane's single row of text tabs.
///
/// Views of one pane are peers, so they are words on one baseline rather than
/// unlabeled glyphs in a boxed selector: the selected view reads from its
/// filled surface and full-strength label, the others stay quiet until
/// hovered. Nothing else belongs in the row; a view's own options live in the
/// pane's header, beside the identity they act on.
///
/// `compact` tightens each tab's inset for a narrow pane. The row clips
/// rather than spilling past the pane if its words still do not fit.
pub fn view_tabs<'a, Message: Clone + 'a>(
    tabs: Vec<ViewTab<'a, Message>>,
    compact: bool,
) -> Element<'a, Message> {
    tab_row(tabs, compact, false)
}

/// [`view_tabs`] for views that open pages beneath themselves, such as a
/// detail or a single record. The selected tab keeps the press message its
/// caller gives it, which returns the view to its root while a page is open
/// beneath it. With nothing open the caller passes none, so choosing the view
/// already on screen is still never an intent.
pub fn nested_view_tabs<'a, Message: Clone + 'a>(
    tabs: Vec<ViewTab<'a, Message>>,
    compact: bool,
) -> Element<'a, Message> {
    tab_row(tabs, compact, true)
}

fn tab_row<'a, Message: Clone + 'a>(
    tabs: Vec<ViewTab<'a, Message>>,
    compact: bool,
    returns: bool,
) -> Element<'a, Message> {
    let row = widget::row::with_children(
        tabs.into_iter()
            .map(|tab| tab_button(tab, compact, None, returns))
            .collect::<Vec<_>>(),
    )
    .spacing(space::XS)
    .align_y(iced::Alignment::Center);
    widget::container(row).width(Length::Fill).clip(true).into()
}

/// A shared view tab, with an optional leading status mark. Compact tabs use
/// caption text and the smaller inline-control height.
pub fn view_tab<'a, Message: Clone + 'a>(
    tab: ViewTab<'a, Message>,
    compact: bool,
    leading: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    tab_button(tab, compact, leading, false)
}

/// A lone view tab that both opens and leaves its view, so pressing it while
/// selected is an intent.
pub struct ToggleViewTab<'a, Message> {
    /// `description` is the accessible name, which stays put as it toggles;
    /// the state is carried by `selected`.
    pub tab: ViewTab<'a, Message>,
    pub icon: Element<'a, Message>,
    /// The tooltip, which says what a press would do now.
    pub hint: Cow<'a, str>,
    /// Draws only the icon, for a bar that has run out of room for the label.
    pub icon_only: bool,
    /// Raised when a pointer starts a press, ahead of the press message, so
    /// the caller can tell a pointer from the keyboard.
    pub on_pointer_press: Message,
}

/// See [`ToggleViewTab`]. Without a press message it is plainly inert: no hover
/// fill and no tooltip to suggest that it would respond behind a modal.
pub fn toggle_view_tab<'a, Message: Clone + 'a>(
    toggle: ToggleViewTab<'a, Message>,
) -> Element<'a, Message> {
    let ToggleViewTab {
        tab,
        icon,
        hint,
        icon_only,
        on_pointer_press,
    } = toggle;
    let enabled = tab.on_press.is_some();
    let (button, _) = tab_control(tab, false, Some(icon), true, !icon_only);
    if enabled {
        tooltip(
            crate::interaction::hover(button.on_press_down(on_pointer_press)),
            hint,
        )
    } else {
        button.into()
    }
}

/// A view tab; `returns` keeps a selected tab's press message.
fn tab_button<'a, Message: Clone + 'a>(
    tab: ViewTab<'a, Message>,
    compact: bool,
    leading: Option<Element<'a, Message>>,
    returns: bool,
) -> Element<'a, Message> {
    let (button, name) = tab_control(tab, compact, leading, returns, true);
    tooltip(crate::interaction::hover(button), name)
}

/// The bare tab and its accessible name.
fn tab_control<'a, Message: Clone + 'a>(
    tab: ViewTab<'a, Message>,
    compact: bool,
    leading: Option<Element<'a, Message>>,
    returns: bool,
    show_label: bool,
) -> (widget::button::Button<'a, Message>, Cow<'a, str>) {
    let ViewTab {
        label,
        description,
        count,
        selected,
        id,
        on_press,
    } = tab;
    let mut content = widget::row::with_capacity(3)
        .spacing(space::S)
        .align_y(iced::Alignment::Center);
    if let Some(leading) = leading {
        content = content.push(leading);
    }
    if show_label {
        let label = if compact {
            design::caption(label)
        } else {
            design::body(label)
        };
        content = content.push(label.wrapping(iced::widget::text::Wrapping::None));
    }
    // The count is drawn, so it is spoken too.
    let name: Cow<'a, str> = match &count {
        Some((count, _)) => format!("{description}: {count}").into(),
        None => description,
    };
    if let Some((count, style)) = count {
        content = content.push(
            design::caption(count)
                .wrapping(iced::widget::text::Wrapping::None)
                .class(cosmic::theme::Text::Custom(style)),
        );
    }
    let mut button = widget::button::custom(widget::container(content).center_y(Length::Fill))
        .class(view_tab_style(selected))
        .name(name.clone())
        .selected(selected)
        .padding([
            0.0,
            if compact || !show_label {
                space::S
            } else {
                space::M
            },
        ])
        .height(view_tab_height(compact))
        .on_press_maybe(if selected && !returns { None } else { on_press });
    if let Some(id) = id {
        button = button.id(id);
    }
    (button, name)
}

pub fn view_tab_height(compact: bool) -> f32 {
    if compact {
        design::COMPACT_CONTROL_SIZE
    } else {
        design::DENSE_ROW_HEIGHT
    }
}

/// [`control_style`]'s surface with a label tone: the selected tab keeps its
/// fill and full-strength label even though it publishes nothing, and the
/// rest read as quieter peers rather than as disabled controls.
fn view_tab_style(selected: bool) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| {
        let component = &theme.current_container().component;
        widget::button::Style {
            background: (selected || focused)
                .then(|| iced::Background::Color(component.selected_state_color().into())),
            text_color: Some(if selected {
                component.on.into()
            } else {
                design::muted_color(theme)
            }),
            // A selected tab already wears the focus fill, so a pressable one
            // shows focus the way other buttons do: a ring outside the control.
            outline_width: if selected && focused {
                design::BORDER_WIDTH
            } else {
                0.0
            },
            outline_color: if selected && focused {
                design::accent_color(theme)
            } else {
                iced::Color::TRANSPARENT
            },
            ..design::button_style()
        }
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| appearance(false, theme)),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.hover.into(),
            )),
            text_color: Some(theme.current_container().component.on.into()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::{accessibility_probe::headless_renderer, activity_spinner};

    fn toggle(on_press: Option<()>, icon_only: bool) -> Element<'static, ()> {
        toggle_view_tab(ToggleViewTab {
            tab: ViewTab {
                label: "Work".into(),
                description: "Channel work".into(),
                count: None,
                selected: true,
                id: None,
                on_press,
            },
            icon: widget::Space::new().width(16).height(16).into(),
            hint: "Close".into(),
            icon_only,
            on_pointer_press: (),
        })
    }

    #[test]
    fn an_inert_toggle_is_a_bare_button_without_hover_or_tooltip() {
        use cosmic::iced::advanced::widget::Tree;
        let bare: Element<'_, ()> = widget::button::custom(widget::Space::new()).into();
        let bare = Tree::new(bare.as_widget()).tag;
        assert_eq!(Tree::new(toggle(None, false).as_widget()).tag, bare);
        assert_ne!(Tree::new(toggle(Some(()), false).as_widget()).tag, bare);
    }

    #[test]
    fn icon_only_toggle_is_narrower_and_keeps_its_name() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{layout, widget::Tree};
        let renderer = headless_renderer();
        let width = |mut tab: Element<'static, ()>| {
            let mut tree = Tree::new(tab.as_widget());
            tab.as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(400.0, 80.0)),
                )
                .size()
                .width
        };
        let icon_only = width(toggle(Some(()), true));
        assert!(icon_only < width(toggle(Some(()), false)));
        assert_eq!(icon_only, 2.0 * space::S + 16.0);
    }

    #[test]
    fn only_a_selected_tab_draws_the_focus_ring_over_its_fill() {
        let theme = cosmic::Theme::default();
        let active = |selected: bool, focused: bool| match view_tab_style(selected) {
            cosmic::theme::Button::Custom { active, .. } => active(focused, &theme),
            _ => unreachable!(),
        };
        assert_eq!(active(true, true).outline_width, design::BORDER_WIDTH);
        assert_eq!(active(true, false).outline_width, 0.0);
        assert_eq!(active(false, true).outline_width, 0.0);
    }

    #[test]
    fn compact_status_tabs_select_only_an_inactive_view() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size};

        let renderer = headless_renderer();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(400.0, 80.0));
        for selected in [false, true] {
            let mut tab = view_tab(
                ViewTab {
                    label: "Worker".into(),
                    description: "Worker · Running".into(),
                    count: None,
                    selected,
                    id: None,
                    on_press: Some(()),
                },
                true,
                Some(activity_spinner(12.0, design::accent_color)),
            );
            let mut tree = Tree::new(tab.as_widget());
            let node = tab.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            assert_eq!(node.size().height, design::COMPACT_CONTROL_SIZE);
            let cursor = mouse::Cursor::Available(node.bounds().center());
            let mut messages = Vec::new();
            for event in [
                mouse::Event::ButtonPressed(mouse::Button::Left),
                mouse::Event::ButtonReleased(mouse::Button::Left),
            ] {
                tab.as_widget_mut().update(
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
            assert_eq!(messages.len(), usize::from(!selected));
        }
    }
}
