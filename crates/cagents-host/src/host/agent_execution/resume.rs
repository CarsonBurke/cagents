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
    model::{ChannelId, TerminalId},
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
}

/// The stopped sessions in `scope` that the caller may resume, one for each
/// lineage: every one for the person, and a session's own agents for a
/// session. Stopped chats only their pane can reopen are counted instead.
pub(super) fn stopped(
    host: &SharedHost,
    authority: &Authority,
    scope: Scope,
) -> Result<StoppedSessions, String> {
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
            candidate(
                member.session.id,
                descriptor(host, &registry, member.session.id, agent),
                || Stopped {
                    id: member.session.id,
                    provider,
                    title: member.session.title.clone(),
                    agent: agent.map(|agent| agent.id),
                    failure: member.failure.clone(),
                },
            )
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
        found.sessions.push(Stopped {
            id: session.id,
            provider: session.launch.provider,
            title: agent.name.clone(),
            agent: Some(agent.id),
            failure: None,
        });
    }
    Ok(found)
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
            return Ok(Resumed {
                stopped: member.session.id,
                session: member.session.id,
                provider: member
                    .session
                    .provider
                    .ok_or("That session runs no provider")?,
                title: member.session.title.clone(),
                agent: open_agent(&agents, member.session.id).map(|agent| agent.id),
                already_running: true,
                starting: false,
                note: None,
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
    // An agent's own start resumes it too; one at a time. Held only until
    // the start is made, never while the provider settles.
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
    let started = reopen(host, &source, Basis::Explicit)?;
    drop(starting_gate);
    let starting = settle(host, &started)?;
    let bound = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_for_session(started.id)?
        .filter(|agent| !agent.closed);
    let note = match (&agent, &bound) {
        (Some(agent), None) => Some(format!(
            "Its agent {} did not take the reopened conversation over, so prompts for that agent \
             still cannot reach it; the conversation itself runs",
            agent.id
        )),
        _ => None,
    };
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

/// Reopens `source`'s conversation as its pane would, and moves the pane
/// showing it to the process that now runs it. Returns that process.
pub(super) fn reopen(host: &SharedHost, source: &Session, basis: Basis) -> Result<Session, String> {
    if source.terminal.is_some() {
        return Err(
            "This conversation is paired with its provider's terminal; resume it from its pane in the app, where the terminal reopens with it"
                .into(),
        );
    }
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
    let request = |request| {
        super::super::chat::process(host, EXECUTION_CLIENT, request)
            .map_err(|error: ProtocolError| error.message)
    };
    let (proof, ended_here) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let hosted = state.chats.get(&source.id);
        if hosted.is_some_and(|record| !record.handle.is_completed()) {
            return Err("This conversation is already running".into());
        }
        (state.proof().clone(), hosted.is_some())
    };
    // An update that stopped this conversation names the original it resumes.
    let original = crate::upgrade::floor::load()
        .ok()
        .and_then(|ledger| ledger.resume_source(source).cloned());
    // A successor prepared but never started is prepared again, as its pane
    // does: that is what refuses while an earlier process may still write
    // its conversation.
    let prepare = source.native_continuity.retains_provider()
        || chat::is_unstarted_resume_successor(source)
        || original.is_some();
    let start = if prepare {
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
        if basis == Basis::Prompt
            && !ended_here
            && !super::super::chat::from_earlier_boot(host, source)
        {
            return Err(format!(
                "The daemon restarted since its provider last ran, so it cannot tell whether that process still writes the conversation. Once it has ended, resume it with `cagents resume {}`",
                source.id
            ));
        }
        // The live daemon proved itself on this request; the start carries
        // its proof, as a pane's restore does, and the daemon checks it
        // against any closed record it still holds under this ID.
        let mut session = source.clone();
        session.proof = Some(proof);
        session
    };
    let ServerResponse::ChatAttached {
        session: started, ..
    } = request(ClientRequest::ChatStart {
        session: start,
        source: None,
        terminate_source: false,
        // With no source terminal the daemon starts the provider with its
        // own native environment, as for a new agent.
        execution_environment: Default::default(),
    })?
    else {
        return Err("The daemon answered the start with something else".into());
    };
    let moved = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .adopt_resumed_chat_pane(source, &started);
    match moved {
        Ok(true) => super::announce(host, &registry),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(chat = %started.id, %error, "the reopened conversation's pane still names its stopped process");
        }
    }
    Ok(started)
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
    const HELD: &str =
        "The daemon is not starting new work now, as during an update; try again shortly";
    crate::host::client::ensure_control_open(host).map_err(|_| HELD.to_owned())?;
    let courier = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox
        .clone();
    if courier.is_some_and(|courier| courier.defers_for_update()) {
        return Err(HELD.into());
    }
    Ok(())
}

/// Whether `reopen` can start `session` at all: a conversation with no
/// paired terminal that recorded its provider conversation, or whose retained
/// owner knows it.
fn resumable(session: &Session) -> bool {
    session.terminal.is_none()
        && (session.launch.session_id.is_some() || session.native_continuity.retains_provider())
}

/// Waits a bounded time for the provider `started` runs to be ready, and
/// fails with its reason when it stops instead. Returns whether it is still
/// starting.
fn settle(host: &SharedHost, started: &Session) -> Result<bool, String> {
    let deadline = Instant::now() + SETTLE;
    loop {
        let handle = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .get(&started.id)
            .map(|record| record.handle.clone());
        let Some(handle) = handle else {
            return Err("The reopened conversation stopped as soon as it started".into());
        };
        if handle.is_completed() {
            return Err(match handle.failure() {
                Some(failure) => {
                    chat::failure::summary(started.launch.provider, &failure).into_owned()
                }
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
    if let Some(record) = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&id)
    {
        return Ok(record
            .handle
            .is_completed()
            .then(|| record.current_session()));
    }
    if let Some(session) = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chat_pane_descriptor(id)?
    {
        return Ok(Some(session));
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
