use super::*;

/// A single flat surface for every app-owned dialog. Opaque content consumes
/// clicks in its padding before they can reach the dismissible backdrop.
pub(crate) fn surface<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    max_width: f32,
) -> Element<'a, M> {
    crate::interaction::opaque(
        widget::container(content)
            .class(cosmic::theme::Container::Dialog(false))
            .padding(space::L)
            .width(Length::Fill)
            .max_width(max_width),
    )
    .into()
}

/// The closing row every confirmation shares: right-aligned, the dismissal
/// before the committing action, a sibling-control gap apart.
pub(crate) fn confirmation_actions<'a, M: 'a>(
    dismiss: impl Into<Element<'a, M>>,
    confirm: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    confirmation_controls(vec![dismiss.into(), confirm.into()])
}

/// Multi-action confirmations retain the same safe-first, trailing row.
pub(crate) fn confirmation_controls<'a, M: 'a>(controls: Vec<Element<'a, M>>) -> Element<'a, M> {
    widget::container(
        widget::row::with_children(controls)
            .spacing(space::S)
            .wrap()
            .align_x(iced::alignment::Horizontal::Right),
    )
    .align_right(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };

    #[test]
    fn long_confirmation_actions_wrap_and_remain_reachable_in_a_narrow_window() {
        let renderer = super::super::tests::headless_renderer();
        let controls = (0..4)
            .map(|index| {
                crate::ui_controls::button::secondary("Keep editing automation")
                    .width(170)
                    .id(widget::Id::from(format!("fixture.action-{index}")))
                    .on_press(Message::DismissStatus)
                    .into()
            })
            .collect();
        let mut element = confirmation_controls(controls);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(300.0, 300.0)),
        );
        #[derive(Default)]
        struct Bounds(Vec<iced::Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                if id.is_some() {
                    self.0.push(bounds);
                }
            }
        }
        let mut bounds = Bounds::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
        assert_eq!(bounds.0.len(), 4);
        assert!(
            bounds
                .0
                .iter()
                .all(|bounds| bounds.x >= 0.0 && bounds.x + bounds.width <= 300.0)
        );
        assert!(
            bounds.0[3].y > bounds.0[0].y,
            "actions must wrap instead of extending beyond the window"
        );
    }
}

pub(super) fn overlay<'a>(
    surface: Element<'a, Message>,
    dismiss: Option<Message>,
    progress: f32,
) -> Element<'a, Message> {
    let progress = progress.clamp(0.0, 1.0);
    let background = widget::container(iced::widget::Space::new())
        .class(cosmic::theme::Container::custom(move |_| {
            iced::widget::container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgba(
                    0.0,
                    0.0,
                    0.0,
                    design::SCRIM_ALPHA * progress,
                ))),
                ..Default::default()
            }
        }))
        .width(Length::Fill)
        .height(Length::Fill);
    let mut backdrop = iced::widget::mouse_area(background);
    if let Some(message) = dismiss {
        backdrop = backdrop.on_press(message);
    }
    let backdrop = crate::interaction::opaque(backdrop).into();
    let centered = widget::container(crate::interaction::fade(
        crate::interaction::opaque(surface),
        progress,
    ))
    .center(Length::Fill)
    .padding(space::L)
    .width(Length::Fill)
    .height(Length::Fill)
    .into();
    crate::interaction::layer(backdrop, centered)
}
