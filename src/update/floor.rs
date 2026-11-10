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
        let label = preview.labels.get(&item.id).unwrap_or(&item.title);
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
            let label = preview.labels.get(&chat.id).unwrap_or(&fallback);
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
    Ok(StopList {
        from_version: preview.from_version.clone(),
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
    fn prepare(&mut self, daemon: &impact::DaemonImpact) -> Result<(), UpdateError>;
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
    fn ended(&mut self, _items: &[super::floor_display::StopItem], _service: bool) {}
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
    guard: &'a crate::instance::UiUpdateGuard,
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
    fn prepare(&mut self, daemon: &impact::DaemonImpact) -> Result<(), UpdateError> {
        source_client_for(daemon)?
            .prepare_confirmed_upgrade(daemon.clone(), false)
            .map(|_| ())
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
    fn ended(&mut self, items: &[super::floor_display::StopItem], service: bool) {
        for item in items {
            self.survivors
                .ended
                .insert((item.kind, item.id), item.clone());
        }
        self.survivors.service_stopped |= service;
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
        guard,
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

fn completed_error(
    error: UpdateError,
    completed: &[super::floor_display::StopItem],
    service: bool,
) -> UpdateError {
    if completed.is_empty() && !service {
        return error;
    }
    let mut message = format!("{error}\n");
    if service {
        message.push_str("The terminal service accepted the stop.\n");
    }
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
    let result = quiesce_inner(
        state,
        job,
        confirmed,
        terminal,
        source,
        &mut completed,
        &mut service_stopped,
    );
    result.map_err(|error| completed_error(error, &completed, service_stopped))
}

fn quiesce_inner(
    state: &Path,
    job: &mut UpdateJob,
    confirmed: &impact::Preview,
    terminal: bool,
    source: &mut impl FloorSource,
    completed: &mut Vec<super::floor_display::StopItem>,
    service_stopped: &mut bool,
) -> Result<(), UpdateError> {
    let Some(initial) = &confirmed.daemon else {
        return Ok(());
    };
    let mut preview = source.observe()?;
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
    let daemon = preview
        .daemon
        .as_ref()
        .ok_or_else(|| invalid("The source disappeared during update review"))?;
    let mut service = needs_service_stop(daemon);
    // With no known ends, prepare itself determines whether the service can
    // checkpoint. A refusal has not ended anything and precedes the one stop list.
    if !daemon.blocked() {
        match source.prepare(daemon) {
            Ok(()) => return source.stopped(state, job, false),
            Err(error) => {
                eprintln!("The source could not checkpoint its work: {error}");
                service = true;
            }
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
            match source.shutdown(last.daemon.as_ref().unwrap(), capability)? {
                ShutdownOutcome::Accepted { .. } => {
                    *service_stopped = true;
                    completed.extend(listed.items.clone());
                    source.ended(&listed.items, true);
                    // Accepted invalidates the old checkpoint even when waiting
                    // for exit or writing the recovery record subsequently fails.
                    let archived = source.archive();
                    let retained = source.retain(daemon, record);
                    if retained.is_err() {
                        for item in completed.iter_mut() {
                            recovery_not_confirmed(item);
                        }
                        source.ended(completed, true);
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
                ShutdownOutcome::InventoryChanged { .. } | ShutdownOutcome::Expired => continue,
                outcome => {
                    return Err(invalid(format!(
                        "The terminal service did not accept the stop: {}. The update ended without requesting termination.",
                        shutdown_refusal(&outcome)
                    )));
                }
            }
        }
        for chat in ending {
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
            source.ended(&ended_items, false);
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
                source.ended(completed, false);
                return Err(error);
            }
        }
        let current = source.observe()?;
        let daemon = current
            .daemon
            .as_ref()
            .ok_or_else(|| invalid("The source disappeared after ending conversations"))?;
        // No prompt or new destructive scope is permitted after the first end.
        source.prepare(daemon)?;
        return source.stopped(state, job, false);
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
    processes: BTreeMap<(i32, u64), Survivor>,
    labels: BTreeMap<TerminalId, String>,
    origins: BTreeMap<TerminalId, TerminalId>,
    scopes: BTreeMap<String, (TerminalId, String)>,
    ambient_scopes: BTreeSet<String>,
    markers: BTreeMap<Vec<u8>, (TerminalId, String)>,
}

impl Survivors {
    pub(super) fn recovery_report(&self) -> String {
        let items: Vec<_> = self.ended.values().cloned().collect();
        if items.is_empty() && !self.service_stopped {
            return String::new();
        }
        completed_error(invalid(""), &items, self.service_stopped).to_string()
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
