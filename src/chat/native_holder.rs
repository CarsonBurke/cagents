//! Durable ownership of a native provider server, independent of the daemon's
//! protocol actor. The holder never submits a prompt or answers an approval.

mod cleanup;
mod transport;
pub(crate) use cleanup::Detached;
pub(crate) use transport::{Holder, adopt, inspect, spawn};

use crate::{
    agent::Provider,
    model::{BootId, HostSessionProof},
};
use serde::{Deserialize, Serialize};

/// Kept only in the holder's memory. Discovery files contain process identity,
/// never the launch recipe or its environment.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Recovery {
    pub source: super::Session,
    pub execution_console: Option<crate::model::LaunchRecipe>,
    pub previous_sources: Vec<super::Session>,
    pub confirmed_thread: Option<String>,
    pub server_version: Option<String>,
    pub server_identity: Option<super::protocol::ServerIdentity>,
    pub server_process: Option<crate::daemon::holder_registry::Process>,
    pub execution_environment: crate::terminal_ipc::ChatExecutionEnvironment,
}

impl Recovery {
    fn validate(&self, boot: &BootId) -> Result<(), String> {
        let valid = |source: &super::Session| {
            source.id == self.source.id
                && source.launch.provider == Provider::Codex
                && source.owns_codex_listener()
                && source
                    .proof
                    .as_ref()
                    .is_some_and(|proof| proof.is_complete() && &proof.boot_id == boot)
        };
        if self.execution_console.as_ref().is_some_and(|recipe| {
            self.source.terminal.as_ref().is_none_or(|terminal| {
                terminal.id != self.source.id
                    || &terminal.launch != recipe
                    || terminal.owned_codex_listener.is_some()
            })
        }) {
            return Err("Native console has inconsistent private pairing evidence".into());
        }
        if !valid(&self.source)
            || self.previous_sources.len() > 1024
            || self.previous_sources.iter().any(|source| !valid(source))
        {
            return Err("Native holder has inconsistent ownership evidence".into());
        }
        if self.confirmed_thread.as_ref().is_some_and(|id| {
            id.is_empty() || id.len() > 1024 || Some(id) != self.source.launch.session_id.as_ref()
        }) || self
            .server_version
            .as_ref()
            .is_some_and(|version| !VERIFIED_CODEX_VERSIONS.contains(&version.as_str()))
        {
            return Err("Native holder has invalid provider confirmation".into());
        }
        let continuity_matches = match (
            &self.confirmed_thread,
            &self.server_version,
            &self.source.native_continuity,
        ) {
            (None, None, super::NativeContinuity::ReconnectOnly) => true,
            (Some(_), Some(expected), super::NativeContinuity::Durable { provider_version }) => {
                expected == provider_version
            }
            _ => false,
        };
        if !continuity_matches {
            return Err("Native continuity capability lacks its provider confirmation".into());
        }
        if self.is_durable() != (self.server_identity.is_some() && self.server_process.is_some()) {
            return Err("Native continuity lacks the exact server and account identity".into());
        }
        Ok(())
    }

    fn is_durable(&self) -> bool {
        self.confirmed_thread.is_some() && self.server_version.is_some()
    }

    fn confirm(
        &mut self,
        thread: String,
        identity: super::protocol::ServerIdentity,
        process: crate::daemon::holder_registry::Process,
    ) -> Result<(), String> {
        let version = verified_codex_version(&identity.user_agent)
            .ok_or("This Codex version has no verified crash-continuity contract")?;
        if !identity.canonical_home.is_absolute()
            || identity.reported_home.is_empty()
            || identity.reported_home.len() > 4096
            || self
                .server_identity
                .as_ref()
                .is_some_and(|old| old != &identity)
            || self
                .server_process
                .as_ref()
                .is_some_and(|old| old != &process)
        {
            return Err("A durable native holder cannot change server or account identity".into());
        }
        if thread.is_empty() || thread.len() > 1024 {
            return Err("Invalid provider-confirmed thread identity".into());
        }
        if self
            .confirmed_thread
            .as_ref()
            .is_some_and(|previous| previous != &thread)
        {
            return Err("A durable native holder cannot change conversations".into());
        }
        let mut source = self.source.clone();
        source.launch.session_id = Some(thread.clone());
        source.native_continuity = super::NativeContinuity::Durable {
            provider_version: version.into(),
        };
        super::protocol::align_resume_identity(&mut source.launch)?;
        if source != self.source {
            self.remember_source()?;
            self.source = source;
        }
        self.confirmed_thread = Some(thread);
        self.server_version = Some(version.into());
        self.server_identity = Some(identity);
        self.server_process = Some(process);
        Ok(())
    }

    fn remember_source(&mut self) -> Result<(), String> {
        if !self.previous_sources.contains(&self.source) {
            // Bound retained recipes by bytes as well as count. A large launch
            // environment must not multiply into gigabytes over proof rotations.
            const MAX_RECOVERY_BYTES: usize = 4 * 1024 * 1024;
            let retained = serde_json::to_vec(self)
                .map_err(|error| error.to_string())?
                .len();
            let additional = serde_json::to_vec(&self.source)
                .map_err(|error| error.to_string())?
                .len();
            if self.previous_sources.len() >= 1024
                || retained.saturating_add(additional) > MAX_RECOVERY_BYTES
            {
                return Err("Native holder ownership lineage is full".into());
            }
            self.previous_sources.push(self.source.clone());
        }
        Ok(())
    }

    fn update_source(
        &mut self,
        source: super::Session,
        execution_console: Option<crate::model::LaunchRecipe>,
    ) -> Result<(), String> {
        let mut expected = self.source.clone();
        expected.terminal = source.terminal.clone();
        if (execution_console.is_some() && source.terminal.is_none())
            || source != expected
            || source.terminal.as_ref().is_some_and(|terminal| {
                terminal.id != source.id
                    || terminal.continuity.live_proof().as_ref() != source.proof.as_ref()
                    || match &execution_console {
                        Some(recipe) => {
                            &terminal.launch != recipe || terminal.owned_codex_listener.is_some()
                        }
                        None => terminal.owned_codex_listener != source.owned_codex_listener,
                    }
            })
        {
            return Err("Paired view update changed native conversation ownership".into());
        }
        if self.source != source {
            self.remember_source()?;
            self.source = source;
        }
        self.execution_console = execution_console;
        Ok(())
    }

    fn adopt(&mut self, source: &super::Session, proof: HostSessionProof) -> Result<(), String> {
        if !self.is_durable() || (&self.source != source && !self.previous_sources.contains(source))
        {
            return Err("Native holder does not recognize the exact source descriptor".into());
        }
        if !proof.is_complete()
            || self
                .source
                .proof
                .as_ref()
                .is_none_or(|old| old.boot_id != proof.boot_id)
        {
            return Err("Native holder cannot cross a boot boundary".into());
        }
        if self.source.proof.as_ref() != Some(&proof) {
            self.remember_source()?;
            self.source.proof = Some(proof);
        }
        Ok(())
    }
}

/// Versions verified with an isolated Responses API fixture while disconnecting
/// during a long command, a yielded-command poll, and a pending approval.
/// This is deliberately an allowlist: a 0.x version increment is not a promise
/// that an app-server preserves all three boundaries without a client.
const VERIFIED_CODEX_VERSIONS: &[&str] = &["0.157.1"];

/// Codex's initialize response includes its own version in a user agent whose
/// prefix is the client name supplied by this application. Use that response
/// from the actual owned server, rather than a separate executable probe that
/// could race an installation update.
pub(crate) fn verified_codex_version(user_agent: &str) -> Option<&str> {
    let version = user_agent
        .strip_prefix("cagents/")?
        .split_whitespace()
        .next()?;
    VERIFIED_CODEX_VERSIONS
        .contains(&version)
        .then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn recovery() -> Recovery {
        let id = crate::model::TerminalId::new();
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("fixture-host"),
            boot_id: BootId::new("fixture-boot"),
            host_incarnation: crate::model::HostIncarnation::new("fixture-incarnation"),
        };
        let socket = std::path::PathBuf::from(format!("/var/tmp/cagents-codex-{id}/control.sock"));
        Recovery {
            source: super::super::Session {
                id,
                launch: super::super::Launch {
                    provider: Provider::Codex,
                    recipe: crate::model::LaunchRecipe {
                        program: "codex".into(),
                        arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
                        working_directory: "/var/tmp".into(),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                    session_id: None,
                },
                proof: Some(proof.clone()),
                terminal: None,
                native_continuity: Default::default(),
                owned_codex_listener: Some(super::super::OwnedCodexListener {
                    socket,
                    allocator: proof,
                }),
            },
            execution_console: None,
            previous_sources: Vec::new(),
            confirmed_thread: None,
            server_version: None,
            server_identity: None,
            server_process: None,
            execution_environment: Default::default(),
        }
    }

    pub(super) fn server_identity() -> super::super::protocol::ServerIdentity {
        super::super::protocol::ServerIdentity {
            user_agent: "cagents/0.157.1 (Linux)".into(),
            canonical_home: "/var/tmp/confirmed-codex-home".into(),
            reported_home: "/var/tmp/confirmed-codex-home".into(),
        }
    }

    #[test]
    fn confirmation_and_adoption_keep_exact_conversation_and_allocator_lineage() {
        let mut recovery = recovery();
        let original = recovery.source.clone();
        let mut successor = original.proof.clone().unwrap();
        successor.host_incarnation = crate::model::HostIncarnation::new("successor");
        assert!(recovery.adopt(&original, successor.clone()).is_err());
        let thread = uuid::Uuid::new_v4().to_string();
        recovery
            .confirm(
                thread.clone(),
                server_identity(),
                crate::daemon::holder_registry::Process { pid: 42, start: 1 },
            )
            .unwrap();
        assert!(recovery.is_durable());
        assert_eq!(recovery.confirmed_thread.as_ref(), Some(&thread));
        assert!(recovery.previous_sources.contains(&original));
        recovery.adopt(&original, successor.clone()).unwrap();
        assert_eq!(recovery.source.proof.as_ref(), Some(&successor));
        assert_eq!(
            recovery.source.owned_codex_listener,
            original.owned_codex_listener
        );
        recovery.validate(&successor.boot_id).unwrap();
        let mut forged = original.clone();
        forged
            .launch
            .recipe
            .environment
            .insert("FORGED".into(), "value".into());
        assert!(recovery.adopt(&forged, successor.clone()).is_err());
        assert!(
            recovery
                .confirm(
                    uuid::Uuid::new_v4().to_string(),
                    server_identity(),
                    crate::daemon::holder_registry::Process { pid: 42, start: 1 }
                )
                .is_err()
        );
        successor.boot_id = BootId::new("another-boot");
        assert!(recovery.adopt(&original, successor).is_err());
        assert_eq!(recovery.confirmed_thread.as_ref(), Some(&thread));
    }

    #[test]
    fn native_continuity_requires_the_exact_tested_server_version() {
        assert_eq!(
            verified_codex_version("cagents/0.157.1 (Linux; x86_64) terminal (cagents; 0.1.3)"),
            Some("0.157.1")
        );
        for user_agent in [
            "cagents/0.153.0 (Linux)",
            "cagents/0.157.2 (Linux)",
            "cagents/0.158.0 (Linux)",
            "cagents/1.0.0 (Linux)",
            "cagents/0.157.1-dev (Linux)",
            "cagents/0.157.10 (Linux)",
            "another-client/0.157.1 (Linux)",
            "cagents/",
            "",
        ] {
            assert_eq!(verified_codex_version(user_agent), None, "{user_agent}");
        }
    }

    #[test]
    fn console_pair_requires_exact_private_evidence_and_survives_adoption() {
        let mut recovery = recovery();
        recovery
            .confirm(
                "thread-console".into(),
                server_identity(),
                crate::daemon::holder_registry::Process { pid: 42, start: 1 },
            )
            .unwrap();
        let mut paired = recovery.source.clone();
        let recipe = crate::model::LaunchRecipe {
            program: "/owned/build/cagents".into(),
            arguments: vec![
                "agent".into(),
                "console".into(),
                uuid::Uuid::new_v4().to_string(),
            ],
            working_directory: "/var/tmp".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let live = |proof: &HostSessionProof| crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        paired.terminal = Some(crate::model::TerminalSession {
            id: paired.id,
            launch: recipe.clone(),
            continuity: live(paired.proof.as_ref().unwrap()),
            transcript_path: None,
            resume_hint: None,
            owned_codex_listener: None,
            recovery_origin: None,
        });
        assert!(recovery.update_source(paired.clone(), None).is_err());
        let mut wrong = recipe.clone();
        wrong.program = "/unrelated/cagents".into();
        assert!(recovery.update_source(paired.clone(), Some(wrong)).is_err());
        recovery
            .update_source(paired.clone(), Some(recipe.clone()))
            .unwrap();
        let mut successor = paired.proof.clone().unwrap();
        successor.host_incarnation = crate::model::HostIncarnation::new("console-successor");
        recovery.adopt(&paired, successor.clone()).unwrap();
        let mut updated = recovery.source.clone();
        updated.terminal.as_mut().unwrap().continuity = live(&successor);
        recovery
            .update_source(updated, Some(recipe.clone()))
            .unwrap();
        assert_eq!(recovery.execution_console, Some(recipe));
        recovery.validate(&successor.boot_id).unwrap();
        assert!(recovery.previous_sources.contains(&paired));
    }

    #[test]
    fn paired_adoption_ack_preserves_both_descriptors_without_changing_conversation() {
        use crate::model::{TerminalContinuity, TerminalSession};
        let mut recovery = recovery();
        recovery
            .confirm(
                "thread-owned".into(),
                server_identity(),
                crate::daemon::holder_registry::Process { pid: 42, start: 1 },
            )
            .unwrap();
        let live = |proof: &HostSessionProof| TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let mut paired = recovery.source.clone();
        paired.terminal = Some(TerminalSession {
            id: paired.id,
            launch: paired.launch.recipe.clone(),
            continuity: live(paired.proof.as_ref().unwrap()),
            resume_hint: None,
            transcript_path: None,
            owned_codex_listener: paired.owned_codex_listener.clone(),
            recovery_origin: None,
        });
        recovery.update_source(paired.clone(), None).unwrap();
        let mut proof = paired.proof.clone().unwrap();
        proof.host_incarnation = crate::model::HostIncarnation::new("next-daemon");
        recovery.adopt(&paired, proof.clone()).unwrap();
        let mut successor = recovery.source.clone();
        successor.terminal.as_mut().unwrap().continuity = live(&proof);
        recovery.update_source(successor.clone(), None).unwrap();
        assert!(successor.retains_native_pair_from(&paired));
        assert!(recovery.previous_sources.contains(&paired));
        assert_eq!(recovery.source, successor);
        // If the ACK was lost, either issued descriptor still proves ownership.
        recovery.adopt(&paired, proof.clone()).unwrap();
        recovery.adopt(&successor, proof.clone()).unwrap();
        let mut forged = successor.clone();
        forged.launch.session_id = Some("different-thread".into());
        assert!(recovery.update_source(forged, None).is_err());
        let mut forged = successor.clone();
        forged.terminal.as_mut().unwrap().id = crate::model::TerminalId::new();
        assert!(recovery.update_source(forged, None).is_err());
        assert_eq!(recovery.source, successor);
    }

    #[test]
    fn large_recipe_cannot_multiply_unbounded_retained_lineage() {
        let mut recovery = recovery();
        recovery
            .source
            .launch
            .recipe
            .environment
            .insert("FIXTURE_LARGE".into(), "x".repeat(2 * 1024 * 1024));
        let original = recovery.source.clone();
        assert!(
            recovery
                .confirm(
                    "exact-thread".into(),
                    server_identity(),
                    crate::daemon::holder_registry::Process { pid: 42, start: 1 },
                )
                .is_err()
        );
        assert_eq!(recovery.source, original);
        assert!(recovery.previous_sources.is_empty());
        assert!(!recovery.is_durable());
    }
}
