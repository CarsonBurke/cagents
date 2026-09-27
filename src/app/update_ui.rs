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
    inspecting: bool,
    /// The installed UI executable this window has been left behind by.
    superseded: Option<PathBuf>,
    restarting: bool,
    pub(super) confirmation: Option<crate::update::UpdateConfirmation>,
    submitted_confirmation: Option<String>,
    dismissed: Option<String>,
    dismissal_loaded: bool,
}

#[derive(Clone, Debug)]
pub enum Action {
    LoadDismissal,
    DismissalLoaded(Result<Option<String>, String>),
    DismissNotice(String),
    DismissalSaved(Result<(), String>),
    Check,
    Checked(Result<UpdateState, String>),
    Apply,
    ChoosePolicy(crate::update::impact::Policy),
    Decided {
        job: String,
        nonce: String,
        result: Result<(), String>,
    },
    ConfirmApply(String),
    ReviewJob,
    CancelApply,
    StartApply,
    Applied(Result<UpdateJob, String>),
    Poll,
    Polled(Option<String>, Result<Option<UpdateJob>, String>),
    InspectInstallation,
    InstallationInspected(Option<PathBuf>),
    Restart,
}

impl State {
    fn updating(&self) -> bool {
        self.applying
            || self
                .job
                .as_ref()
                .is_some_and(|job| !job.phase.is_terminal())
    }
}

impl Cagents {
    fn decide_update(&mut self, choice: crate::update::UpdateChoice) -> Task<Message> {
        let Some(confirmation) = self.update_ui.confirmation.take() else {
            return Task::none();
        };
        let Some(job) = &self.update_ui.job else {
            return Task::none();
        };
        let id = job.id.clone();
        self.update_ui.submitted_confirmation = Some(confirmation.nonce.clone());
        cosmic::task::future(async move {
            let expected_job = id.clone();
            let expected_nonce = confirmation.nonce.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::update::decide(&id, &confirmation.nonce, choice)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::UpdateUi(Action::Decided {
                job: expected_job,
                nonce: expected_nonce,
                result,
            })
        })
    }

    pub(super) fn update_dialog(&self) -> Option<Element<'_, Message>> {
        use crate::update::impact::{Disposition, Policy};
        let confirmation = self.update_ui.confirmation.as_ref()?;
        let preview = &confirmation.preview;
        let mut body = vec![
            widget::text::title3(format!("Update to {}?", preview.candidate.version)).into(),
            notice_body(if preview.replaces_daemon {
                "The terminal service will be replaced. Your windows and tabs will reopen automatically. Review what happens to each session below."
            } else {
                "Your windows will reopen with the new build. The terminal service and its sessions stay running."
            }),
        ];
        let mut blocked = false;
        if let Some(daemon) = &preview.daemon {
            let rows = daemon
                .sessions
                .iter()
                .map(|session| {
                    let label = match session.disposition {
                        Disposition::Continues => "Continues",
                        Disposition::Resumes => "Resumes",
                        Disposition::WaitsForIdle => "Waits for idle",
                        Disposition::Interrupted => "Interrupted",
                        Disposition::ResumePicker => "Choose a conversation",
                        Disposition::Blocked => "Needs attention",
                    };
                    notice_body(format!("{} · {label}\n{}", session.title, session.reason))
                })
                .collect::<Vec<_>>();
            body.push(
                widget::container(widget::scrollable(
                    widget::column::with_children(rows).spacing(10),
                ))
                .max_height(320)
                .into(),
            );
            for reason in &daemon.blockers {
                body.push(notice_body(reason));
            }
            blocked = !daemon.blockers.is_empty()
                || daemon
                    .sessions
                    .iter()
                    .any(|session| session.disposition == Disposition::Blocked);
            if daemon.sessions.iter().any(|session| {
                matches!(
                    session.disposition,
                    Disposition::WaitsForIdle | Disposition::Interrupted
                )
            }) {
                let (label, policy) = if daemon.policy == Policy::WhenIdle {
                    ("Review interruption instead", Policy::Interrupt)
                } else {
                    ("Wait for idle instead", Policy::WhenIdle)
                };
                body.push(
                    widget::button::standard(label)
                        .on_press(Message::UpdateUi(Action::ChoosePolicy(policy)))
                        .into(),
                );
            }
        }
        body.push(
            widget::row::with_children(vec![
                iced::widget::Space::new().width(Length::Fill).into(),
                widget::button::standard("Cancel")
                    .id(widget::Id::new("update.cancel"))
                    .on_press(Message::UpdateUi(Action::CancelApply))
                    .into(),
                widget::button::suggested(
                    if preview.daemon.as_ref().is_some_and(|daemon| {
                        daemon
                            .sessions
                            .iter()
                            .any(|session| session.disposition == Disposition::WaitsForIdle)
                    }) {
                        "Update when idle"
                    } else {
                        "Update and reopen"
                    },
                )
                .on_press_maybe((!blocked).then_some(Message::UpdateUi(Action::ConfirmApply(
                    confirmation.nonce.clone(),
                ))))
                .into(),
            ])
            .spacing(8)
            .into(),
        );
        Some(dialog_ui::overlay(
            dialog_ui::surface(widget::column::with_children(body).spacing(16), 600.0),
            Some(Message::UpdateUi(Action::CancelApply)),
            1.0,
        ))
    }

    fn available_update_notice(&self) -> Option<(String, String, Action)> {
        if !self.update_ui.dismissal_loaded
            || self.update_ui.updating()
            || self.update_ui.restarting
        {
            return None;
        }
        if let Some(installed) = &self.update_ui.superseded {
            return Some((
                format!("installed:{}", installed.display()),
                "A newer build is installed and ready to use.".into(),
                Action::Restart,
            ));
        }
        match &self.update_ui.result {
            Some(UpdateState::Available { release }) => Some((
                release.notice_identity(),
                format!("cagents {} is available.", release.version),
                Action::Apply,
            )),
            _ => None,
        }
    }

    pub(super) fn retained_update_notice(&self) -> bool {
        self.available_update_notice()
            .is_some_and(|(identity, _, _)| self.update_ui.dismissed.as_ref() == Some(&identity))
    }

    pub(super) fn update_notice(&self, retained: bool) -> Option<Element<'_, Message>> {
        let (identity, message, action) = self.available_update_notice()?;
        if (self.update_ui.dismissed.as_ref() == Some(&identity)) != retained {
            return None;
        }
        let blocker = self.window_update_blocker();
        let mut description = vec![widget::text::body(message).into()];
        if let Some(reason) = blocker {
            description.push(design::caption(reason).into());
        }
        let mut controls = vec![
            widget::column::with_children(description)
                .width(Length::Fill)
                .into(),
            widget::button::standard("Review update")
                .on_press_maybe((blocker.is_none()).then_some(Message::UpdateUi(action)))
                .into(),
        ];
        if !retained {
            controls.push(
                crate::ui_controls::icon_control(
                    crate::ui_controls::symbolic_icon("window-close-symbolic"),
                    "Keep this update in application notices",
                    false,
                    !self.modal_is_open(),
                    Some(Message::UpdateUi(Action::DismissNotice(identity))),
                )
                .into(),
            );
        }
        Some(
            widget::container(
                widget::row::with_children(controls)
                    .align_y(Alignment::Center)
                    .spacing(8),
            )
            .padding([8, 12])
            .width(Length::Fill)
            .into(),
        )
    }

    pub(super) fn window_update_blocker(&self) -> Option<&'static str> {
        if self.other_modal_is_open()
            || self.has_active_worktree_operations()
            || self.project_ui.deletion_running()
            || self.channel_ui.deletion_running()
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
        if crate::instance::NAME == "development" && !self.update_ui.updating() {
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
                state
                    .job
                    .as_ref()
                    .and_then(UpdateJob::active_confirmation)
                    .map(|_| Message::UpdateUi(Action::ReviewJob)),
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
            Action::LoadDismissal => {
                let Some(store) = self.session.as_ref().map(SessionRuntime::operation_store) else {
                    self.update_ui.dismissal_loaded = true;
                    return Task::none();
                };
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        store
                            .load_dismissed_update()
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|value| value);
                    Message::UpdateUi(Action::DismissalLoaded(result))
                })
            }
            Action::DismissalLoaded(result) => {
                self.update_ui.dismissal_loaded = true;
                match result {
                    Ok(identity) => self.update_ui.dismissed = identity,
                    Err(error) => tracing::warn!(%error, "could not load update notice dismissal"),
                }
                Task::none()
            }
            Action::DismissNotice(identity) => {
                if self
                    .available_update_notice()
                    .is_none_or(|(current, _, _)| current != identity)
                {
                    return Task::none();
                }
                self.update_ui.dismissed = Some(identity.clone());
                let Some(store) = self.session.as_ref().map(SessionRuntime::operation_store) else {
                    return Task::none();
                };
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        store
                            .save_dismissed_update(&identity)
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|value| value);
                    Message::UpdateUi(Action::DismissalSaved(result))
                })
            }
            Action::DismissalSaved(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::warning(format!(
                        "Could not remember update notice dismissal: {error}"
                    )));
                }
                Task::none()
            }
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
                if self.update_ui.updating() || self.window_update_blocker().is_some() {
                    return Task::none();
                }
                self.update_update_ui(Action::StartApply)
            }
            Action::ChoosePolicy(policy) => {
                self.decide_update(crate::update::UpdateChoice::Policy(policy))
            }
            Action::ReviewJob => {
                if !self.other_modal_is_open() {
                    self.update_ui.confirmation = self
                        .update_ui
                        .job
                        .as_ref()
                        .and_then(|job| job.active_confirmation().cloned());
                }
                Task::none()
            }
            Action::ConfirmApply(nonce) => {
                if self
                    .update_ui
                    .confirmation
                    .as_ref()
                    .is_none_or(|confirmation| confirmation.nonce != nonce)
                {
                    return Task::none();
                }
                self.decide_update(crate::update::UpdateChoice::Confirm)
            }
            Action::CancelApply => self.decide_update(crate::update::UpdateChoice::Cancel),
            Action::Decided { job, nonce, result } => {
                if self
                    .update_ui
                    .job
                    .as_ref()
                    .is_none_or(|current| current.id != job)
                    || self.update_ui.submitted_confirmation.as_ref() != Some(&nonce)
                {
                    return Task::none();
                }
                if let Err(error) = result {
                    self.update_ui.submitted_confirmation = None;
                    self.update_ui.error = Some(error);
                }
                self.update_update_ui(Action::Poll)
            }
            Action::StartApply => {
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
                self.update_ui.restarting = false;
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
                        self.update_ui.confirmation =
                            job.confirmation.clone().filter(|confirmation| {
                                matches!(
                                    job.phase,
                                    UpdatePhase::AwaitingConfirmation | UpdatePhase::WaitingForIdle
                                ) && self.update_ui.submitted_confirmation.as_ref()
                                    != Some(&confirmation.nonce)
                                    && !self.other_modal_is_open()
                            });
                        self.update_ui.job = Some(job);
                    }
                    Ok(None) => {
                        self.update_ui.job = None;
                        self.update_ui.confirmation = None;
                        if expected_job.is_some() {
                            self.update_ui.error = Some("Update status is unavailable".into());
                        }
                    }
                    Err(error) => {
                        self.update_ui.job = None;
                        self.update_ui.confirmation = None;
                        self.update_ui.error = Some(error);
                    }
                }
                Task::none()
            }
            Action::InspectInstallation => {
                if self.update_ui.inspecting
                    || self.update_ui.restarting
                    || self.update_ui.updating()
                    // A release update owns the same launch fence and reopens
                    // the windows itself when it finishes.
                    || crate::instance::update_in_progress()
                {
                    return Task::none();
                }
                self.update_ui.inspecting = true;
                cosmic::task::future(async {
                    let installed = tokio::task::spawn_blocking(crate::update::superseding_ui)
                        .await
                        .unwrap_or(None);
                    Message::UpdateUi(Action::InstallationInspected(installed))
                })
            }
            Action::InstallationInspected(installed) => {
                self.update_ui.inspecting = false;
                if !self.update_ui.restarting && !self.update_ui.updating() {
                    self.update_ui.superseded = installed;
                }
                Task::none()
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
                        crate::update::apply_installed(&installed)
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::UpdateUi(Action::Applied(result))
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_banner_moves_to_retained_notices_and_new_builds_announce_again() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.update_ui.dismissal_loaded = true;
        app.update_ui.superseded = Some(PathBuf::from("/install/versions/one/bin/cagents-ui"));
        assert!(app.update_notice(false).is_some());
        assert!(!app.retained_update_notice());
        let identity = app.available_update_notice().unwrap().0;
        drop(app.update_update_ui(Action::DismissNotice(identity.clone())));
        assert!(app.update_notice(false).is_none());
        assert!(app.retained_update_notice());
        assert!(app.update_notice(true).is_some());
        app.update_ui.superseded = Some(PathBuf::from("/install/versions/two/bin/cagents-ui"));
        drop(app.update_update_ui(Action::DismissNotice(identity)));
        assert!(
            app.update_notice(false).is_some(),
            "an old button cannot dismiss a newer build"
        );
        assert!(!app.retained_update_notice());
    }

    fn custom_widget_ids(element: &mut Element<'_, Message>) -> Vec<String> {
        use cosmic::iced::advanced::widget::Tree;
        use iced_accessibility::id::Internal;

        fn walk(tree: &Tree, out: &mut Vec<String>) {
            if let Some(widget::Id(Internal::Custom(_, name))) = &tree.id {
                out.push(name.to_string());
            }
            for child in &tree.children {
                walk(child, out);
            }
        }

        let mut ids = Vec::new();
        walk(&Tree::new(element.as_widget_mut()), &mut ids);
        ids
    }

    fn position(ids: &[String], name: &str) -> Option<usize> {
        ids.iter().position(|id| id == name)
    }

    #[test]
    fn the_update_icon_sits_immediately_right_of_the_alerts() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.update_ui.superseded = Some(PathBuf::from("/nonexistent/current/bin/cagents-ui"));
        app.notices
            .record(TransientNotice::warning("A saved notice"));
        app.context_inspector_open = true;
        app.context_inspector_transition = None;
        app.window_width = 1600.0;

        let open = custom_widget_ids(&mut app.view());
        assert_eq!(
            open.iter()
                .filter(|id| id.as_str() == "application-update")
                .count(),
            1,
            "the open inspector and the workspace header must not both mount the update icon: {open:?}"
        );
        let alerts = position(&open, "chrome.notices").expect("alerts are shown");
        let update = position(&open, "application-update").expect("update is shown");
        let toggle = position(&open, "command.toggle-context-inspector").expect("sidebar toggle");
        assert!(
            alerts < update && update < toggle,
            "update belongs on the right of the alerts and left of the sidebar toggle: {open:?}"
        );

        app.context_inspector_open = false;
        let closed = custom_widget_ids(&mut app.view());
        assert_eq!(
            closed
                .iter()
                .filter(|id| id.as_str() == "application-update")
                .count(),
            1,
            "closing the inspector leaves exactly one update icon: {closed:?}"
        );
        let alerts =
            position(&closed, "chrome.notices").expect("alerts stay on the workspace header");
        let update = position(&closed, "application-update").expect("update follows the alerts");
        assert!(
            alerts < update,
            "with the inspector closed the update icon is still to the right of the alerts: {closed:?}"
        );
    }

    #[test]
    fn live_session_confirmation_is_modal_and_cancel_never_starts_an_update() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.update_ui.confirmation = Some(crate::update::UpdateConfirmation {
            nonce: uuid::Uuid::new_v4().to_string(),
            preview: crate::update::impact::Preview {
                candidate: crate::update::impact::Candidate {
                    version: "9.8.7".into(),
                    protocol: 32,
                    digest: "verified".into(),
                },
                replaces_daemon: false,
                daemon: None,
            },
            expires_at_unix_ms: u64::MAX,
        });
        assert!(app.modal_is_open());
        assert!(!app.update_ui.applying);
        let confirmation = app.update_ui.confirmation.clone();
        drop(app.update_update_ui(Action::ConfirmApply("stale-preview".into())));
        assert!(app.update_ui.confirmation.is_some());
        drop(app.update_update_ui(Action::CancelApply));
        assert!(app.update_ui.confirmation.is_none());
        assert!(!app.modal_is_open());
        assert!(!app.update_ui.applying);
        // Old status files may retain a preview after helper death. A terminal
        // phase is never an actionable confirmation, even for that old shape.
        drop(app.update_update_ui(Action::Polled(
            None,
            Ok(Some(UpdateJob {
                id: uuid::Uuid::new_v4().to_string(),
                version: "9.8.7".into(),
                phase: UpdatePhase::Failed,
                message: "Helper exited".into(),
                log_path: "/nonexistent/update.log".into(),
                confirmation,
            })),
        )));
        assert!(app.update_ui.confirmation.is_none());
        assert!(!app.modal_is_open());
        assert_eq!(app.update_ui.error.as_deref(), Some("Helper exited"));
    }

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

        drop(app.update_update_ui(Action::Applied(Err("helper unavailable".into()))));
        assert!(
            !app.update_ui.restarting,
            "a helper that never closed this window must not block later attempts"
        );
    }
}
