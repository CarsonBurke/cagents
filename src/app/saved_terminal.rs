//! Saved terminal recipes are data until an exact existing attachment is found.
//! This presentation never asserts an exit, edits a recipe, or starts a process.
use super::*;

#[derive(Default)]
pub(super) struct State {
    pub(super) entries: HashMap<CardId, Entry>,
    outstanding: HashMap<uuid::Uuid, CardId>,
    queue: VecDeque<Waiting>,
    active: Option<uuid::Uuid>,
}

struct Waiting {
    card: CardId,
    request: uuid::Uuid,
    relaunch: bool,
}

pub(super) struct Entry {
    source: TerminalSession,
    request: uuid::Uuid,
    disposition: Disposition,
    directory: Directory,
    authority: crate::agent_execution::Authority,
    socket: Result<Option<PathBuf>, String>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Directory {
    Available,
    Missing,
    Unknown,
}

enum Disposition {
    Pending,
    Dormant,
    Unavailable,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Command {
    Reconnect,
    Relaunch,
    CopyDetails,
    Transcript,
    Folder,
    NewTerminal,
    Close,
}

#[derive(Debug, Clone)]
pub(super) enum Action {
    User {
        card: CardId,
        request: uuid::Uuid,
        command: Command,
    },
    Inspected {
        card: CardId,
        source: TerminalSession,
        request: uuid::Uuid,
        socket: Option<PathBuf>,
        relaunch: bool,
        directory: Directory,
        result: Result<Option<StartedTerminal>, String>,
    },
}
impl Action {
    pub(super) fn is_user_action(&self) -> bool {
        matches!(self, Self::User { .. })
    }
}

fn candidate(session: &TerminalSession) -> bool {
    matches!(
        session.continuity,
        TerminalContinuity::RelaunchAvailable {
            last_exit: None,
            ..
        } | TerminalContinuity::Live { .. }
            | TerminalContinuity::DetachedSameBoot { .. }
    )
}

/// Only the issued descriptor may select an existing attachment. No matching
/// descriptor means disconnected saved data, not evidence that a process ended.
fn attachment(
    source: &TerminalSession,
    terminals: &[crate::terminal::ipc::TerminalDescriptor],
    proof: &HostSessionProof,
) -> Result<Option<TerminalSession>, String> {
    if !proof.is_complete() {
        return Err("The terminal service did not confirm its identity".into());
    }
    if let Some(hosted) = terminals.iter().find(|entry| entry.session.id == source.id) {
        let mut expected = source.clone();
        expected.continuity = hosted.session.continuity.clone();
        if expected != hosted.session {
            return Err("The existing terminal has different saved source evidence".into());
        }
        if !hosted.session.continuity.can_resume_live(proof) && !terminal_ended(&hosted.session) {
            return Err("The existing terminal has uncertain ownership".into());
        }
        return Ok(Some(hosted.session.clone()));
    }
    // A proofless source cannot authenticate a different ID merely by matching
    // its command. Preserve an issued lineage conflict for ordinary recovery.
    if terminals.iter().any(|hosted| {
        hosted.session.recovery_origin == Some(source.recovery_origin.unwrap_or(source.id))
    }) {
        return Err(
            "A terminal shares this saved lineage; reconnect through its recovery controls".into(),
        );
    }
    Ok(None)
}

/// Keep the captured endpoint through the actual attachment, not just its
/// inventory lookup. The normal constructor may resolve the selected device
/// again; this saved-source operation must not follow a later device switch.
fn attach_existing(
    card: CardId,
    recovered: daemon_recovery::RecoveredTerminal,
    socket: PathBuf,
    proof: HostSessionProof,
    sender: async_channel::Sender<TerminalWakeup>,
) -> Result<StartedTerminal, String> {
    let handle = crate::terminal::HostedTerminalHandle::connect_at(
        socket,
        recovered.session.id,
        Some(proof),
        move |wake| {
            let _ = sender.try_send(TerminalWakeup {
                card,
                generation: wake.generation,
            });
        },
    )
    .map_err(|error| error.to_string())?;
    handle.wait_until_attached(crate::terminal::SPAWN_START_TIMEOUT + Duration::from_secs(10))?;
    drop(recovered.recovery_lease);
    Ok(StartedTerminal {
        handle: TerminalSurface::from(handle),
        session: recovered.session,
        spawned: false,
    })
}

impl Cagents {
    pub(super) fn saved_terminal_current(&self, card: CardId) -> Option<&Entry> {
        if self.shared_views.attached.contains(&card)
            || self.shared_views.defaults.contains_key(&card)
            || self.durable.pending_terminal_creations.contains(&card)
            || !matches!(self.cards.get(&card), Some(CardRuntime::Loading))
        {
            return None;
        }
        let entry = self.saved_terminals.entries.get(&card)?;
        self.card_state(card)
            .filter(|state| state.existence.is_open())
            .filter(|state| matches!(&state.kind, CardKind::Terminal(source) if source == &entry.source))?;
        Some(entry)
    }

    pub(super) fn saved_terminal_ready(&self, card: CardId) -> bool {
        self.saved_terminal_current(card).is_some_and(|entry| {
            matches!(entry.disposition, Disposition::Dormant)
                && entry.authority.token().is_none()
                && entry
                    .socket
                    .as_ref()
                    .ok()
                    .and_then(|socket| socket.as_ref())
                    .is_some_and(|expected| {
                        !crate::device::is_remote()
                            && crate::terminal::ipc::daemon_socket_path().ok().as_ref()
                                == Some(expected)
                    })
                && !self.terminal_start_in_flight(card)
                && !self.terminal_retry_pending(card)
                && !self.durable.pending_terminal_creations.contains(&card)
                && !self.shared_views.defaults.contains_key(&card)
                && !self.shared_views.attached.contains(&card)
                && matches!(self.cards.get(&card), Some(CardRuntime::Loading))
        })
    }

    /// Called only from restoration, never a new terminal/clone/user launch.
    pub(super) fn prepare_saved_terminal(
        &mut self,
        card: CardId,
        source: &TerminalSession,
    ) -> bool {
        if !candidate(source)
            || crate::device::is_remote()
            || self.shared_views.defaults.contains_key(&card)
            || self.shared_views.attached.contains(&card)
            || self.durable.pending_terminal_creations.contains(&card)
            || self.terminal_start_in_flight(card)
            || self.terminal_retry_pending(card)
        {
            return false;
        }
        let authority = crate::agent_execution::Authority::from_environment();
        let socket = if authority.token().is_some() || crate::device::is_remote() {
            Ok(None)
        } else {
            crate::terminal::ipc::daemon_socket_path()
                .map(Some)
                .map_err(|error| error.to_string())
        };
        self.saved_terminals.entries.insert(
            card,
            Entry {
                source: source.clone(),
                request: uuid::Uuid::new_v4(),
                disposition: Disposition::Pending,
                directory: Directory::Unknown,
                authority,
                socket,
            },
        );
        self.lifecycle.remove(card);
        self.cards.insert(card, CardRuntime::Loading);
        true
    }

    pub(super) fn inspect_saved_terminal(&mut self, card: CardId) -> Task<Message> {
        self.inspect_saved_terminal_for(card, false)
    }

    fn inspect_saved_terminal_for(&mut self, card: CardId, relaunch: bool) -> Task<Message> {
        if self.terminal_start_in_flight(card)
            || self.pane_close_owned(card)
            || self
                .saved_terminals
                .queue
                .iter()
                .any(|waiting| waiting.card == card)
            || self.saved_terminal_current(card).is_none()
        {
            return Task::none();
        }
        let request = uuid::Uuid::new_v4();
        let entry = self.saved_terminals.entries.get_mut(&card).unwrap();
        entry.request = request;
        entry.disposition = Disposition::Pending;
        self.saved_terminals.queue.push_back(Waiting {
            card,
            request,
            relaunch,
        });
        self.start_next_saved_terminal()
    }

    /// Many saved panes share one blocking inspection lane; opening a window
    /// must not create a thread and connection storm for its saved recipes.
    pub(super) fn start_next_saved_terminal(&mut self) -> Task<Message> {
        if self.saved_terminals.active.is_some() {
            return Task::none();
        }
        let waiting = loop {
            let Some(waiting) = self.saved_terminals.queue.pop_front() else {
                return Task::none();
            };
            if self
                .saved_terminal_current(waiting.card)
                .is_none_or(|entry| entry.request != waiting.request)
                || self.pane_close_owned(waiting.card)
            {
                continue;
            }
            if self.terminal_start_in_flight(waiting.card) {
                self.saved_terminals.queue.push_front(waiting);
                return Task::none();
            }
            break waiting;
        };
        let Waiting {
            card,
            request,
            relaunch,
        } = waiting;
        let entry = self.saved_terminal_current(card).unwrap();
        let source = entry.source.clone();
        let authority = entry.authority.clone();
        let socket = entry.socket.clone();
        self.saved_terminals.active = Some(request);
        // Capture caller scope and selected local endpoint before deferring.
        // A scoped caller cannot fall through to the ordinary User attachment.
        let sender = self.terminal_wake_tx.clone();
        self.begin_terminal_start(card);
        self.saved_terminals.outstanding.insert(request, card);
        cosmic::task::future(async move {
            let endpoint = socket.as_ref().ok().cloned().flatten();
            let original = source.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let directory = if authority.token().is_none()
                    && socket.as_ref().is_ok_and(|path| path.is_some())
                {
                    match std::fs::metadata(&source.launch.working_directory) {
                        Ok(metadata) if metadata.is_dir() => Directory::Available,
                        Ok(_) => Directory::Missing,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            Directory::Missing
                        }
                        Err(_) => Directory::Unknown,
                    }
                } else {
                    Directory::Unknown
                };
                let result = (|| {
                    if authority.token().is_some() {
                        return Err(
                            "Reconnect this saved terminal from its original user window"
                                .to_owned(),
                        );
                    }
                    let socket =
                        socket?.ok_or("Reconnect this saved terminal on its original device")?;
                    // No ensure_daemon_ready, recovery-plan execution, attach_or_spawn,
                    // or creation fallback. An unavailable inventory stays uncertain.
                    let mut client = crate::control::ControlClient::connect(&socket)
                        .map_err(|error| error.to_string())?;
                    let proof = client.proof().clone();
                    // List remains available while startup recovery retains hidden
                    // original owners. BuildInfo does not: never turn that partial
                    // inventory into successful disconnected-pane admission.
                    client.daemon_version().map_err(|error| error.to_string())?;
                    if source.continuity.live_proof().is_some() {
                        let recovered = daemon_recovery::inspect_saved_at(source.clone(), &socket)?;
                        if recovered.expected_host.as_ref() != Some(&proof) {
                            return Err("The terminal service changed during recovery".into());
                        }
                        // Recheck while the existing recovery lease remains held.
                        client.daemon_version().map_err(|error| error.to_string())?;
                        if matches!(
                            recovered.session.continuity,
                            TerminalContinuity::Live { .. }
                                | TerminalContinuity::DetachedSameBoot { .. }
                        ) || recovered.attach_completed
                        {
                            return attach_existing(card, recovered, socket, proof, sender)
                                .map(Some);
                        }
                        // This is an unused, normally authorized replay plan.
                        // Retain the ORIGINAL descriptor, never its derived ID,
                        // recipe, hints or transcript path. Dropping it releases
                        // the existing lease without creating a terminal.
                        drop(recovered);
                        return Ok(None);
                    }
                    let terminals = client.list().map_err(|error| error.to_string())?;
                    client.daemon_version().map_err(|error| error.to_string())?;
                    let Some(session) = attachment(&source, &terminals, &proof)? else {
                        return Ok(None);
                    };
                    attach_existing(
                        card,
                        daemon_recovery::RecoveredTerminal {
                            attach_completed: terminal_ended(&session),
                            session,
                            expected_host: Some(proof.clone()),
                            recovery_lease: None,
                        },
                        socket,
                        proof,
                        sender,
                    )
                    .map(Some)
                })();
                (directory, result)
            })
            .await;
            let (directory, result) = match outcome {
                Ok(outcome) => outcome,
                Err(error) => (
                    Directory::Unknown,
                    Err(format!("Terminal inspection failed: {error}")),
                ),
            };
            Message::SavedTerminal(Action::Inspected {
                card,
                source: original,
                request,
                socket: endpoint,
                relaunch,
                directory,
                result,
            })
        })
    }

    pub(super) fn update_saved_terminal(&mut self, action: Action) -> Task<Message> {
        let Action::Inspected {
            card,
            source,
            request,
            socket,
            relaunch,
            directory,
            result,
        } = action
        else {
            let Action::User {
                card,
                request,
                command,
            } = action
            else {
                unreachable!()
            };
            let Some(entry) = self
                .saved_terminal_current(card)
                .filter(|entry| entry.request == request)
            else {
                return Task::none();
            };
            let pending = matches!(entry.disposition, Disposition::Pending);
            let dormant = matches!(entry.disposition, Disposition::Dormant);
            let source = entry.source.clone();
            if self.pane_close_owned(card) || self.refuse_for_deleting_card(card) {
                return Task::none();
            }
            return match command {
                Command::Reconnect if !pending => self.inspect_saved_terminal(card),
                Command::Relaunch if !pending && dormant => {
                    self.inspect_saved_terminal_for(card, true)
                }
                Command::CopyDetails => {
                    let path = display_path(&source.launch.working_directory);
                    let path: String = path.chars().filter(|c| !c.is_control()).take(240).collect();
                    let state = if pending {
                        "checking existing terminal"
                    } else if dormant {
                        "not connected"
                    } else {
                        "ownership not verified"
                    };
                    let text = format!(
                        "State: {state}\nTerminal: {}\nSaved directory: {path}\nThe saved command was not restarted.",
                        source.id
                    );
                    chat_ui::invalidate_copy_intent();
                    iced::clipboard::write(text).map(cosmic::Action::App)
                }
                Command::Transcript => {
                    self.update_terminal_failure(terminal_failure::Action::Reveal(card))
                }
                Command::Folder => self.update(Message::OpenRepository),
                Command::NewTerminal => self.update(Message::NewTerminal),
                Command::Close => self.update(Message::CloseCard(card)),
                _ => Task::none(),
            };
        };
        if self.saved_terminals.outstanding.remove(&request) != Some(card) {
            drop(result);
            return Task::none();
        }
        if self.saved_terminals.active == Some(request) {
            self.saved_terminals.active = None;
        }
        let current = self.saved_terminal_current(card).is_some_and(|entry| {
            entry.source == source
                && entry.request == request
                && entry.socket.as_ref().ok().cloned().flatten() == socket
        }) && !self.pane_close_owned(card)
            && match socket.as_ref() {
                Some(expected) => {
                    !crate::device::is_remote()
                        && crate::terminal::ipc::daemon_socket_path().ok().as_ref()
                            == Some(expected)
                }
                None => result.is_err(),
            };
        if !current {
            // An attachment is only a subscription; dropping a stale handle
            // must never shut down the terminal it observed.
            drop(result);
            if let Some(entry) = self.saved_terminals.entries.get_mut(&card)
                && entry.request == request
            {
                entry.disposition = Disposition::Unavailable;
            }
            return self.finish_terminal_start(card);
        }
        match result {
            Ok(Some(started)) => {
                self.saved_terminals.entries.remove(&card);
                let exposure = self
                    .card_state(card)
                    .map(|state| state.exposure)
                    .unwrap_or(Exposure::Offscreen);
                self.lifecycle.register(
                    card,
                    exposure,
                    Residency::Hydrating {
                        epoch: unix_millis().max(1) as u64,
                    },
                    Instant::now(),
                );
                self.terminal_started(card, source.id, Ok(started))
            }
            result => {
                let entry = self.saved_terminals.entries.get_mut(&card).unwrap();
                entry.directory = directory;
                let absent = result.is_ok();
                if let Err(error) = &result {
                    tracing::debug!(?card, %error, "saved terminal inspection refused");
                }
                entry.disposition = if absent {
                    Disposition::Dormant
                } else {
                    Disposition::Unavailable
                };
                self.lifecycle.remove(card);
                let finished = self.finish_terminal_start(card);
                if absent
                    && relaunch
                    && self.saved_terminal_ready(card)
                    && matches!(directory, Directory::Available)
                    && !self.modal_is_open()
                    && !self.chrome_ui.is_open()
                    && !self.update_prepared
                    && !self.focused_repository_is_saving()
                {
                    // Explicit consent, after rechecking the exact existing
                    // owner. Retry keeps its normal source/identity fences and
                    // creates its ordinary persistent allocation exactly once.
                    self.saved_terminals.entries.remove(&card);
                    return Task::batch([finished, self.retry_card(card)]);
                }
                finished
            }
        }
    }

    pub(super) fn recheck_saved_terminal(
        &mut self,
        observed: &TerminalSession,
    ) -> Vec<Task<Message>> {
        let cards = self
            .saved_terminals
            .entries
            .iter()
            .filter(|(card, entry)| {
                !matches!(entry.disposition, Disposition::Pending)
                    && (observed.id == entry.source.id
                        || observed.recovery_origin
                            == Some(entry.source.recovery_origin.unwrap_or(entry.source.id)))
                    && self.saved_terminal_current(**card).is_some()
            })
            .map(|(card, _)| *card)
            .collect::<Vec<_>>();
        cards
            .into_iter()
            .map(|card| self.inspect_saved_terminal(card))
            .collect()
    }

    pub(super) fn saved_terminal_status(&self, card: CardId) -> Option<(&'static str, bool)> {
        match self.saved_terminal_current(card)?.disposition {
            Disposition::Pending => Some(("Checking saved terminal", true)),
            Disposition::Dormant => None,
            Disposition::Unavailable => Some(("Reconnect unavailable", false)),
        }
    }

    pub(super) fn saved_terminal_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let entry = self.saved_terminal_current(card)?;
        let pending = matches!(entry.disposition, Disposition::Pending);
        let dormant = matches!(entry.disposition, Disposition::Dormant);
        let request = entry.request;
        let action = |command| {
            Message::SavedTerminal(Action::User {
                card,
                request,
                command,
            })
        };
        let control_id = |control| {
            card_control_id(
                card,
                &format!("saved-terminal.{control}.{request}.{pending}"),
            )
        };
        let description = match entry.disposition {
            Disposition::Pending => "Checking for the existing terminal…",
            Disposition::Dormant => {
                "Not connected. The saved command was not restarted. Reconnect checks for the original terminal. Resume or Restart runs the saved session only when you choose."
            }
            Disposition::Unavailable => {
                "The existing terminal could not be verified. Reconnect to check again; the saved command has not been restarted."
            }
        };
        let reconnect =
            crate::ui_controls::button::primary(if pending { "Checking…" } else { "Reconnect" })
                .id(control_id("reconnect"))
                .name("Reconnect saved terminal");
        let reconnect = if pending {
            reconnect
        } else {
            reconnect.on_press(action(Command::Reconnect))
        };
        let mut controls = vec![reconnect.into()];
        if dormant {
            let label = if entry.source.agent_resume_recipe().is_some() {
                "Resume session"
            } else {
                "Restart"
            };
            controls.push(
                crate::ui_controls::button::secondary(label)
                    .id(control_id("relaunch"))
                    .name("Explicitly relaunch the saved terminal")
                    .on_press(action(Command::Relaunch))
                    .into(),
            );
        }
        controls.extend([
            crate::ui_controls::button::secondary("Open another folder…")
                .id(control_id("folder"))
                .name("Choose another workspace folder")
                .on_press(action(Command::Folder))
                .into(),
            crate::ui_controls::button::secondary("New terminal in selected workspace")
                .id(control_id("new"))
                .name("Create a separate terminal in the currently selected workspace")
                .on_press(action(Command::NewTerminal))
                .into(),
            crate::ui_controls::button::secondary("Close")
                .id(control_id("close"))
                .name("Close saved terminal pane")
                .on_press(action(Command::Close))
                .into(),
        ]);
        let diagnostics = widget::flex_row(vec![
            crate::ui_controls::button::secondary("Copy details")
                .id(control_id("copy"))
                .name("Copy saved terminal details")
                .on_press(action(Command::CopyDetails))
                .into(),
            self.terminal_transcript_control(card, action(Command::Transcript)),
        ])
        .spacing(space::S as u16);
        let directory_notice = match entry.directory {
            Directory::Missing => {
                "The saved directory is missing. Restore it before restarting, or choose another folder for a separate terminal."
            }
            Directory::Unknown => "The saved directory has not been verified.",
            Directory::Available => {
                "Opening another folder or terminal does not change this saved command."
            }
        };
        let content = widget::column::with_children(vec![
            design::title("Saved terminal").into(),
            design::caption(description).into(),
            design::caption(directory_notice).into(),
            design::caption(format!(
                "Saved directory: {}",
                display_path(&entry.source.launch.working_directory)
            ))
            .into(),
            widget::flex_row(controls).spacing(space::S as u16).into(),
            diagnostics.into(),
        ])
        .spacing(space::M)
        .width(Length::Fill);
        Some(crate::ui_controls::owned(
            (request, pending, dormant),
            widget::scrollable(widget::container(content).padding(space::L)).height(Length::Fill),
        ))
    }
}

#[cfg(test)]
#[path = "saved_terminal/tests.rs"]
mod tests;
