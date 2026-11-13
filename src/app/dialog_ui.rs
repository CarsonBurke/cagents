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

/// What a confirmation's committing action costs, which decides where the
/// keyboard starts and whether the person may stop being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Consequence {
    /// Common and recoverable: the committing action takes focus, so Enter
    /// answers the question the person meant to ask, and the dialog offers
    /// "Don't ask again".
    Routine(settings::Confirmation),
    /// Can't be undone: the dismissal takes focus, so a reflexive Enter
    /// keeps everything as it is. The committing action is one Tab, Shift+Tab
    /// or arrow away, and the question is always asked.
    Irreversible,
}

impl Consequence {
    /// The control a confirmation opens with keyboard focus on.
    pub(crate) fn initial_focus(self, dismiss: widget::Id, confirm: widget::Id) -> widget::Id {
        match self {
            Self::Routine(_) => confirm,
            Self::Irreversible => dismiss,
        }
    }

    pub(crate) fn focus(self, dismiss: widget::Id, confirm: widget::Id) -> Task<Message> {
        iced::widget::operation::focus(self.initial_focus(dismiss, confirm))
            .map(cosmic::Action::App)
    }
}

/// A dialog that asks a question: [`surface`] whose controls Tab, Shift+Tab
/// and the arrows cycle through without leaving it. Escape and the backdrop
/// dismiss it as every dialog does.
pub(crate) fn confirmation_surface<'a, M: 'a>(
    content: impl Into<Element<'a, M>>,
    max_width: f32,
) -> Element<'a, M> {
    surface(crate::interaction::focus_scope(content), max_width)
}

pub(crate) fn dont_ask_again_id(confirmation: settings::Confirmation) -> widget::Id {
    widget::Id::new(format!("dialog.dont-ask.{}", confirmation.key()))
}

impl Cagents {
    /// Whether to ask before the routine action `confirmation` guards.
    pub(super) fn asks(&self, confirmation: settings::Confirmation) -> bool {
        self.settings.asks(confirmation)
    }

    /// Opens a routine question: unticked, with focus on its committing
    /// action.
    pub(super) fn open_confirmation(
        &mut self,
        confirmation: settings::Confirmation,
        dismiss: widget::Id,
        confirm: widget::Id,
    ) -> Task<Message> {
        self.dont_ask_again.remove(&confirmation);
        Consequence::Routine(confirmation).focus(dismiss, confirm)
    }

    /// The question was dismissed: a tick in "Don't ask again" is only
    /// a choice once the action it guards goes ahead.
    pub(super) fn dismiss_confirmation(&mut self, confirmation: settings::Confirmation) {
        self.dont_ask_again.remove(&confirmation);
    }

    /// The question was answered by going ahead; remembers a tick in
    /// "Don't ask again" in settings every window shares.
    pub(super) fn commit_confirmation(
        &mut self,
        confirmation: settings::Confirmation,
    ) -> Task<Message> {
        if self.dont_ask_again.remove(&confirmation) {
            self.apply_setting_input(SettingsField::Confirm(confirmation), "false".into())
        } else {
            Task::none()
        }
    }

    /// The "Don't ask again" checkbox of a routine question. The whole row is
    /// one control, so it takes keyboard focus and a click on its label.
    pub(super) fn dont_ask_again_control(
        &self,
        confirmation: settings::Confirmation,
        enabled: bool,
    ) -> Element<'_, Message> {
        let ticked = self.dont_ask_again.contains(&confirmation);
        let toggle = move |ticked| Message::DontAskAgain(confirmation, ticked);
        widget::button::custom(
            crate::ui_controls::field::checkbox(ticked)
                .label("Don't ask again")
                .on_toggle_maybe(enabled.then_some(toggle)),
        )
        .class(cosmic::theme::Button::Text)
        .id(dont_ask_again_id(confirmation))
        .on_press_maybe(enabled.then_some(toggle(!ticked)))
        .into()
    }
}

/// The closing row every confirmation shares: right-aligned, the dismissal
/// before the committing action, a sibling-control gap apart.
pub(crate) fn confirmation_actions<'a, M: 'a>(
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    /// Feed Enter to the real mounted native control after its focus operation.
    pub(in crate::app) fn enter_control(element: Element<'_, Message>, id: widget::Id) -> Message {
        let (mut messages, _) = key_control(element, id, key_press(keyboard::key::Named::Enter));
        assert_eq!(
            messages.len(),
            1,
            "Enter must activate one focused native control"
        );
        messages.pop().unwrap()
    }
    pub(in crate::app) fn key_press(key: keyboard::key::Named) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key.clone()),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }
    }

    pub(in crate::app) fn key_control(
        element: Element<'_, Message>,
        id: widget::Id,
        event: keyboard::Event,
    ) -> (Vec<Message>, bool) {
        let (messages, captured, _) = key_outcome(element, id, event);
        (messages, captured)
    }

    /// Where keyboard focus is after `event` reaches the mounted `element`
    /// with `id` focused, and whether a control claimed the key.
    pub(in crate::app) fn focus_after_key(
        element: Element<'_, Message>,
        id: widget::Id,
        event: keyboard::Event,
    ) -> (Option<widget::Id>, bool) {
        let (_, captured, focused) = key_outcome(element, id, event);
        (focused, captured)
    }

    pub(in crate::app) fn space() -> keyboard::Event {
        let keyboard::Event::KeyPressed {
            physical_key,
            location,
            modifiers,
            repeat,
            ..
        } = key_press(keyboard::key::Named::Tab)
        else {
            unreachable!()
        };
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(" ".into()),
            modified_key: keyboard::Key::Character(" ".into()),
            physical_key,
            location,
            modifiers,
            text: Some(" ".into()),
            repeat,
        }
    }

    pub(in crate::app) fn shift_tab() -> keyboard::Event {
        let keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location,
            text,
            repeat,
            ..
        } = key_press(keyboard::key::Named::Tab)
        else {
            unreachable!()
        };
        keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location,
            modifiers: keyboard::Modifiers::SHIFT,
            text,
            repeat,
        }
    }

    fn key_outcome(
        mut element: Element<'_, Message>,
        id: widget::Id,
        event: keyboard::Event,
    ) -> (Vec<Message>, bool, Option<widget::Id>) {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree, operation},
        };
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        let bounds = iced::Rectangle::with_size(iced::Size::new(800.0, 600.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(id),
        );
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Keyboard(event),
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &bounds,
        );
        let captured = shell.is_event_captured();
        struct Focused(Option<widget::Id>);
        impl Operation for Focused {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _bounds: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if state.is_focused() {
                    self.0 = id.cloned();
                }
            }
        }
        let mut focused = Focused(None);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut focused);
        (messages, captured, focused.0)
    }
}
