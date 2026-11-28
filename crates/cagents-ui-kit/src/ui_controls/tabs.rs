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
    ui_controls::{hover_fill, selected_fill},
};

/// One entry of a [`view_tabs`] bar.
pub struct ViewTab<'a, Message> {
    /// The short visible name.
    pub label: Cow<'a, str>,
    /// The accessible name and tooltip, which say what the label cannot fit.
    pub description: Cow<'a, str>,
    /// How many rows the view holds, drawn as a [`count_badge`] after the
    /// label, in its tone.
    ///
    /// [`count_badge`]: crate::ui_controls::count_badge
    pub count: Option<(Cow<'a, str>, crate::ui_controls::CountTone)>,
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
    // A tab's count comes, grows and goes with what its view holds; while
    // the pointer is on the row, the tabs keep their widths so none slides
    // under it.
    let row = crate::interaction::hover_hold::held_row(
        tabs.into_iter()
            .map(|tab| tab_button(tab, compact, returns))
            .collect(),
        space::XS,
    );
    widget::container(row).width(Length::Fill).clip(true).into()
}

/// One tab of a [`quiet_tab`] strip.
pub struct QuietTab<'a, Message> {
    /// The visible name. Past [`QuietTab::limit`] characters it fades out
    /// rather than ending in an ellipsis; the accessible name keeps it whole.
    pub label: Cow<'a, str>,
    /// How many characters of `label` the tab shows.
    pub limit: Option<usize>,
    /// Drawn whole after the label, which fades before it does: the number
    /// that tells two tabs of one name apart.
    pub suffix: Option<Cow<'a, str>>,
    /// The accessible name and tooltip, which say what the label cannot fit.
    pub description: Cow<'a, str>,
    pub selected: bool,
    pub id: Option<widget::Id>,
    /// What choosing the tab raises. The selected tab publishes nothing.
    pub on_press: Option<Message>,
}

/// How thick the accent line under a selected [`quiet_tab`] is.
pub const QUIET_TAB_UNDERLINE: f32 = 2.0;

/// A quiet tab, with an optional leading status mark: plain caption text
/// with no surface of its own, so a strip of them reads as words rather than
/// as a row of buttons. The selected tab is marked by an accent line along
/// its foot and a full-strength label; the others stay muted until hovered.
/// Every tab keeps the line's room, so selection never moves a label.
pub fn quiet_tab<'a, Message: Clone + 'a>(
    tab: QuietTab<'a, Message>,
    leading: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let QuietTab {
        label,
        limit,
        suffix,
        description,
        selected,
        id,
        on_press,
    } = tab;
    let mut content = widget::row::with_capacity(3)
        .spacing(space::XS)
        .align_y(iced::Alignment::Center);
    if let Some(leading) = leading {
        content = content.push(leading);
    }
    // The label takes its colour from the tab, so it follows hover and
    // selection; only its drawing is cut, never its name.
    let mut label =
        crate::ui_controls::FadeText::caption(label).class(cosmic::theme::Text::Default);
    if let Some(limit) = limit {
        label = label.limit(limit);
    }
    content = content.push(label);
    if let Some(suffix) = suffix {
        content =
            content.push(design::caption(suffix).wrapping(iced::widget::text::Wrapping::None));
    }
    let underline = widget::container(widget::Space::new())
        .width(Length::Fill)
        .height(QUIET_TAB_UNDERLINE)
        .class(if selected {
            cosmic::theme::Container::custom(|theme| widget::container::Style {
                background: Some(iced::Background::Color(design::accent_color(theme))),
                ..Default::default()
            })
        } else {
            design::transparent()
        });
    let body = widget::column::with_children(vec![
        widget::container(content)
            .padding([0.0, space::S])
            .center_y(Length::Fill)
            .into(),
        underline.into(),
    ]);
    let mut button = widget::button::custom(body)
        .class(quiet_tab_style(selected))
        .name(description.clone())
        .selected(selected)
        .padding(0)
        .height(view_tab_height(true))
        .on_press_maybe(if selected { None } else { on_press });
    if let Some(id) = id {
        button = button.id(id);
    }
    tooltip(crate::interaction::hover(button), description)
}

/// A quiet tab has no surface until it is hovered, and the selected one none
/// at all: its accent line says it is the one shown. Keyboard focus rings a
/// tab inside its bounds, as a view tab does.
fn quiet_tab_style(selected: bool) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| widget::button::Style {
        text_color: Some(if selected {
            theme.current_container().component.on.into()
        } else {
            design::muted_color(theme)
        }),
        border_width: if focused { design::BORDER_WIDTH } else { 0.0 },
        border_color: if focused {
            design::accent_color(theme)
        } else {
            iced::Color::TRANSPARENT
        },
        ..design::button_style()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| appearance(false, theme)),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: (!selected).then(|| iced::Background::Color(hover_fill(theme))),
            text_color: Some(theme.current_container().component.on.into()),
            ..appearance(focused, theme)
        }),
        pressed: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(
                theme.current_container().component.pressed.into(),
            )),
            text_color: Some(theme.current_container().component.on.into()),
            ..appearance(focused, theme)
        }),
    }
}

/// A view tab; `returns` keeps a selected tab's press message.
fn tab_button<'a, Message: Clone + 'a>(
    tab: ViewTab<'a, Message>,
    compact: bool,
    returns: bool,
) -> Element<'a, Message> {
    let ViewTab {
        label,
        description,
        count,
        selected,
        id,
        on_press,
    } = tab;
    let mut content = widget::row::with_capacity(2)
        .spacing(space::S)
        .align_y(iced::Alignment::Center);
    let label = if compact {
        design::caption(label)
    } else {
        design::body(label)
    };
    content = content.push(label.wrapping(iced::widget::text::Wrapping::None));
    // The count is drawn, so it is spoken too.
    let name: Cow<'a, str> = match &count {
        Some((count, _)) => format!("{description}: {count}").into(),
        None => description,
    };
    if let Some((count, tone)) = count {
        content = content.push(crate::ui_controls::count_badge(count, tone));
    }
    let mut button = widget::button::custom(widget::container(content).center_y(Length::Fill))
        .class(view_tab_style(selected))
        .name(name.clone())
        .selected(selected)
        .padding([0.0, if compact { space::S } else { space::M }])
        .height(view_tab_height(compact))
        .on_press_maybe(if selected && !returns { None } else { on_press });
    if let Some(id) = id {
        button = button.id(id);
    }
    tooltip(crate::interaction::hover(button), name)
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
/// rest read as quieter peers rather than as disabled controls. A hovered
/// peer lifts by less than the selected tab, and keyboard focus rings a tab
/// rather than filling it, so only the selected tab reads as selected. The
/// ring is drawn inside the tab: the row clips to the tabs' height, which
/// would cut an outline drawn outside them.
///
/// [`control_style`]: crate::ui_controls::control_style
fn view_tab_style(selected: bool) -> cosmic::theme::Button {
    let appearance = move |focused: bool, theme: &cosmic::Theme| widget::button::Style {
        background: selected.then(|| iced::Background::Color(selected_fill(theme))),
        text_color: Some(if selected {
            theme.current_container().component.on.into()
        } else {
            design::muted_color(theme)
        }),
        border_width: if focused { design::BORDER_WIDTH } else { 0.0 },
        border_color: if focused {
            design::accent_color(theme)
        } else {
            iced::Color::TRANSPARENT
        },
        ..design::button_style()
    };
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(move |theme| appearance(false, theme)),
        hovered: Box::new(move |focused, theme| widget::button::Style {
            background: Some(iced::Background::Color(if selected {
                selected_fill(theme)
            } else {
                hover_fill(theme)
            })),
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

    #[test]
    fn only_the_selected_tab_draws_the_selected_fill() {
        for theme in [cosmic::Theme::dark(), cosmic::Theme::light()] {
            let selected_fill = Some(iced::Background::Color(selected_fill(&theme)));
            for selected in [false, true] {
                let cosmic::theme::Button::Custom {
                    active, hovered, ..
                } = view_tab_style(selected)
                else {
                    panic!("a custom style");
                };
                let expected = if selected { selected_fill } else { None };
                let focused = active(true, &theme);
                assert_eq!(focused.background, expected, "focus is not selection");
                // Inside the tab, where the clipping row cannot cut it.
                assert!(focused.border_width > 0.0, "focus rings the tab");
                assert_eq!(focused.outline_width, 0.0);
                assert_eq!(active(false, &theme).border_width, 0.0);
                let hover = hovered(false, &theme).background;
                assert!(hover.is_some());
                assert_eq!(
                    hover == selected_fill,
                    selected,
                    "a hovered peer is not selected"
                );
            }
        }
    }
    #[test]
    fn a_quiet_tab_has_no_surface_and_only_hover_lifts_an_unselected_one() {
        for theme in [cosmic::Theme::dark(), cosmic::Theme::light()] {
            for selected in [false, true] {
                let cosmic::theme::Button::Custom {
                    active, hovered, ..
                } = quiet_tab_style(selected)
                else {
                    panic!("a custom style");
                };
                assert_eq!(active(false, &theme).background, None);
                assert_eq!(
                    active(true, &theme).background,
                    None,
                    "focus is not selection"
                );
                assert!(
                    active(true, &theme).border_width > 0.0,
                    "focus rings the tab"
                );
                assert_eq!(
                    hovered(false, &theme).background.is_some(),
                    !selected,
                    "only a peer lifts under the pointer"
                );
            }
        }
    }

    /// The accent line is the selected tab's only mark, and every tab keeps
    /// its room, so choosing a tab moves no label.
    #[test]
    fn selecting_a_quiet_tab_does_not_resize_it() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{layout, widget::Tree};
        let renderer = headless_renderer();
        let sizes: Vec<_> = [false, true]
            .into_iter()
            .map(|selected| {
                let mut tab: Element<'_, ()> = quiet_tab(
                    QuietTab {
                        label: "Main".into(),
                        limit: None,
                        suffix: None,
                        description: "Main conversation".into(),
                        selected,
                        id: None,
                        on_press: Some(()),
                    },
                    None,
                );
                let mut tree = Tree::new(tab.as_widget());
                let node = tab.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(400.0, 80.0)),
                );
                node.size()
            })
            .collect();
        assert_eq!(sizes[0], sizes[1]);
        assert_eq!(sizes[0].height, design::COMPACT_CONTROL_SIZE);
    }

    use crate::ui_controls::{accessibility_probe::headless_renderer, activity_spinner};

    #[test]
    fn compact_status_tabs_select_only_an_inactive_view() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Point, Rectangle, Size};

        let renderer = headless_renderer();
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(400.0, 80.0));
        for selected in [false, true] {
            let mut tab = quiet_tab(
                QuietTab {
                    label: "Worker".into(),
                    limit: Some(24),
                    suffix: None,
                    description: "Worker · Running".into(),
                    selected,
                    id: None,
                    on_press: Some(()),
                },
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
            // A tab that can be chosen says so under the pointer; the one
            // already shown is inert.
            assert_eq!(
                tab.as_widget().mouse_interaction(
                    &tree,
                    Layout::new(&node),
                    cursor,
                    &viewport,
                    &renderer,
                ),
                if selected {
                    mouse::Interaction::None
                } else {
                    mouse::Interaction::Pointer
                }
            );
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
