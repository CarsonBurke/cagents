//! Answers directory reads from the live hosted sessions and the durable
//! delegation records. Reading confers no authority over any listed session.

use super::{SharedHost, caller, registry};
use crate::{
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
    collections::{BTreeMap, HashMap},
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
        member.active_at = super::transcript::source(host, member.session.id, false)
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
    if members {
        live.extend(exited_chats(host));
    }
    let (model, delegations, lineages) = {
        let registry = registry(host)?;
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let model = registry.model()?;
        let lineages = registry.session_lineages(live.iter().map(|entry| entry.id))?;
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
        (model, delegations, lineages)
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
        let account = accounts
            .as_ref()
            .zip(live.chat.as_ref())
            .zip(live.provider)
            .and_then(|((accounts, chat), provider)| {
                accounts.of(provider, &chat.execution_environment().0)
            });
        let limit = live
            .failure
            .as_deref()
            .zip(live.provider)
            .filter(|_| standing && !live.exited)
            .and_then(|(failure, provider)| usage_limit(provider, failure, account.as_ref(), now));
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
    Ok(Listing {
        caller: me,
        sessions,
        served,
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

/// The usage limit a provider's reported `failure` names, while it holds.
///
/// The provider's message says which limit and, in its own words, when it
/// resets. The account's latest usage reading supplies the reset as a time:
/// the latest reset among its exhausted windows, since the account is out
/// until all of them have reset. A limit whose known reset has passed no
/// longer holds, though the failure stays until the next turn starts.
pub(in crate::host) fn usage_limit(
    provider: Provider,
    failure: &str,
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
    let resets_at = account.and_then(|account| {
        account
            .remaining
            .iter()
            .filter(|window| window.remaining_fraction <= 0.0 && !window.reset_elapsed)
            .filter_map(|window| window.window.resets_at)
            .max()
    });
    let limit = UsageLimit {
        summary,
        reset: failure.reset,
        resets_at,
    };
    (!limit.has_reset(now_ms)).then_some(limit)
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

    #[test]
    fn a_usage_limit_is_dated_by_its_exhausted_window_until_that_window_resets() {
        use crate::account::{UsageWindow, WindowKind, info::RemainingWindow};
        let failure = "You've hit your weekly limit · resets Nov 8, 8am (America/Vancouver)";
        let window = |key: &str, remaining: f32, resets_at: i64| RemainingWindow {
            window: UsageWindow {
                key: key.into(),
                kind: WindowKind::Weekly,
                label: key.into(),
                used: 1.0 - remaining,
                resets_at: Some(resets_at),
                span_secs: None,
                binding: false,
            },
            remaining_fraction: remaining,
            reset_elapsed: false,
        };
        let account = SessionAccount {
            id: "claude/system".parse().unwrap(),
            label: None,
            status: Some(crate::account::UsageStatus::Limited),
            // The window with allowance left does not date the reset.
            remaining: vec![window("Session", 0.5, 9_000), window("Weekly", 0.0, 7_200)],
            stale: false,
        };
        let limit = usage_limit(Provider::Claude, failure, Some(&account), 3_600_000).unwrap();
        assert_eq!(limit.summary, "Claude weekly limit reached");
        assert_eq!(
            limit.reset.as_deref(),
            Some("Nov 8, 8am (America/Vancouver)")
        );
        assert_eq!(limit.resets_at, Some(7_200));
        assert_eq!(
            usage_limit(Provider::Claude, failure, Some(&account), 7_200_000),
            None,
            "a reset that passed lifts the limit"
        );
        let undated = usage_limit(Provider::Claude, failure, None, u64::MAX).unwrap();
        assert_eq!(undated.resets_at, None, "only the provider's words date it");
        assert_eq!(
            usage_limit(
                Provider::Claude,
                "API Error: 500 Internal server error",
                None,
                0
            ),
            None
        );
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
