//! Reopens the conversation of a session whose provider stopped, as its pane
//! does when a window opens it or the person presses Resume, for callers with
//! no window: `cagents resume`, and an agent's next prompt.
//!
//! The daemon asks itself the same requests a pane would, in the same order,
//! so every check those requests make applies unchanged: the exact saved
//! conversation, its original directory and account, the startup recovery
//! hold, and the proof that no earlier process still writes it. A conversation
//! whose actor kept the provider (a retained Claude helper, a durable Codex
//! listener) is first prepared, which proves that owner ended and gives the
//! conversation a successor in its lineage, as is a successor prepared earlier
//! that never started; any other reopens in place. What the stopped provider
//! held is never resent.
use super::{EXECUTION_CLIENT, SharedHost, caller, directory, mailbox, registry, start_gate};
use crate::{
    agent_execution::{
        Agent, Authority, HostedSession, Lineage, Origin, Placement,
        directory::{Member, Scope as Sessions, Surface},
        resume::{Resumed, Scope, Stopped, StoppedSessions},
    },
    chat::{self, Session},
    model::{BootId, ChannelId, TerminalId},
    terminal_ipc::{ClientRequest, ProtocolError, ServerResponse},
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

/// How long a resume waits for its provider to say it is ready before
/// answering that it is still starting. A provider that fails to start,
/// such as one whose sign-in needs attention, usually says so well before.
/// The request holds the daemon's shared control admission meanwhile, so
/// this stays short: an update asked for then waits for no more than this.
const SETTLE: Duration = Duration::from_secs(5);

/// The boot pass can wait longer for a paired provider, outside control
/// admission, before taking it again to open the terminal.
const BOOT_PAIR_SETTLE: Duration = Duration::from_secs(25);

/// Who asks for a conversation to reopen, which decides what proves its
/// earlier process can no longer write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Basis {
    /// The person, or the session that owns it, asked by name, as a pane's
    /// Resume does. Starting it in place is the pane's own restore.
    Explicit,
    /// An agent's next prompt. Without an explicit request it reopens in place
    /// only when this daemon saw its provider exit, or an earlier boot ran it.
    Prompt,
    /// The daemon's own resume when this computer starts, which the person
    /// opted into. It reopens only what an earlier boot ran, whose processes
    /// are all gone.
    Boot,
}

/// The stopped sessions in `scope` that the caller may resume, one for each
/// lineage: every one for the person, and a session's own agents for a
/// session. Stopped chats only their pane can reopen are counted instead.
pub(super) fn stopped(
    host: &SharedHost,
    authority: &Authority,
    scope: Scope,
) -> Result<StoppedSessions, String> {
    listed(host, authority, scope).map(|listing| listing.found)
}

/// What [`stopped`] found, with what it read on the way.
struct Listing {
    found: StoppedSessions,
    /// The saved descriptor of each listed session.
    sources: std::collections::HashMap<TerminalId, Session>,
    /// Stopped chats with nothing saved to reopen, with why.
    unreadable: Vec<(TerminalId, String)>,
    /// The stopped chats `found` counts as reopened only from their panes.
    in_app: Vec<Session>,
}

fn listed(host: &SharedHost, authority: &Authority, scope: Scope) -> Result<Listing, String> {
    let (origin, _) = caller(host, authority)?;
    let wanted = match scope {
        Scope::Sessions(scope) => directory::Wanted::Scope(scope),
        Scope::Channel(channel) => directory::Wanted::Channel(channel),
    };
    let listing = directory::list(host, authority, wanted, true, false)?;
    let registry = registry(host)?;
    let agents = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agents(None)?;
    // Every lineage the listing shows, running or not, is represented by its
    // members; a lineage that runs now carries its conversation already.
    let shown: BTreeSet<Lineage> = listing
        .members()
        .map(|member| member.session.lineage)
        .collect();
    let running: BTreeSet<Lineage> = listing
        .members()
        .filter(|member| !member.exited)
        .map(|member| member.session.lineage)
        .collect();
    let mut candidates: Vec<&Member> = listing
        .members()
        .filter(|member| {
            member.exited
                && member.session.surface == Surface::Chat
                && !running.contains(&member.session.lineage)
        })
        .collect();
    let mut lineages = Successions::default();
    // The listing orders sessions for reading, not by when each ran, so each
    // lineage's members are put latest first by the order they succeeded one
    // another, keeping the order the lineages first appear in.
    let mut first_seen = std::collections::HashMap::new();
    for member in &candidates {
        let seen = first_seen.len();
        first_seen.entry(member.session.lineage).or_insert(seen);
    }
    let mut rank = std::collections::HashMap::new();
    for &lineage in first_seen.keys() {
        for (position, id) in lineages.of(&registry, lineage)?.iter().enumerate() {
            rank.insert(*id, position);
        }
    }
    candidates.sort_by_key(|member| {
        (
            first_seen[&member.session.lineage],
            rank.get(&member.session.id).copied().unwrap_or(usize::MAX),
        )
    });
    let mut sources = std::collections::HashMap::new();
    let mut unreadable = Vec::new();
    let mut in_app = Vec::new();
    let mut found = one_per_lineage(
        candidates,
        |member| member.session.lineage,
        |member| {
            // A session of the lineage this listing does not show, as in
            // another project, may run it.
            if lineages.runs(host, member.session.lineage) {
                return Candidate::Ends;
            }
            let Some(provider) = member.session.provider else {
                return Candidate::Ends;
            };
            let agent = open_agent(&agents, member.session.id);
            if !may_resume(&origin, agent) {
                return Candidate::NotTheirs;
            }
            let read = descriptor(host, &registry, member.session.id, agent);
            // What the boot pass needs of each one, read once here.
            match &read {
                Ok(Some(source)) if resumable(source) => {
                    sources.insert(member.session.id, source.clone());
                }
                Ok(Some(source)) => in_app.push(source.clone()),
                Err(error) => unreadable.push((member.session.id, error.clone())),
                Ok(None) => {}
            }
            candidate(member.session.id, read, || Stopped {
                id: member.session.id,
                provider,
                title: member.session.title.clone(),
                agent: agent.map(|agent| agent.id),
                failure: member.failure.clone(),
            })
        },
    );
    // An agent no pane shows, such as one started from a shell on a machine
    // no window opened, is listed by its agent alone, in the scope it was
    // placed in, when its session is still the one its lineage last
    // continued and nothing of that lineage runs.
    let in_scope = placed_in(&registry, &listing.served)?;
    let hosted: BTreeSet<TerminalId> = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .keys()
        .copied()
        .collect();
    for agent in &agents {
        let Some(HostedSession::Chat(session)) = &agent.session else {
            continue;
        };
        if hosted.contains(&session.id)
            || !in_scope(&agent.placement)
            || !resumable(session)
            || !may_resume(&origin, Some(agent))
        {
            continue;
        }
        let lineage = registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .session_lineage(session.id)?
            .unwrap_or(Lineage(session.id));
        if shown.contains(&lineage)
            || lineages.of(&registry, lineage)?.first() != Some(&session.id)
            || lineages.runs(host, lineage)
        {
            continue;
        }
        sources.insert(session.id, session.clone());
        found.sessions.push(Stopped {
            id: session.id,
            provider: session.launch.provider,
            title: agent.name.clone(),
            agent: Some(agent.id),
            failure: None,
        });
    }
    Ok(Listing {
        found,
        sources,
        unreadable,
        in_app,
    })
}

/// Each lineage's sessions, latest first, read once per listing.
#[derive(Default)]
struct Successions(std::collections::HashMap<Lineage, Vec<TerminalId>>);

impl Successions {
    fn of(
        &mut self,
        registry: &super::SharedRegistry,
        lineage: Lineage,
    ) -> Result<&[TerminalId], String> {
        use std::collections::hash_map::Entry;
        Ok(match self.0.entry(lineage) {
            Entry::Occupied(read) => read.into_mut(),
            Entry::Vacant(unread) => unread.insert(
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .lineage_succession(lineage)?,
            ),
        })
    }

    /// Whether any read session of `lineage` runs on this daemon now.
    fn runs(&self, host: &SharedHost, lineage: Lineage) -> bool {
        self.0.get(&lineage).is_some_and(|sessions| {
            host.lock()
                .unwrap_or_else(|p| p.into_inner())
                .any_live(sessions)
        })
    }
}

/// What a stopped chat whose saved descriptor read as `descriptor` is to a
/// listing: resumable as `stopped` when a resume could start it, so a
/// scope's resume fails only for what was worth trying.
fn candidate(
    id: TerminalId,
    descriptor: Result<Option<Session>, String>,
    stopped: impl FnOnce() -> Stopped,
) -> Candidate {
    match descriptor {
        Ok(Some(source)) if resumable(&source) => Candidate::Resumable(stopped()),
        Ok(Some(_)) => Candidate::InAppOnly,
        // This daemon runs it after all.
        Ok(None) => Candidate::Ends,
        // Nothing saved to reopen, as for one closed meanwhile.
        Err(error) => {
            tracing::debug!(session = %id, %error, "a stopped chat left out of the resumable listing");
            Candidate::Ends
        }
    }
}

/// Whether an agent placed at a given placement is in the scope a listing
/// served: anywhere for every project; its channel, or a view of the
/// project's own, for a project; that channel alone for a channel.
fn placed_in(
    registry: &super::SharedRegistry,
    served: &directory::Wanted,
) -> Result<impl Fn(&Placement) -> bool, String> {
    let channels: std::collections::HashMap<ChannelId, std::path::PathBuf> = match served {
        directory::Wanted::Scope(Sessions::Project(_)) => registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .model()?
            .channels
            .into_iter()
            .map(|channel| (channel.id, channel.repository))
            .collect(),
        _ => Default::default(),
    };
    let served = match served {
        directory::Wanted::Scope(Sessions::All) => Served::All,
        directory::Wanted::Scope(Sessions::Project(project)) => Served::Project(project.clone()),
        directory::Wanted::Channel(Some(channel)) => Served::Channel(*channel),
        // A listing serves a containing directory as its project and the
        // caller's channel by its ID; nothing else is left to match.
        _ => Served::Nothing,
    };
    Ok(move |placement: &Placement| match (&served, placement) {
        (Served::All, _) => true,
        (Served::Project(project), Placement::Worktree { repository, .. }) => repository == project,
        (Served::Project(project), Placement::Channel(channel)) => {
            channels.get(channel) == Some(project)
        }
        (Served::Channel(served), Placement::Channel(channel)) => served == channel,
        _ => false,
    })
}

enum Served {
    All,
    Project(std::path::PathBuf),
    Channel(ChannelId),
    Nothing,
}

/// What one stopped chat is to a listing of what may be resumed.
enum Candidate {
    Resumable(Stopped),
    /// Stopped, but only its pane in the app can reopen it.
    InAppOnly,
    /// Not the caller's to resume; the lineage's next one may be.
    NotTheirs,
    /// Nothing of the lineage is offered: it runs after all, or what it last
    /// continued cannot be read, and an older member would be stale.
    Ends,
}

/// Each lineage's latest conversation the caller may resume, in the order
/// lineages first appear, and how many lineages only the app can reopen.
/// Each lineage's candidates come latest first. A lineage offers only its
/// first one the caller may resume, and nothing when that one only the app
/// can reopen or it ends the lineage: an older member would reopen a stale
/// conversation in place of the one the lineage last continued.
fn one_per_lineage<T, L: Eq + std::hash::Hash>(
    candidates: impl IntoIterator<Item = T>,
    lineage: impl Fn(&T) -> L,
    mut classify: impl FnMut(&T) -> Candidate,
) -> StoppedSessions {
    let mut order: Vec<Vec<T>> = Vec::new();
    let mut index: std::collections::HashMap<L, usize> = std::collections::HashMap::new();
    for candidate in candidates {
        let slot = *index.entry(lineage(&candidate)).or_insert_with(|| {
            order.push(Vec::new());
            order.len() - 1
        });
        order[slot].push(candidate);
    }
    let mut found = StoppedSessions::default();
    for members in order {
        match members
            .iter()
            .map(&mut classify)
            .find(|candidate| !matches!(candidate, Candidate::NotTheirs))
        {
            Some(Candidate::Resumable(stopped)) => found.sessions.push(stopped),
            Some(Candidate::InAppOnly) => found.in_app_only += 1,
            Some(Candidate::NotTheirs | Candidate::Ends) | None => {}
        }
    }
    found
}

/// Reopens the stopped conversation `handle` names, a session, lineage or
/// agent ID or a prefix of one, and says what runs it now.
pub(super) fn resume(
    host: &SharedHost,
    authority: &Authority,
    handle: &str,
) -> Result<Resumed, String> {
    resume_as(host, authority, handle, Basis::Explicit)
}

fn resume_as(
    host: &SharedHost,
    authority: &Authority,
    handle: &str,
    basis: Basis,
) -> Result<Resumed, String> {
    let (origin, _) = caller(host, authority)?;
    let listing = directory::list(
        host,
        authority,
        directory::Wanted::Scope(Sessions::All),
        true,
        false,
    )?;
    let registry = registry(host)?;
    let agents = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agents(None)?;
    let named = match resolve(
        listing.members(),
        &agents,
        handle,
        |lineage| {
            registry
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .lineage_succession(lineage)
        },
        |id| may_resume(&origin, open_agent(&agents, id)),
    )? {
        Resolved::Running(member) => {
            let repaired = retry_codex_pair(host, &registry, &origin, &agents, member)?;
            let id = member.session.id;
            return Ok(Resumed {
                stopped: repaired.as_ref().map_or(id, |(stopped, _)| *stopped),
                session: id,
                provider: member
                    .session
                    .provider
                    .ok_or("That session runs no provider")?,
                title: member.session.title.clone(),
                agent: registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .agent_for_session(id)?
                    .filter(|agent| !agent.closed)
                    .map(|agent| agent.id),
                already_running: true,
                starting: repaired
                    .as_ref()
                    .is_some_and(|(_, reopened)| reopened.starting == Some(true)),
                note: repaired.and_then(|(_, reopened)| {
                    reopened.note.or_else(|| {
                        (basis != Basis::Boot).then(|| "Reopened its paired terminal.".into())
                    })
                }),
            });
        }
        Resolved::Stopped(id, title) => (id, title),
    };
    let (id, title) = named;
    let agent = open_agent(&agents, id).cloned();
    if !may_resume(&origin, agent.as_ref()) {
        return Err(format!(
            "Session {id} is not yours to resume: a session may resume only the agents it started"
        ));
    }
    // An agent's own start resumes it too. Serialize the provider start
    // and any paired terminal before allowing its next prompt to run.
    let gate = agent.as_ref().map(|agent| start_gate(host, agent.id));
    let starting_gate = gate
        .as_ref()
        .map(|gate| gate.lock().unwrap_or_else(|p| p.into_inner()));
    let source = descriptor(host, &registry, id, agent.as_ref())?;
    let Some(source) = source else {
        let session = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&id)
            .map(|record| record.session.launch.provider);
        return match session {
            Some(provider) => Ok(Resumed {
                stopped: id,
                session: id,
                provider,
                title,
                agent: agent.map(|agent| agent.id),
                already_running: true,
                starting: false,
                note: None,
            }),
            None => Err(format!(
                "Session {id} has no saved conversation to resume; it may have been closed"
            )),
        };
    };
    // This request already holds the shared control admission; taking it
    // again could wait behind a queued exclusive request that waits on it.
    open_for_work(host)?;
    let reopened = reopen(host, &source, basis)?;
    drop(starting_gate);
    let started = reopened.session;
    let starting = match reopened.starting {
        Some(starting) => starting,
        None => settle(host, &started)?,
    };
    let bound = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_for_session(started.id)?
        .filter(|agent| !agent.closed);
    let note = reopened.note.or_else(|| match (&agent, &bound) {
        (Some(agent), None) => Some(format!(
            "Its agent {} did not take the reopened conversation over, so prompts for that agent still cannot reach it",
            agent.id
        )),
        _ => None,
    });
    Ok(Resumed {
        stopped: id,
        session: started.id,
        provider: started.launch.provider,
        title,
        agent: bound.map(|agent| agent.id),
        already_running: false,
        starting,
        note,
    })
}

pub(super) struct Reopened {
    pub(super) session: Session,
    pub(super) note: Option<String>,
    starting: Option<bool>,
}

/// Reopens `source`'s conversation as its pane would, and moves the pane
/// showing it to the process that now runs it. Returns that process.
pub(super) fn reopen(
    host: &SharedHost,
    source: &Session,
    basis: Basis,
) -> Result<Reopened, String> {
    let registry = registry(host)?;
    // An agent's record keeps the descriptor it launched; the window showing
    // it saves the conversation its provider confirmed into the pane.
    let learned = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chat_pane_descriptor(source.id)?
        .filter(|pane| {
            source.launch.session_id.is_none()
                && pane.launch.session_id.is_some()
                && crate::agent_execution::same_chat_process(pane, source)
        });
    let source = learned.as_ref().unwrap_or(source);
    let earlier_boot = super::super::chat::from_earlier_boot(host, source);
    if basis == Basis::Boot && !earlier_boot {
        return Err(
            "This boot already ran this conversation, so it is not resumed by itself".into(),
        );
    }
    let request = |request| {
        super::super::chat::process(host, EXECUTION_CLIENT, request)
            .map_err(|error: ProtocolError| error.message)
    };
    let (proof, ended_here) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let hosted = state.chats.get(&source.id);
        if hosted.is_some_and(|record| record.provider_running()) {
            return Err("This conversation is already running".into());
        }
        (
            state.proof().clone(),
            hosted.is_some_and(|record| !record.handle.is_read_only_history()),
        )
    };
    let codex_pair = will_reopen_codex_terminal(source)?;
    if codex_pair && basis == Basis::Prompt {
        return Err(format!(
            "Run `cagents resume {}` to reopen the conversation and its terminal before sending a prompt",
            source.id
        ));
    }
    // An update that stopped this conversation names the original it resumes.
    let original = crate::upgrade::floor::load()
        .ok()
        .and_then(|ledger| ledger.resume_source(source).cloned());
    // A successor prepared but never started is prepared again, as its pane
    // does: that is what refuses while an earlier process may still write
    // its conversation.
    let prepare = source.native_continuity.retains_provider()
        || chat::is_unstarted_resume_successor(source)
        || original.is_some()
        || codex_pair;
    let mut start = if prepare {
        let asked = original.unwrap_or_else(|| source.clone());
        let ServerResponse::ChatResumePrepared { session: prepared } =
            request(ClientRequest::ChatPrepareResume {
                source: asked.clone(),
            })?
        else {
            return Err("The daemon answered the resume with something else".into());
        };
        start_prepared(&asked, prepared)?
    } else {
        if source.launch.session_id.is_none() {
            return Err(
                "This conversation never recorded a provider conversation to resume; start a new one instead"
                    .into(),
            );
        }
        if basis == Basis::Prompt && !ended_here && !earlier_boot {
            return Err(format!(
                "The daemon restarted since its provider last ran, so it cannot tell whether that process still writes the conversation. Once it has ended, resume it with `cagents resume {}`",
                source.id
            ));
        }
        // The live daemon proved itself on this request; the start carries
        // its proof, as a pane's restore does, and the daemon checks it
        // against any closed record it still holds under this ID.
        let mut session = source.clone();
        session.proof = Some(proof.clone());
        session
    };
    if !prepare && let Some(terminal) = &source.terminal {
        if terminal.id != source.id {
            return Err(
                "The saved source terminal has a different allocation from this conversation"
                    .into(),
            );
        }
        // A legacy conversion's terminal ran the provider itself. Replaying
        // that recipe alongside the native provider would give it two writers.
        super::super::chat::require_ended_resume_terminal(host, terminal, &proof, earlier_boot)?;
        start.terminal = None;
    }
    if start.launch.needs_read_only_history() {
        return Err(format!(
            "The saved working directory {} is missing. Restore it before resuming the provider",
            start.launch.recipe.working_directory.display()
        ));
    }
    let response = if codex_pair {
        // Keep the original pane and agent binding until both views attach
        // so a temporary terminal failure can reuse this same provider.
        super::super::chat::start_for_paired_resume(host, EXECUTION_CLIENT, start)
            .map_err(|error| error.message)?
    } else {
        request(ClientRequest::ChatStart {
            session: start,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        })?
    };
    let ServerResponse::ChatAttached {
        session: started, ..
    } = response
    else {
        return Err("The daemon answered the start with something else".into());
    };
    if codex_pair {
        return finish_codex_pair(host, &registry, source, started);
    }
    save_pane(host, &registry, source, &started);
    Ok(Reopened {
        session: started,
        note: None,
        starting: None,
    })
}

fn will_reopen_codex_terminal(source: &Session) -> Result<bool, String> {
    let Some(terminal) = &source.terminal else {
        return Ok(false);
    };
    if has_managed_console(source) {
        return Err(
            "This terminal is a managed agent console. Reopen it through its agent's pane".into(),
        );
    }
    if source.launch.provider != crate::agent::Provider::Codex {
        return Ok(false);
    }
    if terminal.id != source.id || !super::super::chat::can_reopen_codex_terminal(source) {
        return Err(
            "The saved Codex terminal cannot be reopened as a view of this exact conversation"
                .into(),
        );
    }
    Ok(true)
}

/// A failed terminal start leaves the original paired pane in the registry.
/// A later explicit resume can finish that pair against the exact successor
/// that is already serving its thread.
fn retry_codex_pair(
    host: &SharedHost,
    registry: &super::SharedRegistry,
    origin: &Origin,
    agents: &[Agent],
    running: &Member,
) -> Result<Option<(TerminalId, Reopened)>, String> {
    if running.session.surface != Surface::Chat
        || running.session.provider != Some(crate::agent::Provider::Codex)
    {
        return Ok(None);
    }
    let Some(started) = hosted_provider(host, running.session.id) else {
        return Ok(None);
    };
    let source = {
        let store = registry.lock().unwrap_or_else(|p| p.into_inner());
        let mut found = None;
        for id in store.lineage_succession(running.session.lineage)? {
            if id == started.id || !may_resume(origin, open_agent(agents, id)) {
                continue;
            }
            let Some(source) = store.chat_pane_descriptor(id)? else {
                continue;
            };
            if will_reopen_codex_terminal(&source) == Ok(true)
                && is_exact_paired_successor(&source, &started)
            {
                found = Some(source);
                break;
            }
        }
        found
    };
    let Some(source) = source else {
        return Ok(None);
    };
    let gate = open_agent(agents, source.id).map(|agent| start_gate(host, agent.id));
    let _starting_gate = gate
        .as_ref()
        .map(|gate| gate.lock().unwrap_or_else(|p| p.into_inner()));
    open_for_work(host)?;
    finish_codex_pair(host, registry, &source, started).map(|reopened| Some((source.id, reopened)))
}

fn hosted_provider(host: &SharedHost, id: TerminalId) -> Option<Session> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&id)
        .filter(|record| record.provider_running())
        .map(|record| record.current_session())
}

fn is_exact_paired_successor(source: &Session, started: &Session) -> bool {
    let mut expected = source.clone();
    expected.reset_owned_codex_listener();
    expected.id = started.id;
    expected.terminal = None;
    expected.launch == started.launch || expected.resolved_to_owned_codex_listener(started)
}

fn finish_codex_pair(
    host: &SharedHost,
    registry: &super::SharedRegistry,
    source: &Session,
    started: Session,
) -> Result<Reopened, String> {
    let starting = settle(host, &started)?;
    let paired = if starting {
        Err("The provider is still starting".into())
    } else {
        super::super::chat::reopen_codex_terminal(host, EXECUTION_CLIENT, &started)
            .map_err(|error| error.message)
    };
    let (session, note) = match paired {
        Ok(paired) => {
            super::follow_restart(host, &paired);
            save_pane(host, registry, source, &paired);
            (paired, None)
        }
        Err(reason) => (
            started,
            Some(format!(
                "{reason}. Run `cagents resume {}` again to restore the paired terminal",
                source.id
            )),
        ),
    };
    Ok(Reopened {
        session,
        note,
        starting: Some(starting),
    })
}

fn save_pane(
    host: &SharedHost,
    registry: &super::SharedRegistry,
    source: &Session,
    started: &Session,
) {
    let moved = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .adopt_resumed_chat_pane(source, started);
    match moved {
        Ok(true) => super::announce(host, registry),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(chat = %started.id, %error, "the reopened conversation's pane still names its stopped process");
        }
    }
}

/// What starts after the daemon prepared `asked` as `prepared`: the
/// conversation itself when a live owner keeps it or a shared Codex server may
/// serve it, which the start restores and reattaches, never replacing it, as
/// the pane's does; or the successor preparation gave it.
fn start_prepared(asked: &Session, prepared: Session) -> Result<Session, String> {
    if !chat::is_prepared_resume(asked, &prepared) {
        return Err(
            "The daemon's resume would not reopen this exact conversation, so nothing was started"
                .into(),
        );
    }
    Ok(prepared)
}

/// Refuses new work while the daemon is closing or an update holds it back,
/// as the courier's own starts are refused.
fn open_for_work(host: &SharedHost) -> Result<(), String> {
    crate::host::client::ensure_control_open(host).map_err(|_| super::NEW_WORK_HELD.to_owned())?;
    if mailbox::defers_for_update(host) {
        return Err(super::NEW_WORK_HELD.into());
    }
    Ok(())
}

/// Whether there is a saved conversation for the daemon to try to reopen.
fn resumable(session: &Session) -> bool {
    !has_managed_console(session)
        && (session.launch.session_id.is_some() || session.native_continuity.retains_provider())
}

fn has_managed_console(session: &Session) -> bool {
    session
        .terminal
        .as_ref()
        .is_some_and(|terminal| crate::agent_execution::console_agent(&terminal.launch).is_some())
}

/// Waits a bounded time for the provider `started` runs to be ready, and
/// fails with its reason when it stops instead. Returns whether it is still
/// starting.
fn settle(host: &SharedHost, started: &Session) -> Result<bool, String> {
    settle_for(host, started.id, started.launch.provider, SETTLE)
}

fn settle_for(
    host: &SharedHost,
    session: TerminalId,
    provider: crate::agent::Provider,
    budget: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + budget;
    loop {
        let handle = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&session)
            .map(|record| record.handle.clone());
        let Some(handle) = handle else {
            return Err("The reopened conversation stopped as soon as it started".into());
        };
        if handle.is_read_only_history() {
            return Err("The conversation is only available as saved history. Restore its working directory before resuming the provider".into());
        }
        if handle.is_completed() {
            return Err(match handle.failure() {
                Some(failure) => chat::failure::summary(provider, &failure).into_owned(),
                None => "The reopened conversation stopped as soon as it started".into(),
            });
        }
        if handle.is_ready() {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The saved descriptor of the stopped conversation `id`: the one this
/// daemon still holds, else the one its pane saves, else its agent's. `None`
/// when this daemon runs it now.
fn descriptor(
    host: &SharedHost,
    registry: &super::SharedRegistry,
    id: TerminalId,
    agent: Option<&Agent>,
) -> Result<Option<Session>, String> {
    let reader = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        match state.chats.get(&id) {
            Some(record) if record.provider_running() => return Ok(None),
            Some(record) if record.handle.is_read_only_history() => Some(record.current_session()),
            Some(record) => return Ok(Some(record.current_session())),
            None => None,
        }
    };
    let pane = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chat_pane_descriptor(id)?;
    match (reader, pane) {
        (Some(reader), Some(pane))
            if Session {
                proof: reader.proof.clone(),
                ..pane.clone()
            } == reader =>
        {
            // The reader's proof describes when its history was loaded.
            // The pane still records the boot that ran the provider.
            return Ok(Some(pane));
        }
        (Some(reader), _) => return Ok(Some(reader)),
        (None, Some(pane)) => return Ok(Some(pane)),
        (None, None) => {}
    }
    match agent.and_then(|agent| agent.session.as_ref()) {
        Some(HostedSession::Chat(session)) if session.id == id => Ok(Some(session.clone())),
        _ => Err(format!(
            "Session {id} has no saved conversation to resume; it may have been closed"
        )),
    }
}

/// The open agent whose session `id` is.
pub(super) fn open_agent(agents: &[Agent], id: TerminalId) -> Option<&Agent> {
    agents.iter().find(|agent| {
        !agent.closed
            && agent
                .session
                .as_ref()
                .is_some_and(|session| session.id() == id)
    })
}

/// The person resumes anything; a session or automation only what it started.
pub(super) fn may_resume(origin: &Origin, agent: Option<&Agent>) -> bool {
    *origin == Origin::User || agent.is_some_and(|agent| agent.origin == *origin)
}

pub(super) enum Resolved<'a> {
    /// The lineage `handle` names runs now, as this session.
    Running(&'a Member),
    /// This stopped chat, with its title.
    Stopped(TerminalId, Option<String>),
}

/// The one conversation `handle` names among the listed provider sessions
/// and open agents, by session, lineage or agent ID, whole or a prefix, as
/// `cagents send` names its recipient. A lineage with a running session
/// resolves to that session; `latest_first` reads a lineage's sessions,
/// latest first, and `mine` says whether the caller may resume a session.
pub(super) fn resolve<'a>(
    members: impl Iterator<Item = &'a Member>,
    agents: &[Agent],
    handle: &str,
    latest_first: impl FnOnce(Lineage) -> Result<Vec<TerminalId>, String>,
    mine: impl Fn(TerminalId) -> bool,
) -> Result<Resolved<'a>, String> {
    let handle = handle.trim().to_ascii_lowercase();
    if handle.len() < mailbox::MIN_HANDLE {
        return Err(format!(
            "Name the session by at least {} characters of its ID",
            mailbox::MIN_HANDLE
        ));
    }
    let matches = |id: &dyn std::fmt::Display| id.to_string().starts_with(&handle);
    let members: Vec<&Member> = members
        .filter(|member| member.session.provider.is_some())
        .collect();
    let agent_of = |id: TerminalId| open_agent(agents, id);
    let named: Vec<&Member> = members
        .iter()
        .copied()
        .filter(|member| {
            matches(&member.session.id)
                || matches(&member.session.lineage)
                || agent_of(member.session.id).is_some_and(|agent| matches(&agent.id))
        })
        .collect();
    let lineages: BTreeSet<_> = named.iter().map(|member| member.session.lineage).collect();
    if lineages.len() > 1 {
        return Err(format!(
            "{} sessions match {handle}; give more of the ID",
            lineages.len()
        ));
    }
    let Some(lineage) = lineages.into_iter().next() else {
        // An open agent whose conversation no pane or process shows.
        let mut found = agents.iter().filter(|agent| {
            !agent.closed
                && agent.session.as_ref().is_some_and(|session| {
                    matches!(session, HostedSession::Chat(_))
                        && (matches(&agent.id) || matches(&session.id()))
                })
        });
        return match (found.next(), found.next()) {
            (Some(agent), None) => Ok(Resolved::Stopped(
                agent.session.as_ref().expect("filtered").id(),
                agent.name.clone(),
            )),
            (Some(_), Some(_)) => Err(format!(
                "More than one agent matches {handle}; give more of the ID"
            )),
            (None, _) => Err(format!(
                "No session or agent matches {handle}; `cagents resume` lists the stopped sessions you can resume"
            )),
        };
    };
    let lineage_id = lineage;
    let lineage: Vec<&Member> = members
        .iter()
        .copied()
        .filter(|member| member.session.lineage == lineage_id)
        .collect();
    if let Some(running) = lineage
        .iter()
        .copied()
        .filter(|member| !member.exited)
        .min_by_key(|member| member.session.surface != Surface::Chat)
    {
        return Ok(Resolved::Running(running));
    }
    let mut stopped: Vec<&Member> = lineage
        .iter()
        .copied()
        .filter(|member| member.session.surface == Surface::Chat)
        .collect();
    let rank: std::collections::HashMap<TerminalId, usize> = latest_first(lineage_id)?
        .into_iter()
        .enumerate()
        .map(|(position, id)| (id, position))
        .collect();
    stopped.sort_by_key(|member| rank.get(&member.session.id).copied().unwrap_or(usize::MAX));
    // A session named by its own ID is that one, and an agent named by its
    // ID is its own session. Otherwise the handle names the lineage, which
    // reopens the conversation a listing offers for it: the latest one the
    // caller may resume. The root's session ID is the lineage's ID, so the
    // root named whole or by a prefix is the lineage too: reopening the root
    // past a later conversation would continue a stale one.
    let only = |chosen: Vec<&'a Member>| match chosen.as_slice() {
        [one] => Some(*one),
        _ => None,
    };
    let chosen = only(
        stopped
            .iter()
            .copied()
            .filter(|member| member.session.id != lineage_id.0 && matches(&member.session.id))
            .collect(),
    )
    .or_else(|| {
        only(
            stopped
                .iter()
                .copied()
                .filter(|member| {
                    agent_of(member.session.id).is_some_and(|agent| matches(&agent.id))
                })
                .collect(),
        )
    })
    .or_else(|| {
        stopped
            .iter()
            .copied()
            .find(|member| mine(member.session.id))
            .or_else(|| stopped.first().copied())
    });
    match chosen {
        Some(member) => Ok(Resolved::Stopped(
            member.session.id,
            member.session.title.clone(),
        )),
        None => Err(format!(
            "{handle} names no stopped conversation; a terminal session reopens from its pane in the app"
        )),
    }
}

/// Whether the person asked for sessions to resume when this computer starts:
/// the `resume_sessions_after_reboot` preference, off unless it says otherwise.
fn resumes_after_boot() -> bool {
    let Some(path) = crate::instance::settings_path() else {
        return false;
    };
    crate::settings::resume_sessions_after_reboot(&path).unwrap_or_else(|error| {
        tracing::warn!(%error, "sessions are not resumed now that this computer started");
        false
    })
}

/// Names the last boot whose resume ran to the end, in the daemon's data
/// directory, which a logout leaves in place. Only the first daemon of a boot
/// resumes anything by itself: one restarted later in that boot, as by an
/// update or after a crash, finds its boot here and leaves what is stopped
/// for the person.
const HANDLED_BOOT: &str = "resumed-boot";
const HANDLED_BOOT_LIMIT: u64 = 256;

fn handled_boot_directory() -> Option<std::path::PathBuf> {
    crate::instance::project_dirs().map(|dirs| dirs.data_local_dir().to_path_buf())
}

/// The boot `directory` records as handled, if it records one.
fn handled_boot(directory: &std::path::Path) -> Result<Option<BootId>, String> {
    use std::io::Read;
    let path = directory.join(HANDLED_BOOT);
    let file = match crate::secure_file::open_existing_private_file_readonly(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let mut recorded = String::new();
    file.take(HANDLED_BOOT_LIMIT)
        .read_to_string(&mut recorded)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let recorded = recorded.trim();
    Ok((!recorded.is_empty()).then(|| BootId::new(recorded)))
}

/// Records `boot` as handled, replacing the file whole so a crash leaves
/// either the earlier boot or this one.
fn record_handled_boot(directory: &std::path::Path, boot: &BootId) -> Result<(), String> {
    use crate::fs_safety;
    use std::io::Write;
    let failed = |error: &dyn std::fmt::Display| {
        format!(
            "cannot record {} as resumed: {error}",
            directory.join(HANDLED_BOOT).display()
        )
    };
    crate::secure_file::prepare_private_directory(directory).map_err(|e| failed(&e))?;
    let root = fs_safety::Root::open(directory).map_err(|e| failed(&e))?;
    let (mut file, temporary) =
        fs_safety::create_temporary_in(&root, 0o600).map_err(|e| failed(&e))?;
    let written = (|| {
        file.write_all(format!("{boot}\n").as_bytes())?;
        file.sync_all()?;
        fs_safety::rename_within(&root, &temporary, HANDLED_BOOT.as_ref())
            .map_err(std::io::Error::other)
    })();
    if let Err(error) = written {
        let _ = fs_safety::unlink_within(&root, &temporary);
        return Err(failed(&error));
    }
    if let Err(error) = fs_safety::sync_directory(&root) {
        tracing::debug!(%error, "the resumed boot may not survive a crash");
    }
    Ok(())
}

/// What the daemon's own resume did when it started.
#[derive(Debug)]
pub(super) enum AfterBoot {
    /// This boot's resume already ran, so this daemon was restarted within it.
    AlreadyRan,
    /// The person did not ask for it.
    Off,
    Ran(Pass),
}

/// One run of the resume over what an earlier boot left stopped.
#[derive(Debug, Default)]
pub(super) struct Pass {
    pub(super) resumed: Vec<Resumed>,
    /// Each session that could not resume, with why.
    pub(super) failed: Vec<(Stopped, String)>,
    /// Stopped sessions it did not try: ones only their pane in the app can
    /// reopen, and ones whose saved record cannot prove their process ended.
    /// `cagents resume` lists or counts them.
    pub(super) left: Vec<Session>,
    /// The daemon stopped taking new work, as for an update or a shutdown,
    /// before the pass ended. The next daemon of this boot runs it again.
    pub(super) interrupted: bool,
}

/// Once the daemon serves, resumes, if the person opted in and this is the
/// first daemon of this boot to get this far, every stopped conversation an
/// earlier boot ran, as `cagents resume --all` would. Each one that cannot
/// resume is announced and stays listed by `cagents resume`.
pub(in crate::host) fn resume_after_boot(host: std::sync::Weak<crate::host::state::HostLock>) {
    if let AfterBoot::Ran(pass) = resume_once_per_boot(&host) {
        tracing::info!(
            resumed = pass.resumed.len(),
            failed = pass.failed.len(),
            left = pass.left.len(),
            interrupted = pass.interrupted,
            "resumed what an earlier boot ran"
        );
    }
}

/// [`resume_after_boot`]'s pass, unless this boot's already ran or the
/// person did not ask for one.
pub(super) fn resume_once_per_boot(
    host: &std::sync::Weak<crate::host::state::HostLock>,
) -> AfterBoot {
    let Some(boot) = host.upgrade().map(|host| {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .proof()
            .boot_id
            .clone()
    }) else {
        return AfterBoot::Ran(Pass {
            interrupted: true,
            ..Pass::default()
        });
    };
    let Some(directory) = handled_boot_directory().filter(|_| !boot.as_str().is_empty()) else {
        tracing::warn!(
            "this daemon cannot tell which boot it serves, so it resumes nothing by itself"
        );
        return AfterBoot::Off;
    };
    match handled_boot(&directory) {
        Ok(Some(handled)) if handled == boot => return AfterBoot::AlreadyRan,
        Ok(_) => {}
        // Each conversation's own proof still keeps one this boot ran.
        Err(error) => tracing::warn!(%error, "the last resumed boot is unknown"),
    }
    let record = || {
        if let Err(error) = record_handled_boot(&directory, &boot) {
            tracing::warn!(%error, "a daemon restarted within this boot may resume again");
        }
    };
    // Recorded even when off, so turning the setting on later in this boot
    // waits for the next one.
    if !resumes_after_boot() {
        record();
        return AfterBoot::Off;
    }
    let pass = pass(host);
    if !pass.interrupted {
        record();
    }
    AfterBoot::Ran(pass)
}

fn pass(host: &std::sync::Weak<crate::host::state::HostLock>) -> Pass {
    let interrupted = || Pass {
        interrupted: true,
        ..Pass::default()
    };
    let person = Authority::default();
    let Some(shared) = host.upgrade() else {
        return interrupted();
    };
    let listing = match listed(&shared, &person, Scope::Sessions(Sessions::All)) {
        Ok(listing) => listing,
        Err(error) => {
            tracing::warn!(%error, "the sessions to resume could not be listed");
            return interrupted();
        }
    };
    let Listing {
        found,
        mut sources,
        unreadable,
        mut in_app,
    } = listing;
    // As below: one this boot ran is no earlier boot's to report, as when an
    // interrupted pass runs again.
    in_app.retain(|session| !ran_this_boot(&shared, session));
    drop(shared);
    for (session, error) in unreadable {
        tracing::info!(%session, %error, "not resumed: nothing saved to reopen");
    }
    for session in &in_app {
        tracing::info!(session = %session.id, "not resumed: the saved session requires its app pane");
    }
    let mut pass = Pass {
        left: in_app,
        ..Pass::default()
    };
    for stopped in found.sessions {
        let Some(host) = host.upgrade() else {
            pass.interrupted = true;
            break;
        };
        let Some(source) = sources.remove(&stopped.id) else {
            continue;
        };
        if source
            .proof
            .as_ref()
            .is_none_or(|proof| !proof.is_complete())
        {
            tracing::info!(session = %stopped.id, "not resumed: its saved record cannot prove its process ended");
            pass.left.push(source);
            continue;
        }
        // Run by this boot already, as by a window that opened first.
        if ran_this_boot(&host, &source) {
            tracing::info!(session = %stopped.id, "not resumed: this boot already ran it");
            continue;
        }
        // A window that opened meanwhile restores what it shows itself.
        if window_holds(&host, stopped.id) {
            tracing::info!(session = %stopped.id, "not resumed: a window is reopening it");
            continue;
        }
        // Under the daemon's control admission, as every other start, so an
        // update's checkpoint or a shutdown sees the provider it starts.
        let Some(outcome) = mailbox::with_control(&host, || {
            resume_as(&host, &person, &stopped.id.to_string(), Basis::Boot)
        }) else {
            pass.interrupted = true;
            break;
        };
        match outcome {
            Ok(mut resumed) => {
                if resumed.starting && will_reopen_codex_terminal(&source) == Ok(true) {
                    // Provider startup can outlast a control request. Wait
                    // without holding admission, then finish the same pair.
                    let deferred = match settle_for(
                        &host,
                        resumed.session,
                        resumed.provider,
                        BOOT_PAIR_SETTLE,
                    ) {
                        Ok(false) => mailbox::with_control(&host, || {
                            finish_boot_codex_pair(&host, &source, &resumed).map(Some)
                        }),
                        Ok(true) => Some(Ok(None)),
                        Err(reason) => Some(Err(reason)),
                    };
                    match deferred {
                        Some(Ok(Some(finished))) => resumed = finished,
                        Some(Ok(None)) => {}
                        None => {
                            pass.interrupted = true;
                            break;
                        }
                        Some(Err(_)) if open_for_work(&host).is_err() => {
                            pass.interrupted = true;
                            break;
                        }
                        Some(Err(reason)) => {
                            announce_failure(&host, &source, &stopped, &reason);
                            pass.failed.push((stopped, reason));
                            continue;
                        }
                    }
                }
                if let Some(note) = &resumed.note {
                    tracing::warn!(session = %stopped.id, %note, "resume after boot needs attention");
                }
                tracing::info!(session = %stopped.id, running = %resumed.session, "resumed now that this computer started");
                pass.resumed.push(resumed);
            }
            // Not this session's failure: the next daemon of this boot
            // tries again.
            Err(_) if open_for_work(&host).is_err() => {
                pass.interrupted = true;
                break;
            }
            // A window started it first; the daemon refused a second writer.
            Err(reason) if conversation_runs(&host, &source) => {
                tracing::info!(session = %stopped.id, %reason, "not resumed: a window reopened it");
            }
            Err(reason) => {
                tracing::warn!(session = %stopped.id, %reason, "could not resume now that this computer started");
                announce_failure(&host, &source, &stopped, &reason);
                pass.failed.push((stopped, reason));
            }
        }
    }
    if !pass.interrupted
        && let Some(host) = host.upgrade()
    {
        announce_left(&host, &pass.left);
    }
    pass
}

fn finish_boot_codex_pair(
    host: &SharedHost,
    source: &Session,
    resumed: &Resumed,
) -> Result<Resumed, String> {
    open_for_work(host)?;
    let registry = registry(host)?;
    let started = hosted_provider(host, resumed.session)
        .filter(|started| is_exact_paired_successor(source, started))
        .ok_or("The resumed provider changed before its terminal could be reopened")?;
    let finished = finish_codex_pair(host, &registry, source, started)?;
    let agent = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_for_session(finished.session.id)?
        .filter(|agent| !agent.closed)
        .map(|agent| agent.id);
    Ok(Resumed {
        starting: finished.starting == Some(true),
        note: finished.note,
        agent,
        ..resumed.clone()
    })
}

/// Whether `session`'s complete saved record says this boot ran it. One
/// whose record is incomplete may be an earlier boot's.
fn ran_this_boot(host: &SharedHost, session: &Session) -> bool {
    session
        .proof
        .as_ref()
        .is_some_and(|proof| proof.is_complete())
        && !super::super::chat::from_earlier_boot(host, session)
}

/// Whether a window holds the provider attachment, or a start is pending.
fn window_holds(host: &SharedHost, id: TerminalId) -> bool {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.chat_spawning.contains_key(&id)
        || state.chats.get(&id).is_some_and(|record| {
            !record.handle.is_read_only_history()
                && record
                    .subscribers
                    .keys()
                    .any(|client| *client != EXECUTION_CLIENT)
        })
}

/// Whether some process of this daemon runs, or is starting, `source`'s
/// conversation.
fn conversation_runs(host: &SharedHost, source: &Session) -> bool {
    let Some(conversation) = source.launch.session_id.as_deref() else {
        return false;
    };
    let provider = source.launch.provider;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.chats.values().any(|record| {
        record.session.launch.provider == provider
            && record.handle.session_id().as_deref() == Some(conversation)
            && (record.handle.owns_work() || record.provider_running())
    }) || state.chat_spawning.values().any(|pending| {
        pending.launch.provider == provider
            && pending.launch.session_id.as_deref() == Some(conversation)
    })
}

/// A notice that `session` did not resume when this computer started, once
/// for boot `boot`. Desktop alerts follow the person's failure notification
/// setting; the title and text show where they allow previews.
fn notice(
    boot: &BootId,
    session: &Session,
    key: &str,
    conversation: &str,
    preview: &str,
) -> crate::notifications::Event {
    use crate::notifications::{self as notice, presentation};
    let mut event = notice::Event::session(
        session.id,
        session.launch.provider,
        session.launch.recipe.working_directory.clone(),
        notice::Evidence {
            key: format!("resume-after-boot:{boot}{key}"),
            kind: notice::Kind::NotResumed,
        },
    );
    event.content = Some(notice::Content {
        conversation: presentation::title(conversation),
        preview: presentation::plain_preview(preview),
        request: None,
    });
    event
}

fn announce(host: &SharedHost, event: impl FnOnce(&BootId) -> Option<crate::notifications::Event>) {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(notifications) = &state.notifications
        && let Some(event) = event(&state.proof().boot_id)
    {
        notifications.record(event);
    }
}

/// Says that `stopped` did not come back, why, and how to resume it. A long
/// reason is cut, never the command after it.
fn failure_notice(
    boot: &BootId,
    source: &Session,
    stopped: &Stopped,
    reason: &str,
) -> crate::notifications::Event {
    let short = crate::agent_execution::mailbox::short(stopped.id);
    let title = stopped
        .title
        .clone()
        .unwrap_or_else(|| stopped.provider.chat_title().to_owned());
    let reason = reason.trim();
    let reason = if reason.is_empty() || reason.ends_with(['.', '!', '?', '…']) {
        reason.to_owned()
    } else {
        format!("{reason}.")
    };
    notice(
        boot,
        source,
        "",
        &format!("{title} ({short})"),
        &crate::notifications::presentation::plain_preview_ending_with(
            &reason,
            &format!("Then run `cagents resume {short}`."),
        ),
    )
}

fn announce_failure(host: &SharedHost, source: &Session, stopped: &Stopped, reason: &str) {
    announce(host, |boot| {
        Some(failure_notice(boot, source, stopped, reason))
    });
}

/// One notice for every stopped session the resume left without trying,
/// about the first of them, or none when it tried them all.
fn left_notice(boot: &BootId, left: &[Session]) -> Option<crate::notifications::Event> {
    let first = left.first()?;
    let preview = match left.len() {
        1 => "1 stopped session was not resumed. Resume it from the app; `cagents resume` shows what is still stopped.".to_owned(),
        count => format!(
            "{count} stopped sessions were not resumed. Resume them from the app; `cagents resume` shows what is still stopped."
        ),
    };
    Some(notice(boot, first, ":left", "Stopped sessions", &preview))
}

fn announce_left(host: &SharedHost, left: &[Session]) {
    announce(host, |boot| left_notice(boot, left));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof};

    fn conversation(conversation: Option<&str>) -> Session {
        Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: crate::agent::Provider::Antigravity,
                recipe: crate::model::LaunchRecipe {
                    program: "agy".into(),
                    arguments: Vec::new(),
                    working_directory: "/project".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: conversation.map(str::to_owned),
                draft_model: None,
            },
            proof: Some(HostSessionProof {
                host_session_id: HostSessionId::new("earlier"),
                boot_id: BootId::new("earlier"),
                host_incarnation: HostIncarnation::new("earlier"),
            }),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        }
    }

    #[test]
    fn only_a_conversation_a_start_can_name_is_resumable() {
        let session = conversation;
        assert!(resumable(&session(Some("conversation"))));
        // Nothing names the conversation to reopen.
        assert!(!resumable(&session(None)));
        // A retained owner knows it.
        let mut retained = session(None);
        retained.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        assert!(resumable(&retained));
    }

    fn saved(title: Option<&str>) -> (Session, Stopped) {
        let id = TerminalId::new();
        let session = Session {
            id,
            launch: chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: "/project".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("conversation".into()),
                draft_model: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let stopped = Stopped {
            id,
            provider: crate::agent::Provider::Claude,
            title: title.map(str::to_owned),
            agent: None,
            failure: None,
        };
        (session, stopped)
    }

    /// A session that did not resume is announced once for the boot, by its
    /// title and short ID, with the command that resumes it, and only as the
    /// person's failure notifications and previews allow.
    #[test]
    fn a_session_that_did_not_resume_is_announced_with_how_to_resume_it() {
        use crate::notifications::{Kind, Preferences, presentation};
        let boot = BootId::new("boot");
        let (source, stopped) = saved(Some("Fix the parser"));
        let short = crate::agent_execution::mailbox::short(stopped.id);
        let event = failure_notice(&boot, &source, &stopped, "Claude sign-in needs attention");
        event.validate().unwrap();
        assert_eq!(event.evidence.kind, Kind::NotResumed);
        assert_eq!(event.evidence.key, "resume-after-boot:boot");
        assert_eq!(event.source_session(), Some(stopped.id));
        let content = event.content.as_ref().unwrap();
        assert_eq!(content.conversation, format!("Fix the parser ({short})"));
        assert_eq!(
            content.preview,
            format!("Claude sign-in needs attention. Then run `cagents resume {short}`.")
        );
        let shown = presentation::message(&event, true);
        assert_eq!(shown.title, format!("Fix the parser ({short})"));
        assert!(
            shown.body.starts_with(&format!(
                "Did not resume when this computer started: Claude sign-in needs attention. Then run `cagents resume {short}`."
            )),
            "{}",
            shown.body
        );
        // A long reason is cut, never the command that resumes it.
        let long = "The provider said no. ".repeat(40);
        let event = failure_notice(&boot, &source, &stopped, &long);
        let preview = &event.content.as_ref().unwrap().preview;
        assert!(
            preview.ends_with(&format!("… Then run `cagents resume {short}`.")),
            "{preview}"
        );
        assert!(
            presentation::message(&event, true)
                .body
                .contains(&format!("… Then run `cagents resume {short}`.")),
        );
        // Without previews, only what happened.
        let private = presentation::message(&event, false);
        assert!(!private.title.contains("Fix the parser"));
        assert!(
            private
                .body
                .starts_with("Did not resume when this computer started"),
            "{}",
            private.body
        );
        let mut preferences = Preferences::default();
        assert!(preferences.allows(Kind::NotResumed, false));
        preferences.failure = false;
        assert!(!preferences.allows(Kind::NotResumed, false));

        // A reason with nothing to add, or already cut, gains no period.
        for (reason, preview) in [("", ""), ("Cut short…", "Cut short… ")] {
            let event = failure_notice(&boot, &source, &stopped, reason);
            assert_eq!(
                event.content.unwrap().preview,
                format!("{preview}Then run `cagents resume {short}`.")
            );
        }

        // An untitled one is named by its provider.
        let (source, stopped) = saved(None);
        let event = failure_notice(&boot, &source, &stopped, "gone");
        assert!(
            event
                .content
                .unwrap()
                .conversation
                .starts_with(crate::agent::Provider::Claude.chat_title())
        );
    }

    /// The sessions the resume did not try share one notice.
    #[test]
    fn the_sessions_left_untried_share_one_notice() {
        let boot = BootId::new("boot");
        assert!(left_notice(&boot, &[]).is_none());
        let (one, _) = saved(None);
        let (two, _) = saved(None);
        let event = left_notice(&boot, std::slice::from_ref(&one)).unwrap();
        event.validate().unwrap();
        assert_eq!(
            event.content.unwrap().preview,
            "1 stopped session was not resumed. Resume it from the app; `cagents resume` shows what is still stopped."
        );
        let event = left_notice(&boot, &[one.clone(), two]).unwrap();
        assert_eq!(event.evidence.key, "resume-after-boot:boot:left");
        assert_eq!(event.source_session(), Some(one.id));
        assert_eq!(
            event.content.unwrap().preview,
            "2 stopped sessions were not resumed. Resume them from the app; `cagents resume` shows what is still stopped."
        );
    }

    /// The resumed boot is replaced whole, privately, and nothing else is
    /// left beside it.
    #[test]
    fn the_resumed_boot_is_recorded_privately_and_replaced_whole() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data");
        assert_eq!(handled_boot(&path), Ok(None));
        record_handled_boot(&path, &BootId::new("first")).unwrap();
        assert_eq!(handled_boot(&path), Ok(Some(BootId::new("first"))));
        record_handled_boot(&path, &BootId::new("second")).unwrap();
        assert_eq!(handled_boot(&path), Ok(Some(BootId::new("second"))));
        let file = path.join(HANDLED_BOOT);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(&path).unwrap().count(),
            1,
            "no temporary file is left"
        );
    }

    #[test]
    fn a_listing_offers_each_lineage_once_and_counts_what_only_the_app_reopens() {
        let stopped = |id: TerminalId| Stopped {
            id,
            provider: crate::agent::Provider::Codex,
            title: None,
            agent: None,
            failure: Some("Codex sign-in needs attention".into()),
        };
        let [
            newest,
            older,
            other,
            paired,
            foreign,
            ahead,
            stale,
            not_theirs,
            theirs,
            unreadable,
            behind,
        ] = std::array::from_fn(|_| TerminalId::new());
        #[derive(Clone, Copy)]
        enum Is {
            Resumable,
            InAppOnly,
            NotTheirs,
            Ends,
        }
        // (session, lineage, what it is), each lineage's latest first.
        let members = [
            (newest, 1, Is::Resumable),
            (other, 2, Is::Resumable),
            (older, 1, Is::Resumable),
            (paired, 3, Is::InAppOnly),
            (foreign, 4, Is::NotTheirs),
            // Only the app reopens the conversation this lineage last
            // continued, so it offers none of its older ones.
            (ahead, 5, Is::InAppOnly),
            (not_theirs, 6, Is::NotTheirs),
            (stale, 5, Is::Resumable),
            // One the caller may not resume leaves the lineage's next.
            (theirs, 6, Is::Resumable),
            // One that runs after all, or whose descriptor cannot be read,
            // leaves nothing of its lineage to offer.
            (unreadable, 7, Is::Ends),
            (behind, 7, Is::Resumable),
        ];
        let found = one_per_lineage(
            members,
            |(_, lineage, _)| *lineage,
            |(id, _, is)| match is {
                Is::Resumable => Candidate::Resumable(stopped(*id)),
                Is::InAppOnly => Candidate::InAppOnly,
                Is::NotTheirs => Candidate::NotTheirs,
                Is::Ends => Candidate::Ends,
            },
        );
        assert_eq!(
            found.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            [newest, other, theirs],
            "each lineage's latest conversation, once"
        );
        assert_eq!(found.in_app_only, 2);
        assert_eq!(
            found.sessions[0].failure.as_deref(),
            Some("Codex sign-in needs attention")
        );
    }

    #[test]
    fn a_descriptor_that_cannot_be_read_or_still_runs_offers_nothing() {
        let id = TerminalId::new();
        let stopped = || Stopped {
            id,
            provider: crate::agent::Provider::Claude,
            title: None,
            agent: None,
            failure: None,
        };
        assert!(matches!(
            candidate(id, Err("no saved conversation".into()), stopped),
            Candidate::Ends
        ));
        assert!(matches!(candidate(id, Ok(None), stopped), Candidate::Ends));
    }

    #[test]
    fn a_preparation_that_returns_the_conversation_itself_starts_it() {
        let mut asked = conversation(Some("conversation"));
        asked.native_continuity = chat::NativeContinuity::ReconnectOnly;
        // A live owner keeps it: the start restores and reattaches it.
        assert_eq!(start_prepared(&asked, asked.clone()), Ok(asked.clone()));
        // A successor under a new allocation.
        let mut successor = asked.clone();
        successor.id = TerminalId::new();
        successor.proof = None;
        assert_eq!(
            start_prepared(&asked, successor.clone()),
            Ok(successor.clone())
        );
        // Anything else is not this conversation.
        let mut other = successor;
        other.launch.session_id = Some("another conversation".into());
        assert!(start_prepared(&asked, other).is_err());
    }
}
