//! A same-daemon writer replacement is distinct from an upgrade or a snapshot.
//! Only the correlated restart response authorizes a pane to change its pair.
use super::Session;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartReceipt {
    source: Session,
    replacement: Session,
}
#[cfg(any(feature = "ui", feature = "host", test))]
impl RestartReceipt {
    #[cfg(any(feature = "host", test))]
    pub(crate) fn confirm(source: Session, replacement: Session) -> Result<Self, String> {
        let receipt = Self {
            source,
            replacement,
        };
        if !receipt.is_valid() {
            return Err(
                "The restarted writer changed its exact conversation or paired view".into(),
            );
        }
        Ok(receipt)
    }
    pub(crate) fn source(&self) -> &Session {
        &self.source
    }
    pub(crate) fn replacement(&self) -> &Session {
        &self.replacement
    }
    pub(crate) fn is_valid(&self) -> bool {
        let old = &self.source;
        let new = &self.replacement;
        if old.id != new.id
            || old.proof != new.proof
            || !new
                .proof
                .as_ref()
                .is_some_and(crate::model::HostSessionProof::is_complete)
            || old.launch.provider != new.launch.provider
            || old.launch.recipe != new.launch.recipe
            || old
                .launch
                .session_id
                .as_ref()
                .is_some_and(|id| new.launch.session_id.as_ref() != Some(id))
            || new
                .launch
                .session_id
                .as_ref()
                .is_none_or(|id| id.is_empty() || id.len() > 1024)
            || old.owned_codex_listener != new.owned_codex_listener
            || !old.has_valid_native_continuity()
            || !new.has_valid_native_continuity()
            || (matches!(
                old.native_continuity,
                super::NativeContinuity::RetainedClaudeActor { .. }
            ) && old.native_continuity != new.native_continuity)
        {
            return false;
        }
        match (&old.terminal, &new.terminal) {
            (None, None) => true,
            (None, Some(next)) => {
                next.id == new.id
                    && next.continuity.live_proof().as_ref() == new.proof.as_ref()
                    && (crate::agent_execution::console_agent(&next.launch).is_some()
                        && next.launch.environment.is_empty()
                        && next.launch.unset_environment.is_empty()
                        && next.launch.working_directory == new.launch.recipe.working_directory
                        && next.owned_codex_listener.is_none()
                        || new.owns_codex_listener()
                            && next.owned_codex_listener == new.owned_codex_listener
                            && new.launch.terminal_recipe().as_ref() == Some(&next.launch))
            }
            (Some(previous), Some(next)) => {
                if previous.id != old.id
                    || next.id != old.id
                    || previous.continuity.live_proof().as_ref() != old.proof.as_ref()
                    || next.continuity.live_proof().as_ref() != new.proof.as_ref()
                {
                    return false;
                }
                if let Some(agent) = crate::agent_execution::console_agent(&previous.launch) {
                    crate::agent_execution::console_agent(&next.launch) == Some(agent)
                        && previous.launch == next.launch
                        && previous.owned_codex_listener.is_none()
                        && next.owned_codex_listener.is_none()
                } else {
                    old.launch.provider == crate::agent::Provider::Codex
                        && old.owns_codex_listener()
                        && new.owns_codex_listener()
                        && next.owned_codex_listener == new.owned_codex_listener
                        && new.launch.terminal_recipe().as_ref() == Some(&next.launch)
                }
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_preserves_immutable_identity_and_only_replaces_the_paired_view() {
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("restart"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("same-daemon"),
        };
        let id = crate::model::TerminalId::new();
        let agent = crate::model::AgentId::new();
        let recipe = crate::model::LaunchRecipe {
            program: "claude".into(),
            arguments: vec![],
            working_directory: "/fixture".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        let console = crate::model::LaunchRecipe {
            program: "/matched/cagents".into(),
            arguments: vec!["agent".into(), "console".into(), agent.to_string()],
            ..recipe.clone()
        };
        let mut terminal =
            crate::terminal_ipc::TerminalDescriptor::relaunchable(id, console, None).session;
        terminal.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let source = Session {
            id,
            launch: super::super::Launch {
                provider: crate::agent::Provider::Claude,
                recipe,
                session_id: Some("11111111-1111-4111-8111-111111111111".into()),
            },
            proof: Some(proof),
            terminal: Some(terminal),
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let mut replacement = source.clone();
        replacement.terminal.as_mut().unwrap().transcript_path =
            Some("/fixture/new-console.log".into());
        let receipt = RestartReceipt::confirm(source.clone(), replacement.clone()).unwrap();
        assert!(receipt.is_valid());
        for field in 0..8 {
            let mut changed = replacement.clone();
            match field {
                0 => {
                    changed
                        .launch
                        .recipe
                        .environment
                        .insert("ACCOUNT".into(), "different".into());
                }
                1 => {
                    changed
                        .launch
                        .recipe
                        .unset_environment
                        .insert("ACCOUNT".into());
                }
                2 => changed
                    .launch
                    .recipe
                    .arguments
                    .push("--different-option".into()),
                3 => changed.launch.recipe.working_directory = "/another".into(),
                4 => {
                    changed.launch.session_id = Some("22222222-2222-4222-8222-222222222222".into())
                }
                5 => {
                    changed.proof.as_mut().unwrap().host_incarnation =
                        crate::model::HostIncarnation::new("another-daemon")
                }
                6 => {
                    changed.terminal.as_mut().unwrap().launch.arguments[2] =
                        crate::model::AgentId::new().to_string()
                }
                _ => changed.terminal = None,
            }
            assert!(
                RestartReceipt::confirm(source.clone(), changed).is_err(),
                "changed field {field}"
            );
        }
        let mut retained = source.clone();
        retained.native_continuity =
            super::super::NativeContinuity::RetainedClaudeActor { protocol: 1 };
        let mut retained_next = replacement.clone();
        retained_next.native_continuity = retained.native_continuity.clone();
        assert!(RestartReceipt::confirm(retained.clone(), retained_next.clone()).is_ok());
        retained_next.native_continuity = super::super::NativeContinuity::ReconnectOnly;
        assert!(
            RestartReceipt::confirm(retained, retained_next).is_err(),
            "restart cannot lose retained actor ownership"
        );
        let mut early = source;
        early.launch.session_id = None;
        assert!(RestartReceipt::confirm(early, replacement).is_ok());
    }
}
