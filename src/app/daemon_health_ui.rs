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
    CheckAgain {
        generation: u64,
    },
    Cancel,
    Confirm {
        generation: u64,
    },
    Prepared {
        generation: u64,
        target: Result<Arc<crate::daemon::RestartTarget>, String>,
    },
    Finished(Result<(), String>),
}

impl State {
    fn available_action(&self) -> Option<Action> {
        if self.preparing || self.restarting || !self.health.unavailable() {
            None
        } else if self.target.is_some() {
            Some(Action::Confirm {
                generation: self.generation,
            })
        } else if self.error.is_some() && !crate::device::is_remote() {
            Some(Action::CheckAgain {
                generation: self.generation,
            })
        } else {
            None
        }
    }
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
                state.confirmation = true;
                state.restore_keyboard_nav = Some(self.core.keyboard_nav());
                self.core.set_keyboard_nav(false);
                self.prepare_daemon_restart()
            }
            Action::CheckAgain { generation }
                if generation == self.daemon_health_ui.generation
                    && !self.daemon_health_is_covered()
                    && self.daemon_health_ui.confirmation
                    && matches!(
                        self.daemon_health_ui.available_action(),
                        Some(Action::CheckAgain { .. })
                    ) =>
            {
                self.prepare_daemon_restart()
            }
            Action::Prepared { generation, target }
                if self.daemon_health_ui.confirmation
                    && self.daemon_health_ui.preparing
                    && self.daemon_health_ui.generation == generation =>
            {
                let state = &mut self.daemon_health_ui;
                state.preparing = false;
                state.focus = 0;
                match target {
                    Ok(target) => state.target = Some(target),
                    Err(error) => state.error = Some(error),
                }
                self.focus_daemon_health_if_exposed()
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
            Action::Confirm { generation }
                if generation == self.daemon_health_ui.generation
                    && !self.daemon_health_is_covered()
                    && self.daemon_health_ui.confirmation
                    && matches!(
                        self.daemon_health_ui.available_action(),
                        Some(Action::Confirm { .. })
                    ) =>
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
                    self.focus_daemon_health_if_exposed()
                } else {
                    self.daemon_health_restore_focus()
                }
            }
            _ => Task::none(),
        }
    }
    /// Verification never signals a process. A newly captured target still
    /// requires the person's separate Restart confirmation.
    fn prepare_daemon_restart(&mut self) -> Task<Message> {
        let state = &mut self.daemon_health_ui;
        state.generation = state.generation.wrapping_add(1);
        state.preparing = true;
        state.target = None;
        state.error = None;
        state.focus = 0;
        let generation = state.generation;
        let preparation = cosmic::task::future(async move {
            let target = tokio::task::spawn_blocking(crate::daemon::restart_target)
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map(Arc::new).map_err(|error| error.to_string()));
            Message::DaemonHealthUi(Action::Prepared { generation, target })
        });
        Task::batch([preparation, health_focus(state, 0)])
    }

    pub(super) fn daemon_health_is_covered(&self) -> bool {
        #[cfg(feature = "integrated-browser")]
        if self.browser_import.open() {
            return true;
        }
        self.automation_approval_open()
            || self.transient_work_close.is_some()
            || self.automation_ui.draft_dialog_open()
            || self.device_ui.open
            || self.chat_ui.image_viewer.is_some()
            || self.terminal_paste.is_some()
            || self.session_recovery.open()
    }

    pub(super) fn focus_daemon_health_after_cover(&mut self) -> Task<Message> {
        self.daemon_health_ui.focus = 0;
        self.focus_daemon_health_if_exposed()
    }

    fn focus_daemon_health_if_exposed(&self) -> Task<Message> {
        if self.daemon_health_is_covered() {
            Task::none()
        } else {
            health_focus(&self.daemon_health_ui, 0)
        }
    }

    fn daemon_health_restore_focus(&self) -> Task<Message> {
        if self.daemon_health_is_covered() {
            return Task::none();
        }
        let target = self
            .workspace
            .focused()
            .map(|card| self.card_keyboard_focus(card))
            .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID));
        iced::runtime::task::effect(iced::runtime::Action::widget(HealthFocus {
            marker: None,
            target,
            mounted: false,
            present: false,
            foreign_focus: false,
        }))
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
        if !self.daemon_health_ui.confirmation || self.daemon_health_is_covered() {
            return None;
        }
        let keyboard::Event::KeyPressed {
            key,
            modifiers,
            repeat,
            ..
        } = event
        else {
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
            let count = 1 + usize::from(self.daemon_health_ui.available_action().is_some());
            let backwards = modifiers.shift() || *key == Named::ArrowLeft;
            self.daemon_health_ui.focus = if backwards {
                (self.daemon_health_ui.focus.min(count - 1) + count - 1) % count
            } else {
                (self.daemon_health_ui.focus + 1) % count
            };
            return Some(health_focus(
                &self.daemon_health_ui,
                self.daemon_health_ui.focus,
            ));
        }
        if *key == Named::Enter
            && !repeat
            && !captured
            && !modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
        {
            let action = if self.daemon_health_ui.focus == 1 {
                self.daemon_health_ui
                    .available_action()
                    .unwrap_or(Action::Cancel)
            } else {
                Action::Cancel
            };
            return Some(self.update_daemon_health_ui(action));
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
        let available_action = state.available_action();
        let confirm = if matches!(available_action, Some(Action::CheckAgain { .. })) {
            rows.push(notice_detail(
                "Check again to verify the current daemon. Restarting still requires a separate confirmation.",
            ));
            crate::ui_controls::button::secondary("Check again")
        } else {
            crate::ui_controls::button::destructive("Restart daemon")
        };
        rows.push(dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("daemon-health.cancel"))
                .on_press_maybe(
                    (!state.restarting).then_some(Message::DaemonHealthUi(Action::Cancel)),
                ),
            confirm
                .id(health_action_id(state))
                .on_press_maybe(available_action.map(Message::DaemonHealthUi)),
        ));
        let overlay = dialog_ui::overlay(
            dialog_ui::surface(widget::column::with_children(rows).spacing(space::M), 560.0),
            (!state.restarting).then_some(Message::DaemonHealthUi(Action::Cancel)),
            1.0,
        );
        // Both pointer state and lifted named state belong to this review and
        // action. A held Check again cannot become a Restart press.
        Some(
            iced::widget::keyed::Column::with_children([(
                (
                    state.generation,
                    matches!(available_action, Some(Action::CheckAgain { .. })),
                ),
                widget::container(overlay).id(health_marker(state)).into(),
            )])
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        )
    }
}

fn timestamp(time: std::time::SystemTime) -> String {
    let millis = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    unix_time_label(i64::try_from(millis).unwrap_or(i64::MAX))
}

fn health_action_id(state: &State) -> widget::Id {
    let role = if matches!(state.available_action(), Some(Action::CheckAgain { .. })) {
        "check-again"
    } else {
        "confirm"
    };
    widget::Id::new(format!("daemon-health.{}.{role}", state.generation))
}

fn health_marker(state: &State) -> widget::Id {
    // A newer keyboard choice or review phase withdraws earlier focus effects
    // even when it belongs to the same ownership verification generation.
    widget::Id::new(format!(
        "daemon-health.{}.dialog.{}.{}.{}.{}.{}.{}",
        state.generation,
        state.focus,
        state.preparing,
        state.restarting,
        state.target.is_some(),
        state.error.is_some(),
        state.health.unavailable(),
    ))
}

fn health_focus(state: &State, index: usize) -> Task<Message> {
    iced::runtime::task::effect(iced::runtime::Action::widget(HealthFocus {
        marker: Some(health_marker(state)),
        target: if index == 1 {
            health_action_id(state)
        } else {
            widget::Id::new("daemon-health.cancel")
        },
        mounted: false,
        present: false,
        foreign_focus: false,
    }))
}

/// Check the mounted frame before chaining focus. A different dialog can open
/// after the application returned this task but before its operation runs.
struct HealthFocus {
    marker: Option<widget::Id>,
    target: widget::Id,
    mounted: bool,
    present: bool,
    foreign_focus: bool,
}

impl iced::advanced::widget::Operation for HealthFocus {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation)) {
        visit(self);
    }
    fn container(&mut self, id: Option<&widget::Id>, _: iced::Rectangle) {
        self.mounted |= self
            .marker
            .as_ref()
            .is_some_and(|marker| id == Some(marker));
    }
    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        _: iced::Rectangle,
        state: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        self.present |= id == Some(&self.target);
        self.foreign_focus |= state.is_focused() && id != Some(&self.target);
    }
    fn finish(&self) -> iced::advanced::widget::operation::Outcome<()> {
        use iced::advanced::widget::operation::{Outcome, focusable};
        if self.present
            && if self.marker.is_some() {
                self.mounted
            } else {
                !self.foreign_focus
            }
        {
            Outcome::Chain(Box::new(focusable::focus(self.target.clone())))
        } else {
            Outcome::None
        }
    }
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
        drop(app.update_daemon_health_ui(Action::Confirm {
            generation: app.daemon_health_ui.generation,
        }));
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
        drop(app.update_daemon_health_ui(Action::Confirm {
            generation: app.daemon_health_ui.generation,
        }));
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

    #[test]
    fn failed_daemon_verification_can_check_again_with_fresh_generation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.core.set_keyboard_nav(true);
        app.daemon_health_ui.health = daemon_health::State::Unreachable {
            since: std::time::SystemTime::UNIX_EPOCH,
        };
        drop(app.update_daemon_health_ui(Action::Open));
        let original = app.daemon_health_ui.generation;
        let task = app.update_daemon_health_ui(Action::Prepared {
            generation: original,
            target: Err("ownership changed".into()),
        });
        assert_eq!(
            super::super::tests::focused_element(app.daemon_health_dialog().unwrap(), task),
            Some(widget::Id::new("daemon-health.cancel")),
        );
        assert!(matches!(
            app.daemon_health_ui.available_action(),
            Some(Action::CheckAgain { .. })
        ));
        let message = dialog_ui::tests::enter_control(
            app.daemon_health_dialog().unwrap(),
            health_action_id(&app.daemon_health_ui),
        );
        assert!(matches!(
            message,
            Message::DaemonHealthUi(Action::CheckAgain { .. })
        ));
        drop(app.update(message));
        let retry = app.daemon_health_ui.generation;
        assert_ne!(retry, original);
        assert!(app.daemon_health_ui.preparing);
        assert!(!app.daemon_health_ui.restarting);
        assert!(app.daemon_health_ui.target.is_none());
        assert!(app.daemon_health_ui.error.is_none());
        assert!(app.daemon_health_ui.available_action().is_none());
        assert_eq!(app.daemon_health_ui.focus, 0);
        assert_eq!(app.daemon_health_ui.restore_keyboard_nav, Some(true));
        drop(app.update_daemon_health_ui(Action::CheckAgain {
            generation: app.daemon_health_ui.generation,
        }));
        assert_eq!(
            app.daemon_health_ui.generation, retry,
            "one verification at a time"
        );
        drop(app.update_daemon_health_ui(Action::Confirm {
            generation: app.daemon_health_ui.generation,
        }));
        assert!(
            !app.daemon_health_ui.restarting,
            "checking cannot authorize a restart"
        );
        drop(app.update_daemon_health_ui(Action::Prepared {
            generation: original,
            target: Err("obsolete failure".into()),
        }));
        assert!(app.daemon_health_ui.preparing);
        assert!(app.daemon_health_ui.error.is_none());
        drop(app.update_daemon_health_ui(Action::Prepared {
            generation: retry,
            target: Err("current failure".into()),
        }));
        drop(app.update_daemon_health_ui(Action::Prepared {
            generation: retry,
            target: Err("duplicate failure".into()),
        }));
        assert_eq!(
            app.daemon_health_ui.error.as_deref(),
            Some("current failure")
        );
        drop(app.daemon_health_key_event(&key(keyboard::key::Named::Escape), false));
        assert!(!app.daemon_health_ui.confirmation);
        assert!(app.core.keyboard_nav());
    }

    #[test]
    fn failed_daemon_restart_exposes_keyboard_check_again_without_replaying_restart() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.daemon_health_ui.health = daemon_health::State::Unresponsive {
            since: std::time::SystemTime::UNIX_EPOCH,
            timeout: Duration::from_secs(5),
        };
        drop(app.update_daemon_health_ui(Action::Open));
        app.daemon_health_ui.preparing = false;
        app.daemon_health_ui.restarting = true;
        drop(app.update_daemon_health_ui(Action::Finished(Err("restart refused".into()))));
        assert!(matches!(
            app.daemon_health_ui.available_action(),
            Some(Action::CheckAgain { .. })
        ));
        let generation = app.daemon_health_ui.generation;
        drop(app.daemon_health_key_event(&key(keyboard::key::Named::Tab), false));
        assert_eq!(app.daemon_health_ui.focus, 1);
        drop(app.daemon_health_key_event(&key(keyboard::key::Named::Enter), false));
        assert_ne!(app.daemon_health_ui.generation, generation);
        assert!(app.daemon_health_ui.preparing);
        assert!(!app.daemon_health_ui.restarting);
        assert!(app.daemon_health_ui.target.is_none());
        assert_eq!(app.daemon_health_ui.focus, 0);
        drop(app.daemon_health_key_event(&key(keyboard::key::Named::Enter), false));
        assert!(
            !app.daemon_health_ui.confirmation,
            "default Enter stays on Cancel"
        );
    }

    #[test]
    fn covered_daemon_results_and_cancellation_leave_device_focus_untouched() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.daemon_health_ui.health = daemon_health::State::Unreachable {
            since: std::time::SystemTime::UNIX_EPOCH,
        };
        for completion in 0..4 {
            drop(app.update_daemon_health_ui(Action::Open));
            app.device_ui.open = true;
            let task = match completion {
                0 => app.update_daemon_health_ui(Action::Prepared {
                    generation: app.daemon_health_ui.generation,
                    target: Err("verification refused".into()),
                }),
                1 | 2 => {
                    app.daemon_health_ui.preparing = false;
                    app.daemon_health_ui.restarting = true;
                    app.update_daemon_health_ui(Action::Finished(if completion == 1 {
                        Err("restart refused".into())
                    } else {
                        Ok(())
                    }))
                }
                _ => app.update_daemon_health_ui(Action::Cancel),
            };
            assert!(
                iced::runtime::task::into_stream(task).is_none(),
                "covered recovery must not emit any focus operation"
            );
            assert!(app.device_ui.open);
            app.device_ui.open = false;
            drop(app.update_daemon_health_ui(Action::Cancel));
        }
    }

    #[test]
    fn exposing_daemon_dialog_resets_logical_and_native_focus_to_cancel() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.daemon_health_ui.confirmation = true;
        app.daemon_health_ui.focus = 1;
        app.device_ui.open = true;
        assert!(iced::runtime::task::into_stream(app.focus_daemon_health_after_cover()).is_none());
        assert_eq!(app.daemon_health_ui.focus, 0);
        app.daemon_health_ui.focus = 1;
        app.device_ui.open = false;
        let task = app.focus_daemon_health_after_cover();
        assert_eq!(app.daemon_health_ui.focus, 0);
        assert_eq!(
            super::super::tests::focused_element(app.daemon_health_dialog().unwrap(), task),
            Some(widget::Id::new("daemon-health.cancel"))
        );
        drop(app.daemon_health_key_event(&key(keyboard::key::Named::Enter), false));
        assert!(!app.daemon_health_ui.confirmation);
    }

    fn daemon_restart_review_with_dirty_draft() -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.automation_ui.update(crate::automation_ui::Action::New);
        app.automation_ui
            .update(crate::automation_ui::Action::Field(
                crate::automation_ui::Field::Name,
                "Retained draft".into(),
            ));
        app.daemon_health_ui.health = daemon_health::State::Unreachable {
            since: std::time::SystemTime::UNIX_EPOCH,
        };
        drop(app.update(Message::DaemonHealthUi(Action::Open)));
        drop(app.update(Message::DaemonHealthUi(Action::Prepared {
            generation: app.daemon_health_ui.generation,
            target: Ok(Arc::new(crate::daemon::RestartTarget::inert_for_ui_test())),
        })));
        drop(app.update(Message::KeyEvent {
            event: key(keyboard::key::Named::Tab),
            captured: false,
        }));
        assert_eq!(app.daemon_health_ui.focus, 1);
        assert!(matches!(
            app.daemon_health_ui.available_action(),
            Some(Action::Confirm { .. })
        ));
        app
    }

    #[test]
    fn cancelling_transient_close_returns_daemon_review_to_cancel() {
        let mut app = daemon_restart_review_with_dirty_draft();
        let window = window::Id::unique();
        drop(app.update(Message::WindowCloseRequested(window)));
        assert_eq!(app.transient_work_close, Some(window));
        let cancel = dialog_ui::tests::enter_control(
            app.dialog().unwrap(),
            widget::Id::new("dialog.transient-close.cancel"),
        );
        assert!(matches!(cancel, Message::CancelTransientWorkClose));
        let restoring = app.update(cancel);
        assert!(app.transient_work_close.is_none());
        assert_eq!(app.daemon_health_ui.focus, 0);
        assert_eq!(
            super::super::tests::focused_element(app.dialog().unwrap(), restoring),
            Some(widget::Id::new("daemon-health.cancel")),
        );
        drop(app.update(Message::KeyEvent {
            event: key(keyboard::key::Named::Enter),
            captured: false,
        }));
        assert!(!app.daemon_health_ui.confirmation);
        assert!(!app.daemon_health_ui.restarting);
        assert!(app.automation_ui.draft_dirty());
    }

    #[test]
    fn transient_close_cancellation_only_resets_a_newly_exposed_daemon_review() {
        for scene in ["stale", "clean-device", "dirty-device"] {
            let mut app = daemon_restart_review_with_dirty_draft();
            let generation = app.daemon_health_ui.generation;
            let target = app.daemon_health_ui.target.as_ref().unwrap().clone();
            if scene != "stale" {
                drop(app.update(Message::WindowCloseRequested(window::Id::unique())));
                assert!(app.transient_work_close.is_some());
                app.device_ui.open = true;
            }
            if scene == "dirty-device" {
                use cosmic::widget::text_editor;
                // The first request on fresh default Device state owns
                // generation 1. Drop its worker task and inject the reply.
                drop(app.update_device(device_ui::Action::Read("retained.txt".into())));
                drop(app.update_device(device_ui::Action::Loaded {
                    generation: 1,
                    result: Ok(crate::device_service::Reply::Text("original".into())),
                }));
                drop(
                    app.update_device(device_ui::Action::Edit(text_editor::Action::Edit(
                        text_editor::Edit::Insert('!'),
                    ))),
                );
                assert!(app.device_ui.dirty());
            }
            let restoring = app.update(Message::CancelTransientWorkClose);
            if scene == "dirty-device" {
                assert_eq!(
                    super::super::tests::focused_element(app.dialog().unwrap(), restoring),
                    Some(device_ui::file_editor_id()),
                    "the mounted device editor owns focus above the daemon review"
                );
            } else {
                assert_eq!(restoring.units(), 0, "{scene} cannot emit a focus effect");
            }
            assert!(app.transient_work_close.is_none());
            assert_eq!(
                app.daemon_health_ui.focus, 1,
                "only removing the last actual cover may reset the daemon review"
            );
            assert!(app.daemon_health_ui.confirmation);
            assert!(!app.daemon_health_ui.restarting);
            assert_eq!(app.daemon_health_ui.generation, generation);
            assert!(Arc::ptr_eq(
                app.daemon_health_ui.target.as_ref().unwrap(),
                &target
            ));
            assert!(app.automation_ui.draft_dirty());
        }
    }

    #[test]
    fn delayed_daemon_focus_tasks_leave_newly_mounted_keyboard_owner_focused() {
        for restore in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
            app.daemon_health_ui.confirmation = true;
            let delayed = if restore {
                app.daemon_health_restore_focus()
            } else {
                health_focus(&app.daemon_health_ui, 0)
            };
            let owner = widget::Id::new("fixture.new-dialog.input");
            // The newer dialog is the mounted frame when the older operation
            // executes. Keep the underlying Settings target mounted too for
            // the restoration case; target presence alone is insufficient.
            let element = widget::column::with_children(vec![
                crate::ui_controls::field::text("New dialog", "")
                    .id(owner.clone())
                    .on_input(|_| Message::SettingsSearchChanged(String::new()))
                    .into(),
                crate::ui_controls::field::text("Underlying settings", "")
                    .id(crate::settings_widgets::search_id())
                    .on_input(|_| Message::SettingsSearchChanged(String::new()))
                    .into(),
            ])
            .into();
            let task = iced::widget::operation::focus(owner.clone())
                .map(cosmic::Action::App)
                .chain(delayed);
            assert_eq!(
                super::super::tests::focused_element(element, task),
                Some(owner)
            );
        }
    }

    #[test]
    fn delayed_daemon_preparation_focus_cannot_override_a_newer_tab_choice() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.daemon_health_ui.health = daemon_health::State::Unreachable {
            since: std::time::SystemTime::UNIX_EPOCH,
        };
        drop(app.update_daemon_health_ui(Action::Open));
        let older = app.update_daemon_health_ui(Action::Prepared {
            generation: app.daemon_health_ui.generation,
            target: Err("verification refused".into()),
        });
        assert_eq!(app.daemon_health_ui.focus, 0);
        let newer = app
            .daemon_health_key_event(&key(keyboard::key::Named::Tab), false)
            .expect("the visible daemon dialog owns Tab");
        assert_eq!(app.daemon_health_ui.focus, 1);
        let current_action = health_action_id(&app.daemon_health_ui);
        assert_eq!(
            super::super::tests::focused_element(
                app.daemon_health_dialog().unwrap(),
                newer.chain(older),
            ),
            Some(current_action),
            "the current Tab effect must focus Check again; older Cancel must have no effect",
        );
    }

    #[test]
    fn successful_daemon_recheck_requires_a_new_separate_confirmation() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.daemon_health_ui.health = daemon_health::State::Unreachable {
            since: std::time::SystemTime::UNIX_EPOCH,
        };
        drop(app.update_daemon_health_ui(Action::Open));
        let original = app.daemon_health_ui.generation;
        drop(app.update_daemon_health_ui(Action::Prepared {
            generation: original,
            target: Err("ownership changed".into()),
        }));
        let check_id = health_action_id(&app.daemon_health_ui);
        drop(app.update_daemon_health_ui(Action::CheckAgain {
            generation: original,
        }));
        let current = app.daemon_health_ui.generation;
        let target = Arc::new(crate::daemon::RestartTarget::inert_for_ui_test());
        drop(app.update_daemon_health_ui(Action::Prepared {
            generation: current,
            target: Ok(target.clone()),
        }));
        assert!(!app.daemon_health_ui.restarting);
        assert!(Arc::ptr_eq(
            app.daemon_health_ui.target.as_ref().unwrap(),
            &target
        ));
        assert!(matches!(app.daemon_health_ui.available_action(),
            Some(Action::Confirm { generation }) if generation == current));
        assert_ne!(check_id, health_action_id(&app.daemon_health_ui));
        for stale in [
            Action::CheckAgain {
                generation: original,
            },
            Action::Confirm {
                generation: original,
            },
        ] {
            drop(app.update_daemon_health_ui(stale));
            assert!(!app.daemon_health_ui.restarting);
            assert!(Arc::ptr_eq(
                app.daemon_health_ui.target.as_ref().unwrap(),
                &target
            ));
        }
        app.daemon_health_ui.focus = 1;
        let mut repeated = key(keyboard::key::Named::Enter);
        if let keyboard::Event::KeyPressed { repeat, .. } = &mut repeated {
            *repeat = true;
        }
        for captured in [false, true] {
            drop(app.daemon_health_key_event(&repeated, captured));
            assert!(!app.daemon_health_ui.restarting);
            assert!(Arc::ptr_eq(
                app.daemon_health_ui.target.as_ref().unwrap(),
                &target
            ));
        }
        let message = dialog_ui::tests::enter_control(
            app.daemon_health_dialog().unwrap(),
            health_action_id(&app.daemon_health_ui),
        );
        assert!(
            matches!(message, Message::DaemonHealthUi(Action::Confirm { generation }) if generation == current)
        );
        // This task is deliberately dropped: state transition is under test,
        // and the fixture cannot signal any process.
        drop(app.update(message));
        assert!(app.daemon_health_ui.restarting);
        assert!(app.daemon_health_ui.target.is_none());
    }

    fn pointer_frame(
        element: &mut Element<'_, Message>,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        id: &widget::Id,
    ) -> (iced::advanced::layout::Node, iced::Point) {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, operation::Focusable},
        };
        struct Bounds(widget::Id, Option<iced::Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn Focusable,
            ) {
                if id == Some(&self.0) {
                    self.1 = Some(bounds);
                }
            }
        }
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1000.0, 900.0)),
        );
        let mut bounds = Bounds(id.clone(), None);
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), renderer, &mut bounds);
        (node, bounds.1.expect("review action is mounted").center())
    }

    fn pointer_event(
        element: &mut Element<'_, Message>,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        node: &iced::advanced::layout::Node,
        point: iced::Point,
        event: iced::mouse::Event,
    ) -> Vec<Message> {
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            tree,
            &iced::Event::Mouse(event),
            iced::advanced::Layout::new(node),
            iced::mouse::Cursor::Available(point),
            renderer,
            &mut iced::advanced::clipboard::Null,
            &mut iced::advanced::Shell::new(&mut messages),
            &iced::Rectangle::with_size(iced::Size::new(1000.0, 900.0)),
        );
        messages
    }

    #[test]
    fn held_daemon_review_press_cannot_restart_a_freshly_verified_target() {
        use iced::{
            advanced::widget::{Tree, tree},
            mouse,
        };
        for original_confirm in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            app.daemon_health_ui.health = daemon_health::State::Unreachable {
                since: std::time::SystemTime::UNIX_EPOCH,
            };
            drop(app.update_daemon_health_ui(Action::Open));
            let original = app.daemon_health_ui.generation;
            drop(app.update_daemon_health_ui(Action::Prepared {
                generation: original,
                target: if original_confirm {
                    Ok(Arc::new(crate::daemon::RestartTarget::inert_for_ui_test()))
                } else {
                    Err("verification refused".into())
                },
            }));
            let renderer = super::super::tests::headless_renderer();
            let mut element = app.daemon_health_dialog().unwrap();
            let mut tree_state = Tree::new(element.as_widget());
            let (node, point) = pointer_frame(
                &mut element,
                &mut tree_state,
                &renderer,
                &health_action_id(&app.daemon_health_ui),
            );
            assert!(
                pointer_event(
                    &mut element,
                    &mut tree_state,
                    &renderer,
                    &node,
                    point,
                    mouse::Event::ButtonPressed(mouse::Button::Left)
                )
                .is_empty()
            );
            drop(element);
            if original_confirm {
                app.daemon_health_ui.target = None;
                app.daemon_health_ui.restarting = true;
                drop(app.update_daemon_health_ui(Action::Finished(Err("restart refused".into()))));
            }
            drop(app.update_daemon_health_ui(Action::CheckAgain {
                generation: original,
            }));
            let current = app.daemon_health_ui.generation;
            drop(app.update_daemon_health_ui(Action::Prepared {
                generation: current,
                target: Ok(Arc::new(crate::daemon::RestartTarget::inert_for_ui_test())),
            }));
            let mut element = app.daemon_health_dialog().unwrap();
            // Model iced's actual named-state lift/restore around keyed diff.
            // There is deliberately no intervening empty widget frame.
            tree::NAMED.with(|named| *named.borrow_mut() = tree_state.take_all_named());
            tree_state.diff(element.as_widget_mut());
            iced::runtime::user_interface::restore_cached_named_states(&mut tree_state);
            tree::NAMED.with(|named| named.borrow_mut().clear());
            let (node, point) = pointer_frame(
                &mut element,
                &mut tree_state,
                &renderer,
                &health_action_id(&app.daemon_health_ui),
            );
            assert!(
                pointer_event(
                    &mut element,
                    &mut tree_state,
                    &renderer,
                    &node,
                    point,
                    mouse::Event::ButtonReleased(mouse::Button::Left)
                )
                .is_empty(),
                "a held gesture cannot inherit a fresh restart target"
            );
            assert!(
                pointer_event(
                    &mut element,
                    &mut tree_state,
                    &renderer,
                    &node,
                    point,
                    mouse::Event::ButtonPressed(mouse::Button::Left)
                )
                .is_empty()
            );
            let messages = pointer_event(
                &mut element,
                &mut tree_state,
                &renderer,
                &node,
                point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            );
            assert!(matches!(messages.as_slice(),
                [Message::DaemonHealthUi(Action::Confirm { generation })] if *generation == current));
        }
    }
}
