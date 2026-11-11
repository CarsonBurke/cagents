//! Read-only, transaction-bound proof that an update left a usable service.
//! No checkpoint in this module is replayed, promoted, archived or removed.
use super::*;
use crate::{
    model::{HostSessionProof, TerminalContinuity, TerminalSession},
    terminal_ipc::{StartupState, TerminalDescriptor},
};
use std::collections::BTreeSet;

const VERSION: u8 = 1;
pub(crate) const CENSUS_ENV: &str = "CAGENTS_INSTALL_CENSUS_PATH";
pub(crate) const OUTCOME_ENV: &str = "CAGENTS_INSTALL_OUTCOME_PATH";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Census {
    version: u8,
    pub transaction: String,
    pub source: Option<HostSessionProof>,
    terminals: Vec<TerminalSession>,
    chats: Vec<crate::chat::Session>,
    #[serde(default)]
    checkpoint_before: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Outcome {
    Usable {
        serving: HostSessionProof,
        terminals: usize,
        chats: usize,
        #[serde(default)]
        paused_terminals: Vec<crate::model::TerminalId>,
        #[serde(default)]
        paused_chats: Vec<crate::model::TerminalId>,
    },
    /// A verified absence is useful for staging a fresh installation, but is
    /// deliberately not a usable daemon or a completed daemon replacement.
    NoSource,
    NeedsTargetRecovery {
        checkpoint_digest: String,
        source: HostSessionProof,
    },
    Unverified {
        serving: Option<HostSessionProof>,
        readiness: Option<StartupState>,
        surviving_terminals: usize,
        expected_terminals: usize,
        missing: Vec<crate::model::TerminalId>,
        unexpected: Vec<crate::model::TerminalId>,
        changed: Vec<crate::model::TerminalId>,
        detail: String,
    },
    RestartPermitted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u8,
    pub transaction: String,
    pub census_digest: String,
    pub outcome: Outcome,
    #[serde(default)]
    checkpoint_before: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TargetRecoveryRequired {
    pub transaction: String,
    pub census_digest: String,
    pub checkpoint_digest: String,
    pub source: HostSessionProof,
    pub checkpoint_before: Option<String>,
    #[serde(default)]
    pub recovery_command: Option<String>,
}
impl std::fmt::Display for TargetRecoveryRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(command) = &self.recovery_command {
            return write!(
                f,
                "The previous build cannot recover this Prepared checkpoint. Its saved panes and checkpoint are retained; daemon readiness is unverified. Run this exact command to recover through the retained target without starting providers: {command}"
            );
        }
        write!(
            f,
            "The previous build cannot recover this Prepared checkpoint. Its saved panes and checkpoint are retained; terminal readiness is unverified. Keep the checkpoint for the staged build's recovery owner (transaction {}). Run `cagents --no-start daemon status` and `cagents --no-start terminal list` to inspect what is serving. Do not restart the previous build or delete the checkpoint.",
            self.transaction
        )
    }
}
impl std::error::Error for TargetRecoveryRequired {}

impl Census {
    fn from_recovered(
        transaction: String,
        recovered: &crate::upgrade::recovery::AlreadyRecovered,
    ) -> Result<Self, UpdateError> {
        // Only the shared exact checkpoint/request/partition classifier can
        // produce this value. Its complete original scope is distinct from a
        // fresh current roster or metadata-only discovery candidate.
        let census = Self {
            version: VERSION,
            transaction,
            source: Some(recovered.source.clone()),
            terminals: recovered.census.terminals.clone(),
            chats: recovered.census.chats.clone(),
            checkpoint_before: Some(recovered.checkpoint_hash().to_owned()),
        };
        census.validate()?;
        Ok(census)
    }

    pub(super) fn from_pending(
        transaction: String,
        pending: &crate::upgrade::recovery::PendingRecovery,
    ) -> Result<Self, UpdateError> {
        if !pending.original_census_complete {
            return Err(invalid(
                "The complete pre-update census is unavailable; an empty or checkpoint-only source cannot establish recovery",
            ));
        }
        let census = Self {
            version: VERSION,
            transaction,
            source: Some(pending.source.clone()),
            terminals: pending.census.terminals.clone(),
            chats: pending.census.chats.clone(),
            checkpoint_before: Some(pending.checkpoint_hash.clone()),
        };
        census.validate()?;
        Ok(census)
    }

    pub(crate) fn capture(transaction: String) -> Result<Self, UpdateError> {
        if crate::device::is_remote() {
            return Err(invalid("Update census requires the local service"));
        }
        if uuid::Uuid::parse_str(&transaction).is_err() {
            return Err(invalid("Invalid update census transaction"));
        }
        // A retained checkpoint is a source, even after its process exits.
        // Never replace that inventory with an empty socket observation.
        let handled = inspect_pending_source(&std::env::current_exe()?, None)?;
        match observe() {
            Ok((source, terminals, chats, readiness)) => {
                require_captured_owner(handled.as_ref(), Some(&source))?;
                if readiness != StartupState::Ready {
                    return Err(invalid(
                        "The source daemon is not ready. Run `cagents --no-start daemon status` and inspect retained recovery before updating.",
                    ));
                }
                let checkpoint = checkpoint_status()?;
                if handled.is_none()
                    && checkpoint.as_ref().is_some_and(|status| {
                        status.phase == crate::upgrade::handoff::Phase::Prepared
                    })
                {
                    // A preparation may have appeared during the socket read.
                    // Admit its serving replacement under the same strict
                    // rule before recording the current source inventory.
                    let now_handled = inspect_pending_source(&std::env::current_exe()?, None)?;
                    require_captured_owner(now_handled.as_ref(), Some(&source))?;
                    if now_handled.is_none() {
                        return Err(invalid(
                            "The Prepared checkpoint changed during census capture; its retained inventory was kept",
                        ));
                    }
                }
                let census = Self {
                    version: VERSION,
                    transaction,
                    source: Some(source),
                    terminals: terminals.into_iter().map(|t| t.session).collect(),
                    chats,
                    checkpoint_before: checkpoint.map(|s| s.digest),
                };
                census.validate()?;
                Ok(census)
            }
            Err(error) => {
                // A settled Prepared checkpoint never turns into an empty
                // source merely because its verified replacement exits here.
                require_captured_owner(handled.as_ref(), None)?;
                if checkpoint_status()?
                    .is_some_and(|status| status.phase == crate::upgrade::handoff::Phase::Prepared)
                {
                    inspect_pending(&std::env::current_exe()?, None)?;
                    return Err(invalid(
                        "The recovered daemon changed or exited during census capture; its retained source inventory was kept, and no empty census was created",
                    ));
                }
                let paths =
                    crate::daemon::RuntimePaths::discover().map_err(|e| invalid(e.to_string()))?;
                if !crate::daemon::HostLease::is_held(&paths).map_err(|e| invalid(e.to_string()))?
                    && std::os::unix::net::UnixStream::connect(&paths.socket).is_err_and(|e| {
                        matches!(
                            e.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                        )
                    })
                {
                    let checkpoint = checkpoint_status()?;
                    if checkpoint.as_ref().is_some_and(|status| {
                        status.phase == crate::upgrade::handoff::Phase::Prepared
                    }) {
                        inspect_pending(&std::env::current_exe()?, None)?;
                        return Err(invalid(
                            "A Prepared checkpoint appeared during census capture; its source inventory was kept, and no empty census was created",
                        ));
                    }
                    Ok(Self {
                        version: VERSION,
                        transaction,
                        source: None,
                        terminals: vec![],
                        chats: vec![],
                        checkpoint_before: checkpoint.map(|s| s.digest),
                    })
                } else {
                    Err(error)
                }
            }
        }
    }
    fn validate(&self) -> Result<(), UpdateError> {
        let terminals: BTreeSet<_> = self.terminals.iter().map(|t| t.id).collect();
        let chats: BTreeSet<_> = self.chats.iter().map(|t| t.id).collect();
        if self.version != VERSION
            || uuid::Uuid::parse_str(&self.transaction).is_err()
            || terminals.len() != self.terminals.len()
            || chats.len() != self.chats.len()
            || self.terminals.len() > 1024
            || self.chats.len() > 128
            || self.source.as_ref().is_some_and(|p| !p.is_complete())
            || (self.source.is_none() && (!self.terminals.is_empty() || !self.chats.is_empty()))
        {
            return Err(invalid(
                "Invalid or duplicate update census; no recovery was authorized",
            ));
        }
        Ok(())
    }
    pub(crate) fn digest(&self) -> Result<String, UpdateError> {
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(self)?)))
    }
    pub(crate) fn load(path: &Path) -> Result<Self, UpdateError> {
        let census: Self = serde_json::from_slice(&read_private(path, JOB_STATE_LIMIT)?)?;
        census.validate()?;
        Ok(census)
    }
    pub(crate) fn save(&self, path: &Path) -> Result<(), UpdateError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > JOB_STATE_LIMIT {
            return Err(invalid(
                "The terminal census exceeds its private size limit; keep the service running and reduce open sessions before retrying",
            ));
        }
        let mut file = private_create(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        File::open(
            path.parent()
                .ok_or_else(|| invalid("Census needs a private directory"))?,
        )?
        .sync_all()?;
        Ok(())
    }
    fn receipt(&self, outcome: Outcome) -> Result<Receipt, UpdateError> {
        Ok(Receipt {
            version: VERSION,
            transaction: self.transaction.clone(),
            census_digest: self.digest()?,
            outcome,
            checkpoint_before: self.checkpoint_before.clone(),
        })
    }
    pub(crate) fn unverified(&self, detail: String) -> Result<Receipt, UpdateError> {
        self.receipt(Outcome::Unverified {
            serving: None,
            readiness: None,
            surviving_terminals: 0,
            expected_terminals: self.terminals.len(),
            missing: self.terminals.iter().map(|t| t.id).collect(),
            unexpected: vec![],
            changed: vec![],
            detail,
        })
    }
    pub(crate) fn verify(&self) -> Result<Receipt, UpdateError> {
        match observe_with(|proof, terminals, chats, target| {
            // Evidence failure must not discard the independently observed
            // serving owner or skip the closing readiness/inventory fences.
            let evidence = (|| -> Result<_, UpdateError> {
                let lineage = crate::upgrade::handoff::Store::existing()
                    .and_then(|store| {
                        store
                            .map_or_else(|| Ok(Default::default()), |store| store.recovered_chats())
                    })
                    .map_err(invalid)?;
                let prepared = if self.source.as_ref() == Some(proof) {
                    None
                } else {
                    crate::upgrade::recovery::verified::VerifiedView::load(
                        proof, target, terminals, chats,
                    )
                    .map_err(invalid)?
                };
                let paused_ready = if self.source.as_ref() != Some(proof) && prepared.is_none() {
                    crate::upgrade::recovery::verified::ready::ReadyView::load(
                        proof, target, terminals, chats,
                    )
                    .map_err(invalid)?
                } else {
                    None
                };
                Ok((lineage, prepared, paused_ready))
            })();
            Ok(evidence)
        }) {
            Ok(((proof, terminals, chats, readiness), Ok((lineage, prepared, paused_ready)))) => {
                let mut outcome = compare_with_recovery(
                    self,
                    &proof,
                    &terminals,
                    &chats,
                    &lineage,
                    prepared.as_ref(),
                    paused_ready.as_ref(),
                );
                if readiness != StartupState::Ready {
                    outcome=match outcome {
                        Outcome::Unverified {surviving_terminals,expected_terminals,missing,unexpected,changed,..}=>Outcome::Unverified {serving:Some(proof),readiness:Some(readiness),surviving_terminals,expected_terminals,missing,unexpected,changed,detail:"The daemon answered but is not Ready; retained or pending restoration is not usable recovery".into()},
                        _=>Outcome::Unverified {serving:Some(proof),readiness:Some(readiness),surviving_terminals:self.terminals.len(),expected_terminals:self.terminals.len(),missing:vec![],unexpected:vec![],changed:vec![],detail:"The listed inventory matches but the daemon is not Ready; recovery is unverified".into()},
                    };
                }
                self.receipt(outcome)
            }
            Ok((observation, Err(error))) => {
                self.receipt(unverified_evidence(self, observation, error))
            }
            Err(error) => self.receipt(Outcome::Unverified {
                serving: None,
                readiness: None,
                surviving_terminals: 0,
                expected_terminals: self.terminals.len(),
                missing: self.terminals.iter().map(|t| t.id).collect(),
                unexpected: vec![],
                changed: vec![],
                detail: error.to_string(),
            }),
        }
    }
    /// A release worker may predate the replacement's wire protocol. Ask the
    /// confirmed target CLI for a fresh read-only proof instead of decoding a
    /// new wire through the old worker or treating protocol rejection as death.
    pub(crate) fn verify_with_target(
        &self,
        target: &Path,
        state: &Path,
        job: &str,
    ) -> Result<Receipt, UpdateError> {
        self.verify_with_target_mode(target, state, job, false)
    }

    pub(super) fn verify_with_target_retiring(
        &self,
        target: &Path,
        state: &Path,
        job: &str,
    ) -> Result<Receipt, UpdateError> {
        self.verify_with_target_mode(target, state, job, true)
    }

    fn verify_with_target_mode(
        &self,
        target: &Path,
        state: &Path,
        job: &str,
        retire: bool,
    ) -> Result<Receipt, UpdateError> {
        let executable = target.join("bin/cagents");
        let before = match fs::symlink_metadata(&executable) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let receipt = self.verify()?;
                if retire && receipt.usable() {
                    retire_current_service()?;
                    return self.verify();
                }
                return Ok(receipt);
            }
            Err(e) => return self.unverified(e.to_string()),
        };
        if !before.is_file()
            || !crate::install_guard::trusted_executable_owner(before.uid())
            || before.mode() & 0o022 != 0
        {
            return self
                .unverified("The recovery verifier executable is unsafe; it was not run".into());
        }
        let census_path = census_path(state, job);
        // Tests and direct integrations may supply a census before persistence.
        if !census_path.exists() {
            self.save(&census_path)?;
        }
        let expected = Census::load(&census_path)?;
        if expected.digest()? != self.digest()? {
            return self.unverified("The saved census changed before recovery verification".into());
        }
        let receipt_path = outcome_path(state, job);
        // A stale usable receipt must never substitute for this probe.
        self.unverified("Fresh readiness verification is pending".into())?
            .save(&receipt_path)?;
        let mut command = Command::new(&executable);
        command
            .args(["--no-start", "daemon", "verify-update-census", "--census"])
            .arg(&census_path)
            .arg("--outcome")
            .arg(&receipt_path);
        if retire {
            command.arg("--retire");
        }
        crate::install_guard::sanitize_child_environment(&mut command);
        unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
        let executed = bounded_command(
            &mut command,
            "Recovery readiness verification",
            SMALL_ASSET_LIMIT,
            Duration::from_secs(20),
            &CancellationToken::new(),
        );
        let after = fs::symlink_metadata(&executable)?;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return self.unverified("The recovery verifier changed during execution".into());
        }
        let receipt: Receipt =
            serde_json::from_slice(&read_private(&receipt_path, JOB_STATE_LIMIT)?)?;
        receipt.matches(self)?;
        if executed.is_err() && receipt.usable() {
            return self.unverified("The verifier did not finish successfully; its partial receipt is not readiness proof".into());
        }
        Ok(receipt)
    }
    /// Inspection is read-only. A Prepared checkpoint never authorizes an old
    /// restart; the capable-target owner must prove job causality and custody.
    pub(crate) fn inspect_restart(&self) -> Result<Receipt, UpdateError> {
        let Some(status) = checkpoint_status()? else {
            return self.receipt(Outcome::RestartPermitted);
        };
        if self.source.as_ref() != Some(&status.proof) {
            return Err(invalid(
                "A different source owns the checkpoint. Keep it unchanged and inspect the staged build's recovery; do not restart the previous build.",
            ));
        }
        match status.phase {
            crate::upgrade::handoff::Phase::Prepared => {
                self.receipt(Outcome::NeedsTargetRecovery {
                    checkpoint_digest: status.digest,
                    source: status.proof,
                })
            }
            crate::upgrade::handoff::Phase::Ready => self.receipt(Outcome::RestartPermitted),
            crate::upgrade::handoff::Phase::Restoring => Err(invalid(
                "Partial checkpoint restoration is unverified. Keep the checkpoint and inspect the staged build's recovery; do not restart the previous build.",
            )),
        }
    }
}

impl Receipt {
    pub(crate) fn usable(&self) -> bool {
        matches!(self.outcome, Outcome::Usable { .. })
    }
    pub(crate) fn has_listener(&self) -> bool {
        matches!(
            self.outcome,
            Outcome::Usable { .. }
                | Outcome::Unverified {
                    serving: Some(_),
                    ..
                }
        )
    }
    pub(crate) fn matches(&self, census: &Census) -> Result<(), UpdateError> {
        if self.version != VERSION
            || self.transaction != census.transaction
            || self.census_digest != census.digest()?
            || self.checkpoint_before != census.checkpoint_before
        {
            return Err(invalid(
                "Recovery receipt belongs to a different update census",
            ));
        }
        Ok(())
    }
    pub(crate) fn target_required(&self) -> Option<TargetRecoveryRequired> {
        if let Outcome::NeedsTargetRecovery {
            checkpoint_digest,
            source,
        } = &self.outcome
        {
            Some(TargetRecoveryRequired {
                transaction: self.transaction.clone(),
                census_digest: self.census_digest.clone(),
                checkpoint_digest: checkpoint_digest.clone(),
                source: source.clone(),
                checkpoint_before: self.checkpoint_before.clone(),
                recovery_command: None,
            })
        } else {
            None
        }
    }
    pub(crate) fn save(&self, path: &Path) -> Result<(), UpdateError> {
        // Atomic replace: both the prior receipt and temporary are private.
        let parent = path
            .parent()
            .ok_or_else(|| invalid("Recovery receipt needs a private directory"))?;
        let temporary = parent.join(format!(".recovery-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut f = private_create(&temporary)?;
            serde_json::to_writer(&mut f, self)?;
            f.sync_all()?;
            fs::rename(&temporary, path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
    pub(crate) fn failure(&self) -> UpdateError {
        if let Some(required) = self.target_required() {
            return UpdateError::TargetRecoveryRequired(required);
        }
        let description = match &self.outcome {
            Outcome::Unverified {
                surviving_terminals,
                expected_terminals,
                missing,
                unexpected,
                changed,
                detail,
                ..
            } => format!(
                "{surviving_terminals} of {expected_terminals} original terminals were verified; missing {}, unexpected {}, changed {}. {detail}",
                missing.len(),
                unexpected.len(),
                changed.len()
            ),
            Outcome::NoSource => "No source daemon was serving; saved panes are retained".into(),
            _ => "Daemon readiness and terminal restoration have not been verified".into(),
        };
        invalid(format!(
            "Update recovery is unverified: {description}. Keep the checkpoint and saved panes. Run `cagents --no-start daemon status` and `cagents --no-start terminal list` before retrying the staged update; do not delete recovery state."
        ))
    }
}

type Observation = (
    HostSessionProof,
    Vec<TerminalDescriptor>,
    Vec<crate::chat::Session>,
    StartupState,
);

trait InventorySource {
    fn proof(&self) -> &HostSessionProof;
    fn inventory_generation(&mut self) -> Result<u64, UpdateError>;
    fn executable(&self) -> Result<PathBuf, UpdateError>;
    fn startup_state(&mut self) -> Result<StartupState, UpdateError>;
    fn list_terminals(&mut self) -> Result<Vec<TerminalDescriptor>, UpdateError>;
    fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, UpdateError>;
}

impl InventorySource for source::Client {
    fn proof(&self) -> &HostSessionProof {
        source::Client::proof(self)
    }
    fn executable(&self) -> Result<PathBuf, UpdateError> {
        self.serving_executable()
    }
    fn inventory_generation(&mut self) -> Result<u64, UpdateError> {
        source::Client::inventory_generation(self)
    }
    fn startup_state(&mut self) -> Result<StartupState, UpdateError> {
        source::Client::startup_state(self)
    }
    fn list_terminals(&mut self) -> Result<Vec<TerminalDescriptor>, UpdateError> {
        source::Client::list_terminals(self)
    }
    fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, UpdateError> {
        source::Client::list_chats(self)
    }
}

fn observe() -> Result<Observation, UpdateError> {
    observe_with(|_, _, _, _| Ok(())).map(|(inventory, ())| inventory)
}

fn observe_with<E>(
    read_evidence: impl FnOnce(
        &HostSessionProof,
        &[TerminalDescriptor],
        &[crate::chat::Session],
        &Path,
    ) -> Result<E, UpdateError>,
) -> Result<(Observation, E), UpdateError> {
    if crate::device::is_remote() {
        return Err(invalid("Update recovery requires the local daemon"));
    }
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let mut client =
        source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
    read_inventory_with(&mut client, read_evidence)
}

#[cfg(test)]
fn read_inventory(client: &mut impl InventorySource) -> Result<Observation, UpdateError> {
    read_inventory_with(client, |_, _, _, _| Ok(())).map(|(inventory, ())| inventory)
}

fn read_inventory_with<E>(
    client: &mut impl InventorySource,
    read_evidence: impl FnOnce(
        &HostSessionProof,
        &[TerminalDescriptor],
        &[crate::chat::Session],
        &Path,
    ) -> Result<E, UpdateError>,
) -> Result<(Observation, E), UpdateError> {
    let proof = client.proof().clone();
    let generation = client.inventory_generation()?;
    let readiness = client.startup_state()?;
    let terminals = client.list_terminals()?;
    let chats = client.list_chats()?;
    let executable = client.executable()?;
    // Durable ancestry is read inside the same generation fence as its exact
    // listed replacement. Loading it never resolves, adopts or starts a chat.
    let evidence = read_evidence(&proof, &terminals, &chats, &executable)?;
    let after_readiness = client.startup_state()?;
    let after_generation = client.inventory_generation()?;
    // The source increments this generation for terminal/chat topology and
    // completion. Bracketing both lists rejects a hybrid roster even if counts
    // or readiness return to their original values between the separate RPCs.
    if after_generation != generation
        || after_readiness != readiness
        || client.proof() != &proof
        || client.executable()? != executable
    {
        return Err(invalid(
            "Daemon ownership, readiness or inventory changed during census verification",
        ));
    }
    Ok(((proof, terminals, chats, readiness), evidence))
}

/// Report only the continuity directly present in the authenticated roster.
/// Failed durable evidence cannot promote this diagnostic into a usable proof.
fn unverified_evidence(census: &Census, observation: Observation, error: UpdateError) -> Outcome {
    let (proof, terminals, chats, readiness) = observation;
    let direct = compare_with_recovery(
        census,
        &proof,
        &terminals,
        &chats,
        &Default::default(),
        None,
        None,
    );
    let (surviving_terminals, missing, unexpected, changed) = match direct {
        Outcome::Unverified {
            surviving_terminals,
            missing,
            unexpected,
            changed,
            ..
        } => (surviving_terminals, missing, unexpected, changed),
        Outcome::Usable { terminals, .. } => (terminals, vec![], vec![], vec![]),
        _ => (
            0,
            census
                .terminals
                .iter()
                .map(|terminal| terminal.id)
                .collect(),
            vec![],
            vec![],
        ),
    };
    let service = match &readiness {
        StartupState::Ready => "Ready and answering".into(),
        StartupState::Restoring { awaiting } => {
            format!("answering while restoring {awaiting} owners")
        }
        StartupState::Retained { .. } => "answering in recovery-only mode".into(),
    };
    let detail = format!(
        "The daemon is {service}; {surviving_terminals} of {} original terminals have verified continuity. Recovery lineage could not be verified: {error}. Inspect `cagents --no-start daemon status` and `cagents --no-start terminal list`; keep the checkpoint and saved panes.",
        census.terminals.len()
    );
    Outcome::Unverified {
        serving: Some(proof),
        readiness: Some(readiness),
        surviving_terminals,
        expected_terminals: census.terminals.len(),
        missing,
        unexpected,
        changed,
        detail,
    }
}

#[cfg(test)]
fn compare(
    census: &Census,
    proof: &HostSessionProof,
    terminals: &[TerminalDescriptor],
    chats: &[crate::chat::Session],
) -> Outcome {
    compare_with_lineage(census, proof, terminals, chats, &Default::default())
}

#[cfg(test)]
fn compare_with_lineage(
    census: &Census,
    proof: &HostSessionProof,
    terminals: &[TerminalDescriptor],
    chats: &[crate::chat::Session],
    lineage: &crate::upgrade::handoff::RecoveredChats,
) -> Outcome {
    compare_with_recovery(census, proof, terminals, chats, lineage, None, None)
}

pub(crate) fn compare_with_recovery(
    census: &Census,
    proof: &HostSessionProof,
    terminals: &[TerminalDescriptor],
    chats: &[crate::chat::Session],
    lineage: &crate::upgrade::handoff::RecoveredChats,
    prepared: Option<&crate::upgrade::recovery::verified::VerifiedView>,
    paused_ready: Option<&crate::upgrade::recovery::verified::ready::ReadyView>,
) -> Outcome {
    let mut paused_terminals = vec![];
    let mut paused_chats = vec![];
    let mut missing = vec![];
    let mut changed = vec![];
    let mut unexpected = vec![];
    let mut used = BTreeSet::new();
    let mut surviving = 0;
    for expected in &census.terminals {
        let matches: Vec<_> = terminals
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.session.id == expected.id
                    || t.upgrade_sources
                        .iter()
                        .any(|s| issued_terminal_matches(expected, s))
                    || t.upgrade_relaunches
                        .iter()
                        .any(|r| issued_terminal_matches(expected, &r.source))
            })
            .collect();
        if matches.is_empty() {
            if prepared.is_some_and(|view| view.paused_terminal(expected))
                || paused_ready.is_some_and(|view| view.paused_terminal(expected))
            {
                paused_terminals.push(expected.id);
            } else {
                missing.push(expected.id);
            }
            continue;
        }
        if matches.len() != 1 {
            changed.push(expected.id);
            continue;
        }
        let (index, current) = matches[0];
        let issued = current
            .upgrade_sources
            .iter()
            .any(|s| issued_terminal_matches(expected, s))
            || current
                .upgrade_relaunches
                .iter()
                .any(|r| issued_terminal_matches(expected, &r.source));
        let same_owner =
            expected.continuity.live_proof() == current.session.continuity.live_proof();
        if !used.insert(index)
            || (expected.continuity.live_proof().is_some() && !same_owner && !issued)
            || (current.session.launch != expected.launch
                && !current.upgrade_relaunches.iter().any(|r| {
                    issued_terminal_matches(expected, &r.source)
                        && r.recipe == current.session.launch
                }))
            || !continuity_matches(&expected.continuity, &current.session.continuity, proof)
        {
            changed.push(expected.id);
        } else {
            surviving += 1;
        }
    }
    for (i, t) in terminals.iter().enumerate() {
        if !used.contains(&i) {
            unexpected.push(t.session.id);
        }
    }
    let mut surviving_chats = 0;
    for expected in &census.chats {
        let same: Vec<_> = chats.iter().filter(|c| c.id == expected.id).collect();
        if same.is_empty() {
            if prepared.is_some_and(|view| view.paused_chat(expected))
                || paused_ready.is_some_and(|view| view.paused_chat(expected))
            {
                paused_chats.push(expected.id);
            } else {
                missing.push(expected.id);
            }
        } else if same.len() != 1
            || !(same_owner_chat_matches(expected, same[0], proof)
                || lineage.proves(expected, same[0], proof)
                || prepared.is_some_and(|view| view.proves_chat(expected, same[0])))
        {
            changed.push(expected.id);
        } else {
            surviving_chats += 1;
        }
    }
    for current in chats {
        if !census.chats.iter().any(|c| c.id == current.id) {
            unexpected.push(current.id);
        }
    }
    if !proof.is_complete() || !missing.is_empty() || !changed.is_empty() || !unexpected.is_empty()
    {
        Outcome::Unverified {
            serving: Some(proof.clone()),
            readiness: Some(StartupState::Ready),
            surviving_terminals: surviving,
            expected_terminals: census.terminals.len(),
            missing,
            unexpected,
            changed,
            detail: format!(
                "The ready daemon's identity or inventory differs from the pre-update census; {surviving_chats} of {} original native chats have exact issued lineage to the serving owner",
                census.chats.len()
            ),
        }
    } else {
        Outcome::Usable {
            serving: proof.clone(),
            terminals: terminals.len(),
            chats: chats.len(),
            paused_terminals,
            paused_chats,
        }
    }
}
fn same_owner_chat_matches(
    expected: &crate::chat::Session,
    current: &crate::chat::Session,
    serving: &HostSessionProof,
) -> bool {
    // With the original serving owner, the exact issued descriptor is enough.
    // Changed owners additionally require the read-only durable lineage above.
    expected.proof.as_ref() == Some(serving)
        && current.proof.as_ref() == Some(serving)
        && current == expected
}

fn issued_terminal_matches(expected: &TerminalSession, issued: &TerminalSession) -> bool {
    let mut normalized = issued.clone();
    if expected.continuity.live_proof().is_some()
        && expected.continuity.live_proof() == issued.continuity.live_proof()
    {
        normalized.continuity = expected.continuity.clone();
    }
    expected == &normalized
}
fn continuity_matches(
    before: &TerminalContinuity,
    after: &TerminalContinuity,
    proof: &HostSessionProof,
) -> bool {
    if before.live_proof().is_some() {
        after.can_resume_live(proof)
    } else {
        before == after
    }
}

pub(crate) fn validate_file(path: &Path) -> Result<(), UpdateError> {
    Census::load(path).map(|_| ())
}
pub(crate) fn capture_file(output: &Path) -> Result<(), UpdateError> {
    Census::capture(uuid::Uuid::new_v4().to_string())?.save(output)
}

pub(super) fn inspect_pending(
    candidate_cli: &Path,
    census: Option<&Path>,
) -> Result<(), UpdateError> {
    inspect_pending_source(candidate_cli, census).map(|_| ())
}

fn inspect_pending_source(
    candidate_cli: &Path,
    census: Option<&Path>,
) -> Result<Option<HostSessionProof>, UpdateError> {
    if crate::device::is_remote() {
        return Err(invalid("Update recovery requires the local service"));
    }
    let mut pending = crate::upgrade::recovery::PendingRecovery::inspect().map_err(invalid)?;
    if let Some(pending) = pending.as_mut() {
        match serving_prepared(pending) {
            PreparedService::HandledVerified { serving } => return Ok(Some(serving)),
            PreparedService::RetainedUnverified(detail) => {
                return Err(invalid(format!(
                    "Current daemon readiness and the original inventory are unverified: {detail}. The retained checkpoint and recovery claim evidence were kept, and any serving daemon was left running. Run `cagents --no-start daemon status` and `cagents --no-start terminal list` to inspect the current service before another update"
                )));
            }
            PreparedService::Unresolved => {}
        }
        if let Some(census) = census {
            *pending = crate::upgrade::recovery::PendingRecovery::retain_installer_census(census)
                .map_err(invalid)?
                .ok_or_else(|| invalid("The Prepared checkpoint disappeared during inspection"))?;
        }
        let recovered_path = if !pending.original_census_complete && census.is_none() {
            let path = retained_job_census(candidate_cli, pending)?;
            *pending = crate::upgrade::recovery::PendingRecovery::retain_installer_census(&path)
                .map_err(invalid)?
                .ok_or_else(|| invalid("The Prepared checkpoint disappeared during inspection"))?;
            Some(path)
        } else {
            None
        };
        return Err(pending_required(
            pending,
            candidate_cli,
            census.or(recovered_path.as_deref()),
        )?);
    }
    Ok(None)
}

enum PreparedService {
    Unresolved,
    HandledVerified { serving: HostSessionProof },
    RetainedUnverified(String),
}

/// Read settled history only through the actual kernel-authenticated serving
/// executable. The history classifier supplies no replay or adoption authority.
fn serving_prepared(pending: &crate::upgrade::recovery::PendingRecovery) -> PreparedService {
    let claim_before = match pending.has_recovery_claim() {
        Ok(present) => present,
        Err(error) => return PreparedService::RetainedUnverified(error),
    };
    let mut classified = false;
    let observed = observe_with(|_, _, _, target| {
        let Some(store) = crate::upgrade::handoff::Store::existing().map_err(invalid)? else {
            return Ok(None);
        };
        let settled =
            match crate::upgrade::recovery::classify_retained_prepared(&store.path, target) {
                Ok(Some(settled)) => settled,
                Ok(None) | Err(_) => return Ok(None),
            };
        classified = true;
        if settled.source != pending.source
            || settled.census != pending.census
            || settled.witness.checkpoint_hash != pending.checkpoint_hash
            || settled.witness.target != target
        {
            return Err(invalid(
                "The settled record differs from the retained original source",
            ));
        }
        Census::from_pending(uuid::Uuid::new_v4().to_string(), pending)?
            .verify()
            .map(Some)
    });
    // A claim can appear while a daemon is settling. Recheck before offering
    // a manual entry point which deliberately refuses any existing claim.
    let claim_present = match pending.has_recovery_claim() {
        Ok(present) => claim_before || present,
        Err(error) => return PreparedService::RetainedUnverified(error),
    };
    match observed {
        Ok(((proof, _, _, readiness), receipt)) => {
            prepared_service_outcome(&pending.source, proof, readiness, receipt, claim_present)
        }
        Err(error) if classified || claim_present => {
            PreparedService::RetainedUnverified(error.to_string())
        }
        Err(_) => PreparedService::Unresolved,
    }
}

fn prepared_service_outcome(
    source: &HostSessionProof,
    observed: HostSessionProof,
    readiness: StartupState,
    classified_receipt: Option<Receipt>,
    claim_present: bool,
) -> PreparedService {
    let Some(receipt) = classified_receipt else {
        // Same-owner inventory equality cannot establish that a Prepared
        // checkpoint was ever handled by a replacement.
        return if claim_present {
            PreparedService::RetainedUnverified(
                "Retained recovery claim evidence exists, but an exact settled Ready service was not verified"
                    .into(),
            )
        } else {
            PreparedService::Unresolved
        };
    };
    if observed != *source
        && observed.is_complete()
        && readiness == StartupState::Ready
        && matches!(&receipt.outcome, Outcome::Usable { serving, .. } if *serving == observed)
    {
        PreparedService::HandledVerified { serving: observed }
    } else {
        PreparedService::RetainedUnverified(if receipt.usable() {
            "The serving identity or readiness changed during verification".into()
        } else {
            receipt.failure().to_string()
        })
    }
}

fn require_captured_owner(
    handled: Option<&HostSessionProof>,
    observed: Option<&HostSessionProof>,
) -> Result<(), UpdateError> {
    if handled.is_some() && handled != observed {
        return Err(invalid(
            "The recovered daemon changed or exited during census capture; its retained source inventory was kept, and no empty census was created",
        ));
    }
    Ok(())
}

/// Complete immutable Prepared evidence only after the sole original-census
/// verifier proves a fresh Ready socket and its complete live/paused inventory.
/// This never starts, stops, attaches or replays any owner.
pub(crate) fn retire_current_service() -> Result<RetirementStatus, UpdateError> {
    retire_service(RetirementTrigger::CurrentJob)
}

#[cfg(feature = "host")]
pub(crate) fn refresh_completed_service() -> Result<RetirementStatus, UpdateError> {
    retire_service(RetirementTrigger::ColdStart)
}

/// Mechanical evidence status, not another daemon usability outcome.
#[derive(Debug)]
pub(crate) enum RetirementStatus {
    NotRequired,
    AlreadyCompleted,
    Completed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetirementTrigger {
    CurrentJob,
    #[cfg(feature = "host")]
    ColdStart,
}

enum RetirementWork {
    NotRequired,
    AlreadyCompleted,
    Pending {
        recovered: crate::upgrade::recovery::AlreadyRecovered,
        original: Census,
        target: PathBuf,
        receipt: Receipt,
    },
}

fn retire_service(trigger: RetirementTrigger) -> Result<RetirementStatus, UpdateError> {
    let result = (|| {
        let Some(store) = crate::upgrade::handoff::Store::existing().map_err(invalid)? else {
            return Ok(RetirementStatus::NotRequired);
        };
        let socket = crate::terminal_ipc::daemon_socket_path()?;
        let mut client = source::Client::connect(&socket).map_err(|e| invalid(e.to_string()))?;
        let generation = client.inventory_generation()?;
        let ((proof, terminals, chats, readiness), retirement) = read_inventory_with(
            &mut client,
            |proof, terminals, chats, target| {
                let canonical_prepared = store
                    .checkpoint_status()
                    .map_err(invalid)?
                    .is_some_and(|status| status.phase == crate::upgrade::handoff::Phase::Prepared);
                let recovered = match crate::upgrade::recovery::pending_retirement(
                    &store.path,
                    target,
                ) {
                    Ok(recovered) => recovered,
                    Err(error)
                        if trigger == RetirementTrigger::CurrentJob && !canonical_prepared =>
                    {
                        tracing::warn!(%error, "unrelated historical recovery evidence was left untouched");
                        return Ok(RetirementWork::NotRequired);
                    }
                    Err(error) => return Err(invalid(error)),
                };
                let Some(recovered) = recovered else {
                    return Ok(RetirementWork::NotRequired);
                };
                if trigger == RetirementTrigger::CurrentJob && !canonical_prepared {
                    // This job has its own fresh RootCensus. Finished history
                    // must not gate it on owners the person later closed.
                    return match crate::upgrade::recovery::verified::completed_archive(
                        &store.path,
                        target,
                        &recovered,
                    ) {
                        Ok(true) => Ok(RetirementWork::AlreadyCompleted),
                        Ok(false) => {
                            tracing::warn!(
                                "historical recovery archival is unverified; its evidence was left untouched"
                            );
                            Ok(RetirementWork::NotRequired)
                        }
                        Err(error) => {
                            tracing::warn!(%error, "historical recovery archival is unverified; its evidence was left untouched");
                            Ok(RetirementWork::NotRequired)
                        }
                    };
                }
                if recovered.source == *proof {
                    return Err(invalid(
                        "The original source cannot prove its own replacement was completed",
                    ));
                }
                let original =
                    Census::from_recovered(uuid::Uuid::new_v4().to_string(), &recovered)?;
                let receipt = original.verify()?;
                let (paused_terminals, paused_chats) =
                    retirement_partition(&receipt, proof, &StartupState::Ready, terminals, chats)?;
                recovered
                    .validate_current_inventory(
                        proof,
                        terminals,
                        chats,
                        &paused_terminals,
                        &paused_chats,
                    )
                    .map_err(invalid)?;
                Ok(RetirementWork::Pending {
                    recovered,
                    original,
                    target: target.to_owned(),
                    receipt,
                })
            },
        )?;
        let (recovered, original, target, receipt) = match retirement {
            RetirementWork::NotRequired => return Ok(RetirementStatus::NotRequired),
            RetirementWork::AlreadyCompleted => {
                if readiness != StartupState::Ready {
                    return Err(invalid(
                        "The serving daemon was not Ready while inspecting completed recovery evidence",
                    ));
                }
                return Ok(RetirementStatus::AlreadyCompleted);
            }
            RetirementWork::Pending {
                recovered,
                original,
                target,
                receipt,
            } => (recovered, original, target, receipt),
        };
        let (paused_terminals, paused_chats) =
            retirement_partition(&receipt, &proof, &readiness, &terminals, &chats)?;
        if client.inventory_generation()? != generation
            || client.startup_state()? != StartupState::Ready
            || client.serving_executable()? != target
        {
            return Err(invalid(
                "The Ready service or inventory changed before recovery completion",
            ));
        }
        // Mechanical archival cannot establish user success. It follows the
        // exact root receipt above while the authenticated client stays pinned.
        recovered
            .archive_completed(&proof, &terminals, &chats, &paused_terminals, &paused_chats)
            .map_err(invalid)?;
        // Independently open the archived evidence through the same root
        // verifier. A planned pointer alone is not durable completion.
        let refreshed = original.verify()?;
        retirement_partition(&refreshed, &proof, &StartupState::Ready, &terminals, &chats)?;
        if client.inventory_generation()? != generation
            || client.startup_state()? != StartupState::Ready
            || client.serving_executable()? != target
        {
            return Err(invalid(
                "The Ready service or inventory changed while completing recovery",
            ));
        }
        Ok(RetirementStatus::Completed)
    })();
    result.map_err(|error: UpdateError| invalid(format!("Recovery completion could not be durably verified: {error}. Any serving daemon was kept running and its checkpoint/outcome evidence was preserved. Run `cagents --no-start daemon status` and `cagents --no-start terminal list` before another update")))
}

fn retirement_partition(
    receipt: &Receipt,
    proof: &HostSessionProof,
    readiness: &StartupState,
    live_terminals: &[TerminalDescriptor],
    live_chats: &[crate::chat::Session],
) -> Result<(Vec<crate::model::TerminalId>, Vec<crate::model::TerminalId>), UpdateError> {
    match &receipt.outcome {
        Outcome::Usable {
            serving,
            terminals,
            chats,
            paused_terminals,
            paused_chats,
        } if serving == proof
            && proof.is_complete()
            && *readiness == StartupState::Ready
            && *terminals == live_terminals.len()
            && *chats == live_chats.len() =>
        {
            Ok((paused_terminals.clone(), paused_chats.clone()))
        }
        Outcome::Usable { .. } => Err(invalid(
            "The root recovery receipt does not match this Ready serving proof and current inventory; completion was not authorized",
        )),
        _ => Err(receipt.failure()),
    }
}

/// Resolve one private, request-bound job, rather than searching by timestamps
/// or borrowing another update's census. A retry cannot overwrite this scope.
fn retained_job_census(
    candidate_cli: &Path,
    pending: &crate::upgrade::recovery::PendingRecovery,
) -> Result<PathBuf, UpdateError> {
    let missing = || {
        invalid(
            "A Prepared checkpoint is retained, but its complete original census is unavailable. Nothing was restarted or recaptured. Keep the checkpoint and the failed install's original private update-census.json; rerun the retained target's inspect-pending-update-recovery with --census pointing to that file before recovering",
        )
    };
    let layout = locate(candidate_cli).map_err(|_| missing())?;
    let state = layout.prefix.join(".update");
    let directory = fs::symlink_metadata(&state).map_err(|_| missing())?;
    if !directory.is_dir()
        || directory.uid() != unsafe { libc::geteuid() }
        || directory.mode() & 0o077 != 0
    {
        return Err(missing());
    }
    let status_path = state.join("status.json");
    let status_bytes = read_private(&status_path, JOB_STATE_LIMIT).map_err(|_| missing())?;
    let job: UpdateJob = serde_json::from_slice(&status_bytes).map_err(|_| missing())?;
    if uuid::Uuid::parse_str(&job.id).is_err() || job.version != layout.version {
        return Err(missing());
    }
    let request: UpdateRequest = serde_json::from_slice(
        &read_private(&state.join(format!("{}.request", job.id)), JOB_STATE_LIMIT)
            .map_err(|_| missing())?,
    )
    .map_err(|_| missing())?;
    let target_matches = match request {
        UpdateRequest::Installed { root } | UpdateRequest::InstalledFromTerminal { root } => root
            .canonicalize()
            .is_ok_and(|root| root == layout.version_root),
        UpdateRequest::Release(release) | UpdateRequest::ReleaseFromTerminal(release) => {
            release.version == job.version
        }
    };
    let path = census_path(&state, &job.id);
    let original = Census::load(&path).map_err(|_| missing())?;
    if !target_matches
        || original.transaction != job.id
        || original.source.as_ref() != Some(&pending.source)
        || original.checkpoint_before.as_ref() == Some(&pending.checkpoint_hash)
        || read_private(&status_path, JOB_STATE_LIMIT)? != status_bytes
    {
        return Err(missing());
    }
    Ok(path)
}

fn pending_required(
    pending: &crate::upgrade::recovery::PendingRecovery,
    candidate_cli: &Path,
    original_census: Option<&Path>,
) -> Result<UpdateError, UpdateError> {
    if !pending.original_census_complete {
        return Err(invalid(
            "The complete original census is unavailable; no recovery command was issued from a checkpoint-only inventory. Keep the checkpoint and the original installer census; no source inventory was recaptured",
        ));
    }
    let census = Census {
        version: VERSION,
        transaction: uuid::Uuid::new_v4().to_string(),
        source: Some(pending.source.clone()),
        terminals: pending.census.terminals.clone(),
        chats: pending.census.chats.clone(),
        checkpoint_before: Some(pending.checkpoint_hash.clone()),
    };
    census.validate()?;
    Ok(UpdateError::TargetRecoveryRequired(
        TargetRecoveryRequired {
            transaction: census.transaction.clone(),
            census_digest: census.digest()?,
            checkpoint_digest: pending.checkpoint_hash.clone(),
            source: pending.source.clone(),
            checkpoint_before: census.checkpoint_before.clone(),
            recovery_command: Some(pending.command(candidate_cli, original_census)),
        },
    ))
}

/// Preserve the original installer census before reporting a target-only
/// recovery. The command names the complete retained target, never a temporary
/// coordinator or the old `current` executable.
pub(crate) fn verify_file_with_candidate(
    census: &Path,
    outcome: &Path,
    candidate_cli: Option<&Path>,
) -> Result<Receipt, UpdateError> {
    let original = Census::load(census)?;
    let permission = original.inspect_restart()?;
    if let Some(mut required) = permission.target_required() {
        permission.save(outcome)?;
        if let Some(candidate_cli) = candidate_cli {
            let pending =
                crate::upgrade::recovery::PendingRecovery::retain_installer_census(census)
                    .map_err(invalid)?
                    .ok_or_else(|| {
                        invalid("The Prepared checkpoint disappeared during inspection")
                    })?;
            required.recovery_command = Some(pending.command(candidate_cli, Some(census)));
        }
        return Err(UpdateError::TargetRecoveryRequired(required));
    }
    verify_file(census, outcome, true, false, false, false)
}

pub(crate) fn verify_file(
    census: &Path,
    outcome: &Path,
    inspect: bool,
    allow_absent: bool,
    require_original: bool,
    require_absent: bool,
) -> Result<Receipt, UpdateError> {
    if crate::device::is_remote() {
        return Err(invalid("Update recovery requires the local service"));
    }
    let census = Census::load(census)?;
    let mut receipt = if require_absent {
        match census.verify_absence() {
            Ok(receipt) => receipt,
            Err(error) => census.unverified(error.to_string())?,
        }
    } else if allow_absent && census.source.is_none() {
        match census.verify_absence() {
            Ok(receipt) => receipt,
            Err(error) => census.unverified(error.to_string())?,
        }
    } else if inspect {
        match census.inspect_restart() {
            Ok(r) => r,
            Err(e) => census.unverified(e.to_string())?,
        }
    } else {
        census.verify()?
    };
    if require_original {
        let unchanged = matches!(&receipt.outcome,Outcome::Usable{serving,..} if census.source.as_ref()==Some(serving));
        if !unchanged
            && !((allow_absent || require_absent) && matches!(receipt.outcome, Outcome::NoSource))
        {
            receipt = census.unverified(
                "The original service identity is no longer serving; retain the current recovery daemon and candidate installation."
                    .into(),
            )?;
        }
    }
    receipt.save(outcome)?;
    if inspect && matches!(receipt.outcome, Outcome::RestartPermitted) {
        return Ok(receipt);
    }
    if (!require_absent && receipt.usable())
        || ((allow_absent || require_absent) && matches!(receipt.outcome, Outcome::NoSource))
    {
        Ok(receipt)
    } else {
        Err(receipt.failure())
    }
}

/// Final post-window verification may complete archival through the target
/// CLI, including when the coordinator predates its wire protocol.
pub(crate) fn verify_file_retiring(
    census: &Path,
    outcome: &Path,
    allow_absent: bool,
    require_original: bool,
    require_absent: bool,
) -> Result<Receipt, UpdateError> {
    let receipt = verify_file(
        census,
        outcome,
        false,
        allow_absent,
        require_original,
        require_absent,
    )?;
    if !receipt.usable() {
        // A file-only installation has no recovery to retire.
        return Ok(receipt);
    }
    let original = Census::load(census)?;
    if let Err(error) = retire_current_service() {
        original.unverified(error.to_string())?.save(outcome)?;
        return Err(error);
    }
    let refreshed = original.verify()?;
    refreshed.save(outcome)?;
    if refreshed.usable() {
        Ok(refreshed)
    } else {
        Err(refreshed.failure())
    }
}

impl Census {
    pub(super) fn verify_absence(&self) -> Result<Receipt, UpdateError> {
        let fresh = Self::capture(self.transaction.clone())?;
        if !proves_absent(self, &fresh) {
            return Err(invalid(
                "A daemon or pending recovery exists; a fresh installation without a source daemon was not proven",
            ));
        }
        self.receipt(Outcome::NoSource)
    }
}

fn proves_absent(census: &Census, fresh: &Census) -> bool {
    census.transaction == fresh.transaction
        && [census, fresh].iter().all(|observed| {
            observed.source.is_none()
                && observed.terminals.is_empty()
                && observed.chats.is_empty()
                && observed.checkpoint_before.is_none()
        })
}

#[derive(Default)]
pub(super) struct Context {
    pub shutdown_requested: bool,
    pub census: Option<Census>,
}
impl Context {
    pub fn capture(&mut self, state: &Path, job: &UpdateJob) -> Result<(), UpdateError> {
        let census = Census::capture(job.id.clone())?;
        census.save(&census_path(state, &job.id))?;
        self.census = Some(census);
        Ok(())
    }
}
pub(super) fn census_path(state: &Path, job: &str) -> PathBuf {
    state.join(format!("{job}.census.json"))
}
pub(super) fn outcome_path(state: &Path, job: &str) -> PathBuf {
    state.join(format!("{job}.recovery.json"))
}

pub(super) fn checkpoint_status()
-> Result<Option<crate::upgrade::handoff::CheckpointStatus>, UpdateError> {
    crate::upgrade::handoff::Store::existing()
        .map_err(invalid)?
        .map(|s| s.checkpoint_status().map_err(invalid))
        .transpose()
        .map(Option::flatten)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("census-test"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("source"),
        }
    }
    fn terminal() -> TerminalDescriptor {
        let mut terminal = TerminalDescriptor::relaunchable(
            crate::model::TerminalId::new(),
            crate::model::LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec![],
                working_directory: "/private".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            None,
        );
        let p = proof();
        terminal.session.continuity = TerminalContinuity::Live {
            host_session_id: p.host_session_id,
            boot_id: p.boot_id,
            host_incarnation: p.host_incarnation,
        };
        terminal
    }
    fn census(terminal: &TerminalDescriptor) -> Census {
        Census {
            version: VERSION,
            transaction: uuid::Uuid::new_v4().to_string(),
            source: Some(proof()),
            terminals: vec![terminal.session.clone()],
            chats: vec![],
            checkpoint_before: None,
        }
    }
    #[test]
    fn update_recovery_requires_exact_inventory_not_matching_counts() {
        let original = terminal();
        let expected = census(&original);
        assert!(matches!(
            compare(&expected, &proof(), &[original.clone()], &[]),
            Outcome::Usable { .. }
        ));
        for actual in [
            vec![],
            vec![terminal()],
            vec![original.clone(), original.clone()],
            vec![original.clone(), terminal()],
        ] {
            assert!(matches!(
                compare(&expected, &proof(), &actual, &[]),
                Outcome::Unverified { .. }
            ));
        }
        let mut changed = original.clone();
        changed.session.launch.arguments.push("changed".into());
        assert!(matches!(
            compare(&expected, &proof(), &[changed], &[]),
            Outcome::Unverified { .. }
        ));
        let mut ended = original.clone();
        ended.session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(1),
                signal: None,
                occurred_at_unix_ms: 1,
            },
        };
        assert!(matches!(
            compare(&expected, &proof(), &[ended], &[]),
            Outcome::Unverified { .. }
        ));
    }
    #[test]
    fn update_recovery_accepts_only_issued_lineage_and_exact_relaunch_receipt() {
        let original = terminal();
        let expected = census(&original);
        let mut successor = original.clone();
        successor.session.id = crate::model::TerminalId::new();
        successor.session.launch.program = "/new/bin/cagents".into();
        successor.upgrade_relaunches = vec![crate::terminal_ipc::UpgradeRelaunch {
            source: original.session.clone(),
            recipe: successor.session.launch.clone(),
        }];
        assert!(matches!(
            compare(&expected, &proof(), &[successor.clone()], &[]),
            Outcome::Usable { .. }
        ));
        successor.upgrade_relaunches[0].source.id = crate::model::TerminalId::new();
        assert!(matches!(
            compare(&expected, &proof(), &[successor], &[]),
            Outcome::Unverified { .. }
        ));
    }
    #[test]
    fn update_recovery_same_id_and_launch_without_issued_source_is_not_continuity() {
        let original = terminal();
        let expected = census(&original);
        let mut new_proof = proof();
        new_proof.host_incarnation = crate::model::HostIncarnation::new("replacement");
        let mut recreated = original.clone();
        recreated.session.continuity = TerminalContinuity::Live {
            host_session_id: new_proof.host_session_id.clone(),
            boot_id: new_proof.boot_id.clone(),
            host_incarnation: new_proof.host_incarnation.clone(),
        };
        assert!(matches!(
            compare(&expected, &new_proof, &[recreated.clone()], &[]),
            Outcome::Unverified { .. }
        ));
        recreated.upgrade_sources.push(original.session.clone());
        assert!(matches!(
            compare(&expected, &new_proof, &[recreated], &[]),
            Outcome::Usable { .. }
        ));
    }
    fn chat() -> crate::chat::Session {
        let mut recipe = terminal().session.launch;
        recipe.program = "claude".into();
        recipe.arguments.clear();
        crate::chat::Session {
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: Some(proof()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::RetainedClaudeActor { protocol: 2 },
        }
    }

    #[test]
    fn update_recovery_chat_requires_exact_issued_session_and_serving_owner() {
        let original = chat();
        let mut expected = census(&terminal());
        expected.terminals.clear();
        expected.chats.push(original.clone());
        assert!(matches!(
            compare(&expected, &proof(), &[], &[original.clone()]),
            Outcome::Usable { .. }
        ));
        let mut changed_owner = proof();
        changed_owner.host_incarnation = crate::model::HostIncarnation::new("replacement");
        let mut recreated = original.clone();
        recreated.proof = Some(changed_owner.clone());
        for (serving, current) in [
            (proof(), recreated.clone()),
            (changed_owner.clone(), recreated),
            (changed_owner, original.clone()),
        ] {
            assert!(
                matches!(compare(&expected, &serving, &[], &[current]), Outcome::Unverified { changed, .. } if changed == [original.id])
            );
        }
        let mut no_proof = original.clone();
        no_proof.proof = None;
        let mut changed_pair = original.clone();
        changed_pair.terminal = Some(terminal().session);
        let mut changed_continuity = original.clone();
        changed_continuity.native_continuity = crate::chat::NativeContinuity::ReconnectOnly;
        for current in [no_proof, changed_pair, changed_continuity] {
            assert!(
                matches!(compare(&expected, &proof(), &[], &[current]), Outcome::Unverified { changed, .. } if changed == [original.id])
            );
        }
        assert!(matches!(
            compare(&expected, &proof(), &[], &[original.clone(), original]),
            Outcome::Unverified { .. }
        ));
    }

    #[test]
    fn update_recovery_chat_census_accepts_exact_durable_claude_and_codex_replacements() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::upgrade::handoff::Store::open(directory.path()).unwrap();
        let mut expected = census(&terminal());
        expected.terminals.clear();
        let claude = chat();
        let mut codex = chat();
        codex.launch.provider = crate::agent::Provider::Codex;
        codex.launch.recipe.program = "codex".into();
        codex.native_continuity = Default::default();
        expected.chats = vec![claude, codex];
        let mut serving = proof();
        serving.host_incarnation = "replacement".into();
        let current: Vec<_> = expected
            .chats
            .iter()
            .cloned()
            .map(|mut chat| {
                chat.proof = Some(serving.clone());
                chat
            })
            .collect();
        assert!(matches!(
            compare(&expected, &serving, &[], &current),
            Outcome::Unverified { .. }
        ));
        let entries: Vec<_> = expected
            .chats
            .iter()
            .zip(&current)
            .map(
                |(source, replacement)| crate::upgrade::handoff::RecoveredChat {
                    evidence: crate::upgrade::handoff::Chat {
                        source: source.clone(),
                        launch: source.launch.clone(),
                        previous_sources: vec![],
                    },
                    replacement: replacement.clone(),
                    aliases: vec![],
                },
            )
            .collect();
        store.record_recovered(&entries).unwrap();
        let lineage = store.recovered_chats().unwrap();
        assert!(matches!(
            compare_with_lineage(&expected, &serving, &[], &current, &lineage),
            Outcome::Usable { chats: 2, .. }
        ));
        for mutation in 0..4 {
            let mut different = current.clone();
            match mutation {
                0 => different[0].proof.as_mut().unwrap().host_incarnation = "foreign".into(),
                1 => different[1].launch.session_id = Some(uuid::Uuid::new_v4().to_string()),
                2 => different[0]
                    .launch
                    .recipe
                    .arguments
                    .push("--different".into()),
                _ => different.push(current[0].clone()),
            }
            assert!(
                matches!(
                    compare_with_lineage(&expected, &serving, &[], &different, &lineage),
                    Outcome::Unverified { .. }
                ),
                "mutation {mutation}"
            );
        }
    }

    #[test]
    fn update_recovery_require_absent_rejects_serving_inventory_and_pending_checkpoint() {
        let mut empty = census(&terminal());
        empty.source = None;
        empty.terminals.clear();
        assert!(proves_absent(&empty, &empty));
        for mutation in 0..5 {
            let mut changed = empty.clone();
            match mutation {
                0 => changed.source = Some(proof()),
                1 => changed.terminals.push(terminal().session),
                2 => changed.chats.push(chat()),
                3 => changed.checkpoint_before = Some("pending-checkpoint".into()),
                _ => changed.transaction = uuid::Uuid::new_v4().to_string(),
            }
            assert!(
                !proves_absent(&empty, &changed),
                "fresh mutation {mutation}"
            );
            assert!(
                !proves_absent(&changed, &empty),
                "original mutation {mutation}"
            );
        }
    }

    #[test]
    fn update_recovery_pending_admission_requires_settled_history_and_verified_replacement() {
        let original = terminal();
        let expected = census(&original);
        let same_owner = expected
            .receipt(compare(&expected, &proof(), &[original], &[]))
            .unwrap();
        assert!(same_owner.usable());
        assert!(matches!(
            prepared_service_outcome(&proof(), proof(), StartupState::Ready, None, false),
            PreparedService::Unresolved
        ));
        assert!(matches!(
            prepared_service_outcome(&proof(), proof(), StartupState::Ready, None, true),
            PreparedService::RetainedUnverified(_)
        ));
        assert!(matches!(
            prepared_service_outcome(
                &proof(),
                proof(),
                StartupState::Ready,
                Some(same_owner),
                true
            ),
            PreparedService::RetainedUnverified(_)
        ));
        // The serving incarnation can be the first replacement or a later
        // cold owner. Both require their own verified current inventory.
        for incarnation in ["replacement", "cold-replacement"] {
            let mut serving = proof();
            serving.host_incarnation = incarnation.into();
            let verified = expected
                .receipt(Outcome::Usable {
                    serving: serving.clone(),
                    terminals: 1,
                    chats: 0,
                    paused_terminals: vec![],
                    paused_chats: vec![],
                })
                .unwrap();
            assert!(matches!(
                prepared_service_outcome(
                    &proof(),
                    serving.clone(),
                    StartupState::Ready,
                    Some(verified.clone()),
                    true,
                ),
                PreparedService::HandledVerified { serving: accepted } if accepted == serving
            ));
            for (observed, readiness, receipt) in [
                (proof(), StartupState::Ready, verified.clone()),
                (
                    serving.clone(),
                    StartupState::Retained {
                        detail: "restoration is incomplete".into(),
                    },
                    verified,
                ),
                (
                    serving.clone(),
                    StartupState::Ready,
                    expected
                        .unverified("An original owner was explicitly closed after recovery".into())
                        .unwrap(),
                ),
            ] {
                assert!(matches!(
                    prepared_service_outcome(&proof(), observed, readiness, Some(receipt), true),
                    PreparedService::RetainedUnverified(_)
                ));
            }
        }
    }

    #[test]
    fn update_recovery_handled_capture_cannot_fall_back_to_empty_or_foreign_source() {
        let mut serving = proof();
        serving.host_incarnation = "replacement".into();
        let mut foreign = serving.clone();
        foreign.host_incarnation = "different-owner".into();
        assert!(require_captured_owner(None, None).is_ok());
        assert!(require_captured_owner(Some(&serving), Some(&serving)).is_ok());
        let original = proof();
        for observed in [None, Some(&foreign), Some(&original)] {
            let error = require_captured_owner(Some(&serving), observed).unwrap_err();
            assert!(error.to_string().contains("no empty census was created"));
        }
    }

    #[test]
    fn update_recovery_retirement_requires_the_root_ready_receipt_and_exact_live_partition() {
        let expected = census(&terminal());
        let mut serving = proof();
        serving.host_incarnation = "replacement".into();
        let paused = expected.terminals[0].id;
        let receipt = expected
            .receipt(Outcome::Usable {
                serving: serving.clone(),
                terminals: 0,
                chats: 0,
                paused_terminals: vec![paused],
                paused_chats: vec![],
            })
            .unwrap();
        let partition =
            retirement_partition(&receipt, &serving, &StartupState::Ready, &[], &[]).unwrap();
        assert_eq!(partition, (vec![paused], vec![]));
        for rejected in [
            expected.receipt(Outcome::NoSource).unwrap(),
            expected.receipt(Outcome::RestartPermitted).unwrap(),
            expected
                .unverified("Current holders were not authenticated".into())
                .unwrap(),
        ] {
            assert!(
                retirement_partition(&rejected, &serving, &StartupState::Ready, &[], &[]).is_err()
            );
        }
        assert!(retirement_partition(&receipt, &proof(), &StartupState::Ready, &[], &[]).is_err());
        assert!(
            retirement_partition(
                &receipt,
                &serving,
                &StartupState::Retained {
                    detail: "not Ready".into()
                },
                &[],
                &[]
            )
            .is_err()
        );
        assert!(
            retirement_partition(&receipt, &serving, &StartupState::Ready, &[terminal()], &[])
                .is_err()
        );
        assert!(
            retirement_partition(&receipt, &serving, &StartupState::Ready, &[], &[chat()]).is_err()
        );
    }

    #[test]
    fn update_recovery_allow_absent_cannot_hide_a_ready_checkpoint() {
        const PRIVATE: &str = "CAGENTS_TEST_ALLOW_ABSENT_READY_CHECKPOINT";
        if std::env::var_os(PRIVATE).is_none() {
            let root = tempfile::tempdir().unwrap();
            let runtime = root.path().join("runtime");
            fs::create_dir(&runtime).unwrap();
            fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    std::thread::current().name().unwrap(),
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(PRIVATE, root.path())
                .env("XDG_RUNTIME_DIR", runtime)
                .env("XDG_DATA_HOME", root.path().join("data"))
                .env("XDG_STATE_HOME", root.path().join("state"))
                .env("XDG_CONFIG_HOME", root.path().join("config"))
                .env("XDG_CACHE_HOME", root.path().join("cache"))
                .env("HOME", root.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        let root = PathBuf::from(std::env::var_os(PRIVATE).unwrap());
        let store = crate::upgrade::handoff::Store::discover().unwrap();
        let mut checkpoint =
            crate::upgrade::handoff::Checkpoint::new(proof(), vec![], vec![], vec![]);
        checkpoint.phase = crate::upgrade::handoff::Phase::Ready;
        store.save(&checkpoint).unwrap();
        let before = fs::read(&store.path).unwrap();
        let mut original = census(&terminal());
        original.source = None;
        original.terminals.clear();
        let path = root.join("original-census.json");
        original.save(&path).unwrap();
        let observed = Census::capture(original.transaction.clone()).unwrap();
        assert!(observed.source.is_none());
        assert!(observed.checkpoint_before.is_some());
        let outcome = root.join("outcome.json");
        for require_absent in [false, true] {
            assert!(verify_file(&path, &outcome, false, true, false, require_absent).is_err());
            let receipt: Receipt = serde_json::from_slice(&fs::read(&outcome).unwrap()).unwrap();
            assert!(matches!(receipt.outcome, Outcome::Unverified { .. }));
            assert!(!receipt.usable());
        }
        assert_eq!(fs::read(&store.path).unwrap(), before);
    }

    struct InventoryFixture {
        proof: HostSessionProof,
        generation: u64,
        mutate_between_lists: bool,
        calls: Vec<&'static str>,
    }
    impl InventorySource for InventoryFixture {
        fn proof(&self) -> &HostSessionProof {
            &self.proof
        }
        fn executable(&self) -> Result<PathBuf, UpdateError> {
            Ok("/fixture/cagentsd".into())
        }
        fn inventory_generation(&mut self) -> Result<u64, UpdateError> {
            self.calls.push("fence");
            Ok(self.generation)
        }
        fn startup_state(&mut self) -> Result<StartupState, UpdateError> {
            self.calls.push("ready");
            Ok(StartupState::Ready)
        }
        fn list_terminals(&mut self) -> Result<Vec<TerminalDescriptor>, UpdateError> {
            self.calls.push("terminals");
            if self.mutate_between_lists {
                self.generation += 2;
            }
            Ok(vec![])
        }
        fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, UpdateError> {
            self.calls.push("chats");
            Ok(vec![])
        }
    }

    #[test]
    fn update_recovery_inventory_generation_fences_split_lists_even_when_counts_match() {
        for mutate_between_lists in [false, true] {
            let mut source = InventoryFixture {
                proof: proof(),
                generation: 7,
                mutate_between_lists,
                calls: vec![],
            };
            let observed = read_inventory(&mut source);
            assert_eq!(observed.is_ok(), !mutate_between_lists);
            assert_eq!(
                source.calls,
                ["fence", "ready", "terminals", "chats", "ready", "fence"]
            );
            if mutate_between_lists {
                // A remove-and-add between the lists has identical counts and
                // Ready before/after, but cannot become a usable hybrid census.
                assert!(
                    observed
                        .unwrap_err()
                        .to_string()
                        .contains("inventory changed")
                );
            }
        }
    }

    #[test]
    fn update_recovery_observes_durable_evidence_inside_inventory_fence() {
        let mut source = InventoryFixture {
            proof: proof(),
            generation: 7,
            mutate_between_lists: false,
            calls: vec![],
        };
        let (_, evidence) =
            read_inventory_with(&mut source, |_, _, _, _| Ok("read-only evidence")).unwrap();
        assert_eq!(evidence, "read-only evidence");
        assert_eq!(
            source.calls,
            ["fence", "ready", "terminals", "chats", "ready", "fence"]
        );
        source.calls.clear();
        assert!(
            read_inventory_with(&mut source, |_, _, _, _| Err::<(), _>(invalid(
                "unsafe evidence"
            )))
            .is_err()
        );
        assert_eq!(source.calls, ["fence", "ready", "terminals", "chats"]);
    }

    #[test]
    fn update_recovery_evidence_failure_keeps_serving_ready_and_direct_terminal_continuity() {
        let original = terminal();
        let mut expected = census(&original);
        let missing = chat();
        expected.chats.push(missing.clone());
        let mut serving = proof();
        serving.host_incarnation = "cold-e".into();
        let mut current = original.clone();
        current.session.continuity = TerminalContinuity::Live {
            host_session_id: serving.host_session_id.clone(),
            boot_id: serving.boot_id.clone(),
            host_incarnation: serving.host_incarnation.clone(),
        };
        current.upgrade_sources.push(original.session);
        let outcome = unverified_evidence(
            &expected,
            (serving.clone(), vec![current], vec![], StartupState::Ready),
            invalid("The original accepted native owner is absent"),
        );
        assert!(
            matches!(&outcome, Outcome::Unverified { serving:Some(owner), readiness:Some(StartupState::Ready), surviving_terminals:1, expected_terminals:1, missing:absent, changed, detail, .. }
            if owner == &serving && absent == &[missing.id] && changed.is_empty()
                && detail.contains("1 of 1 original terminals") && detail.contains("--no-start daemon status"))
        );
        assert!(!expected.receipt(outcome).unwrap().usable());
    }

    #[test]
    fn update_recovery_evidence_failure_never_promotes_matching_direct_inventory() {
        let original = terminal();
        let expected = census(&original);
        for readiness in [
            StartupState::Ready,
            StartupState::Restoring { awaiting: 1 },
            StartupState::Retained {
                detail: "original work retained".into(),
            },
        ] {
            let outcome = unverified_evidence(
                &expected,
                (proof(), vec![original.clone()], vec![], readiness.clone()),
                invalid("unsafe recovered lineage"),
            );
            assert!(
                matches!(&outcome, Outcome::Unverified { serving:Some(owner), readiness:Some(observed), surviving_terminals:1, missing, changed, unexpected, .. }
                if owner == &proof() && observed == &readiness && missing.is_empty() && changed.is_empty() && unexpected.is_empty())
            );
            assert!(!expected.receipt(outcome).unwrap().usable());
        }
    }

    #[test]
    fn update_recovery_evidence_error_still_checks_closing_inventory_fence() {
        for mutate_between_lists in [false, true] {
            let mut source = InventoryFixture {
                proof: proof(),
                generation: 7,
                mutate_between_lists,
                calls: vec![],
            };
            let observed = read_inventory_with(&mut source, |_, _, _, _| {
                Ok(Err::<(), _>(invalid("unverified cold lineage")))
            });
            assert_eq!(observed.is_ok(), !mutate_between_lists);
            assert_eq!(
                source.calls,
                ["fence", "ready", "terminals", "chats", "ready", "fence"]
            );
            if !mutate_between_lists {
                let ((owner, _, _, readiness), evidence) = observed.unwrap();
                assert_eq!(owner, proof());
                assert_eq!(readiness, StartupState::Ready);
                assert!(evidence.is_err());
            }
        }
    }

    #[test]
    fn update_recovery_receipt_is_bound_to_one_private_census_and_transaction() {
        let original = terminal();
        let expected = census(&original);
        let receipt = expected
            .receipt(compare(&expected, &proof(), &[original], &[]))
            .unwrap();
        assert!(receipt.usable());
        assert!(receipt.matches(&expected).is_ok());
        let mut changed = expected.clone();
        changed.transaction = uuid::Uuid::new_v4().to_string();
        assert!(receipt.matches(&changed).is_err());
        changed = expected.clone();
        changed.terminals[0]
            .launch
            .arguments
            .push("private-change".into());
        assert!(receipt.matches(&changed).is_err());
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("census.json");
        expected.save(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            Census::load(&path).unwrap().digest().unwrap(),
            expected.digest().unwrap()
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Census::load(&path).is_err());
    }
    #[test]
    fn update_recovery_ready_is_distinct_from_restart_permission_and_prepared_handoff() {
        let expected = census(&terminal());
        let permission = expected.receipt(Outcome::RestartPermitted).unwrap();
        assert!(!permission.usable());
        let prepared = expected
            .receipt(Outcome::NeedsTargetRecovery {
                checkpoint_digest: "immutable-byte-digest".into(),
                source: proof(),
            })
            .unwrap();
        assert!(!prepared.usable());
        assert!(matches!(
            prepared.failure(),
            UpdateError::TargetRecoveryRequired(_)
        ));
        assert_eq!(
            prepared.target_required().unwrap().census_digest,
            expected.digest().unwrap()
        );
        let error = expected
            .unverified("Listener is retained, not ready".into())
            .unwrap();
        assert!(!error.usable());
        assert!(
            error
                .failure()
                .to_string()
                .contains("--no-start terminal list")
        );
        assert!(!error.failure().to_string().contains("daemon start"));
    }
}
