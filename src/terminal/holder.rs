//! Discovery evidence for terminal holders. Only process identities go to disk;
//! recipes and transient execution settings stay in the holder's memory.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::model::{BootId, TerminalId, TerminalSession};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recovery {
    pub source: TerminalSession,
    pub previous_sources: Vec<TerminalSession>,
    pub relaunches: Vec<super::ipc::UpgradeRelaunch>,
    pub options: super::TerminalSpawnOptions,
    pub execution_environment: Option<super::ipc::ChatExecutionEnvironment>,
}

impl Recovery {
    pub(crate) fn validate(&self, boot: &BootId, terminal: TerminalId) -> Result<(), String> {
        let proof = self
            .source
            .continuity
            .live_proof()
            .filter(|proof| proof.is_complete() && &proof.boot_id == boot)
            .ok_or("holder has no complete ownership proof for this boot")?;
        if self.source.id != terminal
            || self.previous_sources.len() > 1024
            || self.previous_sources.iter().any(|source| {
                source.id != terminal
                    || source
                        .continuity
                        .live_proof()
                        .is_none_or(|old| !old.is_complete() || old.boot_id != proof.boot_id)
            })
        {
            return Err("holder has inconsistent terminal lineage".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Process {
    pub pid: i32,
    pub start: u64,
}

impl Process {
    pub fn read(pid: i32) -> io::Result<Self> {
        let stat = fs::read(format!("/proc/{pid}/stat"))?;
        let (_, start) = crate::model::agent_recovery::process_identity(&stat)
            .ok_or_else(|| io::Error::other("invalid process identity"))?;
        Ok(Self { pid, start })
    }
    pub fn is_current(&self) -> bool {
        Self::read(self.pid).is_ok_and(|actual| actual == *self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub version: u32,
    pub application: String,
    pub boot: BootId,
    pub terminal: TerminalId,
    pub holder: Process,
    pub root: Process,
}

pub(crate) fn directory() -> Result<PathBuf, String> {
    crate::daemon::RuntimePaths::discover()
        .map(|paths| paths.directory.join("holders"))
        .map_err(|error| error.to_string())
}

pub(crate) fn socket(directory: &Path, terminal: TerminalId) -> PathBuf {
    directory.join(terminal.to_string())
}

impl Record {
    fn path(&self, directory: &Path) -> PathBuf {
        directory.join(format!("{}.json", self.terminal))
    }

    pub fn create(&self, directory: &Path) -> io::Result<()> {
        let path = self.path(directory);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let identity = file.metadata()?;
        let result = (|| {
            serde_json::to_writer(&mut file, self).map_err(io::Error::other)?;
            file.flush()?;
            file.sync_all()
        })();
        if result.is_err()
            && fs::symlink_metadata(&path).is_ok_and(|actual| {
                actual.dev() == identity.dev() && actual.ino() == identity.ino()
            })
        {
            let _ = fs::remove_file(path);
        }
        result
    }

    pub fn remove(&self, directory: &Path) {
        // A stale cleanup can never erase the evidence of a replacement.
        if read(&self.path(directory)).is_ok_and(|record| record == *self) {
            let _ = fs::remove_file(self.path(directory));
            let _ = fs::remove_file(socket(directory, self.terminal));
        }
    }
}

fn read(path: &Path) -> io::Result<Record> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > 4096
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe holder discovery record",
        ));
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub(crate) fn evidence(socket: &Path) -> Result<Option<Record>, String> {
    let path = socket.with_extension("json");
    let record = match read(&path) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let boot = crate::daemon::read_boot_id().map_err(|error| error.to_string())?;
    if record.version != 3
        || record.application != crate::instance::APPLICATION_ID
        || record.boot != boot
        || socket.file_name() != Some(std::ffi::OsStr::new(&record.terminal.to_string()))
    {
        return Err("holder discovery evidence does not match this endpoint".into());
    }
    Ok(Some(record))
}

pub(crate) fn discover(directory: &Path, boot: &BootId) -> Result<Vec<Record>, String> {
    crate::secure_file::prepare_private_directory(directory).map_err(|error| error.to_string())?;
    let mut records = Vec::new();
    for (index, entry) in fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .enumerate()
    {
        if index >= 8192 {
            return Err("holder directory exceeds discovery limit".into());
        }
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let record = match read(&path) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "ignored unsafe holder record");
                continue;
            }
        };
        if record.version != 3
            || record.application != crate::instance::APPLICATION_ID
            || record.path(directory) != path
            || &record.boot != boot
        {
            tracing::warn!(path = %path.display(), "ignored unrelated holder record");
            continue;
        }
        if !record.holder.is_current() {
            record.remove(directory);
            continue;
        }
        records.push(record);
        if records.len() > 4096 {
            return Err("holder inventory exceeds session limit".into());
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn record() -> Record {
        let process = Process::read(std::process::id() as i32).unwrap();
        Record {
            version: 3,
            application: crate::instance::APPLICATION_ID.into(),
            boot: crate::daemon::read_boot_id().unwrap(),
            terminal: TerminalId::new(),
            holder: process.clone(),
            root: process,
        }
    }

    #[test]
    fn discovery_rejects_foreign_unsafe_and_reused_process_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let valid = record();
        valid.create(directory.path()).unwrap();
        let mut foreign = record();
        foreign.application = "another-installation".into();
        foreign.create(directory.path()).unwrap();
        let mut old_boot = record();
        old_boot.boot = BootId::new("previous-boot");
        old_boot.create(directory.path()).unwrap();
        let unsafe_record = record();
        unsafe_record.create(directory.path()).unwrap();
        fs::set_permissions(
            unsafe_record.path(directory.path()),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        let mut stale = record();
        stale.holder.start += 1;
        stale.create(directory.path()).unwrap();
        fs::write(socket(directory.path(), stale.terminal), "stale endpoint").unwrap();
        let target = directory.path().join("untouched");
        fs::write(&target, "foreign file").unwrap();
        let link = directory.path().join(format!("{}.json", TerminalId::new()));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            discover(directory.path(), &valid.boot).unwrap(),
            vec![valid]
        );
        assert!(foreign.path(directory.path()).exists());
        assert!(old_boot.path(directory.path()).exists());
        assert!(!stale.path(directory.path()).exists());
        assert!(!socket(directory.path(), stale.terminal).exists());
        assert_eq!(fs::read_to_string(target).unwrap(), "foreign file");
    }
}
