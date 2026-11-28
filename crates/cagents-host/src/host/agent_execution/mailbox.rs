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
//! What the person wrote during a turn goes before any message: the courier
//! starts no turn while the provider holds a message they queued, and waits
//! a while for a window holding follow-ups for a provider with no queue to
//! send them. Every message waiting for a conversation then arrives in one
//! prompt, so several cost the conversation one turn rather than one each.
//!
//! A conversation a usage limit stops is idle yet takes nothing, so the
//! courier also judges which ones a limit stops. It holds their messages, and
//! tells every surface showing such a conversation that it is limited and
//! until when, so the daemon gives one account of a limit everywhere.

mod account_watch;
mod wait;

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
    host::model::MessageHandoff,
    model::{MessageId, TerminalId},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[cfg(test)]
use std::sync::Condvar;

/// When the courier looks again after a pass, a queued run or a handoff
/// failed, or while it cannot watch the account files for a usage limit to
/// lift. Everything else that lets a message advance wakes it: a
/// conversation or terminal starting, settling or leaving, a window releasing
/// the person's follow-ups, an account file changing, or a deadline.
const RETRY: Duration = Duration::from_secs(15);
/// The longest the courier waits, once a conversation is idle, for a window
/// to send the follow-ups the person wrote during its turn, which go before
/// any message from another session. A window sends them the moment it sees
/// the turn end; this bounds the wait on one that crashed or cannot.
const PERSON_FIRST: Duration = Duration::from_secs(20);
/// How often the courier forgets messages past their retention.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);
/// The shortest handle a session can be named by, so a typo does not reach
/// a stranger that happens to share one character.
pub(super) const MIN_HANDLE: usize = 4;

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
    let _control = gate.shared();
    crate::host::client::ensure_control_open(host).ok()?;
    if defers_for_update(host) {
        return None;
    }
    Some(work())
}

/// Check under control admission before claiming or starting provider work.
pub(super) fn defers_for_update(host: &SharedHost) -> bool {
    let courier = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox
        .clone();
    courier.is_some_and(|courier| courier.defers_for_update())
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
        // The RPC already holds control admission. Leave the posted message
        // pending just as the courier would, including when it could reopen
        // an agent whose provider exited.
        if defers_for_update(host) {
            return Err("message delivery is paused while cagents updates".into());
        }
        let registry = registry(host)?;
        let seal = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .mailbox_seal
            .clone()
            .ok_or("The mailbox key is unavailable")?;
        let message = &delivery.sent.message;
        let (handoff, _) = to_agent(host, &registry, &seal, message, limited)?;
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
        Ok::<_, String>(Some(started(host, &registry, &accepted, message.id)))
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
        prompt: seal.seal_run(message),
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
///
/// The flag says the handoff failed rather than being held for a reason that
/// reports its own end, so the courier must try again.
fn to_agent(
    host: &SharedHost,
    registry: &SharedRegistry,
    seal: &Seal,
    message: &Message,
    limited: bool,
) -> Result<(Option<Handoff>, bool), String> {
    let Some(lineage) = message.to.lineage else {
        return Ok((None, false));
    };
    let (agent, request, previous, eligible, unverified, ahead) = {
        let store = registry.lock().unwrap_or_else(|p| p.into_inner());
        let Some(agent) = store.lineage_agent(lineage)? else {
            return Ok((None, false));
        };
        let request = message_run(agent.id, message, seal);
        let previous = store.previous_agent_acceptance(&agent.origin, &request)?;
        // The same judgement that orders a follow-up behind this message.
        let eligible = store.takes_messages_as_runs(&agent)?;
        let unverified = store.agent_has_unverified_run(agent.id)?;
        let ahead = store
            .unfinished_agent_runs(agent.id)?
            .first()
            .map(|run| run.id);
        (agent, request, previous, eligible, unverified, ahead)
    };
    let Some(crate::agent_execution::HostedSession::Chat(session)) = &agent.session else {
        return Ok((None, false));
    };
    if !eligible || previous.is_some() {
        return Ok((None, false));
    }
    // What the person wrote for its conversation goes first, and the message
    // after it, together with any other waiting, as a person's conversation
    // takes messages. Writing does not take the agent over; sending does.
    if host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .held_followups
        .contains_key(&session.id)
    {
        return Ok((
            Some(Handoff::Held {
                agent: agent.id,
                reason:
                    "A person wrote its conversation a prompt; it receives the message after that"
                        .into(),
            }),
            false,
        ));
    }
    if unverified {
        return Ok((Some(Handoff::Held {
            agent: agent.id,
            reason: "A run of it was interrupted without proof of what it did, so cagents starts no more runs for it".into(),
        }), false));
    }
    if limited {
        return Ok((Some(Handoff::Held {
            agent: agent.id,
            reason: "A usage limit stops its conversation; it receives the message once the limit resets".into(),
        }), false));
    }
    // A busy agent takes it too, as a run that waits behind its current work:
    // the order of its runs is then the order they were accepted in.
    let handoff = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .hand_message_to_agent(
            message.id,
            session.id,
            &agent.origin,
            &request,
            super::now(),
        );
    let accepted = match handoff {
        Ok(MessageHandoff::Accepted(accepted)) => *accepted,
        Ok(MessageHandoff::Settled) => return Ok((None, false)),
        Ok(MessageHandoff::Behind) => {
            return Ok((Some(Handoff::Behind { agent: agent.id }), false));
        }
        Ok(MessageHandoff::NoSlot) => {
            return Ok((Some(Handoff::NoSlot { agent: agent.id }), false));
        }
        Ok(MessageHandoff::QueueFull) => {
            return Ok((
                Some(Handoff::Waiting {
                    agent: agent.id,
                    behind: ahead,
                }),
                false,
            ));
        }
        Err(reason) => {
            return Ok((
                Some(Handoff::Held {
                    agent: agent.id,
                    reason,
                }),
                true,
            ));
        }
    };
    super::announce(host, registry);
    let handoff = started(host, registry, &accepted, message.id);
    let failed = matches!(handoff, Handoff::Held { .. });
    Ok((Some(handoff), failed))
}

/// Has every message to `agent` that waits to become one of its runs take
/// its place, before a follow-up accepted after them. An error says why one
/// cannot yet.
///
/// `limited` is whether a usage limit stops the agent's own conversation,
/// as the caller judged it for the follow-up.
pub(super) fn hand_earlier(
    host: &SharedHost,
    registry: &SharedRegistry,
    agent: crate::model::AgentId,
    limited: bool,
) -> Result<(), String> {
    let seal = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox_seal
        .clone()
        .ok_or("The mailbox key is unavailable")?;
    // Each pass settles the oldest one, or stops; the bound only guards
    // against a message that settles without leaving the line.
    for _ in 0..=crate::agent_execution::mailbox::MAX_PENDING {
        let Some(message) = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .next_message_in_line(agent, super::now())?
        else {
            return Ok(());
        };
        let why = match to_agent(host, registry, &seal, &message, limited)?.0 {
            Some(Handoff::Run { .. } | Handoff::Ended { .. } | Handoff::Unknown { .. }) => continue,
            // Settled meanwhile, or not the agent's to take as a run, which
            // the follow-up's own acceptance then decides.
            None => return Ok(()),
            Some(Handoff::NoSlot { .. }) => "every agent slot is taken".to_owned(),
            Some(Handoff::Waiting { .. }) => "the agent's queue of waiting prompts is full".into(),
            Some(Handoff::Behind { .. }) => "an earlier one waits".into(),
            // Work of unknown outcome holds back the follow-up too; say so
            // as its own refusal would.
            Some(Handoff::Held { .. })
                if registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .agent_has_unverified_run(agent)? =>
            {
                return Err(crate::host::model::UNVERIFIED_REFUSAL.into());
            }
            Some(Handoff::Held { reason, .. }) => reason,
        };
        return Err(format!(
            "{}{EARLIER}: {why}. Send this once it has",
            crate::agent_execution::NOT_SENT
        ));
    }
    Ok(())
}

/// Why `hand_earlier` refuses a follow-up.
const EARLIER: &str = "a message sent to this agent earlier has not reached it yet";

/// How the agent takes a message as `accepted`, once cagents tried to start
/// it.
fn started(
    host: &SharedHost,
    registry: &SharedRegistry,
    accepted: &Accepted,
    message: MessageId,
) -> Handoff {
    use crate::agent_execution::State as Run;
    let (agent, run) = (accepted.agent.id, accepted.run.id);
    let reason = match super::start(host, accepted) {
        Ok(start) => return Handoff::Run { agent, run, start },
        Err(reason) => reason,
    };
    let ended = {
        let store = registry.lock().unwrap_or_else(|p| p.into_inner());
        store.agent_run(run).and_then(|run| {
            let returned = store
                .message(message)?
                .is_some_and(|message| message.state == State::Pending);
            Ok((run.state, run.provider_key.is_some(), returned))
        })
    };
    match ended {
        // Only a run proved never to have reached the provider gives its
        // message back, which then reaches the conversation as a message.
        Ok((_, false, true)) => Handoff::Held { agent, reason },
        // The provider acknowledged the turn, so the agent took the message
        // as this run, which has since ended without finishing.
        Ok((Run::Failed | Run::Cancelled, true, _)) => Handoff::Ended { agent, run, reason },
        // Interrupted, or ended with its prompt perhaps written but not
        // acknowledged: the provider may have the message, so it stays given.
        Ok(_) => Handoff::Unknown { agent, run, reason },
        Err(_) => Handoff::Held { agent, reason },
    }
}

fn leave(
    host: &SharedHost,
    authority: &Authority,
    outgoing: Outgoing,
) -> Result<Delivery, AcceptanceError> {
    outgoing.validate()?;
    let (origin, session) = caller(host, authority)?;
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
            lineage: session.map(lineage_of).or(linked(&origin, session)),
            session,
            title: session.and_then(title_of),
        };
        let (to, reply_to) = match &outgoing.to {
            To::Session(handle) => (
                Some(
                    resolve(&model.agents, &live, &lineage_of, handle).map_err(|error| {
                        stopped(&listing, &registry, &origin, &model.agents, handle)
                            .unwrap_or(error)
                    })?,
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
/// exited: such a session cannot take a message until it is resumed. It
/// names the session `cagents resume` would reopen for the same handle, and
/// offers the command only to a sender `origin` that may resume it.
fn stopped(
    listing: &directory::Listing,
    registry: &crate::host::model::Registry,
    origin: &Origin,
    agents: &[crate::agent_execution::Agent],
    handle: &str,
) -> Option<String> {
    let mine = |id| super::resume::may_resume(origin, super::resume::open_agent(agents, id));
    let super::resume::Resolved::Stopped(id, _) = super::resume::resolve(
        listing.members(),
        agents,
        handle,
        |lineage| registry.lineage_succession(lineage),
        mine,
    )
    .ok()?
    else {
        return None;
    };
    let member = listing.members().find(|member| member.session.id == id)?;
    let mut reason = format!(
        "Session {} has stopped: its provider exited, so it cannot receive messages",
        member.session.id
    );
    if let Some(failure) = &member.failure {
        reason.push_str(&format!(" ({failure})"));
    }
    if mine(id) {
        reason.push_str(&format!(
            ". Resume its conversation first with `cagents resume {id}`, or message another session from `cagents sessions --all`"
        ));
    } else {
        reason.push_str(&format!(
            ". The person can resume its conversation with `cagents resume {id}`; message another session from `cagents sessions --all` meanwhile"
        ));
    }
    Some(reason)
}

pub(in crate::host) fn inbox(
    host: &SharedHost,
    authority: &Authority,
    query: Inbox,
) -> Result<Vec<Message>, AcceptanceError> {
    let (origin, session) = caller(host, authority)?;
    let registry = registry(host)?;
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let lineage = session
        .map(|session| {
            registry
                .session_lineage(session)
                .map(|lineage| lineage.unwrap_or(Lineage(session)))
        })
        .transpose()?
        .or(linked(&origin, session));
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
) -> Result<Vec<Message>, AcceptanceError> {
    let (origin, session) = caller(host, authority)?;
    let registry = registry(host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let lineage = session
        .map(|session| {
            registry
                .session_lineage(session)
                .map(|lineage| lineage.unwrap_or(Lineage(session)))
        })
        .transpose()?
        .or(linked(&origin, session));
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
) -> Result<(), AcceptanceError> {
    receipt.validate()?;
    let (origin, session) = caller(host, authority)?;
    let registry = registry(host)?;
    let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let lineage = session
        .map(|session| {
            registry
                .session_lineage(session)
                .map(|lineage| lineage.unwrap_or(Lineage(session)))
        })
        .transpose()?
        .or(linked(&origin, session));
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

/// The lineage a session from a linked computer reads and sends mail as. It
/// has no session here, only the lineage its connection proved, and without
/// one it would read the person's own inbox.
fn linked(origin: &Origin, session: Option<TerminalId>) -> Option<Lineage> {
    match (origin, session) {
        (Origin::Child { parent }, None) => Some(*parent),
        _ => None,
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
        return Err(match lineage {
            // A linked computer's session, which has no record here.
            Some(_) => "A session from another computer has no parent on this device; message it from that computer".into(),
            None => "Only a session has a parent; this caller is the user's own CLI".into(),
        });
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
    update: UpdateWait,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum UpdateWait {
    #[default]
    Open,
    Held,
    Retry,
}

struct Inner {
    update_runtime: Option<PathBuf>,
    host: Weak<crate::host::state::HostLock>,
    registry: SharedRegistry,
    seal: Arc<Seal>,
    state: Mutex<WakeState>,
    wait: wait::Wait,
    /// What each conversation's subscribers were last told of its limit.
    limits: Mutex<BTreeMap<TerminalId, Told>>,
    /// When the courier first found each conversation idle while a window
    /// held the person's follow-ups for it, in milliseconds since the epoch.
    yielding: Mutex<BTreeMap<TerminalId, u64>>,
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
        self.0.wait.wake();
    }

    /// Keep unclaimed inputs durable during an update. Follow the
    /// fence without holding daemon control or joining its waiter at shutdown.
    fn defers_for_update(&self) -> bool {
        let Some(runtime) = &self.0.update_runtime else {
            return false;
        };
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        // Only a verified lease can defer work until the courier wakes. A
        // failed probe leaves a retry deadline for delivery, but a new request
        // can check the fence again even if no delivery is pending.
        if state.update == UpdateWait::Held {
            return true;
        }
        let release = match crate::instance::UpdateFenceRelease::observe(runtime) {
            // Only the courier consumes Retry. It may already have skipped
            // a delivery in this pass, even though this request can proceed.
            Ok(None) => return false,
            Ok(Some(release)) => release,
            Err(error) => {
                tracing::warn!(%error, "cannot check the update fence before message delivery");
                state.update = UpdateWait::Retry;
                return true;
            }
        };
        let owner = Arc::downgrade(&self.0);
        match std::thread::Builder::new()
            .name("courier-update-fence".into())
            .spawn(move || {
                let released = release.wait();
                if let Some(owner) = owner.upgrade() {
                    let mut state = owner.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.update = match released {
                        Ok(()) => UpdateWait::Open,
                        Err(error) => {
                            tracing::warn!(%error, "could not follow the update fence release");
                            UpdateWait::Retry
                        }
                    };
                    state.generation = state.generation.wrapping_add(1);
                    owner.wait.wake();
                }
            }) {
            Ok(_) => state.update = UpdateWait::Held,
            Err(error) => {
                tracing::warn!(%error, "could not observe the update fence release");
                state.update = UpdateWait::Retry;
            }
        }
        true
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

    #[cfg(test)]
    pub(in crate::host) fn start(host: &SharedHost) -> Result<Self, String> {
        Self::start_in(host, None)
    }

    pub(in crate::host) fn start_with_update_fence(host: &SharedHost) -> Result<Self, String> {
        let runtime = crate::instance::runtime_directory().map_err(|error| error.to_string())?;
        Self::start_in(host, Some(runtime))
    }

    fn start_in(host: &SharedHost, update_runtime: Option<PathBuf>) -> Result<Self, String> {
        let registry = registry(host)?;
        let seal = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .mailbox_seal
            .clone()
            .ok_or("The mailbox key is unavailable")?;
        let handle = Handle(Arc::new(Inner {
            update_runtime,
            host: Arc::downgrade(host),
            registry,
            seal,
            state: Mutex::new(WakeState::default()),
            wait: wait::Wait::new(),
            limits: Mutex::new(BTreeMap::new()),
            yielding: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            scanned_generation: Mutex::new(None),
            #[cfg(test)]
            scanned: Condvar::new(),
        }));
        let owner = handle.clone();
        // Publish the handle before the first pass reads HostState, so no
        // edge raised in between goes to a courier that is not there yet,
        // and the first pass sees its update fence before it claims work.
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let thread = std::thread::Builder::new()
            .name("message-courier".into())
            .spawn(move || owner.run())
            .map_err(|error| error.to_string())?;
        state.mailbox = Some(handle.clone());
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
        self.handle.0.wait.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// When, in milliseconds since the epoch, the courier looks again after a
/// pass: at the deadline the pass reported, after [`RETRY`] if it failed, and
/// at most at the next prune; never so soon that it spins.
fn courier_deadline(pass: &Result<Option<u64>, String>, now_ms: u64, prune_in: Duration) -> u64 {
    let delivery = match pass {
        Ok(due) => *due,
        Err(_) => Some(now_ms.saturating_add(RETRY.as_millis() as u64)),
    };
    delivery
        .unwrap_or(u64::MAX)
        .min(now_ms.saturating_add(prune_in.as_millis() as u64))
        .max(now_ms.saturating_add(50))
}

impl Handle {
    fn run(&self) {
        let mut accounts = account_watch::AccountWatch::new();
        self.run_with(
            || self.prune(),
            |host| self.deliver(host, accounts.refresh(self)),
        );
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
            #[cfg_attr(not(test), allow(unused_variables))]
            let observed = {
                let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.stopped {
                    return;
                }
                // An edge from here on ends the wait after this pass.
                self.0.wait.begin_pass();
                state.generation
            };
            let Some(host) = self.0.host.upgrade() else {
                return;
            };
            let pass = crate::host::service::attempt("message delivery", || deliver(&host));
            if let Err(error) = &pass {
                tracing::warn!(%error, "cannot deliver messages");
            }
            let prune_in = pruned_at.map_or(PRUNE_EVERY, |at| {
                (at + PRUNE_EVERY).saturating_duration_since(Instant::now())
            });
            let deadline = courier_deadline(&pass, super::now(), prune_in);
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
            if let Err(error) = self.0.wait.until(deadline) {
                // Rescan on a short interval rather than leave mail stranded.
                // Edges, including the one that stops the courier, still end
                // the wait at once.
                tracing::error!(%error, "cannot wait for a delivery deadline");
                let remaining = Duration::from_millis(deadline.saturating_sub(super::now()));
                if let Err(error) = self.0.wait.edge_within(remaining.min(RETRY)) {
                    tracing::error!(%error, "cannot wait for message delivery");
                    std::thread::sleep(RETRY);
                }
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

    /// Gives each lineage with a live, idle conversation the messages waiting
    /// for it. Returns when the courier must look again: when the next held
    /// reply is released, a known usage limit resets or a conversation stops
    /// waiting for the person's follow-ups, and after [`RETRY`] when a step
    /// failed or, unless `watching_accounts`, a conversation is limited;
    /// `None` when only an edge can advance delivery. A message pending for
    /// a recipient that cannot take it now waits for such an edge.
    fn deliver(&self, host: &SharedHost, watching_accounts: bool) -> Result<Option<u64>, String> {
        let limits = self.publish_limits(host, super::now());
        let reset = limits
            .values()
            .filter_map(|limit| limit.resets_at)
            .filter(|at| *at >= 0)
            .map(|at| (at as u64).saturating_mul(1000))
            .min();
        self.forget_yields(host);
        let (queued_failed, deferred) = self.start_queued(host);
        let (released, delivery_failed) = self.deliver_messages(host, &limits)?;
        let update = {
            let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
            let update = state.update;
            if update == UpdateWait::Retry {
                state.update = UpdateWait::Open;
            }
            update
        };
        // A held update fence wakes the courier when it releases.
        let retry = (queued_failed
            || (delivery_failed && update != UpdateWait::Held)
            || update == UpdateWait::Retry
            || !watching_accounts && !limits.is_empty())
        .then(|| super::now().saturating_add(RETRY.as_millis() as u64));
        Ok([released, reset, retry, deferred]
            .into_iter()
            .flatten()
            .min())
    }

    /// Starts the prompts agents hold behind work that has since ended,
    /// except a message that would go before what the person wrote for its
    /// conversation. Returns whether one failed to start, or the queue could
    /// not be read, and when to look again for one that waits.
    fn start_queued(&self, host: &SharedHost) -> (bool, Option<u64>) {
        let now = super::now();
        let mut due: Option<u64> = None;
        let failed = super::start_queued(host, |accepted| {
            match self.waits_for_person(host, accepted, now) {
                Some(until) => {
                    if let Some(until) = until {
                        due = Some(due.map_or(until, |due| due.min(until)));
                    }
                    true
                }
                None => false,
            }
        })
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot start queued agent prompts");
            true
        });
        (failed, due)
    }

    /// Whether `accepted`, a message an agent takes as a run, waits for what
    /// the person wrote for the agent's idle conversation, and until when if
    /// it is a window that holds it. `None` when it need not wait.
    fn waits_for_person(
        &self,
        host: &SharedHost,
        accepted: &crate::agent_execution::Accepted,
        now: u64,
    ) -> Option<Option<u64>> {
        let Some(crate::agent_execution::HostedSession::Chat(session)) = &accepted.agent.session
        else {
            return None;
        };
        let (handle, held) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let record = state.chats.get(&session.id)?;
            (
                record.handle.clone(),
                state.held_followups.contains_key(&session.id),
            )
        };
        // A conversation at work, or one that has ended, is the run's own
        // to wait for or reopen.
        if !handle.is_idle() {
            return None;
        }
        let message = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .run_is_message(accepted.run.id)
            .unwrap_or(false);
        if !message {
            return None;
        }
        if !handle.takes_courier() {
            // The provider holds a message the person queued; its turn ends
            // in a wake.
            return Some(None);
        }
        held.then(|| self.yields(session.id, now))
            .flatten()
            .map(Some)
    }

    /// Forgets when each conversation began to wait for the person's
    /// follow-ups once it no longer does: idle, with a window holding them.
    /// The next time it does, it waits as long again.
    fn forget_yields(&self, host: &SharedHost) {
        let held: Vec<(TerminalId, chat::runtime::Handle)> = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state
                .held_followups
                .keys()
                .filter_map(|id| Some((*id, state.chats.get(id)?.handle.clone())))
                .collect()
        };
        let waiting: BTreeSet<TerminalId> = held
            .into_iter()
            .filter(|(_, handle)| handle.takes_courier())
            .map(|(id, _)| id)
            .collect();
        self.0
            .yielding
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|id, _| waiting.contains(id));
    }

    /// Returns when the courier must look again for a held reply's release
    /// or a person's follow-ups, and whether handing a message over failed.
    fn deliver_messages(
        &self,
        host: &SharedHost,
        limits: &BTreeMap<TerminalId, UsageLimit>,
    ) -> Result<(Option<u64>, bool), String> {
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
            return Ok((released, false));
        }
        let chats: Vec<Conversation> = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            state
                .chats
                .iter()
                .filter(|(_, record)| !record.handle.is_completed())
                .map(|(id, record)| Conversation {
                    id: *id,
                    handle: record.handle.clone(),
                    held: state.held_followups.contains_key(id),
                })
                .collect()
        };
        let lineages = self
            .0
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_lineages(chats.iter().map(|chat| chat.id))?;
        let mut due = released;
        let mut failed = false;
        let mut prompted = BTreeSet::new();
        for message in pending {
            let Some(recipient) = message.to.lineage else {
                continue;
            };
            // The prompt its conversation was given took every message then
            // deliverable to it.
            if prompted.contains(&recipient) {
                continue;
            }
            let limited = chats.iter().any(|chat| {
                lineages.get(&chat.id) == Some(&recipient) && limits.contains_key(&chat.id)
            });
            // One recipient at a time, as reopening an agent's conversation
            // for one can take a while and must not hold off an upgrade for
            // all.
            let Some(delivered) = with_control(host, || {
                self.deliver_message(
                    host, &message, recipient, limited, &chats, &lineages, limits,
                )
            }) else {
                // The daemon takes no work while an upgrade or shutdown holds
                // control. Look again in case it reopens.
                return Ok((due, true));
            };
            match delivered? {
                Delivered::Prompted => {
                    prompted.insert(recipient);
                }
                Delivered::Yields(until) => due = Some(due.map_or(until, |due| due.min(until))),
                Delivered::Elsewhere => {}
                Delivered::Failed => failed = true,
            }
        }
        Ok((due, failed))
    }

    /// Gives one pending message to its recipient: as a run of the agent
    /// cagents hosts for it, or into its idle conversation, together with
    /// every other message waiting for it there.
    #[allow(clippy::too_many_arguments)]
    fn deliver_message(
        &self,
        host: &SharedHost,
        message: &Message,
        recipient: crate::agent_execution::Lineage,
        limited: bool,
        chats: &[Conversation],
        lineages: &BTreeMap<TerminalId, crate::agent_execution::Lineage>,
        limits: &BTreeMap<TerminalId, UsageLimit>,
    ) -> Result<Delivered, String> {
        let failed = match to_agent(host, &self.0.registry, &self.0.seal, message, limited) {
            // Its run has it, or it waits its turn to become one.
            Ok((
                Some(
                    Handoff::Run { .. }
                    | Handoff::Waiting { .. }
                    | Handoff::Behind { .. }
                    | Handoff::NoSlot { .. },
                ),
                _,
            )) => return Ok(Delivered::Elsewhere),
            Ok((Some(Handoff::Ended { reason, run, .. }), _)) => {
                tracing::info!(%reason, %run, message = %message.id, "a message's run ended");
                return Ok(Delivered::Elsewhere);
            }
            // Its run has it, perhaps already with the provider.
            Ok((Some(Handoff::Unknown { reason, run, .. }), _)) => {
                tracing::warn!(%reason, %run, message = %message.id, "a message's run ended without proof of delivery");
                return Ok(Delivered::Elsewhere);
            }
            // Not as a run, but as a message, as a person's conversation
            // takes one.
            Ok((Some(Handoff::Held { reason, .. }), failed)) => {
                tracing::debug!(%reason, message = %message.id, "an agent takes a message as a message");
                failed
            }
            Ok((None, _)) => false,
            Err(error) => {
                tracing::warn!(%error, message = %message.id, "cannot hand a message to its agent");
                return Ok(Delivered::Failed);
            }
        };
        // An idle conversation whose provider holds nothing the person
        // queued: what they queued goes first, and its turn ends idle again.
        let Some(chat) = chats.iter().find(|chat| {
            lineages.get(&chat.id) == Some(&recipient)
                && chat.handle.takes_courier()
                && !limits.contains_key(&chat.id)
        }) else {
            return Ok(if failed {
                Delivered::Failed
            } else {
                Delivered::Elsewhere
            });
        };
        let now = super::now();
        if chat.held
            && let Some(until) = self.yields(chat.id, now)
        {
            tracing::debug!(message = %message.id, session = %chat.id, "messages wait for the person's follow-ups");
            return Ok(Delivered::Yields(until));
        }
        let messages = self.together(message, recipient, now)?;
        Ok(if self.hand_over(&messages, chat.id, &chat.handle)? {
            Delivered::Prompted
        } else {
            Delivered::Failed
        })
    }

    /// Until when, in milliseconds since the epoch, `session`, idle while a
    /// window holds the person's follow-ups for it, still waits for that
    /// window to send them; `None` once it has waited [`PERSON_FIRST`], so a
    /// window that cannot send them holds no message back for longer.
    fn yields(&self, session: TerminalId, now: u64) -> Option<u64> {
        let mut yielding = self.0.yielding.lock().unwrap_or_else(|p| p.into_inner());
        let until = yielding
            .entry(session)
            .or_insert(now)
            .saturating_add(PERSON_FIRST.as_millis() as u64);
        (now < until).then_some(until)
    }

    /// The messages the prompt that gives `message` to its conversation
    /// carries: every one deliverable to its recipient, oldest first, up to
    /// the size of one prompt. An agent cagents hosts takes messages as runs,
    /// one each, so one that takes this one as a message takes it alone.
    fn together(
        &self,
        message: &Message,
        recipient: crate::agent_execution::Lineage,
        now: u64,
    ) -> Result<Vec<Message>, String> {
        let registry = self.0.registry.lock().unwrap_or_else(|p| p.into_inner());
        let takes_runs = match registry.lineage_agent(recipient)? {
            Some(agent) => registry.takes_messages_as_runs(&agent)?,
            None => false,
        };
        if takes_runs {
            return Ok(vec![message.clone()]);
        }
        let mut waiting = registry.deliverable_to(recipient, now)?;
        drop(registry);
        if !waiting.iter().any(|waiting| waiting.id == message.id) {
            waiting.insert(0, message.clone());
        }
        let mut bytes = 0;
        let mut together = Vec::with_capacity(waiting.len());
        for waiting in waiting {
            bytes += self.0.seal.seal(&waiting, now).len() + 2;
            if !together.is_empty() && bytes > crate::agent_execution::MAX_PROMPT_BYTES {
                break;
            }
            together.push(waiting);
        }
        Ok(together)
    }

    /// Claims `messages` for `session` before prompting, so that an inbox
    /// read racing the prompt cannot return them too, and releases the claim
    /// if the conversation refuses the prompt. A prompt the conversation took
    /// but did not acknowledge in time may still run, so its messages stay
    /// delivered: a message reaches a conversation at most once. Each message
    /// settles on its own, as its recipient reading it would settle it.
    ///
    /// Returns false when the courier must try again on its own: the
    /// conversation refused the prompt yet is still idle, so no edge will
    /// report it free.
    fn hand_over(
        &self,
        messages: &[Message],
        session: TerminalId,
        handle: &chat::runtime::Handle,
    ) -> Result<bool, String> {
        let registry = &self.0.registry;
        let ids: Vec<MessageId> = messages.iter().map(|message| message.id).collect();
        let claimed = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .deliver_messages(&ids, session, super::now())?;
        if claimed.is_empty() {
            return Ok(true);
        }
        let given = messages
            .iter()
            .filter(|message| claimed.contains(&message.id));
        if let Some(host) = self.0.host.upgrade() {
            super::given_work(&host, session);
        }
        match handle.try_idle_prompt(self.0.seal.seal_all(given, super::now())) {
            Ok(()) => {}
            Err(Rejection::Refused(error)) => {
                // The conversation took a turn of its own since it was seen
                // idle, most often for a person's prompt; the messages wait
                // for that turn to finish.
                tracing::debug!(%error, messages = claimed.len(), %session, "conversation refused messages");
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .undeliver_messages(&claimed, session)?;
                // A turn the person started, or what they queued with the
                // provider, ends in an edge. A refusal that leaves the
                // conversation free reports nothing.
                return Ok(handle.is_completed() || !handle.takes_courier());
            }
            Err(Rejection::Unacknowledged(error)) => {
                tracing::warn!(%error, messages = claimed.len(), %session, "conversation did not acknowledge messages");
            }
        }
        Ok(true)
    }
}

/// A live conversation as the courier sees it in a pass.
struct Conversation {
    id: TerminalId,
    handle: chat::runtime::Handle,
    /// Whether a window holds follow-ups the person wrote for its next turn.
    held: bool,
}

/// What became of a message the courier tried to deliver.
enum Delivered {
    /// Its conversation was given it in a prompt, with every other message
    /// then waiting for it.
    Prompted,
    /// Its conversation waits until then for a window to send the person's
    /// follow-ups first.
    Yields(u64),
    /// An agent's run took it or will, or no conversation can take it now;
    /// a change that lets one take it wakes the courier.
    Elsewhere,
    /// Handing it over failed in a way nothing reports, so the courier tries
    /// again after [`RETRY`].
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn courier_defers_new_dispatch_and_wakes_when_the_update_releases_its_fence() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("run");
        let filename = if crate::instance::IS_DEVELOPMENT {
            "cagents-development-ui.lock.update"
        } else {
            "cagents-ui.lock.update"
        };
        let host = crate::host::service::test_host();
        {
            let mut state = host.lock().unwrap();
            state.execution_registry = Some(Arc::new(Mutex::new(
                crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            )));
            state.mailbox_seal = Some(Arc::new(Seal::new([7; 32])));
        }
        let courier = Courier::start_in(&host, Some(runtime.clone())).unwrap();
        courier.wait_for_scan(0);
        assert!(defers_for_update(&host), "a failed probe must defer work");
        std::fs::create_dir(&runtime).unwrap();
        assert!(
            !defers_for_update(&host),
            "a new request rechecks the fence"
        );

        let fence = crate::instance::owned_file_lock(
            std::fs::File::create(runtime.join(filename)).unwrap(),
            libc::LOCK_EX,
        )
        .unwrap();
        for _ in 0..2 {
            assert!(with_control(&host, || panic!("started during the update")).is_none());
        }
        let generation = {
            let state = courier.handle.0.state.lock().unwrap();
            assert!(state.update == UpdateWait::Held);
            state.generation
        };
        drop(fence);
        courier.wait_for_scan(generation + 1);
        assert!(courier.handle.0.state.lock().unwrap().update == UpdateWait::Open);
        assert_eq!(with_control(&host, || "delivered"), Some("delivered"));
    }

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
            update_runtime: None,
            host: Weak::new(),
            registry: registry.clone(),
            seal: Arc::new(Seal::new([7; 32])),
            state: Mutex::default(),
            wait: wait::Wait::new(),
            limits: Mutex::default(),
            yielding: Mutex::default(),
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
        struct Delivery(Option<std::thread::JoinHandle<Result<bool, String>>>);
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
            delivery_courier.hand_over(
                std::slice::from_ref(&delivery_message),
                session,
                &delivery_handle,
            )
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

        courier
            .hand_over(std::slice::from_ref(&message), session, &actor.handle)
            .unwrap();
        assert_eq!(stored().state, State::Pending);
        assert_eq!(
            actor.prompt_count(),
            0,
            "an unchanged request stays protected"
        );
        actor.clear_request();
        assert!(actor.handle.is_idle());
        courier
            .hand_over(std::slice::from_ref(&message), session, &actor.handle)
            .unwrap();
        let delivered = stored();
        assert_eq!(delivered.state, State::Delivered);
        assert_eq!(delivered.to.session, Some(session));
        assert_eq!(actor.prompt_count(), 1);
        courier
            .hand_over(std::slice::from_ref(&message), session, &actor.handle)
            .unwrap();
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
            update_runtime: None,
            host: Weak::new(),
            registry: registry.clone(),
            seal: Arc::new(Seal::new([8; 32])),
            state: Mutex::default(),
            wait: wait::Wait::new(),
            limits: Mutex::default(),
            yielding: Mutex::default(),
            scanned_generation: Mutex::new(None),
            scanned: Condvar::new(),
        }));
        let mut actor = RequestRaceFixture::with_input_failure(true);
        courier
            .hand_over(std::slice::from_ref(&message), session, &actor.handle)
            .unwrap();
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
            .hand_over(std::slice::from_ref(&message), session, &successor.handle)
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
            let awaits_receipt = matches!(
                &outcome,
                RetainedInputOutcome::Unavailable | RetainedInputOutcome::Disconnected
            );
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
                update_runtime: None,
                host: Weak::new(),
                registry: registry.clone(),
                seal: Arc::new(Seal::new([9; 32])),
                state: Mutex::default(),
                wait: wait::Wait::new(),
                limits: Mutex::default(),
                yielding: Mutex::default(),
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
                delivery_courier.hand_over(
                    std::slice::from_ref(&delivery_message),
                    session,
                    &delivery_actor,
                )
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
                    retry_courier.hand_over(
                        std::slice::from_ref(&retry_message),
                        session,
                        &retry_actor,
                    )
                });
                assert_eq!(answer(RetainedInputOutcome::Acknowledged(Ok(()))), 1);
                retry.join().unwrap().unwrap();
                assert_eq!(stored().state, State::Delivered);
            } else {
                assert_eq!(settled.state, State::Delivered);
                assert_eq!(settled.to.session, Some(session));
                let retained = actor.pending_retained_inputs();
                // Unknown is a received, terminal receipt. Connection
                // loss retains the envelope until replay yields a receipt.
                assert_eq!(retained.len(), usize::from(awaits_receipt));
                if let Some(envelope) = retained.first() {
                    assert_eq!(envelope.sequence, 1);
                }
                // The real courier path must observe Delivered and return
                // before allocating any replacement input envelope.
                courier
                    .hand_over(std::slice::from_ref(&message), session, &actor)
                    .unwrap();
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
            update_runtime: None,
            host: Arc::downgrade(&host),
            registry: Arc::new(Mutex::new(
                crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            )),
            seal: Arc::new(Seal::new([7; 32])),
            state: Mutex::default(),
            wait: wait::Wait::new(),
            limits: Mutex::default(),
            yielding: Mutex::default(),
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
                    owner.0.wait.wake();
                    reported.send(deliveries).unwrap();
                    Ok(None)
                },
            );
        });
        assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        thread.join().unwrap();
    }

    #[test]
    fn courier_uses_real_deadlines_and_retries_only_failures() {
        let prune_in = Duration::from_secs(1800);
        let now = 1_000_000;
        assert_eq!(courier_deadline(&Ok(None), now, prune_in), now + 1_800_000);
        assert_eq!(
            courier_deadline(&Err("no".into()), now, prune_in),
            now + RETRY.as_millis() as u64
        );
        assert_eq!(
            courier_deadline(&Ok(Some(now - 5)), now, prune_in),
            now + 50,
            "a past deadline must not spin"
        );
        assert_eq!(
            courier_deadline(&Ok(Some(now + 3_000)), now, prune_in),
            now + 3_000
        );
        assert_eq!(
            courier_deadline(&Ok(Some(now + 300_000)), now, prune_in),
            now + 300_000,
            "a far deadline needs no recheck"
        );
        assert_eq!(
            courier_deadline(&Ok(Some(now + 3_600_000)), now, prune_in),
            now + 1_800_000
        );
    }

    #[test]
    fn an_edge_after_a_pass_begins_ends_the_next_wait() {
        // Linux falls back to the condition variable when it cannot create
        // the descriptors, so both must hold the same contract there.
        #[cfg(target_os = "linux")]
        let waits = [wait::Wait::new(), wait::Wait::portable()];
        #[cfg(not(target_os = "linux"))]
        let waits = [wait::Wait::new()];
        for wait in waits {
            wait.wake();
            wait.begin_pass();
            // The edge before the pass began was consumed by it.
            let start = Instant::now();
            wait.until(super::super::now() + 50).unwrap();
            assert!(start.elapsed() >= Duration::from_millis(40));
            wait.begin_pass();
            wait.wake();
            let start = Instant::now();
            wait.until(super::super::now() + 60_000).unwrap();
            assert!(start.elapsed() < Duration::from_secs(5));
            // The fallback wait without a deadline timer ends on an edge too.
            wait.begin_pass();
            let start = Instant::now();
            wait.edge_within(Duration::from_millis(50)).unwrap();
            assert!(start.elapsed() >= Duration::from_millis(40));
            wait.wake();
            let start = Instant::now();
            wait.edge_within(Duration::from_secs(60)).unwrap();
            assert!(start.elapsed() < Duration::from_secs(5));
        }
    }

    #[test]
    fn atomic_account_cache_replacement_wakes_the_courier() {
        let temporary = tempfile::tempdir().unwrap();
        let handle = Handle(Arc::new(Inner {
            host: Weak::new(),
            registry: Arc::new(Mutex::new(
                crate::host::model::Registry::at(&temporary.path().join("model.sqlite3")).unwrap(),
            )),
            seal: Arc::new(Seal::new([7; 32])),
            state: Mutex::default(),
            wait: wait::Wait::new(),
            limits: Mutex::default(),
            yielding: Mutex::default(),
            update_runtime: None,
            scanned_generation: Mutex::new(None),
            scanned: Condvar::new(),
        }));
        let account = temporary.path().join("account-usage.json");
        std::fs::write(&account, "initial fixture").unwrap();
        let mut watched = account_watch::AccountWatch::for_files(vec![account.clone()].into());
        assert!(watched.refresh(&handle));
        let sibling = temporary.path().join("replacement.json");
        std::fs::write(&sibling, "fixture replacement").unwrap();
        // Private reads of the account registry repeat its chmod.
        std::fs::set_permissions(&account, std::fs::metadata(&account).unwrap().permissions())
            .unwrap();
        handle.0.wait.until(super::super::now() + 50).unwrap();
        assert_eq!(handle.0.state.lock().unwrap().generation, 0);
        std::fs::rename(sibling, account).unwrap();
        handle.0.wait.until(super::super::now() + 5_000).unwrap();
        assert!(handle.0.state.lock().unwrap().generation > 0);
    }

    #[test]
    fn pending_mail_for_an_offline_lineage_needs_no_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let mut model =
            crate::host::model::Registry::at(&temporary.path().join("model.sqlite3")).unwrap();
        model
            .post_message(&Message {
                id: MessageId::new(),
                to: Party {
                    lineage: Some(Lineage(TerminalId::new())),
                    session: None,
                    title: None,
                },
                from: Party {
                    lineage: None,
                    session: None,
                    title: None,
                },
                reply_to: None,
                body: "Read when you return".into(),
                wait_until_ms: None,
                state: State::Pending,
                created_ms: super::super::now(),
            })
            .unwrap();
        let host = crate::host::service::test_host();
        host.lock().unwrap().install_model(model);
        let courier = Courier::start(&host).unwrap();
        courier.wait_for_scan(0);
        assert_eq!(courier.handle.deliver(&host, true).unwrap(), None);
    }

    #[test]
    fn empty_courier_waits_for_an_event_instead_of_the_fallback_tick() {
        let directory = tempfile::tempdir().unwrap();
        let host = crate::host::service::test_host();
        let handle = Handle(Arc::new(Inner {
            update_runtime: None,
            host: Arc::downgrade(&host),
            registry: Arc::new(Mutex::new(
                crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            )),
            seal: Arc::new(Seal::new([7; 32])),
            state: Mutex::default(),
            wait: wait::Wait::new(),
            limits: Mutex::default(),
            yielding: Mutex::default(),
            scanned_generation: Mutex::new(None),
            scanned: Condvar::new(),
        }));
        let wake = handle.clone();
        let (reported, received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut deliveries = 0;
            handle.run_with(
                || Ok(()),
                |_| {
                    deliveries += 1;
                    reported.send(deliveries).unwrap();
                    if deliveries == 2 {
                        handle.0.state.lock().unwrap().stopped = true;
                        handle.0.wait.wake();
                    }
                    Ok(None)
                },
            );
        });
        assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        assert!(
            received.recv_timeout(Duration::from_millis(100)).is_err(),
            "an empty courier must not create a periodic wake"
        );
        wake.wake();
        assert_eq!(received.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        thread.join().unwrap();
    }

    /// A courier over a test host listing one live conversation, on a real
    /// actor, with messages from the person waiting for it.
    struct Delivering {
        courier: Handle,
        host: SharedHost,
        registry: SharedRegistry,
        session: TerminalId,
        actor: crate::chat::runtime::courier_tests::RequestRaceFixture,
        messages: Vec<Message>,
        _directory: tempfile::TempDir,
    }

    impl Delivering {
        /// `bodies` are written oldest first.
        fn new(bodies: &[&str]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let host = crate::host::service::test_host();
            let registry = Arc::new(Mutex::new(
                crate::host::model::Registry::at(&directory.path().join("model.sqlite3")).unwrap(),
            ));
            let actor = crate::chat::runtime::courier_tests::RequestRaceFixture::new();
            let session = TerminalId::new();
            let mut recipe = crate::model::LaunchRecipe::shell(directory.path().into()).unwrap();
            recipe.program = "/bin/cat".into();
            recipe.arguments.clear();
            host.lock().unwrap().chats.insert(
                session,
                crate::host::chat::Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: crate::chat::Session {
                        id: session,
                        proof: None,
                        terminal: None,
                        native_continuity: Default::default(),
                        owned_codex_listener: None,
                        launch: crate::chat::Launch {
                            provider: crate::agent::Provider::Omp,
                            recipe,
                            session_id: None,
                            draft_model: None,
                        },
                    },
                    handle: actor.handle.clone(),
                    source_process: None,
                    subscribers: BTreeMap::from([(1, true)]),
                    observations: Default::default(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
            let created = super::super::now() - 1_000;
            let messages: Vec<Message> = bodies
                .iter()
                .enumerate()
                .map(|(index, body)| Message {
                    id: MessageId::new(),
                    from: Party::user(),
                    to: Party {
                        lineage: Some(Lineage(session)),
                        session: None,
                        title: None,
                    },
                    reply_to: None,
                    body: (*body).into(),
                    wait_until_ms: None,
                    state: State::Pending,
                    created_ms: created + index as u64,
                })
                .collect();
            // Posted newest first: when each was written decides the order.
            for message in messages.iter().rev() {
                registry.lock().unwrap().post_message(message).unwrap();
            }
            let courier = Handle(Arc::new(Inner {
                update_runtime: None,
                host: Arc::downgrade(&host),
                registry: registry.clone(),
                seal: Arc::new(Seal::new([7; 32])),
                state: Mutex::default(),
                wait: wait::Wait::new(),
                limits: Mutex::default(),
                yielding: Mutex::default(),
                scanned_generation: Mutex::new(None),
                scanned: Condvar::new(),
            }));
            Self {
                courier,
                host,
                registry,
                session,
                actor,
                messages,
                _directory: directory,
            }
        }

        /// One courier pass; when it must look again.
        /// One of the courier's passes over the messages, as `deliver` makes.
        fn pass(&self) -> Option<u64> {
            self.courier.forget_yields(&self.host);
            self.courier
                .deliver_messages(&self.host, &BTreeMap::new())
                .unwrap()
                .0
        }

        fn states(&self) -> Vec<(State, Option<TerminalId>)> {
            let registry = self.registry.lock().unwrap();
            self.messages
                .iter()
                .map(|message| {
                    let stored = registry.message(message.id).unwrap().unwrap();
                    (stored.state, stored.to.session)
                })
                .collect()
        }

        fn all(&self, state: State) -> Vec<(State, Option<TerminalId>)> {
            let session = (state == State::Delivered).then_some(self.session);
            vec![(state, session); self.messages.len()]
        }

        /// Has a window declare it holds the person's follow-ups, or not.
        fn hold_followups(&self, held: bool) {
            let mut state = self.host.lock().unwrap();
            if held {
                state
                    .held_followups
                    .entry(self.session)
                    .or_default()
                    .insert(1);
            } else {
                state.held_followups.remove(&self.session);
            }
        }

        /// The bodies each prompt the conversation took carried, in the
        /// order it read them.
        fn prompted(&self) -> Vec<Vec<String>> {
            self.actor
                .prompt_texts()
                .iter()
                .map(|text| {
                    crate::agent_execution::mailbox::open_all(
                        text,
                        &crate::agent_execution::mailbox::SealShaped,
                    )
                    .expect("each prompt is the courier's frames")
                    .into_iter()
                    .map(|opened| opened.body)
                    .collect()
                })
                .collect()
        }
    }

    #[test]
    fn every_waiting_message_reaches_the_conversation_in_one_prompt_in_order() {
        let mut fixture = Delivering::new(&["First", "Second", "Third"]);
        assert_eq!(fixture.pass(), None);
        assert_eq!(fixture.prompted(), [["First", "Second", "Third"]]);
        // Each settles on its own, as the recipient reading it would.
        assert_eq!(fixture.states(), fixture.all(State::Delivered));
        // Nothing is given twice.
        fixture.pass();
        assert_eq!(fixture.actor.prompt_count(), 1);
        fixture.actor.close();
    }

    #[test]
    fn the_persons_queued_message_goes_before_waiting_messages() {
        let mut fixture = Delivering::new(&["First", "Second"]);
        fixture.actor.set_persons_queue(true);
        fixture.pass();
        assert_eq!(
            fixture.actor.prompt_count(),
            0,
            "no courier turn ahead of the person"
        );
        assert_eq!(fixture.states(), fixture.all(State::Pending));

        // Seen without it, then refused by the actor once it is queued: the
        // claim is released, and the messages wait for the next turn's end.
        fixture.actor.set_persons_queue(false);
        assert!(fixture.actor.handle.takes_courier());
        fixture.actor.set_persons_queue(true);
        assert!(
            fixture
                .courier
                .hand_over(&fixture.messages, fixture.session, &fixture.actor.handle)
                .unwrap(),
            "the turn for the person's queue ends in a wake; no retry is needed"
        );
        assert_eq!(fixture.actor.prompt_count(), 0);
        assert_eq!(fixture.states(), fixture.all(State::Pending));

        fixture.actor.set_persons_queue(false);
        fixture.pass();
        assert_eq!(fixture.prompted(), [["First", "Second"]]);
        assert_eq!(fixture.states(), fixture.all(State::Delivered));
        fixture.actor.close();
    }

    #[test]
    fn a_window_holding_the_persons_followups_sends_them_first() {
        let mut fixture = Delivering::new(&["First", "Second"]);
        fixture.hold_followups(true);
        let before = super::super::now();
        let until = fixture
            .pass()
            .expect("the courier looks again when it stops waiting");
        let wait = PERSON_FIRST.as_millis() as u64;
        assert!((before + wait..=super::super::now() + wait).contains(&until));
        assert_eq!(fixture.actor.prompt_count(), 0);
        assert_eq!(fixture.states(), fixture.all(State::Pending));

        // The window sends them: the provider is at work on them, and the
        // messages wait for its turn to end.
        fixture.hold_followups(false);
        fixture.actor.set_persons_queue(true);
        fixture.pass();
        assert_eq!(fixture.actor.prompt_count(), 0);
        assert!(
            fixture.courier.0.yielding.lock().unwrap().is_empty(),
            "the next turn waits for its own follow-ups as long again"
        );
        fixture.actor.set_persons_queue(false);
        fixture.pass();
        assert_eq!(fixture.prompted(), [["First", "Second"]]);
        assert_eq!(fixture.states(), fixture.all(State::Delivered));
        fixture.actor.close();
    }

    #[test]
    fn a_window_that_never_sends_its_followups_holds_messages_back_only_so_long() {
        let mut fixture = Delivering::new(&["First", "Second"]);
        fixture.hold_followups(true);
        let until = fixture.pass().expect("the courier looks again");
        assert_eq!(fixture.actor.prompt_count(), 0);
        // Passes while it waits, with or without messages to deliver, keep
        // when the wait began.
        fixture.courier.forget_yields(&fixture.host);
        assert_eq!(fixture.pass(), Some(until));
        // As if the window had held them, unsent, for the whole wait.
        *fixture
            .courier
            .0
            .yielding
            .lock()
            .unwrap()
            .get_mut(&fixture.session)
            .unwrap() -= PERSON_FIRST.as_millis() as u64;
        fixture.pass();
        assert_eq!(fixture.prompted(), [["First", "Second"]]);
        assert_eq!(fixture.states(), fixture.all(State::Delivered));
        fixture.actor.close();
    }

    #[test]
    fn yielding_to_the_persons_followups_is_bounded_from_when_it_began() {
        let fixture = Delivering::new(&[]);
        let wait = PERSON_FIRST.as_millis() as u64;
        let start = 1_000_000;
        let yields = |session, now| fixture.courier.yields(session, now);
        assert_eq!(yields(fixture.session, start), Some(start + wait));
        assert_eq!(
            yields(fixture.session, start + wait - 1),
            Some(start + wait)
        );
        assert_eq!(yields(fixture.session, start + wait), None);
        let other = TerminalId::new();
        assert_eq!(yields(other, start + wait), Some(start + 2 * wait));
    }
}
