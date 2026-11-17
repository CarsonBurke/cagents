//! A hosted proposal never authorizes itself. The window presents the saved
//! configuration and answers with its exact immutable request revision.
use super::automation_ui::transport;
use super::request_failure::{self, Activity};
use super::*;
use crate::automations::{Proposal, ProposalState, Request, Response};
use crate::control::ControlError;

#[derive(Default)]
pub(super) struct State {
    proposal: Option<Proposal>,
    available: Option<Proposal>,
    reading: bool,
    /// The daemon connection every read of pending requests reuses, rather
    /// than having the daemon accept and authenticate one per read.
    reader: Arc<std::sync::Mutex<Option<transport::Connection>>>,
    /// How many times the daemon has said this window's project's pending
    /// requests changed, or that it may have missed saying so.
    changes: u64,
    /// The project the read in flight is for, and `changes` when it began.
    reading_for: Option<(PathBuf, u64)>,
    /// The project the offered request was last read for, and `changes` then.
    /// A read is owed until it matches the project shown and `changes`.
    read_for: Option<(PathBuf, u64)>,
    /// The project whose reads have failed, and how many times in a row,
    /// which spaces its next attempts out; see [`State::retry_interval`].
    failed_reads: Option<(PathBuf, u32)>,
    deciding: bool,
    generation: u64,
    focus: usize,
    error: Option<String>,
    restore_keyboard_nav: Option<bool>,
}

impl State {
    /// The read of pending requests in flight, if any, for the projection
    /// check: the full projection starts one a window owes.
    #[cfg(any(test, feature = "development"))]
    pub(super) fn projection_view(&self) -> String {
        format!(
            "reading {} for {:?} (generation {})",
            self.reading, self.reading_for, self.generation
        )
    }
}

/// How soon a focused window that owes a read tries it again, while an open
/// review, the workspace switcher or a read already in flight holds it off
/// and nothing else that could end that happens.
const READ_INTERVAL: Duration = Duration::from_secs(2);
/// The longest a focused window waits between attempts while the daemon
/// cannot be reached.
const RETRY_INTERVAL: Duration = Duration::from_secs(16);

impl State {
    /// Reads of `repository` in a row that failed.
    fn failed_reads(&self, repository: Option<&Path>) -> u32 {
        match &self.failed_reads {
            Some((failed, count)) if Some(failed.as_path()) == repository => *count,
            _ => 0,
        }
    }

    /// [`READ_INTERVAL`], doubling with each read of `repository` that could
    /// not reach the daemon up to [`RETRY_INTERVAL`].
    fn retry_interval(&self, repository: Option<&Path>) -> Duration {
        READ_INTERVAL
            .saturating_mul(1 << self.failed_reads(repository).min(8))
            .min(RETRY_INTERVAL)
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
    client: &mut transport::Connection,
    repository: &Path,
) -> Result<Vec<Proposal>, ControlError> {
    let unexpected = |actual| ControlError::UnexpectedResponse {
        expected: "automation approval listing",
        actual,
    };
    let Response::Proposals(proposals) = client.request(Request::PendingProposals {
        repository: repository.to_path_buf(),
        after: None,
        limit: 1,
    })?
    else {
        return Err(unexpected("another automation response"));
    };
    let Some(visible) = proposals.first() else {
        return Ok(Vec::new());
    };
    let Response::Proposal(proposal) = client.request(Request::Proposal { id: visible.id })? else {
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
    report: impl Fn(ControlError) -> String,
) -> Result<T, String> {
    if let Some(client) = slot.as_mut() {
        match read(client) {
            Ok(value) => return Ok(value),
            Err(error @ ControlError::Remote { .. }) => return Err(report(error)),
            Err(error) if !closed(&error) => {
                *slot = None;
                return Err(report(error));
            }
            Err(_) => *slot = None,
        }
    }
    let client = slot.insert(connect()?);
    read(client).map_err(|error| {
        if !matches!(error, ControlError::Remote { .. }) {
            *slot = None;
        }
        report(error)
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

    /// A timer runs only while a focused window owes a read it could not
    /// make at once. Otherwise the daemon says when there is something new.
    pub(super) fn automation_approval_subscription(&self) -> Subscription<Message> {
        if self.refresh_ui.focused && self.automation_approval_read_owed() {
            cosmic::iced::time::every(
                self.automation_approval
                    .retry_interval(self.approval_repository()),
            )
            .map(|_| Message::AutomationApproval(Action::Tick))
        } else {
            Subscription::none()
        }
    }

    /// Whether what this window offers may be out of date: it has not read
    /// the project it shows since the daemon last said its requests changed.
    fn automation_approval_read_owed(&self) -> bool {
        let state = &self.automation_approval;
        self.approval_repository().is_some_and(|repository| {
            !state
                .read_for
                .as_ref()
                .is_some_and(|(read, changes)| read == repository && *changes == state.changes)
        })
    }

    /// The daemon said the pending requests of the project at `repository`
    /// changed, or, with none, that it may have missed saying so for any
    /// project, as when this window began watching or could not. A focused
    /// window that shows that project reads them at once, unless reads keep
    /// failing and the daemon has not been heard from since.
    pub(super) fn automation_requests_changed(
        &mut self,
        repository: Option<&Path>,
    ) -> Task<Message> {
        let state = &self.automation_approval;
        let shown = |path: &Path| {
            self.approval_repository() == Some(path)
                || state
                    .read_for
                    .as_ref()
                    .is_some_and(|(read, _)| read == path)
        };
        if repository.is_some_and(|repository| !shown(repository)) {
            // Another project's: showing that one reads it anyway.
            return Task::none();
        }
        if repository.is_some() {
            // The daemon was just heard from.
            self.automation_approval.failed_reads = None;
        }
        self.automation_approval.changes = self.automation_approval.changes.wrapping_add(1);
        self.read_owed_automation_requests()
    }

    /// Reads pending requests now if this window owes a read and can make it:
    /// after anything that could have shown another project, ended what held
    /// a read off, or made one owed. Reads that keep failing are left to the
    /// subscription's slower retries.
    pub(super) fn read_owed_automation_requests(&mut self) -> Task<Message> {
        if self.refresh_ui.focused
            && self.automation_approval_read_owed()
            && self
                .automation_approval
                .failed_reads(self.approval_repository())
                == 0
        {
            self.update_automation_approval(Action::Tick)
        } else {
            Task::none()
        }
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
                self.automation_approval.reading_for =
                    Some((repository.clone(), self.automation_approval.changes));
                self.automation_approval.reading = true;
                self.automation_approval.generation =
                    self.automation_approval.generation.wrapping_add(1);
                let generation = self.automation_approval.generation;
                let reader = Arc::clone(&self.automation_approval.reader);
                let context = transport::Context::capture();
                let scoped = context.is_scoped();
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
                            || context.connect(Activity::ViewApprovals),
                            |client| pending_proposal(client, &repository),
                            |error| {
                                request_failure::message(error, Activity::ViewApprovals, scoped)
                            },
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
                let read_for = self.automation_approval.reading_for.take();
                let Ok(proposals) = result.as_ref() else {
                    // The read stays owed, and is tried again less often.
                    if let Some((repository, _)) = read_for {
                        let failed = self
                            .automation_approval
                            .failed_reads(Some(&repository))
                            .saturating_add(1);
                        self.automation_approval.failed_reads = Some((repository, failed));
                    }
                    return Task::none();
                };
                self.automation_approval.failed_reads = None;
                // Not offered now, so still owed for when it can be.
                if self.modal_is_open()
                    || !self.refresh_ui.focused
                    || self.chrome_ui.workspace_preview_since.is_some()
                {
                    return Task::none();
                }
                self.automation_approval.read_for = read_for;
                self.automation_approval.available = proposals
                    .iter()
                    .find(|proposal| {
                        proposal.state == ProposalState::Pending
                            && self.approval_repository()
                                == Some(proposal.definition.agent.target.repository.as_path())
                    })
                    .cloned();
                // Requests changed during the read, or another project shows.
                self.read_owed_automation_requests()
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
                let context = transport::Context::capture();
                cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        match context.request(Request::ResolveProposal {
                            id,
                            revision,
                            approved,
                        })? {
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
        // The request scrolls; the decision stays in reach at any window size
        // and interface scale.
        let overlay = dialog_ui::overlay(
            dialog_ui::scrolling_surface_with_actions(content, controls, 680.0, 640.0),
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
    mod authority;

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

    /// Pending-request reads reuse one connection, and reach a restarted
    /// daemon on the read that finds the old connection closed.
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

        assert_eq!(
            with_connection(&mut slot, || connect(1), read, |error| error.to_string()),
            Ok(1)
        );
        for _ in 0..3 {
            assert_eq!(
                with_connection(&mut slot, never, read, |error| error.to_string()),
                Ok(1)
            );
        }
        // The daemon answered with a refusal, so its connection still serves.
        let refusal = with_connection(
            &mut slot,
            never,
            |_: &mut Connection| Err::<u32, _>(refused()),
            |error| error.to_string(),
        );
        assert!(refusal.unwrap_err().contains("refused"));
        assert!(matches!(slot, Some(Connection(1))));

        // The daemon restarted: the kept connection is closed, and a new one
        // reads in the same turn.
        let closed_unless_new = |connection: &mut Connection| match connection.0 {
            1 => Err(ControlError::ConnectionClosed),
            id => Ok(id),
        };
        assert_eq!(
            with_connection(
                &mut slot,
                || connect(2),
                closed_unless_new,
                |error| error.to_string()
            ),
            Ok(2)
        );
        assert_eq!(
            with_connection(&mut slot, never, read, |error| error.to_string()),
            Ok(2)
        );

        // A daemon too slow to answer is not asked again on another
        // connection; the next read connects afresh.
        let slow = |_: &mut Connection| {
            Err::<u32, _>(ControlError::ReplyTimeout {
                timeout: Duration::from_secs(5),
                error: crate::terminal_ipc::FrameError::Io(std::io::ErrorKind::TimedOut.into()),
            })
        };
        assert!(with_connection(&mut slot, never, slow, |error| error.to_string()).is_err());
        assert!(slot.is_none());

        // A new connection that fails is not tried again until the next read.
        slot = None;
        let closed = |_: &mut Connection| Err::<u32, _>(ControlError::ConnectionClosed);
        assert!(
            with_connection(&mut slot, || connect(3), closed, |error| error.to_string()).is_err()
        );
        assert!(slot.is_none());
        assert!(
            with_connection(
                &mut slot,
                || Err::<Connection, _>("no daemon".into()),
                read,
                |error| error.to_string()
            )
            .is_err()
        );
        assert!(slot.is_none());
        assert_eq!(connects, 3);
    }

    /// A window reads a project's pending requests when it shows that project
    /// and when the daemon says they changed, never on a timer that runs
    /// while nothing does. A read it could not make or apply stays owed.
    #[test]
    fn approval_reads_follow_the_daemon_instead_of_polling() {
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
        let finish = |app: &mut Cagents, result: Result<Vec<Proposal>, String>| {
            assert!(app.automation_approval.reading, "a read is in flight");
            let generation = app.automation_approval.generation;
            drop(app.update_automation_approval(Action::Read {
                generation,
                result: Arc::new(result),
            }));
        };
        let reads = |app: &mut Cagents| {
            let generation = app.automation_approval.generation;
            drop(app.read_owed_automation_requests());
            app.automation_approval.generation != generation
        };

        // Nothing read yet: the project shown is read.
        assert!(app.automation_approval_read_owed());
        assert!(reads(&mut app));
        finish(&mut app, Ok(Vec::new()));
        assert!(!app.automation_approval_read_owed());
        assert!(!reads(&mut app), "nothing changed, so nothing is read");

        // Another project's change is read only on showing that project.
        drop(app.automation_requests_changed(Some(Path::new("/second"))));
        assert!(!app.automation_approval.reading);
        assert!(!app.automation_approval_read_owed());

        // This project's change is read at once and offered.
        drop(app.automation_requests_changed(Some(Path::new("/fixture"))));
        finish(&mut app, Ok(vec![proposal()]));
        assert!(app.automation_approval.available.is_some());
        assert!(!app.automation_approval_read_owed());

        // A change during a read is read again as soon as it lands.
        drop(app.automation_requests_changed(Some(Path::new("/fixture"))));
        drop(app.automation_requests_changed(Some(Path::new("/fixture"))));
        finish(&mut app, Ok(vec![proposal()]));
        assert!(app.automation_approval_read_owed());
        finish(&mut app, Ok(Vec::new()));
        assert!(app.automation_approval.available.is_none());
        assert!(!app.automation_approval_read_owed());

        // A daemon that cannot be reached is tried less and less often, on
        // the subscription's timer rather than at every chance.
        let interval = |app: &Cagents, repository: &str| {
            app.automation_approval
                .retry_interval(Some(Path::new(repository)))
                .as_secs()
        };
        drop(app.automation_requests_changed(Some(Path::new("/fixture"))));
        let mut intervals = Vec::new();
        for _ in 0..4 {
            finish(&mut app, Err("no daemon".into()));
            assert!(app.automation_approval_read_owed());
            intervals.push(interval(&app, "/fixture"));
            assert!(!reads(&mut app), "a failing read waits for its retry");
            drop(app.automation_requests_changed(None));
            assert!(!app.automation_approval.reading, "so does watching again");
            drop(app.update_automation_approval(Action::Tick));
        }
        assert_eq!(intervals, [4, 8, 16, 16]);
        finish(&mut app, Err("no daemon".into()));

        // Another project shown is read at once, its own reads not having
        // failed, and the daemon telling of a change shows it is back.
        app.active_workspace = Some("/second".into());
        assert_eq!(interval(&app, "/second"), 2);
        assert!(reads(&mut app));
        finish(&mut app, Ok(Vec::new()));
        assert!(!app.automation_approval_read_owed());
        app.active_workspace = Some("/fixture".into());
        assert!(app.automation_approval_read_owed());
        assert!(reads(&mut app), "the earlier failure was cleared by a read");
        finish(&mut app, Err("no daemon".into()));
        drop(app.automation_requests_changed(Some(Path::new("/fixture"))));
        assert!(app.automation_approval.reading);
        finish(&mut app, Ok(Vec::new()));
        assert_eq!(interval(&app, "/fixture"), 2);
        assert!(!app.automation_approval_read_owed());

        // A read that lands while the window is away stays owed, and is
        // made on its return.
        drop(app.automation_requests_changed(Some(Path::new("/fixture"))));
        app.refresh_ui.focused = false;
        finish(&mut app, Ok(vec![proposal()]));
        assert!(app.automation_approval.available.is_none());
        assert!(!reads(&mut app), "an unfocused window waits");
        let window = cosmic::iced::window::Id::RESERVED;
        app.core.set_main_window_id(Some(window));
        drop(app.update(Message::RefreshUi(
            super::super::refresh_ui::Action::Focused(window, true),
        )));
        assert!(app.automation_approval.reading, "focus returning reads");
        finish(&mut app, Ok(vec![proposal()]));
        assert!(app.automation_approval.available.is_some());
        assert!(!app.automation_approval_read_owed());

        // Watching again after missing what the daemon said owes a read.
        drop(app.automation_requests_changed(None));
        finish(&mut app, Ok(vec![proposal()]));
        assert!(!app.automation_approval_read_owed());
    }

    /// Opening a review discards the read in flight, which stays owed and is
    /// made once the review closes.
    #[test]
    fn a_review_opened_during_a_read_reads_again_when_it_closes() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model.projects.push(
            SidebarProject::new(
                ProjectId::new(),
                RepositoryId::new(),
                "/fixture".into(),
                "/fixture".into(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        app.active_workspace = Some("/fixture".into());
        app.refresh_ui.focused = true;
        let offered = proposal();
        drop(app.read_owed_automation_requests());
        let generation = app.automation_approval.generation;
        drop(app.update_automation_approval(Action::Read {
            generation,
            result: Arc::new(Ok(vec![offered.clone()])),
        }));
        assert!(app.automation_approval.available.is_some());

        drop(app.automation_requests_changed(Some(Path::new("/fixture"))));
        let in_flight = app.automation_approval.generation;
        assert!(app.automation_approval.reading);
        drop(app.update_automation_approval(Action::Open {
            id: offered.id,
            revision: offered.revision,
        }));
        assert!(app.automation_approval_open());
        drop(app.update_automation_approval(Action::Read {
            generation: in_flight,
            result: Arc::new(Ok(Vec::new())),
        }));
        assert!(
            app.automation_approval_read_owed(),
            "the late read is set aside"
        );
        drop(app.read_owed_automation_requests());
        assert!(
            !app.automation_approval.reading,
            "not while the review is open"
        );

        let generation = app.automation_approval.generation;
        drop(app.update(Message::AutomationApproval(Action::Dismiss {
            id: offered.id,
            revision: offered.revision,
            generation,
        })));
        assert!(!app.automation_approval_open());
        assert!(app.automation_approval.reading, "closing the review reads");
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
        drop(app.read_owed_automation_requests());
        drop(app.update_automation_approval(Action::Read {
            generation: app.automation_approval.generation,
            result: Arc::new(Ok(vec![unrelated])),
        }));
        assert!(app.automation_approval.available.is_none());
        drop(app.automation_requests_changed(Some(Path::new("/fixture"))));
        let task = app.update_automation_approval(Action::Read {
            generation: app.automation_approval.generation,
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
            generation: app.automation_approval.generation,
            result: Arc::new(Ok(declined)),
        }));
        assert!(!app.automation_approval_open());
        assert!(app.core.keyboard_nav());
        // A read begun before the review is set aside.
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

    /// At the minimum 900x600 window with 150% interface scale, a long
    /// request scrolls and every decision stays whole and inside the window.
    #[test]
    fn approval_decisions_stay_in_reach_at_the_scaled_minimum_window() {
        let (mut app, _) = reviewing();
        app.automation_approval
            .proposal
            .as_mut()
            .unwrap()
            .definition
            .prompt = "Read all of the saved request before deciding.\n".repeat(80);
        let generation = app.automation_approval.generation;
        for role in ["dismiss", "decline", "approve"] {
            let geometry = |window| {
                super::super::dialog_ui::tests::action_geometry(
                    app.automation_approval_dialog().unwrap(),
                    window,
                    &approval_control_id(generation, role),
                )
            };
            let (_, _, _, natural) = geometry(cosmic::iced::Size::new(1200.0, 900.0));
            let (window, viewport, content, control) =
                geometry(cosmic::iced::Size::new(600.0, 400.0));
            assert!(
                control.height == natural.height
                    && control.y >= 0.0
                    && control.y + control.height <= window.height,
                "{role} must stay whole and inside the window: {control:?}, not {natural:?}"
            );
            assert!(viewport.y + viewport.height <= control.y);
            assert!(content > viewport.height, "the request scrolls instead");
        }
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
