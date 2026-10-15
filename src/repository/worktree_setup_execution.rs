//! A free execution flock is not evidence that a dead holder cleaned up.
//! This record is initialized by creation settlement, reserved before spawn,
//! and cleared only by the dedicated holder's descendant-reaping capability.
use super::{OperationId, fs_safety};
use crate::model::TerminalId;
use serde::{Deserialize, Serialize};
use std::{fs::File, io, io::Write, os::unix::fs::MetadataExt, path::{Path, PathBuf}};

const FILE_NAME: &str = "cagents-setup-execution";
const UNKNOWN: &str = "The previous Worktree setup has no proved family cleanup. Its holder may have been lost while child processes survived. Keep this checkout interrupted; resolve its retained terminal ownership before reviewing setup again.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupExecutionReservation {
    pub(crate) creation: OperationId,
    pub(crate) run: OperationId,
    pub(crate) terminal: TerminalId,
    pub(crate) admin: PathBuf,
    pub(crate) admin_identity: (u64, u64),
    pub(crate) lease_identity: (u64, u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    NeverStarted,
    Reserved(SetupExecutionReservation),
    FamilyReaped(SetupExecutionReservation),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Disposition {
    creation: OperationId,
    admin_identity: (u64, u64),
    phase: Phase,
}

fn identity(file: &File) -> io::Result<(u64, u64)> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

fn validate_root(root: &fs_safety::Root, creation: OperationId) -> io::Result<(u64, u64)> {
    if !super::worktree_setup_intent::named_directory_matches(root)
        || super::worktree_setup_intent::read_creation_marker(root)? != Some(creation)
    {
        return Err(io::Error::other("Setup execution administrative generation changed"));
    }
    let metadata = root.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

fn read(root: &fs_safety::Root, creation: OperationId) -> io::Result<Option<Disposition>> {
    let expected = validate_root(root, creation)?;
    let mut file = match fs_safety::open_beneath(root, Path::new(FILE_NAME)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(io::Error::other(error)),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe {libc::geteuid()} || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other("Unsafe setup execution disposition"));
    }
    let bytes = fs_safety::read_bounded(&mut file, 8192)?;
    if bytes.len() > 8192 {return Err(io::Error::other("Oversized setup execution disposition"));}
    let value: Disposition = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if value.creation != creation || value.admin_identity != expected {
        return Err(io::Error::other("Setup execution disposition belongs to another checkout"));
    }
    match &value.phase {
        Phase::NeverStarted => {},
        Phase::Reserved(reservation) | Phase::FamilyReaped(reservation) => {
            if reservation.creation != creation || reservation.admin_identity != expected || reservation.admin != root.path() {
                return Err(io::Error::other("Setup execution reservation identity is inconsistent"));
            }
        }
    }
    Ok(Some(value))
}

fn publish(root: &fs_safety::Root, value: &Disposition, new: bool) -> io::Result<()> {
    validate_root(root, value.creation)?;
    let (mut file, temporary) = fs_safety::create_temporary_in(root, 0o600).map_err(io::Error::other)?;
    let result = (|| {
        serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        validate_root(root, value.creation)?;
        if new {
            fs_safety::rename_new_within(root, &temporary, std::ffi::OsStr::new(FILE_NAME)).map_err(io::Error::other)?;
        } else {
            fs_safety::rename_within(root, &temporary, std::ffi::OsStr::new(FILE_NAME)).map_err(io::Error::other)?;
        }
        fs_safety::sync_directory(root)?;
        validate_root(root, value.creation)?;
        Ok(())
    })();
    let _ = fs_safety::unlink_within(root, &temporary);
    result
}

/// Called only by original creation settlement under its Git lock and lease,
/// before Created can be published or any setup can be scheduled.
pub(super) fn initialize(root: &fs_safety::Root, creation: OperationId) -> io::Result<()> {
    if read(root, creation)?.is_some() {return Ok(());}
    publish(root, &Disposition {creation, admin_identity: validate_root(root, creation)?, phase: Phase::NeverStarted}, true)
}

pub(crate) fn require_quiescent(admin: &Path, creation: OperationId, lease: &File) -> io::Result<()> {
    crate::secure_file::reject_symlink_components(admin)?;
    let root = fs_safety::Root::open(admin)?;
    let value = read(&root, creation)?.ok_or_else(|| io::Error::other(UNKNOWN))?;
    match value.phase {
        Phase::NeverStarted => Ok(()),
        Phase::FamilyReaped(reservation) if reservation.lease_identity == identity(lease)? => Ok(()),
        _ => Err(io::Error::other(UNKNOWN)),
    }
}

/// The caller retains the original exclusive creation lease through handoff.
#[cfg(feature = "host")]
pub(crate) fn reserve(admin: &Path, creation: OperationId, run: OperationId, terminal: TerminalId, lease: &File) -> io::Result<SetupExecutionReservation> {
    require_quiescent(admin, creation, lease)?;
    let root = fs_safety::Root::open(admin)?;
    let reservation = SetupExecutionReservation {creation, run, terminal, admin: admin.into(), admin_identity: validate_root(&root, creation)?, lease_identity: identity(lease)?};
    publish(&root, &Disposition {creation, admin_identity: reservation.admin_identity, phase: Phase::Reserved(reservation.clone())}, false)?;
    Ok(reservation)
}

#[cfg(feature = "host")]
pub(crate) struct SetupExecutionWitness {
    root: fs_safety::Root,
    reservation: SetupExecutionReservation,
}

#[cfg(feature = "host")]
impl SetupExecutionWitness {
    pub(crate) fn open(reservation: SetupExecutionReservation, lease: &File) -> io::Result<Self> {
        if identity(lease)? != reservation.lease_identity {return Err(io::Error::other("Setup execution lease identity changed"));}
        crate::secure_file::reject_symlink_components(&reservation.admin)?;
        let root = fs_safety::Root::open(&reservation.admin)?;
        let actual = read(&root, reservation.creation)?.ok_or_else(|| io::Error::other(UNKNOWN))?;
        if actual.phase != Phase::Reserved(reservation.clone()) {return Err(io::Error::other("Setup execution reservation changed before child launch"));}
        Ok(Self {root, reservation})
    }

    pub(crate) fn validate_launch(&self, command: &portable_pty::CommandBuilder, recovery_terminal: Option<TerminalId>) -> io::Result<()> {
        let environment = |name| command.get_env(name).and_then(|value|value.to_str());
        if environment("CAGENTS_WORKTREE_SETUP") != Some("1")
            || environment("CAGENTS_TERMINAL_ID") != Some(self.reservation.terminal.to_string().as_str())
            || environment("CAGENTS_WORKTREE_SETUP_ID") != Some(self.reservation.run.to_string().as_str())
            || environment("CAGENTS_WORKTREE_SETUP_CREATION_ID") != Some(self.reservation.creation.to_string().as_str())
            || recovery_terminal.is_some_and(|terminal|terminal != self.reservation.terminal)
        {
            return Err(io::Error::other("Setup command does not match its original reservation"));
        }
        Ok(())
    }

    pub(crate) fn family_reaped(&self, _proof: &crate::daemon::owned_children::ReapedFamily) -> io::Result<()> {
        let mut actual = read(&self.root, self.reservation.creation)?.ok_or_else(|| io::Error::other(UNKNOWN))?;
        if actual.phase == Phase::FamilyReaped(self.reservation.clone()) {return Ok(());}
        if actual.phase != Phase::Reserved(self.reservation.clone()) {return Err(io::Error::other("A newer setup execution disposition cannot be overwritten"));}
        actual.phase = Phase::FamilyReaped(self.reservation.clone());
        publish(&self.root, &actual, false)
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};

    #[test]
    fn lost_execution_flock_does_not_clear_a_durable_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(directory.path()).unwrap();
        let creation = OperationId::new();
        super::super::worktree_setup_intent::record_creation_identity(&root, creation).unwrap();
        let lease_path = directory.path().join("original.lease");
        let lease = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&lease_path).unwrap();
        assert_eq!(unsafe {libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)}, 0);
        require_quiescent(directory.path(), creation, &lease).unwrap();
        let reservation = reserve(directory.path(), creation, OperationId::new(), TerminalId::new(), &lease).unwrap();
        let witness = SetupExecutionWitness::open(reservation.clone(), &lease).unwrap();
        drop(witness);
        drop(lease); // A killed holder releases its open-file-description lock.
        let recovered = File::open(&lease_path).unwrap();
        assert_eq!(unsafe {libc::flock(recovered.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)}, 0);
        assert!(require_quiescent(directory.path(), creation, &recovered).is_err(), "a free original flock is not descendant cleanup");
        assert!(reserve(directory.path(), creation, OperationId::new(), TerminalId::new(), &recovered).is_err());
        assert_eq!(read(&root, creation).unwrap().unwrap().phase, Phase::Reserved(reservation));
    }

    #[test]
    fn missing_or_changed_disposition_cannot_be_inferred_from_a_generation_marker() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(directory.path()).unwrap();
        let creation = OperationId::new();
        super::super::worktree_setup_intent::record_creation_identity(&root, creation).unwrap();
        let lease = File::open(directory.path().join(FILE_NAME)).unwrap();
        std::fs::remove_file(directory.path().join(FILE_NAME)).unwrap();
        assert!(require_quiescent(directory.path(), creation, &lease).is_err());
        std::fs::write(directory.path().join(FILE_NAME), b"{}").unwrap();
        assert!(require_quiescent(directory.path(), creation, &lease).is_err());
    }
}

#[cfg(all(test, feature = "host"))]
pub(crate) fn test_reservation(admin: &Path, run: OperationId, terminal: TerminalId, lease: &File) -> SetupExecutionReservation {
    let root = fs_safety::Root::open(admin).unwrap();
    let creation = OperationId::new();
    super::worktree_setup_intent::record_creation_identity(&root, creation).unwrap();
    reserve(admin, creation, run, terminal, lease).unwrap()
}
