//! Resident tray observations never read transcripts or hold Host across actor
//! sampling, window sockets or child launch. A menu ID is a navigation hint;
//! activation must resolve its current, unambiguous resident owner again.
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use super::state::{HostState, SharedHost};
use crate::{
    agent::{ActivityState, Provider},
    chat::runtime::{ActivityObservation, Handle},
    model::{TerminalId, TerminalSession},
    notifications::{Application, Destination},
    terminal::TerminalHandle,
    tray::{AgentTally, TraySnapshot, WaitingAgent, WaitingSurface},
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
    directory: PathBuf,
}

struct ChatSample {
    handle: Handle,
    provider: Provider,
    directory: PathBuf,
}

#[derive(Default)]
struct Candidate {
    terminal: Option<TerminalSample>,
    chat: Option<ChatSample>,
    paired: bool,
    ambiguous: bool,
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
                    .map(|title| title.chars().take(256).collect()),
                directory: record.current_directory().into(),
            });
        }
    }
    for (&id, record) in &state.chats {
        let candidate = candidates.entry(id).or_default();
        candidate.chat = Some(ChatSample {
            handle: record.handle.clone(),
            provider: record.session.launch.provider,
            directory: record.session.launch.recipe.working_directory.clone(),
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
        let mut record = |state,
                          provider,
                          title: Option<&str>,
                          directory: &std::path::Path,
                          surface| match state {
            ActivityState::Working => snapshot.tally.working += 1,
            ActivityState::Waiting => {
                snapshot.tally.waiting += 1;
                if !ambiguous && snapshot.waiting_agents.len() < crate::tray::MAX_WAITING_AGENTS {
                    snapshot
                        .waiting_agents
                        .push(WaitingAgent::new(id, provider, title, directory, surface));
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
                    &chat.directory,
                    if self.paired {
                        WaitingSurface::ChatAndConsole
                    } else {
                        WaitingSurface::Chat
                    },
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
                &sample.directory,
                if self.paired {
                    WaitingSurface::ChatAndConsole
                } else {
                    WaitingSurface::Console
                },
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
    enrich_labels(host, &mut snapshot);
    snapshot
}

fn enrich_labels(host: &SharedHost, snapshot: &mut TraySnapshot) {
    let (registry, proof, captured) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let Some(registry) = state.execution_registry.clone() else {
            return;
        };
        let captured = snapshot
            .waiting_agents
            .iter()
            .filter_map(|row| {
                let source = Source::capture(&state, row.session).ok()?;
                let presentation = source.presentation_sources(&state, row.session).ok()?;
                Some((row.session, source, presentation))
            })
            .collect::<Vec<_>>();
        (registry, state.proof().clone(), captured)
    };
    let labels = {
        // Release registry transaction/mutex before reacquiring Host below.
        let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let sources = captured
            .iter()
            .map(|(id, _, sources)| (*id, sources.clone()))
            .collect::<Vec<_>>();
        registry.tray_labels(&sources)
    };
    let Ok(labels) = labels else {
        return;
    };
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.proof() != &proof
        || !state
            .execution_registry
            .as_ref()
            .is_some_and(|now| Arc::ptr_eq(now, &registry))
    {
        return;
    }
    for row in &mut snapshot.waiting_agents {
        if let Some(label) = labels.get(&row.session)
            && captured
                .iter()
                .any(|(id, source, _)| *id == row.session && source.matches(&state, *id))
        {
            row.set_named_scope(label.name.as_deref(), &label.scope, &label.directory);
        }
    }
}

#[cfg(test)]
pub(super) fn waiting_snapshot_for_test(host: &SharedHost) -> TraySnapshot {
    snapshot_with(host, |id, candidate, snapshot| {
        candidate.observe_sampled(
            id,
            candidate.terminal.as_ref().map(|_| crate::agent::Activity {
                provider: Provider::Claude,
                state: ActivityState::Waiting,
            }),
            candidate.chat.as_ref().map(|_| ActivityObservation {
                ready: true,
                activity: ActivityState::Waiting,
                title: Some("Review".into()),
            }),
            snapshot,
        )
    })
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

    /// Raw records/handles retain preflight custody. Display/focus uses the
    /// same existing authenticated Live projection issued to ordinary clients.
    fn presentation_sources(
        &self,
        state: &HostState,
        id: TerminalId,
    ) -> Result<Vec<crate::agent_execution::HostedSession>, String> {
        if !self.matches(state, id) {
            return Err("The waiting agent changed; reopen the tray menu".into());
        }
        let mut sources = Vec::new();
        if let Some((session, _)) = &self.chat {
            sources.push(crate::agent_execution::HostedSession::Chat(session.clone()));
        }
        if (self.chat.is_none() || self.paired) && self.terminal.is_some() {
            sources.push(crate::agent_execution::HostedSession::Terminal(
                state.native_terminal_projection(id)?,
            ));
        }
        Ok(sources)
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
    focus_with(host, id, Source::waiting, navigate)
}

fn focus_with(
    host: &SharedHost,
    id: TerminalId,
    waiting: impl FnOnce(&Source) -> bool,
    deliver: impl FnOnce(
        &SharedHost,
        &Destination,
        &crate::instance::TrayActivation,
        &Source,
        &Arc<std::sync::Mutex<super::model::Registry>>,
    ) -> Result<String, String>,
) -> Result<String, String> {
    let (source, presentation, registry, proof) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let source = Source::capture(&state, id)?;
        let presentation = source.presentation_sources(&state, id)?;
        (
            source,
            presentation,
            state
                .execution_registry
                .clone()
                .ok_or("The workspace registry is unavailable")?,
            state.proof().clone(),
        )
    };
    if !waiting(&source) {
        return Err("This agent no longer needs attention".into());
    }
    // Read only the immutable registry identity, outside the global Host lock.
    let identity = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .identity()?;
    let sources = presentation
        .iter()
        .map(|source| {
            crate::instance::tray_source_fingerprint(source).map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (destination, intent) = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if !source.matches(&state, id)
            || state.proof() != &proof
            || !state
                .execution_registry
                .as_ref()
                .is_some_and(|now| Arc::ptr_eq(now, &registry))
        {
            return Err("The waiting agent changed; reopen the tray menu".into());
        }
        let destination = state.tray_destination(id);
        let intent = crate::instance::TrayActivation {
            request: uuid::Uuid::new_v4(),
            registry: identity,
            host: proof,
            window: destination.window.clone(),
            session: id,
            sources,
        };
        state.queue_tray_session(destination.clone(), intent.clone())?;
        (destination, intent)
    };
    deliver(host, &destination, &intent, &source, &registry)
}

fn navigate(
    host: &SharedHost,
    destination: &Destination,
    intent: &crate::instance::TrayActivation,
    source: &Source,
    registry: &Arc<std::sync::Mutex<super::model::Registry>>,
) -> Result<String, String> {
    navigate_with(
        host,
        destination,
        intent,
        || {
            if !source.waiting() {
                return Err("This agent no longer needs attention".into());
            }
            {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if !source.matches(&state, intent.session)
                    || state.proof() != &intent.host
                    || !state
                        .execution_registry
                        .as_ref()
                        .is_some_and(|now| Arc::ptr_eq(now, registry))
                {
                    return Err("The waiting agent changed; reopen the tray menu".into());
                }
            }
            crate::instance::activate_window_tray(intent)
                .map_err(|error| format!("Window navigation was not confirmed: {error}"))
        },
        || {
            crate::instance::launch_notification_destination(destination, None).map_err(|error| {
                format!("The navigation request is queued, but the window could not open: {error}")
            })
        },
        || std::thread::sleep(Duration::from_millis(100)),
        std::time::Instant::now,
    )
}

fn navigate_with(
    host: &SharedHost,
    destination: &Destination,
    intent: &crate::instance::TrayActivation,
    mut activate: impl FnMut() -> Result<bool, String>,
    launch: impl FnOnce() -> Result<(), String>,
    mut wait: impl FnMut(),
    mut now: impl FnMut() -> std::time::Instant,
) -> Result<String, String> {
    let current = || {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .pending_tray_session(destination)
            == Some(intent)
    };
    if !current() {
        return Err("The waiting-agent request was replaced".into());
    }
    if !activate()? {
        launch()?;
        // Startup/recovery already owns a separate bounded handshake. Give a
        // successful slow launch its full socket-delivery budget after it ends.
        let started = now();
        // This is the existing tray-action worker, never the UI or Host lock.
        // A cold window may not have bound its listener at launch completion.
        let mut admitted = false;
        for attempt in 0..150 {
            if !current() {
                return Err("The waiting-agent request was replaced".into());
            }
            if now().saturating_duration_since(started) >= Duration::from_secs(15) {
                break;
            }
            if activate()? {
                admitted = true;
                break;
            }
            if attempt != 149 {
                wait();
            }
        }
        if !admitted {
            return Err("The window did not accept the waiting-agent request. Open the app and try again; the exact request remains queued.".into());
        }
    }
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .acknowledge_tray_session(destination, intent);
    Ok("Requested waiting agent".into())
}

#[cfg(test)]
mod tests;
