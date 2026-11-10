//! Candidate-owned fallback for source daemons that cannot checkpoint work.
use super::floor_display::{StopList, chat_item_with_state, terminal_item};
use super::*;
use crate::model::{TerminalId, TerminalSession};
use crate::terminal_ipc::{ShutdownCapability, TerminalDescriptor};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const TERMINAL_CONFIRMATION: &str =
    "needs confirmation in a terminal: run `cagents daemon upgrade`";

fn needs_service_stop(daemon: &impact::DaemonImpact) -> bool {
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
                    items.push(terminal_item(terminal, item, label));
                }
            }
            _ => (),
        }
    }
    if service
        && (inventory.chats.iter().any(|chat| {
            !items
                .iter()
                .any(|item| item.id == chat.id && item.kind == impact::SessionKind::NativeChat)
        }) || inventory
            .terminals
            .iter()
            .filter(|terminal| {
                terminal
                    .session
                    .continuity
                    .can_resume_live(&daemon.capability.proof)
                    && !inventory
                        .chats
                        .iter()
                        .any(|chat| chat.id == terminal.session.id)
            })
            .any(|terminal| {
                !items.iter().any(|item| {
                    item.id == terminal.session.id && item.kind == impact::SessionKind::Terminal
                })
            }))
    {
        return Err(invalid(
            "The source inventory changed during stop review; review again",
        ));
    }
    items.sort_by_key(|item| (item.kind, item.id));
    Ok(StopList {
        from_version: preview.from_version.clone(),
        items,
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
    fn close_chats(
        &mut self,
        daemon: &impact::DaemonImpact,
        ending: Vec<crate::chat::Session>,
    ) -> Result<Vec<TerminalId>, UpdateError>;
    fn preflight(
        &mut self,
        daemon: &impact::DaemonImpact,
    ) -> Result<ShutdownCapability, UpdateError>;
    fn shutdown(
        &mut self,
        daemon: &impact::DaemonImpact,
        capability: ShutdownCapability,
    ) -> Result<(), UpdateError>;
    fn retain(
        &mut self,
        daemon: &impact::DaemonImpact,
        chats: Vec<crate::chat::Session>,
        terminals: Vec<TerminalId>,
        service_stopped: bool,
    ) -> Result<(), UpdateError>;
    fn capture(&mut self, list: &StopList) -> Result<(), UpdateError>;
    fn capture_service(
        &mut self,
        _daemon: &impact::DaemonImpact,
        list: &StopList,
    ) -> Result<(), UpdateError> {
        self.capture(list)
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
        service: bool,
    ) -> Result<Inventory, UpdateError> {
        let mut client = source_client_for(daemon)?;
        let chats = client.list_chats()?;
        let chat_states = chats
            .iter()
            .map(|chat| {
                (
                    chat.id,
                    client
                        .chat_state(chat.clone())
                        .unwrap_or("state unavailable")
                        .to_owned(),
                )
            })
            .collect();
        Ok(Inventory {
            chats,
            chat_states,
            terminals: if service { client.list()? } else { Vec::new() },
        })
    }
    fn prepare(&mut self, daemon: &impact::DaemonImpact) -> Result<(), UpdateError> {
        source_client_for(daemon)?
            .prepare_confirmed_upgrade(daemon.clone(), false)
            .map(|_| ())
    }
    fn close_chats(
        &mut self,
        daemon: &impact::DaemonImpact,
        ending: Vec<crate::chat::Session>,
    ) -> Result<Vec<TerminalId>, UpdateError> {
        end_source_chats(daemon, ending)
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
    ) -> Result<(), UpdateError> {
        source_client_for(daemon)?.confirm_shutdown(capability)
    }
    fn retain(
        &mut self,
        daemon: &impact::DaemonImpact,
        chats: Vec<crate::chat::Session>,
        terminals: Vec<TerminalId>,
        service_stopped: bool,
    ) -> Result<(), UpdateError> {
        let mut record = crate::upgrade::floor::Record::new(self.proof.clone(), chats, terminals);
        record.protocol = source_client_for(daemon)?.protocol();
        record.service_stopped = service_stopped;
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
    fn stopped(
        &mut self,
        state: &Path,
        job: &mut UpdateJob,
        service: bool,
    ) -> Result<(), UpdateError> {
        let pinned = self
            .pinned
            .take()
            .ok_or_else(|| invalid("The source lease was already handed over"))?;
        *self.handoff = Some(wait_daemon_stopped(self.paths.clone(), pinned, state, job)?);
        if service {
            archive_checkpoint()?;
        }
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

fn same_after_our_closes(
    before: &StopList,
    previous: &Inventory,
    after: &StopList,
    current: &Inventory,
    closed: &BTreeSet<TerminalId>,
) -> bool {
    let mut remaining = before.clone();
    remaining.items.retain(|item| {
        !(item.kind == impact::SessionKind::NativeChat && closed.contains(&item.id))
    });
    let mut renewed = after.clone();
    for item in &mut renewed.items {
        let Some(old) = remaining
            .items
            .iter()
            .find(|old| old.id == item.id && old.kind == item.kind)
        else {
            return false;
        };
        let unchanged = match item.kind {
            impact::SessionKind::NativeChat => {
                previous.chats.iter().find(|chat| chat.id == item.id)
                    == current.chats.iter().find(|chat| chat.id == item.id)
            }
            impact::SessionKind::Terminal => {
                let session = |inventory: &Inventory| -> Option<TerminalSession> {
                    inventory
                        .terminals
                        .iter()
                        .find(|terminal| terminal.session.id == item.id)
                        .map(|terminal| terminal.session.clone())
                };
                session(previous) == session(current)
            }
        };
        if !unchanged {
            return false;
        }
        // Old sources put their *global* generation in every impact identity.
        // Our confirmed ChatClose renews all those hashes. Exact unchanged
        // source descriptors prove that no additional owner was authorized.
        if !closed.is_empty() {
            item.identity.clone_from(&old.identity);
        }
    }
    remaining.digest() == renewed.digest()
}

fn quiesce_source(
    state: &Path,
    job: &mut UpdateJob,
    confirmed: &impact::Preview,
    terminal: bool,
    source: &mut impl FloorSource,
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
    let mut force_service = false;
    loop {
        let daemon = preview
            .daemon
            .as_ref()
            .ok_or_else(|| invalid("The source disappeared during update review"))?;
        if daemon.capability.proof != initial.capability.proof {
            return Err(invalid("The source changed during update review"));
        }
        if daemon.blocked() || force_service {
            if !terminal {
                return Err(invalid(TERMINAL_CONFIRMATION));
            }
            let service = force_service || needs_service_stop(daemon);
            let (reviewed, listed) = confirmation::wait_stop(state, job, || {
                let current = source.observe()?;
                if current
                    .daemon
                    .as_ref()
                    .is_none_or(|daemon| daemon.capability.proof != initial.capability.proof)
                {
                    return Err(invalid("The source changed during stop review"));
                }
                let (preview, list, _) = reviewed_inventory(source, current, service)?;
                Ok((preview, list))
            })?;
            let (latest, latest_list, inventory) = {
                let current = source.observe()?;
                reviewed_inventory(source, current, service)?
            };
            if !reviewed.preserves_consent(&latest) || listed.digest() != latest_list.digest() {
                preview = latest;
                force_service = service;
                continue;
            }
            let daemon = latest.daemon.as_ref().unwrap();
            if service {
                source.capture_service(daemon, &listed)?;
            } else {
                source.capture(&listed)?;
            }
            let ending: Vec<_> = inventory
                .chats
                .iter()
                .filter(|chat| {
                    daemon.sessions.iter().any(|item| {
                        item.id == chat.id
                            && item.kind == impact::SessionKind::NativeChat
                            && item.disposition == impact::Disposition::Blocked
                    })
                })
                .cloned()
                .collect();
            let expected: BTreeSet<_> = ending.iter().map(|chat| chat.id).collect();
            let closed: BTreeSet<_> = if ending.is_empty() {
                BTreeSet::new()
            } else {
                source.retain(daemon, ending.clone(), Vec::new(), false)?;
                let ending_list = StopList {
                    from_version: listed.from_version.clone(),
                    items: listed
                        .items
                        .iter()
                        .filter(|item| {
                            item.kind == impact::SessionKind::NativeChat
                                && expected.contains(&item.id)
                        })
                        .cloned()
                        .collect(),
                };
                source.capture(&ending_list)?;
                source.close_chats(daemon, ending)?.into_iter().collect()
            };
            let all_closed = closed == expected;
            preview = source.observe()?;
            if service {
                let (current, current_list, current_inventory) =
                    reviewed_inventory(source, preview, true)?;
                if current
                    .daemon
                    .as_ref()
                    .is_none_or(|daemon| daemon.capability.proof != initial.capability.proof)
                {
                    return Err(invalid(
                        "The source changed after its conversations were ended",
                    ));
                }
                if !same_after_our_closes(
                    &listed,
                    &inventory,
                    &current_list,
                    &current_inventory,
                    &closed,
                ) {
                    force_service = true;
                    preview = current;
                    continue;
                }
                let daemon = current.daemon.as_ref().unwrap();
                let capability = source.preflight(daemon)?;
                let (latest, latest_list, latest_inventory) = {
                    let observed = source.observe()?;
                    reviewed_inventory(source, observed, true)?
                };
                if !current.preserves_consent(&latest)
                    || current_list.digest() != latest_list.digest()
                {
                    force_service = true;
                    preview = latest;
                    continue;
                }
                let stopped_terminals = latest_inventory
                    .terminals
                    .iter()
                    .filter(|terminal| {
                        terminal
                            .session
                            .continuity
                            .can_resume_live(&daemon.capability.proof)
                    })
                    .map(|terminal| terminal.session.id)
                    .collect();
                source.retain(daemon, latest_inventory.chats, stopped_terminals, true)?;
                source.capture_service(daemon, &latest_list)?;
                source.shutdown(daemon, capability)?;
                return source.stopped(state, job, true);
            }
            force_service = !all_closed
                || preview
                    .daemon
                    .as_ref()
                    .is_some_and(impact::DaemonImpact::blocked);
            continue;
        }
        if daemon
            .sessions
            .iter()
            .any(|item| item.disposition == impact::Disposition::WaitsForIdle)
        {
            preview =
                confirmation::wait_approved(state, job, terminal, preview.clone(), |policy| {
                    source.observe_policy(policy)
                })?;
            continue;
        }
        match source.prepare(daemon) {
            Ok(()) => return source.stopped(state, job, false),
            Err(error) => {
                eprintln!("The source could not checkpoint its work: {error}");
                if !terminal {
                    return Err(invalid(TERMINAL_CONFIRMATION));
                }
                force_service = true;
                preview = source.observe()?;
            }
        }
    }
}

fn retain_floor(
    guard: &crate::instance::UiUpdateGuard,
    record: crate::upgrade::floor::Record,
) -> Result<(), UpdateError> {
    let helper = crate::companion::Product::Daemon.beside(&std::env::current_exe()?)?;
    let mut file = anonymous_file()?;
    serde_json::to_writer(&mut file, &record)?;
    file.seek(SeekFrom::Start(0))?;
    seal_file(&file)?;
    let fd = file.as_raw_fd();
    let mut command = Command::new(helper);
    crate::install_guard::sanitize_child_environment(&mut command);
    command
        .arg("--retain-update-floor")
        .arg(format!("/proc/self/fd/{fd}"));
    candidate::inherit(&mut command, fd);
    guard.inherit_for_install(&mut command)?;
    bounded_command(
        &mut command,
        "Retaining stopped update panes",
        SMALL_ASSET_LIMIT,
        Duration::from_secs(30),
        &CancellationToken::new(),
    )?;
    Ok(())
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
    processes: BTreeMap<(i32, u64), Survivor>,
    labels: BTreeMap<TerminalId, String>,
    origins: BTreeMap<TerminalId, TerminalId>,
    scopes: BTreeMap<String, (TerminalId, String)>,
    ambient_scopes: BTreeSet<String>,
    markers: BTreeMap<Vec<u8>, (TerminalId, String)>,
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
mod tests {
    use super::*;
    use crate::model::{
        BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalContinuity,
    };
    use std::sync::{Mutex, atomic::AtomicUsize};

    struct FakeSource {
        preview: impact::Preview,
        inventory: Inventory,
        events: Arc<Mutex<Vec<String>>>,
        observations: Arc<AtomicUsize>,
        successful_closes: BTreeSet<TerminalId>,
    }

    impl FloorSource for FakeSource {
        fn observe(&mut self) -> Result<impact::Preview, UpdateError> {
            self.observations.fetch_add(1, Ordering::Release);
            Ok(self.preview.clone())
        }
        fn observe_policy(
            &mut self,
            policy: impact::Policy,
        ) -> Result<impact::Preview, UpdateError> {
            self.preview.daemon.as_mut().unwrap().policy = policy;
            self.observe()
        }
        fn inventory(
            &mut self,
            _: &impact::DaemonImpact,
            _: bool,
        ) -> Result<Inventory, UpdateError> {
            Ok(self.inventory.clone())
        }
        fn prepare(&mut self, _: &impact::DaemonImpact) -> Result<(), UpdateError> {
            self.events
                .lock()
                .unwrap()
                .push("PrepareConfirmedUpgrade refused".into());
            Err(invalid("No exact resumable conversation"))
        }
        fn close_chats(
            &mut self,
            _: &impact::DaemonImpact,
            ending: Vec<crate::chat::Session>,
        ) -> Result<Vec<TerminalId>, UpdateError> {
            self.events.lock().unwrap().push("ChatClose".into());
            let closed: Vec<_> = ending
                .into_iter()
                .filter(|chat| self.successful_closes.contains(&chat.id))
                .map(|chat| chat.id)
                .collect();
            self.inventory
                .chats
                .retain(|chat| !closed.contains(&chat.id));
            let daemon = self.preview.daemon.as_mut().unwrap();
            daemon.sessions.retain(|item| !closed.contains(&item.id));
            daemon.capability.inventory_generation += 1;
            for item in &mut daemon.sessions {
                item.identity.push_str(" renewed global generation");
            }
            Ok(closed)
        }
        fn preflight(
            &mut self,
            daemon: &impact::DaemonImpact,
        ) -> Result<ShutdownCapability, UpdateError> {
            self.events
                .lock()
                .unwrap()
                .push("ShutdownV1 Preflight".into());
            Ok(daemon.capability.clone())
        }
        fn shutdown(
            &mut self,
            daemon: &impact::DaemonImpact,
            capability: ShutdownCapability,
        ) -> Result<(), UpdateError> {
            assert_eq!(daemon.capability.proof, capability.proof);
            self.events
                .lock()
                .unwrap()
                .push("ShutdownV1 Confirm".into());
            Ok(())
        }
        fn retain(
            &mut self,
            _: &impact::DaemonImpact,
            chats: Vec<crate::chat::Session>,
            terminals: Vec<TerminalId>,
            _service_stopped: bool,
        ) -> Result<(), UpdateError> {
            self.events.lock().unwrap().push(format!(
                "retain {} chats {} terminals",
                chats.len(),
                terminals.len()
            ));
            Ok(())
        }
        fn capture(&mut self, _: &StopList) -> Result<(), UpdateError> {
            Ok(())
        }
        fn stopped(
            &mut self,
            _: &Path,
            _: &mut UpdateJob,
            service: bool,
        ) -> Result<(), UpdateError> {
            self.events.lock().unwrap().push(
                if service {
                    "handoff without checkpoint"
                } else {
                    "handoff with checkpoint"
                }
                .into(),
            );
            Ok(())
        }
    }

    fn fixture() -> FakeSource {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("source"),
        };
        let mut terminal = TerminalDescriptor::relaunchable(
            TerminalId::new(),
            LaunchRecipe {
                program: "bash".into(),
                arguments: Vec::new(),
                working_directory: "/work".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            None,
        );
        terminal.session.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let daemon = impact::DaemonImpact {
            capability: ShutdownCapability {
                proof,
                token: "capability".into(),
                inventory_generation: 1,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: i64::MAX,
            },
            policy: impact::Policy::WhenIdle,
            sessions: vec![impact::SessionImpact {
                id: terminal.session.id,
                kind: impact::SessionKind::Terminal,
                title: "build".into(),
                identity: "exact terminal owner".into(),
                disposition: impact::Disposition::Continues,
                reason: String::new(),
                interrupted_subagents: Vec::new(),
            }],
            blockers: Vec::new(),
        };
        FakeSource {
            preview: impact::Preview {
                editors: Vec::new(),
                candidate: impact::Candidate {
                    version: "new".into(),
                    protocol: 160,
                    digest: "verified".into(),
                },
                replaces_daemon: true,
                daemon: Some(daemon),
                from_version: "old".into(),
                windows: 1,
                stopped: Vec::new(),
                stopped_windows: Vec::new(),
                labels: Default::default(),
            },
            inventory: Inventory {
                chats: Vec::new(),
                chat_states: BTreeMap::new(),
                terminals: vec![terminal],
            },
            events: Arc::new(Mutex::new(Vec::new())),
            observations: Arc::new(AtomicUsize::new(0)),
            successful_closes: BTreeSet::new(),
        }
    }

    fn job(directory: &Path) -> UpdateJob {
        UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "new".into(),
            phase: UpdatePhase::Verifying,
            message: String::new(),
            log_path: directory.join("worker.log"),
            confirmation: None,
        }
    }

    fn await_stop(directory: &Path) -> UpdateJob {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(bytes) = fs::read(directory.join("status.json"))
                && let Ok(job) = serde_json::from_slice::<UpdateJob>(&bytes)
                && job
                    .confirmation
                    .as_ref()
                    .is_some_and(|confirmation| confirmation.stop.is_some())
            {
                return job;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not publish the destructive confirmation"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn submit(directory: &Path, job: &UpdateJob, choice: UpdateChoice) -> PathBuf {
        let nonce = &job.confirmation.as_ref().unwrap().nonce;
        let temporary = directory.join(uuid::Uuid::new_v4().to_string());
        let file = private_create(&temporary).unwrap();
        serde_json::to_writer(
            &file,
            &serde_json::json!({"job":job.id,"nonce":nonce,"choice":choice}),
        )
        .unwrap();
        file.sync_all().unwrap();
        let path = directory.join(format!("{}.{}.decision", job.id, nonce));
        fs::hard_link(&temporary, &path).unwrap();
        fs::remove_file(temporary).unwrap();
        path
    }

    fn proc_fixture(
        root: &Path,
        pid: i32,
        parent: i32,
        start: u64,
        cgroup: &str,
        environment: &[u8],
    ) {
        let directory = root.join(pid.to_string());
        fs::create_dir_all(&directory).unwrap();
        let mut fields = vec!["0".to_owned(); 20];
        fields[0] = "S".into();
        fields[1] = parent.to_string();
        fields[19] = start.to_string();
        fs::write(
            directory.join("stat"),
            format!("{pid} (fixture) {}\n", fields.join(" ")),
        )
        .unwrap();
        fs::write(directory.join("cgroup"), format!("0::{cgroup}\n")).unwrap();
        fs::write(directory.join("environ"), environment).unwrap();
        fs::write(directory.join("cmdline"), format!("work-{pid}\0")).unwrap();
    }

    #[test]
    fn survivor_report_finds_late_reparented_scope_and_origin_members_without_shared_scope_guesses()
    {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let id = TerminalId::new();
        let origin = TerminalId::new();
        let scope = format!(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/cagents-session-{}.scope",
            uuid::Uuid::new_v4()
        );
        let shared = "/user.slice/user-1000.slice/user@1000.service/app.slice";
        proc_fixture(root, 101, 1, 10, &format!("{scope}/provider"), b"");
        proc_fixture(root, 102, 1, 20, shared, b"");
        let list = StopList {
            from_version: "old".into(),
            items: vec![super::super::floor_display::StopItem {
                id,
                kind: impact::SessionKind::Terminal,
                label: "build".into(),
                identity: "reviewed".into(),
                running: "terminal session".into(),
                recovery: "Restart".into(),
            }],
        };
        let mut survivors = Survivors::default();
        survivors.remember_origin(id, origin);
        survivors.remember_list(&list);
        survivors
            .capture_at(
                root,
                crate::daemon::current_uid(),
                BTreeMap::from([(
                    101,
                    (
                        Process {
                            pid: 101,
                            start: 10,
                        },
                        id,
                        "build".into(),
                    ),
                )]),
            )
            .unwrap();
        assert_eq!(survivors.processes.len(), 1);
        assert_eq!(survivors.scopes.keys().collect::<Vec<_>>(), [&scope]);
        fs::remove_dir_all(root.join("101")).unwrap();
        // Both jobs appear only after the last ancestry census, and their
        // original provider/holder has already exited and been reparented.
        proc_fixture(
            root,
            201,
            1,
            31,
            &format!("{scope}/command-new/nested"),
            b"",
        );
        proc_fixture(
            root,
            202,
            1,
            32,
            shared,
            format!("CAGENTS_RECOVERY_ORIGIN={origin}\0").as_bytes(),
        );
        proc_fixture(root, 203, 1, 33, shared, b"");
        proc_fixture(
            root,
            204,
            1,
            34,
            &format!("{shared}/cagents-session-{}.scope", uuid::Uuid::new_v4()),
            b"",
        );
        proc_fixture(
            root,
            205,
            1,
            35,
            shared,
            format!("CAGENTS_RECOVERY_ORIGIN={origin}-different\0").as_bytes(),
        );
        proc_fixture(
            root,
            206,
            1,
            36,
            shared,
            format!("CAGENTS_TERMINAL_ID={id}\0").as_bytes(),
        );
        // A reused numeric PID in an unrelated shared scope is not the root
        // whose epoch was captured before shutdown.
        proc_fixture(root, 101, 1, 99, shared, b"");
        let live = survivors
            .live_at(root, crate::daemon::current_uid())
            .unwrap();
        assert_eq!(
            live.iter()
                .map(|survivor| survivor.process.pid)
                .collect::<Vec<_>>(),
            [201, 202, 206]
        );
        assert!(
            live.iter()
                .all(|survivor| survivor.id == id && survivor.description.contains("build"))
        );
        assert!(
            survivors
                .live_at(root, crate::daemon::current_uid().saturating_add(1))
                .unwrap()
                .is_empty()
        );
        assert_eq!(session_scope(&format!("0::{shared}\n")), None);
    }

    #[test]
    fn a_restarted_terminal_with_an_old_origin_is_not_reported_as_unattached() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let old = TerminalId::new();
        let origin = TerminalId::new();
        let restarted = TerminalId::new();
        let shared = "/user.slice/app.slice";
        proc_fixture(
            root,
            201,
            1,
            20,
            shared,
            format!("CAGENTS_TERMINAL_ID={old}\0CAGENTS_RECOVERY_ORIGIN={origin}\0").as_bytes(),
        );
        proc_fixture(
            root,
            202,
            1,
            30,
            shared,
            format!("CAGENTS_TERMINAL_ID={restarted}\0CAGENTS_RECOVERY_ORIGIN={origin}\0")
                .as_bytes(),
        );
        let mut survivors = Survivors::default();
        survivors.markers.insert(
            format!("CAGENTS_RECOVERY_ORIGIN={origin}").into_bytes(),
            (old, "old build".into()),
        );
        let live = survivors
            .live_at(root, crate::daemon::current_uid())
            .unwrap();
        assert_eq!(live.len(), 2);
        assert!(live.iter().all(|survivor| survivor.id == old));
        let attached = BTreeSet::from([restarted, origin]);
        let unattached = Survivors::unattached_at(root, live.clone(), &attached);
        assert_eq!(
            unattached
                .iter()
                .map(|survivor| survivor.process.pid)
                .collect::<Vec<_>>(),
            [201]
        );
        // Re-read the current terminal marker when the destination inventory
        // is checked, rather than relying on the earlier origin association.
        fs::write(
            root.join("201/environ"),
            format!("CAGENTS_TERMINAL_ID={restarted}\0CAGENTS_RECOVERY_ORIGIN={origin}\0"),
        )
        .unwrap();
        assert!(Survivors::unattached_at(root, live.clone(), &attached).is_empty());
        // Reusing a PID cannot turn a captured epoch into a new survivor.
        proc_fixture(root, 201, 1, 99, shared, b"");
        assert!(Survivors::unattached_at(root, live, &attached).is_empty());
    }

    #[test]
    fn daemon_ancestry_does_not_claim_its_inherited_session_scope() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let ambient = format!(
            "/user.slice/app.slice/cagents-session-{}.scope",
            uuid::Uuid::new_v4()
        );
        let dedicated = format!(
            "/user.slice/app.slice/cagents-session-{}.scope",
            uuid::Uuid::new_v4()
        );
        proc_fixture(root, 101, 1, 10, &ambient, b"");
        proc_fixture(root, 102, 101, 20, &ambient, b"");
        proc_fixture(root, 103, 101, 30, &format!("{dedicated}/provider"), b"");
        let mut survivors = Survivors::default();
        survivors.exclude_ambient(root, 101);
        survivors
            .capture_at(
                root,
                crate::daemon::current_uid(),
                BTreeMap::from([(
                    101,
                    (
                        Process {
                            pid: 101,
                            start: 10,
                        },
                        TerminalId::from_uuid(uuid::Uuid::nil()),
                        "terminal service".into(),
                    ),
                )]),
            )
            .unwrap();
        assert!(!survivors.scopes.contains_key(&ambient));
        assert!(survivors.scopes.contains_key(&dedicated));
        for pid in [101, 102, 103] {
            fs::remove_dir_all(root.join(pid.to_string())).unwrap();
        }
        proc_fixture(root, 201, 1, 40, &ambient, b"");
        proc_fixture(root, 202, 1, 50, &format!("{dedicated}/command-late"), b"");
        let live = survivors
            .live_at(root, crate::daemon::current_uid())
            .unwrap();
        assert_eq!(
            live.iter()
                .map(|survivor| survivor.process.pid)
                .collect::<Vec<_>>(),
            [202]
        );
        assert!(live[0].description.contains("terminal service"));
    }

    #[test]
    fn refused_prepare_stops_the_service_only_with_stop_listed_consent() {
        for stop in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source = fixture();
            let events = source.events.clone();
            let observations = source.observations.clone();
            let path = directory.path().to_owned();
            let worker = thread::spawn(move || {
                let mut source = source;
                let preview = source.preview.clone();
                quiesce_source(&path, &mut job(&path), &preview, true, &mut source)
            });
            let published = await_stop(directory.path());
            assert_eq!(*events.lock().unwrap(), ["PrepareConfirmedUpgrade refused"]);
            if stop {
                let digest = published
                    .confirmation
                    .as_ref()
                    .unwrap()
                    .stop
                    .as_ref()
                    .unwrap()
                    .digest();
                submit(
                    directory.path(),
                    &published,
                    UpdateChoice::StopListed {
                        items_digest: digest,
                    },
                );
                worker.join().unwrap().unwrap();
                assert_eq!(
                    *events.lock().unwrap(),
                    [
                        "PrepareConfirmedUpgrade refused",
                        "ShutdownV1 Preflight",
                        "retain 0 chats 1 terminals",
                        "ShutdownV1 Confirm",
                        "handoff without checkpoint"
                    ]
                );
            } else {
                let count = observations.load(Ordering::Acquire);
                let decision = submit(directory.path(), &published, UpdateChoice::Confirm);
                let deadline = Instant::now() + Duration::from_secs(10);
                while decision.exists() || observations.load(Ordering::Acquire) <= count + 1 {
                    assert!(
                        !worker.is_finished(),
                        "ordinary confirmation allowed a destructive stop"
                    );
                    assert!(Instant::now() < deadline, "worker stopped observing");
                    thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(*events.lock().unwrap(), ["PrepareConfirmedUpgrade refused"]);
                submit(directory.path(), &published, UpdateChoice::Cancel);
                assert!(matches!(
                    worker.join().unwrap(),
                    Err(UpdateError::Cancelled)
                ));
                assert_eq!(*events.lock().unwrap(), ["PrepareConfirmedUpgrade refused"]);
            }
        }
    }

    #[test]
    fn a_window_request_never_enters_the_refused_prepare_stop_path() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture();
        let preview = source.preview.clone();
        let error = quiesce_source(
            directory.path(),
            &mut job(directory.path()),
            &preview,
            false,
            &mut source,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), TERMINAL_CONFIRMATION);
        assert_eq!(
            *source.events.lock().unwrap(),
            ["PrepareConfirmedUpgrade refused"]
        );
        assert!(!directory.path().join("status.json").exists());
    }

    #[test]
    fn one_service_confirmation_covers_successful_and_refused_chat_closes() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture();
        let proof = source
            .preview
            .daemon
            .as_ref()
            .unwrap()
            .capability
            .proof
            .clone();
        for succeeds in [true, false] {
            let chat = crate::chat::Session {
                id: TerminalId::new(),
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe: source.inventory.terminals[0].session.launch.clone(),
                    session_id: Some("recorded".into()),
                },
                proof: Some(proof.clone()),
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            };
            if succeeds {
                source.successful_closes.insert(chat.id);
            }
            source
                .preview
                .daemon
                .as_mut()
                .unwrap()
                .sessions
                .push(impact::SessionImpact {
                    id: chat.id,
                    kind: impact::SessionKind::NativeChat,
                    title: "chat".into(),
                    identity: chat.id.to_string(),
                    disposition: impact::Disposition::Blocked,
                    reason: "cannot checkpoint".into(),
                    interrupted_subagents: Vec::new(),
                });
            source.inventory.chats.push(chat);
        }
        source
            .preview
            .daemon
            .as_mut()
            .unwrap()
            .blockers
            .push("pending handoff".into());
        let events = source.events.clone();
        let path = directory.path().to_owned();
        let worker = thread::spawn(move || {
            let preview = source.preview.clone();
            quiesce_source(&path, &mut job(&path), &preview, true, &mut source)
        });
        let published = await_stop(directory.path());
        let digest = published
            .confirmation
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap()
            .digest();
        submit(
            directory.path(),
            &published,
            UpdateChoice::StopListed {
                items_digest: digest,
            },
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !worker.is_finished() {
            let current: UpdateJob =
                serde_json::from_slice(&fs::read(directory.path().join("status.json")).unwrap())
                    .unwrap();
            submit(directory.path(), &current, UpdateChoice::Cancel);
            let result = worker.join().unwrap();
            panic!("known service consent unexpectedly needed another decision: {result:?}");
        }
        worker.join().unwrap().unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            [
                "retain 2 chats 0 terminals",
                "ChatClose",
                "ShutdownV1 Preflight",
                "retain 1 chats 1 terminals",
                "ShutdownV1 Confirm",
                "handoff without checkpoint"
            ]
        );
    }

    #[test]
    fn floor_a_keeps_waiting_for_other_work_and_cancel_never_escalates_to_service_stop() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture();
        let proof = source
            .preview
            .daemon
            .as_ref()
            .unwrap()
            .capability
            .proof
            .clone();
        for disposition in [
            impact::Disposition::Blocked,
            impact::Disposition::WaitsForIdle,
        ] {
            let chat = crate::chat::Session {
                id: TerminalId::new(),
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Claude,
                    recipe: source.inventory.terminals[0].session.launch.clone(),
                    session_id: Some("recorded".into()),
                },
                proof: Some(proof.clone()),
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            };
            if disposition == impact::Disposition::Blocked {
                source.successful_closes.insert(chat.id);
            }
            source
                .preview
                .daemon
                .as_mut()
                .unwrap()
                .sessions
                .push(impact::SessionImpact {
                    id: chat.id,
                    kind: impact::SessionKind::NativeChat,
                    title: "chat".into(),
                    identity: chat.id.to_string(),
                    disposition,
                    reason: "source state".into(),
                    interrupted_subagents: Vec::new(),
                });
            source.inventory.chats.push(chat);
        }
        let events = source.events.clone();
        let path = directory.path().to_owned();
        let worker = thread::spawn(move || {
            let preview = source.preview.clone();
            quiesce_source(&path, &mut job(&path), &preview, true, &mut source)
        });
        let published = await_stop(directory.path());
        assert_eq!(
            published
                .confirmation
                .as_ref()
                .unwrap()
                .stop
                .as_ref()
                .unwrap()
                .items
                .len(),
            1
        );
        let digest = published
            .confirmation
            .as_ref()
            .unwrap()
            .stop
            .as_ref()
            .unwrap()
            .digest();
        submit(
            directory.path(),
            &published,
            UpdateChoice::StopListed {
                items_digest: digest,
            },
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let waiting = loop {
            if let Ok(bytes) = fs::read(directory.path().join("status.json"))
                && let Ok(job) = serde_json::from_slice::<UpdateJob>(&bytes)
                && job.phase == UpdatePhase::WaitingForIdle
                && job
                    .confirmation
                    .as_ref()
                    .is_some_and(|confirmation| confirmation.stop.is_none())
            {
                break job;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not keep waiting for the working chat"
            );
            assert!(
                !worker.is_finished(),
                "working chat was sent to prepare prematurely"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            *events.lock().unwrap(),
            ["retain 1 chats 0 terminals", "ChatClose"]
        );
        submit(directory.path(), &waiting, UpdateChoice::Cancel);
        assert!(matches!(
            worker.join().unwrap(),
            Err(UpdateError::Cancelled)
        ));
        assert_eq!(
            *events.lock().unwrap(),
            ["retain 1 chats 0 terminals", "ChatClose"]
        );
    }

    #[test]
    fn paired_chat_and_console_are_listed_as_one_pane() {
        let mut source = fixture();
        let terminal = &source.inventory.terminals[0].session;
        let chat = crate::chat::Session {
            id: terminal.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: terminal.launch.clone(),
                session_id: Some("recorded".into()),
            },
            proof: source
                .preview
                .daemon
                .as_ref()
                .map(|daemon| daemon.capability.proof.clone()),
            terminal: Some(terminal.clone()),
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        source.inventory.chats.push(chat);
        source.preview.daemon.as_mut().unwrap().sessions[0].kind = impact::SessionKind::NativeChat;
        let list = stop_list(&source.preview, &source.inventory, true).unwrap();
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].kind, impact::SessionKind::NativeChat);
    }

    #[test]
    fn consent_survives_only_our_known_closes_and_unchanged_source_descriptors() {
        let mut source = fixture();
        let proof = source
            .preview
            .daemon
            .as_ref()
            .unwrap()
            .capability
            .proof
            .clone();
        let chat = crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: source.inventory.terminals[0].session.launch.clone(),
                session_id: Some("recorded".into()),
            },
            proof: Some(proof),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let item = impact::SessionImpact {
            id: chat.id,
            kind: impact::SessionKind::NativeChat,
            title: "chat".into(),
            identity: "exact chat".into(),
            disposition: impact::Disposition::Blocked,
            reason: "cannot checkpoint".into(),
            interrupted_subagents: Vec::new(),
        };
        source.inventory.chats.push(chat.clone());
        source.preview.daemon.as_mut().unwrap().sessions.push(item);
        let before = stop_list(&source.preview, &source.inventory, true).unwrap();
        let original = source.inventory.clone();
        source.successful_closes.insert(chat.id);
        let daemon = source.preview.daemon.as_ref().unwrap().clone();
        let closed = source
            .close_chats(&daemon, vec![chat])
            .unwrap()
            .into_iter()
            .collect();
        let after = stop_list(&source.preview, &source.inventory, true).unwrap();
        assert!(same_after_our_closes(
            &before,
            &original,
            &after,
            &source.inventory,
            &closed
        ));
        source.inventory.terminals[0]
            .session
            .launch
            .arguments
            .push("another command".into());
        assert!(!same_after_our_closes(
            &before,
            &original,
            &after,
            &source.inventory,
            &closed
        ));
    }
}
