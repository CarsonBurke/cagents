//! Settle explicitly consented interruptions before a frozen source checkpoints.
//!
//! Request acceptance is not provider completion. Keep the old daemon serving
//! while its actor publishes an idle turn and its execution registry records
//! that exact run's outcome; only the caller may then request preparation.
use super::{Client, UpdateError, impact, invalid};
use crate::{
    agent_execution::{self, directory},
    chat::{Command, Session, Snapshot, ToolStatus},
    control::ControlClient,
    model::{AgentId, RunId, TerminalId},
    terminal_ipc::{ClientRequest, ServerResponse},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

pub(in crate::update) struct Interrupts {
    source: Option<Client>,
    consent: impact::DaemonImpact,
    observations: BTreeMap<TerminalId, Observation>,
    deadline: Instant,
    current: impact::DaemonImpact,
}

struct Observation {
    session: Session,
    turn_generation: u64,
    publication: u64,
    permitted_children: BTreeSet<String>,
    run: Option<RunIdentity>,
    activity: Activity,
    interrupted_generation: Option<u64>,
    stopped_children: BTreeSet<String>,
    late_turn: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RunIdentity {
    agent: AgentId,
    owner: directory::Owner,
    run: Option<RunId>,
}

struct Activity {
    busy: bool,
    accepted: bool,
    settled: bool,
    completion: bool,
    separate_children: BTreeSet<String>,
    atomic_parent: bool,
}

impl Observation {
    fn project(&mut self, snapshot: Snapshot, initial: bool) -> Result<(), UpdateError> {
        if !initial && snapshot.generation <= self.publication {
            return Err(invalid(
                "The interrupted conversation's publication did not advance",
            ));
        }
        if snapshot.turn_generation < self.turn_generation
            || snapshot.session_id != self.session.launch.session_id
        {
            return Err(invalid(
                "The interrupted conversation changed its provider identity",
            ));
        }
        if snapshot.turn_generation > self.turn_generation {
            self.late_turn = true;
            self.turn_generation = snapshot.turn_generation;
        }
        let children: BTreeSet<_> = snapshot
            .subagents
            .iter()
            .filter(|child| child.status == ToolStatus::Running)
            .map(|child| child.id.clone())
            .collect();
        let background: BTreeSet<_> = snapshot
            .subagents
            .iter()
            .filter(|child| child.background && child.status == ToolStatus::Running)
            .map(|child| child.id.clone())
            .collect();
        if !background.is_subset(&self.permitted_children) {
            return Err(invalid(
                "The conversation has new running background agents; review the update again",
            ));
        }
        let atomic_parent = snapshot.capabilities.interrupt_stops_background;
        if atomic_parent
            && !self
                .run
                .as_ref()
                .is_some_and(|run| run.owner == directory::Owner::User && run.run.is_some())
        {
            return Err(invalid(
                "This source cannot safely interrupt this conversation without also stopping unreviewed background agents; finish its turn before updating",
            ));
        }
        if !background.is_empty() && !snapshot.capabilities.stop_subagent {
            return Err(invalid(
                "This source cannot stop the reviewed background agents individually; finish them before updating",
            ));
        }
        let separate_children = if snapshot.capabilities.stop_subagent {
            background
        } else {
            BTreeSet::new()
        };
        self.stopped_children.retain(|id| children.contains(id));
        self.publication = snapshot.generation;
        self.activity = Activity {
            busy: snapshot.busy,
            accepted: snapshot.turn_accepted(),
            settled: snapshot.ready
                && !snapshot.exited
                && snapshot.error.is_none()
                && !snapshot.history_loading
                && !snapshot.turn_accepted()
                && snapshot.queued_messages.is_empty()
                && children.is_empty(),
            completion: snapshot.finished_at.is_some(),
            separate_children,
            atomic_parent,
        };
        Ok(())
    }

    fn observe_run(&mut self, entries: &[directory::Entry]) -> Result<bool, UpdateError> {
        let (identity, active) = run_at(entries, &self.session)?;
        if match (&self.run, &identity) {
            (Some(before), Some(after)) => {
                before.agent != after.agent || before.owner != after.owner
            }
            (None, None) => false,
            _ => true,
        } {
            return Err(invalid(
                "The interrupted conversation's registry owner changed",
            ));
        }
        if identity != self.run {
            self.late_turn = true;
            self.run = identity;
        }
        Ok(active)
    }
}

fn run_at(
    entries: &[directory::Entry],
    session: &Session,
) -> Result<(Option<RunIdentity>, bool), UpdateError> {
    let mut matches = entries.iter().filter(|entry| entry.id == session.id);
    let entry = matches.next().ok_or_else(|| {
        invalid("The interrupted conversation disappeared from the source run registry")
    })?;
    if matches.next().is_some()
        || entry.provider != Some(session.launch.provider)
        || entry.surface != directory::Surface::Chat
        || entry.directory.as_ref() != Some(&session.launch.recipe.working_directory)
    {
        return Err(invalid(
            "The interrupted conversation's registry owner changed",
        ));
    }
    let Some(delegation) = &entry.delegation else {
        return Ok((None, false));
    };
    if delegation.run.is_some() != delegation.run_state.is_some() {
        return Err(invalid(
            "The source did not confirm the conversation's accepted run state",
        ));
    }
    if delegation.run_state == Some(agent_execution::State::Interrupted) {
        return Err(invalid(
            "The source lost the accepted run's outcome; inspect its conversation before updating",
        ));
    }
    Ok((
        Some(RunIdentity {
            agent: delegation.agent,
            owner: delegation.owner.clone(),
            run: delegation.run,
        }),
        delegation
            .run_state
            .is_some_and(|state| !state.is_terminal()),
    ))
}

fn request(
    client: &mut ControlClient,
    request: ClientRequest,
    deadline: Instant,
) -> Result<ServerResponse, UpdateError> {
    client
        .update_quiescence_request(request, deadline)
        .map_err(|error| invalid(error.to_string()))
}

fn directory(
    client: &mut ControlClient,
    deadline: Instant,
) -> Result<Vec<directory::Entry>, UpdateError> {
    match request(
        client,
        ClientRequest::Agent {
            authority: agent_execution::Authority::default(),
            operation: agent_execution::Operation::Directory(directory::Scope::All),
        },
        deadline,
    )? {
        ServerResponse::Agent(agent_execution::Reply::Directory(directory)) => {
            let mut entries = directory.sessions;
            entries.extend(directory.caller);
            Ok(entries)
        }
        _ => Err(invalid(
            "The source did not return its conversation run registry",
        )),
    }
}

fn check_scope(
    consent: &impact::DaemonImpact,
    observed: &impact::DaemonImpact,
) -> Result<(), UpdateError> {
    let old: BTreeMap<_, _> = consent
        .sessions
        .iter()
        .map(|item| ((item.kind, item.id), item))
        .collect();
    let fresh: BTreeMap<_, _> = observed
        .sessions
        .iter()
        .map(|item| ((item.kind, item.id), item))
        .collect();
    if consent.policy != impact::Policy::Interrupt
        || observed.policy != consent.policy
        || consent.capability.proof != observed.capability.proof
        || consent.capability.inventory_generation != observed.capability.inventory_generation
        || consent.blockers != observed.blockers
        || old.len() != consent.sessions.len()
        || fresh.len() != observed.sessions.len()
        || old.len() != fresh.len()
        || old.iter().any(|(key, before)| {
            fresh.get(key).is_none_or(|after| {
                before.identity != after.identity
                    || !(before.disposition == after.disposition
                        || before.disposition == impact::Disposition::Interrupted
                            && after.disposition == impact::Disposition::Resumes)
                    || after
                        .interrupted_subagents
                        .iter()
                        .any(|id| !before.interrupted_subagents.contains(id))
            })
        })
    {
        return Err(invalid(
            "Source ownership or interruption scope changed; review the update again",
        ));
    }
    Ok(())
}

fn fence(
    client: &mut ControlClient,
    consent: &impact::DaemonImpact,
    deadline: Instant,
) -> Result<(), UpdateError> {
    match request(
        client,
        ClientRequest::UpgradeImpact {
            policy: impact::Policy::Interrupt,
        },
        deadline,
    )? {
        ServerResponse::UpgradeImpact(observed) => check_scope(consent, &observed),
        _ => Err(invalid("The source did not confirm its interruption scope")),
    }
}

impl Interrupts {
    pub(in crate::update) fn begin(
        consent: &impact::DaemonImpact,
        deadline: Instant,
    ) -> Result<Self, UpdateError> {
        let mut result = Self {
            source: None,
            consent: consent.clone(),
            observations: BTreeMap::new(),
            deadline,
            current: consent.clone(),
        };
        if !consent.sessions.iter().any(|item| {
            item.kind == impact::SessionKind::NativeChat
                && item.disposition == impact::Disposition::Interrupted
        }) {
            return Ok(result);
        }
        if crate::device::is_remote() {
            return Err(invalid(
                "Cross-version application upgrades require the local source daemon",
            ));
        }
        let socket = crate::terminal_ipc::daemon_socket_path()?;
        let mut source =
            Client::connect_until(&socket, deadline).map_err(|error| invalid(error.to_string()))?;
        let Client::Current { client, peer } = &mut source else {
            return Err(invalid(
                "This older source cannot confirm interruptions before checkpointing",
            ));
        };
        if !super::current_client_supports(client.protocol()) {
            return Err(invalid(
                "This source protocol has no qualified interruption settlement boundary",
            ));
        }
        if client.proof() != &consent.capability.proof {
            return Err(invalid("The source daemon changed before interruption"));
        }
        peer.verify(client.proof())?;
        result.observations = begin_with(client, consent, deadline)?;
        peer.verify(client.proof())?;
        result.source = Some(source);
        Ok(result)
    }

    pub(in crate::update) fn current_impact(&self) -> &impact::DaemonImpact {
        &self.current
    }

    pub(in crate::update) fn late_turns(&self) -> Vec<TerminalId> {
        self.observations
            .iter()
            .filter_map(|(id, observed)| observed.late_turn.then_some(*id))
            .collect()
    }

    /// Return the exact conversations still awaiting provider or run settlement.
    /// The caller owns cancellation, pacing and the overall deadline.
    pub(in crate::update) fn poll(&mut self) -> Result<Vec<TerminalId>, UpdateError> {
        let Some(Client::Current { client, peer }) = &mut self.source else {
            return Ok(Vec::new());
        };
        peer.verify(client.proof())?;
        let (pending, idle) =
            poll_with(client, &self.consent, &mut self.observations, self.deadline)?;
        peer.verify(client.proof())?;
        if let Some(idle) = idle {
            self.current = idle;
        }
        Ok(pending)
    }
}

fn begin_with(
    client: &mut ControlClient,
    consent: &impact::DaemonImpact,
    deadline: Instant,
) -> Result<BTreeMap<TerminalId, Observation>, UpdateError> {
    fence(client, consent, deadline)?;
    let chats = match request(client, ClientRequest::ChatList, deadline)? {
        ServerResponse::ChatListed { sessions } => sessions,
        _ => return Err(invalid("The source did not list its issued conversations")),
    };
    let entries = directory(client, deadline)?;
    let mut observations = BTreeMap::new();
    for item in consent.sessions.iter().filter(|item| {
        item.kind == impact::SessionKind::NativeChat
            && item.disposition == impact::Disposition::Interrupted
    }) {
        let mut matches = chats.iter().filter(|session| session.id == item.id);
        let session = matches
            .next()
            .filter(|session| session.proof.as_ref() == Some(client.proof()))
            .ok_or_else(|| invalid("The source no longer owns the interrupted conversation"))?;
        if matches.next().is_some() {
            return Err(invalid("The interrupted conversation has ambiguous owners"));
        }
        let snapshot = client
            .observe_update_chat_until(session.clone(), deadline)
            .map_err(|error| invalid(error.to_string()))?;
        let mut observed = Observation {
            session: session.clone(),
            turn_generation: snapshot.turn_generation,
            publication: snapshot.generation,
            permitted_children: item.interrupted_subagents.iter().cloned().collect(),
            run: run_at(&entries, session)?.0,
            activity: Activity {
                busy: false,
                accepted: false,
                settled: false,
                completion: false,
                separate_children: BTreeSet::new(),
                atomic_parent: false,
            },
            interrupted_generation: None,
            stopped_children: BTreeSet::new(),
            late_turn: false,
        };
        observed.project(snapshot, true)?;
        observations.insert(item.id, observed);
    }
    if observations
        .values()
        .any(|observed| observed.activity.accepted && !observed.activity.busy)
    {
        return Err(invalid(
            "The conversation is awaiting user input without an interruptible turn; resolve it before updating",
        ));
    }
    issue(client, consent, &mut observations, deadline)?;
    Ok(observations)
}

/// Consent covers the listed conversation's work, including a turn that
/// starts while it is being reviewed. Frozen sources have no caller-supplied
/// expected-turn interrupt; turnover is reported, never called an atomic fence.
fn issue(
    client: &mut ControlClient,
    consent: &impact::DaemonImpact,
    observations: &mut BTreeMap<TerminalId, Observation>,
    deadline: Instant,
) -> Result<(), UpdateError> {
    for (id, observed) in observations {
        let commands: Vec<_> = observed
            .activity
            .separate_children
            .difference(&observed.stopped_children)
            .map(|id| Command::StopSubagent { id: id.clone() })
            .chain(
                (observed.activity.busy
                    && (!observed.activity.atomic_parent
                        || observed.activity.separate_children.is_empty())
                    && observed.interrupted_generation != Some(observed.turn_generation))
                .then_some(Command::Interrupt),
            )
            .collect();
        for command in commands {
            fence(client, consent, deadline)?;
            observed.observe_run(&directory(client, deadline)?)?;
            let guarded = matches!(command, Command::Interrupt) && observed.activity.atomic_parent;
            let operation = if guarded {
                let run = observed
                    .run
                    .as_ref()
                    .ok_or_else(|| invalid("This source has no guarded interruption owner"))?;
                ClientRequest::Agent {
                    authority: agent_execution::Authority::default(),
                    operation: agent_execution::Operation::Control {
                        agent: run.agent,
                        command: agent_execution::Control::Interrupt {
                            including_subagents: false,
                        },
                    },
                }
            } else {
                ClientRequest::ChatCommand {
                    chat_id: *id,
                    command: command.clone(),
                }
            };
            match request(client, operation, deadline)? {
                ServerResponse::Accepted if !guarded => (),
                ServerResponse::Agent(agent_execution::Reply::Requests(_)) if guarded => (),
                _ => {
                    return Err(invalid(
                        "The source did not accept the reviewed interruption",
                    ));
                }
            }
            match command {
                Command::Interrupt => {
                    observed.interrupted_generation = Some(observed.turn_generation)
                }
                Command::StopSubagent { id } => {
                    observed.stopped_children.insert(id);
                }
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

fn poll_with(
    client: &mut ControlClient,
    consent: &impact::DaemonImpact,
    observations: &mut BTreeMap<TerminalId, Observation>,
    deadline: Instant,
) -> Result<(Vec<TerminalId>, Option<impact::DaemonImpact>), UpdateError> {
    fence(client, consent, deadline)?;
    for (id, observed) in observations.iter_mut() {
        if let Some(snapshot) = client
            .read_update_chat_until(*id, observed.publication, deadline)
            .map_err(|error| invalid(error.to_string()))?
        {
            observed.project(snapshot, false)?;
        }
    }
    issue(client, consent, observations, deadline)?;
    let entries = directory(client, deadline)?;
    let mut pending = Vec::new();
    for (id, observed) in observations.iter_mut() {
        let run_active = observed.observe_run(&entries)?;
        if !observed.activity.settled
            || run_active
            || observed.run.is_none() && !observed.activity.completion
        {
            pending.push(*id);
        }
    }
    let idle = if pending.is_empty() {
        let ServerResponse::UpgradeImpact(idle) = request(
            client,
            ClientRequest::UpgradeImpact {
                policy: impact::Policy::WhenIdle,
            },
            deadline,
        )?
        else {
            return Err(invalid("The source did not confirm idle preparation scope"));
        };
        let mut compared = idle.clone();
        compared.policy = impact::Policy::Interrupt;
        for item in &mut compared.sessions {
            if item.disposition == impact::Disposition::WaitsForIdle
                && observations.contains_key(&item.id)
                && item.kind == impact::SessionKind::NativeChat
            {
                item.disposition = impact::Disposition::Interrupted;
            }
        }
        check_scope(consent, &compared)?;
        for item in &idle.sessions {
            if item.disposition == impact::Disposition::WaitsForIdle
                && item.kind == impact::SessionKind::NativeChat
                && let Some(observed) = observations.get_mut(&item.id)
            {
                observed.late_turn = true;
                pending.push(item.id);
            }
        }
        if !pending.is_empty() {
            return Ok((pending, None));
        }
        // This observer settles only the approved interruptions. Unchanged
        // blocked owners and blockers still need the floor's explicit stop
        // review; they are not evidence that these target turns remain busy.
        // check_scope above rejects any new blocker or unrelated work.
        if !idle.ready() && !consent.blocked() {
            return Err(invalid(
                "The source cannot confirm an idle preparation boundary; inspect its conversations before updating",
            ));
        }
        Some(idle)
    } else {
        None
    };
    Ok((pending, idle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe},
        terminal_ipc::{ClientEnvelope, ServerEnvelope, ServerMessage, read_frame, write_frame},
    };
    use std::{
        os::unix::net::UnixStream,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    fn fixture() -> (Session, impact::DaemonImpact, directory::Entry) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("source"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("daemon"),
        };
        let session = Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "offline-codex".into(),
                    arguments: Vec::new(),
                    working_directory: "/work".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("exact-thread".into()),
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let daemon = impact::DaemonImpact {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof,
                token: "fresh".into(),
                inventory_generation: 1,
                live_terminals: 1,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: i64::MAX,
            },
            policy: impact::Policy::Interrupt,
            sessions: vec![impact::SessionImpact {
                id: session.id,
                kind: impact::SessionKind::NativeChat,
                title: "Offline Codex".into(),
                identity: "exact-owner".into(),
                disposition: impact::Disposition::Interrupted,
                reason: String::new(),
                interrupted_subagents: Vec::new(),
            }],
            blockers: Vec::new(),
        };
        let entry = directory::Entry {
            id: session.id,
            surface: directory::Surface::Chat,
            provider: Some(session.launch.provider),
            title: None,
            channel: None,
            view_scope: None,
            channel_repository: None,
            lineage_channel: None,
            state: crate::agent::ActivityState::Working,
            directory: Some("/work".into()),
            worktree: None,
            project: None,
            project_name: None,
            delegation: Some(directory::Delegation {
                agent: AgentId::new(),
                owner: directory::Owner::User,
                run: Some(RunId::new()),
                run_state: Some(agent_execution::State::Running),
            }),
            relation: directory::Relation::Peer,
            lineage: agent_execution::Lineage(session.id),
        };
        (session, daemon, entry)
    }

    fn snapshot(session: &Session, phase: usize) -> Snapshot {
        Snapshot {
            generation: phase as u64 + 1,
            turn_generation: if phase >= 3 { 4 } else { 3 },
            session_id: session.launch.session_id.clone(),
            ready: true,
            busy: phase == 0 || phase == 3,
            finished_at: (phase > 0).then_some(7),
            ..Default::default()
        }
    }

    fn observation(session: &Session, entry: &directory::Entry) -> Observation {
        let mut observed = Observation {
            session: session.clone(),
            turn_generation: 3,
            publication: 0,
            permitted_children: BTreeSet::new(),
            run: run_at(std::slice::from_ref(entry), session).unwrap().0,
            activity: Activity {
                busy: false,
                accepted: false,
                settled: false,
                completion: false,
                separate_children: BTreeSet::new(),
                atomic_parent: false,
            },
            interrupted_generation: None,
            stopped_children: BTreeSet::new(),
            late_turn: false,
        };
        observed.project(snapshot(session, 0), true).unwrap();
        observed
    }

    #[test]
    fn protocol_156_acceptance_is_not_settlement_and_registry_must_commit_the_outcome() {
        assert_quiescence(false);
    }

    #[test]
    fn settled_target_keeps_previously_reviewed_blocked_owners_for_floor_review() {
        assert_quiescence(true);
    }

    fn assert_quiescence(known_blocked: bool) {
        let (session, mut daemon, entry) = fixture();
        if known_blocked {
            daemon.sessions.push(impact::SessionImpact {
                id: TerminalId::new(),
                kind: impact::SessionKind::Terminal,
                title: "Previously reviewed terminal".into(),
                identity: "exact-blocked-owner".into(),
                disposition: impact::Disposition::Blocked,
                reason: "Needs the floor's explicit stop review".into(),
                interrupted_subagents: Vec::new(),
            });
            daemon
                .blockers
                .push("Previously reviewed source blocker".into());
        }
        let (stream, mut server_stream) = UnixStream::pair().unwrap();
        let phase = Arc::new(AtomicUsize::new(0));
        let commands = Arc::new(AtomicUsize::new(0));
        let server_phase = phase.clone();
        let server_commands = commands.clone();
        let proof = daemon.capability.proof.clone();
        let source = session.clone();
        let impact = daemon.clone();
        let late_run = RunId::new();
        let server = thread::spawn(move || {
            while let Some(envelope) = read_frame::<ClientEnvelope>(&mut server_stream).unwrap() {
                let phase = server_phase.load(Ordering::SeqCst);
                let response = match envelope.request {
                    ClientRequest::Connect { .. } => ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: Vec::new(),
                    },
                    ClientRequest::UpgradeImpact { policy } => {
                        let mut observed = impact.clone();
                        observed.policy = policy;
                        if phase > 0 && phase != 3 {
                            observed.sessions[0].disposition = impact::Disposition::Resumes;
                        }
                        ServerResponse::UpgradeImpact(observed)
                    }
                    ClientRequest::ChatList => ServerResponse::ChatListed {
                        sessions: vec![source.clone()],
                    },
                    ClientRequest::ChatAttach { session } if session == source => {
                        ServerResponse::ChatAttached {
                            session,
                            snapshot: snapshot(&source, phase),
                        }
                    }
                    ClientRequest::ChatVisibility {
                        chat_id,
                        visible: true,
                    } if chat_id == source.id => ServerResponse::Accepted,
                    ClientRequest::ChatCommand {
                        chat_id,
                        command: Command::Interrupt,
                    } if chat_id == source.id => {
                        server_commands.fetch_add(1, Ordering::SeqCst);
                        ServerResponse::Accepted
                    }
                    ClientRequest::ChatRead {
                        chat_id,
                        after_generation: Some(generation),
                    } if chat_id == source.id => ServerResponse::ChatSnapshot {
                        chat_id,
                        snapshot: (generation < phase as u64 + 1).then(|| snapshot(&source, phase)),
                    },
                    ClientRequest::Agent {
                        operation: agent_execution::Operation::Directory(directory::Scope::All),
                        ..
                    } => {
                        let mut entry = entry.clone();
                        if phase >= 3 {
                            entry.delegation.as_mut().unwrap().run = Some(late_run);
                        }
                        if phase > 1 && phase != 3 {
                            entry.delegation.as_mut().unwrap().run_state =
                                Some(agent_execution::State::Cancelled);
                        }
                        ServerResponse::Agent(agent_execution::Reply::Directory(
                            directory::Directory {
                                caller: None,
                                scope: directory::Scope::All,
                                sessions: vec![entry],
                            },
                        ))
                    }
                    other => panic!("unexpected or work-starting request: {other:?}"),
                };
                write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: 156,
                        request_id: Some(envelope.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client =
            ControlClient::from_stream_at_source_protocol(stream, deadline, 156).unwrap();
        let mut observations = begin_with(&mut client, &daemon, deadline).unwrap();
        assert_eq!(commands.load(Ordering::SeqCst), 1);
        assert_eq!(
            poll_with(&mut client, &daemon, &mut observations, deadline)
                .unwrap()
                .0,
            [session.id],
            "an ACK leaves the turn running"
        );
        phase.store(1, Ordering::SeqCst);
        assert_eq!(
            poll_with(&mut client, &daemon, &mut observations, deadline)
                .unwrap()
                .0,
            [session.id],
            "the actor can become idle before the run outcome commits"
        );
        phase.store(2, Ordering::SeqCst);
        assert!(
            poll_with(&mut client, &daemon, &mut observations, deadline)
                .unwrap()
                .0
                .is_empty()
        );
        assert_eq!(
            commands.load(Ordering::SeqCst),
            1,
            "polling never repeats an interruption"
        );
        phase.store(3, Ordering::SeqCst);
        assert_eq!(
            poll_with(&mut client, &daemon, &mut observations, deadline)
                .unwrap()
                .0,
            [session.id]
        );
        assert_eq!(
            commands.load(Ordering::SeqCst),
            2,
            "a later turn in the consented conversation is interrupted once"
        );
        assert!(observations[&session.id].late_turn);
        phase.store(4, Ordering::SeqCst);
        let (pending, idle) = poll_with(&mut client, &daemon, &mut observations, deadline).unwrap();
        assert!(pending.is_empty());
        let idle = idle.unwrap();
        assert_eq!(idle.policy, impact::Policy::WhenIdle);
        assert_eq!(idle.blocked(), known_blocked);
        if known_blocked {
            assert!(
                !idle.ready(),
                "settlement must preserve the outstanding floor review"
            );
            assert_eq!(idle.blockers, daemon.blockers);
            assert_eq!(idle.sessions[1], daemon.sessions[1]);
        }

        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn parent_idle_still_waits_for_individually_stopped_children() {
        let (session, _, entry) = fixture();
        let mut observed = observation(&session, &entry);
        observed.permitted_children.insert("reviewed-child".into());
        let mut child = crate::chat::Subagent {
            id: "reviewed-child".into(),
            status: ToolStatus::Running,
            background: true,
            kind: String::new(),
            description: String::new(),
            model: None,
            waiting: false,
            detail: String::new(),
            items: Vec::new(),
        };
        let mut running = snapshot(&session, 0);
        running.capabilities.interrupt_stops_background = true;
        running.capabilities.stop_subagent = true;
        running.subagents.push(child.clone());
        observed.project(running, true).unwrap();
        observed.stopped_children.insert("reviewed-child".into());
        let mut parent_idle = snapshot(&session, 1);
        parent_idle.capabilities.interrupt_stops_background = true;
        parent_idle.capabilities.stop_subagent = true;
        parent_idle.subagents.push(child.clone());
        observed.project(parent_idle, false).unwrap();
        assert!(
            !observed.activity.settled,
            "parent idle is not the child's completion receipt"
        );
        child.status = ToolStatus::Stopped;
        let mut completed = snapshot(&session, 2);
        completed.subagents.push(child);
        observed.project(completed, false).unwrap();
        assert!(observed.activity.settled);
    }

    #[test]
    fn guarded_parent_waits_for_reviewed_children_and_refuses_a_new_child_at_dispatch() {
        let (session, mut consent, entry) = fixture();
        consent.sessions[0]
            .interrupted_subagents
            .push("reviewed-child".into());
        let mut observed = observation(&session, &entry);
        observed.permitted_children.insert("reviewed-child".into());
        let mut running = snapshot(&session, 0);
        running.capabilities.interrupt_stops_background = true;
        running.capabilities.stop_subagent = true;
        running.subagents.push(crate::chat::Subagent {
            id: "reviewed-child".into(),
            status: ToolStatus::Running,
            background: true,
            kind: String::new(),
            description: String::new(),
            model: None,
            waiting: false,
            detail: String::new(),
            items: Vec::new(),
        });
        observed.project(running, true).unwrap();
        let mut observations = BTreeMap::from([(session.id, observed)]);
        let (stream, mut server_stream) = UnixStream::pair().unwrap();
        let child_stops = Arc::new(AtomicUsize::new(0));
        let guarded_refusals = Arc::new(AtomicUsize::new(0));
        let received_children = child_stops.clone();
        let received_guards = guarded_refusals.clone();
        let proof = consent.capability.proof.clone();
        let impact = consent.clone();
        let source = session.clone();
        let server = thread::spawn(move || {
            while let Some(envelope) = read_frame::<ClientEnvelope>(&mut server_stream).unwrap() {
                let response = match envelope.request {
                    ClientRequest::Connect { .. } => ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: Vec::new(),
                    },
                    ClientRequest::UpgradeImpact {
                        policy: impact::Policy::Interrupt,
                    } => ServerResponse::UpgradeImpact(impact.clone()),
                    ClientRequest::Agent {
                        operation: agent_execution::Operation::Directory(directory::Scope::All),
                        ..
                    } => ServerResponse::Agent(agent_execution::Reply::Directory(
                        directory::Directory {
                            caller: None,
                            scope: directory::Scope::All,
                            sessions: vec![entry.clone()],
                        },
                    )),
                    ClientRequest::ChatCommand {
                        chat_id,
                        command: Command::StopSubagent { id },
                    } if chat_id == source.id && id == "reviewed-child" => {
                        received_children.fetch_add(1, Ordering::SeqCst);
                        ServerResponse::Accepted
                    }
                    ClientRequest::Agent {
                        authority,
                        operation:
                            agent_execution::Operation::Control {
                                agent,
                                command:
                                    agent_execution::Control::Interrupt {
                                        including_subagents: false,
                                    },
                            },
                    } if agent == entry.delegation.as_ref().unwrap().agent => {
                        assert!(authority.token().is_none());
                        // The frozen actor examines child ownership when it
                        // executes this request, after the client's last read.
                        // A child created in that interval must refuse the
                        // interruption instead of becoming collateral work.
                        received_guards.fetch_add(1, Ordering::SeqCst);
                        write_frame(
                            &mut server_stream,
                            &ServerEnvelope {
                                version: 156,
                                request_id: Some(envelope.request_id),
                                proof: proof.clone(),
                                message: ServerMessage::Error(
                                    crate::terminal_ipc::ProtocolError::new(
                                        crate::terminal_ipc::ProtocolErrorCode::InvalidRequest,
                                        "Interrupt would stop an unreviewed background agent",
                                        false,
                                    ),
                                ),
                            },
                        )
                        .unwrap();
                        continue;
                    }
                    other => panic!("unguarded or unreviewed interruption: {other:?}"),
                };
                write_frame(
                    &mut server_stream,
                    &ServerEnvelope {
                        version: 156,
                        request_id: Some(envelope.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response { response },
                    },
                )
                .unwrap();
            }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client =
            ControlClient::from_stream_at_source_protocol(stream, deadline, 156).unwrap();
        issue(&mut client, &consent, &mut observations, deadline).unwrap();
        assert_eq!(child_stops.load(Ordering::SeqCst), 1);
        issue(&mut client, &consent, &mut observations, deadline).unwrap();
        assert_eq!(
            guarded_refusals.load(Ordering::SeqCst),
            0,
            "the child's Stop ACK does not permit the parent interrupt yet"
        );
        let mut child_settled = snapshot(&session, 0);
        child_settled.generation = 2;
        child_settled.capabilities.interrupt_stops_background = true;
        child_settled.capabilities.stop_subagent = true;
        observations
            .get_mut(&session.id)
            .unwrap()
            .project(child_settled, false)
            .unwrap();
        let error = issue(&mut client, &consent, &mut observations, deadline).unwrap_err();
        assert!(error.to_string().contains("unreviewed background"));
        assert_eq!(guarded_refusals.load(Ordering::SeqCst), 1);
        assert!(observations[&session.id].interrupted_generation.is_none());
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn sources_without_an_atomic_background_guard_fail_before_actions() {
        let (session, _, entry) = fixture();
        for missing_guard in 0..3 {
            let mut observed = observation(&session, &entry);
            match missing_guard {
                0 => observed.run = None,
                1 => {
                    observed.run.as_mut().unwrap().owner =
                        directory::Owner::Session(agent_execution::Lineage(TerminalId::new()))
                }
                _ => observed.run.as_mut().unwrap().run = None,
            }
            let mut publication = snapshot(&session, 0);
            publication.capabilities.interrupt_stops_background = true;
            assert!(
                observed
                    .project(publication, true)
                    .unwrap_err()
                    .to_string()
                    .contains("safely interrupt")
            );
        }
        let mut observed = observation(&session, &entry);
        observed.permitted_children.insert("reviewed-child".into());
        let mut publication = snapshot(&session, 0);
        publication.capabilities.interrupt_stops_background = true;
        publication.subagents.push(crate::chat::Subagent {
            id: "reviewed-child".into(),
            status: ToolStatus::Running,
            background: true,
            kind: String::new(),
            description: String::new(),
            model: None,
            waiting: false,
            detail: String::new(),
            items: Vec::new(),
        });
        assert!(
            observed
                .project(publication, true)
                .unwrap_err()
                .to_string()
                .contains("individually")
        );
    }

    #[test]
    fn owner_and_background_scope_stay_exact_while_new_turns_are_reported() {
        let (session, consent, entry) = fixture();
        let mut changed = consent.clone();
        changed.sessions[0].identity = "replacement".into();
        assert!(check_scope(&consent, &changed).is_err());
        let mut observed = observation(&session, &entry);
        let mut replacement = entry.clone();
        replacement.delegation.as_mut().unwrap().run = Some(RunId::new());
        assert!(observed.observe_run(&[replacement]).unwrap());
        assert!(observed.late_turn);
        let mut replacement_owner = entry.clone();
        replacement_owner.delegation.as_mut().unwrap().agent = AgentId::new();
        assert!(observed.observe_run(&[replacement_owner]).is_err());
        for change in 0..4 {
            let mut observed = observation(&session, &entry);
            let mut publication = snapshot(&session, 1);
            match change {
                0 => publication.turn_generation += 1,
                1 => publication.session_id = Some("replacement-thread".into()),
                2 => publication.subagents.push(crate::chat::Subagent {
                    id: "unreviewed-child".into(),
                    status: ToolStatus::Running,
                    background: true,
                    kind: String::new(),
                    description: String::new(),
                    model: None,
                    waiting: false,
                    detail: String::new(),
                    items: Vec::new(),
                }),
                _ => publication
                    .queued_messages
                    .push(crate::chat::QueuedMessage::new(
                        "queued".into(),
                        "new work".into(),
                    )),
            }
            if change == 0 || change == 3 {
                observed.project(publication, false).unwrap();
                if change == 0 {
                    assert!(observed.late_turn);
                }
                if change == 3 {
                    assert!(!observed.activity.settled);
                }
            } else {
                assert!(observed.project(publication, false).is_err());
            }
        }
    }
}
