//! Leaves messages for sessions and hands them to live conversations.
//!
//! Sending needs no authority over the recipient: like a directory read, a
//! message gives the sender no control of the session that receives it. The
//! recipient decides what to do with it, as with any prompt.
//!
//! The courier delivers a message only while its conversation is idle, so it
//! never interrupts a turn or answers a question meant for the user; a busy
//! conversation receives it after its turn. It acts as an execution client,
//! not as a person, so delivering never takes over a delegated agent.
//!
//! A conversation a usage limit stops is idle yet takes nothing, so the
//! courier also judges which ones a limit stops. It holds their messages, and
//! tells every surface showing such a conversation that it is limited and
//! until when, so the daemon gives one account of a limit everywhere.

use super::{SharedHost, SharedRegistry, caller, directory, registry};
use crate::{
    agent_execution::{
        AcceptanceError, Accepted, Authority, Lineage, Origin,
        directory::{Member, Scope, UsageLimit},
        mailbox::{
            Delivery, Handed, Handoff, Inbox, Message, Outgoing, Party, Reach, Receipt, Seal, Sent,
            State, To,
        },
    },
    chat::{self, runtime::Rejection},
    model::{MessageId, TerminalId},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Condvar, Mutex, Weak},
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// How long the courier sleeps when nothing wakes it. A conversation that
/// turned idle wakes it at once; this only bounds a missed edge.
const FALLBACK: Duration = Duration::from_secs(15);
/// How often the courier forgets messages past their retention.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);
/// The shortest handle a session can be named by, so a typo does not reach
/// a stranger that happens to share one character.
const MIN_HANDLE: usize = 4;

/// Sends as an older CLI asks to, without reporting the recipient.
pub(in crate::host) fn send(
    host: &SharedHost,
    authority: &Authority,
    outgoing: Outgoing,
) -> Result<Sent, AcceptanceError> {
    post(host, authority, outgoing).map(|delivery| delivery.sent)
}

/// Leaves a message and reports what its recipient can do with it now.
///
/// A recipient stopped by a usage limit keeps the message until the limit
/// resets, so the sender is not left waiting on a reply that cannot come
/// before then.
pub(in crate::host) fn post(
    host: &SharedHost,
    authority: &Authority,
    outgoing: Outgoing,
) -> Result<Delivery, AcceptanceError> {
    let delivery = leave(host, authority, outgoing)?;
    wake(host);
    Ok(delivery)
}

/// Runs `work` holding the shared side of the upgrade checkpoint's barrier,
/// which must see every prompt its readiness check admitted. `None`, without
/// running it, once the daemon no longer takes work.
pub(super) fn with_control<T>(host: &SharedHost, work: impl FnOnce() -> T) -> Option<T> {
    let gate = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let _control = gate.read().unwrap_or_else(|p| p.into_inner());
    crate::host::client::ensure_control_open(host).ok()?;
    Some(work())
}

/// Leaves a message as `post` does, and has an agent cagents hosts take it as
/// its next run: now, reopening its conversation if its process ended, or
/// once its current work ends.
pub(in crate::host) fn hand(
    host: &SharedHost,
    authority: &Authority,
    outgoing: Outgoing,
) -> Result<Handed, AcceptanceError> {
    let delivery = leave(host, authority, outgoing)?;
    let limited = delivery
        .recipient
        .as_ref()
        .is_some_and(|member| member.limit.is_some());
    let handoff = (|| {
        let registry = registry(host)?;
        let seal = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .mailbox_seal
            .clone()
            .ok_or("The mailbox key is unavailable")?;
        let message = &delivery.sent.message;
        let handoff = to_agent(host, &registry, &seal, message, limited)?;
        if handoff.is_some() {
            return Ok(handoff);
        }
        // The courier may have handed it over first.
        let Some(agent) = message
            .to
            .lineage
            .map(|lineage| {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .lineage_agent(lineage)
            })
            .transpose()?
            .flatten()
        else {
            return Ok(None);
        };
        let Some(accepted) = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .previous_agent_acceptance(&agent.origin, &message_run(agent.id, message, &seal))?
        else {
            return Ok(None);
        };
        Ok::<_, String>(Some(started(host, &registry, &accepted)))
    })();
    wake(host);
    // The message is left either way; a failure to start its run is how the
    // agent takes it, not a failure to send it.
    let agent = handoff.unwrap_or_else(|reason| {
        delivery
            .sent
            .message
            .to
            .lineage
            .and_then(|lineage| {
                registry(host)
                    .ok()?
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .lineage_agent(lineage)
                    .ok()
                    .flatten()
            })
            .map(|agent| Handoff::Held {
                agent: agent.id,
                reason,
            })
    });
    Ok(Handed { delivery, agent })
}

/// The run an agent takes `message` as.
fn message_run(
    agent: crate::model::AgentId,
    message: &Message,
    seal: &Seal,
) -> crate::agent_execution::Submit {
    crate::agent_execution::Submit {
        name: None,
        destination: crate::agent_execution::Destination::Existing(agent),
        prompt: seal.seal(message, message.created_ms),
        idempotency_key: format!("{}{}", crate::host::model::MESSAGE_RUN_KEY, message.id),
    }
}

/// Has the agent cagents hosts for `message`'s recipient take it as its next
/// run, when it can now. `None` when the recipient is no such agent, or the
/// message already became a run once: it then reaches the recipient as a
/// message to any other session does.
///
/// Only an agent a session or the user's CLI owns qualifies, and only until
/// a person takes it over: an automation's agent runs only what its schedule
/// gives it, and a person's conversation is theirs to prompt.
pub(super) fn to_agent(
    host: &SharedHost,
    registry: &SharedRegistry,
    seal: &Seal,
    message: &Message,
    limited: bool,
) -> Result<Option<Handoff>, String> {
    let Some(lineage) = message.to.lineage else {
        return Ok(None);
    };
    let (agent, request, previous, interactive, unverified, ahead) = {
        let store = registry.lock().unwrap_or_else(|p| p.into_inner());
        let Some(agent) = store.lineage_agent(lineage)? else {
            return Ok(None);
        };
        let request = message_run(agent.id, message, seal);
        let previous = store.previous_agent_acceptance(&agent.origin, &request)?;
        let interactive = store.agent_is_interactive(agent.id)?;
        let unverified = store.agent_has_unverified_run(agent.id)?;
        let ahead = store
            .unfinished_agent_runs(agent.id)?
            .first()
            .map(|run| run.id);
        (agent, request, previous, interactive, unverified, ahead)
    };
    let Some(crate::agent_execution::HostedSession::Chat(session)) = &agent.session else {
        return Ok(None);
    };
    if agent.closed
        || matches!(agent.origin, Origin::Automation { .. })
        || interactive
        || previous.is_some()
    {
        return Ok(None);
    }
    if unverified {
        return Ok(Some(Handoff::Held {
            agent: agent.id,
            reason: "A run of it was interrupted without proof of what it did, so cagents starts no more runs for it".into(),
        }));
    }
    if limited {
        return Ok(Some(Handoff::Held {
            agent: agent.id,
            reason: "A usage limit stops its conversation; it receives the message once the limit resets".into(),
        }));
    }
    if ahead.is_some()
        || matches!(
            super::provider(host, registry, &agent)?,
            Some(super::Provider::Busy)
        )
    {
        return Ok(Some(Handoff::Waiting {
            agent: agent.id,
            behind: ahead,
        }));
    }
    let accepted = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .hand_message_to_agent(
            message.id,
            session.id,
            &agent.origin,
            &request,
            super::now(),
        );
    let accepted = match accepted {
        Ok(Some(accepted)) => accepted,
        Ok(None) => return Ok(None),
        Err(reason) => {
            return Ok(Some(Handoff::Held {
                agent: agent.id,
                reason,
            }));
        }
    };
    super::announce(host, registry);
    Ok(Some(started(host, registry, &accepted)))
}

/// How the agent takes a message as `accepted`, once cagents tried to start
/// it.
fn started(host: &SharedHost, registry: &SharedRegistry, accepted: &Accepted) -> Handoff {
    let (agent, run) = (accepted.agent.id, accepted.run.id);
    let reason = match super::start(host, accepted) {
        Ok(start) => return Handoff::Run { agent, run, start },
        Err(reason) => reason,
    };
    let ended = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_run(run)
        .map(|run| (run.state, run.provider_key.is_some()));
    match ended {
        // The provider may have the message, so it stays given.
        Ok((crate::agent_execution::State::Interrupted, _)) => {
            Handoff::Unknown { agent, run, reason }
        }
        // The provider acknowledged the turn: the agent took the message
        // as this run, which has since ended.
        Ok((_, true)) => Handoff::Run {
            agent,
            run,
            start: crate::agent_execution::Start::Sent { resumed: false },
        },
        // Its run ended unsent and gave the message back, which reaches the
        // conversation as a message.
        _ => Handoff::Held { agent, reason },
    }
}

fn leave(
    host: &SharedHost,
    authority: &Authority,
    outgoing: Outgoing,
) -> Result<Delivery, AcceptanceError> {
    outgoing.validate()?;
    let (_, session) = caller(host, authority)?;
    // Read first, as the listing takes the registry lock itself.
    let listing = directory::list(
        host,
        authority,
        directory::Wanted::Scope(Scope::All),
        true,
        true,
    )?;
    let live = directory::live_sessions(host);
    let registry = registry(host)?;
    let now = super::now();
    let (message, reach, recipient) = {
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let model = registry.model()?;
        // An agent's session can be named while its process is stopped.
        let bound = model
            .agents
            .iter()
            .filter_map(|agent| agent.session.as_ref().map(|session| session.id()));
        let lineages = registry.session_lineages(
            live.iter()
                .map(|entry| entry.id)
                .chain(session)
                .chain(bound),
        )?;
        let lineage_of = |id: TerminalId| lineages.get(&id).copied().unwrap_or(Lineage(id));
        // As in a directory listing, a delegated agent without a title of its
        // own is named by the channel its prompt titled.
        let title_of = |id: TerminalId| {
            live.iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| entry.title.clone())
                .or_else(|| {
                    let agent = model.agents.iter().find(|agent| {
                        agent
                            .session
                            .as_ref()
                            .is_some_and(|session| session.id() == id)
                    })?;
                    agent
                        .channel()
                        .and_then(|channel| model.channel(channel))
                        .map(|channel| channel.title.clone())
                })
                .filter(|title| !title.is_empty())
        };
        let from = Party {
            lineage: session.map(lineage_of),
            session,
            title: session.and_then(title_of),
        };
        let (to, reply_to) = match &outgoing.to {
            To::Session(handle) => (
                Some(
                    resolve(&model.agents, &live, &lineage_of, handle)
                        .map_err(|error| stopped(&listing, handle).unwrap_or(error))?,
                ),
                None,
            ),
            To::Parent => (parent(&registry, session, from.lineage)?, None),
            To::Sender(handle) => {
                let original = registry.received_message(from.lineage, handle)?;
                (original.from.lineage, Some(original.id))
            }
        };
        if to == from.lineage {
            return Err("That is you; a message must go to another session".into());
        }
        let reach = match to {
            None => Reach::Inbox,
            Some(to) => {
                let members: Vec<_> = live
                    .iter()
                    .filter(|entry| lineage_of(entry.id) == to)
                    .collect();
                if members
                    .iter()
                    .any(|entry| entry.surface == crate::agent_execution::directory::Surface::Chat)
                {
                    Reach::Conversation
                } else if members.is_empty() {
                    Reach::Offline
                } else {
                    Reach::Inbox
                }
            }
        };
        let recipient = to.and_then(|to| recipient(&listing, to));
        let limited = recipient
            .as_ref()
            .is_some_and(|member| member.limit.is_some());
        let to_title = to.and_then(|to| {
            live.iter()
                .find(|entry| lineage_of(entry.id) == to)
                .and_then(|entry| title_of(entry.id))
        });
        let message = Message {
            id: MessageId::new(),
            from,
            to: Party {
                lineage: to,
                session: None,
                title: to_title,
            },
            reply_to,
            body: outgoing.body,
            // No one can answer before a session of it starts, or before
            // its usage limit resets, so the asker does not wait and a later
            // reply is not held for it.
            wait_until_ms: outgoing
                .wait_ms
                .filter(|_| reach != Reach::Offline && !limited)
                .map(|wait| now.saturating_add(wait)),
            state: State::Pending,
            created_ms: now,
        };
        registry.post_message(&message)?;
        (message, reach, recipient)
    };
    Ok(Delivery {
        sent: Sent { message, reach },
        recipient,
    })
}

/// The session of `lineage` a message reaches, as the listing reports it: a
/// live conversation before a terminal, and either before one whose provider
/// exited.
fn recipient(listing: &directory::Listing, lineage: Lineage) -> Option<Member> {
    listing
        .members()
        .filter(|member| member.session.lineage == lineage)
        .min_by_key(|member| {
            (
                member.exited,
                member.session.surface != crate::agent_execution::directory::Surface::Chat,
            )
        })
        .cloned()
}

/// Why `handle` names no live session when it names one whose provider
/// exited: such a session cannot take a message until it is resumed.
fn stopped(listing: &directory::Listing, handle: &str) -> Option<String> {
    let handle = handle.to_ascii_lowercase();
    if handle.len() < MIN_HANDLE {
        return None;
    }
    let member = listing.members().find(|member| {
        member.exited
            && (member.session.id.to_string().starts_with(&handle)
                || member.session.lineage.to_string().starts_with(&handle))
    })?;
    let mut reason = format!(
        "Session {} has stopped: its provider exited, so it cannot receive messages",
        member.session.id
    );
    if let Some(failure) = &member.failure {
        reason.push_str(&format!(" ({failure})"));
    }
    reason.push_str(
        ". Resume its conversation first, or message another session from `cagents sessions --all`",
    );
    Some(reason)
}

pub(in crate::host) fn inbox(
    host: &SharedHost,
    authority: &Authority,
    query: Inbox,
) -> Result<Vec<Message>, String> {
    let (_, session) = caller(host, authority)?;
    let registry = registry(host)?;
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let lineage = session
        .map(|session| {
            registry
                .session_lineage(session)
                .map(|lineage| lineage.unwrap_or(Lineage(session)))
        })
        .transpose()?;
    let result = registry.read_messages(lineage, session, &query, super::now());
    drop(registry);
    if result.is_ok() {
        wake(host);
    }
    result
}

pub(in crate::host) fn peek(
    host: &SharedHost,
    authority: &Authority,
    query: Inbox,
) -> Result<Vec<Message>, String> {
    let (_, session) = caller(host, authority)?;
    let registry = registry(host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let lineage = session
        .map(|session| {
            registry
                .session_lineage(session)
                .map(|lineage| lineage.unwrap_or(Lineage(session)))
        })
        .transpose()?;
    let mut messages = registry.peek_messages(lineage, &query)?;
    for message in &mut messages {
        if message.state == State::Pending {
            // This returned copy is addressed to the authenticated reader.
            // Persisting delivery waits for its explicit receipt.
            message.to.session = session;
        }
    }
    Ok(messages)
}

pub(in crate::host) fn acknowledge(
    host: &SharedHost,
    authority: &Authority,
    receipt: Receipt,
) -> Result<(), String> {
    receipt.validate()?;
    let (_, session) = caller(host, authority)?;
    let registry = registry(host)?;
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let lineage = session
        .map(|session| {
            registry
                .session_lineage(session)
                .map(|lineage| lineage.unwrap_or(Lineage(session)))
        })
        .transpose()?;
    let result = registry.acknowledge_messages(lineage, session, &receipt, super::now());
    drop(registry);
    if result.is_ok() {
        wake(host);
    }
    result
}

/// Only an unambiguous currently live provider terminal gets a new alert.
/// Offline lineages retain prior attention, and chats keep the courier path.
fn terminal_attention(
    heads: &std::collections::BTreeMap<Lineage, MessageId>,
    live: &[(
        TerminalId,
        crate::agent_execution::directory::Surface,
        Option<crate::agent::Provider>,
    )],
    lineages: &std::collections::BTreeMap<TerminalId, Lineage>,
) -> Vec<crate::notifications::Event> {
    use crate::notifications::{Event, Evidence, Kind, Target};
    // Work scales with currently hosted sessions, not every retained offline
    // recipient. Any second live member makes a lineage ambiguous.
    let mut recipients = std::collections::BTreeMap::new();
    for (id, surface, provider) in live {
        let candidate = (*surface == crate::agent_execution::directory::Surface::Terminal
            && provider.is_some())
        .then_some(*id);
        recipients
            .entry(lineages.get(id).copied().unwrap_or(Lineage(*id)))
            .and_modify(|owner| *owner = None)
            .or_insert(candidate);
    }
    recipients
        .into_iter()
        .filter_map(|(lineage, owner)| {
            let id = owner?;
            let message = heads.get(&lineage)?;
            Some(Event {
                content: None,
                target: Target::SessionInbox(lineage),
                source_session: Some(id),
                provider: None,
                directory: None,
                destination: None,
                evidence: Evidence {
                    key: message.to_string(),
                    kind: Kind::MessageReceived,
                },
            })
        })
        .collect()
}

/// The one lineage `handle` names among live agent sessions and the open
/// agents cagents hosts, including one whose process stopped: by session ID,
/// lineage, or delegated agent ID, whole or a prefix.
fn resolve(
    agents: &[crate::agent_execution::Agent],
    live: &[directory::Live],
    lineage_of: &impl Fn(TerminalId) -> Lineage,
    handle: &str,
) -> Result<Lineage, String> {
    let handle = handle.to_ascii_lowercase();
    if handle.len() < MIN_HANDLE {
        return Err(format!(
            "Name the session by at least {MIN_HANDLE} characters of its ID"
        ));
    }
    let matches = |id: &dyn std::fmt::Display| id.to_string().starts_with(&handle);
    let mut found = BTreeSet::new();
    for entry in live.iter().filter(|entry| entry.provider.is_some()) {
        let lineage = lineage_of(entry.id);
        let agent = agents.iter().find(|agent| {
            agent
                .session
                .as_ref()
                .is_some_and(|session| session.id() == entry.id)
        });
        if matches(&entry.id) || matches(&lineage) || agent.is_some_and(|agent| matches(&agent.id))
        {
            found.insert(lineage);
        }
    }
    for agent in agents.iter().filter(|agent| !agent.closed) {
        let Some(session) = agent.session.as_ref().map(|session| session.id()) else {
            continue;
        };
        let lineage = lineage_of(session);
        if matches(&session) || matches(&lineage) || matches(&agent.id) {
            found.insert(lineage);
        }
    }
    match found.len() {
        1 => Ok(found.pop_first().expect("one lineage")),
        0 => Err(format!(
            "No session or agent matches {handle}; list them with `cagents sessions --all`"
        )),
        _ => Err(format!(
            "{} sessions match {handle}; give more of the ID",
            found.len()
        )),
    }
}

/// Where the caller's parent receives messages: the session whose agent it
/// is, or the user when the user's own CLI started it.
fn parent(
    registry: &super::Registry,
    session: Option<TerminalId>,
    lineage: Option<Lineage>,
) -> Result<Option<Lineage>, String> {
    let Some(session) = session else {
        return Err("Only a session has a parent; this caller is the user's own CLI".into());
    };
    // A relaunched agent keeps its record under the session first bound to it.
    let agent = match registry.agent_for_session(session)? {
        Some(agent) => Some(agent),
        None => match lineage {
            Some(lineage) if lineage.0 != session => registry.agent_for_session(lineage.0)?,
            _ => None,
        },
    };
    match agent.map(|agent| agent.origin) {
        Some(Origin::Child { parent }) => Ok(Some(parent)),
        Some(Origin::User) => Ok(None),
        Some(Origin::Automation { .. }) => {
            Err("An automation started this session; it has no parent to message".into())
        }
        None => Err(
            "This session was not started as an agent; message a session by its ID instead".into(),
        ),
    }
}

pub(in crate::host) fn wake(host: &SharedHost) {
    let courier = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox
        .clone();
    if let Some(courier) = courier {
        courier.wake();
    }
}

#[derive(Default)]
struct WakeState {
    generation: u64,
    stopped: bool,
}

struct Inner {
    host: Weak<crate::host::state::HostLock>,
    registry: SharedRegistry,
    seal: Arc<Seal>,
    state: Mutex<WakeState>,
    changed: Condvar,
    /// What each conversation's subscribers were last told of its limit.
    limits: Mutex<BTreeMap<TerminalId, Told>>,
    #[cfg(test)]
    scanned_generation: Mutex<Option<u64>>,
    #[cfg(test)]
    scanned: Condvar,
}

/// A conversation's usage limit as the courier last judged it, and the
/// subscribers that know it. A subscriber starts out knowing of no limit.
struct Told {
    limit: Option<UsageLimit>,
    clients: BTreeSet<crate::host::state::ClientId>,
}

/// Wakes the courier. Cheap enough to call on every activity edge.
#[derive(Clone)]
pub(in crate::host) struct Handle(Arc<Inner>);

impl Handle {
    pub(in crate::host) fn wake(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        state.generation = state.generation.wrapping_add(1);
        self.0.changed.notify_one();
    }
}

/// Owns the courier thread; dropping it stops the thread.
pub(in crate::host) struct Courier {
    handle: Handle,
    thread: Option<JoinHandle<()>>,
}

impl Courier {
    /// A completed scan witness only: waiting never wakes or advances delivery.
    #[cfg(test)]
    pub(in crate::host) fn wait_for_scan(&self, generation: u64) {
        let completed = self
            .handle
            .0
            .scanned_generation
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let (completed, _) = self
            .handle
            .0
            .scanned
            .wait_timeout_while(completed, Duration::from_secs(5), |completed| {
                completed.is_none_or(|completed| completed < generation)
            })
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            completed.is_some_and(|completed| completed >= generation),
            "courier did not finish the requested scan"
        );
    }

    #[cfg(test)]
    pub(in crate::host) fn wake_and_wait_for_scan(&self) {
        self.handle.wake();
        let generation = self
            .handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation;
        self.wait_for_scan(generation);
    }

    pub(in crate::host) fn start(host: &SharedHost) -> Result<Self, String> {
        let registry = registry(host)?;
        let seal = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .mailbox_seal
            .clone()
            .ok_or("The mailbox key is unavailable")?;
        let handle = Handle(Arc::new(Inner {
            host: Arc::downgrade(host),
            registry,
            seal,
            state: Mutex::new(WakeState::default()),
            changed: Condvar::new(),
            limits: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            scanned_generation: Mutex::new(None),
            #[cfg(test)]
            scanned: Condvar::new(),
        }));
        let owner = handle.clone();
        let thread = std::thread::Builder::new()
            .name("message-courier".into())
            .spawn(move || owner.run())
            .map_err(|error| error.to_string())?;
        host.lock().unwrap_or_else(|p| p.into_inner()).mailbox = Some(handle.clone());
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }
}

impl Drop for Courier {
    fn drop(&mut self) {
        self.handle
            .0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped = true;
        self.handle.0.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Handle {
    fn run(&self) {
        self.run_with(|| self.prune(), |host| self.deliver(host));
    }

    fn run_with(
        &self,
        mut prune: impl FnMut() -> Result<(), String>,
        mut deliver: impl FnMut(&SharedHost) -> Result<Option<u64>, String>,
    ) {
        let mut pruned_at: Option<Instant> = None;
        loop {
            if pruned_at.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
                pruned_at = Some(Instant::now());
                if let Err(error) = crate::host::service::attempt("message pruning", &mut prune) {
                    tracing::warn!(%error, "cannot forget old messages");
                }
            }
            let observed = {
                let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.stopped {
                    return;
                }
                state.generation
            };
            let Some(host) = self.0.host.upgrade() else {
                return;
            };
            let delay = match crate::host::service::attempt("message delivery", || deliver(&host)) {
                Ok(Some(released)) => {
                    Duration::from_millis(released.saturating_sub(super::now())).min(FALLBACK)
                }
                Ok(None) => FALLBACK,
                Err(error) => {
                    tracing::warn!(%error, "cannot deliver messages");
                    FALLBACK
                }
            };
            drop(host);
            #[cfg(test)]
            {
                *self
                    .0
                    .scanned_generation
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(observed);
                self.0.scanned.notify_all();
            }
            let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            let (state, _) = self
                .0
                .changed
                .wait_timeout_while(state, delay, |state| {
                    !state.stopped && state.generation == observed
                })
                .unwrap_or_else(|p| p.into_inner());
            if state.stopped {
                return;
            }
        }
    }

    fn prune(&self) -> Result<(), String> {
        let before = super::now().saturating_sub(crate::agent_execution::mailbox::RETENTION_MS);
        let pruned = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .prune_messages(before)?;
        if pruned > 0 {
            tracing::info!(pruned, "forgot messages past their retention");
        }
        Ok(())
    }

    /// Judges which conversations a usage limit stops, telling every
    /// subscriber of one whose judgement changed, and each new subscriber,
    /// what it now is. Returns the limits, by conversation.
    fn publish_limits(&self, host: &SharedHost, now: u64) -> BTreeMap<TerminalId, UsageLimit> {
        let chats: Vec<(TerminalId, crate::agent::Provider, chat::runtime::Handle)> = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state
                .chats
                .iter()
                .filter(|(_, record)| !record.handle.is_completed())
                .map(|(id, record)| (*id, record.session.launch.provider, record.handle.clone()))
                .collect()
        };
        // Accounts are read once a scan, and only if a provider reported a
        // limit; the judgement reads no transcript.
        let mut accounts = None;
        let limits: BTreeMap<_, _> = chats
            .iter()
            .filter_map(|(id, provider, handle)| {
                directory::chat_limit(
                    *provider,
                    handle,
                    || {
                        accounts
                            .get_or_insert_with(|| super::account_info::Accounts::load(now))
                            .as_ref()
                            .and_then(|accounts| {
                                accounts.of(*provider, &handle.execution_environment().0)
                            })
                    },
                    now,
                )
                .map(|limit| (*id, limit))
            })
            .collect();
        let mut told = self.0.limits.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let proof = state.proof().clone();
        let crate::host::state::HostState { chats, clients, .. } = &mut *state;
        told.retain(|id, _| chats.contains_key(id));
        for (id, record) in chats.iter() {
            let limit = limits.get(id);
            let known = told.entry(*id).or_insert_with(|| Told {
                limit: None,
                clients: record.subscribers.keys().copied().collect(),
            });
            if known.limit.as_ref() != limit {
                known.limit = limit.cloned();
                known.clients.clear();
            }
            known
                .clients
                .retain(|client| record.subscribers.contains_key(client));
            for &client in record.subscribers.keys() {
                if known.clients.contains(&client) {
                    continue;
                }
                let Some(delivery) = clients.get(&client) else {
                    continue;
                };
                let message = crate::terminal_ipc::ServerMessage::ChatLimit {
                    chat_id: *id,
                    limit: known.limit.clone(),
                };
                if delivery
                    .push(crate::terminal_ipc::ServerEnvelope {
                        version: crate::terminal_ipc::PROTOCOL_VERSION,
                        request_id: None,
                        proof: proof.clone(),
                        message,
                    })
                    .is_ok()
                {
                    known.clients.insert(client);
                }
            }
        }
        limits
    }

    /// Gives each lineage with a live, idle conversation its oldest waiting
    /// message. Returns when the courier must look again: when the next held
    /// reply is released or a known usage limit resets.
    fn deliver(&self, host: &SharedHost) -> Result<Option<u64>, String> {
        let limits = self.publish_limits(host, super::now());
        let reset = limits
            .values()
            .filter_map(|limit| limit.resets_at)
            .filter(|at| *at >= 0)
            .map(|at| (at as u64).saturating_mul(1000))
            .min();
        self.start_queued(host);
        let released = self.deliver_messages(host, &limits)?;
        Ok(match (released, reset) {
            (Some(released), Some(reset)) => Some(released.min(reset)),
            (released, reset) => released.or(reset),
        })
    }

    /// Starts the prompts agents hold behind work that has since ended.
    fn start_queued(&self, host: &SharedHost) {
        if let Err(error) = super::start_queued(host) {
            tracing::warn!(%error, "cannot start queued agent prompts");
        }
    }

    fn deliver_messages(
        &self,
        host: &SharedHost,
        limits: &BTreeMap<TerminalId, UsageLimit>,
    ) -> Result<Option<u64>, String> {
        let live = directory::live_sessions(host);
        let (pending, released, heads, pending_lineages, lineages) = {
            let mut registry = self.0.registry.lock().unwrap_or_else(|p| p.into_inner());
            let (pending, released) = registry.deliverable_messages(super::now())?;
            let heads = registry.reconcile_message_attention(super::now())?;
            let pending_lineages = registry.pending_message_lineages()?;
            let lineages = registry.session_lineages(live.iter().map(|entry| entry.id))?;
            (pending, released, heads, pending_lineages, lineages)
        };
        let notifications = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .notifications
            .clone();
        if let Some(notifications) = notifications {
            let events = terminal_attention(
                &heads,
                &live
                    .iter()
                    .map(|entry| (entry.id, entry.surface, entry.provider))
                    .collect::<Vec<_>>(),
                &lineages,
            );
            notifications.reconcile_inboxes(pending_lineages, events);
        }
        if pending.is_empty() {
            return Ok(released);
        }
        let chats: Vec<(TerminalId, crate::agent::Provider, chat::runtime::Handle)> = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state
                .chats
                .iter()
                .filter(|(_, record)| !record.handle.is_completed())
                .map(|(id, record)| (*id, record.session.launch.provider, record.handle.clone()))
                .collect()
        };
        let lineages = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_lineages(chats.iter().map(|(id, _, _)| *id))?;
        for message in pending {
            let Some(recipient) = message.to.lineage else {
                continue;
            };
            let limited = chats
                .iter()
                .any(|(id, _, _)| lineages.get(id) == Some(&recipient) && limits.contains_key(id));
            // One message at a time, as reopening an agent's conversation for
            // one can take a while and must not hold off an upgrade for all.
            let Some(delivered) = with_control(host, || {
                self.deliver_message(
                    host, &message, recipient, limited, &chats, &lineages, limits,
                )
            }) else {
                return Ok(released);
            };
            delivered?;
        }
        Ok(released)
    }

    /// Gives one pending message to its recipient: as a run of the agent
    /// cagents hosts for it, or into its idle conversation.
    #[allow(clippy::too_many_arguments)]
    fn deliver_message(
        &self,
        host: &SharedHost,
        message: &Message,
        recipient: crate::agent_execution::Lineage,
        limited: bool,
        chats: &[(TerminalId, crate::agent::Provider, chat::runtime::Handle)],
        lineages: &BTreeMap<TerminalId, crate::agent_execution::Lineage>,
        limits: &BTreeMap<TerminalId, UsageLimit>,
    ) -> Result<(), String> {
        {
            match to_agent(host, &self.0.registry, &self.0.seal, message, limited) {
                Ok(Some(Handoff::Run { .. } | Handoff::Waiting { .. })) => return Ok(()),
                // Its run has it, perhaps already with the provider.
                Ok(Some(Handoff::Unknown { reason, run, .. })) => {
                    tracing::warn!(%reason, %run, message = %message.id, "a message's run was interrupted");
                    return Ok(());
                }
                // Not as a run, but as a message, as a person's conversation
                // takes one.
                Ok(Some(Handoff::Held { reason, .. })) => {
                    tracing::debug!(%reason, message = %message.id, "an agent takes a message as a message");
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, message = %message.id, "cannot hand a message to its agent");
                    return Ok(());
                }
            }
        }
        let Some((session, _, handle)) = chats.iter().find(|(id, _, handle)| {
            lineages.get(id) == Some(&recipient) && handle.is_idle() && !limits.contains_key(id)
        }) else {
            return Ok(());
        };
        self.hand_over(message, *session, handle)
    }

    /// Claims the message for `session` before prompting, so that an inbox
    /// read racing the prompt cannot return it too, and releases the claim if
    /// the conversation refuses the prompt. A prompt the conversation took
    /// but did not acknowledge in time may still run, so its message stays
    /// delivered: a message reaches a conversation at most once.
    fn hand_over(
        &self,
        message: &Message,
        session: TerminalId,
        handle: &chat::runtime::Handle,
    ) -> Result<(), String> {
        let registry = &self.0.registry;
        if !registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .deliver_message(message.id, session, super::now())?
        {
            return Ok(());
        }
        if let Some(host) = self.0.host.upgrade() {
            super::given_work(&host, session);
        }
        match handle.try_idle_prompt(self.0.seal.seal(message, super::now())) {
            Ok(()) => {}
            Err(Rejection::Refused(error)) => {
                // Most often a person's own prompt won the idle conversation;
                // the message waits for the turn that prompt starts to finish.
                tracing::debug!(%error, message = %message.id, %session, "conversation refused a message");
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .undeliver_message(message.id, session)?;
            }
            Err(Rejection::Unacknowledged(error)) => {
                tracing::warn!(%error, message = %message.id, %session, "conversation did not acknowledge a message");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn courier_requeues_a_claim_if_a_request_arrives_after_its_idle_scan() {
        use crate::{
            agent_execution::mailbox::Party, chat::runtime::courier_tests::RequestRaceFixture,
        };

        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(Mutex::new(
            crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
        ));
        let session = TerminalId::new();
        let message = Message {
            id: crate::model::MessageId::new(),
            from: Party::user(),
            to: Party {
                lineage: Some(Lineage(session)),
                session: None,
                title: None,
            },
            reply_to: None,
            body: "Deliver exactly once after the outstanding approval clears".into(),
            wait_until_ms: None,
            state: State::Pending,
            created_ms: super::super::now(),
        };
        registry.lock().unwrap().post_message(&message).unwrap();
        let courier = Handle(Arc::new(Inner {
            host: Weak::new(),
            registry: registry.clone(),
            seal: Arc::new(Seal::new([7; 32])),
            state: Mutex::default(),
            changed: Condvar::new(),
            limits: Mutex::default(),
            scanned_generation: Mutex::new(None),
            scanned: Condvar::new(),
        }));
        let stored = || {
            registry
                .lock()
                .unwrap()
                .message(message.id)
                .unwrap()
                .unwrap()
        };
        let mut actor = RequestRaceFixture::new();
        // This is the courier's real eligibility observation. Compact's gated
        // adapter step will publish a request before the queued prompt runs.
        assert!(actor.handle.is_idle());
        actor.begin_request();
        let delivery_courier = courier.clone();
        let delivery_handle = actor.handle.clone();
        let delivery_message = message.clone();
        struct Delivery(Option<std::thread::JoinHandle<Result<(), String>>>);
        impl Delivery {
            fn finish(mut self) {
                self.0.take().unwrap().join().unwrap().unwrap();
            }
        }
        impl Drop for Delivery {
            fn drop(&mut self) {
                if let Some(worker) = self.0.take() {
                    // The actor gate has a bounded failure path even if an
                    // earlier assertion unwinds before explicit release.
                    let _ = worker.join();
                }
            }
        }
        let delivery = Delivery(Some(std::thread::spawn(move || {
            delivery_courier.hand_over(&delivery_message, session, &delivery_handle)
        })));
        actor.wait_for_queued_prompt();
        assert_eq!(
            stored().state,
            State::Delivered,
            "the real claim preceded dequeue"
        );
        actor.release_request();
        delivery.finish();
        let refused = stored();
        assert_eq!(refused.id, message.id);
        assert_eq!(
            refused.state,
            State::Pending,
            "refused mail must remain retryable"
        );
        assert_eq!(refused.to.session, None);
        assert_eq!(
            actor.prompt_count(),
            0,
            "an unanswered request cannot admit mail"
        );
        assert_eq!(actor.handle.requests().len(), 1);

        courier.hand_over(&message, session, &actor.handle).unwrap();
        assert_eq!(stored().state, State::Pending);
        assert_eq!(
            actor.prompt_count(),
            0,
            "an unchanged request stays protected"
        );
        actor.clear_request();
        assert!(actor.handle.is_idle());
        courier.hand_over(&message, session, &actor.handle).unwrap();
        let delivered = stored();
        assert_eq!(delivered.state, State::Delivered);
        assert_eq!(delivered.to.session, Some(session));
        assert_eq!(actor.prompt_count(), 1);
        courier.hand_over(&message, session, &actor.handle).unwrap();
        assert_eq!(stored(), delivered);
        assert_eq!(
            actor.prompt_count(),
            1,
            "a settled message cannot be resubmitted"
        );
        actor.close();
    }

    #[test]
    fn courier_keeps_delivery_after_actor_step_fails_with_queued_input() {
        use crate::{
            agent_execution::mailbox::Party, chat::runtime::courier_tests::RequestRaceFixture,
        };
        let directory = tempfile::tempdir().unwrap();
        let registry = Arc::new(Mutex::new(
            crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
        ));
        let session = TerminalId::new();
        let message = Message {
            id: MessageId::new(),
            from: Party::user(),
            to: Party {
                lineage: Some(Lineage(session)),
                session: None,
                title: None,
            },
            reply_to: None,
            body: "Do not replay input after a failed actor step".into(),
            wait_until_ms: None,
            state: State::Pending,
            created_ms: super::super::now(),
        };
        registry.lock().unwrap().post_message(&message).unwrap();
        let courier = Handle(Arc::new(Inner {
            host: Weak::new(),
            registry: registry.clone(),
            seal: Arc::new(Seal::new([8; 32])),
            state: Mutex::default(),
            changed: Condvar::new(),
            limits: Mutex::default(),
            scanned_generation: Mutex::new(None),
            scanned: Condvar::new(),
        }));
        let mut actor = RequestRaceFixture::with_input_failure(true);
        courier.hand_over(&message, session, &actor.handle).unwrap();
        let delivered = registry
            .lock()
            .unwrap()
            .message(message.id)
            .unwrap()
            .unwrap();
        assert_eq!(delivered.state, State::Delivered);
        assert_eq!(delivered.to.session, Some(session));
        // A later idle actor must not receive a replacement prompt, even if
        // the first actor exited before its caller could observe provider I/O.
        let mut successor = RequestRaceFixture::new();
        courier
            .hand_over(&message, session, &successor.handle)
            .unwrap();
        assert_eq!(actor.prompt_count(), 1);
        assert_eq!(successor.prompt_count(), 0);
        actor.close();
        successor.close();
    }

    #[test]
    fn retained_idle_uncertainty_keeps_the_courier_claim_and_explicit_refusal_requeues() {
        use crate::chat::runtime::{Handle as RuntimeHandle, RetainedInputOutcome};
        for outcome in [
            RetainedInputOutcome::Acknowledged(Err("provider refused idle input".into())),
            RetainedInputOutcome::Unknown("provider effect remains unknown".into()),
            RetainedInputOutcome::Unavailable,
            RetainedInputOutcome::Disconnected,
        ] {
            let refused = matches!(&outcome, RetainedInputOutcome::Acknowledged(Err(_)));
            let directory = tempfile::tempdir().unwrap();
            let registry = Arc::new(Mutex::new(
                crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            ));
            let session = TerminalId::new();
            let message = Message {
                id: MessageId::new(),
                from: Party::user(),
                to: Party {
                    lineage: Some(Lineage(session)),
                    session: None,
                    title: None,
                },
                reply_to: None,
                body: "A courier input must not be regenerated after an uncertain effect".into(),
                wait_until_ms: None,
                state: State::Pending,
                created_ms: super::super::now(),
            };
            registry.lock().unwrap().post_message(&message).unwrap();
            let courier = Handle(Arc::new(Inner {
                host: Weak::new(),
                registry: registry.clone(),
                seal: Arc::new(Seal::new([9; 32])),
                state: Mutex::default(),
                changed: Condvar::new(),
                limits: Mutex::default(),
                scanned_generation: Mutex::new(None),
                scanned: Condvar::new(),
            }));
            let stored = || {
                registry
                    .lock()
                    .unwrap()
                    .message(message.id)
                    .unwrap()
                    .unwrap()
            };
            let (actor, answer) = RuntimeHandle::fixture_retained_input();
            let delivery_courier = courier.clone();
            let delivery_actor = actor.clone();
            let delivery_message = message.clone();
            let delivery = std::thread::spawn(move || {
                delivery_courier.hand_over(&delivery_message, session, &delivery_actor)
            });
            // This completes a genuinely pending Remote IdlePrompt envelope;
            // hand_over has already committed its claim before invoking it.
            assert_eq!(answer(outcome), 1);
            delivery.join().unwrap().unwrap();
            let settled = stored();
            if refused {
                assert_eq!(settled.state, State::Pending);
                assert_eq!(settled.to.session, None);
                assert!(actor.pending_retained_inputs().is_empty());
                // A later positive receipt can deliver the same durable mail.
                let (retry_actor, answer) = RuntimeHandle::fixture_retained_input();
                let retry_courier = courier.clone();
                let retry_message = message.clone();
                let retry = std::thread::spawn(move || {
                    retry_courier.hand_over(&retry_message, session, &retry_actor)
                });
                assert_eq!(answer(RetainedInputOutcome::Acknowledged(Ok(()))), 1);
                retry.join().unwrap().unwrap();
                assert_eq!(stored().state, State::Delivered);
            } else {
                assert_eq!(settled.state, State::Delivered);
                assert_eq!(settled.to.session, Some(session));
                let retained = actor.pending_retained_inputs();
                assert_eq!(retained.len(), 1);
                assert_eq!(retained[0].sequence, 1);
                // The real courier path must observe Delivered and return
                // before allocating any replacement input envelope.
                courier.hand_over(&message, session, &actor).unwrap();
                assert_eq!(stored(), settled);
                assert_eq!(actor.pending_retained_inputs(), retained);
            }
        }
    }

    #[test]
    fn terminal_attention_requires_a_unique_live_provider_and_tracks_successor() {
        use crate::{agent::Provider, agent_execution::directory::Surface};
        let original = TerminalId::new();
        let successor = TerminalId::new();
        let lineage = Lineage(original);
        let heads = std::collections::BTreeMap::from([(lineage, MessageId::new())]);
        let lineages =
            std::collections::BTreeMap::from([(successor, lineage), (original, lineage)]);
        let terminal = (successor, Surface::Terminal, Some(Provider::Claude));
        let events = terminal_attention(&heads, &[terminal], &lineages);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_session(), Some(successor));
        assert_eq!(
            events[0].target,
            crate::notifications::Target::SessionInbox(lineage)
        );
        assert!(events[0].validate().is_ok());
        assert!(terminal_attention(&heads, &[], &lineages).is_empty());
        assert!(
            terminal_attention(
                &heads,
                &[(successor, Surface::Chat, Some(Provider::Claude))],
                &lineages
            )
            .is_empty()
        );
        assert!(
            terminal_attention(&heads, &[(successor, Surface::Terminal, None)], &lineages)
                .is_empty()
        );
        assert!(
            terminal_attention(
                &heads,
                &[
                    terminal,
                    (original, Surface::Terminal, Some(Provider::Claude))
                ],
                &lineages
            )
            .is_empty()
        );
    }

    #[test]
    fn courier_survives_prune_and_delivery_panics_and_stops() {
        let directory = tempfile::tempdir().unwrap();
        let host = crate::host::service::test_host();
        let handle = Handle(Arc::new(Inner {
            host: Arc::downgrade(&host),
            registry: Arc::new(Mutex::new(
                crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            )),
            seal: Arc::new(Seal::new([7; 32])),
            state: Mutex::default(),
            changed: Condvar::new(),
            limits: Mutex::default(),
            scanned_generation: Mutex::new(None),
            scanned: Condvar::new(),
        }));
        let owner = handle.clone();
        let (reported, received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut deliveries = 0;
            owner.run_with(
                || panic!("injected pruning failure"),
                |host| {
                    deliveries += 1;
                    if deliveries == 1 {
                        owner.wake();
                        let _guard = host.lock().unwrap();
                        let _reentrant = host.lock().unwrap();
                        unreachable!();
                    }
                    // The guard from the failed attempt has unwound.
                    let _guard = host.lock().unwrap();
                    owner.0.state.lock().unwrap().stopped = true;
                    reported.send(deliveries).unwrap();
                    Ok(None)
                },
            );
        });
        assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        thread.join().unwrap();
    }
}
