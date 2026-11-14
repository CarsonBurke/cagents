//! Answers directory reads from the live hosted sessions and the durable
//! delegation records. Reading confers no authority over any listed session.

use super::{SharedHost, caller, registry};
use crate::workspace_model::views::Scope as ViewScope;
use crate::{
    account::{UsageStatus, UsageWindow, info::RemainingWindow},
    agent::{ActivityState, Provider},
    agent_execution::{
        Authority, Lineage, Origin,
        directory::{
            Delegation, Directory, Entry, Member, Members, Owner, Relation, Scope, SessionAccount,
            Sessions, Surface, UsageLimit,
        },
    },
    model::{ChannelId, TerminalId},
    project_location::{Placement, place},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

/// What the host lock yields; everything slower happens after releasing it.
pub(in crate::host) struct Live {
    pub(in crate::host) id: TerminalId,
    pub(super) surface: Surface,
    pub(in crate::host) provider: Option<Provider>,
    pub(in crate::host) title: Option<String>,
    pub(in crate::host) state: ActivityState,
    pub(in crate::host) directory: PathBuf,
    /// A chat whose provider exited; only channel listings include one.
    pub(in crate::host) exited: bool,
    pub(in crate::host) failure: Option<String>,
    /// A chat's runtime, for what a listing reads after releasing the host.
    pub(in crate::host) chat: Option<crate::chat::runtime::Handle>,
}

/// A failure is context in a listing, not a diagnostic dump.
const FAILURE_BYTES: usize = 300;

/// Which sessions a listing returns.
pub(super) enum Wanted {
    Scope(Scope),
    /// Members of this channel, or of the caller's own when absent.
    Channel(Option<ChannelId>),
}

/// Sessions with the state a member view reports.
pub(super) struct Listing {
    pub(super) caller: Option<Member>,
    pub(super) sessions: Vec<Member>,
    /// What was served: a directory widened to `All`, or the caller's channel.
    pub(super) served: Wanted,
    /// The launch of each listed chat that only a saved pane holds, which
    /// locates its provider history.
    pub(super) saved: HashMap<TerminalId, crate::chat::Launch>,
}

impl Listing {
    pub(super) fn members(&self) -> impl Iterator<Item = &Member> {
        self.caller.iter().chain(&self.sessions)
    }
}

pub(super) fn read(
    host: &SharedHost,
    authority: &Authority,
    scope: Scope,
) -> Result<Directory, String> {
    let listing = list(host, authority, Wanted::Scope(scope), false, false)?;
    let Wanted::Scope(scope) = listing.served else {
        unreachable!("a directory read serves a scope");
    };
    Ok(Directory {
        caller: listing.caller.map(|member| member.session),
        scope,
        sessions: listing
            .sessions
            .into_iter()
            .map(|member| member.session)
            .collect(),
    })
}

/// A directory read as members, with each session's usage limit and account.
pub(super) fn sessions(
    host: &SharedHost,
    authority: &Authority,
    scope: Scope,
) -> Result<Sessions, String> {
    let listing = list(host, authority, Wanted::Scope(scope), false, true)?;
    let Wanted::Scope(scope) = listing.served else {
        unreachable!("a directory read serves a scope");
    };
    Ok(Sessions {
        caller: listing.caller,
        scope,
        sessions: listing.sessions,
    })
}

/// The sessions in a channel, including chats whose provider exited.
pub(super) fn channel(
    host: &SharedHost,
    authority: &Authority,
    channel: Option<ChannelId>,
) -> Result<Members, String> {
    let mut listing = list(host, authority, Wanted::Channel(channel), true, true)?;
    let Wanted::Channel(Some(channel)) = listing.served else {
        unreachable!("a channel read serves one channel");
    };
    // How recently each session worked is what a reader picking up a
    // channel's work needs first; it costs a file lookup per session, so only
    // the small channel listing pays for it.
    for member in listing.caller.iter_mut().chain(&mut listing.sessions) {
        let saved = listing.saved.get(&member.session.id);
        member.active_at = super::transcript::source(host, member.session.id, false, saved)
            .ok()
            .and_then(|source| source.modified());
    }
    // Whoever worked most recently is likeliest the one to pick up from.
    listing
        .sessions
        .sort_by_key(|member| std::cmp::Reverse(member.active_at));
    Ok(Members {
        channel,
        caller: listing.caller,
        sessions: listing.sessions,
    })
}

/// The listing behind every directory read. A view of individual members, as
/// of one channel or one session's history, also includes chats whose
/// provider exited, and names each session only by its own title: the
/// channel's title, a fallback elsewhere, would label every member alike.
/// With `standing`, each chat member also reports its usage limit and the
/// account it runs on, which costs a read of the account registry and cache.
pub(super) fn list(
    host: &SharedHost,
    authority: &Authority,
    wanted: Wanted,
    members: bool,
    standing: bool,
) -> Result<Listing, String> {
    // Reading confers nothing, so a capability whose session has ended reads
    // as a caller outside cagents rather than failing the whole listing.
    let (caller_origin, caller) = caller(host, authority).unwrap_or((Origin::User, None));
    let mut live = live_sessions(host);
    let mut saved = HashMap::new();
    if members {
        live.extend(exited_chats(host));
    }
    let (model, delegations, lineages, succession) = {
        let registry = registry(host)?;
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let model = registry.model()?;
        if members {
            // A named channel's listing needs only that channel's panes.
            let only = match &wanted {
                Wanted::Channel(Some(channel)) => Some(ViewScope::Channel(*channel)),
                _ => None,
            };
            for (entry, launch) in saved_chats(&model, only.as_ref(), &live) {
                saved.insert(entry.id, launch);
                live.push(entry);
            }
        }
        let lineages = registry.session_lineages(live.iter().map(|entry| entry.id))?;
        // Only the caller's lineage is followed: it is what an agent the
        // caller starts falls back to.
        let succession = match caller {
            Some(caller) => registry
                .lineage_succession(lineages.get(&caller).copied().unwrap_or(Lineage(caller)))?,
            None => Vec::new(),
        };
        let mut delegations = BTreeMap::new();
        // Only a live session's delegation is listed, so only its latest run
        // is read.
        for agent in &model.agents {
            let Some(session) = &agent.session else {
                continue;
            };
            if !live.iter().any(|entry| entry.id == session.id()) {
                continue;
            }
            let run = registry.latest_agent_run(agent.id)?;
            delegations.insert(
                session.id(),
                (
                    Delegation {
                        agent: agent.id,
                        owner: Owner::from(&agent.origin),
                        run: Some(run.id),
                        run_state: Some(run.state),
                    },
                    agent.placement.clone(),
                ),
            );
        }
        (model, delegations, lineages, succession)
    };
    let lineage = |id: TerminalId| lineages.get(&id).copied().unwrap_or(Lineage(id));
    // A plain shell is listed only where it matters: as the caller, or as the
    // session that owns a listed agent.
    live.retain(|entry| {
        entry.provider.is_some()
            || Some(entry.id) == caller
            || delegations
                .values()
                .any(|(delegation, _)| delegation.owner == Owner::Session(lineage(entry.id)))
    });
    let caller_owner = Owner::from(&caller_origin);
    let parent = caller
        .and_then(|caller| delegations.get(&caller))
        .and_then(|(delegation, _)| match delegation.owner {
            Owner::Session(parent) => Some(parent),
            Owner::User | Owner::Automation(_) => None,
        });

    let mut placements: HashMap<PathBuf, Placement> = HashMap::new();
    let mut place_cached = |directory: &PathBuf| -> (Option<PathBuf>, Option<PathBuf>) {
        let placement = placements
            .entry(directory.clone())
            .or_insert_with(|| place(directory, &model));
        (placement.worktree.clone(), placement.project.clone())
    };
    let (served, wanted) = match wanted {
        Wanted::Channel(Some(channel)) if model.channel(channel).is_none() => {
            return Err(format!(
                "No channel {channel}; list them with `cagents channel list`"
            ));
        }
        // Resolved against the caller's own entry below.
        Wanted::Channel(channel) => (Wanted::Channel(channel), None),
        Wanted::Scope(Scope::All) => (Wanted::Scope(Scope::All), None),
        Wanted::Scope(Scope::Containing(directory)) => match place_cached(&directory).1 {
            Some(project) => (
                Wanted::Scope(Scope::Project(project.clone())),
                Some(project),
            ),
            None => (Wanted::Scope(Scope::All), None),
        },
        Wanted::Scope(Scope::Project(path)) => {
            let project = if model.project(&path).is_some() {
                path
            } else if path.is_relative() {
                named(&model, &path)?
            } else {
                place_cached(&path)
                    .1
                    .ok_or_else(|| format!("No open project contains {}", path.display()))?
            };
            (
                Wanted::Scope(Scope::Project(project.clone())),
                Some(project),
            )
        }
    };

    let scopes = session_scopes(&model);
    let view_scope = |id: TerminalId| {
        scopes
            .get(&id)
            .cloned()
            .unwrap_or_else(|| delegations.get(&id).map(|(_, scope)| scope.clone()))
    };
    let channel_of = |scope: &Option<crate::agent_execution::Placement>| match scope {
        Some(crate::agent_execution::Placement::Channel(id)) => Some(*id),
        _ => None,
    };
    let caller_channel = caller.and_then(|caller| channel_of(&view_scope(caller)));
    let lineage_channel = lineage_channel(&model, &scopes, &succession);
    let now = super::now();
    let accounts = standing
        .then(|| super::account_info::Accounts::load(now))
        .flatten();
    let mut me = None;
    let mut sessions = Vec::new();
    for live in live {
        let (worktree, project) = place_cached(&live.directory);
        let delegation = delegations
            .get(&live.id)
            .map(|(delegation, _)| delegation.clone());
        let view_scope = view_scope(live.id);
        let channel = channel_of(&view_scope);
        let relation = if Some(live.id) == caller {
            Relation::Myself
        } else if Some(lineage(live.id)) == parent {
            Relation::Parent
        } else if delegation
            .as_ref()
            .is_some_and(|delegation| delegation.owner == caller_owner)
        {
            Relation::Child
        } else {
            Relation::Peer
        };
        let related = relation != Relation::Peer;
        if !related && wanted.is_some() && project != wanted {
            continue;
        }
        let entry = Entry {
            id: live.id,
            surface: live.surface,
            channel,
            view_scope,
            channel_repository: channel
                .and_then(|id| model.channel(id))
                .map(|channel| channel.repository.clone()),
            lineage_channel: (Some(live.id) == caller)
                .then(|| lineage_channel.clone())
                .flatten(),
            provider: live.provider,
            // Channel metadata supplies a fallback when the provider has no
            // title, except where it would name the caller's own channel, or
            // every member of one alike.
            title: live.title.or_else(|| {
                channel
                    .filter(|channel| !members && Some(*channel) != caller_channel)
                    .and_then(|channel| model.channel(channel))
                    .map(|channel| channel.title.clone())
            }),
            state: live.state,
            directory: Some(live.directory),
            worktree,
            project_name: project
                .as_ref()
                .and_then(|project| model.project(project))
                .map(|project| project.name.clone()),
            project,
            delegation,
            relation,
            lineage: lineage(live.id),
        };
        let mut account = accounts
            .as_ref()
            .zip(live.chat.as_ref())
            .zip(live.provider)
            .and_then(|((accounts, chat), provider)| {
                accounts.of(provider, &chat.execution_environment().0)
            });
        let limit = live
            .provider
            .filter(|_| standing && !live.exited)
            .and_then(|provider| {
                stopping_limit(
                    provider,
                    live.chat.as_ref().is_some_and(|chat| chat.turn_accepted()),
                    live.failure.as_deref(),
                    live.chat.as_ref().and_then(|chat| chat.finished_at()),
                    || account.clone(),
                    now,
                )
            });
        // Allowance read before the provider refused a turn is no basis for a
        // plan: the refusal is newer evidence.
        if limit.is_some()
            && let Some(account) = account.as_mut()
            && account
                .observed_at
                .zip(live.chat.as_ref().and_then(|chat| chat.finished_at()))
                .is_none_or(|(observed, failed)| observed <= failed)
        {
            account.stale = true;
        }
        let member = Member {
            session: entry,
            exited: live.exited,
            limit,
            account,
            failure: live.failure.map(|failure| {
                let mut failure = match live.provider {
                    Some(provider) => {
                        crate::chat::failure::summary(provider, &failure).into_owned()
                    }
                    None => failure,
                };
                if crate::agent_execution::truncate(&mut failure, FAILURE_BYTES) {
                    failure.push('…');
                }
                failure
            }),
            active_at: None,
        };
        if relation == Relation::Myself {
            me = Some(member);
        } else {
            sessions.push(member);
        }
    }
    let served = match served {
        Wanted::Channel(channel) => {
            let channel = channel
                .or_else(|| me.as_ref().and_then(|me: &Member| me.session.channel))
                .ok_or(
                    "This caller is not in a channel; name one with --channel CHANNEL, \
                     from `cagents channel list`",
                )?;
            sessions.retain(|member| member.session.channel == Some(channel));
            if me
                .as_ref()
                .is_some_and(|me| me.session.channel != Some(channel))
            {
                me = None;
            }
            Wanted::Channel(Some(channel))
        }
        served => served,
    };
    sessions.sort_by_cached_key(|Member { session: entry, .. }| {
        (
            match entry.relation {
                Relation::Parent => 0,
                Relation::Child => 1,
                Relation::Myself | Relation::Peer => 2,
            },
            match entry.state {
                ActivityState::Waiting => 0,
                ActivityState::Working => 1,
                ActivityState::Idle => 2,
                ActivityState::Unknown => 3,
            },
            // Sessions outside every open project come after those in one.
            entry.project_name.is_none(),
            entry.project_name.clone(),
            entry.title.is_none(),
            entry.title.clone(),
        )
    });
    let listed = me
        .iter()
        .chain(&sessions)
        .map(|member| member.session.id)
        .collect::<HashSet<_>>();
    saved.retain(|id, _| listed.contains(id));
    Ok(Listing {
        caller: me,
        sessions,
        served,
        saved,
    })
}

/// The open project called `name`, ignoring case. Names are not unique, so a
/// name two projects share is refused with their paths.
pub(in crate::host) fn named(
    model: &crate::workspace_model::WorkspaceModel,
    name: &Path,
) -> Result<PathBuf, String> {
    let wanted = name.to_string_lossy();
    let matches: Vec<_> = model
        .projects
        .iter()
        .filter(|project| project.name.eq_ignore_ascii_case(&wanted))
        .collect();
    match matches.as_slice() {
        [project] => Ok(project.repository.clone()),
        [] => Err(format!("No open project is named {wanted}")),
        projects => Err(format!(
            "{} open projects are named {wanted}; pass one's path: {}",
            projects.len(),
            projects
                .iter()
                .map(|project| project.repository.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Live provider conversations and terminals; the caller decides which
/// plain shells matter.
pub(in crate::host) fn live_sessions(host: &SharedHost) -> Vec<Live> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let mut live = Vec::new();
    for (id, record) in &state.chats {
        if record.handle.is_completed() {
            continue;
        }
        live.push(Live {
            id: *id,
            surface: Surface::Chat,
            provider: Some(record.session.launch.provider),
            // Resident metadata only; a directory read never loads a transcript.
            title: record.handle.title(),
            state: record.handle.activity(),
            directory: record.session.launch.recipe.working_directory.clone(),
            exited: false,
            failure: record.handle.failure(),
            chat: Some(record.handle.clone()),
        });
    }
    for (id, record) in &state.terminals {
        // A chat's own terminal view and an agent's console present a
        // conversation already listed as a chat.
        if record.handle.is_completed()
            || state.chats.contains_key(id)
            || crate::agent_execution::console_agent(&record.session.launch).is_some()
        {
            continue;
        }
        let activity = record.handle.agent_activity();
        live.push(Live {
            id: *id,
            surface: Surface::Terminal,
            provider: activity.map(|activity| activity.provider),
            title: record.conversation_title().map(str::to_owned),
            state: activity.map_or(ActivityState::Unknown, |activity| activity.state),
            directory: record.current_directory().to_path_buf(),
            exited: false,
            failure: None,
            chat: None,
        });
    }
    live
}

/// Chats whose provider exited but whose pane remains, so their
/// conversation can still be read.
fn exited_chats(host: &SharedHost) -> Vec<Live> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    state
        .chats
        .iter()
        .filter(|(_, record)| record.handle.is_completed())
        .map(|(id, record)| Live {
            id: *id,
            surface: Surface::Chat,
            provider: Some(record.session.launch.provider),
            title: record.handle.title(),
            state: ActivityState::Idle,
            directory: record.session.launch.recipe.working_directory.clone(),
            exited: true,
            failure: record.handle.failure(),
            chat: Some(record.handle.clone()),
        })
        .collect()
}

/// Chats a pane still shows that this daemon is not hosting, such as one
/// saved before a restart that no window has reopened yet, with the launch
/// that locates each one's history. Its provider is not running, but its
/// conversation is still a member of its view and stays readable. With
/// `only`, just that view's panes are read.
fn saved_chats(
    model: &crate::workspace_model::WorkspaceModel,
    only: Option<&ViewScope>,
    hosted: &[Live],
) -> Vec<(Live, crate::chat::Launch)> {
    let hosted = hosted.iter().map(|live| live.id).collect::<HashSet<_>>();
    let mut saved: BTreeMap<TerminalId, (Live, crate::chat::Launch)> = BTreeMap::new();
    let views = model
        .views
        .iter()
        .filter(|view| only.is_none_or(|scope| view.scope == *scope));
    for pane in views.flat_map(|view| &view.panes) {
        let crate::model::CardKind::Chat(session) = &pane.kind else {
            continue;
        };
        if session.is_unstarted() || hosted.contains(&session.id) {
            continue;
        }
        saved.entry(session.id).or_insert_with(|| {
            let live = Live {
                id: session.id,
                surface: Surface::Chat,
                provider: Some(session.launch.provider),
                title: Some(pane.user_title.as_ref().unwrap_or(&pane.title).clone())
                    .filter(|title| !title.is_empty()),
                state: ActivityState::Idle,
                directory: session.launch.recipe.working_directory.clone(),
                exited: true,
                failure: None,
                chat: None,
            };
            (live, session.launch.clone())
        });
    }
    saved.into_values().collect()
}

/// The usage limit that stops a conversation now: the one its provider last
/// reported, dated and lifted by the readings of the account it runs on.
/// `account` is asked for only when the provider did report a limit, since
/// reading accounts touches the registry and the usage cache.
pub(in crate::host) fn chat_limit(
    provider: Provider,
    handle: &crate::chat::runtime::Handle,
    account: impl FnOnce() -> Option<SessionAccount>,
    now_ms: u64,
) -> Option<UsageLimit> {
    stopping_limit(
        provider,
        handle.turn_accepted(),
        handle.failure().as_deref(),
        handle.finished_at(),
        account,
        now_ms,
    )
}

/// The usage limit that stops a conversation whose provider last reported
/// `failure`. Once its own next turn is accepted (`turn_accepted`), no limit
/// stops it, whatever failure is still on record.
/// Background children working on do not count: they keep a refused
/// conversation looking busy without its taking a prompt.
fn stopping_limit(
    provider: Provider,
    turn_accepted: bool,
    failure: Option<&str>,
    failed_at: Option<u64>,
    account: impl FnOnce() -> Option<SessionAccount>,
    now_ms: u64,
) -> Option<UsageLimit> {
    if turn_accepted {
        return None;
    }
    let failure = failure?;
    usage_limit(provider, failure, failed_at, None, now_ms)?;
    usage_limit(provider, failure, failed_at, account().as_ref(), now_ms)
}

/// The usage limit a provider's reported `failure` names, while it holds.
///
/// The provider's message says which limit and, in its own words, when it
/// resets. The account's usage readings supply the reset as a time: the
/// latest reset among its exhausted windows that falls after the failure,
/// since the account is out until all of them have reset, and a reset
/// before the failure cannot be the one that ends it. The limit no longer
/// holds once that reset passes, or once a reading taken after the failure
/// shows the account with allowance again, even though the failure itself
/// stays until the next turn starts. That reading is newer evidence than the
/// failure however old it grows, and outranks the windows' dates, since a
/// provider can permit usage with a window used up. `failed_at` is when the turn that
/// reported it finished, in Unix milliseconds, where known.
pub(in crate::host) fn usage_limit(
    provider: Provider,
    failure: &str,
    failed_at: Option<u64>,
    account: Option<&SessionAccount>,
    now_ms: u64,
) -> Option<UsageLimit> {
    let failure = crate::chat::failure::Failure::parse(provider, failure)?;
    if failure.kind != crate::chat::failure::FailureKind::AccountLimit {
        return None;
    }
    let mut summary = failure.summary;
    if let Some(reset) = &failure.reset {
        let suffix = format!(" · resets {reset}");
        if let Some(stripped) = summary.strip_suffix(&suffix) {
            summary.truncate(stripped.len());
        }
    }
    let after_failure = |at_ms: u64| failed_at.is_none_or(|failed| at_ms > failed);
    // A window used up until after the failure that bears on it. One whose
    // reset has since passed still counts: that reset is what ends the limit.
    // A window scoped to one model, of any span, bears on it only when the
    // provider named that model: one model's allowance running out leaves
    // the others usable.
    let spent_past_failure = |remaining: &&RemainingWindow| {
        let window = &remaining.window;
        remaining.remaining_fraction <= 0.0
            && bears_on(&summary, scope(provider, window))
            && window
                .resets_at
                .is_some_and(|at| at >= 0 && after_failure((at as u64).saturating_mul(1000)))
    };
    let spent = || {
        account
            .into_iter()
            .flat_map(|account| account.remaining.iter())
            .filter(spent_past_failure)
    };
    let resets_at = spent().filter_map(|window| window.window.resets_at).max();
    let model_spent = spent().any(|window| scope(provider, &window.window).is_some());
    let limit = UsageLimit {
        summary,
        reset: failure.reset,
        resets_at,
    };
    if limit.has_reset(now_ms) {
        return None;
    }
    // A reading taken after the failure is newer evidence than it, however
    // old that reading has since grown. It lifts the limit when the provider
    // says the account may be used as usual, or else when it shows allowance
    // and no window spent past the failure. Either way the spent window of a
    // model the failure named still holds it: it leaves the account's status,
    // and Codex's permission, untouched, yet stops a conversation on that model.
    let recovered = failed_at.is_some()
        && !model_spent
        && account.is_some_and(|account| {
            account.observed_at.is_some_and(after_failure)
                && account.ordinary_usage_allowed.unwrap_or(
                    limit.resets_at.is_none()
                        && matches!(account.status, Some(UsageStatus::Ok | UsageStatus::Warning)),
                )
        });
    (!recovered).then_some(limit)
}

/// The model, bucket or surface a provider scoped a window to, read from
/// what it reported rather than from how the label reads.
fn scope(provider: Provider, window: &UsageWindow) -> Option<&str> {
    match provider {
        Provider::Claude => crate::account::claude::window_scope(window),
        Provider::Codex => crate::account::codex::window_scope(window),
        Provider::Omp => None,
    }
}

/// Whether a window bears on a limit: an account-wide one always does, and
/// one scoped to a model only when the limit's summary names it, as "Claude
/// Opus limit reached" names the Opus weekly window.
fn bears_on(summary: &str, scope: Option<&str>) -> bool {
    scope.is_none_or(|scope| summary.to_lowercase().contains(&scope.to_lowercase()))
}

/// The channel the latest session of a lineage belonged to, `succession`
/// listing its sessions newest first. A managed agent's own placement is its
/// membership; any other session belongs where exactly one channel shows it.
/// One shown in two channels says nothing, and an older session decides.
fn lineage_channel(
    model: &crate::workspace_model::WorkspaceModel,
    scopes: &BTreeMap<TerminalId, Option<crate::agent_execution::Placement>>,
    succession: &[TerminalId],
) -> Option<crate::agent_execution::directory::ChannelHome> {
    succession.iter().find_map(|session| {
        let placed = model
            .agents
            .iter()
            .filter(|agent| !agent.closed)
            .find(|agent| {
                agent
                    .session
                    .as_ref()
                    .is_some_and(|hosted| hosted.id() == *session)
            })
            .and_then(|agent| agent.channel());
        let shown = match scopes.get(session) {
            Some(Some(crate::agent_execution::Placement::Channel(channel))) => Some(*channel),
            _ => None,
        };
        let channel = placed.or(shown)?;
        let channel = model.channel(channel)?;
        Some(crate::agent_execution::directory::ChannelHome {
            id: channel.id,
            repository: channel.repository.clone(),
        })
    })
}

/// Only exact live pane membership supplies a view. Multiple distinct views
/// are ambiguous; neither cwd nor window focus chooses one of them.
fn session_scopes(
    model: &crate::workspace_model::WorkspaceModel,
) -> BTreeMap<TerminalId, Option<crate::agent_execution::Placement>> {
    let mut scopes: BTreeMap<TerminalId, Option<crate::agent_execution::Placement>> =
        BTreeMap::new();
    for view in &model.views {
        for pane in &view.panes {
            let id = match &pane.kind {
                crate::model::CardKind::Chat(session) => session.id,
                crate::model::CardKind::Terminal(session) => session.id,
                _ => continue,
            };
            scopes
                .entry(id)
                .and_modify(|known| {
                    if known.as_ref() != Some(&view.scope) {
                        *known = None;
                    }
                })
                .or_insert_with(|| Some(view.scope.clone()));
        }
    }
    scopes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An account as [`super::super::account_info::Accounts::of`] reports
    /// it at `now`: windows, staleness and all, from one usage reading.
    fn account_reading(
        status: crate::account::UsageStatus,
        observed_at: u64,
        windows: &[(&str, crate::account::WindowKind, f32, i64)],
        now: u64,
    ) -> SessionAccount {
        use crate::account::{UsageSnapshot, UsageSource, UsageWindow, info::RemainingWindow};
        let usage = UsageSnapshot {
            windows: windows
                .iter()
                .map(|(label, kind, used, resets_at)| UsageWindow {
                    // Keyed as both providers key a scoped window.
                    key: label
                        .split_once(" · ")
                        .map_or_else(|| (*label).into(), |(_, scope)| format!("scoped:{scope}")),
                    kind: *kind,
                    label: (*label).into(),
                    used: *used,
                    resets_at: Some(*resets_at),
                    span_secs: None,
                    binding: false,
                })
                .collect(),
            credits: None,
            status,
            ordinary_usage_allowed: None,
            observed_at,
            source: UsageSource::Live,
            plan: None,
            detail: None,
        };
        let remaining = RemainingWindow::of(&usage, now);
        SessionAccount {
            id: "claude/system".parse().unwrap(),
            label: None,
            status: Some(status),
            stale: RemainingWindow::stale(now.checked_sub(observed_at), &remaining),
            remaining,
            observed_at: Some(observed_at),
            ordinary_usage_allowed: None,
        }
    }

    #[test]
    fn a_usage_limit_is_dated_by_its_exhausted_window_until_that_window_resets() {
        use crate::account::{UsageStatus, WindowKind};
        let failure = "You've hit your weekly limit · resets Nov 8, 8am (America/Vancouver)";
        // The turn failed at 1,000,000 s; the weekly window resets two hours
        // later, the five-hour one later still, with allowance left.
        let failed_at = 1_000_000_000;
        let weekly_reset = 1_007_200;
        let exhausted = |now| {
            account_reading(
                UsageStatus::Limited,
                failed_at - 60_000,
                &[
                    ("Session", WindowKind::Session, 0.5, 1_009_000),
                    ("Weekly", WindowKind::Weekly, 1.0, weekly_reset),
                ],
                now,
            )
        };
        let limit = |account: Option<&SessionAccount>, now| {
            usage_limit(Provider::Claude, failure, Some(failed_at), account, now)
        };
        let soon = failed_at + 3_600_000;
        let held = limit(Some(&exhausted(soon)), soon).unwrap();
        assert_eq!(held.summary, "Claude weekly limit reached");
        assert_eq!(
            held.reset.as_deref(),
            Some("Nov 8, 8am (America/Vancouver)")
        );
        assert_eq!(
            held.resets_at,
            Some(weekly_reset),
            "the window with allowance left does not date it"
        );

        // The same reading, read once its reset has passed, marks that window
        // elapsed; the reset it names still ends the limit.
        let after_reset = weekly_reset as u64 * 1000 + 1;
        assert_eq!(
            limit(Some(&exhausted(after_reset)), after_reset),
            None,
            "a reset that passed lifts the limit"
        );

        // A reading taken after the reset no longer shows an exhausted window,
        // but it was taken after the failure and shows allowance: lifted.
        let refreshed = account_reading(
            UsageStatus::Ok,
            after_reset,
            &[("Weekly", WindowKind::Weekly, 0.02, weekly_reset + 604_800)],
            after_reset + 1_000,
        );
        assert_eq!(
            limit(Some(&refreshed), after_reset + 1_000),
            None,
            "allowance read after the failure lifts it"
        );
        assert!(
            usage_limit(
                Provider::Claude,
                failure,
                None,
                Some(&refreshed),
                after_reset + 1_000
            )
            .is_some(),
            "a failure of unknown time is not known to predate the reading"
        );

        // Allowance read soon after the failure stays evidence once that
        // reading has grown stale, before any reset: the limit does not
        // return without another refusal.
        let recovered_early = |now| {
            account_reading(
                UsageStatus::Ok,
                failed_at + 60_000,
                &[("Weekly", WindowKind::Weekly, 0.5, weekly_reset)],
                now,
            )
        };
        let stale_later = failed_at + 3_000_000;
        assert!(recovered_early(stale_later).stale);
        assert_eq!(
            limit(Some(&recovered_early(stale_later)), stale_later),
            None,
            "a recovery does not lapse as its reading ages"
        );
        // Codex can permit ordinary usage with a window used up; a reading
        // after the failure that says so outranks that window's date.
        let codex = |status, allowed| {
            usage_limit(
                Provider::Codex,
                "Usage limit reached for this plan",
                Some(failed_at),
                Some(&SessionAccount {
                    ordinary_usage_allowed: allowed,
                    ..account_reading(
                        status,
                        failed_at + 60_000,
                        &[("Weekly", WindowKind::Weekly, 1.0, weekly_reset)],
                        failed_at + 120_000,
                    )
                }),
                failed_at + 120_000,
            )
        };
        assert_eq!(
            codex(UsageStatus::Limited, Some(false)).unwrap().resets_at,
            Some(weekly_reset)
        );
        assert_eq!(
            codex(UsageStatus::Warning, Some(true)),
            None,
            "permission read after the failure lifts a dated limit"
        );
        let codex_model_spent = usage_limit(
            Provider::Codex,
            "Usage limit reached for this plan",
            Some(failed_at),
            Some(&SessionAccount {
                ordinary_usage_allowed: Some(true),
                ..account_reading(
                    UsageStatus::Warning,
                    failed_at + 60_000,
                    &[(
                        "Weekly · GPT-5.1-Codex-Max",
                        WindowKind::Scoped,
                        1.0,
                        weekly_reset,
                    )],
                    failed_at + 120_000,
                )
            }),
            failed_at + 120_000,
        );
        let codex_model_session = usage_limit(
            Provider::Codex,
            "Usage limit reached for this plan",
            Some(failed_at),
            Some(&account_reading(
                UsageStatus::Ok,
                failed_at + 60_000,
                &[
                    ("5h window", WindowKind::Session, 0.3, 1_009_000),
                    (
                        "5h window · GPT-5.1-Codex-Max",
                        WindowKind::Session,
                        1.0,
                        weekly_reset,
                    ),
                ],
                failed_at + 120_000,
            )),
            failed_at + 120_000,
        );
        assert_eq!(
            codex_model_session, None,
            "a model's own short window does not date a limit that did not name it"
        );
        assert_eq!(
            codex_model_spent, None,
            "a model's spent window does not hold a limit that did not name it"
        );

        // Scope is what the provider said a window applies to, not its label:
        // a kind this build does not know is labelled under its group, yet an
        // account-wide one still dates a limit.
        let mut burst = account_reading(
            UsageStatus::Limited,
            failed_at + 60_000,
            &[(
                "Session · Daily burst",
                WindowKind::Session,
                1.0,
                weekly_reset,
            )],
            failed_at + 120_000,
        );
        burst.remaining[0].window.key = "daily_burst".into();
        assert_eq!(
            usage_limit(
                Provider::Claude,
                "You've hit your limit",
                Some(failed_at),
                Some(&burst),
                failed_at + 120_000,
            )
            .expect("an account-wide window holds")
            .resets_at,
            Some(weekly_reset)
        );

        // A model's own window used up leaves the account's status Ok, yet
        // stops a conversation on that model until it resets: a reading after
        // the failure that still shows it used up does not lift the limit.
        let opus = |used| {
            usage_limit(
                Provider::Claude,
                "You've hit your Opus limit · resets Nov 8, 8am (America/Vancouver)",
                Some(failed_at),
                Some(&account_reading(
                    UsageStatus::Ok,
                    failed_at + 60_000,
                    &[
                        ("Session", WindowKind::Session, 0.1, 1_009_000),
                        ("Weekly · Opus", WindowKind::Scoped, used, weekly_reset),
                    ],
                    failed_at + 120_000,
                )),
                failed_at + 120_000,
            )
        };
        assert_eq!(
            opus(1.0).expect("a used-up model window holds").resets_at,
            Some(weekly_reset)
        );
        assert_eq!(opus(0.4), None, "that window with allowance lifts it");

        // Another model's spent window says nothing of a session limit: the
        // session window dates it, and a reading with allowance lifts it.
        let session_reset = 1_003_600;
        let session_limit = |session_used, now| {
            usage_limit(
                Provider::Claude,
                "You've hit your session limit · resets 2pm",
                Some(failed_at),
                Some(&account_reading(
                    UsageStatus::Warning,
                    failed_at + 60_000,
                    &[
                        ("Session", WindowKind::Session, session_used, session_reset),
                        ("Weekly · Opus", WindowKind::Scoped, 1.0, weekly_reset),
                    ],
                    now,
                )),
                now,
            )
        };
        assert_eq!(
            session_limit(1.0, failed_at + 120_000)
                .expect("the spent session window holds it")
                .resets_at,
            Some(session_reset)
        );
        assert_eq!(
            session_limit(1.0, session_reset as u64 * 1000 + 1),
            None,
            "the session reset lifts it, not the Opus one"
        );
        assert_eq!(
            session_limit(0.2, failed_at + 120_000),
            None,
            "allowance read after the failure lifts it despite Opus"
        );

        // Allowance read before the failure says nothing about after it, and
        // a window that reset before the failure is not the one that ends it.
        let earlier = account_reading(
            UsageStatus::Ok,
            failed_at - 600_000,
            &[
                (
                    "Session",
                    WindowKind::Session,
                    1.0,
                    (failed_at / 1000) as i64 - 60,
                ),
                ("Weekly", WindowKind::Weekly, 0.9, weekly_reset),
            ],
            failed_at + 60_000,
        );
        let undated = limit(Some(&earlier), failed_at + 60_000).unwrap();
        assert_eq!(undated.resets_at, None);

        assert_eq!(
            limit(None, u64::MAX).unwrap().resets_at,
            None,
            "only the provider's words date it"
        );
        assert_eq!(
            usage_limit(
                Provider::Claude,
                "API Error: 500 Internal server error",
                Some(failed_at),
                None,
                0
            ),
            None
        );
    }

    #[test]
    fn a_conversation_whose_turn_was_accepted_is_not_stopped_by_its_last_limit() {
        let failure = Some("You've hit your weekly limit · resets Nov 8, 8am (America/Vancouver)");
        let limit = |turn_accepted| {
            stopping_limit(
                Provider::Claude,
                turn_accepted,
                failure,
                Some(1_000_000_000),
                || None,
                1_000_060_000,
            )
        };
        // Refused, though background children may still be working.
        assert!(limit(false).is_some());
        // An accepted turn outranks whatever failure is still on record.
        assert_eq!(limit(true), None, "an accepted turn is not limited");
        assert_eq!(
            stopping_limit(
                Provider::Claude,
                false,
                None,
                None,
                || panic!("no account is read without a reported limit"),
                0,
            ),
            None
        );
    }

    /// The pane a terminal session `id` shows in, which `scopes` reads.
    fn terminal_pane(id: TerminalId) -> crate::workspace_model::views::Pane {
        use crate::model::{CardId, CardKind, LaunchRecipe, TerminalContinuity, TerminalSession};
        let recipe = LaunchRecipe {
            program: "sh".into(),
            arguments: vec![],
            working_directory: "/repo".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        crate::workspace_model::views::Pane {
            id: CardId::new(),
            title: "Shell".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            scope: None,
            kind: CardKind::Terminal(TerminalSession {
                id,
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                launch: recipe.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
            }),
        }
    }

    #[test]
    fn lineage_channel_is_where_the_latest_placed_session_of_the_lineage_belongs() {
        use crate::{
            agent_execution::directory::ChannelHome,
            workspace_model::{
                ModelChannel, WorkspaceModel,
                views::{Scope, View},
            },
        };
        let mut model = WorkspaceModel::default();
        let [first, second, third] = [(); 3].map(|_| ChannelId::new());
        for (ordinal, id) in [first, second, third].into_iter().enumerate() {
            model.channels.push(ModelChannel {
                id,
                repository: "/repo".into(),
                title: format!("Channel {ordinal}"),
                title_source: Default::default(),
                ordinal: ordinal as u32,
            });
        }
        let home = |id| {
            Some(ChannelHome {
                id,
                repository: "/repo".into(),
            })
        };
        let (caller, predecessor) = (TerminalId::new(), TerminalId::new());
        let succession = [caller, predecessor];
        // Nothing of the lineage is in a channel.
        let scopes = session_scopes(&model);
        assert_eq!(lineage_channel(&model, &scopes, &succession), None);

        // The predecessor's pane is still in one channel.
        let mut kept = View::empty(Scope::Channel(first));
        kept.panes.push(terminal_pane(predecessor));
        model.views.push(kept);
        let scopes = session_scopes(&model);
        assert_eq!(lineage_channel(&model, &scopes, &succession), home(first));

        // The caller shown in two channels says nothing; the older session
        // still decides.
        for channel in [second, third] {
            let mut view = View::empty(Scope::Channel(channel));
            view.panes.push(terminal_pane(caller));
            model.views.push(view);
        }
        let scopes = session_scopes(&model);
        assert_eq!(scopes[&caller], None);
        assert_eq!(lineage_channel(&model, &scopes, &succession), home(first));

        // A managed agent's own placement is its membership, however many
        // views show it.
        let mut agent = crate::agent_execution::Agent::fixture_placed_in(third);
        let crate::model::CardKind::Terminal(session) = terminal_pane(caller).kind else {
            unreachable!()
        };
        agent.session = Some(crate::agent_execution::HostedSession::Terminal(session));
        model.agents.push(agent);
        assert_eq!(lineage_channel(&model, &scopes, &succession), home(third));
        // ...until it is closed.
        model.agents[0].closed = true;
        assert_eq!(lineage_channel(&model, &scopes, &succession), home(first));

        // A deleted channel is no home; nothing older is placed.
        model.channels.retain(|channel| channel.id != first);
        assert_eq!(lineage_channel(&model, &scopes, &succession), None);
    }

    #[test]
    fn terminal_membership_reports_worktree_views_and_refuses_ambiguous_placement() {
        use crate::{
            model::{
                CardId, CardKind, ChannelId, LaunchRecipe, TerminalContinuity, TerminalSession,
            },
            workspace_model::{
                WorkspaceModel,
                views::{Pane, Scope, View},
            },
        };
        let id = TerminalId::new();
        let recipe = LaunchRecipe {
            program: "sh".into(),
            arguments: vec![],
            working_directory: "/repo".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let pane = Pane {
            id: CardId::new(),
            title: "Shell".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            scope: None,
            kind: CardKind::Terminal(TerminalSession {
                id,
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                launch: recipe.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe,
                    last_exit: None,
                },
            }),
        };
        let mut model = WorkspaceModel::default();
        let mut strip = View::empty(Scope::Worktree {
            repository: "/repo".into(),
            path: "/repo".into(),
        });
        strip.panes.push(pane.clone());
        model.views.push(strip);
        let worktree = model.views[0].scope.clone();
        assert_eq!(session_scopes(&model)[&id], Some(worktree));
        model.views.clear();
        let channel = ChannelId::new();
        let mut first = View::empty(Scope::Channel(channel));
        first.panes.push(pane.clone());
        model.views.push(first.clone());
        assert_eq!(session_scopes(&model)[&id], Some(channel.into()));
        model.views.push(first);
        assert_eq!(
            session_scopes(&model)[&id],
            Some(channel.into()),
            "duplicate same-channel metadata is unambiguous"
        );
        let mut other = View::empty(Scope::Channel(ChannelId::new()));
        other.panes.push(pane);
        model.views.push(other);
        assert_eq!(
            session_scopes(&model)[&id],
            None,
            "conflicting legacy descriptors choose no channel"
        );
    }
}
