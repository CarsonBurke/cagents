use super::*;
use crate::update::{UpdateJob, UpdatePhase, UpdateState};

#[derive(Default)]
pub(super) struct State {
    result: Option<UpdateState>,
    job: Option<UpdateJob>,
    error: Option<String>,
    checking: bool,
    applying: bool,
    polling: bool,
    /// The installed UI executable this window has been left behind by.
    superseded: Option<PathBuf>,
    restarting: bool,
}

#[derive(Clone, Debug)]
pub enum Action {
    Check,
    Checked(Result<UpdateState, String>),
    Apply,
    Applied(Result<UpdateJob, String>),
    Poll,
    Polled(Option<String>, Result<Option<UpdateJob>, String>),
    InspectInstallation,
    Restart,
    Restarted(Result<(), String>),
    RestartLapsed,
}

impl State {
    fn updating(&self) -> bool {
        self.applying
            || self.job.as_ref().is_some_and(|job| {
                !matches!(job.phase, UpdatePhase::Complete | UpdatePhase::Failed)
            })
    }
}

impl Cagents {
    pub(super) fn window_update_blocker(&self) -> Option<&'static str> {
        if self.modal_is_open()
            || self.has_active_worktree_operations()
            || self
                .sidebar_model
                .source_control
                .as_ref()
                .is_some_and(|source| source.busy())
            || !self.settings_pending.is_empty()
            || self.chat_ui.transition_pending()
        {
            return Some("Finish pending operations and dialogs before updating.");
        }
        if let Some(reason) = self.parked_chat_restart_blocker() {
            return Some(reason);
        }
        for runtime in self.cards.values() {
            match runtime {
                CardRuntime::Repository(repository) if repository.blocks_navigation() => {
                    return Some("Save or discard open editor changes before updating.");
                }
                CardRuntime::Chat(chat) => {
                    if let Some(reason) = chat.restart_blocker() {
                        return Some(reason);
                    }
                }
                _ => {}
            }
        }
        None
    }

    pub(super) fn has_update_control(&self) -> bool {
        self.update_ui.updating()
            || self.update_ui.error.is_some()
            || self.update_ui.superseded.is_some()
            || matches!(self.update_ui.result, Some(UpdateState::Available { .. }))
    }

    pub(super) fn update_subscription(&self) -> Subscription<Message> {
        // An install performed under this window breaks every terminal it owns
        // the moment the daemon restarts, so the window has to notice on its
        // own timescale rather than on the release check's. This needs no
        // instance gate: it is inert unless the running executable is itself
        // inside an installation tree, which a source build never is.
        let installation = iced::time::every(Duration::from_secs(5))
            .map(|_| Message::UpdateUi(Action::InspectInstallation));
        // Release discovery talks to GitHub and drives the installer, neither
        // of which a development instance may do.
        if crate::instance::NAME == "development" {
            return installation;
        }
        let progress = if self.update_ui.updating() {
            iced::time::every(Duration::from_secs(1)).map(|_| Message::UpdateUi(Action::Poll))
        } else {
            iced::time::every(Duration::from_secs(6 * 60 * 60))
                .map(|_| Message::UpdateUi(Action::Check))
        };
        Subscription::batch([progress, installation])
    }

    pub(super) fn update_control(&self) -> Option<Element<'_, Message>> {
        let state = &self.update_ui;
        let (icon, label, action) = if state.updating() {
            (
                "software-update-available-symbolic",
                state.job.as_ref().map_or_else(
                    || "Preparing application update".to_owned(),
                    |job| format!("Updating to {}: {}", job.version, job.message),
                ),
                None,
            )
        } else if state.restarting {
            (
                "software-update-available-symbolic",
                "Saving and reopening windows with the installed build".to_owned(),
                None,
            )
        } else if state.superseded.is_some() {
            (
                "software-update-available-symbolic",
                "A newer build is installed; reopen windows to use it".to_owned(),
                Some(Message::UpdateUi(Action::Restart)),
            )
        } else if let Some(error) = &state.error {
            match &state.result {
                Some(UpdateState::Available { release }) => (
                    "dialog-warning-symbolic",
                    format!(
                        "Update failed: {error}. Retry update to {}",
                        release.version
                    ),
                    Some(Message::UpdateUi(Action::Apply)),
                ),
                _ => (
                    "dialog-warning-symbolic",
                    format!("Update failed: {error}. Check again"),
                    Some(Message::UpdateUi(Action::Check)),
                ),
            }
        } else if let Some(UpdateState::Available { release }) = &state.result {
            (
                "software-update-available-symbolic",
                format!(
                    "Update to {} (saves and restarts application windows)",
                    release.version
                ),
                Some(Message::UpdateUi(Action::Apply)),
            )
        } else {
            return None;
        };
        // `window_update_blocker` already computes the actionable reason this
        // window would refuse the update; without consulting it here the
        // control stays live, `update::apply` runs, and the refusal only
        // surfaces asynchronously through the notices popover.
        let blocker = matches!(
            action,
            Some(Message::UpdateUi(Action::Apply | Action::Restart))
        )
        .then(|| self.window_update_blocker())
        .flatten();
        let label = match blocker {
            Some(reason) => format!("{label} — {reason}"),
            None => label,
        };
        let enabled = !self.modal_is_open() && action.is_some() && blocker.is_none();
        Some(
            crate::ui_controls::icon_control(
                crate::ui_controls::symbolic_icon(icon),
                label,
                false,
                enabled,
                enabled.then_some(action).flatten(),
            )
            .id(widget::Id::new("application-update"))
            .into(),
        )
    }

    pub(super) fn update_update_ui(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Check => {
                if crate::instance::NAME == "development"
                    || self.update_ui.checking
                    || self.update_ui.updating()
                {
                    return Task::none();
                }
                self.update_ui.checking = true;
                self.update_ui.error = None;
                cosmic::task::future(async {
                    let result = tokio::task::spawn_blocking(|| {
                        crate::update::check(&crate::update::CancellationToken::default())
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::UpdateUi(Action::Checked(result))
                })
            }
            Action::Checked(result) => {
                self.update_ui.checking = false;
                match result {
                    Ok(result) => {
                        let supported = !matches!(result, UpdateState::Unsupported { .. });
                        self.update_ui.result = Some(result);
                        if supported {
                            return self.update_update_ui(Action::Poll);
                        }
                    }
                    Err(error) => self.update_ui.error = Some(error),
                }
                Task::none()
            }
            Action::Apply => {
                if self.update_ui.updating() {
                    return Task::none();
                }
                // The daemon handshake refuses a blocked update anyway; fail
                // here so the reason reaches the user as a notice instead of
                // an opaque asynchronous failure.
                if let Some(reason) = self.window_update_blocker() {
                    self.status = Some(TransientNotice::warning(reason.to_owned()));
                    return Task::none();
                }
                let Some(UpdateState::Available { release }) = &self.update_ui.result else {
                    return Task::none();
                };
                let release = release.clone();
                self.update_ui.applying = true;
                self.update_ui.error = None;
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        crate::update::apply(&release).map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::UpdateUi(Action::Applied(result))
                })
            }
            Action::Applied(result) => {
                self.update_ui.applying = false;
                match result {
                    Ok(job) => self.update_ui.job = Some(job),
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error.clone()));
                        self.update_ui.error = Some(error);
                    }
                }
                Task::none()
            }
            Action::Poll => {
                if self.update_ui.polling {
                    return Task::none();
                }
                self.update_ui.polling = true;
                let expected_job = self.update_ui.job.as_ref().map(|job| job.id.clone());
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(|| {
                        crate::update::status().map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::UpdateUi(Action::Polled(expected_job, result))
                })
            }
            Action::Polled(expected_job, result) => {
                self.update_ui.polling = false;
                if self.update_ui.job.as_ref().map(|job| &job.id) != expected_job.as_ref() {
                    return Task::none();
                }
                match result {
                    Ok(Some(job)) => {
                        if matches!(job.phase, UpdatePhase::Failed) {
                            self.update_ui.error = Some(job.message.clone());
                            self.status = Some(TransientNotice::error(job.message.clone()));
                        }
                        self.update_ui.job = Some(job);
                    }
                    Ok(None) => {
                        self.update_ui.job = None;
                        if expected_job.is_some() {
                            self.update_ui.error = Some("Update status is unavailable".into());
                        }
                    }
                    Err(error) => {
                        self.update_ui.job = None;
                        self.update_ui.error = Some(error);
                    }
                }
                Task::none()
            }
            Action::InspectInstallation => {
                if self.update_ui.restarting
                    || self.update_ui.updating()
                    // A release update owns the same launch fence and reopens
                    // the windows itself when it finishes.
                    || crate::instance::update_in_progress()
                {
                    return Task::none();
                }
                // A handful of stats against a warm dentry cache, and a task
                // round-trip would have to re-read all of it before acting.
                self.update_ui.superseded = crate::update::superseding_ui();
                if self.update_ui.superseded.is_none() {
                    return Task::none();
                }
                self.update_update_ui(Action::Restart)
            }
            Action::Restart => {
                let Some(installed) = self.update_ui.superseded.clone() else {
                    return Task::none();
                };
                // Unsaved editor work, an open dialog or a running worktree
                // operation all outrank the restart: the control keeps naming
                // the reason, and the next inspection retries once it clears.
                if self.update_ui.restarting
                    || self.update_ui.updating()
                    || self.window_update_blocker().is_some()
                {
                    return Task::none();
                }
                self.update_ui.restarting = true;
                self.update_ui.error = None;
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        crate::update::restart_into_installed(&installed)
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::UpdateUi(Action::Restarted(result))
                })
            }
            Action::Restarted(result) => match result {
                // The helper now owns the close; this window ordinarily exits
                // inside it. Time the wait out rather than leaving the window
                // permanently convinced a restart is still coming, since a
                // helper can lose the fence to another window's operation.
                Ok(()) => cosmic::task::future(async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Message::UpdateUi(Action::RestartLapsed)
                }),
                Err(error) => {
                    self.update_ui.restarting = false;
                    self.status = Some(TransientNotice::error(error.clone()));
                    self.update_ui.error = Some(error);
                    Task::none()
                }
            },
            Action::RestartLapsed => {
                self.update_ui.restarting = false;
                Task::none()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The restart saves and closes every window, so it must lose to anything
    /// that would refuse to be saved, and it must run exactly once.
    #[test]
    fn a_superseded_window_reopens_only_when_nothing_holds_it_open() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let installed = PathBuf::from("/nonexistent/current/bin/cagents-ui");
        app.update_ui.superseded = Some(installed.clone());
        app.settings_reset_confirm = true;

        drop(app.update_update_ui(Action::Restart));
        assert!(
            !app.update_ui.restarting,
            "a window showing a confirmation dialog must not be closed under it"
        );
        assert!(app.has_update_control());

        app.settings_reset_confirm = false;
        drop(app.update_update_ui(Action::Restart));
        assert!(app.update_ui.restarting);

        // A second tick must neither re-spawn the helper nor forget which
        // executable this window is being replaced by.
        drop(app.update_update_ui(Action::InspectInstallation));
        drop(app.update_update_ui(Action::Restart));
        assert_eq!(app.update_ui.superseded, Some(installed));

        drop(app.update_update_ui(Action::RestartLapsed));
        assert!(
            !app.update_ui.restarting,
            "a helper that never closed this window must not block later attempts"
        );
    }
}
