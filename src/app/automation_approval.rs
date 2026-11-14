//! A hosted proposal never authorizes itself. The window presents the saved
//! configuration and answers with its exact immutable request revision.
use super::*;
use crate::automations::{Proposal, ProposalState, Request, Response};
use crate::control::{ControlClient, ControlError};

#[derive(Default)]
pub(super) struct State {
    proposal: Option<Proposal>,
    available: Option<Proposal>,
    reading: bool,
    /// The daemon connection every read of pending requests reuses. A focused
    /// window reads every two seconds, and a connection apiece had the daemon
    /// accept, authenticate and serve a new one each time.
    reader: Arc<std::sync::Mutex<Option<ControlClient>>>,
    /// Reads in a row that found nothing to review for `read_repository`,
    /// which space the next read out; see [`State::read_interval`].
    idle_reads: u32,
    read_repository: Option<PathBuf>,
    deciding: bool,
    generation: u64,
    focus: usize,
    error: Option<String>,
    restore_keyboard_nav: Option<bool>,
}

/// How often a focused window reads pending requests while one could
/// appear at any moment: one is offered, an agent is working, or the window
/// has not yet read the project it shows.
const READ_INTERVAL: Duration = Duration::from_secs(2);
/// The longest a focused window waits between reads while none could. Only
/// an agent makes a request, so this bounds how late one made by an agent
/// this window doesn't show is offered.
const IDLE_READ_INTERVAL: Duration = Duration::from_secs(16);

impl State {
    /// [`READ_INTERVAL`], doubling with each read that found nothing up to
    /// [`IDLE_READ_INTERVAL`].
    fn idle_read_interval(&self) -> Duration {
        READ_INTERVAL
            .saturating_mul(1 << self.idle_reads.min(8))
            .min(IDLE_READ_INTERVAL)
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Tick,
    Open {
        id: uuid::Uuid,
        revision: u64,
    },
    Read {
        generation: u64,
        result: Arc<Result<Vec<Proposal>, String>>,
    },
    Dismiss {
        id: uuid::Uuid,
        revision: u64,
        generation: u64,
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

/// The pending request for `repository` a window offers for review, if any.
fn pending_proposal(
    client: &mut ControlClient,
    repository: &Path,
) -> Result<Vec<Proposal>, ControlError> {
    let unexpected = |actual| ControlError::UnexpectedResponse {
        expected: "automation approval listing",
        actual,
    };
    let Response::Proposals(proposals) = client.automation(
        crate::agent_execution::Authority::default(),
        Request::PendingProposals {
            repository: repository.to_path_buf(),
            after: None,
            limit: 1,
        },
    )?
    else {
        return Err(unexpected("another automation response"));
    };
    let Some(visible) = proposals.first() else {
        return Ok(Vec::new());
    };
    let Response::Proposal(proposal) = client.automation(
        crate::agent_execution::Authority::default(),
        Request::Proposal { id: visible.id },
    )?
    else {
        return Err(unexpected("another automation response"));
    };
    Ok(vec![*proposal])
}

/// Runs `read` on the connection `slot` holds, connecting first when it holds
/// none. A connection the daemon answered on is kept, refusals included; one
/// that failed is dropped. When a kept connection turns out to have closed,
/// as when the daemon restarted, one new connection reads at once instead; a
/// daemon too slow to answer is not asked again until the next read.
fn with_connection<C, T>(
    slot: &mut Option<C>,
    connect: impl FnOnce() -> Result<C, String>,
    read: impl Fn(&mut C) -> Result<T, ControlError>,
) -> Result<T, String> {
    if let Some(client) = slot.as_mut() {
        match read(client) {
            Ok(value) => return Ok(value),
            Err(error @ ControlError::Remote { .. }) => return Err(error.to_string()),
            Err(error) if !closed(&error) => {
                *slot = None;
                return Err(error.to_string());
            }
            Err(_) => *slot = None,
        }
    }
    let client = slot.insert(connect()?);
    read(client).map_err(|error| {
        if !matches!(error, ControlError::Remote { .. }) {
            *slot = None;
        }
        error.to_string()
    })
}

/// Whether `error` says the daemon closed the connection: it stopped, or
/// another daemon took over and told it to go first.
fn closed(error: &ControlError) -> bool {
    match error {
        ControlError::ConnectionClosed
        | ControlError::ConnectionRejected { .. }
        | ControlError::UnexpectedMessage { .. } => true,
        ControlError::Frame(crate::terminal_ipc::FrameError::Io(error)) => matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
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
    fn approval_repository(&self) -> Option<&Path> {
        match self.active_workspace_destination()? {
            WorkspaceDestination::Folder(id) => self
                .sidebar_model
                .project(id)
                .map(|project| project.repository.as_path()),
            WorkspaceDestination::Worktree(id) => self
                .sidebar_model
                .projects
                .iter()
                .find(|project| project.worktrees.iter().any(|worktree| worktree.id == id))
                .map(|project| project.repository.as_path()),
            WorkspaceDestination::Channel(id) => self
                .durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
                .and_then(|channel| self.sidebar_model.project(channel.project_id))
                .map(|project| project.repository.as_path()),
        }
    }

    pub(super) fn automation_approval_banner(&self) -> Element<'_, Message> {
        let Some(proposal) = self
            .automation_approval
            .available
            .as_ref()
            .filter(|proposal| {
                self.approval_repository()
                    == Some(proposal.definition.agent.target.repository.as_path())
            })
        else {
            return iced::widget::Space::new().height(Length::Shrink).into();
        };
        widget::container(
            widget::row::with_children(vec![
                design::body(format!(
                    "Scheduled work requested: {}",
                    proposal.definition.name
                ))
                .width(Length::Fill)
                .into(),
                crate::ui_controls::button::secondary("Review request")
                    .on_press_maybe(
                        (!self.modal_is_open()).then_some(Message::AutomationApproval(
                            Action::Open {
                                id: proposal.id,
                                revision: proposal.revision,
                            },
                        )),
                    )
                    .into(),
            ])
            .spacing(space::M)
            .align_y(Alignment::Center),
        )
        .padding(space::M)
        .width(Length::Fill)
        .into()
    }

    pub(super) fn automation_approval_subscription(&self) -> Subscription<Message> {
        if self.refresh_ui.focused && !self.sidebar_model.projects.is_empty() {
            cosmic::iced::time::every(self.automation_approval_read_interval())
                .map(|_| Message::AutomationApproval(Action::Tick))
        } else {
            Subscription::none()
        }
    }

    /// How long until the next read of pending requests. An agent asking
    /// for approval waits in its turn for the answer, so while any agent
    /// works, reads keep the quick pace that offers its request at once.
    fn automation_approval_read_interval(&self) -> Duration {
        let state = &self.automation_approval;
        if state.available.is_some()
            || self.sidebar_state.any_agent_working()
            || self.approval_repository() != state.read_repository.as_deref()
        {
            READ_INTERVAL
        } else {
            state.idle_read_interval()
        }
    }

    /// Something that could have made a request happened: an agent's turn
    /// ended, or the window was focused again. Reads go back to their quick
    /// pace until they find nothing again.
    pub(super) fn automation_approval_activity(&mut self) {
        self.automation_approval.idle_reads = 0;
    }

    pub(super) fn adopt_automation_approval_keyboard_nav(&mut self, restore: bool) -> bool {
        if !self.automation_approval_open() {
            return false;
        }
        self.automation_approval.restore_keyboard_nav = Some(restore);
        true
    }

    pub(super) fn cancel_automation_approval(&mut self) -> Task<Message> {
        let Some(proposal) = self.automation_approval.proposal.as_ref() else {
            return Task::none();
        };
        let action = Action::Dismiss {
            id: proposal.id,
            revision: proposal.revision,
            generation: self.automation_approval.generation,
        };
        self.update_automation_approval(action)
    }

    pub(super) fn automation_approval_open(&self) -> bool {
        self.automation_approval.proposal.is_some()
    }

    fn approval_action_is_current(&self, id: uuid::Uuid, revision: u64, generation: u64) -> bool {
        !self.update_prepared
            && self.update_ui.confirmation.is_none()
            && !self.automation_approval.deciding
            && self.automation_approval.generation == generation
            && self
                .automation_approval
                .proposal
                .as_ref()
                .is_some_and(|proposal| proposal.id == id && proposal.revision == revision)
    }

    fn close_automation_approval(&mut self) -> Task<Message> {
        let state = &mut self.automation_approval;
        state.proposal = None;
        state.generation = state.generation.wrapping_add(1);
        if let Some(restore) = state.restore_keyboard_nav.take() {
            self.core.set_keyboard_nav(restore);
        }
        if self.update_prepared || self.update_ui.confirmation.is_some() {
            return Task::none();
        }
        iced::widget::operation::focus(
            self.workspace
                .focused()
                .map(|card| self.card_keyboard_focus(card))
                .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID)),
        )
        .map(cosmic::Action::App)
    }

    pub(super) fn update_automation_approval(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Tick => {
                if self.modal_is_open()
                    || self.automation_approval.reading
                    || self.chrome_ui.workspace_preview_since.is_some()
                {
                    return Task::none();
                }
                let Some(repository) = self.approval_repository().map(Path::to_path_buf) else {
                    return Task::none();
                };
                if self.automation_approval.read_repository.as_ref() != Some(&repository) {
                    self.automation_approval.idle_reads = 0;
                    self.automation_approval.read_repository = Some(repository.clone());
                }
                self.automation_approval.reading = true;
                self.automation_approval.generation =
                    self.automation_approval.generation.wrapping_add(1);
                let generation = self.automation_approval.generation;
                let reader = Arc::clone(&self.automation_approval.reader);
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        // A read that panicked may have left a reply unread.
                        let mut reader = reader.lock().unwrap_or_else(|poisoned| {
                            let mut reader = poisoned.into_inner();
                            *reader = None;
                            reader
                        });
                        with_connection(
                            &mut reader,
                            || {
                                let socket = crate::terminal::ensure_daemon_ready()
                                    .map_err(|error| error.to_string())?;
                                ControlClient::connect(&socket).map_err(|error| error.to_string())
                            },
                            |client| pending_proposal(client, &repository),
                        )
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                    Message::AutomationApproval(Action::Read {
                        generation,
                        result: Arc::new(result),
                    })
                })
            }
            Action::Read { generation, result } => {
                if generation != self.automation_approval.generation {
                    return Task::none();
                }
                self.automation_approval.reading = false;
                // Nothing to review, or no daemon to ask: read less often.
                // An offered request keeps the quick pace so its review
                // follows another window resolving it.
                let offered = result.as_ref().as_ref().is_ok_and(|proposals| {
                    proposals
                        .iter()
                        .any(|proposal| proposal.state == ProposalState::Pending)
                });
                self.automation_approval.idle_reads = if offered {
                    0
                } else {
                    self.automation_approval.idle_reads.saturating_add(1)
                };
                if self.modal_is_open()
                    || !self.refresh_ui.focused
                    || self.chrome_ui.workspace_preview_since.is_some()
                {
                    return Task::none();
                }
                let Ok(proposals) = result.as_ref() else {
                    return Task::none();
                };
                self.automation_approval.available = proposals
                    .iter()
                    .find(|proposal| {
                        proposal.state == ProposalState::Pending
                            && self.approval_repository()
                                == Some(proposal.definition.agent.target.repository.as_path())
                    })
                    .cloned();
                Task::none()
            }
            Action::Open { id, revision } => {
                if self.update_prepared
                    || self.modal_is_open()
                    || self.chrome_ui.workspace_preview_since.is_some()
                {
                    return Task::none();
                }
                let Some(proposal) = self
                    .automation_approval
                    .available
                    .as_ref()
                    .filter(|proposal| {
                        proposal.id == id
                            && proposal.revision == revision
                            && self.approval_repository()
                                == Some(proposal.definition.agent.target.repository.as_path())
                    })
                    .cloned()
                else {
                    return Task::none();
                };
                self.automation_approval.generation =
                    self.automation_approval.generation.wrapping_add(1);
                self.automation_approval.reading = false;
                self.automation_approval.proposal = Some(proposal);
                self.automation_approval.focus = 0;
                self.automation_approval.error = None;
                self.automation_approval.restore_keyboard_nav = Some(self.core.keyboard_nav());
                self.core.set_keyboard_nav(false);
                approval_focus(self.automation_approval.generation, 0)
            }
            Action::Dismiss {
                id,
                revision,
                generation,
            } => {
                if !self.approval_action_is_current(id, revision, generation) {
                    return Task::none();
                }
                // Dismissal is local navigation. The immutable pending proposal
                // stays available for another review; no daemon decision runs.
                self.automation_approval.available = self.automation_approval.proposal.clone();
                self.automation_approval.error = None;
                self.close_automation_approval()
            }
            Action::Decide {
                id,
                revision,
                generation,
                approved,
            } => {
                if !self.approval_action_is_current(id, revision, generation) {
                    return Task::none();
                }
                let state = &mut self.automation_approval;
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
                        state.available = None;
                        self.close_automation_approval()
                    }
                    Err(error) => {
                        state.error = Some(error.to_string());
                        state.focus = 0;
                        approval_focus(state.generation, 0)
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
        if self.update_ui.confirmation.is_some() {
            return None;
        }
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return Some(Task::none());
        };
        if !self.approval_action_is_current(
            proposal.id,
            proposal.revision,
            self.automation_approval.generation,
        ) {
            return Some(Task::none());
        }
        let key = match key {
            keyboard::Key::Named(key) => key,
            _ => return Some(Task::none()),
        };
        use keyboard::key::Named;
        if matches!(key, Named::Tab | Named::ArrowLeft | Named::ArrowRight) {
            let backwards = *key == Named::ArrowLeft || (*key == Named::Tab && modifiers.shift());
            self.automation_approval.focus =
                (self.automation_approval.focus + if backwards { 2 } else { 1 }) % 3;
            return Some(approval_focus(
                self.automation_approval.generation,
                self.automation_approval.focus,
            ));
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
        let decision = |approved| {
            Message::AutomationApproval(Action::Decide {
                id: proposal.id,
                revision: proposal.revision,
                generation: state.generation,
                approved,
            })
        };
        let dismiss = Message::AutomationApproval(Action::Dismiss {
            id: proposal.id,
            revision: proposal.revision,
            generation: state.generation,
        });
        let controls = dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Not now")
                .id(approval_control_id(state.generation, "dismiss"))
                .on_press_maybe((!state.deciding).then(|| dismiss.clone())),
            widget::row::with_children(vec![
                crate::ui_controls::button::secondary("Decline")
                    .id(approval_control_id(state.generation, "decline"))
                    .on_press_maybe((!state.deciding).then(|| decision(false)))
                    .into(),
                crate::ui_controls::button::primary("Approve and enable")
                    .id(approval_control_id(state.generation, "approve"))
                    .on_press_maybe((!state.deciding).then(|| decision(true)))
                    .into(),
            ])
            .spacing(space::S),
        );
        let surface = widget::column::with_children(vec![
            widget::scrollable(content)
                .height(Length::Fixed(440.0))
                .into(),
            controls,
        ])
        .spacing(space::M);
        let overlay = dialog_ui::overlay(
            dialog_ui::surface(surface, 680.0),
            (!state.deciding).then_some(dismiss),
            1.0,
        );
        // Reset unnamed pointer state for each review. Named button state is
        // restored outside keyed diff, so those IDs also carry this generation.
        Some(
            iced::widget::keyed::Column::with_children([(state.generation, overlay)])
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
        )
    }
}

fn approval_control_id(generation: u64, role: &str) -> widget::Id {
    widget::Id::new(format!("automation-approval.{generation}.{role}"))
}

fn approval_focus(generation: u64, index: usize) -> Task<Message> {
    iced::widget::operation::focus(approval_control_id(
        generation,
        match index {
            0 => "dismiss",
            1 => "decline",
            _ => "approve",
        },
    ))
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

    /// A focused window reads pending requests every two seconds on one
    /// connection, and reaches a restarted daemon on the read that finds the
    /// old connection closed.
    #[test]
    fn approval_reads_reuse_one_connection_and_replace_one_that_closed() {
        struct Connection(u32);
        let refused = || ControlError::Remote {
            code: crate::terminal_ipc::ProtocolErrorCode::TerminalUnavailable,
            message: "refused".into(),
            retryable: false,
        };
        let never = || -> Result<Connection, String> { panic!("no new connection") };
        let mut slot = None;
        let mut connects = 0;
        let mut connect = |id| {
            connects += 1;
            Ok(Connection(id))
        };
        let read = |connection: &mut Connection| Ok::<_, ControlError>(connection.0);

        assert_eq!(with_connection(&mut slot, || connect(1), read), Ok(1));
        for _ in 0..3 {
            assert_eq!(with_connection(&mut slot, never, read), Ok(1));
        }
        // The daemon answered with a refusal, so its connection still serves.
        let refusal = with_connection(&mut slot, never, |_: &mut Connection| {
            Err::<u32, _>(refused())
        });
        assert!(refusal.unwrap_err().contains("refused"));
        assert!(matches!(slot, Some(Connection(1))));

        // The daemon restarted: the kept connection is closed, and a new one
        // reads in the same turn.
        let closed_unless_new = |connection: &mut Connection| match connection.0 {
            1 => Err(ControlError::ConnectionClosed),
            id => Ok(id),
        };
        assert_eq!(
            with_connection(&mut slot, || connect(2), closed_unless_new),
            Ok(2)
        );
        assert_eq!(with_connection(&mut slot, never, read), Ok(2));

        // A daemon too slow to answer is not asked again on another
        // connection; the next read connects afresh.
        let slow = |_: &mut Connection| {
            Err::<u32, _>(ControlError::ReplyTimeout {
                timeout: Duration::from_secs(5),
                error: crate::terminal_ipc::FrameError::Io(std::io::ErrorKind::TimedOut.into()),
            })
        };
        assert!(with_connection(&mut slot, never, slow).is_err());
        assert!(slot.is_none());

        // A new connection that fails is not tried again until the next read.
        slot = None;
        let closed = |_: &mut Connection| Err::<u32, _>(ControlError::ConnectionClosed);
        assert!(with_connection(&mut slot, || connect(3), closed).is_err());
        assert!(slot.is_none());
        assert!(
            with_connection(&mut slot, || Err::<Connection, _>("no daemon".into()), read).is_err()
        );
        assert!(slot.is_none());
        assert_eq!(connects, 3);
    }

    /// A focused window with nothing to review reads less and less often,
    /// and quickly again while a request could appear: one is offered, an
    /// agent is working, another project opened or anything could have made
    /// one.
    #[test]
    fn idle_approval_reads_back_off_until_something_could_have_changed() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        for repository in ["/fixture", "/second"] {
            app.sidebar_model.projects.push(
                SidebarProject::new(
                    ProjectId::new(),
                    RepositoryId::new(),
                    repository.into(),
                    repository.into(),
                    Vec::new(),
                    Vec::new(),
                )
                .with_kind(ProjectKind::Folder),
            );
        }
        app.active_workspace = Some("/fixture".into());
        app.refresh_ui.focused = true;
        let interval = |app: &Cagents| app.automation_approval_read_interval().as_secs();
        let read = |app: &mut Cagents, result: Result<Vec<Proposal>, String>| {
            drop(app.update_automation_approval(Action::Tick));
            let generation = app.automation_approval.generation;
            drop(app.update_automation_approval(Action::Read {
                generation,
                result: Arc::new(result),
            }));
            interval(app)
        };
        assert_eq!(interval(&app), 2);
        let idle = (0..5)
            .map(|_| read(&mut app, Ok(Vec::new())))
            .collect::<Vec<_>>();
        assert_eq!(idle, [4, 8, 16, 16, 16]);
        assert_eq!(read(&mut app, Err("no daemon".into())), 16);

        // An offered request keeps reads quick until it is gone.
        assert_eq!(read(&mut app, Ok(vec![proposal()])), 2);
        assert_eq!(read(&mut app, Ok(Vec::new())), 4);
        assert_eq!(read(&mut app, Ok(Vec::new())), 8);

        // An agent asking for approval waits in its turn for the answer.
        let agent = CardId::new();
        let activity = |state| crate::agent::Activity {
            provider: crate::agent::Provider::Claude,
            state,
        };
        app.sidebar_state
            .set_card_activity(agent, activity(crate::agent::ActivityState::Working));
        assert_eq!(interval(&app), 2);
        assert_eq!(read(&mut app, Ok(Vec::new())), 2);
        app.sidebar_state
            .set_card_activity(agent, activity(crate::agent::ActivityState::Idle));
        assert_eq!(interval(&app), 16);

        // An agent's turn ended: the window's notifications changed.
        drop(
            app.update_notifications(super::super::notification_ui::Action::Snapshot(
                crate::notifications::Snapshot::default(),
            )),
        );
        assert_eq!(interval(&app), 2);
        assert_eq!(read(&mut app, Ok(Vec::new())), 4);
        assert_eq!(read(&mut app, Ok(Vec::new())), 8);

        // Another project's requests are read at once at the quick pace.
        app.active_workspace = Some("/second".into());
        assert_eq!(interval(&app), 2);
        assert_eq!(read(&mut app, Ok(Vec::new())), 4);
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
        drop(app.update_automation_approval(Action::Read {
            generation: 0,
            result: Arc::new(Ok(vec![unrelated])),
        }));
        assert!(app.automation_approval.available.is_none());
        let task = app.update_automation_approval(Action::Read {
            generation: 0,
            result: Arc::new(Ok(vec![proposal.clone()])),
        });
        assert!(
            iced::runtime::task::into_stream(task).is_none(),
            "background attention must not move keyboard focus"
        );
        assert!(!app.modal_is_open());
        assert!(app.core.keyboard_nav());
        assert_eq!(app.automation_approval.available, Some(proposal.clone()));
        drop(app.update_automation_approval(Action::Open {
            id: uuid::Uuid::new_v4(),
            revision: 1,
        }));
        assert!(!app.automation_approval_open());
        let task = app.update_automation_approval(Action::Open {
            id: proposal.id,
            revision: proposal.revision,
        });
        assert!(app.modal_is_open());
        assert!(!app.core.keyboard_nav());
        assert_eq!(
            super::super::tests::focused_element(app.automation_approval_dialog().unwrap(), task),
            Some(approval_control_id(
                app.automation_approval.generation,
                "dismiss"
            ))
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
        drop(app.update_automation_approval(Action::Read {
            generation: 0,
            result: Arc::new(Ok(vec![proposal])),
        }));
        assert!(!app.automation_approval_open());
        assert!(app.automation_approval.available.is_none());
    }

    fn reviewing() -> (Cagents, Proposal) {
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
        app.automation_approval.available = Some(proposal.clone());
        drop(app.update(Message::AutomationApproval(Action::Open {
            id: proposal.id,
            revision: proposal.revision,
        })));
        assert!(app.automation_approval_open());
        (app, proposal)
    }

    fn dialog_frame(
        element: &mut Element<'_, Message>,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
    ) -> (
        cosmic::iced::advanced::layout::Node,
        HashMap<String, cosmic::iced::Rectangle>,
    ) {
        use cosmic::iced::advanced::{
            Layout, layout,
            widget::{Operation, operation::Focusable},
        };
        struct Bounds(HashMap<String, cosmic::iced::Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: cosmic::iced::Rectangle,
                _: &mut dyn Focusable,
            ) {
                if let Some(id) = id {
                    self.0.insert(id.to_string(), bounds);
                }
            }
        }
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(
                cosmic::iced::Size::ZERO,
                cosmic::iced::Size::new(1000.0, 900.0),
            ),
        );
        let mut bounds = Bounds(HashMap::new());
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), renderer, &mut bounds);
        (node, bounds.0)
    }

    fn dialog_pointer_event(
        element: &mut Element<'_, Message>,
        tree: &mut cosmic::iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        node: &cosmic::iced::advanced::layout::Node,
        point: cosmic::iced::Point,
        event: cosmic::iced::mouse::Event,
    ) -> Vec<Message> {
        use cosmic::iced::{
            Event, Rectangle, Size,
            advanced::{Layout, Shell, clipboard},
            mouse,
        };
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            tree,
            &Event::Mouse(event),
            Layout::new(node),
            mouse::Cursor::Available(point),
            renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(1000.0, 900.0)),
        );
        messages
    }

    // Deliver pointer events to the production dialog and its actual widgets.
    fn dialog_click(app: &Cagents, control: Option<&str>) -> Vec<Message> {
        use cosmic::iced::{Point, advanced::widget::Tree, mouse};
        let renderer = super::super::tests::headless_renderer();
        let mut element = app.automation_approval_dialog().unwrap();
        let mut tree = Tree::new(element.as_widget());
        let (node, bounds) = dialog_frame(&mut element, &mut tree, &renderer);
        let point = control
            .map(|role| {
                bounds[&approval_control_id(app.automation_approval.generation, role).to_string()]
                    .center()
            })
            .unwrap_or(Point::new(4.0, 4.0));
        let mut messages = dialog_pointer_event(
            &mut element,
            &mut tree,
            &renderer,
            &node,
            point,
            mouse::Event::ButtonPressed(mouse::Button::Left),
        );
        messages.extend(dialog_pointer_event(
            &mut element,
            &mut tree,
            &renderer,
            &node,
            point,
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ));
        messages
    }

    #[test]
    fn approval_native_press_does_not_transfer_to_a_reopened_review() {
        use cosmic::iced::{
            advanced::widget::{Tree, tree},
            mouse,
        };
        for role in ["dismiss", "decline", "approve"] {
            let (mut app, proposal) = reviewing();
            let old = app.automation_approval.generation;
            let renderer = super::super::tests::headless_renderer();
            let mut element = app.automation_approval_dialog().unwrap();
            let mut state = Tree::new(element.as_widget());
            let (node, bounds) = dialog_frame(&mut element, &mut state, &renderer);
            let point = bounds[&approval_control_id(old, role).to_string()].center();
            assert!(
                dialog_pointer_event(
                    &mut element,
                    &mut state,
                    &renderer,
                    &node,
                    point,
                    mouse::Event::ButtonPressed(mouse::Button::Left)
                )
                .is_empty()
            );
            drop(element);
            // No empty widget frame runs between the old press and new review.
            drop(app.update(Message::KeyEvent {
                event: key(keyboard::key::Named::Escape),
                captured: false,
            }));
            assert!(!app.automation_approval_open());
            assert_eq!(app.automation_approval.available, Some(proposal.clone()));
            drop(app.update(Message::AutomationApproval(Action::Open {
                id: proposal.id,
                revision: proposal.revision,
            })));
            let current = app.automation_approval.generation;
            assert_ne!(old, current);
            let mut element = app.automation_approval_dialog().unwrap();
            // The pinned iced runtime lifts named widget state before keyed diff
            // and restores it afterward. A key alone cannot isolate these IDs.
            tree::NAMED.with(|named| *named.borrow_mut() = state.take_all_named());
            state.diff(element.as_widget_mut());
            iced::runtime::user_interface::restore_cached_named_states(&mut state);
            tree::NAMED.with(|named| named.borrow_mut().clear());
            let (node, bounds) = dialog_frame(&mut element, &mut state, &renderer);
            let current_point = bounds[&approval_control_id(current, role).to_string()].center();
            assert_eq!(point, current_point);
            assert!(
                dialog_pointer_event(
                    &mut element,
                    &mut state,
                    &renderer,
                    &node,
                    current_point,
                    mouse::Event::ButtonReleased(mouse::Button::Left)
                )
                .is_empty()
            );
            assert!(
                dialog_pointer_event(
                    &mut element,
                    &mut state,
                    &renderer,
                    &node,
                    current_point,
                    mouse::Event::ButtonPressed(mouse::Button::Left)
                )
                .is_empty()
            );
            let messages = dialog_pointer_event(
                &mut element,
                &mut state,
                &renderer,
                &node,
                current_point,
                mouse::Event::ButtonReleased(mouse::Button::Left),
            );
            assert_eq!(messages.len(), 1, "the current control still works");
            assert!(match &messages[0] {
                Message::AutomationApproval(Action::Dismiss {
                    id,
                    revision,
                    generation,
                }) if role == "dismiss" =>
                    *id == proposal.id && *revision == proposal.revision && *generation == current,
                Message::AutomationApproval(Action::Decide {
                    id,
                    revision,
                    generation,
                    approved,
                }) if role != "dismiss" =>
                    *id == proposal.id
                        && *revision == proposal.revision
                        && *generation == current
                        && *approved == (role == "approve"),
                _ => false,
            });
            drop(element);
            for message in messages {
                drop(app.update(message));
            }
            assert_eq!(app.automation_approval.deciding, role != "dismiss");
        }
    }

    #[test]
    fn approval_escape_and_default_enter_keep_the_proposal_pending_and_reopenable() {
        for named in [keyboard::key::Named::Escape, keyboard::key::Named::Enter] {
            let (mut app, proposal) = reviewing();
            assert_eq!(
                super::super::tests::focused_element(
                    app.automation_approval_dialog().unwrap(),
                    approval_focus(app.automation_approval.generation, 0)
                ),
                Some(approval_control_id(
                    app.automation_approval.generation,
                    "dismiss"
                ))
            );
            drop(app.update(Message::KeyEvent {
                event: key(named),
                captured: false,
            }));
            assert!(!app.automation_approval_open());
            assert!(!app.automation_approval.deciding);
            assert_eq!(app.automation_approval.available, Some(proposal.clone()));
            assert!(app.core.keyboard_nav());
            drop(app.update(Message::AutomationApproval(Action::Open {
                id: proposal.id,
                revision: proposal.revision,
            })));
            assert_eq!(app.automation_approval.proposal, Some(proposal));
            assert_eq!(app.automation_approval.focus, 0);
        }
    }

    #[test]
    fn approval_backdrop_and_not_now_emit_only_exact_neutral_dismissal() {
        for control in [None, Some("dismiss")] {
            let (mut app, proposal) = reviewing();
            let generation = app.automation_approval.generation;
            let messages = dialog_click(&app, control);
            assert_eq!(messages.len(), 1);
            assert!(
                matches!(&messages[0], Message::AutomationApproval(Action::Dismiss { id, revision, generation: shown })
                if *id==proposal.id && *revision==proposal.revision && *shown==generation)
            );
            for message in messages {
                drop(app.update(message));
            }
            assert!(!app.automation_approval_open());
            assert!(!app.automation_approval.deciding);
            assert_eq!(app.automation_approval.available, Some(proposal));
        }
    }

    #[test]
    fn approval_explicit_buttons_and_keyboard_decisions_keep_the_exact_review() {
        for (index, control, approved) in [(1, "decline", false), (2, "approve", true)] {
            for use_keyboard in [false, true] {
                let (mut app, proposal) = reviewing();
                let generation = app.automation_approval.generation;
                let messages = dialog_click(&app, Some(control));
                assert_eq!(messages.len(), 1);
                assert!(
                    matches!(&messages[0], Message::AutomationApproval(Action::Decide { id, revision, generation: shown, approved: choice })
                if *id==proposal.id && *revision==proposal.revision && *shown==generation && *choice==approved)
                );
                if use_keyboard {
                    for _ in 0..index {
                        drop(app.update(Message::KeyEvent {
                            event: key(keyboard::key::Named::Tab),
                            captured: false,
                        }));
                    }
                    assert_eq!(app.automation_approval.focus, index);
                    drop(app.update(Message::KeyEvent {
                        event: key(keyboard::key::Named::Enter),
                        captured: false,
                    }));
                } else {
                    for message in messages {
                        drop(app.update(message));
                    }
                }
                assert!(app.automation_approval.deciding);
                drop(app.update(Message::CancelDialog));
                assert!(
                    app.automation_approval_open(),
                    "an in-flight decision owns its receipt"
                );
                let mut receipt = proposal;
                receipt.state = if approved {
                    ProposalState::Approved {
                        automation: uuid::Uuid::new_v4(),
                        execution_revision: 1,
                    }
                } else {
                    ProposalState::Declined
                };
                drop(app.update(Message::AutomationApproval(Action::Finished {
                    generation,
                    result: Arc::new(Ok(receipt)),
                })));
                assert!(!app.automation_approval_open());
                assert!(app.automation_approval.available.is_none());
            }
        }
    }

    #[test]
    fn approval_reopened_review_rejects_old_actions_and_prepared_user_input() {
        let (mut app, proposal) = reviewing();
        let old = app.automation_approval.generation;
        drop(app.update(Message::CancelDialog));
        assert_eq!(app.automation_approval.available, Some(proposal.clone()));
        drop(app.update(Message::AutomationApproval(Action::Open {
            id: proposal.id,
            revision: proposal.revision,
        })));
        let current = app.automation_approval.generation;
        assert_ne!(old, current);
        for generation in [old, current] {
            app.update_prepared = generation == current;
            for action in [
                Action::Dismiss {
                    id: proposal.id,
                    revision: proposal.revision,
                    generation,
                },
                Action::Decide {
                    id: proposal.id,
                    revision: proposal.revision,
                    generation,
                    approved: false,
                },
            ] {
                drop(app.update(Message::AutomationApproval(action)));
                assert_eq!(app.automation_approval.proposal, Some(proposal.clone()));
                assert!(!app.automation_approval.deciding);
            }
        }
        app.update_prepared = false;
        for (id, revision) in [
            (uuid::Uuid::new_v4(), proposal.revision),
            (proposal.id, proposal.revision + 1),
        ] {
            drop(app.update(Message::AutomationApproval(Action::Decide {
                id,
                revision,
                generation: current,
                approved: true,
            })));
            assert!(!app.automation_approval.deciding);
        }
        drop(app.update(Message::AutomationApproval(Action::Decide {
            id: proposal.id,
            revision: proposal.revision,
            generation: current,
            approved: true,
        })));
        assert!(app.automation_approval.deciding);
        let mut stale = proposal.clone();
        stale.state = ProposalState::Declined;
        drop(app.update(Message::AutomationApproval(Action::Finished {
            generation: old,
            result: Arc::new(Ok(stale)),
        })));
        assert!(
            app.automation_approval.deciding,
            "an old completion cannot retire the new decision"
        );
        assert_eq!(app.automation_approval.proposal, Some(proposal));
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
            Some(approval_control_id(
                app.automation_approval.generation,
                "dismiss"
            ))
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
}
