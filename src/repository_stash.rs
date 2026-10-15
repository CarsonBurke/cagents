//! Stash deletion under the same files-backend locks used by Git. A reflog
//! selector is checked only after acquiring the ref lock; external pushes
//! cannot retarget it between confirmation and deletion.
use super::*;
use std::{io::Write, os::unix::fs::PermissionsExt};

#[derive(Debug)]
pub(super) enum StashDropFailure {
    Refused(String),
    Uncertain(String),
}
impl From<String> for StashDropFailure {
    fn from(reason: String) -> Self {
        Self::Refused(reason)
    }
}
impl From<&str> for StashDropFailure {
    fn from(reason: &str) -> Self {
        Self::Refused(reason.to_owned())
    }
}
impl fmt::Display for StashDropFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(reason) => formatter.write_str(reason),
            Self::Uncertain(reason) => write!(
                formatter,
                "Stash refs may have changed; inspect the stash list before retrying: {reason}"
            ),
        }
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    pub(super) fn drop_stash_at(
        &self,
        worktree: &Path,
        reference: &str,
        expected: &Oid,
        cancellation: &CancellationToken,
    ) -> Result<(), StashDropFailure> {
        let index = reference
            .strip_prefix("stash@{")
            .and_then(|reference| reference.strip_suffix('}'))
            .and_then(|index| index.parse::<usize>().ok())
            .ok_or("Invalid stash selector")?;
        let common = self
            .common_dir(worktree, cancellation)
            .map_err(|error| error.to_string())?;
        // A repository using another ref backend must be handled by that
        // backend's transaction API. Never edit an unfamiliar storage format.
        let storage = self
            .run(
                &self.git_read(worktree, ["config", "--get", "extensions.refStorage"]),
                cancellation,
            )
            .map_err(|error| error.to_string())?;
        if storage.success && String::from_utf8_lossy(&storage.stdout).trim() != "files" {
            return Err(
                "This repository's ref storage does not support safe stash deletion".into(),
            );
        }
        if !storage.success && storage.exit_code != Some(1) {
            return Err("Could not identify this repository's ref storage".into());
        }
        let mut reference_lock = GitFileLock::acquire(&common.join("refs/stash"))?;
        let mut log_lock = GitFileLock::acquire(&common.join("logs/refs/stash"))?;
        // pack-refs may otherwise move the loose ref during the transaction.
        let mut packed_lock = GitFileLock::acquire(&common.join("packed-refs"))?;
        let log = locked_contents(&log_lock, OsStr::new("stash"))?
            .ok_or("The selected stash no longer exists")?;
        let mut records = log
            .split_inclusive(|byte| *byte == b'\n')
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        let position = records
            .len()
            .checked_sub(index.saturating_add(1))
            .ok_or("The selected stash no longer exists")?;
        let fields = stash_log_fields(&records[position])?;
        if fields.1 != expected.as_str().as_bytes() {
            return Err("The selected stash changed; refresh before dropping it".into());
        }
        let previous = fields.0.to_vec();
        // Validate every remaining object id before preparing any write.
        for record in &records {
            let (_, new, _) = stash_log_fields(record)?;
            Oid::parse_object(String::from_utf8_lossy(new).into_owned())
                .map_err(|error| error.to_string())?;
        }
        let original_head = stash_log_fields(records.last().ok_or("The stash log is empty")?)?
            .1
            .to_vec();
        let loose = locked_contents(&reference_lock, OsStr::new("stash"))?;
        let packed = locked_contents(&packed_lock, OsStr::new("packed-refs"))?;
        let packed_stash = packed.as_ref().and_then(|bytes| {
            bytes
                .split(|byte| *byte == b'\n')
                .find_map(|line| line.strip_suffix(b" refs/stash").map(<[u8]>::to_vec))
        });
        let current_head = loose
            .as_ref()
            .map(|bytes| bytes.strip_suffix(b"\n").unwrap_or(bytes).to_vec())
            .or(packed_stash);
        if current_head.as_deref() != Some(original_head.as_slice()) {
            return Err("The stash ref and reflog disagree; both were preserved".into());
        }
        records.remove(position);
        if let Some(next) = records.get_mut(position) {
            let (_, _, old_length) = stash_log_fields(next)?;
            next.splice(..old_length, previous);
        }
        let next_head = records
            .last()
            .map(|record| stash_log_fields(record).map(|(_, new, _)| new.to_vec()))
            .transpose()?;
        let log_bytes = records.concat();
        let log_mode = locked_mode(&log_lock, OsStr::new("stash"), 0o600);
        let ref_mode = locked_mode(&reference_lock, OsStr::new("stash"), log_mode);
        let packed_mode = locked_mode(&packed_lock, OsStr::new("packed-refs"), log_mode);
        log_lock.prepare_contents(&log_bytes, log_mode)?;
        if let Some(head) = &next_head {
            let mut bytes = head.clone();
            bytes.push(b'\n');
            reference_lock.prepare_contents(&bytes, ref_mode)?;
        }
        let packed_without_stash = packed.as_ref().map(|bytes| {
            let mut retained = Vec::with_capacity(bytes.len());
            let mut removed = false;
            for line in bytes.split_inclusive(|byte| *byte == b'\n') {
                if line
                    .strip_suffix(b"\n")
                    .unwrap_or(line)
                    .ends_with(b" refs/stash")
                {
                    removed = true;
                    continue;
                }
                if removed && line.starts_with(b"^") {
                    removed = false;
                    continue;
                }
                removed = false;
                retained.extend_from_slice(line);
            }
            retained
        });
        if next_head.is_none()
            && let Some(bytes) = &packed_without_stash
        {
            packed_lock.prepare_contents(bytes, packed_mode)?;
        }
        if cancellation.is_cancelled() {
            return Err("Stash deletion cancelled before changing refs".into());
        }
        // Like Git's files backend, publish the reflog before the ref rename.
        // All ref, reflog and packing locks remain held until publication ends.
        if log_bytes.is_empty() {
            log_lock
                .remove_contents(OsStr::new("stash"))
                .map_err(StashDropFailure::Uncertain)?;
        } else {
            log_lock
                .publish(OsStr::new("stash"))
                .map_err(StashDropFailure::Uncertain)?;
        }
        if next_head.is_some() {
            reference_lock
                .publish(OsStr::new("stash"))
                .map_err(StashDropFailure::Uncertain)?;
        } else {
            reference_lock
                .remove_contents(OsStr::new("stash"))
                .map_err(StashDropFailure::Uncertain)?;
            if packed_without_stash.is_some() {
                packed_lock
                    .publish(OsStr::new("packed-refs"))
                    .map_err(StashDropFailure::Uncertain)?;
            }
        }
        Ok(())
    }
}

// Keep the rest of a reflog line (identity, timestamp and subject) byte exact.
fn stash_log_fields(record: &[u8]) -> Result<(&[u8], &[u8], usize), String> {
    let old_end = record
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or("Malformed stash reflog")?;
    let new_start = old_end + 1;
    let new_end = record[new_start..]
        .iter()
        .position(|byte| *byte == b' ')
        .map(|end| new_start + end)
        .ok_or("Malformed stash reflog")?;
    let old = &record[..old_end];
    let new = &record[new_start..new_end];
    if old.len() != new.len()
        || !matches!(old.len(), 40 | 64)
        || !old.iter().chain(new).all(u8::is_ascii_hexdigit)
    {
        return Err("Malformed stash reflog object id".into());
    }
    Ok((old, new, old_end))
}

fn locked_contents(lock: &GitFileLock, name: &OsStr) -> Result<Option<Vec<u8>>, String> {
    lock.check()?;
    match fs_safety::open_beneath(&lock.parent, Path::new(name)) {
        Ok(mut file) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            Ok(Some(bytes))
        }
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn locked_mode(lock: &GitFileLock, name: &OsStr, fallback: u32) -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs_safety::metadata_beneath(&lock.parent, Path::new(name))
        .map(|metadata| metadata.mode() & 0o777)
        .unwrap_or(fallback)
}

impl GitFileLock {
    fn prepare_contents(&mut self, bytes: &[u8], mode: u32) -> Result<(), String> {
        self.check()?;
        let result = (|| {
            let mut file = &self.file;
            file.write_all(bytes)?;
            self.file
                .set_permissions(fs::Permissions::from_mode(mode))?;
            self.file.sync_all()
        })();
        // Even a partial write changes the lock's inode version. Track our
        // own change so cleanup can still release the lock on an I/O failure.
        self.version = Self::version(&self.file.metadata().map_err(|error| error.to_string())?);
        result.map_err(|error| error.to_string())?;
        self.check()
    }

    fn publish(&mut self, name: &OsStr) -> Result<(), String> {
        self.check()?;
        fs_safety::rename_within(&self.parent, &self.name, name)
            .map_err(|error| error.to_string())?;
        self.active = false;
        fs_safety::sync_directory(&self.parent).map_err(|error| error.to_string())
    }

    fn remove_contents(&self, name: &OsStr) -> Result<(), String> {
        self.check()?;
        match fs_safety::unlink_within(&self.parent, name) {
            Ok(()) => fs_safety::sync_directory(&self.parent).map_err(|error| error.to_string()),
            Err(error) if error.is_not_found() => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}
