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
}

#[derive(Clone, Debug)]
pub enum Action {
    Check,
    Checked(Result<UpdateState, String>),
    Apply,
    Applied(Result<UpdateJob, String>),
    Poll,
    Polled(Option<String>, Result<Option<UpdateJob>, String>),
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
        for runtime in self.cards.values() {
            match runtime {
                CardRuntime::Repository(repository) if repository.blocks_navigation() => {
                    return Some("Save or discard open editor changes before updating.");
                }
                CardRuntime::Chat(chat) => {
                    if let Some(reason) = chat.transition_blocker() {
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
            || matches!(self.update_ui.result, Some(UpdateState::Available { .. }))
    }

    pub(super) fn update_subscription(&self) -> Subscription<Message> {
        if crate::instance::NAME == "development" {
            return Subscription::none();
        }
        if self.update_ui.updating() {
            iced::time::every(Duration::from_secs(1)).map(|_| Message::UpdateUi(Action::Poll))
        } else {
            iced::time::every(Duration::from_secs(6 * 60 * 60))
                .map(|_| Message::UpdateUi(Action::Check))
        }
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
        Some(
            crate::ui_controls::icon_control(
                crate::ui_controls::symbolic_icon(icon),
                label,
                false,
                !self.modal_is_open() && action.is_some(),
                (!self.modal_is_open()).then_some(action).flatten(),
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
                if self.modal_is_open() || self.update_ui.updating() {
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
        }
    }
}
