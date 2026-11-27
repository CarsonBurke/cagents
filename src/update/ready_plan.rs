//! Durable inputs for the installed updater's exact Ready activation.
use super::*;
use crate::install_guard::ReadyLaunch;
use crate::upgrade::{
    handoff::{INSTALLED_ACTIVATION_LIMIT as LIMIT, Store},
    recovery::{ReadyPlanEvidence, Receipt},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan<'a> {
    version: u16,
    target: PathBuf,
    selection: installed::Selection,
    stamp: installed::Stamp,
    binaries: String,
    ready: ReadyPlanEvidence<'a>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundSourceImage {
    reservation_digest: [u8; 32],
    image: SourceImage,
}

/// Captured from the still-serving daemon and its privately owned matched set
/// before sending its prepare request. A version path alone can be repaired
/// in place while an updater is gone.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceImage {
    root: PathBuf,
    version: String,
    proof: crate::model::HostSessionProof,
    stamp: installed::Stamp,
    binaries: String,
}

impl SourceImage {
    pub(super) fn capture(
        source: &crate::daemon::lease_handoff::HandoffSource,
        layout: &InstallLayout,
        selection: &installed::Selection,
    ) -> Result<Self, UpdateError> {
        let executable = layout.version_root.join("bin/cagentsd");
        let stamp = installed::stamp(&layout.version_root)?;
        let (device, inode) = stamp.daemon_inode();
        if source.executable()? != executable
            || !selection.source_is_unchanged(layout)?
            || served_version_root(layout)? != layout.version_root
            || crate::process::mapped_executable_identity(source.pid, &executable)?
                != (u64::from(device as u32), inode)
        {
            return Err(invalid(
                "The serving daemon is not the exact selected build",
            ));
        }
        let binaries = installed::Candidate::Local(&layout.version_root).digest()?;
        if stamp != installed::stamp(&layout.version_root)?
            || source.executable()? != executable
            || !selection.source_is_unchanged(layout)?
            || crate::process::mapped_executable_identity(source.pid, &executable)?
                != (u64::from(device as u32), inode)
            || source.exited()?
        {
            return Err(invalid(
                "The selected build changed during source inspection",
            ));
        }
        Ok(Self {
            root: layout.version_root.clone(),
            version: layout.version.clone(),
            proof: source.proof.clone(),
            stamp,
            binaries,
        })
    }

    fn verify_selected(
        &self,
        layout: &InstallLayout,
        proof: &crate::model::HostSessionProof,
    ) -> Result<(), UpdateError> {
        if self.root != layout.version_root
            || self.version != layout.version
            || self.proof != *proof
            || self.stamp != installed::stamp(&self.root)?
            || self.binaries != installed::Candidate::Local(&self.root).digest()?
            || self.stamp != installed::stamp(&self.root)?
        {
            return Err(invalid(
                "The original selected build no longer matches the approved update",
            ));
        }
        Ok(())
    }
}

/// One activation owns the saved workspace even when it has no live updater
/// or daemon. The record includes the entire plan so no second pathname is
/// needed to resume an already published reservation.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation<'a> {
    job: String,
    prefix: PathBuf,
    plan: Plan<'a>,
    #[serde(skip)]
    stored_digest: [u8; 32],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitingAdmission,
    Claimed,
    Returning,
    Selected,
    Released,
}

impl Reservation<'_> {
    fn state(&self) -> PathBuf {
        self.prefix.join(".update")
    }

    fn selected_bin(&self) -> Option<PathBuf> {
        self.prefix
            .join("current")
            .canonicalize()
            .is_ok_and(|current| current == self.plan.target)
            .then(|| self.plan.target.join("bin"))
    }

    fn return_path(&self) -> PathBuf {
        self.state().join(format!("{}.ready-return.json", self.job))
    }

    fn return_binding(&self) -> Result<Vec<u8>, UpdateError> {
        let digest: [u8; 32] = Sha256::digest(serde_json::to_vec(self)?).into();
        Ok(serde_json::to_vec(&digest)?)
    }

    fn original_image(&self) -> Result<Option<SourceImage>, UpdateError> {
        let Some(bytes) = read(&source_path(&self.state(), &self.job))? else {
            return Ok(None);
        };
        let original: BoundSourceImage = serde_json::from_slice(&bytes)?;
        if original.reservation_digest != self.stored_digest {
            return Err(invalid(
                "The original build evidence belongs to a different update",
            ));
        }
        Ok(Some(original.image))
    }

    fn verify_original_if_recorded(&self, source: &InstallLayout) -> Result<(), UpdateError> {
        if let Some(original) = self.original_image()? {
            original.verify_selected(source, self.plan.ready.source())?;
        }
        Ok(())
    }

    fn returning(&self) -> Result<bool, UpdateError> {
        let Some(bytes) = read(&self.return_path())? else {
            return Ok(false);
        };
        if bytes != self.return_binding()? {
            return Err(invalid(
                "The saved return plan belongs to a different update",
            ));
        }
        Ok(true)
    }

    fn phase(&self) -> Result<Phase, UpdateError> {
        if !self.plan.ready.same_boot()? {
            // Runtime custody expires at reboot. Keep the checkpoint for the
            // ordinary saved-work restore.
            return Ok(Phase::Released);
        }
        let state = self.state();
        let job = cutover::read_job(&state)?
            .ok_or_else(|| invalid("The reserved update job is missing"))?;
        let layout = InstallLayout {
            prefix: self.prefix.clone(),
            version: job.version.clone(),
            version_root: self.plan.target.clone(),
        };
        if job.id != self.job
            || Path::new(&job.version).file_name() != Some(std::ffi::OsStr::new(&job.version))
            || self.prefix.join("versions").join(&job.version) != self.plan.target
            || !self.plan.ready.names_job(&state, &job.id)
        {
            return Err(invalid(
                "The reservation names a different installed update",
            ));
        }
        if served_version_root(&layout)? == layout.version_root {
            let Some(selection) = cutover::recorded_selection(&layout, &state, &job)? else {
                return Err(invalid(
                    "The update selector has no matching completion evidence",
                ));
            };
            return Ok(
                if job.phase == UpdatePhase::Complete || selection == cutover::Stage::Deferred {
                    Phase::Released
                } else {
                    Phase::Selected
                },
            );
        }
        if !self.plan.selection.source_is_unchanged(&layout)? {
            return Err(invalid(
                "The installation changed while an update owns the saved work",
            ));
        }
        if self.returning()? {
            Ok(Phase::Returning)
        } else if self.plan.ready.claim_exists().map_err(invalid)? {
            Ok(Phase::Claimed)
        } else {
            Ok(Phase::AwaitingAdmission)
        }
    }
}

fn shared_reservation() -> Result<Option<Reservation<'static>>, UpdateError> {
    let Some(store) = Store::observe_existing().map_err(invalid)? else {
        return Ok(None);
    };
    let Some(bytes) = store.installed_activation().map_err(invalid)? else {
        return Ok(None);
    };
    let mut reservation: Reservation<'static> = serde_json::from_slice(&bytes)?;
    if reservation.plan.version != 1
        || !reservation.prefix.is_absolute()
        || !uuid::Uuid::parse_str(&reservation.job)
            .is_ok_and(|id| id.to_string() == reservation.job)
    {
        return Err(invalid("Invalid saved-work update reservation"));
    }
    reservation.stored_digest = Sha256::digest(&bytes).into();
    Ok(Some(reservation))
}

/// Recovery of an already selected attempt still needs the reservation's
/// exact target and matched binaries. Older attempts may lack a reservation.
#[cfg(feature = "host")]
pub(super) fn selected_evidence(
    layout: &InstallLayout,
    state: &Path,
    job: &UpdateJob,
) -> Result<Option<ReadyPlanEvidence<'static>>, UpdateError> {
    let Some(reservation) = shared_reservation()? else {
        return Ok(None);
    };
    if reservation.job != job.id
        || reservation.prefix != layout.prefix
        || reservation.plan.target != layout.version_root
        || !reservation.plan.ready.names_job(state, &job.id)
        || !reservation
            .plan
            .ready
            .names_current_runtime()
            .map_err(invalid)?
        || reservation.phase()? != Phase::Selected
    {
        return Err(invalid(
            "The installed cutover does not own the selected Ready reservation",
        ));
    }
    reservation.plan.verify_binaries(state, job)?;
    Ok(Some(reservation.plan.ready))
}

/// The account fence serializes this reservation check with publication.
pub(crate) fn require_no_activation() -> Result<(), UpdateError> {
    if let Some(reservation) = shared_reservation()?
        && reservation.phase()? != Phase::Released
    {
        return Err(UpdateError::Busy);
    }
    Ok(())
}

pub(super) fn reserves_job(state: &Path, job: &UpdateJob) -> Result<bool, UpdateError> {
    let Some(reservation) = shared_reservation()? else {
        return Ok(false);
    };
    Ok(reservation.job == job.id
        && reservation.state() == state
        && reservation.phase()? != Phase::Released)
}

pub(crate) fn begin_new_job() -> Result<(), UpdateError> {
    if let Some(reservation) = shared_reservation()? {
        if reservation.phase()? != Phase::Released {
            return Err(UpdateError::Busy);
        }
        let store = Store::observe_existing()
            .map_err(invalid)?
            .ok_or_else(|| invalid("The previous update's saved work disappeared"))?;
        store.retire_installed_activation().map_err(invalid)?;
    }
    Ok(())
}

pub fn admit_saved_state(executable: &Path) -> Result<(), UpdateError> {
    let Some(reservation) = shared_reservation()? else {
        return Ok(());
    };
    match reservation.phase()? {
        Phase::Released => Ok(()),
        phase @ (Phase::Selected | Phase::Returning) => {
            let layout = locate(executable)?;
            let matched_version = if phase == Phase::Selected {
                layout.version_root == reservation.plan.target
            } else {
                reservation.plan.selection.source_is_unchanged(&layout)?
                    && served_version_root(&layout)? == layout.version_root
            };
            if layout.prefix == reservation.prefix
                && matched_version
                && reservation
                    .plan
                    .ready
                    .names_current_runtime()
                    .map_err(invalid)?
            {
                Ok(())
            } else {
                Err(UpdateError::Busy)
            }
        }
        Phase::AwaitingAdmission | Phase::Claimed => Err(UpdateError::Busy),
    }
}

#[cfg(feature = "host")]
pub(crate) fn admit_ready(
    authority: &crate::upgrade::recovery::Authority,
) -> Result<Option<ReadyPlanEvidence<'static>>, UpdateError> {
    let Some(reservation) = shared_reservation()? else {
        return Ok(None);
    };
    if !reservation.plan.ready.same_authority(authority) {
        return Err(invalid("The saved work is reserved for a different update"));
    }
    if !matches!(
        reservation.phase()?,
        Phase::AwaitingAdmission | Phase::Claimed
    ) {
        return Err(invalid(
            "The approved update no longer owns the saved-work reservation",
        ));
    }
    Ok(Some(reservation.plan.ready))
}

fn refreshed_reservation(
    original: &Reservation<'_>,
) -> Result<Option<Reservation<'static>>, UpdateError> {
    let current = shared_reservation()?;
    if let Some(entry) = &current
        && entry.stored_digest != original.stored_digest
    {
        return Err(invalid(
            "The update changed before the application could reopen",
        ));
    }
    Ok(current)
}

fn settled_bin(
    original: &Reservation<'_>,
    source: &InstallLayout,
) -> Result<Option<PathBuf>, UpdateError> {
    if original.plan.selection.source_is_unchanged(source)? {
        return Ok(None);
    }
    let bin = original
        .selected_bin()
        .ok_or_else(|| invalid("The installed application changed before it could reopen"))?;
    installed::selected_candidate(&source.prefix, &original.plan.target)?;
    Ok(Some(bin))
}

/// A launch through the original selector can continue this exact approval.
/// Use a directory/lease watcher while another coordinator still owns it.
pub(crate) fn resume_selected(
    executable: &Path,
    mut launch: ReadyLaunch<'_>,
) -> Result<Option<PathBuf>, UpdateError> {
    let source = match locate(executable) {
        Ok(layout) => layout,
        Err(UpdateError::UnsupportedInstallation) => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(original) = shared_reservation()? else {
        return Ok(None);
    };
    if source.prefix != original.prefix || source.version_root == original.plan.target {
        return Ok(None);
    }
    let state = original.state();
    let deadline = Instant::now() + crate::install_guard::daemon_start_timeout()?;
    let mut last = String::new();
    let mut notice_shown = false;
    loop {
        let Some(reservation) = refreshed_reservation(&original)? else {
            return settled_bin(&original, &source);
        };
        let bin = reservation.plan.target.join("bin");
        let phase = reservation.phase()?;
        if phase == Phase::Released {
            return Ok(reservation.selected_bin());
        }
        if !reservation
            .plan
            .ready
            .names_current_runtime()
            .map_err(invalid)?
        {
            return Err(UpdateError::Busy);
        }
        if phase == Phase::Selected {
            return Ok(Some(bin));
        }
        let lock = match acquire_lock(&state) {
            Ok(lock) => lock,
            Err(UpdateError::Busy) => {
                // Register before reading progress. The lease also wakes us
                // if the owner exits before publishing another snapshot.
                let watcher = progress::TerminalChanges::watch_directory(&state)
                    .map_err(|_| update_in_progress())?;
                match &mut launch {
                    ReadyLaunch::Terminal(report) => {
                        if let Some(message) = installing_message(&state, &original.job) {
                            report_progress(report, &mut last, &message);
                        } else if last.is_empty() {
                            report_progress(report, &mut last, FINISHING);
                        }
                    }
                    ReadyLaunch::Window(show_progress) if !notice_shown => {
                        show_progress();
                        notice_shown = true;
                    }
                    ReadyLaunch::Window(_) => (),
                }
                if watcher.wait(deadline) != progress::TerminalWake::Changed {
                    return Err(update_in_progress());
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        let Some(reservation) = refreshed_reservation(&original)? else {
            return settled_bin(&original, &source);
        };
        match reservation.phase()? {
            Phase::Released => return Ok(reservation.selected_bin()),
            Phase::Selected => return Ok(Some(reservation.plan.target.join("bin"))),
            Phase::Returning => {
                report_returning(&mut launch, notice_shown, &mut last);
                reservation.finish_return(&source)?;
                return Ok(None);
            }
            Phase::Claimed => reservation
                .plan
                .ready
                .require_pre_effects_retry()
                .map_err(invalid)?,
            Phase::AwaitingAdmission => (),
        }
        let Some(job) = cutover::read_job(&state)? else {
            return Err(invalid("The reserved update job is missing"));
        };
        let target = match reservation.plan.target_layout(&source.prefix, &job) {
            Ok(target) => target,
            Err(error) => {
                let Some(target) = reservation
                    .settle_unavailable_candidate(&source, &state, &job, &lock, &error)?
                else {
                    report_returning(&mut launch, notice_shown, &mut last);
                    reservation.finish_return(&source)?;
                    return Ok(None);
                };
                target
            }
        };
        let log = OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(state.join(format!("{}.log", job.id)))?;
        validate_lock(&log)?;
        let watcher = matches!(launch, ReadyLaunch::Terminal(_))
            .then(|| progress::TerminalChanges::watch_directory(&state).ok())
            .flatten();
        let mut worker = spawn_update_worker(&bin.join("cagents"), &job.id, log, &lock)?;
        drop(lock.into_file());
        let exited = match launch {
            ReadyLaunch::Terminal(report) => {
                report_progress(report, &mut last, FINISHING);
                observe_worker(worker, watcher, &state, &job, report, &mut last, deadline)?
            }
            ReadyLaunch::Window(show_progress) => {
                if !notice_shown {
                    show_progress();
                }
                worker.wait()?
            }
        };
        if !exited.success() {
            return Err(invalid(
                "The approved update could not finish starting the staged build",
            ));
        }
        installed::selected_candidate(&source.prefix, &target.version_root)?;
        return Ok(Some(bin));
    }
}

impl Reservation<'_> {
    fn settle_unavailable_candidate(
        &self,
        source: &InstallLayout,
        state: &Path,
        job: &UpdateJob,
        worker: &File,
        failure: &UpdateError,
    ) -> Result<Option<InstallLayout>, UpdateError> {
        let guard = crate::instance::UiUpdateGuard::acquire()?;
        let _publication = crate::install_guard::reserve_darwin_publication()?;
        if let Ok(target) = self.plan.target_layout(&source.prefix, job) {
            return Ok(Some(target));
        }
        let handoff = acquire_unserved_runtime()?;
        if self.phase()? != Phase::AwaitingAdmission
            || !self.plan.selection.source_is_unchanged(source)?
            || served_version_root(source)? != source.version_root
            || guard.has_running_windows()?
        {
            return Err(invalid(
                "The selected application no longer owns the saved work",
            ));
        }
        self.plan
            .ready
            .verify_unclaimed_source(worker, &guard, &handoff)
            .map_err(invalid)?;
        self.verify_original_if_recorded(source)?;
        eprintln!("The staged update could not be verified: {failure}");
        cutover::persist(state, &self.return_path(), &self.return_binding()?, true)?;
        Ok(None)
    }

    fn finish_return(&self, source: &InstallLayout) -> Result<(), UpdateError> {
        let slots = self.plan.ready.saved_windows();
        let executable = source.version_root.join("bin/cagents-ui");
        let guard = crate::instance::UiUpdateGuard::acquire()?;
        if self.phase()? != Phase::Returning
            || !self.plan.selection.source_is_unchanged(source)?
            || served_version_root(source)? != source.version_root
        {
            return Err(invalid(
                "This update can only reopen its original selected build",
            ));
        }
        self.verify_original_if_recorded(source)?;
        guard.relaunch_missing_windows(slots, &executable)?;

        let guard = crate::instance::UiUpdateGuard::acquire()?;
        let _publication = crate::install_guard::reserve_darwin_publication()?;
        if self.phase()? != Phase::Returning {
            return Err(invalid(
                "The application changed while reopening its windows",
            ));
        }
        guard.verify_cutover_windows(slots, &executable)?;
        self.verify_original_if_recorded(source)?;
        let store = Store::observe_existing()
            .map_err(invalid)?
            .ok_or_else(|| invalid("The approved Ready checkpoint disappeared"))?;
        let state = self.state();
        let mut job = cutover::read_job(&state)?
            .ok_or_else(|| invalid("The reserved update job is missing"))?;
        job.phase = UpdatePhase::Failed;
        job.message =
            "The staged update changed. Your work is available in the current version.".into();
        job.refusal = None;
        write_job(&state, &job)?;
        store.retire_installed_activation().map_err(invalid)?;
        Ok(())
    }
}

const FINISHING: &str = "Finishing the approved update";

fn report_returning(launch: &mut ReadyLaunch<'_>, notice_shown: bool, last: &mut String) {
    match launch {
        ReadyLaunch::Terminal(report) => {
            report_progress(report, last, "Reopening the current version")
        }
        ReadyLaunch::Window(show_progress) if !notice_shown => show_progress(),
        ReadyLaunch::Window(_) => (),
    }
}

fn update_in_progress() -> UpdateError {
    io::Error::new(io::ErrorKind::WouldBlock, crate::instance::UpdateInProgress).into()
}

fn installing_message(state: &Path, job: &str) -> Option<String> {
    let snapshot = cutover::read_job(state).ok()??;
    (snapshot.id == job && snapshot.phase == UpdatePhase::Installing).then_some(snapshot.message)
}

fn report_progress(report: &mut dyn FnMut(&str), last: &mut String, message: &str) {
    if last != message {
        report(message);
        last.clear();
        last.push_str(message);
    }
}

fn observe_worker(
    mut worker: std::process::Child,
    watcher: Option<progress::TerminalChanges>,
    state: &Path,
    original: &UpdateJob,
    report: &mut dyn FnMut(&str),
    last: &mut String,
    deadline: Instant,
) -> io::Result<std::process::ExitStatus> {
    use std::os::unix::net::UnixStream;

    let (Some(watcher), Ok((exited, wake))) = (watcher, UnixStream::pair()) else {
        return worker.wait();
    };
    thread::scope(|scope| {
        let wait = scope.spawn(move || {
            let result = worker.wait();
            drop(wake);
            result
        });
        let mut before_worker = Some(original.message.as_str());
        let mut observe = || {
            if let Some(message) = installing_message(state, &original.id)
                && before_worker != Some(message.as_str())
            {
                before_worker = None;
                report_progress(report, last, &message);
            }
        };
        loop {
            observe();
            if progress::TerminalChanges::wait_with_input(
                Some(&watcher),
                Some(exited.as_raw_fd()),
                Some(deadline),
            ) != progress::TerminalWake::Changed
            {
                break;
            }
        }
        let result = wait.join().expect("waiting for the update worker panicked");
        observe();
        result
    })
}

/// Keeps another updater from taking over while the user archives a failed
/// attempt. The caller retains the exclusive account and runtime leases.
pub(crate) struct Abandonment {
    _worker: crate::instance::OwnedLock,
    reservation: Reservation<'static>,
}

pub(crate) fn abandon_claimed() -> Result<Option<Abandonment>, UpdateError> {
    let Some(reservation) = shared_reservation()? else {
        return Ok(None);
    };
    let phase = reservation.phase()?;
    if phase == Phase::Released {
        return Ok(None);
    }
    if !matches!(phase, Phase::Claimed | Phase::Selected)
        || !reservation
            .plan
            .ready
            .names_current_runtime()
            .map_err(invalid)?
    {
        return Err(UpdateError::Busy);
    }
    let _worker = acquire_lock(&reservation.state())?;
    if !reservation
        .plan
        .ready
        .claimed_process_exited()
        .map_err(invalid)?
    {
        return Err(UpdateError::Busy);
    }
    Ok(Some(Abandonment {
        _worker,
        reservation,
    }))
}

impl Abandonment {
    pub(crate) fn archive(&self, store: &Store) -> Result<PathBuf, String> {
        let path = store
            .archive_claimed_update(&self.reservation.job, self.reservation.plan.ready.source())?;
        store.retire_installed_activation()?;
        Ok(path)
    }
}

pub(super) struct Recorded {
    path: PathBuf,
    digest: [u8; 32],
}

impl Recorded {
    pub(super) fn unchanged(&self) -> Result<bool, UpdateError> {
        let bytes = read(&self.path)?
            .ok_or_else(|| invalid("The original Ready activation plan disappeared"))?;
        Ok(Sha256::digest(&bytes)[..] == self.digest)
    }
}

pub(super) fn record(
    state: &Path,
    job: &UpdateJob,
    receipt: &Receipt,
    selection: &installed::Selection,
    stamp: &installed::Stamp,
    binaries: &str,
    original: SourceImage,
) -> Result<Recorded, UpdateError> {
    let prefix = state.parent().ok_or_else(unsupported_install)?;
    let ready = receipt.ready_plan_evidence().map_err(invalid)?;
    if ready.source() != &original.proof {
        return Err(invalid(
            "The acknowledged checkpoint belongs to another source",
        ));
    }
    let bytes = serde_json::to_vec(&Reservation {
        job: job.id.clone(),
        prefix: prefix.to_owned(),
        stored_digest: [0; 32],
        plan: Plan {
            version: 1,
            target: prefix.join("versions").join(&job.version),
            selection: selection.clone(),
            stamp: stamp.clone(),
            binaries: binaries.into(),
            ready,
        },
    })?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("The Ready activation plan exceeds its size limit"));
    }
    let store = Store::observe_existing()
        .map_err(invalid)?
        .ok_or_else(|| invalid("The acknowledged upgrade store disappeared"))?;
    begin_new_job()?;
    let digest = Sha256::digest(&bytes).into();
    // Old installed launchers strictly decode the shared reservation. Publish
    // new evidence separately and make it durable before that reservation.
    cutover::persist(
        state,
        &source_path(state, &job.id),
        &serde_json::to_vec(&BoundSourceImage {
            reservation_digest: digest,
            image: original,
        })?,
        true,
    )?;
    store.create_installed_activation(&bytes).map_err(invalid)?;
    Ok(Recorded {
        path: store.installed_activation_path(),
        digest,
    })
}

fn source_path(state: &Path, job: &str) -> PathBuf {
    state.join(format!("{job}.ready-source.json"))
}

fn path(state: &Path, job: &str) -> Result<PathBuf, UpdateError> {
    if uuid::Uuid::parse_str(job).is_ok_and(|id| id.to_string() == job) {
        Ok(state.join(format!("{job}.ready-plan.json")))
    } else {
        Err(invalid("Invalid Ready activation job"))
    }
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, UpdateError> {
    let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(invalid(
            "The Ready activation plan has no private file location",
        ));
    };
    let root = crate::fs_safety::Root::open(directory)?;
    let file = match crate::fs_safety::open_beneath(&root, Path::new(name)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(invalid(error.to_string())),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > LIMIT
    {
        return Err(invalid("The Ready activation plan is not a private file"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("The Ready activation plan exceeds its size limit"));
    }
    Ok(Some(bytes))
}

fn load(state: &Path, job: &UpdateJob) -> Result<Option<(Plan<'static>, bool)>, UpdateError> {
    if !matches!(job.phase, UpdatePhase::Installing | UpdatePhase::Failed) {
        return Ok(None);
    }
    if let Some(reservation) = shared_reservation()? {
        if reservation.state() == state && reservation.job == job.id {
            if reservation.returning()? {
                return Err(invalid(
                    "This update is already reopening the selected build",
                ));
            }
            return Ok(Some((reservation.plan, true)));
        }
        if reservation.phase()? != Phase::Released {
            return Err(UpdateError::Busy);
        }
    }
    let Some(bytes) = read(&path(state, &job.id)?)? else {
        return Ok(None);
    };
    let plan: Plan<'static> = serde_json::from_slice(&bytes)?;
    if plan.version != 1 {
        return Err(invalid("Unsupported Ready activation plan"));
    }
    Ok(Some((plan, false)))
}

impl Plan<'_> {
    fn target_layout(&self, prefix: &Path, job: &UpdateJob) -> Result<InstallLayout, UpdateError> {
        let layout = locate(&self.target.join("bin/cagents"))?;
        if layout.prefix != prefix
            || layout.version_root != self.target
            || layout.version != job.version
            || self.selection != installed::Selection::capture(&layout)?
            || self.stamp != installed::stamp(&self.target)?
        {
            return Err(invalid(
                "The approved installation changed while the update was interrupted",
            ));
        }
        Ok(layout)
    }

    fn verify_binaries(&self, state: &Path, job: &UpdateJob) -> Result<(), UpdateError> {
        let bytes = read_private(
            &state.join(format!("{}.request", job.id)),
            SMALL_ASSET_LIMIT,
        )?;
        let candidate = match serde_json::from_slice(&bytes)? {
            UpdateRequest::Installed { ref root }
            | UpdateRequest::InstalledFromTerminal { ref root }
                if *root == self.target =>
            {
                installed::Candidate::Local(&self.target).digest()?
            }
            UpdateRequest::Release(ref release)
            | UpdateRequest::ReleaseFromTerminal(ref release)
                if release.version == job.version =>
            {
                release_stage::verify_interrupted_candidate(
                    state,
                    job,
                    &self.target,
                    self.selection.previous_link(),
                    &Sha256::digest(&bytes).into(),
                )?
            }
            _ => {
                return Err(invalid(
                    "The interrupted update request names another candidate",
                ));
            }
        };
        if candidate != self.binaries || installed::stamp(&self.target)? != self.stamp {
            return Err(invalid(
                "The approved build changed while the update was interrupted",
            ));
        }
        Ok(())
    }

    fn selection_is_untouched(&self, layout: &InstallLayout) -> Result<bool, UpdateError> {
        Ok(installed::Selection::capture(layout)? == self.selection)
    }
}

pub(super) fn recover_worker(
    installation: &Installation,
    state: &Path,
    job: &mut UpdateJob,
    worker: &File,
) -> Result<(), UpdateError> {
    let (plan, reserved) =
        load(state, job)?.ok_or_else(|| invalid("This update has no Ready activation plan"))?;
    let target = plan.target_layout(&installation.prefix, job)?;
    if target.version != installation.version {
        return Err(invalid(
            "The interrupted update needs its original staged build",
        ));
    }
    let executable = fs::symlink_metadata(target.version_root.join("bin/cagents"))?;
    if crate::process::mapped_region_identity(
        std::process::id(),
        recover_worker as *const () as usize as u64,
    )? != (executable.dev(), executable.ino())
    {
        return Err(invalid(
            "The interrupted update worker no longer matches the approved CLI",
        ));
    }
    let mut guard = crate::instance::UiUpdateGuard::acquire()?;
    if guard.has_running_windows()? {
        return Err(invalid(
            "A window reopened before the approved update could resume",
        ));
    }
    let daemon = target.version_root.join("bin/cagentsd");
    let publication = crate::install_guard::reserve_darwin_publication()?;
    plan.verify_binaries(state, job)?;
    if !plan.selection_is_untouched(&target)? {
        return Err(invalid(
            "The installed selector changed before the update could resume",
        ));
    }
    let handoff = acquire_unserved_runtime()?;
    let receipt = plan
        .ready
        .reopen(&daemon, worker, &guard, &handoff, reserved)
        .map_err(invalid)?;
    let mut standby = crate::daemon::standby::Parked::launch_with_custody(
        &daemon,
        plan.stamp.daemon_inode(),
        &guard,
        plan.selection.previous_link(),
        publication,
    )?;
    set_phase(state, job, UpdatePhase::Installing, FINISHING)?;
    let timeout = crate::install_guard::daemon_start_timeout()?;
    let started = standby.activate(&daemon, Some(&handoff), Some(&receipt), timeout, || {
        let _ = installed::report_overdue_startup(state, job);
    });
    if standby.may_own_runtime() {
        guard.leave_launch_fence_with_successor();
    } else {
        started?;
    }
    // A lost acknowledgement can race the exact successor becoming Ready.
    // Its verified listener settles the transfer and its own launcher
    // completes the original job after our final fence copies close.
    installed::confirm_daemon_ready(&daemon, Some(standby.pid()), timeout)?;
    installed::selected_candidate(&installation.prefix, &target.version_root)
}

fn acquire_unserved_runtime() -> Result<crate::daemon::lease_handoff::LeaseHandoff, UpdateError> {
    let paths =
        crate::daemon::RuntimePaths::discover().map_err(|error| invalid(error.to_string()))?;
    let handoff = crate::daemon::lease_handoff::LeaseHandoff::try_acquire(&paths)
        .map_err(|error| invalid(error.to_string()))?
        .ok_or_else(|| invalid("The terminal service already has an owner"))?;
    match crate::descriptor::connect_settled(&paths.socket) {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => return Err(error.into()),
        Ok(_) => return Err(invalid("The terminal service is already serving")),
    }
    Ok(handoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_source_refuses_same_version_binary_replacement() {
        use crate::model::{BootId, HostIncarnation, HostSessionId, HostSessionProof};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("versions/0.2.4");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["cagents", "cagentsd", "cagents-ui"] {
            fs::write(bin.join(name), name).unwrap();
        }
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("source-session"),
            host_incarnation: HostIncarnation::new("source-incarnation"),
            boot_id: BootId::new("source-boot"),
        };
        let original = SourceImage {
            root: root.clone(),
            version: "0.2.4".into(),
            proof: proof.clone(),
            stamp: installed::stamp(&root).unwrap(),
            binaries: installed::Candidate::Local(&root).digest().unwrap(),
        };
        let layout = InstallLayout {
            prefix: directory.path().to_owned(),
            version_root: root,
            version: "0.2.4".into(),
        };
        original.verify_selected(&layout, &proof).unwrap();
        let mut wrong = proof.clone();
        wrong.host_incarnation = HostIncarnation::new("different-incarnation");
        assert!(original.verify_selected(&layout, &wrong).is_err());

        let replacement = bin.join("replacement");
        fs::write(&replacement, "cagents-ui").unwrap();
        fs::rename(replacement, bin.join("cagents-ui")).unwrap();
        assert!(original.verify_selected(&layout, &proof).is_err());
    }

    #[test]
    fn terminal_progress_reads_the_exact_job_even_if_its_worker_exits_before_notification() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path();
        let _lease = acquire_lock(state).unwrap();
        let mut job = UpdateJob {
            id: uuid::Uuid::new_v4().to_string(),
            version: "0.2.4".into(),
            phase: UpdatePhase::Installing,
            message: "Starting the installed application".into(),
            log_path: state.join("update.log"),
            confirmation: None,
            refusal: None,
            in_flight: Vec::new(),
            recorded_by: None,
        };
        let original = job.clone();
        job.id = uuid::Uuid::new_v4().to_string();
        write_job(state, &job).unwrap();
        assert!(installing_message(state, &original.id).is_none());

        let watcher = progress::TerminalChanges::watch_directory(state).unwrap();
        job.id.clone_from(&original.id);
        job.message = "Restoring sessions is taking longer".into();
        let next = state.join("next");
        serde_json::to_writer(private_create(&next).unwrap(), &job).unwrap();
        let worker = Command::new("/bin/mv")
            .arg(next)
            .arg(state.join("status.json"))
            .spawn()
            .unwrap();
        let mut messages = Vec::new();
        let status = observe_worker(
            worker,
            Some(watcher),
            state,
            &original,
            &mut |message| messages.push(message.to_owned()),
            &mut FINISHING.to_owned(),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert!(status.success());
        assert_eq!(messages, [job.message]);
    }

    #[test]
    fn retry_requires_the_original_private_plan_contents() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("ready-plan.json");
        let bytes = b"exact Ready plan";
        cutover::persist(state.path(), &path, bytes, true).unwrap();
        let recorded = Recorded {
            path: path.clone(),
            digest: Sha256::digest(bytes).into(),
        };
        assert!(recorded.unchanged().unwrap());
        fs::write(&path, b"changed Ready plan").unwrap();
        assert!(!recorded.unchanged().unwrap());

        let alias = state.path().join("alias");
        fs::hard_link(&path, &alias).unwrap();
        assert!(recorded.unchanged().is_err());
        fs::remove_file(path).unwrap();
        let name = std::ffi::CString::new(recorded.path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the pathname is NUL-terminated and refers to a private fixture directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(recorded.unchanged().is_err());
    }
}
