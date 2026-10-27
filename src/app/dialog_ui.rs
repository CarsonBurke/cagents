use super::*;

/// Absence permits the caller's normal workspace fallback. An exposed owner
/// preserving its current phase or user focus must never take that fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DialogFocus {
    NoOwner,
    Preserve,
    Focus(widget::Id),
}

impl Cagents {
    pub(super) fn exposed_dialog_focus(&self) -> DialogFocus {
        use DialogFocus::{Focus, NoOwner, Preserve};

        // Keep this order aligned with Application::dialog. These higher
        // owners retain their own initial and phase-specific focus policy.
        if self.automation_approval_open() || self.transient_work_close.is_some() {
            return Preserve;
        }
        if self.automation_ui.draft_dialog_open() {
            return Focus(crate::automation_ui::draft_dialog_focus_id());
        }
        if self.device_ui.open {
            return if self.device_ui.dirty() {
                Focus(device_ui::file_editor_id())
            } else {
                Preserve
            };
        }
        if self.chat_ui.image_viewer.is_some() || self.terminal_paste.is_some() {
            return Preserve;
        }
        #[cfg(feature = "integrated-browser")]
        if self.browser_import.open() {
            return Preserve;
        }
        if self.session_recovery.open()
            || self.daemon_health_ui.confirmation
            || self.update_ui.confirmation.is_some()
            || self.terminal_close_dialog_visible()
        {
            return Preserve;
        }
        if self.settings_reset_confirm {
            return self.reset_dialog_focus_target().map_or(Preserve, Focus);
        }
        if self.sidebar_state.pending_discard().is_some() {
            return Focus(sidebar::discard_cancel_id());
        }
        if let Some(close) = &self.dirty_repository_close {
            // The renderer returns None here, rather than mounting one of the
            // lower dialogs, if the repository runtime no longer exists.
            if !matches!(
                self.cards.get(&close.card),
                Some(CardRuntime::Repository(_))
            ) {
                return NoOwner;
            }
            return self.dirty_close_focus_target().map_or(Preserve, Focus);
        }
        if let Some(dialog) = &self.sidebar_selection_ui.dialog {
            return Focus(dialog.focus_target());
        }
        if self.project_ui.modal_open() {
            return Focus(self.project_ui.focus_target());
        }
        if self.channel_ui.modal_open() {
            return self.channel_ui.focus_target().map_or(Preserve, Focus);
        }
        if let Some(dialog) = &self.worktree_dialog {
            return if !dialog.is_resolving() {
                Focus(worktree_dialog_focus_id())
            } else {
                Preserve
            };
        }
        if self.resume_ui.open {
            return Focus(resume_ui::focus_id());
        }
        if self.pull_request_dialog.is_some() {
            return Focus(pull_request_dialog_focus_id());
        }
        if self.context_inspector_is_overlay() {
            return Focus(context_inspector_toggle_id());
        }
        NoOwner
    }

    pub(super) fn restore_dialog_focus(
        &self,
        fallback: impl FnOnce() -> Task<Message>,
    ) -> Task<Message> {
        match self.exposed_dialog_focus() {
            DialogFocus::NoOwner => fallback(),
            DialogFocus::Preserve => Task::none(),
            DialogFocus::Focus(target) => focus_target_task(target),
        }
    }

    pub(super) fn focus_exposed_dialog(&self) -> Task<Message> {
        self.restore_dialog_focus(Task::none)
    }
}

/// Explicit focus must not blur the whole window when its captured destination
/// was removed or replaced before the task reached the mounted native tree.
pub(super) fn focus_target_task(target: widget::Id) -> Task<Message> {
    use iced::advanced::widget::{Operation, operation};

    struct MountedTarget {
        target: widget::Id,
        found: bool,
    }

    impl<T: Send + 'static> Operation<T> for MountedTarget {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
            visit(self);
        }

        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _: iced::Rectangle,
            _: &mut dyn operation::Focusable,
        ) {
            self.found |= id == Some(&self.target);
        }

        fn finish(&self) -> operation::Outcome<T> {
            if self.found {
                operation::Outcome::Chain(Box::new(operation::focusable::focus(
                    self.target.clone(),
                )))
            } else {
                operation::Outcome::None
            }
        }
    }

    iced::advanced::widget::operate(MountedTarget {
        target,
        found: false,
    })
    .map(cosmic::Action::App)
}

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
        mut element: Element<'_, Message>,
        id: widget::Id,
        event: keyboard::Event,
    ) -> (Vec<Message>, bool) {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Tree, operation},
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
        (messages, captured)
    }

    fn apply_native_task(
        ui: &mut iced::runtime::UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
        renderer: &cosmic::Renderer,
        task: Task<Message>,
    ) {
        use iced::{
            advanced::widget::operation,
            futures::{FutureExt, StreamExt},
        };
        let Some(mut stream) = iced::runtime::task::into_stream(task) else {
            return;
        };
        loop {
            match stream.next().now_or_never() {
                Some(Some(iced::runtime::Action::Widget(mut operation))) => {
                    for step in 0..8 {
                        ui.operate(renderer, operation.as_mut());
                        match operation.finish() {
                            operation::Outcome::Chain(next) => {
                                assert!(
                                    step < 7,
                                    "focus operation must have a bounded finish chain"
                                );
                                operation = next;
                            }
                            operation::Outcome::None => break,
                            _ => panic!("focus must not emit an application or provider action"),
                        }
                    }
                }
                Some(None) => break,
                _ => panic!("dialog focus contains only immediately available native operations"),
            }
        }
    }

    fn native_focus(
        ui: &mut iced::runtime::UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
        renderer: &cosmic::Renderer,
    ) -> Vec<Option<widget::Id>> {
        use iced::advanced::widget::{Operation, operation};
        #[derive(Default)]
        struct Focus(Vec<Option<widget::Id>>);
        impl Operation for Focus {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if state.is_focused() {
                    self.0.push(id.cloned());
                }
            }
        }
        let mut focus = Focus::default();
        ui.operate(renderer, &mut focus);
        focus.0
    }

    #[test]
    fn viewer_restore_preserves_native_focus_without_evaluating_workspace_fallback() {
        use iced::{
            advanced::widget::operation,
            runtime::{UserInterface, user_interface::Cache},
        };
        use std::cell::Cell;
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        app.worktree_dialog = Some(WorktreeDialog::new(
            ProjectId::new(),
            "/fixture".into(),
            WorktreeDraft::default(),
        ));
        app.chat_ui.image_viewer = Some(chat_ui::images::Gallery::file(
            "/fixture/public-image.png".into(),
            None,
            iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
        ));
        assert_eq!(
            app.exposed_dialog_focus(),
            DialogFocus::Preserve,
            "the viewer is above the existing worktree form"
        );
        let fallback_called = Cell::new(false);
        let foreign = widget::Id::new("dialog-test.viewer-composer");
        let task = app.restore_dialog_focus(|| {
            fallback_called.set(true);
            focus_target_task(worktree_dialog_focus_id())
        });
        assert!(!fallback_called.get());
        assert_eq!(task.units(), 0);
        let content = widget::column::with_children(vec![
            app.dialog().expect("actual viewer is mounted"),
            crate::ui_controls::field::text("Composer", "keep draft")
                .id(foreign.clone())
                .on_input(|_| Message::CancelDialog)
                .into(),
        ]);
        let mut renderer = super::super::tests::headless_renderer();
        let mut ui = UserInterface::build(
            content,
            iced::Size::new(900.0, 900.0),
            Cache::new(),
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(foreign.clone()),
        );
        assert_eq!(native_focus(&mut ui, &renderer), [Some(foreign.clone())]);
        apply_native_task(&mut ui, &renderer, task);
        assert_eq!(native_focus(&mut ui, &renderer), [Some(foreign)]);
        assert!(!fallback_called.get());
    }

    #[test]
    fn no_dialog_restores_fallback_and_legacy_forms_restore_their_real_editable_field() {
        use iced::runtime::{UserInterface, user_interface::Cache};
        use std::cell::Cell;
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        assert_eq!(app.exposed_dialog_focus(), DialogFocus::NoOwner);
        let calls = Cell::new(0);
        let id = widget::Id::new("dialog-test.workspace-fallback");
        let task = app.restore_dialog_focus(|| {
            calls.set(calls.get() + 1);
            focus_target_task(id.clone())
        });
        assert_eq!(calls.get(), 1);
        let mut renderer = super::super::tests::headless_renderer();
        {
            let content = crate::ui_controls::field::text("Workspace", "draft")
                .id(id.clone())
                .on_input(|_| Message::CancelDialog);
            let mut ui = UserInterface::build(
                content,
                iced::Size::new(800.0, 600.0),
                Cache::new(),
                &mut renderer,
            );
            apply_native_task(&mut ui, &renderer, task);
            assert_eq!(native_focus(&mut ui, &renderer), [Some(id)]);
        }
        for pull_request in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            app.context_inspector_open = false;
            let expected = if pull_request {
                app.pull_request_dialog = Some(PullRequestDialog {
                    project_id: ProjectId::new(),
                    repository: GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap(),
                    worktree: None,
                    base: "main".into(),
                    head: "topic".into(),
                    title: String::new(),
                    body: String::new().into(),
                    draft: false,
                    retry_confirmation: None,
                });
                pull_request_dialog_focus_id()
            } else {
                app.worktree_dialog = Some(WorktreeDialog::new(
                    ProjectId::new(),
                    "/fixture".into(),
                    WorktreeDraft::default(),
                ));
                worktree_dialog_focus_id()
            };
            assert_eq!(
                app.exposed_dialog_focus(),
                DialogFocus::Focus(expected.clone())
            );
            let task = app
                .restore_dialog_focus(|| panic!("a mounted form cannot fall through to workspace"));
            let mut ui = UserInterface::build(
                app.dialog().expect("actual native form"),
                iced::Size::new(800.0, 900.0),
                Cache::new(),
                &mut renderer,
            );
            apply_native_task(&mut ui, &renderer, task);
            assert_eq!(native_focus(&mut ui, &renderer), [Some(expected)]);
        }
    }

    #[test]
    fn missing_captured_dialog_target_does_not_blur_current_native_focus() {
        use iced::{
            advanced::widget::operation,
            runtime::{UserInterface, user_interface::Cache},
        };
        let old = widget::Id::new("dialog-test.presentation-a.confirm.enabled");
        let current = widget::Id::new("dialog-test.presentation-b.confirm.enabled");
        let foreign = widget::Id::new("dialog-test.foreign-input");
        let queued = focus_target_task(old);
        assert_eq!(queued.units(), 1);
        let content = widget::column::with_children(vec![
            crate::ui_controls::button::primary("Confirm")
                .id(current.clone())
                .on_press(Message::CancelDialog)
                .into(),
            crate::ui_controls::field::text("Other input", "keep selection")
                .id(foreign.clone())
                .on_input(|_| Message::CancelDialog)
                .into(),
        ]);
        let mut renderer = super::super::tests::headless_renderer();
        let mut ui = UserInterface::build(
            content,
            iced::Size::new(800.0, 600.0),
            Cache::new(),
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(foreign.clone()),
        );
        assert_eq!(native_focus(&mut ui, &renderer), [Some(foreign.clone())]);
        apply_native_task(&mut ui, &renderer, queued);
        assert_eq!(
            native_focus(&mut ui, &renderer),
            [Some(foreign)],
            "a missing old destination must not invoke vanilla focus's global unfocus"
        );
        apply_native_task(&mut ui, &renderer, focus_target_task(current.clone()));
        assert_eq!(
            native_focus(&mut ui, &renderer),
            [Some(current)],
            "the exact mounted current destination still focuses normally"
        );
    }
}
