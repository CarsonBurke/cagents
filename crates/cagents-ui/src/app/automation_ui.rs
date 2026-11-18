use super::*;
use crate::automation_ui::{Action, Effect, TargetChoice};

pub(super) mod transport;

impl Cagents {
    pub(super) fn automation_subscription(&self) -> Subscription<Message> {
        if self.refresh_ui.focused
            && !self.modal_is_open()
            && self
                .workspace
                .focused()
                .and_then(|id| self.card_state(id))
                .is_some_and(|card| matches!(card.kind, CardKind::Automations))
        {
            cosmic::iced::time::every(Duration::from_secs(5))
                .map(|_| Message::AutomationUi(Action::Tick))
        } else {
            Subscription::none()
        }
    }

    fn refresh_automation_choices(&mut self) {
        self.automation_ui
            .set_archived_accounts(self.automation_archived_account_choices());
        let mut targets = Vec::new();
        for project in &self.sidebar_model.projects {
            let directories: Vec<&Path> = if project.kind == crate::model::ProjectKind::Folder {
                vec![project.repository.as_path()]
            } else {
                project
                    .worktrees
                    .iter()
                    .filter(|row| row.deletion.is_none())
                    .map(|row| row.path.as_path())
                    .collect()
            };
            for directory in directories {
                targets.push(TargetChoice {
                    label: format!("{} · {}", project.name, display_path(directory)),
                    target: crate::agent_execution::Target {
                        repository: project.repository.clone(),
                        directory: directory.to_path_buf(),
                        placement: None,
                    },
                    defaults: crate::agent::Provider::ALL
                        .into_iter()
                        .map(|provider| {
                            let account = self.default_account_for_directory(provider, directory);
                            (provider, account.id())
                        })
                        .collect(),
                });
            }
        }
        self.automation_ui.set_choices(
            targets,
            self.automation_account_choices(),
            self.automation_models(),
        );
    }

    pub(super) fn open_automations(&mut self) -> Task<Message> {
        self.refresh_automation_choices();
        let open = self.open_utility_card(CardKind::Automations);
        // A retained editor owns its choices and does not need a list read.
        // Starting a foreground refresh here would drop the first keystrokes
        // after reopening the pane or returning from a window-close warning.
        let read = if self.automation_ui.has_draft() {
            Task::none()
        } else {
            self.update_automation_ui(Action::Refresh)
        };
        Task::batch([open, read])
    }

    pub(super) fn update_automation_ui(&mut self, action: Action) -> Task<Message> {
        // Restored utility cards bypass open_automations, and the account
        // catalog can finish loading after the card opens. Keep labels and
        // availability current before displaying or confirming a definition.
        // Existing drafts own their choice snapshots, so polling cannot move
        // a user's account or target selection.
        if matches!(
            action,
            Action::Tick
                | Action::Refresh
                | Action::Select(_)
                | Action::New
                | Action::Edit
                | Action::Enable
                | Action::RunNow
        ) {
            self.refresh_automation_choices();
        }
        let was_draft_dialog = self.automation_ui.draft_dialog_open();
        let was_asking = self.automation_ui.question();
        let cancels_question = matches!(action, Action::CancelConfirmation);
        let opens_run = matches!(action, Action::OpenSession);
        match self.automation_ui.update(action) {
            None if self.automation_ui.question().is_some()
                && self.automation_ui.question() != was_asking =>
            {
                let (_, question) = self.automation_ui.question().expect("asked above");
                question.focus(
                    crate::automation_ui::confirmation_cancel_id(),
                    crate::automation_ui::confirmation_confirm_id(),
                )
            }
            // A question dismissed leaves the keyboard on the control that
            // asked it, not nowhere.
            None if cancels_question && self.automation_ui.question().is_none() => match was_asking
            {
                Some((_, question)) => iced::widget::operation::focus(
                    crate::automation_ui::question_opener_id(question),
                )
                .map(cosmic::Action::App),
                None => Task::none(),
            },
            None if !was_draft_dialog && self.automation_ui.draft_dialog_open() => {
                iced::widget::operation::focus(crate::automation_ui::draft_dialog_focus_id())
                    .map(cosmic::Action::App)
            }
            None if was_draft_dialog
                && self.automation_ui.has_draft()
                && !self.automation_ui.draft_dialog_open() =>
            {
                iced::widget::operation::focus(crate::automation_ui::draft_editor_focus_id())
                    .map(cosmic::Action::App)
            }
            None => Task::none(),
            Some(Effect::ReadRun(run)) => {
                let ids = self
                    .notification_ui
                    .snapshot
                    .notices
                    .iter()
                    .filter(|notice| {
                        notice.event.target() == crate::notifications::Target::AgentRun(run)
                            && !notice.acknowledged
                    })
                    .map(|notice| notice.id)
                    .collect::<Vec<_>>();
                for id in ids {
                    self.notification_ui.acknowledge(id);
                }
                Task::none()
            }
            Some(Effect::Request { sequence, request }) => {
                let context = transport::Context::capture();
                iced::Task::future(async move {
                    tokio::task::spawn_blocking(move || {
                        let activity = super::request_failure::Activity::automation(&request);
                        let scoped = context.is_scoped();
                        let mut client = context.connect(activity)?;
                        let response = client.request(request).map_err(|error| {
                            super::request_failure::message(error, activity, scoped)
                        })?;
                        // An explicit Open must have its accepted title before
                        // adoption, even if it overtook the workspace watcher.
                        let labels = if opens_run {
                            client
                                .read_agent_labels()
                                .map_err(crate::control::ControlError::reason)?
                        } else {
                            Vec::new()
                        };
                        Ok::<_, String>((response, labels))
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
                })
                .then(move |result| {
                    let (result, labels) = match result {
                        Ok((response, labels)) => (Ok(response), labels),
                        Err(error) => (Err(error), Vec::new()),
                    };
                    let received = cosmic::task::message(Message::AutomationUi(Action::Received {
                        sequence,
                        result: Arc::new(result),
                    }));
                    if labels.is_empty() {
                        received
                    } else {
                        cosmic::task::message(Message::WorkspaceModelObserved(
                            super::workspace_model_sync::ModelUpdate::AgentLabels(labels),
                        ))
                        .chain(received)
                    }
                })
            }
            Some(Effect::OpenRun(detail)) => {
                let Some(agent) = detail.agent else {
                    return Task::none();
                };
                if agent.closed
                    || !matches!(agent.origin, crate::agent_execution::Origin::Automation { id } if id == detail.firing.automation)
                {
                    self.status = Some(TransientNotice::error(
                        "This run has no live automation agent. Its result remains in history.",
                    ));
                    return Task::none();
                }
                if self
                    .card_state(CardId::from_uuid(agent.id.as_uuid()))
                    .is_some_and(|card| !card.existence.is_open())
                {
                    self.status = Some(TransientNotice::warning(
                        "This agent's pane was closed in this window. Its result remains available in run history.",
                    ));
                    return Task::none();
                }
                let Some(placement) = self.workspace_model.agent_view_scope(&agent).cloned() else {
                    self.status = Some(TransientNotice::warning(
                        "This agent's exact pane is no longer available in a single view. Its result remains available in run history.",
                    ));
                    return Task::none();
                };
                let selected = match &placement {
                    crate::agent_execution::Placement::Channel(id) => {
                        let Some(channel) = self
                            .durable
                            .channels
                            .iter()
                            .find(|channel| channel.id == *id)
                        else {
                            self.status = Some(TransientNotice::error(
                                "The agent's project and channel are not loaded in this window. Open the project, then try again.",
                            ));
                            return Task::none();
                        };
                        if self.sidebar_model.project(channel.project_id).is_none() {
                            return Task::none();
                        }
                        self.navigate_to_channel(*id)
                    }
                    crate::agent_execution::Placement::Worktree { path, .. } => {
                        if self.project_ui.deleting_directory(path)
                            || self.worktree_view_scope(path).as_ref() != Some(&placement)
                        {
                            self.status = Some(TransientNotice::error(
                                "The agent's worktree view is not loaded in this window. Open its project, then try again.",
                            ));
                            return Task::none();
                        }
                        self.navigate_to_worktree(path.clone(), false)
                    }
                };
                let adopted = self
                    .adopt_execution_agent(&agent)
                    .unwrap_or_else(Task::none);
                Task::batch([
                    adopted,
                    selected,
                    cosmic::task::message(Message::FocusCard(CardId::from_uuid(
                        agent.id.as_uuid(),
                    ))),
                ])
            }
        }
    }
}
