use super::*;
use crate::automation_ui::{Action, Effect, TargetChoice};

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
                        channel: None,
                    },
                });
            }
        }
        self.automation_ui
            .set_choices(targets, self.automation_account_choices());
    }

    pub(super) fn open_automations(&mut self) -> Task<Message> {
        self.refresh_automation_choices();
        let open = self.open_utility_card(CardKind::Automations);
        let read = self.update_automation_ui(Action::Refresh);
        Task::batch([open, read])
    }

    pub(super) fn update_automation_ui(&mut self, action: Action) -> Task<Message> {
        if matches!(action, Action::New | Action::Edit) {
            self.refresh_automation_choices();
        }
        match self.automation_ui.update(action) {
            None => Task::none(),
            Some(Effect::Request { sequence, request }) => cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let socket = crate::terminal::ensure_daemon_ready()
                        .map_err(|error| error.to_string())?;
                    let mut client = crate::control::ControlClient::connect(&socket)
                        .map_err(|error| error.to_string())?;
                    client
                        .automation(crate::agent_execution::Authority::default(), request)
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                Message::AutomationUi(Action::Received {
                    sequence,
                    result: Arc::new(result),
                })
            }),
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
                let Some(channel) = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == agent.channel)
                else {
                    self.status = Some(TransientNotice::error(
                        "The agent's project and channel are not loaded in this window. Open the project, then try again.",
                    ));
                    return Task::none();
                };
                if self.sidebar_model.project(channel.project_id).is_none() {
                    return Task::none();
                }
                let adopted = self
                    .adopt_execution_agent(&agent)
                    .unwrap_or_else(Task::none);
                let selected = self.select_channel(agent.channel);
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
