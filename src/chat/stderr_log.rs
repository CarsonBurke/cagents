//! Private bounded provider diagnostics. File locks protect live writers and
//! retained chat readers; only closed, unleased logs participate in retention.
use super::ProviderExitLog;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        fs::{FileExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

pub(crate) const MAX_BYTES: u64 = 8 * 1024 * 1024;
const CLOSED_BUDGET: u64 = 128 * 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Descriptor {
    pub path: PathBuf,
    pub log: ProviderExitLog,
}
#[derive(Clone)]
pub(crate) struct Lease {
    file: Arc<File>,
    pub descriptor: Descriptor,
}
impl Lease {
    pub fn open(descriptor: Descriptor) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&descriptor.path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || descriptor.log.bytes > MAX_BYTES
        {
            return Err(io::Error::other(
                "Provider output log identity or permissions are invalid",
            ));
        }
        lock(&file, libc::LOCK_SH | libc::LOCK_NB)?;
        Ok(Self {
            file: Arc::new(file),
            descriptor,
        })
    }
    pub fn read(&self, offset: u64) -> Result<(Vec<u8>, Option<u64>, u64), String> {
        let retained = self.descriptor.log.bytes.min(MAX_BYTES);
        if offset > retained {
            return Err("Provider output cursor exceeds the retained log".into());
        }
        let count = (retained - offset).min(256 * 1024) as usize;
        let mut raw = vec![0; count];
        self.file
            .read_exact_at(&mut raw, offset)
            .map_err(|error| format!("Retained provider output is unavailable: {error}"))?;
        let next = offset + count as u64;
        Ok((
            raw,
            (next < retained).then_some(next),
            self.descriptor.log.omitted,
        ))
    }
}

pub(crate) struct Capture {
    file: Arc<File>,
    descriptor: Descriptor,
    total: u64,
    failed: bool,
}
impl Capture {
    pub fn new() -> io::Result<Self> {
        #[cfg(not(test))]
        let directory = crate::instance::project_dirs()
            .ok_or_else(|| io::Error::other("Provider diagnostic cache directory is unavailable"))?
            .cache_dir()
            .join("provider-stderr");
        #[cfg(test)]
        let directory = {
            // Actor tests must never prune the user's production diagnostics.
            static DIRECTORY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
            DIRECTORY
                .get_or_init(|| {
                    PathBuf::from("/var/tmp")
                        .join(format!("cagents-stderr-tests-{}", uuid::Uuid::new_v4()))
                })
                .clone()
        };
        Self::at(&directory, CLOSED_BUDGET, RETENTION)
    }
    fn at(directory: &Path, budget: u64, retention: Duration) -> io::Result<Self> {
        crate::secure_file::prepare_private_directory(directory)?;
        prune(directory, budget, retention)?;
        let id = uuid::Uuid::new_v4().to_string();
        let path = directory.join(format!("{id}.log"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        lock(&file, libc::LOCK_EX | libc::LOCK_NB)?;
        Ok(Self {
            file: Arc::new(file),
            descriptor: Descriptor {
                path,
                log: ProviderExitLog {
                    id,
                    bytes: 0,
                    omitted: 0,
                },
            },
            total: 0,
            failed: false,
        })
    }
    pub fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let keep = (MAX_BYTES - self.descriptor.log.bytes).min(bytes.len() as u64) as usize;
        if !self.failed && keep > 0 {
            if (&*self.file).write_all(&bytes[..keep]).is_err() {
                self.failed = true;
                self.descriptor.log.bytes = self
                    .file
                    .metadata()
                    .map(|metadata| metadata.len().min(MAX_BYTES))
                    .unwrap_or(self.descriptor.log.bytes);
                tracing::warn!("Provider diagnostic log could not retain additional output");
            } else {
                self.descriptor.log.bytes += keep as u64;
            }
        }
        self.descriptor.log.omitted = self.total.saturating_sub(self.descriptor.log.bytes);
    }
    pub fn lease(&self) -> Lease {
        Lease {
            file: self.file.clone(),
            descriptor: self.descriptor.clone(),
        }
    }
    /// A confirmed exited holder shares its immutable diagnostic with the
    /// adopting daemon before dropping its own lease, leaving no pruning gap.
    pub fn share(&self) -> io::Result<Descriptor> {
        lock(&self.file, libc::LOCK_SH)?;
        Ok(self.descriptor.clone())
    }
}
fn lock(file: &File, operation: i32) -> io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn prune(directory: &Path, budget: u64, retention: Duration) -> io::Result<()> {
    let mut closed = Vec::new();
    let mut bytes = 0u64;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("log") {
            continue;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(entry.path())
        else {
            continue;
        };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || lock(&file, libc::LOCK_EX | libc::LOCK_NB).is_err()
        {
            continue;
        }
        bytes = bytes.saturating_add(metadata.len());
        closed.push((
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            metadata.len(),
            entry.path(),
        ));
    }
    closed.sort_by_key(|entry| entry.0);
    let mut files = closed.len();
    for (modified, length, path) in closed {
        if bytes <= budget
            && files <= 512
            && SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                < retention
        {
            continue;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
        else {
            continue;
        };
        if lock(&file, libc::LOCK_EX | libc::LOCK_NB).is_ok() && fs::remove_file(path).is_ok() {
            bytes = bytes.saturating_sub(length);
            files = files.saturating_sub(1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_logs_retain_complete_output_until_the_cap_and_report_omissions() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let path = directory.path().join("logs");
        let mut capture = Capture::at(&path, CLOSED_BUDGET, RETENTION).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        capture.push(b"first\n\x1b[31mAuthentication failed\x1b[0m\n");
        let lease = capture.lease();
        assert_eq!(lease.file.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            lease.read(0).unwrap(),
            (
                b"first\n\x1b[31mAuthentication failed\x1b[0m\n".to_vec(),
                None,
                0
            )
        );
        capture.push(&vec![b'x'; MAX_BYTES as usize]);
        capture.push(b"more stderr after cap");
        let lease = capture.lease();
        assert_eq!(lease.descriptor.log.bytes, MAX_BYTES);
        assert_eq!(lease.file.metadata().unwrap().len(), MAX_BYTES);
        assert!(lease.descriptor.log.omitted > 0);
        let mut cursor = 0;
        let mut read = 0;
        loop {
            let (chunk, next, omitted) = lease.read(cursor).unwrap();
            assert!(chunk.len() <= 256 * 1024);
            assert_eq!(omitted, lease.descriptor.log.omitted);
            read += chunk.len();
            if let Some(next) = next {
                cursor = next;
            } else {
                break;
            }
        }
        assert_eq!(read, MAX_BYTES as usize);
        assert!(lease.read(MAX_BYTES + 1).is_err());
    }
    #[test]
    fn retention_prunes_only_closed_unleased_logs_and_respects_age_and_budget() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut live = Capture::at(directory.path(), 128, RETENTION).unwrap();
        live.push(b"protected");
        let live_path = live.descriptor.path.clone();
        let mut closed = Capture::at(directory.path(), 128, RETENTION).unwrap();
        closed.push(&[b'x'; 64]);
        let closed_path = closed.descriptor.path.clone();
        drop(closed);
        prune(directory.path(), 32, RETENTION).unwrap();
        assert!(!closed_path.exists());
        assert!(live_path.exists());
        let reader = live.lease();
        drop(live);
        prune(directory.path(), 0, Duration::ZERO).unwrap();
        assert!(live_path.exists());
        drop(reader);
        prune(directory.path(), 0, Duration::ZERO).unwrap();
        assert!(!live_path.exists());
    }
    #[test]
    fn holder_to_daemon_shared_lease_prevents_pruning_after_writer_drops() {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let mut capture = Capture::at(directory.path(), 128, RETENTION).unwrap();
        capture.push(b"complete stderr");
        let descriptor = capture.share().unwrap();
        let lease = Lease::open(descriptor.clone()).unwrap();
        drop(capture);
        prune(directory.path(), 0, Duration::ZERO).unwrap();
        assert!(descriptor.path.exists());
        assert_eq!(lease.read(0).unwrap().0, b"complete stderr");
        drop(lease);
        prune(directory.path(), 0, Duration::ZERO).unwrap();
        assert!(!descriptor.path.exists());
    }
}
