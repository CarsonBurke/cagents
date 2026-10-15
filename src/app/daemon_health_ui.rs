use super::*;
use crate::daemon_health;

#[derive(Default)]
pub(super) struct State {
    pub health: daemon_health::State,
    pub confirmation: bool,
    preparing: bool,
    restarting: bool,
    generation: u64,
    target: Option<Arc<crate::daemon::RestartTarget>>,
    error: Option<String>,
    focus: usize,
    restore_keyboard_nav: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum Action {
    Open,
    Cancel,
    Confirm,
    Prepared {
        generation: u64,
        target: Result<Arc<crate::daemon::RestartTarget>, String>,
    },
    Finished(Result<(), String>),
}

impl Cagents {
    pub(super) fn update_daemon_health_ui(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Open
                if self.daemon_health_ui.health.unavailable()
                    && !self.modal_is_open()
                    && !self.daemon_health_ui.restarting
                    && !crate::device::is_remote() =>
            {
                let state = &mut self.daemon_health_ui;
                state.generation = state.generation.wrapping_add(1);
                state.confirmation = true;
                state.preparing = true;
                state.target = None;
                state.error = None;
                state.focus = 0;
                state.restore_keyboard_nav = Some(self.core.keyboard_nav());
                self.core.set_keyboard_nav(false);
                let generation = state.generation;
                let preparation = cosmic::task::future(async move {
                    let target = tokio::task::spawn_blocking(crate::daemon::restart_target)
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result.map(Arc::new).map_err(|error| error.to_string()));
                    Message::DaemonHealthUi(Action::Prepared { generation, target })
                });
                Task::batch([preparation, self.modal_focus(health_focus_id(0))])
            }
            Action::Prepared { generation, target }
                if self.daemon_health_ui.confirmation
                    && self.daemon_health_ui.generation == generation =>
            {
                let state = &mut self.daemon_health_ui;
                state.preparing = false;
                match target {
                    Ok(target) => state.target = Some(target),
                    Err(error) => state.error = Some(error),
                }
                self.modal_focus(widget::Id::new("daemon-health.cancel"))
            }
            Action::Cancel
                if self.daemon_health_ui.confirmation && !self.daemon_health_ui.restarting =>
            {
                self.daemon_health_ui.confirmation = false;
                self.daemon_health_ui.target = None;
                if let Some(restore) = self.daemon_health_ui.restore_keyboard_nav.take() {
                    self.core.set_keyboard_nav(restore);
                }
                self.daemon_health_restore_focus()
            }
            Action::Confirm
                if self.daemon_health_ui.confirmation
                    && !self.daemon_health_ui.restarting
                    && self.daemon_health_ui.health.unavailable() =>
            {
                let Some(target) = self.daemon_health_ui.target.take() else {
                    return Task::none();
                };
                self.daemon_health_ui.restarting = true;
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        target.terminate().map_err(|error| error.to_string())?;
                        crate::terminal::ensure_daemon_ready()
                            .map(|_| ())
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::DaemonHealthUi(Action::Finished(result))
                })
            }
            Action::Finished(result) if self.daemon_health_ui.restarting => {
                self.daemon_health_ui.restarting = false;
                match result {
                    Ok(()) => {
                        self.daemon_health_ui.confirmation = false;
                        self.daemon_health_ui.error = None;
                        if let Some(restore) = self.daemon_health_ui.restore_keyboard_nav.take() {
                            self.core.set_keyboard_nav(restore);
                        }
                    }
                    Err(error) => self.daemon_health_ui.error = Some(error),
                }
                if self.daemon_health_ui.confirmation {
                    self.daemon_health_ui.focus = 0;
                    self.modal_focus(health_focus_id(0))
                } else {
                    self.daemon_health_restore_focus()
                }
            }
            _ => Task::none(),
        }
    }
    fn daemon_health_restore_focus(&self) -> Task<Message> {
        let target = self
            .workspace
            .focused()
            .map(|card| self.card_keyboard_focus(card))
            .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID));
        self.modal_restore_focus(target)
    }
    pub(super) fn adopt_daemon_health_keyboard_nav(&mut self, restore: bool) -> bool {
        if !self.daemon_health_ui.confirmation {
            return false;
        }
        self.daemon_health_ui.restore_keyboard_nav = Some(restore);
        self.core.set_keyboard_nav(false);
        true
    }
    pub(super) fn daemon_health_key_event(
        &mut self,
        event: &keyboard::Event,
        captured: bool,
    ) -> Option<Task<Message>> {
        if !self.daemon_health_ui.confirmation {
            return None;
        }
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };
        let keyboard::Key::Named(key) = key else {
            return Some(Task::none());
        };
        use keyboard::key::Named;
        if *key == Named::Escape {
            return Some(self.update_daemon_health_ui(Action::Cancel));
        }
        if self.daemon_health_ui.restarting {
            return Some(Task::none());
        }
        if matches!(key, Named::Tab | Named::ArrowLeft | Named::ArrowRight) {
            let count = 1 + usize::from(
                self.daemon_health_ui.target.is_some()
                    && self.daemon_health_ui.health.unavailable(),
            );
            let backwards = modifiers.shift() || *key == Named::ArrowLeft;
            self.daemon_health_ui.focus = if backwards {
                (self.daemon_health_ui.focus.min(count - 1) + count - 1) % count
            } else {
                (self.daemon_health_ui.focus + 1) % count
            };
            return Some(self.modal_focus(health_focus_id(self.daemon_health_ui.focus)));
        }
        if *key == Named::Enter
            && !captured
            && !modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
        {
            return Some(self.update_daemon_health_ui(
                if self.daemon_health_ui.focus == 1 && self.daemon_health_ui.target.is_some() {
                    Action::Confirm
                } else {
                    Action::Cancel
                },
            ));
        }
        Some(Task::none())
    }
    pub(super) fn daemon_health_banner(&self) -> Element<'_, Message> {
        let detail = match &self.daemon_health_ui.health {
            daemon_health::State::Responsive => {
                return iced::widget::Space::new().height(Length::Shrink).into();
            }
            daemon_health::State::Unresponsive { since, timeout } => format!(
                "The daemon has not answered since {}. A reply timed out after {} s.",
                timestamp(*since),
                timeout.as_secs_f64(),
            ),
            daemon_health::State::Incompatible { actual, .. } => format!(
                "This window requires daemon protocol {}, but the last daemon reply used protocol {actual}. The window and daemon cannot communicate.",
                crate::terminal_ipc::PROTOCOL_VERSION,
            ),
            daemon_health::State::Unreachable { since } => format!(
                "The daemon has been unreachable since {}. Reconnecting…",
                timestamp(*since),
            ),
        };
        let mut row =
            widget::row::with_children(vec![design::body(detail).width(Length::Fill).into()])
                .spacing(space::M)
                .align_y(Alignment::Center);
        if !crate::device::is_remote() {
            row = row.push(
                crate::ui_controls::button::secondary(if self.daemon_health_ui.restarting {
                    "Restarting…"
                } else {
                    "Restart daemon"
                })
                .on_press_maybe(
                    (!self.daemon_health_ui.restarting && !self.modal_is_open())
                        .then_some(Message::DaemonHealthUi(Action::Open)),
                ),
            );
        }
        widget::container(row)
            .padding(space::M)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Dialog(false))
            .into()
    }

    pub(super) fn daemon_health_dialog(&self) -> Option<Element<'_, Message>> {
        let state = &self.daemon_health_ui;
        if !state.confirmation {
            return None;
        }
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title(
                if matches!(state.health, daemon_health::State::Incompatible { .. }) {
                    "Restart the incompatible daemon?"
                } else {
                    "Restart the unresponsive daemon?"
                },
            )
            .into(),
            notice_body(
                "Active native turns and their background subagents will stop. Hosted terminals can close; conversations without durable provider state cannot be resumed. Saved files and drafts remain on disk.",
            ),
            notice_detail(
                "The exact daemon shown by this confirmation will receive a stop request. If it cannot exit within five seconds, that same process will be forcibly stopped. No restart happens automatically.",
            ),
        ];
        if state.preparing {
            rows.push(notice_detail("Verifying daemon process ownership…"));
        }
        if state.restarting {
            rows.push(notice_detail("Restarting the confirmed daemon…"));
        }
        if let Some(error) = &state.error {
            rows.push(notice_detail(error));
        }
        rows.push(dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("daemon-health.cancel"))
                .on_press_maybe(
                    (!state.restarting).then_some(Message::DaemonHealthUi(Action::Cancel)),
                ),
            crate::ui_controls::button::destructive("Restart daemon")
                .id(widget::Id::new("daemon-health.confirm"))
                .on_press_maybe(
                    (state.target.is_some() && !state.restarting && state.health.unavailable())
                        .then_some(Message::DaemonHealthUi(Action::Confirm)),
                ),
        ));
        Some(dialog_ui::overlay(
            dialog_ui::surface(widget::column::with_children(rows).spacing(space::M), 560.0),
            (!state.restarting).then_some(Message::DaemonHealthUi(Action::Cancel)),
            1.0,
        ))
    }
}

fn timestamp(time: std::time::SystemTime) -> String {
    let millis = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    unix_time_label(i64::try_from(millis).unwrap_or(i64::MAX))
}

fn health_focus_id(index: usize) -> widget::Id {
    widget::Id::new(if index == 1 {
        "daemon-health.confirm"
    } else {
        "daemon-health.cancel"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(key: keyboard::key::Named) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key.clone()),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }
    }
    #[test]
    fn completed_lower_restart_cannot_restore_into_a_present_workspace_widget() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.daemon_health_ui.confirmation = true;
        app.daemon_health_ui.restarting = true;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        let current = widget::Id::new("dialog.reset-settings.confirm");
        // Deliberately operate both surfaces: presence alone does not authorize
        // a lower feature to move focus out of the surviving modal.
        let root = app.workspace.focused()
            .map(|card| app.card_keyboard_focus(card))
            .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID));
        let field = |id| -> Element<'_, Message> {
            widget::text_input("", "")
                .id(id)
                .on_input(|_| Message::DismissStatus)
                .into()
        };
        let content = widget::column::with_children(vec![field(root), field(current.clone())]).into();
        let feature = app.update_daemon_health_ui(Action::Finished(Ok(())));
        assert!(!app.daemon_health_ui.confirmation);
        assert_eq!(app.top_modal(), Some(modal_ui::Modal::SettingsReset));
        let completion = app.finish_modal_update(feature);
        let task = iced::widget::operation::focus(current.clone()).map(cosmic::Action::App)
            .chain(completion);
        assert_eq!(super::super::tests::focused_element(content, task), Some(current));
    }

    #[test]
    fn lower_completion_cannot_enable_a_second_tab_owner_for_the_upper_dialog() {
        use iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.daemon_health_ui.confirmation = true;
        app.daemon_health_ui.restarting = true;
        app.daemon_health_ui.restore_keyboard_nav = Some(true);
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = true;
        app.settings_reset_focus_confirm = false;
        app.core.set_keyboard_nav(false);
        let opening = app.finish_modal_update(Task::none());
        assert_eq!(super::super::tests::focused_element(app.dialog().unwrap(), opening),
            Some(widget::Id::new("dialog.reset-settings.cancel")));
        drop(app.update(Message::DaemonHealthUi(Action::Finished(Ok(())))));
        assert!(app.core.keyboard_nav(), "the lower feature restored the ordinary Cosmic subscription");
        let renderer = super::super::tests::headless_renderer();
        for (backwards, repeat, expected) in [(false, false, true), (true, false, false), (false, true, false)] {
            let mut event = key(keyboard::key::Named::Tab);
            if let keyboard::Event::KeyPressed { modifiers, repeat: repeated, .. } = &mut event {
                *modifiers = if backwards { keyboard::Modifiers::SHIFT } else { keyboard::Modifiers::empty() };
                *repeated = repeat;
            }
            let captured = {
                let mut dialog = app.dialog().unwrap();
                let mut tree = Tree::new(&dialog);
                let bounds = iced::Rectangle::with_size(iced::Size::new(800.0, 600.0));
                let node = dialog.as_widget_mut().layout(&mut tree, &renderer,
                    &layout::Limits::new(iced::Size::ZERO, bounds.size()));
                let mut messages = Vec::new();
                let mut shell = Shell::new(&mut messages);
                dialog.as_widget_mut().update(&mut tree, &iced::Event::Keyboard(event.clone()),
                    Layout::new(&node), mouse::Cursor::Unavailable, &renderer, &mut clipboard::Null, &mut shell, &bounds);
                shell.is_event_captured()
            };
            assert!(captured, "Cosmic's ignored-key subscription must not receive this Tab, including repeats");
            let task = app.update(Message::KeyEvent { event, captured });
            assert_eq!(app.settings_reset_focus_confirm, expected);
            if !repeat {
                assert_eq!(super::super::tests::focused_element(app.dialog().unwrap(), task),
                    Some(widget::Id::new(if expected { "dialog.reset-settings.confirm" } else { "dialog.reset-settings.cancel" })));
            }
        }
    }

    #[test]
    fn protocol_mismatch_offers_reviewed_restart_without_stopping_anything() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(
            app.update(Message::DaemonHealth(daemon_health::State::Incompatible {
                since: std::time::SystemTime::UNIX_EPOCH,
                actual: crate::terminal_ipc::PROTOCOL_VERSION + 1,
            })),
        );
        assert!(app.daemon_health_ui.health.unavailable());
        drop(app.update_daemon_health_ui(Action::Open));
        assert!(app.daemon_health_ui.confirmation);
        assert!(!app.daemon_health_ui.restarting);
        drop(app.update_daemon_health_ui(Action::Confirm));
        assert!(
            !app.daemon_health_ui.restarting,
            "unverified ownership cannot stop a process"
        );
        let generation = app.daemon_health_ui.generation;
        drop(app.update_daemon_health_ui(Action::Prepared {
            generation,
            target: Err("ownership changed".into()),
        }));
        assert!(app.daemon_health_ui.error.is_some());
        drop(app.update(Message::DaemonHealth(daemon_health::State::Responsive)));
        assert!(!app.daemon_health_ui.confirmation);
    }

    #[test]
    fn daemon_health_confirmation_skips_disabled_confirm_and_fences_late_preparation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.core.set_keyboard_nav(true);
        app.daemon_health_ui.health = daemon_health::State::Unresponsive {
            since: std::time::SystemTime::UNIX_EPOCH,
            timeout: Duration::from_secs(5),
        };
        drop(app.update_daemon_health_ui(Action::Open));
        let generation = app.daemon_health_ui.generation;
        assert!(app.daemon_health_ui.confirmation);
        assert!(!app.core.keyboard_nav());
        drop(app.update_daemon_health_ui(Action::Confirm));
        assert!(
            !app.daemon_health_ui.restarting,
            "unprepared confirmation cannot send a signal"
        );
        for _ in 0..5 {
            drop(app.daemon_health_key_event(&key(keyboard::key::Named::Tab), false));
            assert_eq!(app.daemon_health_ui.focus, 0);
        }
        drop(app.update_daemon_health_ui(Action::Prepared {
            generation,
            target: Err("no owner".into()),
        }));
        drop(app.daemon_health_key_event(&key(keyboard::key::Named::Enter), false));
        assert!(!app.daemon_health_ui.confirmation);
        assert!(app.core.keyboard_nav());
        drop(app.update_daemon_health_ui(Action::Open));
        assert!(app.daemon_health_ui.preparing);
        drop(app.update_daemon_health_ui(Action::Prepared {
            generation,
            target: Err("old failure".into()),
        }));
        assert!(app.daemon_health_ui.preparing);
        assert!(app.daemon_health_ui.error.is_none());
        drop(app.update(Message::DaemonHealth(daemon_health::State::Responsive)));
        assert!(
            !app.daemon_health_ui.confirmation,
            "a reply cancels an unsubmitted restart confirmation"
        );
        assert!(app.core.keyboard_nav());
    }
    #[test]
    fn daemon_health_dialog_returns_focus_on_cancel_reconnection_and_restart() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
        app.workspace.resize_viewport(1200.0);
        for finish in 0..4 {
            app.daemon_health_ui.health = daemon_health::State::Unreachable {
                since: std::time::SystemTime::UNIX_EPOCH,
            };
            drop(app.update_daemon_health_ui(Action::Open));
            assert!(app.daemon_health_ui.confirmation);
            let task = match finish {
                0 => app.update_daemon_health_ui(Action::Cancel),
                1 => app.update(Message::DaemonHealth(daemon_health::State::Responsive)),
                2 => {
                    app.daemon_health_ui.restarting = true;
                    app.update_daemon_health_ui(Action::Finished(Ok(())))
                }
                _ => {
                    app.daemon_health_ui.restarting = true;
                    app.daemon_health_ui.focus = 1;
                    app.update_daemon_health_ui(Action::Finished(Err(
                        "fixture restart refused".into()
                    )))
                }
            };
            if finish == 3 {
                assert!(app.daemon_health_ui.confirmation);
                assert_eq!(app.daemon_health_ui.focus, 0);
                assert_eq!(
                    super::super::tests::focused_element(app.daemon_health_dialog().unwrap(), task),
                    Some(widget::Id::new("daemon-health.cancel"))
                );
                drop(app.update_daemon_health_ui(Action::Cancel));
            } else {
                assert!(!app.daemon_health_ui.confirmation);
                assert_eq!(
                    super::super::tests::focused(&app, task),
                    Some(crate::settings_widgets::search_id())
                );
            }
            assert!(
                iced::runtime::task::into_stream(app.update_daemon_health_ui(Action::Cancel))
                    .is_none(),
                "a stale cancel cannot steal a subsequent control's focus"
            );
        }
    }
}
