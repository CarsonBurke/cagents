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
}
impl std::fmt::Display for TargetRecoveryRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The previous build cannot recover this Prepared checkpoint. Its saved panes and checkpoint are retained; terminal readiness is unverified. Keep the checkpoint for the staged build's recovery owner (transaction {}). Run `cagents --no-start daemon status` and `cagents --no-start terminal list` to inspect what is serving. Do not restart the previous build or delete the checkpoint.",
            self.transaction
        )
    }
}
impl std::error::Error for TargetRecoveryRequired {}

impl Census {
    pub(crate) fn capture(transaction: String) -> Result<Self, UpdateError> {
        if crate::device::is_remote() {
            return Err(invalid("Update census requires the local service"));
        }
        if uuid::Uuid::parse_str(&transaction).is_err() {
            return Err(invalid("Invalid update census transaction"));
        }
        match observe() {
            Ok((source, terminals, chats, readiness)) => {
                if readiness != StartupState::Ready {
                    return Err(invalid(
                        "The source daemon is not ready. Run `cagents --no-start daemon status` and inspect retained recovery before updating.",
                    ));
                }
                let census = Self {
                    version: VERSION,
                    transaction,
                    source: Some(source),
                    terminals: terminals.into_iter().map(|t| t.session).collect(),
                    chats,
                    checkpoint_before: checkpoint_status()?.map(|s| s.digest),
                };
                census.validate()?;
                Ok(census)
            }
            Err(error) => {
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
                    Ok(Self {
                        version: VERSION,
                        transaction,
                        source: None,
                        terminals: vec![],
                        chats: vec![],
                        checkpoint_before: checkpoint_status()?.map(|s| s.digest),
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
        match observe() {
            Ok((proof, terminals, chats, readiness)) => {
                let mut outcome = compare(self, &proof, &terminals, &chats);
                if readiness != StartupState::Ready {
                    outcome=match outcome {
                        Outcome::Unverified {surviving_terminals,expected_terminals,missing,unexpected,changed,..}=>Outcome::Unverified {serving:Some(proof),readiness:Some(readiness),surviving_terminals,expected_terminals,missing,unexpected,changed,detail:"The daemon answered but is not Ready; retained or pending restoration is not usable recovery".into()},
                        _=>Outcome::Unverified {serving:Some(proof),readiness:Some(readiness),surviving_terminals:self.terminals.len(),expected_terminals:self.terminals.len(),missing:vec![],unexpected:vec![],changed:vec![],detail:"The listed inventory matches but the daemon is not Ready; recovery is unverified".into()},
                    };
                }
                self.receipt(outcome)
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
        let executable = target.join("bin/cagents");
        let before = match fs::symlink_metadata(&executable) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return self.verify(),
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
    fn startup_state(&mut self) -> Result<StartupState, UpdateError>;
    fn list_terminals(&mut self) -> Result<Vec<TerminalDescriptor>, UpdateError>;
    fn list_chats(&mut self) -> Result<Vec<crate::chat::Session>, UpdateError>;
}

impl InventorySource for source::Client {
    fn proof(&self) -> &HostSessionProof {
        source::Client::proof(self)
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
    if crate::device::is_remote() {
        return Err(invalid("Update recovery requires the local daemon"));
    }
    let socket = crate::terminal_ipc::daemon_socket_path()?;
    let mut client =
        source::Client::connect(&socket).map_err(|error| invalid(error.to_string()))?;
    read_inventory(&mut client)
}

fn read_inventory(client: &mut impl InventorySource) -> Result<Observation, UpdateError> {
    let proof = client.proof().clone();
    let generation = client.inventory_generation()?;
    let readiness = client.startup_state()?;
    let terminals = client.list_terminals()?;
    let chats = client.list_chats()?;
    let after_readiness = client.startup_state()?;
    let after_generation = client.inventory_generation()?;
    // The source increments this generation for terminal/chat topology and
    // completion. Bracketing both lists rejects a hybrid roster even if counts
    // or readiness return to their original values between the separate RPCs.
    if after_generation != generation || after_readiness != readiness || client.proof() != &proof {
        return Err(invalid(
            "Daemon ownership, readiness or inventory changed during census verification",
        ));
    }
    Ok((proof, terminals, chats, readiness))
}

fn compare(
    census: &Census,
    proof: &HostSessionProof,
    terminals: &[TerminalDescriptor],
    chats: &[crate::chat::Session],
) -> Outcome {
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
            missing.push(expected.id);
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
            missing.push(expected.id);
        } else if same.len() != 1 || !same_owner_chat_matches(expected, same[0], proof) {
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
            detail: if census
                .chats
                .iter()
                .any(|chat| chat.proof.as_ref() != Some(proof))
            {
                format!(
                    "The ready daemon's identity or inventory differs from the pre-update census; {surviving_chats} of {} original native chats were verified. This read-only protocol does not expose issued chat ancestry across daemon owners, so matching chat IDs and launch recipes cannot verify restored conversations.",
                    census.chats.len()
                )
            } else {
                format!(
                    "The ready daemon's identity or inventory differs from the pre-update census; {surviving_chats} of {} original native chats were verified",
                    census.chats.len()
                )
            },
        }
    } else {
        Outcome::Usable {
            serving: proof.clone(),
            terminals: terminals.len(),
            chats: chats.len(),
        }
    }
}
fn same_owner_chat_matches(
    expected: &crate::chat::Session,
    current: &crate::chat::Session,
    serving: &HostSessionProof,
) -> bool {
    // ResolveUpgradeChat can adopt a missing retained owner, recover cleanup
    // attachments, or register history aliases. It is not a read-only lineage
    // probe, even after a list: a concurrent close can make that source missing.
    // Until a read-only ancestry contract exists, changed owners stay unverified.
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
pub(crate) fn verify_file(
    census: &Path,
    outcome: &Path,
    inspect: bool,
    allow_absent: bool,
    require_original: bool,
) -> Result<Receipt, UpdateError> {
    if crate::device::is_remote() {
        return Err(invalid("Update recovery requires the local service"));
    }
    let census = Census::load(census)?;
    let receipt = if allow_absent
        && census.source.is_none()
        && Census::capture(census.transaction.clone()).is_ok_and(|c| c.source.is_none())
    {
        census.receipt(Outcome::NoSource)?
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
        if !unchanged && !(allow_absent && matches!(receipt.outcome, Outcome::NoSource)) {
            return Err(invalid(
                "The original service identity is no longer serving; retain the current recovery daemon and candidate installation.",
            ));
        }
    }
    receipt.save(outcome)?;
    if inspect && matches!(receipt.outcome, Outcome::RestartPermitted) {
        return Ok(receipt);
    }
    if receipt.usable() || (allow_absent && matches!(receipt.outcome, Outcome::NoSource)) {
        Ok(receipt)
    } else {
        Err(receipt.failure())
    }
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
        crate::chat::Session {
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: terminal().session.launch,
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
