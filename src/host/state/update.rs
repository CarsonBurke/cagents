use super::*;
use crate::update::impact::{DaemonImpact, Disposition, Policy, SessionImpact, SessionKind};
use sha2::{Digest, Sha256};

impl HostState {
    pub(crate) fn upgrade_impact(&mut self, policy: Policy) -> Result<DaemonImpact, ProtocolError> {
        let capability = self
            .shutdown_preflight(Instant::now())
            .map_err(|outcome| upgrade_error(format!("Cannot inspect update impact: {outcome:?}")))?
            .capability;
        let mut impact = DaemonImpact {
            capability,
            policy,
            sessions: Vec::new(),
            blockers: Vec::new(),
        };
        if !self.spawning.is_empty() || !self.chat_spawning.is_empty() {
            impact
                .blockers
                .push("Wait for starting sessions to finish opening.".into());
        }
        if self
            .chat_handoffs
            .values()
            .any(|receipt| !receipt.unacknowledgeable(Instant::now()))
        {
            impact
                .blockers
                .push("Finish saving the pending return to terminal.".into());
        }
        for (&id, record) in &self.terminals {
            if record.handle.is_completed() || self.chats.contains_key(&id) {
                continue;
            }
            let source = self.descriptor(0, id)?.session;
            let (disposition, reason) = if record.execution_environment.is_none() {
                (
                    Disposition::Continues,
                    "The terminal and its running processes stay alive.",
                )
            } else {
                (
                    Disposition::Blocked,
                    "This terminal has a private execution environment; finish it or return to its native conversation before updating.",
                )
            };
            impact.sessions.push(SessionImpact {
                id,
                kind: SessionKind::Terminal,
                title: std::path::Path::new(&source.launch.program)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(80)
                    .collect(),
                identity: identity(&(self.inventory_generation, &source))?,
                disposition,
                reason: reason.into(),
            });
        }
        for (&id, record) in &self.chats {
            if !record.handle.owns_work() {
                continue;
            }
            let (busy, ready) = record.handle.update_activity();
            let confirmed = record.handle.confirmed_session_id();
            let mut launch = record.session.launch.clone();
            launch.session_id = confirmed.clone();
            let (disposition, reason) = if record.returning_terminal {
                (
                    Disposition::Blocked,
                    "Finish the pending return to terminal or close this conversation.",
                )
            } else if record.handle.is_completed() {
                (
                    Disposition::Blocked,
                    "Close this conversation to finish its pending provider cleanup, then retry.",
                )
            } else if !ready || launch.terminal_recipe().is_none() {
                (
                    Disposition::Blocked,
                    "Wait for the provider to confirm its resumable conversation, or close it.",
                )
            } else if busy && policy == Policy::WhenIdle {
                (
                    Disposition::WaitsForIdle,
                    "The update will wait for this conversation to become idle, then resume it.",
                )
            } else if busy {
                (
                    Disposition::Interrupted,
                    "The active turn will be interrupted; the saved conversation will resume.",
                )
            } else {
                (
                    Disposition::Resumes,
                    "The saved conversation will reopen with a new provider process.",
                )
            };
            impact.sessions.push(SessionImpact {
                id,
                kind: SessionKind::NativeChat,
                title: record.session.launch.provider.label().into(),
                identity: identity(&(self.inventory_generation, &record.session, confirmed))?,
                disposition,
                reason: reason.into(),
            });
        }
        impact
            .sessions
            .sort_by_key(|session| (session.kind, session.id));
        Ok(impact)
    }
}

fn identity(value: &impl serde::Serialize) -> Result<String, ProtocolError> {
    let bytes = serde_json::to_vec(value).map_err(|error| upgrade_error(error.to_string()))?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
