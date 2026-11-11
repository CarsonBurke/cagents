//! Candidate-owned fallback for source daemons that cannot checkpoint work.
use super::floor_display::{StopList, chat_item_with_state, terminal_item};
use super::*;
use crate::model::TerminalId;
use crate::terminal_ipc::{ShutdownCapability, ShutdownOutcome, TerminalDescriptor};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn terminal_confirmation(state: &Path, job: &UpdateJob) -> Result<String, UpdateError> {
    let request: UpdateRequest = serde_json::from_slice(&read_private(
        &state.join(format!("{}.request", job.id)),
        SMALL_ASSET_LIMIT,
    )?)?;
    let command = match request {
        UpdateRequest::Release(release) | UpdateRequest::ReleaseFromTerminal(release) => {
            format!("cagents daemon upgrade --version {}", release.version)
        }
        _ => "cagents daemon upgrade".to_owned(),
    };
    Ok(format!(
        "Stopping live work needs confirmation in a terminal. Run `{command}` from a system terminal outside cagents."
    ))
}

pub(super) fn preview_stop_list(
    preview: &impact::Preview,
) -> Result<Option<StopList>, UpdateError> {
    let Some(daemon) = preview.daemon.as_ref().filter(|daemon| daemon.blocked()) else {
        return Ok(None);
    };
    let mut client = source_client_for(daemon)?;
    let inventory = Inventory {
        chats: client.list_chats()?,
        terminals: client.list()?,
        chat_states: BTreeMap::new(),
    };
    let service = needs_service_stop(daemon)
        || super::floor_prediction::needs_service_stop(
            preview,
            &inventory.chats,
            &inventory.terminals,
        )?;
    stop_list(preview, &inventory, service).map(Some)
}

pub(super) fn needs_service_stop(daemon: &impact::DaemonImpact) -> bool {
    !daemon.blockers.is_empty()
        || daemon.sessions.iter().any(|item| {
            item.kind == impact::SessionKind::Terminal
                && item.disposition == impact::Disposition::Blocked
        })
}

#[derive(Clone, Default)]
struct Inventory {
    chats: Vec<crate::chat::Session>,
    chat_states: BTreeMap<TerminalId, String>,
    terminals: Vec<TerminalDescriptor>,
}

fn stop_list(
    preview: &impact::Preview,
    inventory: &Inventory,
    service: bool,
) -> Result<StopList, UpdateError> {
    let daemon = preview
        .daemon
        .as_ref()
        .ok_or_else(|| invalid("The source service is no longer running"))?;
    let processes = super::floor_display::TerminalProcesses::observe();
    let mut items = Vec::new();
    for item in &daemon.sessions {
        let label = preview.labels.names.get(&item.id).unwrap_or(&item.title);
        match item.kind {
            impact::SessionKind::NativeChat
                if service || item.disposition == impact::Disposition::Blocked =>
            {
                let chat = inventory
                    .chats
                    .iter()
                    .find(|chat| chat.id == item.id)
                    .ok_or_else(|| {
                        invalid("The source conversations changed during stop review")
                    })?;
                items.push(chat_item_with_state(
                    chat,
                    item,
                    label,
                    inventory
                        .chat_states
                        .get(&chat.id)
                        .map(String::as_str)
                        .unwrap_or("state unavailable"),
                ));
            }
            impact::SessionKind::Terminal if service => {
                // A paired console belongs to the chat's pane and is covered by
                // that single chat entry, exactly as the source impact reports it.
                if inventory.chats.iter().any(|chat| chat.id == item.id) {
                    continue;
                }
                let terminal = inventory
                    .terminals
                    .iter()
                    .find(|terminal| terminal.session.id == item.id)
                    .ok_or_else(|| invalid("The source terminals changed during stop review"))?;
                if terminal
                    .session
                    .continuity
                    .can_resume_live(&daemon.capability.proof)
                {
                    items.push(terminal_item(terminal, item, label, &processes));
                }
            }
            _ => (),
        }
    }
    if service {
        // Older sources expose pending launches in List/ChatList before their
        // UpgradeImpact acquires an entry. They are still owners the confirmed
        // service stop ends, so bind them to their exact issued descriptors.
        for chat in &inventory.chats {
            if items
                .iter()
                .any(|item| item.id == chat.id && item.kind == impact::SessionKind::NativeChat)
            {
                continue;
            }
            let fallback = format!("{} chat", chat.launch.provider.label());
            let label = preview.labels.names.get(&chat.id).unwrap_or(&fallback);
            let item = descriptor_impact(chat.id, impact::SessionKind::NativeChat, label, chat)?;
            items.push(chat_item_with_state(
                chat,
                &item,
                label,
                inventory
                    .chat_states
                    .get(&chat.id)
                    .map(String::as_str)
                    .unwrap_or("state unavailable"),
            ));
        }
        for terminal in &inventory.terminals {
            let id = terminal.session.id;
            if !terminal
                .session
                .continuity
                .can_resume_live(&daemon.capability.proof)
                || inventory.chats.iter().any(|chat| chat.id == id)
                || items
                    .iter()
                    .any(|item| item.id == id && item.kind == impact::SessionKind::Terminal)
            {
                continue;
            }
            let label = preview
                .labels
                .names
                .get(&id)
                .map(String::as_str)
                .or(terminal.title.as_deref())
                .unwrap_or("Terminal");
            let item =
                descriptor_impact(id, impact::SessionKind::Terminal, label, &terminal.session)?;
            items.push(terminal_item(terminal, &item, label, &processes));
        }
    }
    items.sort_by_key(|item| (item.kind, item.id));
    let (background_tasks, background_identity) = if service {
        (
            daemon.capability.background_subagents,
            daemon.capability.background_identity.clone(),
        )
    } else {
        let tasks: BTreeSet<_> = daemon
            .sessions
            .iter()
            .filter(|session| items.iter().any(|item| item.id == session.id))
            .flat_map(|session| {
                session
                    .interrupted_subagents
                    .iter()
                    .map(move |task| (session.id, task))
            })
            .collect();
        let identity = if tasks.is_empty() {
            String::new()
        } else {
            format!("{:x}", Sha256::digest(serde_json::to_vec(&tasks)?))
        };
        (tasks.len() as u64, identity)
    };
    Ok(StopList {
        from_version: preview.from_version.clone(),
        background_tasks,
        background_identity,
        items,
    })
}

fn descriptor_impact<T: Serialize>(
    id: TerminalId,
    kind: impact::SessionKind,
    title: &str,
    descriptor: &T,
) -> Result<impact::SessionImpact, UpdateError> {
    Ok(impact::SessionImpact {
        id,
        kind,
        title: title.into(),
        identity: format!("{:x}", Sha256::digest(serde_json::to_vec(descriptor)?)),
        disposition: impact::Disposition::Blocked,
        reason: String::new(),
        interrupted_subagents: Vec::new(),
    })
}

/// The worker engine uses the same source operations in production and in its
/// refused-prepare tests. The adapter owns authentication, persistence and the
/// final lease transfer; only this engine decides when destruction is allowed.
trait FloorSource {
    fn observe(&mut self) -> Result<impact::Preview, UpdateError>;
    fn observe_policy(&mut self, policy: impact::Policy) -> Result<impact::Preview, UpdateError>;
    fn inventory(
        &mut self,
        daemon: &impact::DaemonImpact,
        service: bool,
    ) -> Result<Inventory, UpdateError>;
    fn begin_preparation(
        &mut self,
        _state: &Path,
        _job: &UpdateJob,
        _daemon: &impact::DaemonImpact,
    ) -> Result<(), UpdateError> {
        Ok(())
    }
    fn prepare(&mut self, daemon: &impact::DaemonImpact) -> Result<(), source::PrepareError>;
    fn begin_interrupts(
        &mut self,
        daemon: &impact::DaemonImpact,
        deadline: Instant,
    ) -> Result<(), UpdateError>;
    fn pending_interrupts(&mut self) -> Result<Vec<TerminalId>, UpdateError>;
    fn interrupt_impact(&self) -> Result<impact::DaemonImpact, UpdateError>;
    fn close_chat(
        &mut self,
        daemon: &impact::DaemonImpact,
        chat: crate::chat::Session,
    ) -> Result<crate::chat::Snapshot, UpdateError>;
    fn capture_record(
        &mut self,
        daemon: &impact::DaemonImpact,
        chats: Vec<crate::chat::Session>,
        terminals: Vec<TerminalId>,
        service: bool,
    ) -> Result<crate::upgrade::floor::Record, UpdateError>;
    fn predict_service(
        &mut self,
        preview: &impact::Preview,
        inventory: &Inventory,
    ) -> Result<bool, UpdateError>;
    fn preflight(
        &mut self,
        daemon: &impact::DaemonImpact,
    ) -> Result<ShutdownCapability, UpdateError>;
    fn shutdown(
        &mut self,
        daemon: &impact::DaemonImpact,
        capability: ShutdownCapability,
    ) -> Result<ShutdownOutcome, UpdateError>;
    fn retain(
        &mut self,
        daemon: &impact::DaemonImpact,
        record: crate::upgrade::floor::Record,
    ) -> Result<(), UpdateError>;
    fn capture(&mut self, list: &StopList) -> Result<(), UpdateError>;
    fn capture_service(
        &mut self,
        _daemon: &impact::DaemonImpact,
        list: &StopList,
    ) -> Result<(), UpdateError> {
        self.capture(list)
    }
    fn ended(
        &mut self,
        _items: &[super::floor_display::StopItem],
        _service: bool,
        _background_tasks: u64,
    ) {
    }
    fn archive(&mut self) -> Result<(), UpdateError> {
        Ok(())
    }
    fn stopped(
        &mut self,
        state: &Path,
        job: &mut UpdateJob,
        service: bool,
    ) -> Result<(), UpdateError>;
}

struct NativeSource<'a, O> {
    observe: O,
    policy: impact::Policy,
    proof: crate::model::HostSessionProof,
    paths: crate::daemon::RuntimePaths,
    pinned: Option<crate::daemon::lease_handoff::HandoffSource>,
    handoff: &'a mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
    survivors: &'a mut Survivors,
    reviewed: &'a impact::Preview,
    guard: &'a crate::instance::UiUpdateGuard,
    interrupts: Option<source::Interrupts>,
    interrupt_reviewed: Option<impact::DaemonImpact>,
    preparing_client: Option<source::Client>,
    preparation: Option<crate::upgrade::recovery::Preparation>,
}

/// Frozen lifecycle sources can prepare and prove Ready without exposing the
/// current inventory/snapshot DTOs. They never acquire Prepared recovery power.
fn preparation_census(
    legacy: bool,
    checkpoint_absent: impl FnOnce() -> Result<(), String>,
    observe: impl FnOnce() -> Result<crate::upgrade::recovery::Census, UpdateError>,
) -> Result<Option<crate::upgrade::recovery::Census>, UpdateError> {
    if legacy {
        checkpoint_absent().map_err(invalid)?;
        Ok(None)
    } else {
        observe().map(Some)
    }
}

impl<O: FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>> FloorSource
    for NativeSource<'_, O>
{
    fn observe(&mut self) -> Result<impact::Preview, UpdateError> {
        (self.observe)(self.policy)
    }
    fn observe_policy(&mut self, policy: impact::Policy) -> Result<impact::Preview, UpdateError> {
        self.policy = policy;
        self.observe()
    }
    fn inventory(
        &mut self,
        daemon: &impact::DaemonImpact,
        _service: bool,
    ) -> Result<Inventory, UpdateError> {
        let mut client = source_client_for(daemon)?;
        let chats = client.list_chats()?;
        let terminals = client.list()?;
        let chat_states = chats
            .iter()
            .map(|chat| {
                let activity = terminals
                    .iter()
                    .find(|terminal| terminal.session.id == chat.id)
                    .and_then(|terminal| terminal.agent_activity.as_ref())
                    .map(|activity| activity.state);
                let state = match activity {
                    Some(crate::agent::ActivityState::Working) => "working",
                    Some(crate::agent::ActivityState::Waiting) => "waiting",
                    Some(crate::agent::ActivityState::Idle) => "idle",
                    _ if daemon.sessions.iter().any(|item| {
                        item.id == chat.id && item.disposition == impact::Disposition::WaitsForIdle
                    }) =>
                    {
                        "working"
                    }
                    _ => "state unavailable",
                };
                (chat.id, state.to_owned())
            })
            .collect();
        Ok(Inventory {
            chats,
            chat_states,
            terminals,
        })
    }
    fn begin_preparation(
        &mut self,
        state: &Path,
        job: &UpdateJob,
        daemon: &impact::DaemonImpact,
    ) -> Result<(), UpdateError> {
        let mut client = source_client_for(daemon)?;
        let census = preparation_census(
            matches!(&client, source::Client::Legacy(_)),
            crate::upgrade::recovery::require_checkpoint_absent,
            || {
                let chats = client.list_chats()?;
                let mut snapshots = crate::upgrade::recovery::SnapshotCapture::default();
                for chat in &chats {
                    let snapshot = client.capture_chat_snapshot(chat.clone())?;
                    snapshots.push(chat.clone(), snapshot).map_err(invalid)?;
                }
                let terminals: Vec<_> = client
                    .list()?
                    .into_iter()
                    .map(|item| item.session)
                    .collect();
                let mut presentation = crate::upgrade::floor::Record::new(
                    self.proof.clone(),
                    chats.clone(),
                    terminals.iter().map(|terminal| terminal.id).collect(),
                );
                presentation.protocol = client.protocol();
                // Saved panes can predate ChatList's confirmed conversation ID.
                // Capture their exact aliases before preparation changes ownership.
                let presentation = capture_floor(self.guard, presentation)?;
                Ok(crate::upgrade::recovery::Census {
                    terminals,
                    chats,
                    snapshots: snapshots.finish(),
                    aliases: presentation.aliases,
                    presentations: presentation.presentations,
                })
            },
        )?;
        self.survivors.prepared_source = Some(match census {
            Some(census) => {
                self.preparation = Some(
                    crate::upgrade::recovery::Preparation::begin(
                        state,
                        &job.id,
                        daemon,
                        census.clone(),
                        self.guard,
                    )
                    .map_err(invalid)?,
                );
                PreparationEvidence::Full {
                    daemon: daemon.clone(),
                    census,
                }
            }
            None => {
                self.preparation = None;
                PreparationEvidence::ScopeOnly(daemon.clone())
            }
        });
        for item in &daemon.sessions {
            self.survivors.labels.insert(
                item.id,
                self.reviewed
                    .labels
                    .names
                    .get(&item.id)
                    .unwrap_or(&item.title)
                    .clone(),
            );
        }
        self.preparing_client = Some(client);
        Ok(())
    }
    fn prepare(&mut self, daemon: &impact::DaemonImpact) -> Result<(), source::PrepareError> {
        let mut client = self.preparing_client.take().ok_or_else(|| {
            source::PrepareError::Invalid(invalid(
                "The update has not captured its source preparation census",
            ))
        })?;
        client.prepare_confirmed_upgrade(daemon.clone(), false)?;
        // An accepted prepare has crossed the point of no return even when
        // capturing its checkpoint subsequently fails. Never classify that
        // local capture failure as a refused prepare and stop the service.
        self.survivors.record_prepared(self.reviewed, daemon);
        if let Some(interrupted) = &self.interrupt_reviewed {
            self.survivors.record_prepared(self.reviewed, interrupted);
        }
        if let Some(preparation) = self.preparation.take() {
            match preparation.acknowledged() {
                Ok(captured) => self.survivors.captured_preparation = Some(captured),
                Err(error) => self.survivors.recovery_capture_error = Some(error),
            }
        }
        Ok(())
    }
    fn begin_interrupts(
        &mut self,
        daemon: &impact::DaemonImpact,
        deadline: Instant,
    ) -> Result<(), UpdateError> {
        self.survivors
            .record_interrupt_requests(self.reviewed, daemon);
        self.interrupt_reviewed = Some(daemon.clone());
        self.interrupts = Some(source::Interrupts::begin(daemon, deadline)?);
        Ok(())
    }
    fn pending_interrupts(&mut self) -> Result<Vec<TerminalId>, UpdateError> {
        let interrupts = self
            .interrupts
            .as_mut()
            .ok_or_else(|| invalid("The update has not requested these interruptions"))?;
        let pending = interrupts.poll()?;
        if let Some(reviewed) = &self.interrupt_reviewed {
            self.survivors
                .record_settled_interruptions(self.reviewed, reviewed, &pending);
        }
        self.survivors.late_turns.extend(interrupts.late_turns());
        Ok(pending)
    }
    fn interrupt_impact(&self) -> Result<impact::DaemonImpact, UpdateError> {
        self.interrupts
            .as_ref()
            .map(|interrupts| interrupts.current_impact().clone())
            .ok_or_else(|| invalid("The update has not observed interruption settlement"))
    }
    fn close_chat(
        &mut self,
        daemon: &impact::DaemonImpact,
        chat: crate::chat::Session,
    ) -> Result<crate::chat::Snapshot, UpdateError> {
        source_client_for(daemon)?.close_chat(chat)
    }
    fn capture_record(
        &mut self,
        daemon: &impact::DaemonImpact,
        chats: Vec<crate::chat::Session>,
        terminals: Vec<TerminalId>,
        service: bool,
    ) -> Result<crate::upgrade::floor::Record, UpdateError> {
        let mut record = crate::upgrade::floor::Record::new(self.proof.clone(), chats, terminals);
        record.protocol = source_client_for(daemon)?.protocol();
        record.service_stopped = service;
        capture_floor(self.guard, record)
    }
    fn predict_service(
        &mut self,
        preview: &impact::Preview,
        inventory: &Inventory,
    ) -> Result<bool, UpdateError> {
        super::floor_prediction::needs_service_stop(preview, &inventory.chats, &inventory.terminals)
    }
    fn preflight(
        &mut self,
        daemon: &impact::DaemonImpact,
    ) -> Result<ShutdownCapability, UpdateError> {
        source_client_for(daemon)?.shutdown_preflight()
    }
    fn shutdown(
        &mut self,
        daemon: &impact::DaemonImpact,
        capability: ShutdownCapability,
    ) -> Result<ShutdownOutcome, UpdateError> {
        source_client_for(daemon)?.confirm_shutdown(capability)
    }
    fn retain(
        &mut self,
        _: &impact::DaemonImpact,
        record: crate::upgrade::floor::Record,
    ) -> Result<(), UpdateError> {
        retain_floor(self.guard, record)
    }
    fn capture(&mut self, list: &StopList) -> Result<(), UpdateError> {
        let socket = crate::terminal_ipc::daemon_socket_path()?;
        let mut client =
            source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
        if client.proof() != &self.proof {
            return Err(invalid("The source changed during its survivor census"));
        }
        if let Some(pid) = client.peer_pid() {
            self.survivors.exclude_ambient(Path::new("/proc"), pid);
        }
        let ids: BTreeSet<_> = list.items.iter().map(|item| item.id).collect();
        for terminal in client.list()? {
            if ids.contains(&terminal.session.id) {
                self.survivors.remember_origin(
                    terminal.session.id,
                    terminal
                        .session
                        .recovery_origin
                        .unwrap_or(terminal.session.id),
                );
            }
        }
        self.survivors.capture(list, None)
    }
    fn capture_service(
        &mut self,
        daemon: &impact::DaemonImpact,
        list: &StopList,
    ) -> Result<(), UpdateError> {
        let mut client = source_client_for(daemon)?;
        if let Some(pid) = client.peer_pid() {
            self.survivors.exclude_ambient(Path::new("/proc"), pid);
        }
        let ids: BTreeSet<_> = list.items.iter().map(|item| item.id).collect();
        for terminal in client.list()? {
            if ids.contains(&terminal.session.id) {
                self.survivors.remember_origin(
                    terminal.session.id,
                    terminal
                        .session
                        .recovery_origin
                        .unwrap_or(terminal.session.id),
                );
            }
        }
        let root = client
            .peer_pid()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(|pid| crate::daemon::holder_registry::Process::read(pid).ok());
        self.survivors.capture(list, root)
    }
    fn ended(
        &mut self,
        items: &[super::floor_display::StopItem],
        service: bool,
        background_tasks: u64,
    ) {
        for item in items {
            self.survivors
                .ended
                .insert((item.kind, item.id), item.clone());
        }
        self.survivors.service_stopped |= service;
        self.survivors.background_tasks = background_tasks;
    }
    fn archive(&mut self) -> Result<(), UpdateError> {
        archive_checkpoint()
    }
    fn stopped(
        &mut self,
        state: &Path,
        job: &mut UpdateJob,
        _service: bool,
    ) -> Result<(), UpdateError> {
        let pinned = self
            .pinned
            .take()
            .ok_or_else(|| invalid("The source lease was already handed over"))?;
        *self.handoff = Some(wait_daemon_stopped(self.paths.clone(), pinned, state, job)?);
        Ok(())
    }
}

/// Called while windows have flushed and prepared, but remain open. No floor
/// action occurs until the terminal has supplied StopListed for this inventory.
pub(super) fn quiesce(
    state: &Path,
    job: &mut UpdateJob,
    confirmed: &impact::Preview,
    terminal: bool,
    observe: impl FnMut(impact::Policy) -> Result<impact::Preview, UpdateError>,
    handoff: &mut Option<crate::daemon::lease_handoff::LeaseHandoff>,
    survivors: &mut Survivors,
    guard: &crate::instance::UiUpdateGuard,
) -> Result<(), UpdateError> {
    let Some(initial) = &confirmed.daemon else {
        return Ok(());
    };
    let paths =
        crate::daemon::RuntimePaths::discover().map_err(|error| invalid(error.to_string()))?;
    let pinned =
        crate::daemon::lease_handoff::HandoffSource::pin(&paths, &initial.capability.proof)
            .map_err(|error| invalid(error.to_string()))?;
    let mut source = NativeSource {
        observe,
        policy: initial.policy,
        proof: initial.capability.proof.clone(),
        paths,
        pinned: Some(pinned),
        handoff,
        survivors,
        reviewed: confirmed,
        guard,
        interrupts: None,
        interrupt_reviewed: None,
        preparing_client: None,
        preparation: None,
    };
    quiesce_source(state, job, confirmed, terminal, &mut source)
}

fn reviewed_inventory(
    source: &mut impl FloorSource,
    preview: impact::Preview,
    service: bool,
) -> Result<(impact::Preview, StopList, Inventory), UpdateError> {
    let daemon = preview
        .daemon
        .as_ref()
        .ok_or_else(|| invalid("The source disappeared during stop review"))?;
    let inventory = source.inventory(daemon, service)?;
    let list = stop_list(&preview, &inventory, service)?;
    Ok((preview, list, inventory))
}

fn shutdown_refusal(outcome: &ShutdownOutcome) -> &'static str {
    match outcome {
        ShutdownOutcome::EmptyRequired { .. } => "it requires an empty inventory",
        ShutdownOutcome::DaemonRestarted => "the service restarted during confirmation",
        ShutdownOutcome::InvalidCapability => "the service rejected its stop permission",
        ShutdownOutcome::AlreadyShuttingDown => "the service is already shutting down",
        ShutdownOutcome::ConfirmationRequired { .. } => "the service requires a new stop review",
        ShutdownOutcome::InventoryChanged { .. } => "its inventory changed",
        ShutdownOutcome::Expired => "its stop permission expired",
        ShutdownOutcome::Accepted { .. } => "the stop was accepted",
    }
}

fn recovery_not_confirmed(item: &mut super::floor_display::StopItem) {
    item.recovery = match item.kind {
        impact::SessionKind::NativeChat => {
            "could not confirm that the captured transcript was saved; start a new conversation"
        }
        impact::SessionKind::Terminal => {
            "could not confirm that the terminal's update recovery was saved; restart the terminal"
        }
    }
    .into();
}

fn background_recovery(tasks: u64) -> String {
    match tasks {
        0 => String::new(),
        1 => "1 background task stopped; it does not resume automatically.\n".into(),
        _ => format!("{tasks} background tasks stopped; they do not resume automatically.\n"),
    }
}

fn completed_error(
    error: UpdateError,
    completed: &[super::floor_display::StopItem],
    service: bool,
    background_tasks: u64,
) -> UpdateError {
    if completed.is_empty() && !service && background_tasks == 0 {
        return error;
    }
    let mut message = format!("{error}\n");
    if service {
        message.push_str("The terminal service accepted the stop.\n");
    }
    message.push_str(&background_recovery(background_tasks));
    message.push_str("Ended or stopped:\n");
    for item in completed {
        message.push_str(&format!("  {}: {}\n", item.label, item.recovery));
    }
    invalid(message)
}

fn quiesce_source(
    state: &Path,
    job: &mut UpdateJob,
    confirmed: &impact::Preview,
    terminal: bool,
    source: &mut impl FloorSource,
) -> Result<(), UpdateError> {
    let mut completed = Vec::new();
    let mut service_stopped = false;
    let mut background_tasks = 0;
    let result = quiesce_inner(
        state,
        job,
        confirmed,
        terminal,
        source,
        &mut completed,
        &mut service_stopped,
        &mut background_tasks,
    );
    result.map_err(|error| completed_error(error, &completed, service_stopped, background_tasks))
}

fn quiesce_inner(
    state: &Path,
    job: &mut UpdateJob,
    confirmed: &impact::Preview,
    terminal: bool,
    source: &mut impl FloorSource,
    completed: &mut Vec<super::floor_display::StopItem>,
    service_stopped: &mut bool,
    background_tasks: &mut u64,
) -> Result<(), UpdateError> {
    let Some(initial) = &confirmed.daemon else {
        return Ok(());
    };
    confirmation::check_cancelled(state, job)?;
    let mut preview = source.observe()?;
    confirmation::check_cancelled(state, job)?;
    if !confirmed.preserves_consent(&preview) {
        return Err(invalid(
            "Session ownership changed before stopping the source; review the update again",
        ));
    }
    // Finish the ordinary, cancellable wait before collecting destructive consent.
    if preview.daemon.as_ref().is_some_and(|daemon| {
        daemon
            .sessions
            .iter()
            .any(|item| item.disposition == impact::Disposition::WaitsForIdle)
    }) {
        preview = confirmation::wait_approved(state, job, preview, |policy| {
            source.observe_policy(policy)
        })?;
    }
    // Interrupted is a consented outcome, not a provider receipt. Old sources
    // prepare their checkpoint before attempting interruption during shutdown;
    // settle it while they can still serve, before crossing that boundary.
    preview = settle_interrupts(state, job, preview, source, Duration::from_secs(30))?;
    let daemon = preview
        .daemon
        .as_ref()
        .ok_or_else(|| invalid("The source disappeared during update review"))?;
    let mut service = needs_service_stop(daemon);
    // With no known ends, prepare itself determines whether the service can
    // checkpoint. A refusal has not ended anything and precedes the one stop list.
    if !daemon.blocked() {
        source.begin_preparation(state, job, daemon)?;
        confirmation::prepared(state, job)?;
        match source.prepare(daemon) {
            Ok(()) => return source.stopped(state, job, false),
            Err(source::PrepareError::Refused(error)) => {
                confirmation::resume_cancellation(state, job)?;
                eprintln!("The source could not checkpoint its work: {error}");
                service = true;
            }
            Err(error) => return Err(error.into()),
        }
    }
    if !service {
        let inventory = source.inventory(daemon, true)?;
        service = source.predict_service(&preview, &inventory)?;
    }
    'review: loop {
        let (reviewed, listed) = confirmation::wait_stop(state, job, terminal, || {
            let current = source.observe()?;
            if current
                .daemon
                .as_ref()
                .is_none_or(|daemon| daemon.capability.proof != initial.capability.proof)
            {
                return Err(invalid("The source changed during stop review"));
            }
            let (_, _, inventory) = reviewed_inventory(source, current.clone(), true)?;
            service |= needs_service_stop(current.daemon.as_ref().unwrap())
                || source.predict_service(&current, &inventory)?;
            // Mint the service capability before asking, then validate it again
            // against a fresh descriptor/job digest immediately before the action.
            if service {
                source.preflight(current.daemon.as_ref().unwrap())?;
            }
            let list = stop_list(&current, &inventory, service)?;
            Ok((current, list))
        })?;
        let current = source.observe()?;
        let (latest, latest_list, inventory) = reviewed_inventory(source, current, service)?;
        if !service
            && (needs_service_stop(latest.daemon.as_ref().unwrap())
                || source.predict_service(&latest, &inventory)?)
        {
            service = true;
            continue;
        }
        if !reviewed.preserves_stop_consent(&latest) || listed.digest() != latest_list.digest() {
            continue;
        }
        let daemon = latest.daemon.as_ref().unwrap();
        let ending: Vec<_> = inventory
            .chats
            .iter()
            .filter(|chat| {
                listed
                    .items
                    .iter()
                    .any(|item| item.kind == impact::SessionKind::NativeChat && item.id == chat.id)
            })
            .cloned()
            .collect();
        let terminal_ids = listed
            .items
            .iter()
            .filter(|item| item.kind == impact::SessionKind::Terminal)
            .map(|item| item.id)
            .collect();
        let record = source.capture_record(daemon, ending.clone(), terminal_ids, service)?;
        if service {
            source.capture_service(daemon, &listed)?;
        } else {
            source.capture(&listed)?;
        }
        // Snapshot capture and survivor inspection can take time. A new owner or
        // foreground command must be shown to the person before anything ends.
        let observed = source.observe()?;
        let (fresh, fresh_list, fresh_inventory) = reviewed_inventory(source, observed, service)?;
        if !service
            && (needs_service_stop(fresh.daemon.as_ref().unwrap())
                || source.predict_service(&fresh, &fresh_inventory)?)
        {
            service = true;
            continue;
        }
        if !latest.preserves_stop_consent(&fresh) || listed.digest() != fresh_list.digest() {
            continue;
        }
        let daemon = fresh.daemon.as_ref().unwrap();
        if service {
            let capability = source.preflight(daemon)?;
            let observed = source.observe()?;
            let (last, last_list, _) = reviewed_inventory(source, observed, true)?;
            if !fresh.preserves_stop_consent(&last) || listed.digest() != last_list.digest() {
                continue;
            }
            confirmation::prepared(state, job)?;
            match source.shutdown(last.daemon.as_ref().unwrap(), capability)? {
                ShutdownOutcome::Accepted { .. } => {
                    *service_stopped = true;
                    *background_tasks = listed.background_tasks;
                    completed.extend(listed.items.clone());
                    source.ended(&listed.items, true, *background_tasks);
                    // Accepted invalidates the old checkpoint even when waiting
                    // for exit or writing the recovery record subsequently fails.
                    let archived = source.archive();
                    let retained = source.retain(daemon, record);
                    if retained.is_err() {
                        for item in completed.iter_mut() {
                            recovery_not_confirmed(item);
                        }
                        source.ended(completed, true, *background_tasks);
                    }
                    match (archived, retained) {
                        (Ok(()), Ok(())) => (),
                        (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
                        (Err(archive), Err(retain)) => {
                            return Err(invalid(format!(
                                "{archive}; could not confirm stopped-pane recovery: {retain}"
                            )));
                        }
                    }
                    return source.stopped(state, job, true);
                }
                ShutdownOutcome::InventoryChanged { .. } | ShutdownOutcome::Expired => {
                    confirmation::resume_cancellation(state, job)?;
                    continue;
                }
                outcome => {
                    return Err(invalid(format!(
                        "The terminal service did not accept the stop: {}. The update ended without requesting termination.",
                        shutdown_refusal(&outcome)
                    )));
                }
            }
        }
        for chat in ending {
            if completed.is_empty() {
                confirmation::prepared(state, job)?;
            }
            let snapshot = match source.close_chat(daemon, chat.clone()) {
                Ok(snapshot) => snapshot,
                Err(error) if completed.is_empty() => {
                    // Attachment can reveal a descriptor change after our last
                    // inventory read. It has not authorized any close: renew the
                    // same confirmation flow when its exact list changed.
                    let observed = source.observe()?;
                    let (current, current_list, _) = reviewed_inventory(source, observed, service)?;
                    if !fresh.preserves_stop_consent(&current)
                        || listed.digest() != current_list.digest()
                    {
                        confirmation::resume_cancellation(state, job)?;
                        continue 'review;
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            completed.extend(
                listed
                    .items
                    .iter()
                    .filter(|item| item.id == chat.id)
                    .cloned(),
            );
            let ended_items: Vec<_> = listed
                .items
                .iter()
                .filter(|item| item.id == chat.id)
                .cloned()
                .collect();
            *background_tasks += daemon
                .sessions
                .iter()
                .find(|session| {
                    session.id == chat.id && session.kind == impact::SessionKind::NativeChat
                })
                .map_or(0, |session| session.interrupted_subagents.len() as u64);
            source.ended(&ended_items, false, *background_tasks);
            let mut ended = record.clone();
            ended.chats.retain(|source| source.id == chat.id);
            ended.aliases.retain(|source| source.id == chat.id);
            ended
                .presentations
                .retain(|pane| pane.session.id == chat.id);
            let retained = ended
                .acknowledge_snapshot(&chat, snapshot)
                .map_err(invalid)
                .and_then(|()| source.retain(daemon, ended));
            if let Err(error) = retained {
                for item in completed.iter_mut().filter(|item| item.id == chat.id) {
                    recovery_not_confirmed(item);
                }
                source.ended(completed, false, *background_tasks);
                return Err(error);
            }
        }
        let current = source.observe()?;
        let daemon = current
            .daemon
            .as_ref()
            .ok_or_else(|| invalid("The source disappeared after ending conversations"))?;
        if daemon
            .sessions
            .iter()
            .any(|item| item.disposition == impact::Disposition::Interrupted)
        {
            return Err(invalid(
                "A conversation started working again; review the update before preparing it",
            ));
        }
        // No prompt or new destructive scope is permitted after the first end.
        source.begin_preparation(state, job, daemon)?;
        if completed.is_empty() {
            confirmation::prepared(state, job)?;
        }
        source.prepare(daemon)?;
        return source.stopped(state, job, false);
    }
}

fn settle_interrupts(
    state: &Path,
    job: &mut UpdateJob,
    preview: impact::Preview,
    source: &mut impl FloorSource,
    timeout: Duration,
) -> Result<impact::Preview, UpdateError> {
    let Some(daemon) = preview.daemon.as_ref() else {
        return Ok(preview);
    };
    let interrupted: BTreeMap<_, _> = daemon
        .sessions
        .iter()
        .filter(|item| item.disposition == impact::Disposition::Interrupted)
        .map(|item| {
            let label = preview.labels.names.get(&item.id).unwrap_or(&item.title);
            (item.id, label.clone())
        })
        .collect();
    if interrupted.is_empty() {
        return Ok(preview);
    }
    confirmation::check_cancelled(state, job)?;
    let deadline = Instant::now() + timeout;
    let timeout_error = |pending: &[TerminalId]| {
        let names: Vec<_> = if pending.is_empty() {
            interrupted.values().map(String::as_str).collect()
        } else {
            pending
                .iter()
                .filter_map(|id| interrupted.get(id).map(String::as_str))
                .collect()
        };
        invalid(format!(
            "{} did not stop in time; nothing was changed in the installation. The source daemon and panes are still running.",
            names.join(", ")
        ))
    };
    let mut pending: Vec<_> = interrupted.keys().copied().collect();
    set_phase(
        state,
        job,
        UpdatePhase::ClosingWindows,
        "Waiting for the confirmed interruptions before preparing the update",
    )?;
    eprintln!(
        "Waiting for these conversations to stop: {}",
        interrupted.values().cloned().collect::<Vec<_>>().join(", ")
    );
    if let Err(error) = source.begin_interrupts(daemon, deadline) {
        return Err(if Instant::now() >= deadline {
            timeout_error(&pending)
        } else {
            error
        });
    }
    loop {
        confirmation::check_cancelled(state, job)?;
        if Instant::now() >= deadline {
            return Err(timeout_error(&pending));
        }
        pending = match source.pending_interrupts() {
            Ok(pending) => pending,
            Err(error) => {
                return Err(if Instant::now() >= deadline {
                    timeout_error(&pending)
                } else {
                    error
                });
            }
        };
        confirmation::check_cancelled(state, job)?;
        if Instant::now() >= deadline {
            return Err(timeout_error(&pending));
        }
        if pending.is_empty() {
            // The interrupt observer obtains this final idle capability with
            // the same deadline. An independent observation here could extend
            // the bound or allow Interrupt preparation of newly busy work.
            let mut current = preview.clone();
            current.daemon = Some(source.interrupt_impact()?);
            let mut expected = preview.clone();
            let idle = expected.daemon.as_mut().unwrap();
            idle.policy = impact::Policy::WhenIdle;
            for item in &mut idle.sessions {
                if interrupted.contains_key(&item.id) {
                    item.disposition = impact::Disposition::Resumes;
                }
            }
            confirmation::check_cancelled(state, job)?;
            if !expected.preserves_consent(&current)
                || current.daemon.as_ref().is_none_or(|daemon| {
                    daemon.policy != impact::Policy::WhenIdle
                        || daemon.sessions.iter().any(|item| {
                            interrupted.contains_key(&item.id)
                                && item.disposition != impact::Disposition::Resumes
                        })
                })
            {
                return Err(invalid(
                    "Conversations changed while stopping their turns; review the update again. The source daemon is still running.",
                ));
            }
            return Ok(current);
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn floor_record_helper(
    guard: &crate::instance::UiUpdateGuard,
    mut record: crate::upgrade::floor::Record,
    capture: bool,
) -> Result<File, UpdateError> {
    let helper = crate::companion::Product::Daemon.beside(&std::env::current_exe()?)?;
    let mut file = anonymous_file()?;
    file.write_all(&record.encode_capture().map_err(invalid)?)?;
    file.seek(SeekFrom::Start(0))?;
    seal_file(&file)?;
    let fd = file.as_raw_fd();
    let mut command = Command::new(helper);
    crate::install_guard::sanitize_child_environment(&mut command);
    command
        .arg(if capture {
            "--capture-update-floor"
        } else {
            "--retain-update-floor"
        })
        .arg(format!("/proc/self/fd/{fd}"));
    candidate::inherit(&mut command, fd);
    guard.inherit_for_install(&mut command)?;
    bounded_command(
        &mut command,
        "Retaining stopped update panes",
        8 * 1024 * 1024,
        Duration::from_secs(30),
        &CancellationToken::new(),
    )
}

fn capture_floor(
    guard: &crate::instance::UiUpdateGuard,
    record: crate::upgrade::floor::Record,
) -> Result<crate::upgrade::floor::Record, UpdateError> {
    let output = floor_record_helper(guard, record, true)?;
    Ok(serde_json::from_reader(output)?)
}
fn retain_floor(
    guard: &crate::instance::UiUpdateGuard,
    record: crate::upgrade::floor::Record,
) -> Result<(), UpdateError> {
    floor_record_helper(guard, record, false).map(|_| ())
}

fn archive_checkpoint() -> Result<(), UpdateError> {
    if let Some(store) = crate::upgrade::handoff::Store::existing().map_err(invalid)? {
        store.archive_floor().map_err(invalid)?;
    }
    Ok(())
}

/// Capture process epochs and private session associations before teardown.
/// The report rescans those associations after roots exit, so jobs forked during
/// shutdown are still visible. Shared user/application cgroups grant no link.
#[derive(Default)]
pub(super) struct Survivors {
    ended: BTreeMap<(impact::SessionKind, TerminalId), super::floor_display::StopItem>,
    service_stopped: bool,
    source_prepared: bool,
    prepared_source: Option<PreparationEvidence>,
    captured_preparation: Option<crate::upgrade::recovery::CapturedPreparation>,
    recovery_capture_error: Option<String>,
    receipt: Option<crate::upgrade::recovery::Receipt>,
    recovery_outcome: Option<crate::upgrade::recovery::RecoveryOutcome>,
    interrupted: BTreeMap<(impact::SessionKind, TerminalId), InterruptedTurn>,
    unconfirmed_interrupts: BTreeMap<(impact::SessionKind, TerminalId), String>,
    late_turns: BTreeSet<TerminalId>,
    background_tasks: u64,
    processes: BTreeMap<(i32, u64), Survivor>,
    labels: BTreeMap<TerminalId, String>,
    origins: BTreeMap<TerminalId, TerminalId>,
    scopes: BTreeMap<String, (TerminalId, String)>,
    ambient_scopes: BTreeSet<String>,
    markers: BTreeMap<Vec<u8>, (TerminalId, String)>,
}

enum PreparationEvidence {
    Full {
        daemon: impact::DaemonImpact,
        census: crate::upgrade::recovery::Census,
    },
    ScopeOnly(impact::DaemonImpact),
}

impl PreparationEvidence {
    fn census(&self) -> Option<&crate::upgrade::recovery::Census> {
        match self {
            Self::Full { census, .. } => Some(census),
            Self::ScopeOnly(_) => None,
        }
    }

    fn require_ready(&self) -> Result<(), String> {
        match self {
            Self::Full { daemon, census } => {
                crate::upgrade::recovery::require_ready(daemon, census)
            }
            Self::ScopeOnly(daemon) => crate::upgrade::recovery::require_ready_scope(daemon),
        }
    }
}

struct InterruptedTurn {
    label: String,
    recovery: String,
    background_tasks: BTreeSet<String>,
}

impl Survivors {
    /// Run only after the original daemon exited and its exact lease was
    /// acquired. A foreign or changed Prepared checkpoint never authorizes a
    /// normal startup, even if the original prepare was acknowledged.
    pub(super) fn authorize_successor(
        &mut self,
        target_daemon: &Path,
        verified_digest: Option<&str>,
        handoff: Option<&crate::daemon::lease_handoff::LeaseHandoff>,
    ) -> Result<(), UpdateError> {
        if !self.source_prepared {
            return Ok(());
        }
        let evidence = self
            .prepared_source
            .as_ref()
            .ok_or_else(|| invalid("The acknowledged source has no captured preparation census"))?;
        let handoff = handoff.ok_or_else(|| {
            invalid("The acknowledged source has not released its daemon lease for recovery")
        })?;
        if matches!(evidence, PreparationEvidence::ScopeOnly(_)) {
            return evidence.require_ready().map_err(invalid);
        }
        if let Some(captured) = self.captured_preparation.take() {
            let receipt = match verified_digest {
                Some(digest) => captured.receipt_with_digest(target_daemon, digest, handoff),
                None => captured.receipt(target_daemon, handoff),
            };
            match receipt {
                Ok(receipt) => {
                    self.receipt = receipt;
                    return Ok(());
                }
                Err(error) => self.recovery_capture_error = Some(error),
            }
        }
        evidence.require_ready().map_err(|error| {
            invalid(match &self.recovery_capture_error {
                Some(capture) => format!("{error}. Automatic recovery was refused: {capture}"),
                None => error,
            })
        })
    }

    pub(super) fn inherit_recovery(&self, command: &mut Command) -> Result<(), UpdateError> {
        if let Some(receipt) = &self.receipt {
            receipt.inherit(command)?;
        }
        Ok(())
    }

    pub(super) fn verify_successor(&mut self, target_daemon: &Path) -> Result<(), UpdateError> {
        self.verify_successor_inner(target_daemon, false)
    }

    /// Mechanical completion after the root recovery verifier has established
    /// a fresh usable census. Core inventory counts alone do not authorize it.
    /// No subsequent startup may receive the spent receipt.
    pub(super) fn archive_recovered_successor(&mut self) -> Result<(), UpdateError> {
        let Some(receipt) = &self.receipt else {
            return Ok(());
        };
        let target = receipt.target().to_owned();
        self.verify_successor_inner(&target, true)?;
        self.receipt = None;
        Ok(())
    }

    fn verify_successor_inner(
        &mut self,
        target_daemon: &Path,
        archive: bool,
    ) -> Result<(), UpdateError> {
        if crate::daemon::running_executable()
            .map_err(|error| invalid(error.to_string()))?
            .as_deref()
            != Some(target_daemon)
        {
            return Err(invalid(
                "The terminal service does not belong to the confirmed installation; its saved panes and recovery state were retained. Check cagents daemon status before retrying",
            ));
        }
        let socket = crate::terminal_ipc::daemon_socket_path()?;
        let source =
            source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
        let source::Client::Current { mut client, peer } = source else {
            return Err(invalid(
                "The confirmed successor cannot verify its recovery inventory",
            ));
        };
        if client.protocol() != crate::terminal_ipc::PROTOCOL_VERSION {
            return Err(invalid(
                "The confirmed successor does not serve this update's matched protocol",
            ));
        }
        peer.verify(client.proof())?;
        if client
            .startup_state()
            .map_err(|error| invalid(error.to_string()))?
            != crate::terminal_ipc::StartupState::Ready
        {
            return Err(invalid(
                "The confirmed terminal service has not finished recovering its saved panes",
            ));
        }
        if let Some(receipt) = &self.receipt {
            let terminals = client.list().map_err(|error| invalid(error.to_string()))?;
            let chats = client
                .list_chats()
                .map_err(|error| invalid(error.to_string()))?;
            peer.verify(client.proof())?;
            let outcome = if archive {
                receipt.archive_verified(client.proof(), &terminals, &chats)
            } else {
                receipt.verify(client.proof(), &terminals, &chats)
            };
            self.recovery_outcome = Some(outcome.map_err(invalid)?);
        }
        peer.verify(client.proof())?;
        Ok(())
    }

    /// Failure cleanup cannot fall through to the ordinary checkpoint replay
    /// after an accepted preparation. It may keep a verified serving daemon or
    /// retain Prepared evidence for explicit recovery.
    pub(super) fn require_normal_restart(&self) -> Result<(), UpdateError> {
        if !self.source_prepared {
            return Ok(());
        }
        let evidence = self
            .prepared_source
            .as_ref()
            .ok_or_else(|| invalid("The acknowledged source has no captured preparation census"))?;
        evidence.require_ready().map_err(invalid)
    }

    pub(super) fn paused_report(&self) -> String {
        let Some(outcome) = &self.recovery_outcome else {
            return String::new();
        };
        if outcome.paused_chats.is_empty() && outcome.paused_terminals.is_empty() {
            return String::new();
        }
        let mut report = String::from("\nPaused after update recovery:\n");
        for id in &outcome.paused_chats {
            let label = self
                .labels
                .get(id)
                .map(String::as_str)
                .unwrap_or("Conversation");
            let recorded = self
                .prepared_source
                .as_ref()
                .and_then(PreparationEvidence::census)
                .is_some_and(|census| {
                    census.chats.iter().any(|chat| {
                        chat.id == *id
                            && chat
                                .launch
                                .session_id
                                .as_ref()
                                .is_some_and(|id| !id.trim().is_empty())
                    })
                });
            let action = if recorded {
                "choose Resume conversation in the new tab (+) menu to continue"
            } else {
                "start a new conversation to continue"
            };
            report.push_str(&format!("  {label}: {action}\n"));
        }
        for id in &outcome.paused_terminals {
            let label = self
                .labels
                .get(id)
                .map(String::as_str)
                .unwrap_or("Terminal");
            report.push_str(&format!("  {label}: reopen a terminal to continue\n"));
        }
        report
    }

    fn record_interrupt_requests(
        &mut self,
        reviewed: &impact::Preview,
        daemon: &impact::DaemonImpact,
    ) {
        for session in &daemon.sessions {
            if session.disposition == impact::Disposition::Interrupted {
                self.unconfirmed_interrupts.insert(
                    (session.kind, session.id),
                    reviewed
                        .labels
                        .names
                        .get(&session.id)
                        .unwrap_or(&session.title)
                        .clone(),
                );
            }
        }
    }
    /// Call only after the source accepts preparation. An interrupted turn is
    /// different from a whole chat ended by the floor, so retain that scope in
    /// its report rather than describing the conversation itself as stopped.
    fn record_prepared(&mut self, reviewed: &impact::Preview, daemon: &impact::DaemonImpact) {
        self.source_prepared = true;
        self.record_settled_interruptions(reviewed, daemon, &[]);
    }

    fn record_settled_interruptions(
        &mut self,
        reviewed: &impact::Preview,
        daemon: &impact::DaemonImpact,
        pending: &[TerminalId],
    ) {
        for session in &daemon.sessions {
            let key = (session.kind, session.id);
            if session.disposition != impact::Disposition::Interrupted
                || self.ended.contains_key(&key)
                || pending.contains(&session.id)
            {
                continue;
            }
            let label = reviewed
                .labels
                .names
                .get(&session.id)
                .unwrap_or(&session.title)
                .clone();
            self.unconfirmed_interrupts.remove(&key);
            let recovery = reviewed
                .stopped
                .iter()
                .find(|pane| pane.pane == session.id)
                .map_or_else(
                    || "its saved conversation was kept".to_owned(),
                    |pane| format!("after the update: {}. {}", pane.reason, pane.recovery),
                );
            self.interrupted
                .entry(key)
                .or_insert_with(|| InterruptedTurn {
                    label,
                    recovery,
                    background_tasks: session.interrupted_subagents.iter().cloned().collect(),
                });
        }
    }

    pub(super) fn prepared_report(&self) -> String {
        let turns: Vec<_> = self
            .interrupted
            .iter()
            .filter(|(key, _)| !self.ended.contains_key(*key))
            .collect();
        if turns.is_empty() && self.unconfirmed_interrupts.is_empty() {
            return String::new();
        }
        let mut report = String::new();
        if !turns.is_empty() {
            report.push_str("\nCurrent turns interrupted:\n");
        }
        for ((_, id), turn) in &turns {
            let recovery = if self
                .recovery_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.paused_chats.contains(id))
            {
                "its conversation is paused after recovery"
            } else {
                &turn.recovery
            };
            report.push_str(&format!(
                "  {}: current turn interrupted; {}\n",
                turn.label, recovery
            ));
        }
        for ((_, id), turn) in &self.interrupted {
            if self.late_turns.contains(id) {
                report.push_str(&format!(
                    "  {}: also stopped a turn that started during review\n",
                    turn.label
                ));
            }
        }
        let background_tasks: usize = turns
            .iter()
            .map(|(_, turn)| turn.background_tasks.len())
            .sum();
        report.push_str(&background_recovery(background_tasks as u64));
        if !self.unconfirmed_interrupts.is_empty() {
            report.push_str("\nInterruption was not confirmed:\n");
            for (key, label) in &self.unconfirmed_interrupts {
                if !self.ended.contains_key(key) {
                    report.push_str(&format!(
                        "  {label}: check its current turn in the running daemon\n"
                    ));
                }
            }
        }
        report
    }

    pub(super) fn needs_daemon_recovery(&self) -> bool {
        self.service_stopped || self.source_prepared
    }

    pub(super) fn recovery_report(&self) -> String {
        let items: Vec<_> = self.ended.values().cloned().collect();
        if items.is_empty() && !self.service_stopped && self.background_tasks == 0 {
            return String::new();
        }
        completed_error(
            invalid(""),
            &items,
            self.service_stopped,
            self.background_tasks,
        )
        .to_string()
    }
}

type Process = crate::daemon::holder_registry::Process;

#[derive(Clone)]
struct Survivor {
    process: Process,
    id: TerminalId,
    description: String,
}

struct ProcessRow {
    process: Process,
    parent: i32,
    scope: Option<String>,
    environment: Vec<u8>,
}

/// Only a unique cagents session scope, including its provider/command leaves,
/// may associate processes after their common parent has gone. Matching a
/// user's app.slice or shell scope would include unrelated work.
fn session_scope(contents: &str) -> Option<String> {
    let membership = contents.lines().find_map(|line| line.strip_prefix("0::"))?;
    if !membership.starts_with('/') {
        return None;
    }
    let mut length = 0;
    for component in membership.split('/') {
        if !component.is_empty() {
            length += component.len() + 1;
            let Some(identity) = component
                .strip_prefix("cagents-session-")
                .and_then(|value| value.strip_suffix(".scope"))
            else {
                continue;
            };
            uuid::Uuid::parse_str(identity).ok()?;
            return membership.get(..length).map(str::to_owned);
        }
    }
    None
}

fn bounded_proc_file(path: &Path) -> Vec<u8> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file
        .take(METADATA_LIMIT + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > METADATA_LIMIT
    {
        return Vec::new();
    }
    bytes
}

fn epoch_current(proc_root: &Path, process: &Process) -> bool {
    let bytes = bounded_proc_file(&proc_root.join(process.pid.to_string()).join("stat"));
    crate::model::agent_recovery::process_identity(&bytes)
        .is_some_and(|(_, start)| start == process.start)
}

fn process_rows(root: &Path, uid: u32) -> io::Result<Vec<ProcessRow>> {
    let mut rows = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        if !entry.metadata().is_ok_and(|metadata| metadata.uid() == uid) {
            continue;
        }
        let bytes = bounded_proc_file(&entry.path().join("stat"));
        let Ok(stat) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let Some((_, tail)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = tail.split_whitespace().collect();
        if matches!(fields.first(), Some(&"Z" | &"X")) {
            continue;
        }
        let Some(parent) = fields.get(1).and_then(|value| value.parse::<i32>().ok()) else {
            continue;
        };
        let Some(start) = fields.get(19).and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let cgroup = bounded_proc_file(&entry.path().join("cgroup"));
        rows.push(ProcessRow {
            process: Process { pid, start },
            parent,
            scope: std::str::from_utf8(&cgroup).ok().and_then(session_scope),
            environment: bounded_proc_file(&entry.path().join("environ")),
        });
    }
    Ok(rows)
}

impl Survivors {
    fn remember_origin(&mut self, id: TerminalId, origin: TerminalId) {
        self.origins.insert(id, origin);
    }

    fn exclude_ambient(&mut self, proc_root: &Path, pid: u32) {
        let bytes = bounded_proc_file(&proc_root.join(pid.to_string()).join("cgroup"));
        if let Some(scope) = std::str::from_utf8(&bytes).ok().and_then(session_scope) {
            self.scopes.remove(&scope);
            self.ambient_scopes.insert(scope);
        }
    }

    fn remember_list(&mut self, listed: &StopList) {
        for item in &listed.items {
            self.labels.insert(item.id, item.label.clone());
            self.markers.insert(
                format!("CAGENTS_TERMINAL_ID={}", item.id).into_bytes(),
                (item.id, item.label.clone()),
            );
            let origin = self.origins.get(&item.id).copied().unwrap_or(item.id);
            self.markers.insert(
                format!("CAGENTS_RECOVERY_ORIGIN={origin}").into_bytes(),
                (item.id, item.label.clone()),
            );
        }
    }

    fn capture(&mut self, listed: &StopList, daemon: Option<Process>) -> Result<(), UpdateError> {
        use crate::daemon::holder_registry::{self, Kind};
        self.remember_list(listed);
        let mut roots = BTreeMap::new();
        if let Some(process) = daemon {
            roots.insert(
                process.pid,
                (
                    process,
                    TerminalId::from_uuid(uuid::Uuid::nil()),
                    "terminal service".to_owned(),
                ),
            );
        }
        for kind in [Kind::Terminal, Kind::Native] {
            for record in holder_registry::observed(kind)? {
                if let Some(label) = self.labels.get(&record.terminal) {
                    for process in [record.holder, record.root] {
                        roots.insert(process.pid, (process, record.terminal, label.clone()));
                    }
                }
            }
        }
        self.capture_at(Path::new("/proc"), crate::daemon::current_uid(), roots)?;
        Ok(())
    }

    fn capture_at(
        &mut self,
        proc_root: &Path,
        uid: u32,
        roots: BTreeMap<i32, (Process, TerminalId, String)>,
    ) -> io::Result<()> {
        let rows = process_rows(proc_root, uid)?;
        let mut owners: BTreeMap<i32, (TerminalId, String)> = roots
            .into_iter()
            .filter_map(|(pid, (process, id, label))| {
                rows.iter()
                    .any(|row| row.process == process)
                    .then_some((pid, (id, label)))
            })
            .collect();
        for row in &rows {
            if let Some(owner) = self.association(row) {
                owners.entry(row.process.pid).or_insert(owner);
            }
        }
        loop {
            let mut changed = false;
            for row in &rows {
                if !owners.contains_key(&row.process.pid)
                    && let Some(owner) = owners.get(&row.parent).cloned()
                {
                    owners.insert(row.process.pid, owner);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for row in rows {
            if !epoch_current(proc_root, &row.process) {
                continue;
            }
            if let Some((id, label)) = owners.get(&row.process.pid) {
                if let Some(scope) = row.scope
                    && !self.ambient_scopes.contains(&scope)
                {
                    self.scopes
                        .entry(scope)
                        .or_insert_with(|| (*id, label.clone()));
                }
                self.remember_process(proc_root, row.process, *id, label);
            }
        }
        Ok(())
    }

    fn association(&self, row: &ProcessRow) -> Option<(TerminalId, String)> {
        row.scope
            .as_ref()
            .filter(|scope| !self.ambient_scopes.contains(*scope))
            .and_then(|scope| self.scopes.get(scope))
            .cloned()
            .or_else(|| {
                row.environment
                    .split(|byte| *byte == 0)
                    .find_map(|entry| self.markers.get(entry).cloned())
            })
    }

    fn remember_process(
        &mut self,
        proc_root: &Path,
        process: Process,
        id: TerminalId,
        label: &str,
    ) {
        let bytes = bounded_proc_file(&proc_root.join(process.pid.to_string()).join("cmdline"));
        let command: String = String::from_utf8_lossy(&bytes)
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        if !epoch_current(proc_root, &process) {
            return;
        }
        self.processes.insert(
            (process.pid, process.start),
            Survivor {
                id,
                description: format!("  {label}: {} (pid {})", command.trim(), process.pid),
                process,
            },
        );
    }

    fn live_at(&self, proc_root: &Path, uid: u32) -> io::Result<Vec<Survivor>> {
        let rows = process_rows(proc_root, uid)?;
        let mut live = self.processes.clone();
        live.retain(|_, survivor| {
            rows.iter().any(|row| row.process == survivor.process)
                && epoch_current(proc_root, &survivor.process)
        });
        // Reparenting does not change cgroup membership or inherited markers.
        // Discover late forks after the source/helper exited, pinning each new
        // epoch rather than carrying a numeric PID from the earlier census.
        let mut discovered = Self::default();
        for row in rows {
            if let Some((id, label)) = self.association(&row) {
                discovered.remember_process(proc_root, row.process, id, &label);
            }
        }
        live.extend(discovered.processes);
        Ok(live.into_values().collect())
    }

    fn unattached_at(
        proc_root: &Path,
        live: Vec<Survivor>,
        attached: &BTreeSet<TerminalId>,
    ) -> Vec<Survivor> {
        live.into_iter()
            .filter(|survivor| {
                if !epoch_current(proc_root, &survivor.process) || attached.contains(&survivor.id) {
                    return false;
                }
                // A restarted terminal inherits its recovery origin. Its current
                // terminal marker can name a new, attached destination allocation;
                // origin evidence alone must not hide genuinely detached old work.
                let environment = bounded_proc_file(
                    &proc_root
                        .join(survivor.process.pid.to_string())
                        .join("environ"),
                );
                let currently_attached = environment
                    .split(|byte| *byte == 0)
                    .filter_map(|entry| {
                        let value =
                            std::str::from_utf8(entry.strip_prefix(b"CAGENTS_TERMINAL_ID=")?)
                                .ok()?;
                        let id: TerminalId = value.parse().ok()?;
                        (id.to_string() == value).then_some(id)
                    })
                    .any(|id| attached.contains(&id));
                // Reading proc environment is not an epoch pin: discard the old
                // observation if this numeric PID was reused during inspection.
                epoch_current(proc_root, &survivor.process) && !currently_attached
            })
            .collect()
    }

    pub(super) fn report(&self) -> (Vec<String>, Vec<String>) {
        if self.processes.is_empty() && self.scopes.is_empty() && self.markers.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let live = match self.live_at(Path::new("/proc"), crate::daemon::current_uid()) {
            Ok(live) => live,
            Err(_) => {
                return (
                    Vec::new(),
                    vec!["Surviving work could not be inspected".into()],
                );
            }
        };
        if live.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let attached = (|| -> Result<BTreeSet<TerminalId>, UpdateError> {
            let socket = crate::terminal_ipc::daemon_socket_path()?;
            let mut client =
                source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
            let proof = client.proof().clone();
            let mut ids: BTreeSet<_> = client
                .list_chats()?
                .into_iter()
                .map(|chat| chat.id)
                .collect();
            ids.extend(
                client
                    .list()?
                    .into_iter()
                    .filter(|terminal| terminal.session.continuity.can_resume_live(&proof))
                    .map(|terminal| terminal.session.id),
            );
            Ok(ids)
        })();
        match attached {
            Ok(attached) => (
                Self::unattached_at(Path::new("/proc"), live, &attached)
                    .into_iter()
                    .map(|survivor| survivor.description)
                    .collect(),
                Vec::new(),
            ),
            Err(_) => (
                Vec::new(),
                live.into_iter()
                    .filter(|survivor| epoch_current(Path::new("/proc"), &survivor.process))
                    .map(|survivor| survivor.description)
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests;
