//! Generic dialog rendering. Callers own surface identities and confirmation policy.

use cosmic::{
    Element,
    iced::{self, Length, Padding},
    widget,
};

use crate::design::{self, space};

/// A single flat surface for a dialog. Opaque content consumes
/// clicks in its padding before they can reach the dismissible backdrop.
pub fn surface<'a, M: 'a>(content: impl Into<Element<'a, M>>, max_width: f32) -> Element<'a, M> {
    crate::interaction::opaque(
        widget::container(content)
            .class(cosmic::theme::Container::Dialog(false))
            .padding(space::L)
            .width(Length::Fill)
            .max_width(max_width),
    )
}

/// A dialog's scrolled content, held off the scrollbar's rail by `padding`.
/// It keeps the dialog's text color: cosmic's default container would set
/// the window background's instead, dimming everything inside.
fn scrolled_body<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    padding: Padding,
) -> Element<'a, M> {
    widget::container(content)
        .padding(design::with_vertical_rail(padding))
        .class(cosmic::theme::Container::custom(|_| Default::default()))
        .into()
}

/// A list inside a dialog's body that scrolls on its own once it is taller
/// than `max_height`, fading into the dialog at its edges. It is as wide as
/// the body, so its scrollbar runs along the dialog's edge. Like the body
/// around it, it keeps the dialog's text color.
pub fn scrolled_region<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    max_height: f32,
) -> Element<'a, M> {
    widget::container(crate::interaction::overflow_fade_y(
        crate::interaction::reveal_focus(
            widget::scrollable(scrolled_body(content, Padding::ZERO))
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .width(Length::Fill),
        ),
        design::dialog_base,
    ))
    .width(Length::Fill)
    .max_height(max_height)
    .class(cosmic::theme::Container::custom(|_| Default::default()))
    .into()
}

/// A [`surface`] whose whole body scrolls when the window is too short for
/// it. `height` is the dialog's own: `Shrink` to fit its content, or `Fill`
/// to take `max_height` whatever it holds.
///
/// The viewport runs to the dialog's border, so content leaves through the
/// dialog's edge and fades into it, rather than being cut off a padding's
/// width short of it: the vertical padding scrolls with the content. The
/// sides keep theirs, which holds the fade and the scrollbar's thumb clear
/// of the rounded corners.
pub fn scrolling_surface<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    max_width: f32,
    max_height: f32,
    height: Length,
) -> Element<'a, M> {
    let edge = space::L - design::BORDER_WIDTH;
    crate::interaction::opaque(
        widget::container(crate::interaction::overflow_fade_y(
            widget::scrollable(scrolled_body(
                content,
                Padding {
                    top: edge,
                    bottom: edge,
                    ..Padding::ZERO
                },
            ))
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .width(Length::Fill)
            .height(height),
            design::dialog_base,
        ))
        .class(cosmic::theme::Container::Dialog(false))
        .padding(Padding {
            top: design::BORDER_WIDTH,
            bottom: design::BORDER_WIDTH,
            left: space::L,
            right: space::L,
        })
        .width(Length::Fill)
        .height(height)
        .max_width(max_width)
        .max_height(max_height),
    )
}

/// A [`scrolling_surface`] whose closing `actions` stay below the body,
/// always in reach: when the window is too short for the whole dialog, only
/// the body gives up height and scrolls. The dialog fits its content up to
/// `max_height`.
///
/// The viewport runs from the dialog's top border down to the actions, so
/// content fades into the gap above them; the gap and the top padding scroll
/// with the content.
pub fn scrolling_surface_with_actions<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    actions: impl Into<Element<'a, M>>,
    max_width: f32,
    max_height: f32,
) -> Element<'a, M> {
    surface_with_actions(content, actions, max_width, max_height, None)
}

/// A [`scrolling_surface_with_actions`] whose surface carries `id`.
fn surface_with_actions<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    actions: impl Into<Element<'a, M>>,
    max_width: f32,
    max_height: f32,
    id: Option<widget::Id>,
) -> Element<'a, M> {
    let surface = widget::container(scrolling_with_actions(
        content,
        actions,
        Padding {
            top: space::L - design::BORDER_WIDTH,
            bottom: space::M,
            ..Padding::ZERO
        },
        design::dialog_base,
    ))
    .class(cosmic::theme::Container::Dialog(false))
    .padding(Padding {
        top: design::BORDER_WIDTH,
        bottom: space::L,
        left: space::L,
        right: space::L,
    })
    .width(Length::Fill)
    .max_width(max_width)
    .max_height(max_height);
    crate::interaction::opaque(match id {
        Some(id) => surface.id(id),
        None => surface,
    })
}

/// `content` above the `actions` that close it, which stay whole and in
/// reach: when the space is too short for both, only the content gives up
/// height and scrolls, fading at its edges into `fill`, the surface behind
/// it. `padding` holds the content off the scrollbar's rail and scrolls with
/// it. The pair is as tall as its parts, so short content leaves no gap
/// above the actions.
pub fn scrolling_with_actions<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    actions: impl Into<Element<'a, M>>,
    padding: Padding,
    fill: fn(&cosmic::Theme) -> iced::Color,
) -> Element<'a, M> {
    crate::interaction::footed(
        crate::interaction::overflow_fade_y(
            // Tab to a field below the fold brings it into view.
            crate::interaction::reveal_focus(
                widget::scrollable(scrolled_body(content, padding))
                    .direction(iced::widget::scrollable::Direction::Vertical(
                        design::scrollbar(),
                    ))
                    .width(Length::Fill)
                    .height(Length::Shrink),
            ),
            fill,
        ),
        actions,
    )
}

/// How tall a dialog with closing actions grows before its body scrolls,
/// in a window with room for it.
pub const MAX_HEIGHT: f32 = 640.0;

/// A confirmation identified by `id`: its `content` above the `actions` that
/// answer it, which stay whole and in reach however long the content is
/// (see [`scrolling_surface_with_actions`]). Tab, Shift+Tab and the arrows
/// cycle through its controls without leaving it. Escape and the backdrop
/// dismiss it as every dialog does. Its caller supplies the initial focus policy.
pub fn confirmation_surface<'a, M: 'a>(
    id: widget::Id,
    content: impl Into<Element<'a, M>>,
    actions: impl Into<Element<'a, M>>,
    max_width: f32,
) -> Element<'a, M> {
    tall_confirmation_surface(id, content, actions, max_width, MAX_HEIGHT)
}

/// A [`confirmation_surface`] that grows to `max_height`, rather than
/// [`MAX_HEIGHT`], before its body scrolls: for a question whose content,
/// such as a list of what it affects, is long as a rule.
pub fn tall_confirmation_surface<'a, M: 'a>(
    id: widget::Id,
    content: impl Into<Element<'a, M>>,
    actions: impl Into<Element<'a, M>>,
    max_width: f32,
    max_height: f32,
) -> Element<'a, M> {
    crate::interaction::focus_scope(surface_with_actions(
        content,
        actions,
        max_width,
        max_height,
        Some(id),
    ))
}

/// A question asked in place: `content`, its answers among it, marked as
/// carrying `id`. Unlike a [`confirmation_surface`] it doesn't hold the
/// keyboard, so Tab can leave it for what it is about; its opener focuses
/// according to its policy, and Escape answers it with its dismissal.
pub fn inline_question<'a, M: 'a>(
    id: widget::Id,
    content: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    widget::container(content).id(id).into()
}

/// The closing row every confirmation shares: right-aligned, the dismissal
/// before the committing action, a sibling-control gap apart.
pub fn confirmation_actions<'a, M: 'a>(
    dismiss: impl Into<Element<'a, M>>,
    confirm: impl Into<Element<'a, M>>,
) -> Element<'a, M> {
    widget::row::with_children(vec![
        iced::widget::Space::new().width(Length::Fill).into(),
        dismiss.into(),
        confirm.into(),
    ])
    .spacing(space::S)
    .into()
}
