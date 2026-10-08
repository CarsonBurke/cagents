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

use super::{SharedHost, SharedRegistry, caller, directory, registry};
use crate::{
    agent_execution::{
        Authority, Lineage, Origin,
        mailbox::{Inbox, Message, Outgoing, Party, Reach, Seal, Sent, State, To},
    },
    chat::{self, Command, PromptMode, runtime::Rejection},
    model::{MessageId, TerminalId},
};
use std::{
    collections::BTreeSet,
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

pub(in crate::host) fn send(
    host: &SharedHost,
    authority: &Authority,
    outgoing: Outgoing,
) -> Result<Sent, String> {
    outgoing.validate()?;
    let (_, session) = caller(host, authority)?;
    let live = directory::live_sessions(host);
    let registry = registry(host)?;
    let now = super::now();
    let (message, reach) = {
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let lineages =
            registry.session_lineages(live.iter().map(|entry| entry.id).chain(session))?;
        let lineage_of = |id: TerminalId| lineages.get(&id).copied().unwrap_or(Lineage(id));
        let model = registry.model()?;
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
                    Some(model.channel(agent.channel)?.title.clone())
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
                Some(resolve(&model.agents, &live, &lineage_of, handle)?),
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
            // No one can answer before a session of it starts, so the asker
            // does not wait and a later reply is not held for it.
            wait_until_ms: outgoing
                .wait_ms
                .filter(|_| reach != Reach::Offline)
                .map(|wait| now.saturating_add(wait)),
            state: State::Pending,
            created_ms: now,
        };
        registry.post_message(&message)?;
        (message, reach)
    };
    wake(host);
    Ok(Sent { message, reach })
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
    registry.read_messages(lineage, session, &query, super::now())
}

/// The one lineage `handle` names among live agent sessions: by session ID,
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
    match found.len() {
        1 => Ok(found.pop_first().expect("one lineage")),
        0 => Err(format!(
            "No live agent session matches {handle}; list them with `cagents sessions --all`"
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
        let mut pruned_at: Option<Instant> = None;
        loop {
            if pruned_at.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
                pruned_at = Some(Instant::now());
                if let Err(error) = self.prune() {
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
            let delay = match self.deliver(&host) {
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

    /// Gives each lineage with a live, idle conversation its oldest waiting
    /// message. Returns when the next held reply is released.
    fn deliver(&self, host: &SharedHost) -> Result<Option<u64>, String> {
        let (pending, released) = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .deliverable_messages(super::now())?;
        if pending.is_empty() {
            return Ok(released);
        }
        // An upgrade checkpoint must see every prompt its readiness check
        // admitted; hold the shared side of its barrier while delivering.
        let gate = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        let _control = gate.read().unwrap_or_else(|p| p.into_inner());
        if crate::host::client::ensure_control_open(host).is_err() {
            return Ok(released);
        }
        let chats: Vec<(TerminalId, chat::runtime::Handle)> = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state
                .chats
                .iter()
                .filter(|(_, record)| !record.handle.is_completed())
                .map(|(id, record)| (*id, record.handle.clone()))
                .collect()
        };
        let lineages = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_lineages(chats.iter().map(|(id, _)| *id))?;
        for message in pending {
            let Some(recipient) = message.to.lineage else {
                continue;
            };
            let Some((session, handle)) = chats
                .iter()
                .find(|(id, handle)| lineages.get(id) == Some(&recipient) && handle.is_idle())
            else {
                continue;
            };
            self.hand_over(&message, *session, handle)?;
        }
        Ok(released)
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
        let prompt = Command::Prompt {
            text: self.0.seal.seal(message, super::now()),
            attachments: Vec::new(),
            mode: PromptMode::Send,
        };
        match handle.try_command(prompt) {
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
