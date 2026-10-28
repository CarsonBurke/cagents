//! Resident tray observations never read transcripts or hold Host across actor
//! sampling, window sockets or child launch. A menu ID is a navigation hint;
//! activation must resolve its current, unambiguous resident owner again.
use std::{collections::BTreeMap, path::Path, sync::Arc};

use super::state::{HostState, SharedHost};
use crate::{
    agent::{ActivityState, Provider},
    chat::runtime::{ActivityObservation, Handle},
    model::{TerminalId, TerminalSession},
    notifications::{Application, Destination},
    terminal::TerminalHandle,
    tray::{AgentTally, TraySnapshot, WaitingAgent},
};

pub(super) fn start(host: &SharedHost, application: Application) -> crate::tray::TrayGuard {
    // The inherited capability must never become the user's cross-session
    // navigation authority. Preserve the existing tally-only path separately.
    if !attention_enabled(
        application,
        &crate::agent_execution::Authority::from_environment(),
    ) {
        let weak = Arc::downgrade(host);
        return crate::tray::start(move || {
            let host = weak.upgrade()?;
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            Some(state.agent_tally())
        });
    }
    let observe = Arc::downgrade(host);
    let activate = Arc::downgrade(host);
    crate::tray::start_with_attention(
        move || Some(snapshot(&observe.upgrade()?)),
        move |id| {
            if !user_scope() {
                return Err("Session-scoped processes cannot navigate other agents".into());
            }
            focus(&activate.upgrade().ok_or("The daemon has stopped")?, id)
        },
    )
}

fn user_scope() -> bool {
    crate::agent_execution::Authority::from_environment()
        .token()
        .is_none()
}

fn attention_enabled(
    application: Application,
    authority: &crate::agent_execution::Authority,
) -> bool {
    application == Application::Cagents && authority.token().is_none()
}

struct TerminalSample {
    handle: TerminalHandle,
    title: Option<String>,
    basename: String,
}

struct ChatSample {
    handle: Handle,
    provider: Provider,
    basename: String,
}

#[derive(Default)]
struct Candidate {
    terminal: Option<TerminalSample>,
    chat: Option<ChatSample>,
    paired: bool,
    ambiguous: bool,
}

fn basename(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .chars()
        .take(100)
        .collect()
}

fn paired(state: &HostState, id: TerminalId) -> bool {
    let (Some(terminal), Some(chat)) = (state.terminals.get(&id), state.chats.get(&id)) else {
        return false;
    };
    chat.paired_terminal
        .as_ref()
        .is_some_and(|handle| handle.same_worker(&terminal.handle))
        && chat.session.terminal.as_ref().is_some_and(|source| {
            super::chat::matches_paired_terminal(&terminal.session, source, state.proof())
        })
}

fn capture(state: &HostState) -> BTreeMap<TerminalId, Candidate> {
    let mut candidates: BTreeMap<TerminalId, Candidate> = BTreeMap::new();
    for (&id, record) in &state.terminals {
        if record.is_running() {
            candidates.entry(id).or_default().terminal = Some(TerminalSample {
                handle: record.handle.clone(),
                title: record
                    .conversation_title()
                    .map(|title| title.chars().take(100).collect()),
                basename: basename(record.current_directory()),
            });
        }
    }
    for (&id, record) in &state.chats {
        let candidate = candidates.entry(id).or_default();
        candidate.chat = Some(ChatSample {
            handle: record.handle.clone(),
            provider: record.session.launch.provider,
            basename: basename(&record.session.launch.recipe.working_directory),
        });
        candidate.paired = paired(state, id);
        candidate.ambiguous = state.terminals.contains_key(&id) && !candidate.paired;
    }
    candidates
}

fn native_activity(observation: &Option<ActivityObservation>) -> Option<ActivityState> {
    observation
        .as_ref()
        .filter(|sample| sample.ready && sample.activity != ActivityState::Unknown)
        .map(|sample| sample.activity)
}

impl Candidate {
    /// An actual pair has one semantic owner. A busy/unready native sample
    /// preserves terminal evidence; a ready Idle sample clears old Waiting.
    fn observe(&self, id: TerminalId, snapshot: &mut TraySnapshot) {
        let terminal = self.terminal.as_ref().and_then(|sample| {
            (!sample.handle.is_completed())
                .then(|| sample.handle.agent_activity())
                .flatten()
        });
        let native = self
            .chat
            .as_ref()
            .and_then(|sample| sample.handle.attention_activity());
        self.observe_sampled(id, terminal, native, snapshot);
    }

    fn observe_sampled(
        &self,
        id: TerminalId,
        terminal: Option<crate::agent::Activity>,
        native: Option<ActivityObservation>,
        snapshot: &mut TraySnapshot,
    ) {
        let activity = native_activity(&native);
        let ambiguous = self.ambiguous;
        let mut record = |state, provider, title: Option<&str>, directory: &str| match state {
            ActivityState::Working => snapshot.tally.working += 1,
            ActivityState::Waiting => {
                snapshot.tally.waiting += 1;
                if !ambiguous && snapshot.waiting_agents.len() < crate::tray::MAX_WAITING_AGENTS {
                    snapshot.waiting_agents.push(WaitingAgent::new(
                        id,
                        provider,
                        title,
                        Path::new(directory),
                    ));
                }
            }
            ActivityState::Idle | ActivityState::Unknown => {}
        };
        if let Some(chat) = &self.chat {
            if let Some(activity) = activity {
                record(
                    activity,
                    chat.provider,
                    native.as_ref().and_then(|s| s.title.as_deref()),
                    &chat.basename,
                );
            }
        }
        if (!self.paired || activity.is_none())
            && let (Some(sample), Some(activity)) = (&self.terminal, terminal)
        {
            record(
                activity.state,
                activity.provider,
                sample.title.as_deref(),
                &sample.basename,
            );
        }
    }
}

fn snapshot(host: &SharedHost) -> TraySnapshot {
    snapshot_with(host, |id, candidate, snapshot| {
        candidate.observe(id, snapshot)
    })
}

fn snapshot_with(
    host: &SharedHost,
    mut observe: impl FnMut(TerminalId, &Candidate, &mut TraySnapshot),
) -> TraySnapshot {
    let candidates = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        capture(&state)
    };
    let mut snapshot = TraySnapshot {
        tally: AgentTally::default(),
        waiting_agents: Vec::new(),
    };
    for (id, candidate) in candidates {
        observe(id, &candidate, &mut snapshot);
    }
    snapshot
}

struct Source {
    terminal: Option<(TerminalSession, TerminalHandle)>,
    chat: Option<(crate::chat::Session, Handle)>,
    paired: bool,
}

impl Source {
    fn capture(state: &HostState, id: TerminalId) -> Result<Self, String> {
        let terminal = state
            .terminals
            .get(&id)
            .filter(|record| record.is_running())
            .map(|record| (record.session.clone(), record.handle.clone()));
        let chat = state
            .chats
            .get(&id)
            .map(|record| (record.session.clone(), record.handle.clone()));
        let paired = paired(state, id);
        if terminal.is_none() && chat.is_none()
            || state.terminals.contains_key(&id) && chat.is_some() && !paired
        {
            return Err("This waiting agent is no longer unambiguously available".into());
        }
        Ok(Self {
            terminal,
            chat,
            paired,
        })
    }

    fn waiting(&self) -> bool {
        let native = self
            .chat
            .as_ref()
            .and_then(|(_, handle)| handle.attention_activity());
        if let Some(activity) = native_activity(&native) {
            if self.paired || self.terminal.is_none() {
                return activity == ActivityState::Waiting;
            }
        }
        self.terminal.as_ref().is_some_and(|(_, handle)| {
            !handle.is_completed()
                && handle
                    .agent_activity()
                    .is_some_and(|a| a.state == ActivityState::Waiting)
        })
    }

    fn matches(&self, state: &HostState, id: TerminalId) -> bool {
        let Ok(current) = Self::capture(state, id) else {
            return false;
        };
        let terminal = match (&self.terminal, &current.terminal) {
            (Some((source, handle)), Some((now, worker))) => {
                source == now && handle.same_worker(worker)
            }
            (None, None) => true,
            _ => false,
        };
        let chat = match (&self.chat, &current.chat) {
            (Some((source, handle)), Some((now, actor))) => {
                source == now && handle.same_actor(actor) && handle.same_attachment(actor)
            }
            (None, None) => true,
            _ => false,
        };
        terminal && chat && self.paired == current.paired
    }
}

fn focus(host: &SharedHost, id: TerminalId) -> Result<String, String> {
    let source = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        Source::capture(&state, id)?
    };
    if !source.waiting() {
        return Err("This agent no longer needs attention".into());
    }
    let destination = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !source.matches(&state, id) {
            return Err("The waiting agent changed; reopen the tray menu".into());
        }
        let destination = state.tray_destination(id);
        state.queue_tray_session(destination.clone(), id)?;
        destination
    };
    navigate(host, &destination, id)
}

fn navigate(
    host: &SharedHost,
    destination: &Destination,
    id: TerminalId,
) -> Result<String, String> {
    navigate_with(
        host,
        destination,
        id,
        || {
            crate::instance::activate_window_session(&destination.window, id, None)
                .map_err(|error| format!("Window navigation was not confirmed: {error}"))
        },
        || {
            crate::instance::launch_notification_destination(destination, None).map_err(|error| {
                format!("The navigation request is queued, but the window could not open: {error}")
            })
        },
    )
}

fn navigate_with(
    host: &SharedHost,
    destination: &Destination,
    id: TerminalId,
    mut activate: impl FnMut() -> Result<bool, String>,
    launch: impl FnOnce() -> Result<(), String>,
) -> Result<String, String> {
    if !activate()? {
        launch()?;
        if !activate()? {
            return Err(
                "The navigation request is queued; window navigation is not yet confirmed".into(),
            );
        }
    }
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .acknowledge_tray_session(destination, id);
    Ok("Requested waiting agent".into())
}

#[cfg(test)]
mod tests;
