//! Finish an installed Ready update after its coordinator releases custody.
//! This record can admit saved windows after exact live work is verified. It
//! carries no authority to start a daemon or restore a checkpoint.
use super::*;
use crate::model::HostSessionProof;
#[cfg(feature = "host")]
use crate::terminal_ipc::StartupState;

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
        restoring_digest: String,
        ready_digest: String,
    },
    Unsettled,
}

#[cfg(feature = "host")]
impl StartupCheckpoint {
    pub fn restoring_digest(&self) -> Option<&str> {
        match self {
            Self::AdoptionOnly {
                restoring_digest, ..
            } => Some(restoring_digest),
            _ => None,
        }
    }
}

#[cfg(feature = "host")]
pub fn startup_checkpoint() -> StartupCheckpoint {
    use crate::upgrade::handoff::Store;
    let checkpoint = Store::existing()
        .and_then(|store| store.map_or(Ok(None), |store| store.load_with_digest()));
    match checkpoint {
        Ok(None) => StartupCheckpoint::Absent,
        Ok(Some((checkpoint, restoring_digest))) => {
            let source = checkpoint.proof.clone();
            match checkpoint.adoption_ready_digest() {
                Ok(Some(ready_digest)) => StartupCheckpoint::AdoptionOnly {
                    source,
                    restoring_digest,
                    ready_digest,
                },
                _ => StartupCheckpoint::Unsettled,
            }
        }
        Err(_) => StartupCheckpoint::Unsettled,
    }
}

/// A single snapshot of actual running owners under the serving host's lock.
#[cfg(feature = "host")]
pub struct LiveInventory {
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

#[cfg(any(feature = "host", target_os = "macos", test))]
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

    #[cfg(feature = "host")]
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
        let daemon = layout.version_root.join("bin/cagentsd");
        if self.target != daemon || served_version_root(layout)? != layout.version_root {
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

    #[cfg(feature = "host")]
    fn original_exited(&self) -> Result<bool, UpdateError> {
        Ok(crate::pinned_process::exited_since(
            self.process.pid as u32,
            self.process.start,
        )?)
    }

    #[cfg(feature = "host")]
    fn verify_resumed_inventory(
        &self,
        saved: OriginalInventory,
        serving: &HostSessionProof,
        mut inventory: LiveInventory,
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
    intent.publish_with_witness(&state, inventory, original.retry_of)
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
                StartupCheckpoint::Unsettled => false,
            };
            if !adoption_only || recovery::checkpoint_status()?.is_some() {
                return Err(invalid(
                    "Saved work had not finished reopening before the daemon restarted",
                ));
            }
            let generation = client.inventory_generation()?;
            intent.verify_resumed_inventory(saved, successor, observe_live())?;
            if client.inventory_generation()? != generation {
                return Err(invalid(
                    "The running work changed during update verification",
                ));
            }
        }
        finish_windows(&intent, layout, successor)
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
    if finished.is_err() {
        intent.stage = Stage::Deferred;
        intent.persist(state)?;
    }
    retention::remember_success(state, &job.id, &job.phase);
    finished.map(|()| Some(job.id))
}

#[cfg(feature = "host")]
fn finish_windows(
    intent: &Intent,
    layout: &InstallLayout,
    serving: &HostSessionProof,
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
        if verified.is_err() {
            launched?;
            verified?;
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
