//! Finish an installed Ready update after its coordinator releases custody.
//! This record can admit saved windows after exact live work is verified. It
//! carries no authority to start a daemon or restore a checkpoint.
use super::*;
use crate::model::HostSessionProof;
#[cfg(feature = "host")]
use crate::terminal_ipc::StartupState;
use crate::upgrade::handoff::{CheckpointStatus, Phase, Store};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    job: String,
    request_digest: String,
    checkpoint_digest: String,
    target: PathBuf,
    target_image: (u64, u64),
    window_image: (u64, u64),
    source: HostSessionProof,
    successor: HostSessionProof,
    process: crate::daemon::holder_registry::Process,
    windows: Vec<String>,
    stage: Stage,
    #[serde(default, skip_serializing_if = "WitnessLocation::is_legacy")]
    witness: WitnessLocation,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Stage {
    Pending,
    Deferred,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WitnessLocation {
    #[default]
    Legacy,
    Attempt,
}

impl WitnessLocation {
    fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy)
    }
}

#[cfg(feature = "host")]
pub(crate) struct Original<'a> {
    pub(crate) source: &'a HostSessionProof,
    pub(crate) inventory: &'a crate::upgrade::recovery::Census,
    pub(crate) restored_checkpoint_digest: Option<&'a str>,
    pub(crate) parked_ready: bool,
    pub(crate) retry_of: Option<(
        &'a HostSessionProof,
        &'a crate::daemon::holder_registry::Process,
    )>,
}

#[cfg(feature = "host")]
pub enum StartupCheckpoint {
    Absent,
    AdoptionOnly {
        source: HostSessionProof,
        saved_digest: String,
        ready_digest: String,
    },
    ParkedReady {
        source: HostSessionProof,
        digest: String,
    },
    Unsettled,
}

#[cfg(feature = "host")]
impl StartupCheckpoint {
    pub fn saved_digest(&self) -> Option<&str> {
        match self {
            Self::AdoptionOnly { saved_digest, .. } => Some(saved_digest),
            _ => None,
        }
    }
}

#[cfg(feature = "host")]
pub fn startup_checkpoint() -> Result<StartupCheckpoint, UpdateError> {
    let checkpoint = Store::existing()
        .and_then(|store| store.map_or(Ok(None), |store| store.load_with_digest()));
    match checkpoint {
        Ok(None) => Ok(StartupCheckpoint::Absent),
        Ok(Some((checkpoint, saved_digest))) => {
            let source = checkpoint.proof.clone();
            if checkpoint.phase == Phase::Ready
                && let Some(ready_digest) = selected_parked_checkpoint(&checkpoint, &saved_digest)?
            {
                return Ok(StartupCheckpoint::ParkedReady {
                    source,
                    digest: ready_digest,
                });
            }
            match checkpoint.adoption_ready_digest() {
                Ok(Some(ready_digest)) => Ok(StartupCheckpoint::AdoptionOnly {
                    source,
                    saved_digest,
                    ready_digest,
                }),
                _ => Ok(StartupCheckpoint::Unsettled),
            }
        }
        Err(_) => Ok(StartupCheckpoint::Unsettled),
    }
}

#[cfg(feature = "host")]
fn selected_parked_checkpoint(
    checkpoint: &crate::upgrade::handoff::Checkpoint,
    digest: &str,
) -> Result<Option<String>, UpdateError> {
    let executable = crate::process::current_executable()?;
    let layout = match locate(&executable) {
        Ok(layout) => layout,
        Err(UpdateError::UnsupportedInstallation) => return Ok(None),
        Err(error) => return Err(error),
    };
    let state = layout.prefix.join(".update");
    let Some(job) = read_job(&state)? else {
        return Ok(None);
    };
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed) {
        return Ok(None);
    }
    let Some(intent) = read(&state, &job.id)? else {
        return Ok(None);
    };
    if intent.stage != Stage::Pending || !intent.names_selected(&layout)? {
        return Ok(None);
    }
    if !intent.recorded_parked(&state)? {
        return Err(invalid(
            "A selected update cannot reopen an unverified Ready checkpoint",
        ));
    }
    let ready = ready_plan::selected_evidence(&layout, &state, &job)?
        .ok_or_else(|| invalid("The selected parked update has no Ready reservation"))?;
    let retained = intent.read_witness(&state)?;
    intent.verify_request(&state)?;
    ready.verify_unmodified_checkpoint().map_err(invalid)?;
    if intent.witness.is_legacy()
        || crate::process::mapped_region_identity(
            std::process::id(),
            crate::process::own_code_address(),
        )? != intent.target_image
        || !intent.original_exited()?
        || checkpoint.proof != intent.source
        || digest != intent.checkpoint_digest
        || !ready.matches_cutover_inventory(&retained.terminals, &retained.chats)
        || !crate::upgrade::recovery::verified::ready::capture()
            .map_err(invalid)?
            .is_some_and(|capture| capture.covers_only_carried_work(checkpoint))
    {
        return Err(invalid(
            "The selected parked update no longer has its exact carried checkpoint",
        ));
    }
    intent.verify_exited_claim(&ready)?;
    ready
        .verify_parked_workspace_initialization()
        .map_err(invalid)?;
    Ok(Some(intent.checkpoint_digest))
}

/// Verify the exact carried inventory before retiring a parked checkpoint.
#[cfg(feature = "host")]
pub fn verify_selected_parked_inventory(
    serving: &HostSessionProof,
    terminals: &[crate::terminal_ipc::TerminalDescriptor],
    chats: &[crate::chat::Session],
    completed: &std::collections::BTreeSet<crate::model::TerminalId>,
) -> Result<(), UpdateError> {
    let layout = locate(&crate::process::current_executable()?)?;
    let state = layout.prefix.join(".update");
    let job = read_job(&state)?.ok_or_else(|| invalid("The selected parked update has no job"))?;
    let (intent, saved) = restart_intent(&layout, &state, &job)?
        .ok_or_else(|| invalid("The selected parked update lost its witnessed terminal roster"))?;
    if !intent.recorded_parked(&state)? {
        return Err(invalid(
            "The selected update has no parked checkpoint receipt",
        ));
    }
    recovery::verify_ready_inventory(
        &job.id,
        &intent.source,
        &crate::upgrade::recovery::Census {
            terminals: saved.terminals,
            chats: saved.chats,
            ..Default::default()
        },
        serving,
        terminals,
        chats,
        &Store::discover()
            .map_err(invalid)?
            .recovered_chats()
            .map_err(invalid)?,
        completed,
    )
    .map_err(invalid)
}

/// A single snapshot of actual running owners under the serving host's lock.
#[cfg(feature = "host")]
pub struct LiveInventory {
    pub completed: std::collections::BTreeSet<crate::model::TerminalId>,
    pub terminals: Vec<crate::terminal_ipc::TerminalDescriptor>,
    pub chats: Vec<crate::chat::Session>,
    pub adopted_sources: std::collections::BTreeMap<
        crate::model::TerminalId,
        Vec<crate::upgrade::handoff::ChatSource>,
    >,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginalInventory {
    terminals: Vec<crate::model::TerminalSession>,
    chats: Vec<crate::chat::Session>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Witness {
    version: u16,
    intent_digest: String,
    inventory_digest: String,
    inventory: OriginalInventory,
}

const INVENTORY_LIMIT: u64 = crate::upgrade::recovery::STANDBY_RECEIPT_LIMIT as u64;

pub(super) fn persist(
    state: &Path,
    target: &Path,
    bytes: &[u8],
    exclusive: bool,
) -> Result<(), UpdateError> {
    let temporary = state.join(format!(".cutover-{}", uuid::Uuid::new_v4()));
    let saved = (|| {
        let mut file = private_create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if exclusive {
            let root = crate::fs_safety::Root::open(state)?;
            crate::fs_safety::rename_new_within(
                &root,
                temporary.file_name().expect("cutover temporary name"),
                target.file_name().expect("cutover file name"),
            )
            .map_err(|error| invalid(error.to_string()))?;
        } else {
            fs::rename(&temporary, target)?;
        }
        File::open(state)?.sync_all()?;
        Ok(())
    })();
    if saved.is_err() {
        let _ = fs::remove_file(temporary);
    }
    saved
}

#[cfg(all(target_os = "macos", feature = "host"))]
pub(crate) fn persist_ready_evidence(path: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    let state = path
        .parent()
        .ok_or_else(|| invalid("Ready claim has no update directory"))?;
    persist(state, path, bytes, true)
}

fn path(state: &Path, job: &str) -> PathBuf {
    state.join(format!("{job}.cutover.json"))
}

pub(super) fn read_job(state: &Path) -> Result<Option<UpdateJob>, UpdateError> {
    match read_private(&state.join("status.json"), JOB_STATE_LIMIT) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read(state: &Path, job: &str) -> Result<Option<Intent>, UpdateError> {
    if uuid::Uuid::parse_str(job).is_err() {
        return Err(invalid("Invalid cutover job"));
    }
    match read_private(&path(state, job), JOB_STATE_LIMIT) {
        Ok(bytes) => {
            let intent: Intent = serde_json::from_slice(&bytes)?;
            intent.validate()?;
            if intent.job != job {
                return Err(invalid("Installed cutover belongs to a different job"));
            }
            Ok(Some(intent))
        }
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl Intent {
    fn pending_digest(&self) -> Result<String, UpdateError> {
        let mut pending = self.clone();
        pending.stage = Stage::Pending;
        hashed_json(&pending)
    }

    fn parked_path(&self, state: &Path) -> Result<PathBuf, UpdateError> {
        let digest = self.pending_digest()?;
        Ok(state.join(format!("{}.cutover-parked.{digest}.json", self.job)))
    }

    fn recorded_parked(&self, state: &Path) -> Result<bool, UpdateError> {
        match read_private(&self.parked_path(state)?, SMALL_ASSET_LIMIT) {
            Ok(bytes) if bytes == serde_json::to_vec(&self.pending_digest()?)? => Ok(true),
            Ok(_) => Err(invalid("The installed parked checkpoint receipt changed")),
            Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn witness_path(&self, state: &Path) -> Result<PathBuf, UpdateError> {
        let job = &self.job;
        Ok(match self.witness {
            WitnessLocation::Legacy => state.join(format!("{job}.cutover-inventory.json")),
            WitnessLocation::Attempt => {
                let digest = self.pending_digest()?;
                state.join(format!("{job}.cutover-inventory.{digest}.json"))
            }
        })
    }

    fn witness_present(&self, state: &Path) -> Result<bool, UpdateError> {
        match fs::symlink_metadata(self.witness_path(state)?) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn validate(&self) -> Result<(), UpdateError> {
        let hash = |text: &str| text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit());
        if uuid::Uuid::parse_str(&self.job).is_err()
            || !hash(&self.request_digest)
            || !hash(&self.checkpoint_digest)
            || !self.target.is_absolute()
            || !self.source.is_complete()
            || !self.successor.is_complete()
            || self.source == self.successor
            || self.source.boot_id != self.successor.boot_id
            || self.process.pid <= 1
            || self.process.start == 0
            || self.windows.len() > 1024
            || self
                .windows
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.windows.len()
        {
            return Err(invalid("Invalid installed cutover evidence"));
        }
        for slot in &self.windows {
            crate::instance::validate_window_slot(slot)?;
        }
        Ok(())
    }

    fn verify_request(&self, state: &Path) -> Result<(), UpdateError> {
        let request = read_private(
            &state.join(format!("{}.request", self.job)),
            SMALL_ASSET_LIMIT,
        )?;
        let digest = Sha256::digest(request);
        if format!("{digest:x}") != self.request_digest {
            return Err(invalid("Installed cutover request changed"));
        }
        Ok(())
    }

    fn names_selected(&self, layout: &InstallLayout) -> Result<bool, UpdateError> {
        if self.target != layout.version_root.join("bin/cagentsd")
            || served_version_root(layout)? != layout.version_root
        {
            return Ok(false);
        }
        self.names_target(layout)
    }

    fn names_target(&self, layout: &InstallLayout) -> Result<bool, UpdateError> {
        let daemon = layout.version_root.join("bin/cagentsd");
        if self.target != daemon {
            return Ok(false);
        }
        let named = fs::symlink_metadata(daemon)?;
        let ui = fs::symlink_metadata(layout.version_root.join("bin/cagents-ui"))?;
        Ok(named.is_file()
            && (named.dev(), named.ino()) == self.target_image
            && ui.is_file()
            && (ui.dev(), ui.ino()) == self.window_image)
    }

    #[cfg(feature = "host")]
    fn verify_serving(
        &self,
        layout: &InstallLayout,
        serving: &HostSessionProof,
    ) -> Result<source::Client, UpdateError> {
        if !self.names_selected(layout)?
            || crate::process::mapped_region_identity(
                std::process::id(),
                crate::process::own_code_address(),
            )? != self.target_image
        {
            return Err(invalid("The restored build is no longer selected"));
        }
        let mut client =
            source::Client::connect_matched(&crate::terminal_ipc::daemon_socket_path()?)
                .map_err(|error| invalid(error.to_string()))?;
        if client.proof() != serving
            || client.peer_pid() != std::process::id()
            || client.serving_executable()? != self.target
            || client.startup_state()? != StartupState::Ready
        {
            return Err(invalid(
                "The selected service no longer owns the restored work",
            ));
        }
        Ok(client)
    }

    fn original_alive(&self) -> Result<bool, UpdateError> {
        // A missing process or a recycled PID can settle only within the
        // recorded boot. Any observation error keeps the original reserved.
        match crate::daemon::holder_registry::Process::read(self.process.pid) {
            Ok(current) => Ok(current == self.process),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn original_exited(&self) -> Result<bool, UpdateError> {
        Ok(crate::pinned_process::exited_since(
            self.process.pid as u32,
            self.process.start,
        )?)
    }

    fn verify_selected_ready_claim(
        &self,
        layout: &InstallLayout,
        state: &Path,
        job: &UpdateJob,
    ) -> Result<bool, UpdateError> {
        if self.witness.is_legacy() {
            return Ok(false);
        }
        let Some(ready) = ready_plan::selected_evidence(layout, state, job)? else {
            return Ok(false);
        };
        self.verify_exited_claim(&ready)?;
        Ok(true)
    }

    fn verify_exited_claim(
        &self,
        ready: &crate::upgrade::recovery::ReadyPlanEvidence<'_>,
    ) -> Result<(), UpdateError> {
        ready
            .verify_exited_cutover_claim(
                &self.request_digest,
                &self.checkpoint_digest,
                &self.source,
                &self.windows,
                &self.successor,
                &self.process,
            )
            .map_err(invalid)
    }

    #[cfg(feature = "host")]
    fn verify_resumed_inventory(
        &self,
        saved: OriginalInventory,
        serving: &HostSessionProof,
        mut inventory: LiveInventory,
        state: &Path,
    ) -> Result<(), UpdateError> {
        let store = crate::upgrade::handoff::Store::discover().map_err(invalid)?;
        let mut lineage = store.recovered_chats().map_err(invalid)?;
        let mut terminals = inventory.terminals;
        terminals.retain(|terminal| {
            saved
                .terminals
                .iter()
                .any(|source| source.id == terminal.session.id)
        });
        let mut chats = inventory.chats;
        chats.retain(|chat| saved.chats.iter().any(|source| source.id == chat.id));
        for chat in &chats {
            if chat.native_continuity.retains_provider()
                && let Some(sources) = inventory.adopted_sources.remove(&chat.id)
            {
                lineage.observe_live_adoption(chat, sources);
            }
        }
        if !self.recorded_parked(state)? {
            inventory.completed.clear();
        }
        recovery::verify_ready_inventory(
            &self.job,
            &self.source,
            &crate::upgrade::recovery::Census {
                terminals: saved.terminals,
                chats: saved.chats,
                ..Default::default()
            },
            serving,
            &terminals,
            &chats,
            &lineage,
            &inventory.completed,
        )
        .map_err(invalid)
    }

    #[cfg(any(feature = "host", test))]
    fn persist(&self, state: &Path) -> Result<(), UpdateError> {
        persist(
            state,
            &path(state, &self.job),
            &serde_json::to_vec(self)?,
            false,
        )
    }

    fn read_witness(&self, state: &Path) -> Result<OriginalInventory, UpdateError> {
        let bytes = read_private(&self.witness_path(state)?, INVENTORY_LIMIT)?;
        let witness: Witness = serde_json::from_slice(&bytes)?;
        if witness.version != 1
            || witness.intent_digest != self.pending_digest()?
            || witness.inventory_digest != hashed_json(&witness.inventory)?
        {
            return Err(invalid("The original cutover inventory changed"));
        }
        Ok(witness.inventory)
    }

    #[cfg(any(feature = "host", test))]
    fn publish_with_witness(
        mut self,
        state: &Path,
        inventory: OriginalInventory,
        retry_of: Option<(&HostSessionProof, &crate::daemon::holder_registry::Process)>,
    ) -> Result<(), UpdateError> {
        let recorded = read(state, &self.job)?;
        let mut replace = false;
        if let Some(recorded) = &recorded {
            self.witness = recorded.witness;
            if self.pending_digest()? != recorded.pending_digest()? {
                let Some((proof, process)) = retry_of else {
                    return Err(invalid("An exact cutover intent is already recorded"));
                };
                let mut original = self.clone();
                original.successor = proof.clone();
                original.process = process.clone();
                if recorded.stage != Stage::Pending
                    || recorded.witness.is_legacy()
                    || original.pending_digest()? != recorded.pending_digest()?
                    || hashed_json(&recorded.read_witness(state)?)? != hashed_json(&inventory)?
                {
                    return Err(invalid(
                        "The recorded cutover is not the exact unpublished predecessor",
                    ));
                }
                replace = true;
            }
        }
        let witness = self.encode_witness(inventory)?;
        let witness_path = self.witness_path(state)?;
        if self.witness_present(state)? {
            if read_private(&witness_path, INVENTORY_LIMIT)? != witness {
                return Err(invalid("An exact cutover inventory is already recorded"));
            }
        } else {
            persist(state, &witness_path, &witness, true)?;
        }
        if replace || recorded.is_none() {
            // The intent is the reference to a complete immutable witness.
            // A crash before publishing it leaves no reference to this attempt.
            persist(
                state,
                &path(state, &self.job),
                &serde_json::to_vec(&self)?,
                !replace,
            )?;
        }
        Ok(())
    }

    #[cfg(any(feature = "host", test))]
    fn encode_witness(&self, inventory: OriginalInventory) -> Result<Vec<u8>, UpdateError> {
        let bytes = serde_json::to_vec(&Witness {
            version: 1,
            intent_digest: self.pending_digest()?,
            inventory_digest: hashed_json(&inventory)?,
            inventory,
        })?;
        if bytes.len() as u64 > INVENTORY_LIMIT {
            return Err(invalid(
                "The original cutover inventory exceeds the evidence limit",
            ));
        }
        Ok(bytes)
    }
}

fn hashed_json(value: &impl Serialize) -> Result<String, UpdateError> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

/// Written with the transferred worker lease and launch fence held, after
/// verifying all original owners. The selector remains the commit point.
#[cfg(feature = "host")]
pub(crate) fn record(
    job: &str,
    request_digest: &str,
    checkpoint_digest: &str,
    target: &Path,
    original: Original<'_>,
    successor: &HostSessionProof,
    windows: &[String],
) -> Result<(), UpdateError> {
    let layout = locate(target)?;
    // The admission also verified the original selector under the launch
    // fence. A selected attempt must never be replaced, even by that retry.
    if original.parked_ready && original.restored_checkpoint_digest.is_some() {
        return Err(invalid("A parked cutover has not restored its checkpoint"));
    }
    if original.retry_of.is_some() && served_version_root(&layout)? == layout.version_root {
        return Err(invalid("The recorded cutover is already selected"));
    }
    let state = state_dir(&layout.prefix)?;
    let expected = read_job(&state)?.ok_or_else(|| invalid("The cutover job is missing"))?;
    if expected.id != job
        || expected.version != layout.version
        || expected.phase != UpdatePhase::Installing
    {
        return Err(invalid(
            "The installed cutover no longer belongs to this update",
        ));
    }
    let target_image = crate::process::mapped_region_identity(
        std::process::id(),
        crate::process::own_code_address(),
    )?;
    let ui = fs::symlink_metadata(layout.version_root.join("bin/cagents-ui"))?;
    let intent = Intent {
        job: job.into(),
        request_digest: request_digest.into(),
        checkpoint_digest: checkpoint_digest.into(),
        target: target.into(),
        target_image,
        window_image: (ui.dev(), ui.ino()),
        source: original.source.clone(),
        successor: successor.clone(),
        process: crate::daemon::holder_registry::Process::read(std::process::id() as i32)?,
        windows: windows.to_vec(),
        stage: Stage::Pending,
        witness: WitnessLocation::Attempt,
    };
    intent.validate()?;
    intent.verify_request(&state)?;
    let inventory = OriginalInventory {
        terminals: original.inventory.terminals.clone(),
        chats: original.inventory.chats.clone(),
    };
    if let Some(digest) = original.restored_checkpoint_digest {
        let store = Store::observe_existing().map_err(invalid)?.ok_or_else(|| {
            invalid("The verified restored checkpoint disappeared before selection")
        })?;
        let checkpoint = store
            .checkpoint_status()
            .map_err(invalid)?
            .filter(|checkpoint| {
                checkpoint.phase == Phase::Restoring
                    && checkpoint.proof == intent.source
                    && checkpoint.digest == digest
            })
            .ok_or_else(|| invalid("The verified restored checkpoint changed before selection"))?;
        let record = RestoredCheckpoint {
            version: 1,
            intent_digest: intent.pending_digest()?,
            restoring_digest: checkpoint.digest,
        };
        let path = restored_checkpoint_path(&state, &intent.job);
        intent.publish_with_witness(&state, inventory, original.retry_of)?;
        persist(&state, &path, &serde_json::to_vec(&record)?, true)
    } else if original.parked_ready {
        intent
            .clone()
            .publish_with_witness(&state, inventory, original.retry_of)?;
        let path = intent.parked_path(&state)?;
        persist(
            &state,
            &path,
            &serde_json::to_vec(&intent.pending_digest()?)?,
            true,
        )
    } else {
        intent.publish_with_witness(&state, inventory, original.retry_of)
    }
}

pub(super) fn recorded_selection(
    layout: &InstallLayout,
    state: &Path,
    job: &UpdateJob,
) -> Result<Option<Stage>, UpdateError> {
    let Some(intent) = read(state, &job.id)? else {
        return Ok(None);
    };
    if !intent.names_selected(layout)? || !intent.witness_present(state)? {
        return Ok(None);
    }
    intent.read_witness(state)?;
    Ok(Some(intent.stage))
}

/// Both launchers and the selected daemon recheck the exact committed Ready
/// attempt after the previous claimant has exited.
fn restart_intent(
    layout: &InstallLayout,
    state: &Path,
    job: &UpdateJob,
) -> Result<Option<(Intent, OriginalInventory)>, UpdateError> {
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed)
        || layout.version != job.version
    {
        return Ok(None);
    }
    let Some(intent) = read(state, &job.id)? else {
        return Ok(None);
    };
    if intent.stage != Stage::Pending
        || !intent.names_selected(layout)?
        || !intent.original_exited()?
        || !intent.witness_present(state)?
        || !intent.verify_selected_ready_claim(layout, state, job)?
    {
        return Ok(None);
    }
    intent.verify_request(state)?;
    let original = intent.read_witness(state)?;
    Ok(Some((intent, original)))
}

/// A Ready claimant verified the live inventory before selection. This
/// receipt permits closing the exact completed checkpoint after selection,
/// without making its launch entries available for a second restore.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoredCheckpoint {
    version: u16,
    intent_digest: String,
    restoring_digest: String,
}

fn restored_checkpoint_path(state: &Path, job: &str) -> PathBuf {
    state.join(format!("{job}.cutover-restored.json"))
}

fn checkpoint_retirement_matches(
    state: &Path,
    intent: &Intent,
    checkpoint: &CheckpointStatus,
) -> Result<bool, UpdateError> {
    if checkpoint.phase != Phase::Restoring || checkpoint.proof != intent.source {
        return Ok(false);
    }
    let bytes = match read_private(
        &restored_checkpoint_path(state, &intent.job),
        SMALL_ASSET_LIMIT,
    ) {
        Ok(bytes) => bytes,
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let retired: RestoredCheckpoint = serde_json::from_slice(&bytes)?;
    Ok(retired.version == 1
        && retired.intent_digest == intent.pending_digest()?
        && retired.restoring_digest == checkpoint.digest)
}

/// Runs under the selected daemon's canonical saved-data and runtime claims,
/// before ordinary recovery may read or act on checkpoint contents.
#[cfg(feature = "host")]
pub fn retire_completed_checkpoint() -> Result<(), UpdateError> {
    let Some(store) = Store::observe_existing().map_err(invalid)? else {
        return Ok(());
    };
    let Some(checkpoint) = store.checkpoint_status().map_err(invalid)? else {
        return Ok(());
    };
    if checkpoint.phase != Phase::Restoring {
        return Ok(());
    }
    let executable = crate::process::current_executable()?;
    let layout = match locate(&executable) {
        Ok(layout) => layout,
        Err(UpdateError::UnsupportedInstallation) => return Ok(()),
        Err(error) => return Err(error),
    };
    let state = layout.prefix.join(".update");
    let Some(job) = read_job(&state)? else {
        return Ok(());
    };
    let Some((intent, _)) = restart_intent(&layout, &state, &job)? else {
        return Ok(());
    };
    if crate::process::mapped_region_identity(
        std::process::id(),
        crate::process::own_code_address(),
    )? == intent.target_image
        && checkpoint_retirement_matches(&state, &intent, &checkpoint)?
        && store.checkpoint_status().map_err(invalid)?.as_ref() == Some(&checkpoint)
    {
        store.remove().map_err(invalid)?;
    }
    Ok(())
}

/// Continue selection only when the same claimant durably verified the
/// restored inventory before exiting. The caller holds updater, account,
/// launch, and runtime custody and rechecks the original selector.
pub(super) fn verify_completed_preselection(
    layout: &InstallLayout,
    state: &Path,
    job: &UpdateJob,
    ready: &crate::upgrade::recovery::ReadyPlanEvidence<'_>,
    runtime: &crate::daemon::lease_handoff::LeaseHandoff,
    standby_pid: Option<u32>,
) -> Result<bool, UpdateError> {
    let Some(intent) = read(state, &job.id)? else {
        return Ok(false);
    };
    if intent.stage != Stage::Pending
        || intent.witness.is_legacy()
        || intent.recorded_parked(state)?
        || standby_pid.is_some_and(|pid| intent.process.pid as u32 != pid)
        || !intent.names_target(layout)?
        || !intent.original_exited()?
    {
        return Ok(false);
    }
    let Some(store) = Store::observe_existing().map_err(invalid)? else {
        return Ok(false);
    };
    let Some(checkpoint) = store.checkpoint_status().map_err(invalid)? else {
        return Ok(false);
    };
    if !checkpoint_retirement_matches(state, &intent, &checkpoint)? {
        return Ok(false);
    }
    intent.verify_request(state)?;
    let inventory = intent.read_witness(state)?;
    if !ready.matches_cutover_inventory(&inventory.terminals, &inventory.chats) {
        return Err(invalid(
            "The completed cutover inventory differs from the acknowledged census",
        ));
    }
    ready
        .verify_exited_cutover_claim(
            &intent.request_digest,
            &intent.checkpoint_digest,
            &intent.source,
            &intent.windows,
            &intent.successor,
            &intent.process,
        )
        .map_err(invalid)?;
    ready
        .verify_completed_runtime_owner(runtime, &intent.successor, &intent.process)
        .map_err(invalid)?;
    Ok(true)
}

/// A receipt permits delegating to the staged worker. Only that worker
/// may verify the full recorded restore and exchange the selector.
pub(super) fn has_restored_checkpoint_receipt(
    state: &Path,
    job: &str,
) -> Result<bool, UpdateError> {
    match read_private(&restored_checkpoint_path(state, job), SMALL_ASSET_LIMIT) {
        Ok(_) => Ok(true),
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) struct SelectedRestart {
    cli: PathBuf,
    intent: Intent,
}

pub(super) fn restart_after_worker(
    installation: &Installation,
    state: &Path,
    job: &UpdateJob,
) -> Result<Option<SelectedRestart>, UpdateError> {
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed) {
        return Ok(None);
    }
    let layout = locate(&installation.prefix.join("current/bin/cagents"))?;
    let Some((intent, _)) = restart_intent(&layout, state, job)? else {
        return Ok(None);
    };
    let Some(store) = Store::observe_existing().map_err(invalid)? else {
        return Ok(None);
    };
    if let Some(checkpoint) = store.checkpoint_status().map_err(invalid)?
        && !(intent.recorded_parked(state)?
            && checkpoint.proof == intent.source
            && checkpoint.phase == Phase::Ready
            && checkpoint.digest == intent.checkpoint_digest)
        && !checkpoint_retirement_matches(state, &intent, &checkpoint)?
    {
        return Ok(None);
    }
    Ok(Some(SelectedRestart {
        cli: layout.version_root.join("bin/cagents"),
        intent,
    }))
}

fn restart_failure_path(state: &Path, job: &str) -> PathBuf {
    state.join(format!("{job}.cutover-start-failed.json"))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartFailure {
    intent_digest: String,
    message: String,
}

/// A launch failure is a status verdict, not a change to the selected
/// update's authority. A later ordinary launch can still settle the job.
pub(super) fn selected_start_failure(
    state: &Path,
    job: &UpdateJob,
) -> Result<Option<String>, UpdateError> {
    let bytes = match read_private(&restart_failure_path(state, &job.id), SMALL_ASSET_LIMIT) {
        Ok(bytes) => bytes,
        Err(UpdateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(intent) = read(state, &job.id)? else {
        return Ok(None);
    };
    let failed: RestartFailure = serde_json::from_slice(&bytes)?;
    Ok((failed.intent_digest == intent.pending_digest()?).then_some(failed.message))
}

impl SelectedRestart {
    /// The caller has closed the worker and handoff descriptions before this
    /// ordinary launch can obtain its own runtime and saved-data leases.
    pub(super) fn launch(self, state: &Path) -> Result<(), UpdateError> {
        let mut command = Command::new(self.cli);
        command
            .args(["--json", "daemon", "start"])
            .stdin(Stdio::null());
        crate::install_guard::sanitize_child_environment(&mut command);
        // SAFETY: the hook seals inherited descriptors before exec.
        unsafe { command.pre_exec(crate::daemon::seal_inherited_descriptors) };
        let failure = match command.status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => format!("Selected daemon restart command reported {status}"),
            Err(error) => format!("Could not start the selected daemon: {error}"),
        };
        // A manual launch could have won recovery during the attempt. Keep
        // any verdict that has already moved the exact job past this stage.
        let _worker = crate::instance::owned_file_lock(open_worker_lock(state)?, libc::LOCK_EX)?;
        if let Some(job) = read_job(state)?
            && job.id == self.intent.job
            && matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed)
            && let Some(current) = read(state, &job.id)?
            && current.stage == Stage::Pending
            && current.pending_digest()? == self.intent.pending_digest()?
        {
            let receipt = RestartFailure {
                intent_digest: self.intent.pending_digest()?,
                message: failure.clone(),
            };
            persist(
                state,
                &restart_failure_path(state, &job.id),
                &serde_json::to_vec(&receipt)?,
                false,
            )?;
            // Wake existing readers of status.json. The verdict and message
            // travel together in the receipt they read on the next snapshot.
            let mut job = job;
            job.phase = UpdatePhase::Failed;
            write_job(state, &job)?;
        }
        Err(invalid(failure))
    }
}

/// Recover only previously confirmed Codex threads for the exact selected
/// Ready update. Called under runtime and saved-data leases after ordinary
/// holder discovery and interrupted run settlement.
#[cfg(feature = "host")]
pub fn selected_reconnectable_codex() -> Result<Vec<crate::upgrade::handoff::Chat>, UpdateError> {
    let Some(restart) = pending_restart(&crate::process::current_executable()?)? else {
        return Ok(Vec::new());
    };
    if crate::process::mapped_region_identity(
        std::process::id(),
        crate::process::own_code_address(),
    )? != restart.intent.target_image
    {
        return Ok(Vec::new());
    }
    let store = Store::observe_existing()
        .map_err(invalid)?
        .ok_or_else(|| invalid("Selected Ready recovery is missing its saved work directory"))?;
    restart.reconnectable_codex(&store)
}

struct PendingRestart {
    state: PathBuf,
    intent: Intent,
    original: OriginalInventory,
}

impl PendingRestart {
    fn reconnectable_codex(
        &self,
        store: &Store,
    ) -> Result<Vec<crate::upgrade::handoff::Chat>, UpdateError> {
        let recovered = store.recovered_chats().map_err(invalid)?;
        Ok(recovered.reconnectable_codex(&self.original.chats, &self.intent.successor))
    }

    fn startup_allowance(&self) -> Result<Duration, UpdateError> {
        let Some(store) = Store::observe_existing().map_err(invalid)? else {
            return Ok(Duration::ZERO);
        };
        // Only the ordinary recovery path can do this work. A checkpoint
        // scheduled for retirement may still be present when a launcher reads it.
        if let Some(checkpoint) = store.checkpoint_status().map_err(invalid)?
            && !checkpoint_retirement_matches(&self.state, &self.intent, &checkpoint)?
        {
            return Ok(Duration::ZERO);
        }
        Ok(crate::upgrade::handoff::selected_reconnect_allowance(
            self.reconnectable_codex(&store)?.len(),
        ))
    }
}

fn pending_restart(executable: &Path) -> Result<Option<PendingRestart>, UpdateError> {
    let layout = match locate(executable) {
        Ok(layout) => layout,
        Err(UpdateError::UnsupportedInstallation) => return Ok(None),
        Err(error) => return Err(error),
    };
    let state = layout.prefix.join(".update");
    let Some(job) = read_job(&state)? else {
        return Ok(None);
    };
    let Some((intent, original)) = restart_intent(&layout, &state, &job)? else {
        return Ok(None);
    };
    Ok(Some(PendingRestart {
        state,
        intent,
        original,
    }))
}

/// A deadline estimate grants no recovery authority. Invalid or already
/// settled selected evidence leaves the ordinary checkpoint budget in effect.
pub(crate) fn selected_startup_allowance(executable: &Path) -> Duration {
    pending_restart(executable)
        .ok()
        .flatten()
        .and_then(|restart| restart.startup_allowance().ok())
        .unwrap_or_default()
}

/// Joining a listener must budget for the daemon at the other end, including
/// when the caller is an editor companion from a different installation.
pub(crate) fn selected_listener_startup_allowance(
    socket: &Path,
    peer: crate::daemon::PeerCredentials,
) -> Duration {
    let allowance = || {
        let paths = crate::daemon::RuntimePaths::discover().ok()?;
        if paths.socket != socket || peer.uid != crate::daemon::current_uid() {
            return None;
        }
        let process = crate::daemon::lease_handoff::observe_owner(&paths).ok()?;
        if process.pid() != peer.pid {
            return None;
        }
        let executable = process.executable().ok()?;
        let restart = pending_restart(&executable).ok()??;
        if crate::process::mapped_executable_identity(peer.pid, &executable).ok()?
            != restart.intent.target_image
            || process.exited().ok()?
        {
            return None;
        }
        restart.startup_allowance().ok()
    };
    allowance().unwrap_or_default()
}

/// An unrelated job must not replace the unfinished status between the
/// coordinator releasing its lease and this successor acquiring its own.
pub(super) fn pending_selected(
    layout: &InstallLayout,
    state: &Path,
    job: &UpdateJob,
) -> Result<bool, UpdateError> {
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed)
        || layout.version != job.version
    {
        return Ok(false);
    }
    let Some(intent) = read(state, &job.id)? else {
        return Ok(false);
    };
    if intent.stage != Stage::Pending
        || !intent.names_selected(layout)?
        || crate::daemon::read_boot_id()? != intent.successor.boot_id
    {
        return Ok(false);
    }
    // Keep a recoverable job reserved until another exact selected daemon
    // can finish it, even if the first successor no longer exists.
    if intent.witness_present(state)? {
        return Ok(true);
    }
    intent.original_alive()
}

#[cfg(feature = "host")]
pub(super) fn finish(
    layout: &InstallLayout,
    state: &Path,
    successor: &HostSessionProof,
    mut job: UpdateJob,
    entry_checkpoint: StartupCheckpoint,
    observe_live: impl FnOnce() -> LiveInventory,
) -> Result<Option<String>, UpdateError> {
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed)
        || layout.version != job.version
    {
        return Ok(None);
    }
    let Some(mut intent) = read(state, &job.id)? else {
        return Ok(None);
    };
    if intent.stage != Stage::Pending
        || successor.boot_id != intent.successor.boot_id
        || !intent.names_selected(layout)?
    {
        return Ok(None);
    }
    let reentered = &intent.successor != successor;
    if reentered && (!intent.witness_present(state)? || !intent.original_exited()?) {
        return Ok(None);
    }
    if reentered {
        intent.verify_selected_ready_claim(layout, state, &job)?;
    }
    if !reentered && intent.process.pid != std::process::id() as i32 {
        return Ok(None);
    }
    let saved = intent.read_witness(state)?;
    let finished = (|| {
        intent.verify_request(state)?;
        let mut client = intent.verify_serving(layout, successor)?;
        if reentered {
            let adoption_only = match entry_checkpoint {
                StartupCheckpoint::Absent => true,
                StartupCheckpoint::AdoptionOnly {
                    source,
                    ready_digest,
                    ..
                } => source == intent.source && ready_digest == intent.checkpoint_digest,
                StartupCheckpoint::ParkedReady { source, digest } => {
                    intent.recorded_parked(state)?
                        && source == intent.source
                        && digest == intent.checkpoint_digest
                }
                StartupCheckpoint::Unsettled => false,
            };
            if !adoption_only || recovery::checkpoint_status()?.is_some() {
                return Err(invalid(
                    "Saved work had not finished reopening before the daemon restarted",
                ));
            }
            let generation = client.inventory_generation()?;
            intent.verify_resumed_inventory(saved, successor, observe_live(), state)?;
            if client.inventory_generation()? != generation {
                return Err(invalid(
                    "The running work changed during update verification",
                ));
            }
        }
        finish_windows(&intent, layout, successor, state, &job)
    })();
    job.confirmation = None;
    job.refusal = None;
    match finished {
        Ok(()) => {
            job.phase = UpdatePhase::Complete;
            job.message = "Update installed; saved windows reopened".into();
        }
        Err(ref error) => {
            job.phase = UpdatePhase::Failed;
            job.message = format!(
                "The new build is installed, but reopening its saved windows could not be confirmed. {error}"
            );
        }
    }
    write_job(state, &job)?;
    if let Err(error) = &finished {
        if intent.recorded_parked(state)? && recovery::checkpoint_status()?.is_some() {
            let receipt = RestartFailure {
                intent_digest: intent.pending_digest()?,
                message: error.record_message(),
            };
            persist(
                state,
                &restart_failure_path(state, &job.id),
                &serde_json::to_vec(&receipt)?,
                false,
            )?;
        } else {
            intent.stage = Stage::Deferred;
            intent.persist(state)?;
        }
    }
    retention::remember_success(state, &job.id, &job.phase);
    finished.map(|()| Some(job.id))
}

#[cfg(feature = "host")]
fn finish_windows(
    intent: &Intent,
    layout: &InstallLayout,
    serving: &HostSessionProof,
    state: &Path,
    job: &UpdateJob,
) -> Result<(), UpdateError> {
    let ui = layout.version_root.join("bin/cagents-ui");
    if !intent.windows.is_empty() {
        let mut guard = crate::instance::UiUpdateGuard::acquire_for_cutover()?;
        guard.set_recovery_restore_only();
        let launched = guard.relaunch_missing_windows(&intent.windows, &ui);
        // A natural launch can legitimately win a slot after we release the
        // fence. An authenticated Ready owner settles that slot either way.
        let verified = crate::instance::UiUpdateGuard::acquire_for_cutover()
            .and_then(|guard| guard.verify_cutover_windows(&intent.windows, &ui));
        if let Err(error) = verified {
            return Err(windows::reopen_failure(
                state,
                job,
                launched.err().unwrap_or(error),
            ));
        }
    }
    intent.verify_serving(layout, serving).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_inventory() -> OriginalInventory {
        OriginalInventory {
            terminals: Vec::new(),
            chats: Vec::new(),
        }
    }

    fn intent() -> Intent {
        let source = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("original-ready"),
            boot_id: crate::model::BootId::new("same-boot"),
            host_incarnation: crate::model::HostIncarnation::new("original"),
        };
        Intent {
            job: uuid::Uuid::new_v4().to_string(),
            request_digest: "a".repeat(64),
            checkpoint_digest: "b".repeat(64),
            target: "/private/install/versions/0.2.4/bin/cagentsd".into(),
            target_image: (42, 12),
            window_image: (42, 14),
            successor: HostSessionProof {
                host_incarnation: crate::model::HostIncarnation::new("replacement"),
                ..source.clone()
            },
            source,
            process: crate::daemon::holder_registry::Process { pid: 42, start: 17 },
            windows: vec!["main".into()],
            stage: Stage::Pending,
            witness: WitnessLocation::Legacy,
        }
    }

    #[test]
    fn completed_checkpoint_retirement_matches_the_selected_attempt_and_exact_bytes() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let state = directory.path();
        let selected = intent();
        let mut checkpoint = CheckpointStatus {
            proof: selected.source.clone(),
            phase: Phase::Restoring,
            digest: "c".repeat(64),
        };
        let eligible =
            |intent, checkpoint| checkpoint_retirement_matches(state, intent, checkpoint).unwrap();
        assert!(!eligible(&selected, &checkpoint));
        let record = RestoredCheckpoint {
            version: 1,
            intent_digest: selected.pending_digest().unwrap(),
            restoring_digest: checkpoint.digest.clone(),
        };
        persist(
            state,
            &restored_checkpoint_path(state, &selected.job),
            &serde_json::to_vec(&record).unwrap(),
            true,
        )
        .unwrap();
        assert!(eligible(&selected, &checkpoint));

        let mut retry = selected.clone();
        retry.process.start += 1;
        assert!(!eligible(&retry, &checkpoint));
        checkpoint.phase = Phase::Ready;
        assert!(!eligible(&selected, &checkpoint));
        checkpoint.phase = Phase::Restoring;
        checkpoint.proof = selected.successor.clone();
        assert!(!eligible(&selected, &checkpoint));
        checkpoint.proof = selected.source.clone();
        checkpoint.digest = "b".repeat(64);
        assert!(!eligible(&selected, &checkpoint));
    }

    #[test]
    fn selected_launch_failure_marks_only_its_exact_attempt() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let state = directory.path();
        let mut selected = intent();
        let job = UpdateJob {
            id: selected.job.clone(),
            version: "0.2.4".into(),
            phase: UpdatePhase::Failed,
            message: String::new(),
            log_path: state.join("update.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        selected.persist(state).unwrap();
        assert_eq!(selected_start_failure(state, &job).unwrap(), None);
        let receipt = RestartFailure {
            intent_digest: selected.pending_digest().unwrap(),
            message: "Selected daemon launch failed".into(),
        };
        persist(
            state,
            &restart_failure_path(state, &job.id),
            &serde_json::to_vec(&receipt).unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(
            selected_start_failure(state, &job).unwrap(),
            Some(receipt.message)
        );

        selected.process.start += 1;
        selected.persist(state).unwrap();
        assert_eq!(selected_start_failure(state, &job).unwrap(), None);
    }

    #[test]
    fn selector_intent_requires_the_exact_same_boot_scope_and_unique_windows() {
        let mut intent = intent();
        intent.validate().unwrap();
        intent.windows.push("main".into());
        assert!(intent.validate().is_err());
        *intent.windows.last_mut().unwrap() = "../main".into();
        assert!(intent.validate().is_err());
        intent.windows.pop();
        intent.successor = intent.source.clone();
        assert!(intent.validate().is_err());
        intent.successor.boot_id = crate::model::BootId::new("another-boot");
        assert!(intent.validate().is_err());
    }

    #[test]
    fn an_unreferenced_witness_never_blocks_another_authorized_attempt() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let state = directory.path();
        let mut first = intent();
        first.witness = WitnessLocation::Attempt;
        let unreferenced = first.witness_path(state).unwrap();
        let first_witness = first.encode_witness(empty_inventory()).unwrap();
        persist(state, &unreferenced, &first_witness, true).unwrap();
        assert!(read(state, &first.job).unwrap().is_none());

        let mut next = first.clone();
        next.successor.host_incarnation = crate::model::HostIncarnation::new("next");
        next.process.start += 1;
        let path = next.witness_path(state).unwrap();
        assert_ne!(unreferenced, path);
        assert!(!next.witness_present(state).unwrap());
        next.clone()
            .publish_with_witness(state, empty_inventory(), None)
            .unwrap();
        let recorded = read(state, &next.job).unwrap().unwrap();
        assert_eq!(recorded.witness_path(state).unwrap(), path);
        recorded.read_witness(state).unwrap();
        assert_eq!(fs::read(&unreferenced).unwrap(), first_witness);

        let third = first.clone();
        assert!(
            third
                .publish_with_witness(state, empty_inventory(), None)
                .is_err()
        );
        assert_eq!(
            read(state, &next.job)
                .unwrap()
                .unwrap()
                .witness_path(state)
                .unwrap(),
            path
        );
    }

    #[test]
    fn only_the_exact_pending_predecessor_can_be_replaced_by_the_authorized_retry() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let state = directory.path();
        let mut first = intent();
        first.witness = WitnessLocation::Attempt;
        first
            .clone()
            .publish_with_witness(state, empty_inventory(), None)
            .unwrap();
        let first_witness = first.witness_path(state).unwrap();
        let first_bytes = fs::read(&first_witness).unwrap();

        let mut retry = first.clone();
        retry.successor.host_incarnation = crate::model::HostIncarnation::new("retry");
        retry.process.start += 1;
        let wrong = Some((&first.successor, &retry.process));
        let exact = Some((&first.successor, &first.process));
        assert!(
            retry
                .clone()
                .publish_with_witness(state, empty_inventory(), wrong)
                .is_err()
        );

        let mut different_scope = retry.clone();
        different_scope.windows.push("other".into());
        assert!(
            different_scope
                .publish_with_witness(state, empty_inventory(), exact)
                .is_err()
        );

        let mut deferred = first.clone();
        deferred.stage = Stage::Deferred;
        deferred.persist(state).unwrap();
        assert!(
            retry
                .clone()
                .publish_with_witness(state, empty_inventory(), exact)
                .is_err()
        );
        first.persist(state).unwrap();

        fs::write(&first_witness, b"damaged witness").unwrap();
        assert!(
            retry
                .clone()
                .publish_with_witness(state, empty_inventory(), exact)
                .is_err()
        );
        fs::write(&first_witness, &first_bytes).unwrap();
        assert_eq!(
            read(state, &first.job).unwrap().unwrap().process,
            first.process
        );
        assert!(!retry.witness_present(state).unwrap());

        retry
            .clone()
            .publish_with_witness(state, empty_inventory(), exact)
            .unwrap();
        let selected = read(state, &retry.job).unwrap().unwrap();
        assert_eq!(selected.process, retry.process);
        selected.read_witness(state).unwrap();
        assert_eq!(fs::read(first_witness).unwrap(), first_bytes);
        retry
            .publish_with_witness(state, empty_inventory(), exact)
            .unwrap();
    }

    #[test]
    fn a_legacy_witness_authenticates_both_stages_and_publication_keeps_the_latest() {
        let directory = crate::test_fixture::tempdir().unwrap();
        let state = directory.path();
        let mut legacy = intent();
        let original = serde_json::to_vec(&legacy).unwrap();
        assert!(
            !serde_json::from_slice::<serde_json::Value>(&original)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("witness")
        );
        let witness = legacy.witness_path(state).unwrap();
        assert_eq!(
            witness.file_name().unwrap().to_str().unwrap(),
            format!("{}.cutover-inventory.json", legacy.job)
        );
        let inventory = empty_inventory();
        let bytes = serde_json::to_vec(&Witness {
            version: 1,
            intent_digest: format!("{:x}", Sha256::digest(&original)),
            inventory_digest: hashed_json(&inventory).unwrap(),
            inventory,
        })
        .unwrap();
        persist(state, &witness, &bytes, true).unwrap();
        persist(state, &path(state, &legacy.job), &original, true).unwrap();
        read(state, &legacy.job)
            .unwrap()
            .unwrap()
            .read_witness(state)
            .unwrap();

        legacy.stage = Stage::Deferred;
        legacy.persist(state).unwrap();
        let mut repeated = legacy.clone();
        repeated.stage = Stage::Pending;
        repeated.witness = WitnessLocation::Attempt;
        repeated
            .publish_with_witness(state, empty_inventory(), None)
            .unwrap();
        let latest = read(state, &legacy.job).unwrap().unwrap();
        assert!(latest.stage == Stage::Deferred);
        assert_eq!(latest.witness_path(state).unwrap(), witness);
        latest.read_witness(state).unwrap();
        assert_eq!(fs::read(&witness).unwrap(), bytes);

        let mut changed: Witness = serde_json::from_slice(&bytes).unwrap();
        changed.inventory_digest = "0".repeat(64);
        fs::write(&witness, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(latest.read_witness(state).is_err());
    }

    #[test]
    fn a_live_selected_intent_reserves_its_job_even_after_a_worker_failure() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().canonicalize().unwrap();
        let version = "0.2.4";
        let version_root = prefix.join("versions").join(version);
        fs::create_dir_all(version_root.join("bin")).unwrap();
        fs::write(
            prefix.join(".cagents-install-root"),
            "cagents install root\n",
        )
        .unwrap();
        for name in ["cagents", "cagentsd", "cagents-ui"] {
            File::create(version_root.join("bin").join(name)).unwrap();
        }
        std::os::unix::fs::symlink(Path::new("versions").join(version), prefix.join("current"))
            .unwrap();
        let layout = locate(&version_root.join("bin/cagentsd")).unwrap();
        let state = state_dir(&prefix).unwrap();
        let mut intent = intent();
        intent.witness = WitnessLocation::Attempt;
        intent.target = version_root.join("bin/cagentsd");
        let daemon = fs::metadata(&intent.target).unwrap();
        intent.target_image = (daemon.dev(), daemon.ino());
        let window = fs::metadata(version_root.join("bin/cagents-ui")).unwrap();
        intent.window_image = (window.dev(), window.ino());
        intent.process =
            crate::daemon::holder_registry::Process::read(std::process::id() as i32).unwrap();
        intent.successor.boot_id = crate::daemon::read_boot_id().unwrap();
        intent.source.boot_id = intent.successor.boot_id.clone();
        #[cfg(feature = "host")]
        {
            let error = record(
                &intent.job,
                &intent.request_digest,
                &intent.checkpoint_digest,
                &intent.target,
                Original {
                    source: &intent.source,
                    inventory: &crate::upgrade::recovery::Census::default(),
                    restored_checkpoint_digest: None,
                    parked_ready: false,
                    retry_of: Some((&intent.successor, &intent.process)),
                },
                &intent.successor,
                &intent.windows,
            )
            .unwrap_err();
            assert!(error.to_string().contains("already selected"));
        }
        let job = UpdateJob {
            id: intent.job.clone(),
            version: version.into(),
            phase: UpdatePhase::Failed,
            message: "Worker exited after selection".into(),
            log_path: state.join("update.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let record = path(&state, &job.id);
        private_create(&record).unwrap();
        let save = |intent: &Intent| {
            fs::write(&record, serde_json::to_vec(intent).unwrap()).unwrap();
        };
        save(&intent);
        assert!(pending_selected(&layout, &state, &job).unwrap());
        intent.stage = Stage::Deferred;
        save(&intent);
        assert!(!pending_selected(&layout, &state, &job).unwrap());
        intent.stage = Stage::Pending;
        intent.process.start += 1;
        save(&intent);
        assert!(!pending_selected(&layout, &state, &job).unwrap());
        assert!(recorded_selection(&layout, &state, &job).unwrap().is_none());
        let witness = intent.witness_path(&state).unwrap();
        private_create(&witness).unwrap();
        assert!(pending_selected(&layout, &state, &job).unwrap());
        assert!(recorded_selection(&layout, &state, &job).is_err());
        let bytes = intent.encode_witness(empty_inventory()).unwrap();
        fs::write(&witness, bytes).unwrap();
        assert!(matches!(
            recorded_selection(&layout, &state, &job).unwrap(),
            Some(Stage::Pending)
        ));
    }
}
