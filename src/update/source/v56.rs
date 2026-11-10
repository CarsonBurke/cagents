//! Frozen lifecycle payloads from 0387aa2 (protocol 56). The shared bounded
//! transport authenticates the exact source peer; no current response DTO is read.
use super::{UpdateError, impact, invalid, v77};
use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof};
use crate::terminal_ipc as ipc;
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
struct Proof {
    host_session_id: String,
    boot_id: String,
    host_incarnation: String,
}
impl From<&HostSessionProof> for Proof {
    fn from(proof: &HostSessionProof) -> Self {
        Self {
            host_session_id: proof.host_session_id.as_str().into(),
            boot_id: proof.boot_id.as_str().into(),
            host_incarnation: proof.host_incarnation.as_str().into(),
        }
    }
}
impl From<Proof> for HostSessionProof {
    fn from(proof: Proof) -> Self {
        Self {
            host_session_id: HostSessionId::new(proof.host_session_id),
            boot_id: BootId::new(proof.boot_id),
            host_incarnation: HostIncarnation::new(proof.host_incarnation),
        }
    }
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Policy {
    WhenIdle,
    Interrupt,
}
impl From<impact::Policy> for Policy {
    fn from(policy: impact::Policy) -> Self {
        match policy {
            impact::Policy::WhenIdle => Self::WhenIdle,
            impact::Policy::Interrupt => Self::Interrupt,
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Capability {
    proof: Proof,
    token: String,
    inventory_generation: u64,
    live_terminals: u64,
    expires_at_unix_ms: i64,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Terminal,
    NativeChat,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Disposition {
    Continues,
    Resumes,
    WaitsForIdle,
    Interrupted,
    ResumePicker,
    Blocked,
}
#[derive(Serialize, Deserialize)]
struct Session {
    id: uuid::Uuid,
    kind: Kind,
    title: String,
    identity: String,
    disposition: Disposition,
    reason: String,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Impact {
    capability: Capability,
    policy: Policy,
    sessions: Vec<Session>,
    blockers: Vec<String>,
}

impl From<impact::DaemonImpact> for Impact {
    fn from(value: impact::DaemonImpact) -> Self {
        let c = value.capability;
        Self {
            capability: Capability {
                proof: Proof::from(&c.proof),
                token: c.token,
                inventory_generation: c.inventory_generation,
                live_terminals: c.live_terminals,
                expires_at_unix_ms: c.expires_at_unix_ms,
            },
            policy: value.policy.into(),
            sessions: value
                .sessions
                .into_iter()
                .map(|s| Session {
                    id: s.id.into(),
                    kind: match s.kind {
                        impact::SessionKind::Terminal => Kind::Terminal,
                        impact::SessionKind::NativeChat => Kind::NativeChat,
                    },
                    title: s.title,
                    identity: s.identity,
                    disposition: match s.disposition {
                        impact::Disposition::Continues => Disposition::Continues,
                        impact::Disposition::Resumes => Disposition::Resumes,
                        impact::Disposition::WaitsForIdle => Disposition::WaitsForIdle,
                        impact::Disposition::Interrupted => Disposition::Interrupted,
                        impact::Disposition::ResumePicker => Disposition::ResumePicker,
                        impact::Disposition::Blocked => Disposition::Blocked,
                    },
                    reason: s.reason,
                })
                .collect(),
            blockers: value.blockers,
        }
    }
}
impl From<Impact> for impact::DaemonImpact {
    fn from(value: Impact) -> Self {
        let c = value.capability;
        Self {
            capability: crate::terminal_ipc::ShutdownCapability {
                proof: c.proof.into(),
                token: c.token,
                inventory_generation: c.inventory_generation,
                live_terminals: c.live_terminals,
                background_subagents: 0,
                background_identity: String::new(),
                expires_at_unix_ms: c.expires_at_unix_ms,
            },
            policy: match value.policy {
                Policy::WhenIdle => impact::Policy::WhenIdle,
                Policy::Interrupt => impact::Policy::Interrupt,
            },
            sessions: value
                .sessions
                .into_iter()
                .map(|s| impact::SessionImpact {
                    id: s.id.into(),
                    kind: match s.kind {
                        Kind::Terminal => impact::SessionKind::Terminal,
                        Kind::NativeChat => impact::SessionKind::NativeChat,
                    },
                    title: s.title,
                    identity: s.identity,
                    disposition: match s.disposition {
                        Disposition::Continues => impact::Disposition::Continues,
                        Disposition::Resumes => impact::Disposition::Resumes,
                        Disposition::WaitsForIdle => impact::Disposition::WaitsForIdle,
                        Disposition::Interrupted => impact::Disposition::Interrupted,
                        Disposition::ResumePicker => impact::Disposition::ResumePicker,
                        Disposition::Blocked => impact::Disposition::Blocked,
                    },
                    reason: s.reason,
                    interrupted_subagents: Vec::new(),
                })
                .collect(),
            blockers: value.blockers,
        }
    }
}

pub(crate) struct Client(v77::Client);
impl Client {
    pub(super) fn connect(socket: &Path, deadline: Instant) -> Result<Self, UpdateError> {
        v77::Client::connect_at_protocol(socket, deadline, 56).map(Self)
    }

    pub(super) fn peer_pid(&self) -> u32 {
        self.0.peer_pid()
    }
    pub(super) fn list(&mut self) -> Result<Vec<ipc::TerminalDescriptor>, UpdateError> {
        let bytes = self.0.request(1, (), 1, Duration::from_secs(30))?;
        let terminals: Vec<Terminal> = postcard::from_bytes(&bytes)
            .map_err(|_| invalid("Malformed protocol56 terminal inventory"))?;
        if terminals.len() > 1024 {
            return Err(invalid(
                "Source terminal inventory exceeds its inspection limit",
            ));
        }
        Ok(terminals.into_iter().map(Into::into).collect())
    }
    pub(super) fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, UpdateError> {
        let bytes = self.0.request(29, (), 13, Duration::from_secs(30))?;
        let chats: Vec<crate::chat::Session> = postcard::from_bytes(&bytes)
            .map_err(|_| invalid("Malformed protocol56 chat inventory"))?;
        if chats.len() > 128 {
            return Err(invalid(
                "Source chat inventory exceeds its inspection limit",
            ));
        }
        Ok(chats)
    }
    pub(super) fn shutdown_preflight(&mut self) -> Result<ipc::ShutdownCapability, UpdateError> {
        let bytes = self
            .0
            .request(18, (1u16, 0u32), 8, Duration::from_secs(30))?;
        let capability: Capability = postcard::from_bytes(&bytes)
            .map_err(|_| invalid("Malformed protocol56 shutdown capability"))?;
        let capability: ipc::ShutdownCapability = capability.into();
        if capability.proof != *self.proof() {
            return Err(invalid("The source shutdown capability changed its owner"));
        }
        Ok(capability)
    }
    pub(super) fn confirm_shutdown(
        &mut self,
        capability: ipc::ShutdownCapability,
    ) -> Result<ipc::ShutdownOutcome, UpdateError> {
        validate_capability(&capability)?;
        let bytes = self.0.request(
            18,
            (1u16, 2u32, Capability::from(capability)),
            10,
            Duration::from_secs(60),
        )?;
        postcard::from_bytes(&bytes).map_err(|_| invalid("Malformed protocol56 shutdown receipt"))
    }
    pub(super) fn protocol(&self) -> u16 {
        self.0.protocol()
    }
    pub(super) fn proof(&self) -> &HostSessionProof {
        self.0.proof()
    }
    pub(super) fn upgrade_impact(
        &mut self,
        policy: impact::Policy,
    ) -> Result<impact::DaemonImpact, UpdateError> {
        let bytes = self
            .0
            .request(36, Policy::from(policy), 9, Duration::from_secs(30))?;
        let observed: Impact = postcard::from_bytes(&bytes)
            .map_err(|_| invalid("Malformed protocol56 upgrade impact"))?;
        let observed = impact::DaemonImpact::from(observed);
        if observed.capability.proof != *self.proof()
            || observed.policy != policy
            || observed.sessions.len() > 1152
        {
            return Err(invalid(
                "The protocol56 source impact does not match its authenticated inventory",
            ));
        }
        Ok(observed)
    }
    pub(super) fn prepare_confirmed_upgrade(
        &mut self,
        impact: impact::DaemonImpact,
        relaunch_agents: bool,
    ) -> Result<(u64, u64), UpdateError> {
        // These fields have no representation in 56. Refuse fabricated authority
        // rather than silently dropping it from an outgoing preparation.
        if impact.capability.background_subagents != 0
            || !impact.capability.background_identity.is_empty()
            || impact
                .sessions
                .iter()
                .any(|s| !s.interrupted_subagents.is_empty())
        {
            return Err(invalid(
                "The reviewed capability is not a protocol56 capability",
            ));
        }
        let bytes = self.0.request(
            37,
            (Impact::from(impact), relaunch_agents),
            17,
            Duration::from_secs(60),
        )?;
        let counts: (u64, u64) = postcard::from_bytes(&bytes)
            .map_err(|_| invalid("Malformed protocol56 checkpoint receipt"))?;
        if counts.0 > 1024 || counts.1 > 128 {
            return Err(invalid(
                "The source checkpoint receipt exceeds its declared inventory limits",
            ));
        }
        Ok(counts)
    }
}

#[derive(Deserialize)]
pub(crate) struct Terminal {
    screen_lost_after_crash: bool,
    session: crate::model::TerminalSession,
    generation: u64,
    event_sequence: u64,
    oldest_event_sequence: u64,
    attached: bool,
    transcript: Option<Transcript>,
    activity: Option<crate::model::TerminalOutputActivity>,
    agent_activity: Option<crate::agent::Activity>,
    upgrade_sources: Vec<crate::model::TerminalSession>,
    upgrade_relaunches: Vec<ipc::UpgradeRelaunch>,
}
#[derive(Deserialize)]
struct Transcript {
    path: std::path::PathBuf,
    byte_len: u64,
    durable_offset: u64,
    updated_at_unix_ms: i64,
}
impl From<Terminal> for ipc::TerminalDescriptor {
    fn from(value: Terminal) -> Self {
        ipc::TerminalDescriptor {
            screen_lost_after_crash: value.screen_lost_after_crash,
            session: value.session,
            generation: value.generation,
            event_sequence: value.event_sequence,
            oldest_event_sequence: value.oldest_event_sequence,
            attached: value.attached,
            transcript: value.transcript.map(|t| crate::model::TranscriptMetadata {
                path: t.path,
                byte_len: t.byte_len,
                durable_offset: t.durable_offset,
                updated_at_unix_ms: t.updated_at_unix_ms,
                location: None,
            }),
            activity: value.activity,
            agent_activity: value.agent_activity,
            upgrade_sources: value.upgrade_sources,
            upgrade_relaunches: value.upgrade_relaunches,
            title: None,
        }
    }
}

fn validate_capability(c: &ipc::ShutdownCapability) -> Result<(), UpdateError> {
    if c.background_subagents != 0 || !c.background_identity.is_empty() {
        return Err(invalid(
            "The reviewed capability is not a protocol56 capability",
        ));
    }
    Ok(())
}
impl From<ipc::ShutdownCapability> for Capability {
    fn from(c: ipc::ShutdownCapability) -> Self {
        Self {
            proof: Proof::from(&c.proof),
            token: c.token,
            inventory_generation: c.inventory_generation,
            live_terminals: c.live_terminals,
            expires_at_unix_ms: c.expires_at_unix_ms,
        }
    }
}
impl From<Capability> for ipc::ShutdownCapability {
    fn from(c: Capability) -> Self {
        Self {
            proof: c.proof.into(),
            token: c.token,
            inventory_generation: c.inventory_generation,
            live_terminals: c.live_terminals,
            background_subagents: 0,
            background_identity: String::new(),
            expires_at_unix_ms: c.expires_at_unix_ms,
        }
    }
}

#[cfg(test)]
mod tests;
