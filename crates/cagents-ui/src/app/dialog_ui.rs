use super::*;

pub(crate) use crate::dialog_ui::{
    MAX_HEIGHT, confirmation_actions, scrolled_region, scrolling_surface,
    scrolling_surface_with_actions, scrolling_with_actions, surface,
};

pub(crate) use crate::question::{
    Question, confirmation_surface, dont_ask_again_id, inline_question, tall_confirmation_surface,
};

impl Cagents {
    /// The inline question in front of the person, and the message its
    /// dismissal sends, in the order Escape reaches them. A dialog over the
    /// window is in front of all of them; then an account menu's or the
    /// account popover's; then the focused pane's; then a usage reset's
    /// asked anywhere else, as Escape has always withdrawn one. Escape
    /// itself reaches them layer by layer, through
    /// [`Self::account_question_escape`] and [`Self::pane_escape`]; this
    /// names the one in front for the tests that check that order.
    #[cfg(test)]
    pub(super) fn inline_question(&self) -> Option<(Question, Message)> {
        if self.modal_is_open() {
            return None;
        }
        self.forget_home_question()
            .or_else(|| self.usage_reset_question(true))
            .or_else(|| self.pane_question())
            .or_else(|| self.usage_reset_question(false))
    }

    fn forget_home_question(&self) -> Option<(Question, Message)> {
        self.account_forget_asked().then(|| {
            (
                Question::ForgetHome,
                Message::AccountUi(account_ui::Action::RemoveCancel),
            )
        })
    }

    fn usage_reset_question(&self, in_popover: bool) -> Option<(Question, Message)> {
        self.usage_reset_asked(in_popover).map(|account| {
            (
                Question::UseUsageReset,
                Message::AccountUi(account_ui::Action::ResetCancel(account)),
            )
        })
    }

    /// The question asked in the focused pane, unless something covers the
    /// pane's content.
    fn pane_question(&self) -> Option<(Question, Message)> {
        let card = self.workspace.focused()?;
        if self.pane_content_inputs_blocked(card) {
            return None;
        }
        match self.cards.get(&card)? {
            // Its questions are asked in the side panel, which a narrow card
            // hides: Escape can't answer what isn't there.
            CardRuntime::PullRequests(pane) if self.pull_request_side_shown(card, pane) => {
                let (question, action) = pane.review.pane_question()?;
                Some((question, Message::PullRequestReview { card, action }))
            }
            CardRuntime::Chat(_) => self.focused_close_refusal().map(|target| {
                (
                    Question::EndCloseHolders,
                    Message::ChatUi(chat_ui::Action::DismissCloseRefusal(target)),
                )
            }),
            CardRuntime::Automations => self.automation_ui.question().map(|(_, question)| {
                (
                    question,
                    Message::AutomationUi(crate::automation_ui::Action::CancelConfirmation),
                )
            }),
            _ => None,
        }
    }

    /// What Escape answers in an account's popups, ahead of the menu and
    /// popover they sit in, which would otherwise close around them: a
    /// Forget home question goes back to its menu, and a usage reset asked
    /// in the account popover is withdrawn.
    ///
    /// Unlike a dialog, an inline question doesn't hold the keyboard, so a
    /// key a focused control captured, such as a composer interrupting its
    /// agent or a terminal taking Escape, was that control's and answers no
    /// question. Forget home is the exception: it is asked inside an
    /// account menu, which, like every popup, takes Escape however it
    /// arrives, and backing out of the question is the menu's first step.
    pub(super) fn account_question_escape(&self, captured: bool) -> Option<Message> {
        if self.modal_is_open() {
            return None;
        }
        self.forget_home_question()
            .or_else(|| {
                (!captured)
                    .then(|| self.usage_reset_question(true))
                    .flatten()
            })
            .map(|(_, dismiss)| dismiss)
    }

    /// What an Escape no control captured answers in the focused pane, once
    /// every popup, picker and overlay in front of it has had the key: its
    /// question's dismissal, or else the pull request review popover
    /// closing; failing those, a usage reset asked elsewhere is withdrawn.
    /// A captured key was the control's, as with
    /// [`Self::account_question_escape`].
    pub(super) fn pane_escape(&self) -> Option<Message> {
        if self.modal_is_open() {
            return None;
        }
        if let Some((_, dismiss)) = self.pane_question() {
            return Some(dismiss);
        }
        self.review_popover_escape()
            .or_else(|| self.usage_reset_question(false).map(|(_, dismiss)| dismiss))
    }

    fn review_popover_escape(&self) -> Option<Message> {
        let card = self.workspace.focused()?;
        if self.pane_content_inputs_blocked(card) {
            return None;
        }
        let Some(CardRuntime::PullRequests(pane)) = self.cards.get(&card) else {
            return None;
        };
        // The popover hangs from the side panel's header and hides with it.
        if !self.pull_request_side_shown(card, pane) {
            return None;
        }
        pane.review
            .review_escape()
            .map(|action| Message::PullRequestReview { card, action })
    }

    /// Whether to ask before the routine action `confirmation` guards.
    pub(super) fn asks(&self, confirmation: settings::Confirmation) -> bool {
        self.settings.asks(confirmation)
    }

    /// Opens `question`, with focus where it decides and any "Don't ask
    /// again" unticked.
    pub(super) fn open_question(
        &mut self,
        question: Question,
        dismiss: widget::Id,
        confirm: widget::Id,
    ) -> Task<Message> {
        if let Some(confirmation) = question.skippable() {
            self.dont_ask_again.remove(&confirmation);
        }
        question.focus(dismiss, confirm)
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
        .class(crate::ui_controls::button::quiet_class())
        .id(dont_ask_again_id(confirmation))
        .on_press_maybe(enabled.then_some(toggle(!ticked)))
        .into()
    }
}

pub(super) fn overlay<'a>(
    surface: Element<'a, Message>,
    dismiss: Option<Message>,
    progress: f32,
) -> Element<'a, Message> {
    let progress = progress.clamp(0.0, 1.0);
    let background = widget::container(iced::widget::Space::new())
        .class(cosmic::theme::Container::custom(move |theme| {
            iced::widget::container::Style {
                background: Some(iced::Background::Color(
                    design::modal_scrim(theme).scale_alpha(progress),
                )),
                ..Default::default()
            }
        }))
        .width(Length::Fill)
        .height(Length::Fill);
    let mut backdrop = iced::widget::mouse_area(background);
    if let Some(message) = dismiss {
        backdrop = backdrop.on_press(message);
    }
    let backdrop = crate::interaction::opaque(backdrop);
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
            key: keyboard::Key::Named(key),
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

    /// Tab and Shift+Tab within a dialog pass over a button that cannot act.
    #[test]
    fn dialog_tab_passes_over_disabled_buttons() {
        let ids = ["first", "disabled", "last"].map(widget::Id::new);
        let dialog = || {
            confirmation_surface(
                Question::DeleteChannel,
                widget::text("Choose one"),
                widget::row::with_children(
                    ids.iter()
                        .enumerate()
                        .map(|(index, id)| {
                            widget::button::text("Action")
                                .id(id.clone())
                                .on_press_maybe((index != 1).then(|| {
                                    Message::UpdateUi(super::super::update_ui::Action::CopyDetails)
                                }))
                                .into()
                        })
                        .collect::<Vec<_>>(),
                ),
                400.0,
            )
        };
        let tab = key_press(keyboard::key::Named::Tab);
        assert_eq!(
            focus_after_key(dialog(), ids[0].clone(), tab.clone()),
            (Some(ids[2].clone()), true),
            "Tab passes over the disabled button"
        );
        assert_eq!(
            focus_after_key(dialog(), ids[2].clone(), shift_tab()),
            (Some(ids[0].clone()), true),
            "Shift+Tab passes over it too"
        );
        assert_eq!(
            focus_after_key(dialog(), ids[1].clone(), tab),
            (Some(ids[2].clone()), true),
            "Tab moves on from a button disabled while focused"
        );
    }

    /// Tab through a dialog whose body scrolls: each control it reaches in
    /// the body is brought into view, clear of the fades at its edges, and
    /// Shift+Tab brings the first back.
    #[test]
    fn tab_brings_a_control_below_the_fold_into_view() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Operation, Tree, operation},
        };
        let ids: Vec<widget::Id> = (0..12)
            .map(|row| widget::Id::new(format!("row-{row}")))
            .collect();
        let mut element = overlay(
            crate::interaction::focus_scope(scrolling_surface_with_actions(
                widget::column::with_children(
                    ids.iter()
                        .map(|id| {
                            widget::button::text("Choose this row")
                                .id(id.clone())
                                .on_press(Message::CancelDialog)
                                .into()
                        })
                        .collect::<Vec<_>>(),
                )
                .spacing(space::M),
                crate::ui_controls::button::secondary("Cancel")
                    .id(widget::Id::new("cancel"))
                    .on_press(Message::CancelDialog),
                480.0,
                MAX_HEIGHT,
            )),
            None,
            1.0,
        );
        let renderer = crate::app::tests::headless_renderer();
        let window = iced::Rectangle::with_size(SCALED_MINIMUM_WINDOW);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, window.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(ids[0].clone()),
        );
        let press = |element: &mut Element<'_, Message>, tree: &mut Tree, event| {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(&node),
                iced::mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &window,
            );
        };
        // The body's viewport, the focused control and where it is drawn.
        #[derive(Default)]
        struct Seen {
            viewport: Option<iced::Rectangle>,
            content: f32,
            scrolled: f32,
            focused: Option<(widget::Id, iced::Rectangle)>,
        }
        impl Operation for Seen {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                content: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::scrollable::Scrollable,
            ) {
                if self.viewport.is_none() {
                    self.viewport = Some(bounds);
                    self.content = content.height;
                    self.scrolled = translation.y;
                }
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if state.is_focused() {
                    self.focused = id.cloned().map(|id| (id, bounds));
                }
            }
        }
        let seen = |element: &mut Element<'_, Message>, tree: &mut Tree| {
            let mut seen = Seen::default();
            element
                .as_widget_mut()
                .operate(tree, Layout::new(&node), &renderer, &mut seen);
            let viewport = seen.viewport.expect("the body scrolls");
            let (id, bounds) = seen.focused.expect("a control has focus");
            (
                viewport,
                id,
                bounds - iced::Vector::new(0.0, seen.scrolled),
                seen.scrolled,
                seen.content,
            )
        };
        // Clear of the fade at each edge, which is as deep as the content
        // hidden beyond it, up to the full fade.
        let in_view =
            |viewport: iced::Rectangle, drawn: iced::Rectangle, scrolled: f32, content: f32| {
                let fade = |hidden: f32| hidden.clamp(0.0, crate::interaction::OVERFLOW_FADE);
                drawn.y >= viewport.y + fade(scrolled) - 0.5
                    && drawn.y + drawn.height
                        <= viewport.y + viewport.height - fade(content - viewport.height - scrolled)
                            + 0.5
            };
        let tab = iced::Event::Keyboard(key_press(keyboard::key::Named::Tab));
        // A frame follows the key, as it does in the app.
        let frame = iced::Event::Window(iced::window::Event::RedrawRequested(
            iced::time::Instant::now(),
        ));
        for row in &ids[1..] {
            press(&mut element, &mut tree, tab.clone());
            press(&mut element, &mut tree, frame.clone());
            let (viewport, id, drawn, scrolled, content) = seen(&mut element, &mut tree);
            assert_eq!(&id, row);
            assert!(
                in_view(viewport, drawn, scrolled, content),
                "{id:?} is drawn at {drawn:?}, outside {viewport:?}"
            );
        }
        assert!(
            seen(&mut element, &mut tree).3 > 0.0,
            "the last row is reached by scrolling"
        );
        for _ in 1..ids.len() {
            press(&mut element, &mut tree, iced::Event::Keyboard(shift_tab()));
            press(&mut element, &mut tree, frame.clone());
        }
        let (viewport, id, drawn, scrolled, content) = seen(&mut element, &mut tree);
        assert_eq!(id, ids[0]);
        assert!(
            in_view(viewport, drawn, scrolled, content),
            "{drawn:?} outside {viewport:?}"
        );
        assert_eq!(scrolled, 0.0, "the first row sits flush at the top again");
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

    /// The scroll region's viewport within a laid-out dialog, and its
    /// content's height.
    fn scroll_geometry(
        mut element: Element<'_, Message>,
        window: iced::Size,
    ) -> (iced::Rectangle, iced::Rectangle, f32) {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        #[derive(Default)]
        struct Viewport(iced::Rectangle, f32);
        impl Operation for Viewport {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                content: iced::Rectangle,
                _: iced::Vector,
                _: &mut dyn iced::advanced::widget::operation::scrollable::Scrollable,
            ) {
                self.0 = bounds;
                self.1 = content.height;
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, window),
        );
        let mut viewport = Viewport::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut viewport);
        (node.bounds(), viewport.0, viewport.1)
    }

    #[test]
    fn a_scrolling_dialog_scrolls_its_padding_and_runs_its_viewport_to_the_border() {
        let window = iced::Size::new(800.0, 800.0);
        // Taller than the dialog may be: the viewport takes the dialog's whole
        // height inside its border, and the padding scrolls with the content.
        let (dialog, viewport, content) = scroll_geometry(
            scrolling_surface(
                widget::Space::new().height(1000.0),
                600.0,
                300.0,
                Length::Shrink,
            ),
            window,
        );
        assert_eq!(dialog.height, 300.0);
        assert_eq!(viewport.y, dialog.y + design::BORDER_WIDTH);
        assert_eq!(viewport.height, dialog.height - 2.0 * design::BORDER_WIDTH);
        assert_eq!(content, 1000.0 + 2.0 * (space::L - design::BORDER_WIDTH));
        // The sides keep the dialog's inset, which keeps the scrollbar's
        // thumb clear of the rounded corners.
        assert_eq!(viewport.x, dialog.x + space::L);
        assert_eq!(
            dialog.x + dialog.width - (viewport.x + viewport.width),
            space::L
        );
        // Short content: the dialog fits it, with the same padding as an
        // ordinary dialog.
        let (dialog, _, _) = scroll_geometry(
            scrolling_surface(
                widget::Space::new().height(100.0),
                600.0,
                300.0,
                Length::Shrink,
            ),
            window,
        );
        assert_eq!(dialog.height, 100.0 + 2.0 * space::L);
    }

    /// The minimum 900x600 window at the supported 150% interface scale lays
    /// a dialog out in 600x400 logical pixels, less the overlay's inset.
    #[test]
    fn scrolling_dialog_actions_stay_in_reach_and_only_the_body_scrolls() {
        let action = widget::Id::new("dialog.test.action");
        let dialog = |body: f32| {
            overlay(
                scrolling_surface_with_actions(
                    widget::Space::new().height(body),
                    widget::button::text("Action")
                        .id(action.clone())
                        .height(32.0)
                        .on_press(Message::CancelDialog),
                    600.0,
                    560.0,
                ),
                None,
                1.0,
            )
        };
        let scaled = iced::Size::new(600.0, 400.0);
        let (window, viewport, content, button) = action_geometry(dialog(2000.0), scaled, &action);
        assert!(
            button.y >= 0.0 && button.y + button.height <= window.height,
            "the action is inside the window: {button:?}"
        );
        assert_eq!(button.height, 32.0);
        assert!(viewport.y + viewport.height <= button.y);
        assert!(content > viewport.height, "the body scrolls instead");
        // A roomy window caps the dialog at its maximum height.
        let (_, viewport, _, button) =
            action_geometry(dialog(2000.0), iced::Size::new(1200.0, 900.0), &action);
        let top = viewport.y - design::BORDER_WIDTH;
        assert_eq!(button.y + button.height + space::L - top, 560.0);
        // A short body fits: no scrolling and no gap above the actions.
        let (_, viewport, content, button) = action_geometry(dialog(100.0), scaled, &action);
        assert_eq!(content, viewport.height);
        assert_eq!(viewport.y + viewport.height, button.y);
    }

    /// The minimum 900x600 window at the supported 150% interface scale, in
    /// logical pixels.
    pub(in crate::app) const SCALED_MINIMUM_WINDOW: iced::Size = iced::Size::new(600.0, 400.0);

    /// A message long enough to fill any dialog at the scaled minimum window.
    pub(in crate::app) fn long_message() -> String {
        "fatal: the operation could not finish because something went wrong. ".repeat(40)
    }

    /// Each control `ids` names stays whole and inside the scaled minimum
    /// window, as tall as in a roomy one, below the dialog's body, which
    /// scrolls instead.
    pub(in crate::app) fn assert_actions_stay_in_reach<'a>(
        dialog: impl Fn() -> Element<'a, Message>,
        ids: &[widget::Id],
    ) {
        for id in ids {
            let (_, _, _, natural) = action_geometry(dialog(), iced::Size::new(1200.0, 900.0), id);
            let (window, viewport, content, control) =
                action_geometry(dialog(), SCALED_MINIMUM_WINDOW, id);
            assert!(
                natural.height > 0.0,
                "{id:?} is a control in the dialog: {natural:?}"
            );
            assert!(
                control.height == natural.height
                    && control.y >= 0.0
                    && control.y + control.height <= window.height,
                "{id:?} must stay whole and inside the window: {control:?}, not {natural:?}"
            );
            assert!(
                viewport.y + viewport.height <= control.y,
                "{id:?} sits below the body: {viewport:?}, {control:?}"
            );
            assert!(content > viewport.height, "the body scrolls instead");
        }
    }

    /// Every confirmation keeps its answers in reach, however long what it
    /// asks about is.
    #[test]
    fn a_confirmation_keeps_its_answers_in_reach_whatever_it_says() {
        let [dismiss, confirm] = ["dismiss", "confirm"].map(widget::Id::new);
        assert_actions_stay_in_reach(
            || {
                overlay(
                    confirmation_surface(
                        Question::DeleteChannel,
                        widget::column::with_children(vec![
                            design::title("Delete it?").into(),
                            design::body(long_message()).into(),
                        ])
                        .spacing(space::M),
                        confirmation_actions(
                            crate::ui_controls::button::secondary("Cancel")
                                .id(dismiss.clone())
                                .on_press(Message::CancelDialog),
                            crate::ui_controls::button::destructive("Delete")
                                .id(confirm.clone())
                                .on_press(Message::CancelDialog),
                        ),
                        480.0,
                    ),
                    None,
                    1.0,
                )
            },
            &[dismiss.clone(), confirm.clone()],
        );
    }

    /// The window, the dialog body's scroll viewport, its content height and
    /// the bounds of the control or container `id`, in a laid-out dialog.
    pub(in crate::app) fn action_geometry(
        mut element: Element<'_, Message>,
        window: iced::Size,
        id: &widget::Id,
    ) -> (iced::Rectangle, iced::Rectangle, f32, iced::Rectangle) {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation},
        };
        struct Geometry<'a>(&'a widget::Id, iced::Rectangle, f32, iced::Rectangle);
        impl Operation for Geometry<'_> {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: iced::Rectangle,
                content: iced::Rectangle,
                _: iced::Vector,
                _: &mut dyn operation::scrollable::Scrollable,
            ) {
                // The dialog's body is the outermost scrollable; lists inside
                // it come later.
                if self.1 == iced::Rectangle::default() {
                    self.1 = bounds;
                    self.2 = content.height;
                }
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                if id == Some(self.0) {
                    self.3 = bounds;
                }
            }
            fn container(&mut self, id: Option<&widget::Id>, bounds: iced::Rectangle) {
                if id == Some(self.0) {
                    self.3 = bounds;
                }
            }
        }
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, window),
        );
        let mut geometry = Geometry(
            id,
            iced::Rectangle::default(),
            0.0,
            iced::Rectangle::default(),
        );
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut geometry);
        (node.bounds(), geometry.1, geometry.2, geometry.3)
    }
}
