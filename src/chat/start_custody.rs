//! Local evidence of a start whose physical effect has not been retired.
//!
//! These records never authorize a provider or manufacture an issued Session.
//! The original request and the authenticated owner observed later stay distinct.
use super::Session;
use crate::model::{CardId, ChannelId, HostSessionProof, ProjectId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartCustody {
    pub operation: uuid::Uuid,
    pub card: CardId,
    pub workspace: WorkspaceId,
    pub source: Session,
    pub project: Option<ProjectId>,
    pub channel: Option<ChannelId>,
    pub directory: Option<PathBuf>,
    pub registry: Option<uuid::Uuid>,
    pub device: Option<(String, String, String)>,
    /// A one-way role fingerprint; never an opaque caller capability.
    pub caller: Option<String>,
    /// Recorded from the actual connected socket before its start write.
    pub destination: Option<HostSessionProof>,
    /// A read-only authenticated observation, checkpointed before Close.
    pub observed: Option<Session>,
    pub failure: Option<String>,
}

impl StartCustody {
    pub(crate) fn covers_directory(&self, directory: &std::path::Path) -> bool {
        self.source
            .launch
            .recipe
            .working_directory
            .starts_with(directory)
            || self
                .directory
                .as_deref()
                .is_some_and(|scope| scope.starts_with(directory))
    }

    /// Exact launch pairing within the original observed daemon epoch. A
    /// newly issued proof is evidence to retain, never a replacement for raw
    /// request proof. A provider may have learned its conversation ID meanwhile.
    pub(crate) fn permits_observed_identity_learning(
        &self,
        observed: &Session,
        current: &Session,
    ) -> bool {
        if !self.matches_observed(observed) || !self.matches_observed(current) {
            return false;
        }
        if observed == current {
            return true;
        }
        if observed.launch.session_id.is_some()
            || !current
                .launch
                .session_id
                .as_ref()
                .is_some_and(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
        {
            return false;
        }
        let mut learned = observed.clone();
        learned.launch.session_id = current.launch.session_id.clone();
        learned == *current
    }

    pub(crate) fn matches_observed(&self, owner: &Session) -> bool {
        self.destination
            .as_ref()
            .is_some_and(|proof| proof.is_complete() && owner.proof.as_ref() == Some(proof))
            && self.source.id == owner.id
            && self.source.launch.provider == owner.launch.provider
            && self.source.launch.recipe == owner.launch.recipe
            && (self.source.launch.session_id.is_none()
                || self.source.launch.session_id == owner.launch.session_id)
            && self.source.terminal == owner.terminal
            && self.source.owned_codex_listener == owner.owned_codex_listener
    }
}

pub(crate) fn caller_fingerprint(authority: &crate::agent_execution::Authority) -> Option<String> {
    use sha2::{Digest, Sha256};
    authority
        .token()
        .map(|token| format!("{:x}", Sha256::digest(token.as_bytes())))
}

pub(crate) fn device_key() -> Option<(String, String, String)> {
    crate::device::selected().map(|device| {
        (
            device.name.clone(),
            device.host.clone(),
            device.executable.clone(),
        )
    })
}

/// Merge only additional observations for the same immutable operation/source.
/// A stale window may lack an entry, destination or newer observed descriptor.
pub(crate) fn merge_records(
    mut stored: Vec<StartCustody>,
    incoming: &[StartCustody],
    released: &std::collections::HashSet<uuid::Uuid>,
) -> Result<Vec<StartCustody>, String> {
    for entry in incoming {
        if released.contains(&entry.operation) {
            continue;
        }
        if let Some(old) = stored
            .iter_mut()
            .find(|old| old.operation == entry.operation)
        {
            let mut comparable = entry.clone();
            comparable.destination = old.destination.clone();
            comparable.observed = old.observed.clone();
            comparable.failure = old.failure.clone();
            if &comparable != old {
                return Err(
                    "A native start custody operation changed its original source or context"
                        .into(),
                );
            }
            if let Some(destination) = &entry.destination {
                if old
                    .destination
                    .as_ref()
                    .is_some_and(|old| old != destination)
                {
                    return Err(
                        "A native start custody operation changed its original destination".into(),
                    );
                }
                old.destination = Some(destination.clone());
            }
            if let Some(owner) = &entry.observed {
                if !old.matches_observed(owner)
                    || old.observed.as_ref().is_some_and(|old| old != owner)
                {
                    return Err(
                        "A native start custody operation changed its observed owner".into(),
                    );
                }
                old.observed = Some(owner.clone());
            }
            if entry.failure.is_some() {
                old.failure = entry.failure.clone();
            }
        } else {
            if entry
                .observed
                .as_ref()
                .is_some_and(|owner| !entry.matches_observed(owner))
            {
                return Err("A native start custody record has no exact observed owner".into());
            }
            stored.push(entry.clone());
        }
    }
    stored.retain(|entry| !released.contains(&entry.operation));
    Ok(stored)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        model::{LaunchRecipe, TerminalId},
    };
    pub(crate) fn fixture() -> StartCustody {
        StartCustody {
            operation: uuid::Uuid::new_v4(),
            card: CardId::new(),
            workspace: WorkspaceId::new(),
            source: Session {
                id: TerminalId::new(),
                launch: super::super::Launch {
                    provider: Provider::Claude,
                    recipe: LaunchRecipe {
                        program: "claude".into(),
                        arguments: vec![],
                        working_directory: "/project".into(),
                        environment: Default::default(),
                        unset_environment: Default::default(),
                    },
                    session_id: None,
                },
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            },
            project: None,
            channel: None,
            directory: Some("/filed".into()),
            registry: Some(uuid::Uuid::new_v4()),
            device: None,
            caller: None,
            destination: Some(HostSessionProof {
                host_session_id: "original".into(),
                boot_id: "boot".into(),
                host_incarnation: "epoch".into(),
            }),
            observed: None,
            failure: None,
        }
    }

    #[test]
    fn start_custody_merges_only_exact_additional_facts_and_never_forgets_unknown_operations() {
        let mut entry = fixture();
        assert!(entry.source.is_unstarted());
        let mut owner = entry.source.clone();
        owner.proof = entry.destination.clone();
        owner.launch.session_id = Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into());
        owner.native_continuity =
            super::super::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        assert!(entry.matches_observed(&owner));
        entry.observed = Some(owner.clone());
        let missing = merge_records(vec![entry.clone()], &[], &Default::default()).unwrap();
        assert_eq!(missing, [entry.clone()]);
        let mut stale = entry.clone();
        stale.destination = None;
        stale.observed = None;
        assert_eq!(
            merge_records(missing, &[stale], &Default::default()).unwrap(),
            [entry.clone()]
        );
        for changed in 0..7 {
            let mut replacement = owner.clone();
            match changed {
                0 => replacement.proof.as_mut().unwrap().host_incarnation = "later-epoch".into(),
                1 => replacement.id = TerminalId::new(),
                2 => replacement.launch.provider = Provider::Codex,
                3 => replacement.launch.recipe.working_directory = "/neighbor".into(),
                4 => {
                    replacement
                        .launch
                        .recipe
                        .environment
                        .insert("CLAUDE_CONFIG_DIR".into(), "/different-account".into());
                }
                5 => replacement
                    .launch
                    .recipe
                    .arguments
                    .push("--different-policy".into()),
                _ => {
                    replacement
                        .launch
                        .recipe
                        .unset_environment
                        .insert("CLAUDE_CONFIG_DIR".into());
                }
            }
            assert!(
                !entry.matches_observed(&replacement),
                "different owner {changed}"
            );
            let mut corrupt = entry.clone();
            corrupt.observed = Some(replacement);
            assert!(merge_records(vec![entry.clone()], &[corrupt], &Default::default()).is_err());
        }
        let mut changed_source = entry.clone();
        changed_source.source.launch.recipe.working_directory = "/other".into();
        assert!(
            merge_records(vec![entry.clone()], &[changed_source], &Default::default()).is_err()
        );
        let released = std::collections::HashSet::from([entry.operation]);
        assert!(
            merge_records(vec![entry.clone()], &[entry], &released)
                .unwrap()
                .is_empty()
        );
    }
}
