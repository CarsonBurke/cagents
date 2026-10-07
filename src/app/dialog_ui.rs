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
pub(super) fn confirmation_actions<'a>(
    dismiss: impl Into<Element<'a, Message>>,
    confirm: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::row::with_children(vec![
        iced::widget::Space::new().width(Length::Fill).into(),
        dismiss.into(),
        confirm.into(),
    ])
    .spacing(space::S)
    .into()
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
