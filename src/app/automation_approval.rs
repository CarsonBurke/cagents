//! A hosted proposal never authorizes itself. The window presents the saved
//! configuration and answers with its exact immutable request revision.
use super::*;
use crate::automations::{Proposal, ProposalState, ProposalSummary, Request, Response};

#[derive(Default)]
pub(super) struct State {
    proposal: Option<Proposal>,
    available: Option<Proposal>,
    pending: Vec<ProposalSummary>,
    total: u64,
    pushed: Option<(uuid::Uuid, u64, u64)>,
    watch_epoch: u64,
    page_generation: u64,
    page_offset: u64,
    page_loading: bool,
    review_refresh: Option<(u64, Option<(uuid::Uuid, u64, u64)>)>,
    reading: Option<(uuid::Uuid, u64)>,
    deciding: bool,
    generation: u64,
    focus: usize,
    error: Option<String>,
    restore_keyboard_nav: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum Action {
    /// Close the overlay without answering the pending request.
    Dismiss {
        id: uuid::Uuid,
        revision: u64,
        generation: u64,
    },
    Updated(Vec<ProposalSummary>),
    Page { generation: u64, after: Option<uuid::Uuid>, offset: u64 },
    PageLoaded { generation: u64, after: Option<uuid::Uuid>, offset: u64, result: Arc<Result<Vec<ProposalSummary>, String>> },
    ReviewRefreshed { generation: u64, pushed: Option<(uuid::Uuid, u64, u64)>, result: Arc<Result<Proposal, String>> },
    Loaded {
        id: uuid::Uuid,
        revision: u64,
        result: Arc<Result<Proposal, String>>,
    },
    Open {
        id: uuid::Uuid,
        revision: u64,
    },
    Decide {
        id: uuid::Uuid,
        revision: u64,
        generation: u64,
        approved: bool,
    },
    Finished {
        generation: u64,
        result: Arc<Result<Proposal, String>>,
    },
}

fn placement_description(proposal: &Proposal) -> String {
    match &proposal.definition.agent.target.placement {
        Some(crate::agent_execution::Placement::Worktree { path, .. }) => {
            format!("Worktree {}", path.display())
        }
        Some(crate::agent_execution::Placement::Channel(id)) => format!(
            "Channel {} ({id})",
            proposal
                .channel_title
                .as_deref()
                .unwrap_or("Selected channel")
        ),
        None => "New channel named after this automation".into(),
    }
}

impl Cagents {
    pub(super) fn automation_approval_banner(&self) -> Element<'_, Message> {
        let pending = &self.automation_approval.pending;
        if pending.is_empty() && self.automation_approval.total == 0 {
            return iced::widget::Space::new().height(Length::Shrink).into();
        }
        let rows = pending
            .iter()
            .map(|proposal| {
                widget::row::with_children(vec![
                    design::body(format!(
                        "Scheduled work requested: {} · {}",
                        proposal.name,
                        proposal.repository.display()
                    ))
                    .width(Length::Fill)
                    .into(),
                    crate::ui_controls::button::secondary("Review request")
                        .on_press_maybe((!self.modal_is_open()).then_some(
                            Message::AutomationApproval(Action::Open {
                                id: proposal.id,
                                revision: proposal.revision,
                            }),
                        ))
                        .into(),
                ])
                .spacing(space::M)
                .align_y(Alignment::Center)
                .into()
            })
            .collect();
        widget::container(
            widget::column::with_children(vec![
                design::heading(format!(
                    "{} pending scheduled-work request(s)",
                    self.automation_approval.total.max(pending.len() as u64)
                ))
                .into(),
                widget::container(
                    widget::scrollable(widget::column::with_children(rows).spacing(space::S))
                        .height(Length::Shrink),
                )
                .max_height(160)
                .into(),
                widget::row::with_children(vec![
                    design::body(format!("Showing {}–{}", if pending.is_empty() { 0 } else { self.automation_approval.page_offset + 1 }, self.automation_approval.page_offset.saturating_add(pending.len() as u64))).into(),
                    crate::ui_controls::button::secondary("First requests")
                        .on_press_maybe((!self.modal_is_open() && !self.automation_approval.page_loading && self.automation_approval.total > 0).then_some(Message::AutomationApproval(Action::Page { generation: self.automation_approval.page_generation, after: None, offset: 0 }))).into(),
                    crate::ui_controls::button::secondary("Next requests")
                        .on_press_maybe((!self.modal_is_open() && !self.automation_approval.page_loading && pending.len() == crate::automations::PROPOSAL_PAGE_SIZE && self.automation_approval.page_offset.saturating_add(pending.len() as u64) < self.automation_approval.total).then_some(Message::AutomationApproval(Action::Page { generation: self.automation_approval.page_generation, after: pending.last().map(|proposal| proposal.id), offset: self.automation_approval.page_offset.saturating_add(pending.len() as u64) }))).into(),
                ]).spacing(space::S).align_y(Alignment::Center).into(),
            ])
            .spacing(space::S),
        )
        .padding(space::M)
        .width(Length::Fill)
        .into()
    }

    pub(super) fn automation_proposals_reconnected(&mut self) {
        let state = &mut self.automation_approval;
        state.watch_epoch = state.watch_epoch.wrapping_add(1);
        state.pushed = None;
        state.review_refresh = None;
        state.page_generation = state.page_generation.wrapping_add(1);
        state.page_loading = false;
    }

    pub(super) fn sync_automation_proposal_page(&mut self, identity: uuid::Uuid, sequence: u64, total: u64, pending: Vec<ProposalSummary>) -> Task<Message> {
        let key = (identity, sequence, self.automation_approval.watch_epoch);
        if self.automation_approval.pushed == Some(key) { return Task::none(); }
        self.automation_approval.pushed = Some(key);
        self.automation_approval.total = total;
        self.automation_approval.page_generation = self.automation_approval.page_generation.wrapping_add(1);
        self.automation_approval.page_loading = false;
        self.automation_approval.page_offset = 0;
        self.automation_approval.review_refresh = None;
        self.update_automation_approval(Action::Updated(pending.into_iter().take(crate::automations::PROPOSAL_PAGE_SIZE).collect()))
    }

    fn refresh_automation_approval_review(&mut self) -> Task<Message> {
        let Some(proposal) = self.automation_approval.proposal.as_ref() else { return Task::none(); };
        let key = (self.automation_approval.generation, self.automation_approval.pushed);
        if self.automation_approval.review_refresh == Some(key) { return Task::none(); }
        let id = proposal.id;
        self.automation_approval.review_refresh = Some(key);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
                let mut client = crate::control::ControlClient::connect(&socket).map_err(|error| error.to_string())?;
                match client.automation(crate::agent_execution::Authority::default(), Request::Proposal { id }).map_err(|error| error.to_string())? {
                    Response::Proposal(proposal) => Ok(*proposal),
                    _ => Err("No exact request receipt was returned".into()),
                }
            }).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::AutomationApproval(Action::ReviewRefreshed { generation: key.0, pushed: key.1, result: Arc::new(result) })
        })
    }

    pub(super) fn automation_approval_subscription(&self) -> Subscription<Message> {
        // The existing model watch delivers creation, resolution and reconnect
        // snapshots. An idle window needs no timer or repeated IPC handshakes.
        Subscription::none()
    }

    pub(super) fn adopt_automation_approval_keyboard_nav(&mut self, restore: bool) -> bool {
        if !self.automation_approval_open() {
            return false;
        }
        self.automation_approval.restore_keyboard_nav = Some(restore);
        true
    }

    pub(super) fn cancel_automation_approval(&mut self) -> Task<Message> {
        let Some(action) = self.automation_approval_dismiss_action() else {
            return Task::none();
        };
        self.update_automation_approval(action)
    }

    pub(super) fn automation_approval_dismiss_action(&self) -> Option<Action> {
        let proposal = self.automation_approval.proposal.as_ref()?;
        Some(Action::Dismiss {
            id: proposal.id,
            revision: proposal.revision,
            generation: self.automation_approval.generation,
        })
    }

    pub(super) fn approval_user_action_matches(
        &self,
        id: uuid::Uuid,
        revision: u64,
        generation: u64,
    ) -> bool {
        self.automation_approval.generation == generation
            && self
                .automation_approval
                .proposal
                .as_ref()
                .is_some_and(|proposal| proposal.id == id && proposal.revision == revision)
    }

    /// Model retirement is independent of user input and may remove hidden
    /// backing state. User dismissal must first validate its captured review.
    fn retire_automation_approval(&mut self) -> Task<Message> {
        let state = &mut self.automation_approval;
        if state.deciding || state.proposal.take().is_none() {
            return Task::none();
        }
        state.error = None;
        if let Some(restore) = state.restore_keyboard_nav.take() {
            self.core.set_keyboard_nav(restore);
        }
        iced::widget::operation::focus(
            self.workspace
                .focused()
                .map(|card| self.card_keyboard_focus(card))
                .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID)),
        )
        .map(cosmic::Action::App)
    }

    pub(super) fn automation_approval_open(&self) -> bool {
        self.automation_approval.proposal.is_some()
    }

    pub(super) fn approval_dialog_generation(&self) -> Option<u64> {
        self.automation_approval
            .proposal
            .as_ref()
            .map(|_| self.automation_approval.generation)
    }

    pub(super) fn update_automation_approval(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Updated(pending) => {
                self.automation_approval.pending = pending.into_iter().take(crate::automations::PROPOSAL_PAGE_SIZE).collect();
                if self
                    .automation_approval
                    .available
                    .as_ref()
                    .is_some_and(|proposal| {
                        !self.automation_approval.pending.iter().any(|summary| {
                            summary.id == proposal.id && summary.revision == proposal.revision
                        })
                    })
                {
                    self.automation_approval.available = None;
                }
                if !self.automation_approval.deciding
                    && self
                        .automation_approval
                        .proposal
                        .as_ref()
                        .is_some_and(|proposal| {
                            !self.automation_approval.pending.iter().any(|summary| {
                                summary.id == proposal.id && summary.revision == proposal.revision
                            })
                        })
                {
                    // A bounded/queued watch packet is discovery, not a private
                    // decision receipt. An exact read also fences a packet that
                    // predates a notification-opened review.
                    return self.refresh_automation_approval_review();
                }
                Task::none()
            }
            Action::Page { generation, after, offset } => {
                if generation != self.automation_approval.page_generation || self.automation_approval.page_loading || self.modal_is_open() || self.update_prepared {
                    return Task::none();
                }
                self.automation_approval.page_generation = self.automation_approval.page_generation.wrapping_add(1);
                let generation = self.automation_approval.page_generation;
                self.automation_approval.page_loading = true;
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
                        let mut client = crate::control::ControlClient::connect(&socket).map_err(|error| error.to_string())?;
                        match client.automation(crate::agent_execution::Authority::default(), Request::AllPendingProposals { after, limit: crate::automations::PROPOSAL_PAGE_SIZE }).map_err(|error| error.to_string())? {
                            Response::Proposals(page) => Ok(page),
                            _ => Err("No pending request page was returned".into()),
                        }
                    }).await.map_err(|error| error.to_string()).and_then(|result| result);
                    Message::AutomationApproval(Action::PageLoaded { generation, after, offset, result: Arc::new(result) })
                })
            }
            Action::PageLoaded { generation, after, offset, result } => {
                if !self.automation_approval.page_loading || generation != self.automation_approval.page_generation {
                    return Task::none();
                }
                self.automation_approval.page_loading = false;
                match result.as_ref() {
                    Ok(page) if page.len() <= crate::automations::PROPOSAL_PAGE_SIZE && page.windows(2).all(|pair| pair[0].id < pair[1].id) && page.first().is_none_or(|proposal| after.is_none_or(|after| proposal.id > after)) => {
                        self.automation_approval.pending = page.clone();
                        self.automation_approval.page_offset = offset;
                    }
                    _ => self.status = Some(TransientNotice::error("Pending requests could not be loaded. Retry from the first page.")),
                }
                Task::none()
            }
            Action::ReviewRefreshed { generation, pushed, result } => {
                if self.automation_approval.review_refresh != Some((generation, pushed)) {
                    return Task::none();
                }
                self.automation_approval.review_refresh = None;
                if self.automation_approval.generation != generation || self.automation_approval.deciding {
                    return Task::none();
                }
                let Some(expected) = self.automation_approval.proposal.as_ref() else { return Task::none(); };
                match result.as_ref() {
                    Ok(receipt) => {
                        let mut expected = expected.clone();
                        expected.state = receipt.state.clone();
                        if &expected == receipt && receipt.state != ProposalState::Pending {
                            return self.retire_automation_approval();
                        }
                    }
                    Err(_) => self.status = Some(TransientNotice::warning("The request could not be refreshed; its decision remains unresolved.")),
                }
                Task::none()
            }
            Action::Loaded {
                id,
                revision,
                result,
            } => {
                if self.automation_approval.reading != Some((id, revision)) {
                    return Task::none();
                }
                self.automation_approval.reading = None;
                let proposal = match result.as_ref() {
                    Ok(proposal)
                        if proposal.id == id
                            && proposal.revision == revision
                            && proposal.state == ProposalState::Pending =>
                    {
                        proposal.clone()
                    }
                    Ok(_) => return Task::none(),
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error));
                        return Task::none();
                    }
                };
                // An explicit notification activation can precede its model
                // push. The exact user-authorized Pending reply is sufficient
                // to review it; this never approves or enables any schedule.
                if self.automation_approval.pending.len() < crate::automations::PROPOSAL_PAGE_SIZE && !self
                    .automation_approval
                    .pending
                    .iter()
                    .any(|summary| summary.id == id)
                {
                    self.automation_approval
                        .pending
                        .push(ProposalSummary::from(&proposal));
                }
                self.automation_approval.available = Some(proposal);
                self.update_automation_approval(Action::Open { id, revision })
            }
            Action::Dismiss {
                id,
                revision,
                generation,
            } => {
                if !self.approval_user_action_matches(id, revision, generation) {
                    return Task::none();
                }
                self.retire_automation_approval()
            }
            Action::Open { id, revision } => {
                if self.modal_is_open() || self.chrome_ui.workspace_preview_since.is_some() {
                    return Task::none();
                }
                let proposal = self
                    .automation_approval
                    .available
                    .as_ref()
                    .filter(|proposal| proposal.id == id && proposal.revision == revision)
                    .cloned();
                let Some(proposal) = proposal else {
                    if self.automation_approval.reading.is_some() {
                        return Task::none();
                    }
                    self.automation_approval.reading = Some((id, revision));
                    return cosmic::task::future(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let socket = crate::terminal::ensure_daemon_ready()
                                .map_err(|error| error.to_string())?;
                            let mut client = crate::control::ControlClient::connect(&socket)
                                .map_err(|error| error.to_string())?;
                            match client
                                .automation(
                                    crate::agent_execution::Authority::default(),
                                    Request::Proposal { id },
                                )
                                .map_err(|error| error.to_string())?
                            {
                                Response::Proposal(proposal) => Ok(*proposal),
                                _ => Err("Unexpected automation proposal response".to_owned()),
                            }
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()));
                        Message::AutomationApproval(Action::Loaded {
                            id,
                            revision,
                            result: Arc::new(result),
                        })
                    });
                };
                self.automation_approval.generation =
                    self.automation_approval.generation.wrapping_add(1);
                self.automation_approval.reading = None;
                self.automation_approval.proposal = Some(proposal);
                self.automation_approval.focus = 0;
                self.automation_approval.error = None;
                self.automation_approval.restore_keyboard_nav = Some(self.core.keyboard_nav());
                self.core.set_keyboard_nav(false);
                approval_focus(0)
            }
            Action::Decide {
                id,
                revision,
                generation,
                approved,
            } => {
                if !self.approval_user_action_matches(id, revision, generation) {
                    return Task::none();
                }
                let state = &mut self.automation_approval;
                if state.deciding {
                    return Task::none();
                }
                state.deciding = true;
                state.error = None;
                let generation = state.generation;
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let socket = crate::terminal::ensure_daemon_ready()
                            .map_err(|error| error.to_string())?;
                        let mut client = crate::control::ControlClient::connect(&socket)
                            .map_err(|error| error.to_string())?;
                        match client
                            .automation(
                                crate::agent_execution::Authority::default(),
                                Request::ResolveProposal {
                                    id,
                                    revision,
                                    approved,
                                },
                            )
                            .map_err(|error| error.to_string())?
                        {
                            Response::Proposal(proposal) => Ok(*proposal),
                            _ => Err("Unexpected automation approval result".into()),
                        }
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                    Message::AutomationApproval(Action::Finished {
                        generation,
                        result: Arc::new(result),
                    })
                })
            }
            Action::Finished { generation, result } => {
                let state = &mut self.automation_approval;
                if generation != state.generation || !state.deciding {
                    return Task::none();
                }
                state.deciding = false;
                let verified = result
                    .as_ref()
                    .as_ref()
                    .map_err(|error| error.to_string())
                    .and_then(|receipt| {
                        let mut expected = state
                            .proposal
                            .clone()
                            .ok_or_else(|| "No automation proposal is being reviewed".to_owned())?;
                        expected.state = receipt.state.clone();
                        if receipt.state == ProposalState::Pending || &expected != receipt {
                            Err(
                                "The daemon did not return the exact resolved approval receipt"
                                    .to_owned(),
                            )
                        } else {
                            Ok(receipt)
                        }
                    });
                match verified {
                    Ok(receipt) => {
                        self.status = Some(match receipt.state {
                            ProposalState::Approved { .. } => TransientNotice::success(format!(
                                "Enabled automation {}",
                                receipt.definition.name
                            )),
                            _ => TransientNotice::warning(
                                "Automation request declined; no schedule was created",
                            ),
                        });
                        state.proposal = None;
                        state.available = None;
                        state.pending.retain(|pending| pending.id != receipt.id);
                        if let Some(restore) = state.restore_keyboard_nav.take() {
                            self.core.set_keyboard_nav(restore);
                        }
                        iced::widget::operation::focus(
                            self.workspace
                                .focused()
                                .map(|card| self.card_keyboard_focus(card))
                                .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID)),
                        )
                        .map(cosmic::Action::App)
                    }
                    Err(error) => {
                        state.error = Some(error.to_string());
                        state.focus = 0;
                        approval_focus(0)
                    }
                }
            }
        }
    }

    pub(super) fn automation_approval_key(
        &mut self,
        event: &keyboard::Event,
        captured: bool,
    ) -> Option<Task<Message>> {
        let proposal = self.automation_approval.proposal.as_ref()?;
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return Some(Task::none());
        };
        if self.automation_approval.deciding {
            return Some(Task::none());
        }
        let key = match key {
            keyboard::Key::Named(key) => key,
            _ => return Some(Task::none()),
        };
        use keyboard::key::Named;
        if matches!(key, Named::Tab | Named::ArrowLeft | Named::ArrowRight) {
            let backwards = modifiers.shift() || *key == Named::ArrowLeft;
            self.automation_approval.focus =
                (self.automation_approval.focus + if backwards { 2 } else { 1 }) % 3;
            return Some(approval_focus(self.automation_approval.focus));
        }
        if *key == Named::Escape
            || (*key == Named::Enter
                && !captured
                && !modifiers.control()
                && !modifiers.alt()
                && !modifiers.logo())
        {
            let action = if *key == Named::Escape || self.automation_approval.focus == 0 {
                Action::Dismiss {
                    id: proposal.id,
                    revision: proposal.revision,
                    generation: self.automation_approval.generation,
                }
            } else {
                Action::Decide {
                    id: proposal.id,
                    revision: proposal.revision,
                    generation: self.automation_approval.generation,
                    approved: self.automation_approval.focus == 2,
                }
            };
            return Some(self.update_automation_approval(action));
        }
        Some(Task::none())
    }

    pub(super) fn automation_approval_dialog(&self) -> Option<Element<'_, Message>> {
        let state = &self.automation_approval;
        let proposal = state.proposal.as_ref()?;
        let definition = &proposal.definition;
        let account = self
            .automation_account_choices()
            .into_iter()
            .find(|account| Some(&account.id) == definition.agent.account.as_ref())
            .map(|account| account.label)
            .unwrap_or_else(|| "Saved account unavailable".into());
        let description = format!(
            "{}\nProvider: {}\nAccount: {} ({})\nProject: {}\nDirectory: {}\nView: {}\nModel: {}\nReasoning effort: {}\nPermissions: {}\nSurface: {:?}\nSession: {:?}\nSchedule: {} ({})\nExecution revision: {}",
            definition.name,
            definition.agent.provider.label(),
            account,
            definition
                .agent
                .account
                .as_ref()
                .map(|id| id.to_string())
                .unwrap_or_default(),
            definition.agent.target.repository.display(),
            definition.agent.target.directory.display(),
            placement_description(proposal),
            definition
                .agent
                .model
                .as_deref()
                .unwrap_or("Provider default"),
            definition
                .agent
                .reasoning_effort
                .as_deref()
                .unwrap_or("Provider default"),
            definition
                .agent
                .permission_mode
                .as_deref()
                .unwrap_or("Provider default"),
            definition.agent.surface,
            definition.session_mode,
            definition.schedule.pattern.expression().unwrap_or_default(),
            definition.schedule.timezone,
            proposal.revision
        );
        let mut content = widget::column::with_children(vec![design::title("Approve scheduled work?").into(),
            notice_body("A hosted conversation requests recurring work. Approval enables only this saved configuration, including its unattended tools and account."),
            notice_body(description), notice_body(&definition.prompt)]).spacing(space::M);
        if let Some(precheck) = &definition.precheck {
            content = content.push(notice_body(format!(
                "Shell precheck ({} seconds):\n{}",
                precheck.timeout_seconds, precheck.command
            )));
        }
        if let Some(error) = &state.error {
            content = content.push(notice_detail(error));
        }
        let dismiss = Action::Dismiss {
            id: proposal.id,
            revision: proposal.revision,
            generation: state.generation,
        };
        let decision = |approved| {
            Message::AutomationApproval(Action::Decide {
                id: proposal.id,
                revision: proposal.revision,
                generation: state.generation,
                approved,
            })
        };
        let controls = dialog_ui::confirmation_controls(vec![
            crate::ui_controls::button::secondary("Not now")
                .id(widget::Id::new("automation-approval.dismiss"))
                .on_press_maybe(
                    (!state.deciding).then_some(Message::AutomationApproval(dismiss.clone())),
                )
                .into(),
            crate::ui_controls::button::secondary("Decline")
                .id(widget::Id::new("automation-approval.decline"))
                .on_press_maybe((!state.deciding).then(|| decision(false)))
                .into(),
            crate::ui_controls::button::primary("Approve and enable")
                .id(widget::Id::new("automation-approval.approve"))
                .on_press_maybe((!state.deciding).then(|| decision(true)))
                .into(),
        ]);
        let surface = widget::column::with_children(vec![
            widget::scrollable(content)
                .height(Length::Fixed(440.0))
                .into(),
            controls.into(),
        ])
        .spacing(space::M);
        Some(dialog_ui::overlay(
            dialog_ui::surface(surface, 680.0),
            (!state.deciding).then_some(Message::AutomationApproval(dismiss)),
            1.0,
        ))
    }
}

pub(super) fn default_focus_id() -> widget::Id {
    widget::Id::new("automation-approval.dismiss")
}

fn approval_focus(index: usize) -> Task<Message> {
    iced::widget::operation::focus(widget::Id::new(match index {
        0 => "automation-approval.dismiss",
        1 => "automation-approval.decline",
        _ => "automation-approval.approve",
    }))
    .map(cosmic::Action::App)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Origin, Spec, Surface, Target},
        automations::{Definition, DirectoryIdentity, Schedule, SchedulePattern, SessionMode},
    };

    fn proposal() -> Proposal {
        let definition = Definition {
            name: "Weekday recap".into(),
            prompt: "Review only the approved project".into(),
            agent: Spec {
                provider: Provider::Codex,
                account: Some("codex/system".parse().unwrap()),
                target: Target {
                    repository: "/fixture".into(),
                    directory: "/fixture".into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: Some("offline".into()),
                reasoning_effort: Some("low".into()),
                permission_mode: None,
            },
            schedule: Schedule {
                timezone: "America/Vancouver".into(),
                pattern: SchedulePattern::Weekdays {
                    hour: 9,
                    minute: 30,
                },
            },
            session_mode: SessionMode::Fresh,
            visibility: crate::automations::Visibility::default(),
            catch_up: crate::automations::CatchUp::default(),
            precheck: None,
        };
        Proposal {
            id: uuid::Uuid::new_v4(),
            revision: 1,
            owner: Origin::User,
            requested: definition.clone(),
            definition,
            target_identity: DirectoryIdentity {
                device: 1,
                inode: 2,
            },
            channel_title: None,
            created_ms: 1,
            state: ProposalState::Pending,
        }
    }

    #[test]
    fn approval_describes_worktree_existing_channel_and_fresh_channel_destinations() {
        use crate::agent_execution::Placement;
        let mut proposal = proposal();
        assert_eq!(
            placement_description(&proposal),
            "New channel named after this automation"
        );
        let id = crate::model::ChannelId::new();
        proposal.definition.agent.target.placement = Some(Placement::Channel(id));
        proposal.channel_title = Some("Review queue".into());
        assert_eq!(
            placement_description(&proposal),
            format!("Channel Review queue ({id})")
        );
        proposal.definition.agent.target.placement = Some(Placement::Worktree {
            repository: "/fixture".into(),
            path: "/fixture/feature branch".into(),
        });
        // A previously saved channel title cannot relabel a worktree destination.
        assert_eq!(
            placement_description(&proposal),
            "Worktree /fixture/feature branch"
        );
    }

    fn key(named: keyboard::key::Named) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(named.clone()),
            modified_key: keyboard::Key::Named(named),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }
    }

    #[test]
    fn pending_requests_require_explicit_review_and_do_not_interrupt_another_project() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model.projects.push(
            SidebarProject::new(
                ProjectId::new(),
                RepositoryId::new(),
                "Fixture".into(),
                "/fixture".into(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        app.active_workspace = Some("/fixture".into());
        app.refresh_ui.focused = true;
        app.core.set_keyboard_nav(true);
        let proposal = proposal();
        let mut unrelated = proposal.clone();
        unrelated.definition.agent.target.repository = "/other-project".into();
        drop(
            app.update_automation_approval(Action::Updated(vec![ProposalSummary::from(
                &unrelated,
            )])),
        );
        assert!(app.automation_approval.available.is_none());
        assert_eq!(
            app.automation_approval.pending.len(),
            1,
            "another project's request updates global attention"
        );
        let task =
            app.update_automation_approval(Action::Updated(vec![ProposalSummary::from(&proposal)]));
        assert!(
            iced::runtime::task::into_stream(task).is_none(),
            "background attention must not move keyboard focus"
        );
        assert!(!app.modal_is_open());
        assert!(app.core.keyboard_nav());
        assert!(
            app.automation_approval.available.is_none(),
            "model pushes carry bounded summaries; full consent is fetched only for explicit review"
        );
        let absent = uuid::Uuid::new_v4();
        drop(app.update_automation_approval(Action::Open {
            id: absent,
            revision: 1,
        }));
        assert!(!app.automation_approval_open());
        drop(app.update_automation_approval(Action::Loaded {
            id: absent,
            revision: 1,
            result: Arc::new(Err("The request no longer exists".into())),
        }));
        drop(app.update_automation_approval(Action::Open {
            id: proposal.id,
            revision: proposal.revision,
        }));
        assert!(
            !app.modal_is_open(),
            "the exact private proposal must arrive before displaying consent"
        );
        let task = app.update_automation_approval(Action::Loaded {
            id: proposal.id,
            revision: proposal.revision,
            result: Arc::new(Ok(proposal.clone())),
        });
        assert!(app.modal_is_open());
        assert!(!app.core.keyboard_nav());
        assert_eq!(
            super::super::tests::focused_element(app.automation_approval_dialog().unwrap(), task),
            Some(widget::Id::new("automation-approval.dismiss"))
        );
        app.automation_approval.deciding = true;
        let mut declined = proposal.clone();
        declined.state = ProposalState::Declined;
        drop(app.update_automation_approval(Action::Finished {
            generation: 1,
            result: Arc::new(Ok(declined)),
        }));
        assert!(!app.automation_approval_open());
        assert!(app.core.keyboard_nav());
        drop(app.update_automation_approval(Action::Loaded {
            id: proposal.id,
            revision: proposal.revision,
            result: Arc::new(Ok(proposal)),
        }));
        assert!(!app.automation_approval_open());
        assert!(app.automation_approval.available.is_none());
    }

    #[test]
    fn explicit_notification_review_can_arrive_before_the_proposal_model_push() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let proposal = proposal();
        assert!(app.automation_approval.pending.is_empty());
        drop(app.update_automation_approval(Action::Open {
            id: proposal.id,
            revision: proposal.revision,
        }));
        assert!(!app.automation_approval_open());
        drop(app.update_automation_approval(Action::Loaded {
            id: proposal.id,
            revision: proposal.revision,
            result: Arc::new(Ok(proposal.clone())),
        }));
        assert!(app.automation_approval_open());
        assert_eq!(
            app.automation_approval.pending,
            vec![ProposalSummary::from(&proposal)]
        );
        assert!(!app.automation_approval.deciding);
        drop(app.cancel_automation_approval());
        assert!(!app.automation_approval_open());
        assert_eq!(app.automation_approval.pending.len(), 1);
        // An old/unsolicited load cannot reopen a dismissed dialog.
        drop(app.update_automation_approval(Action::Loaded {
            id: proposal.id,
            revision: proposal.revision,
            result: Arc::new(Ok(proposal)),
        }));
        assert!(!app.automation_approval_open());
    }

    #[test]
    fn escape_backdrop_and_default_enter_leave_the_proposal_pending() {
        for event in [
            Some(keyboard::key::Named::Escape),
            Some(keyboard::key::Named::Enter),
            None,
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            let proposal = proposal();
            app.automation_approval.available = Some(proposal.clone());
            app.automation_approval.pending = vec![ProposalSummary::from(&proposal)];
            app.automation_approval.proposal = Some(proposal.clone());
            assert_eq!(
                super::super::tests::focused_element(
                    app.automation_approval_dialog().unwrap(),
                    approval_focus(0)
                ),
                Some(widget::Id::new("automation-approval.dismiss"))
            );
            if let Some(event) = event {
                drop(app.automation_approval_key(&key(event), false));
            } else {
                // The backdrop dispatches this same neutral action.
                drop(app.cancel_automation_approval());
            }
            assert!(!app.automation_approval_open());
            assert!(!app.automation_approval.deciding);
            assert_eq!(app.automation_approval.available, Some(proposal));
        }
    }

    #[test]
    fn explicit_decline_and_approve_use_the_saved_revision() {
        for focus in [1, 2] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            app.automation_approval.proposal = Some(proposal());
            app.automation_approval.focus = focus;
            drop(app.automation_approval_key(&key(keyboard::key::Named::Enter), false));
            assert!(app.automation_approval.deciding);
            assert!(app.automation_approval_open());
        }
    }

    #[test]
    fn approval_failure_returns_focus_to_not_now_and_leaves_the_request_unresolved() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.automation_approval.proposal = Some(proposal());
        app.automation_approval.deciding = true;
        app.automation_approval.focus = 1;
        let task = app.update_automation_approval(Action::Finished {
            generation: 0,
            result: Arc::new(Err("The saved account was removed".into())),
        });
        assert!(app.automation_approval_open());
        assert!(!app.automation_approval.deciding);
        assert_eq!(
            super::super::tests::focused_element(app.automation_approval_dialog().unwrap(), task),
            Some(widget::Id::new("automation-approval.dismiss"))
        );
        let unresolved = app.automation_approval.proposal.clone().unwrap();
        app.automation_approval.deciding = true;
        drop(app.update_automation_approval(Action::Finished {
            generation: 0,
            result: Arc::new(Ok(unresolved.clone())),
        }));
        assert!(
            app.automation_approval_open(),
            "a pending reply proves no decision"
        );
        let mut wrong = unresolved;
        wrong.id = uuid::Uuid::new_v4();
        wrong.state = ProposalState::Declined;
        app.automation_approval.deciding = true;
        drop(app.update_automation_approval(Action::Finished {
            generation: 0,
            result: Arc::new(Ok(wrong)),
        }));
        assert!(
            app.automation_approval_open(),
            "another proposal's receipt cannot dismiss this review"
        );
    }

    #[test]
    fn queued_user_actions_cannot_control_a_reopened_review_of_the_same_proposal() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let proposal = proposal();
        app.automation_approval.available = Some(proposal.clone());
        app.automation_approval.pending = vec![ProposalSummary::from(&proposal)];
        drop(app.update_automation_approval(Action::Open {
            id: proposal.id,
            revision: proposal.revision,
        }));
        let old_dismiss = app.automation_approval_dismiss_action().unwrap();
        let old_generation = app.approval_dialog_generation().unwrap();
        drop(app.cancel_automation_approval());
        drop(app.update_automation_approval(Action::Open {
            id: proposal.id,
            revision: proposal.revision,
        }));
        let current_generation = app.approval_dialog_generation().unwrap();
        assert_ne!(old_generation, current_generation);
        drop(app.update_automation_approval(old_dismiss));
        assert!(app.automation_approval_open());
        drop(app.update_automation_approval(Action::Decide {
            id: proposal.id,
            revision: proposal.revision,
            generation: old_generation,
            approved: true,
        }));
        assert!(!app.automation_approval.deciding);
        assert_eq!(app.automation_approval.pending.len(), 1);
        drop(app.update_automation_approval(Action::Decide {
            id: proposal.id,
            revision: proposal.revision,
            generation: current_generation,
            approved: true,
        }));
        assert!(app.automation_approval.deciding);
    }

    #[test]
    fn model_resolution_retires_backing_state_independently_of_user_actions() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let proposal = proposal();
        app.automation_approval.proposal = Some(proposal.clone());
        app.automation_approval.available = Some(proposal.clone());
        app.automation_approval.pending = vec![ProposalSummary::from(&proposal)];
        app.automation_approval.generation = 17;
        drop(app.update_automation_approval(Action::Updated(Vec::new())));
        assert!(app.automation_approval_open());
        let mut resolved = proposal.clone();
        resolved.state = ProposalState::Declined;
        drop(app.update_automation_approval(Action::ReviewRefreshed {
            generation: app.automation_approval.generation,
            pushed: app.automation_approval.pushed,
            result: Arc::new(Ok(resolved)),
        }));
        assert!(!app.automation_approval_open());
        assert!(app.automation_approval.available.is_none());
        assert!(!app.automation_approval.deciding);
    }

    fn request_page(proposal: &Proposal, first: u128, last: u128) -> Vec<ProposalSummary> {
        (first..=last).map(|index| { let mut summary = ProposalSummary::from(proposal); summary.id = uuid::Uuid::from_u128(index); summary }).collect()
    }

    #[test]
    fn global_proposal_pages_bound_summaries_and_keep_late_requests_reachable() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let identity = uuid::Uuid::new_v4();
        let proposal = proposal();
        drop(app.sync_automation_proposal_page(identity, 1, 10_000, request_page(&proposal, 1, 10_000)));
        assert_eq!(app.automation_approval.total, 10_000);
        assert_eq!(app.automation_approval.pending.len(), 64);
        let generation = app.automation_approval.page_generation;
        drop(app.update_automation_approval(Action::Page { generation, after: Some(uuid::Uuid::from_u128(64)), offset: 64 }));
        let request = app.automation_approval.page_generation;
        drop(app.update_automation_approval(Action::PageLoaded { generation: request, after: Some(uuid::Uuid::from_u128(64)), offset: 64, result: Arc::new(Ok(request_page(&proposal,65,128))) }));
        assert_eq!(app.automation_approval.page_offset,64);
        assert_eq!(app.automation_approval.pending[0].id,uuid::Uuid::from_u128(65));
        assert_eq!(app.automation_approval.pending.last().unwrap().id,uuid::Uuid::from_u128(128));
        // An unrelated execution/model edit must not reset explicit navigation.
        drop(app.sync_automation_proposal_page(identity,1,10_000,request_page(&proposal,1,64)));
        assert_eq!(app.automation_approval.page_offset,64);
        // A new consent edge resets the page and fences its old completion.
        drop(app.sync_automation_proposal_page(identity,2,9_999,request_page(&proposal,1,64)));
        drop(app.update_automation_approval(Action::PageLoaded { generation: request, after: Some(uuid::Uuid::from_u128(64)), offset:64, result:Arc::new(Ok(request_page(&proposal,65,128))) }));
        assert_eq!(app.automation_approval.page_offset,0);
        assert_eq!(app.automation_approval.pending[0].id,uuid::Uuid::from_u128(1));
        assert_eq!(app.automation_approval.total,9_999);
    }

    #[test]
    fn a_bounded_push_never_treats_an_omitted_review_as_a_decision() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut proposal = proposal();
        proposal.id = uuid::Uuid::from_u128(999);
        app.automation_approval.available = Some(proposal.clone());
        drop(app.update_automation_approval(Action::Open { id:proposal.id, revision:proposal.revision }));
        let generation = app.automation_approval.generation;
        let identity = uuid::Uuid::new_v4();
        drop(app.sync_automation_proposal_page(identity,1,1_000,request_page(&proposal,1,64)));
        assert!(app.automation_approval_open());
        assert_eq!(app.automation_approval.proposal.as_ref().unwrap().state,ProposalState::Pending);
        assert!(app.automation_approval.review_refresh.is_some());
        let pushed = app.automation_approval.pushed;
        drop(app.update_automation_approval(Action::ReviewRefreshed { generation, pushed, result:Arc::new(Ok(proposal.clone())) }));
        assert!(app.automation_approval_open());
        drop(app.sync_automation_proposal_page(identity,2,999,request_page(&proposal,1,64)));
        let pushed = app.automation_approval.pushed;
        let mut resolved = proposal.clone(); resolved.state = ProposalState::Declined;
        drop(app.update_automation_approval(Action::ReviewRefreshed { generation, pushed, result:Arc::new(Ok(resolved)) }));
        assert!(!app.automation_approval_open(),"only the exact private receipt can retire an omitted review");
        assert_eq!(app.automation_approval.total,999);
    }
    #[test]
    fn global_request_banner_builds_only_one_page_of_real_widgets() {
        use cosmic::iced::{Rectangle, Size, advanced::{Layout, layout, renderer::Headless, widget::{Operation,Tree}}};
        #[derive(Default)]
        struct Labels(Vec<String>);
        impl Operation for Labels {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) { visit(self); }
            fn text(&mut self, _:Option<&widget::Id>, _:Rectangle, text:&str) { self.0.push(text.to_owned()); }
        }
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let proposal = proposal();
        drop(app.sync_automation_proposal_page(uuid::Uuid::new_v4(),1,10_000,request_page(&proposal,1,10_000)));
        let renderer = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(cosmic::Renderer::new(cosmic::iced::Font::default(),cosmic::iced::Pixels(14.0),Some("tiny-skia"))).unwrap();
        let mut element = app.automation_approval_banner();
        let mut tree = Tree::new(element.as_widget_mut());
        let node = element.as_widget_mut().layout(&mut tree,&renderer,&layout::Limits::new(Size::ZERO,Size::new(900.0,1000.0)));
        let mut labels = Labels::default();
        element.as_widget_mut().operate(&mut tree,Layout::new(&node),&renderer,&mut labels);
        assert_eq!(labels.0.iter().filter(|text| text.starts_with("Scheduled work requested:")).count(),64);
        assert!(labels.0.iter().any(|text| text=="10000 pending scheduled-work request(s)"));
        assert!(labels.0.iter().any(|text| text=="Next requests"));
    }
    #[test]
    fn reconnect_fences_pages_and_private_review_receipts_even_at_the_same_clock() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let identity = uuid::Uuid::new_v4();
        let mut proposal = proposal();
        proposal.id = uuid::Uuid::from_u128(999);
        drop(app.sync_automation_proposal_page(identity,1,1_000,request_page(&proposal,1,64)));
        let generation = app.automation_approval.page_generation;
        drop(app.update_automation_approval(Action::Page { generation, after:Some(uuid::Uuid::from_u128(64)),offset:64 }));
        let old_page = app.automation_approval.page_generation;
        app.automation_approval.available = Some(proposal.clone());
        drop(app.update_automation_approval(Action::Open { id:proposal.id,revision:proposal.revision }));
        drop(app.sync_automation_proposal_page(identity,2,1_000,request_page(&proposal,1,64)));
        let old_push = app.automation_approval.pushed;
        let review = app.automation_approval.generation;
        assert!(app.automation_approval.review_refresh.is_some());
        app.automation_proposals_reconnected();
        drop(app.sync_automation_proposal_page(identity,2,1_000,request_page(&proposal,1,64)));
        let new_push = app.automation_approval.pushed;
        assert_ne!(old_push,new_push);
        assert!(app.automation_approval.review_refresh.is_some());
        let mut stale = proposal.clone(); stale.state=ProposalState::Declined;
        drop(app.update_automation_approval(Action::ReviewRefreshed {generation:review,pushed:old_push,result:Arc::new(Ok(stale))}));
        assert!(app.automation_approval_open(),"old daemon receipt cannot retire the current review");
        drop(app.update_automation_approval(Action::PageLoaded {generation:old_page,after:Some(uuid::Uuid::from_u128(64)),offset:64,result:Arc::new(Ok(request_page(&proposal,65,128)))}));
        assert_eq!(app.automation_approval.page_offset,0);
        assert_eq!(app.automation_approval.pending[0].id,uuid::Uuid::from_u128(1));
        drop(app.update_automation_approval(Action::ReviewRefreshed {generation:review,pushed:new_push,result:Arc::new(Ok(proposal))}));
        assert!(app.automation_approval_open());
        assert!(app.automation_approval.review_refresh.is_none());
    }

}
