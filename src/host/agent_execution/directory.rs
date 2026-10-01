//! Answers directory reads from the live hosted sessions and the durable
//! delegation records. Reading confers no authority over any listed session.

use super::{SharedHost, origin, registry};
use crate::{
    agent::{ActivityState, Provider},
    agent_execution::{
        Authority, Origin,
        directory::{Delegation, Directory, Entry, Owner, Relation, Scope, Surface},
    },
    model::TerminalId,
    project_location::{Placement, place},
};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

/// What the host lock yields; everything slower happens after releasing it.
struct Live {
    id: TerminalId,
    surface: Surface,
    provider: Option<Provider>,
    title: Option<String>,
    state: ActivityState,
    directory: PathBuf,
}

pub(super) fn read(
    host: &SharedHost,
    authority: &Authority,
    scope: Scope,
) -> Result<Directory, String> {
    // Reading confers nothing, so a capability whose session has ended reads
    // as a caller outside cagents rather than failing the whole listing.
    let caller_origin = origin(host, authority).unwrap_or(Origin::User);
    let caller = match caller_origin {
        Origin::Child { parent } => Some(parent),
        Origin::User | Origin::Automation { .. } => None,
    };
    let mut live = live_sessions(host);
    let (model, delegations) = {
        let registry = registry(host)?;
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let model = registry.model()?;
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
                    agent.channel,
                ),
            );
        }
        (model, delegations)
    };
    // A plain shell is listed only where it matters: as the caller, or as the
    // session that owns a listed agent.
    live.retain(|entry| {
        entry.provider.is_some()
            || Some(entry.id) == caller
            || delegations
                .values()
                .any(|(delegation, _)| delegation.owner == Owner::Session(entry.id))
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
    let (scope, wanted) = match scope {
        Scope::All => (Scope::All, None),
        Scope::Containing(directory) => match place_cached(&directory).1 {
            Some(project) => (Scope::Project(project.clone()), Some(project)),
            None => (Scope::All, None),
        },
        Scope::Project(path) => {
            let project = if model.project(&path).is_some() {
                path
            } else {
                place_cached(&path)
                    .1
                    .ok_or_else(|| format!("No open project contains {}", path.display()))?
            };
            (Scope::Project(project.clone()), Some(project))
        }
    };

    let mut me = None;
    let mut sessions = Vec::new();
    for live in live {
        let (worktree, project) = place_cached(&live.directory);
        let (delegation, channel) = delegations.get(&live.id).cloned().unzip();
        let relation = if Some(live.id) == caller {
            Relation::Myself
        } else if Some(live.id) == parent {
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
            provider: live.provider,
            // A delegated agent is named by the channel its prompt titled.
            title: live.title.or_else(|| {
                channel
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
        };
        if relation == Relation::Myself {
            me = Some(entry);
        } else {
            sessions.push(entry);
        }
    }
    sessions.sort_by_cached_key(|entry| {
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
    Ok(Directory {
        caller: me,
        scope,
        sessions,
    })
}

/// Live provider conversations and terminals; the caller decides which
/// plain shells matter.
fn live_sessions(host: &SharedHost) -> Vec<Live> {
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
        });
    }
    live
}
