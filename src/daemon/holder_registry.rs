//! Private discovery evidence shared by terminal and native-provider holders.
//! Only process identities go to disk; recipes and environment stay in memory.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::model::{BootId, TerminalId};

mod connection;
pub(crate) use connection::connect_until;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    #[default]
    Terminal,
    Native,
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

/// Behavioural identity is separate from the wire protocol: additive fields
/// keep existing durable helpers adoptable while exposing upgrades to users.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BuildIdentity {
    pub version: String,
    pub commit: Option<String>,
    pub capabilities: Vec<String>,
}

impl BuildIdentity {
    pub(crate) fn current(kind: Kind) -> Self {
        Self {
            version: crate::VERSION.into(),
            commit: crate::VERSION
                .split("+local.")
                .nth(1)
                .and_then(|metadata| metadata.split('.').next())
                .map(str::to_owned),
            capabilities: match kind {
                Kind::Native => vec!["event-driven-orphan-reaping".into()],
                Kind::Terminal => Vec::new(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub version: u32,
    #[serde(default)]
    pub build: Option<BuildIdentity>,
    /// Older terminal records predate native holders. Their existing private
    /// directory and absent kind continue to identify them as terminals.
    #[serde(default)]
    pub kind: Kind,
    pub application: String,
    pub boot: BootId,
    pub terminal: TerminalId,
    pub holder: Process,
    pub root: Process,
}

pub(crate) const RETAINED_CLAUDE_CAPABILITY: &str = "claude-actor-v2";

/// Retained Claude discovery visibility only. Attachment still requires the
/// original record, authenticated peer, daemon lease and actor epoch. Ordinary
/// terminal discovery and native Codex matching remain application-exact.
pub(crate) fn record_application_matches(record: &Record) -> bool {
    record_application_matches_for(
        record,
        crate::instance::APPLICATION_ID,
        crate::instance::IS_DEVELOPMENT,
    )
}

fn record_application_matches_for(record: &Record, current: &str, development: bool) -> bool {
    record.application == current
        || (!development
            && record.kind == Kind::Native
            && production_role_pair(&record.application, current)
            && record.build.as_ref().is_some_and(|build| {
                build
                    .capabilities
                    .iter()
                    .any(|capability| capability == RETAINED_CLAUDE_CAPABILITY)
            }))
}

/// Read-only recovery intake may inspect the other production package's original
/// holder. This is not adoption or cleanup authority: those paths must retain
/// an authenticated exact-record transition after checking live issued lineage.
fn recovery_role(application: &str) -> bool {
    production_role_pair(application, crate::instance::APPLICATION_ID)
}

pub(crate) fn production_role_pair(original: &str, successor: &str) -> bool {
    matches!(
        (original, successor),
        ("dev.cagents.Cagents", "dev.cagents.Cagents.Browserless")
            | ("dev.cagents.Cagents.Browserless", "dev.cagents.Cagents")
    )
}

/// Private, process-local custody established only after authenticated Inspect
/// or an exact durable cleanup receipt, and exact issued-source matching. It never changes the original Record.
#[derive(Clone)]
pub(crate) struct OriginalRole {
    record: Record,
    original: crate::model::HostSessionProof,
    successor: crate::model::HostSessionProof,
    application: &'static str,
}

impl OriginalRole {
    pub(crate) fn authenticated(
        record: &Record,
        original: &crate::model::HostSessionProof,
        successor: &crate::model::HostSessionProof,
    ) -> Result<Option<Self>, String> {
        if record.application == crate::instance::APPLICATION_ID {
            return Ok(None);
        }
        if !recovery_role(&record.application)
            || !original.is_complete()
            || !successor.is_complete()
            || original.boot_id != record.boot
            || successor.boot_id != record.boot
        {
            return Err("Holder role transition lacks exact same-boot issued ownership".into());
        }
        let paths = crate::daemon::RuntimePaths::discover().map_err(|e| e.to_string())?;
        if crate::daemon::lease_owner(&paths.lock, successor).map_err(|e| e.to_string())?
            != std::process::id()
        {
            return Err("Holder role transition caller no longer owns the daemon lease".into());
        }
        Ok(Some(Self {
            record: record.clone(),
            original: original.clone(),
            successor: successor.clone(),
            application: crate::instance::APPLICATION_ID,
        }))
    }

    pub(crate) fn matches(&self, record: &Record, proof: &crate::model::HostSessionProof) -> bool {
        self.application == crate::instance::APPLICATION_ID && self.matches_original(record, proof)
    }

    fn matches_original(&self, record: &Record, proof: &crate::model::HostSessionProof) -> bool {
        self.record == *record
            && self.successor == *proof
            && self.original.is_complete()
            && self.original.boot_id == record.boot
            && proof.is_complete()
            && proof.boot_id == record.boot
            && production_role_pair(&record.application, self.application)
    }
}

pub(crate) fn directory(kind: Kind) -> Result<PathBuf, String> {
    crate::daemon::RuntimePaths::discover()
        .map(|paths| {
            paths.directory.join(match kind {
                Kind::Terminal => "holders",
                Kind::Native => "native-holders",
            })
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn socket(directory: &Path, terminal: TerminalId) -> PathBuf {
    directory.join(terminal.to_string())
}

/// A Unix address includes its terminating NUL. Filesystem paths have no such
/// small limit; Linux can resolve a long endpoint through a held directory FD.
pub(crate) fn socket_fits(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: sockaddr_un is plain data and only its field length is read.
    let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len();
    path.as_os_str().as_bytes().len() < capacity
}

fn with_socket_address<T>(
    path: &Path,
    operation: impl FnOnce(&Path) -> io::Result<T>,
) -> io::Result<T> {
    use std::os::fd::AsRawFd;
    if socket_fits(path) {
        return operation(path);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "holder endpoint has no directory",
            )
        })?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "holder endpoint has no filename",
        )
    })?;
    crate::secure_file::reject_symlink_components(parent)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe holder socket directory",
        ));
    }
    // Hold the FD until bind/connect completes. The endpoint remains at its
    // original filesystem path, so discovery, authentication, and inode-based
    // retirement retain their existing identity and survive daemon upgrades.
    let address = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
    operation(&address)
}

pub(crate) fn bind(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    with_socket_address(path, |address| {
        std::os::unix::net::UnixListener::bind(address)
    })
}

pub(crate) fn connect(path: &Path) -> io::Result<std::os::unix::net::UnixStream> {
    with_socket_address(path, |address| {
        std::os::unix::net::UnixStream::connect(address)
    })
}

impl Record {
    pub(crate) fn is_outdated(&self) -> bool {
        self.build.as_ref().is_none_or(|build| {
            build.version != crate::VERSION
                || (self.kind == Kind::Native
                    && !build
                        .capabilities
                        .iter()
                        .any(|capability| capability == "event-driven-orphan-reaping"))
        })
    }

    /// A diagnostic snapshot, never process-control authority. Verify the
    /// holder's start identity both before and after scanning its direct children.
    pub(crate) fn zombie_children(&self) -> io::Result<usize> {
        if !self.holder.is_current() {
            return Err(io::Error::other("holder process identity changed"));
        }
        // Linux records children on the thread that created them, even though
        // their stat parent is the process leader. Inspect every live thread.
        let mut children = std::collections::BTreeSet::new();
        for task in fs::read_dir(format!("/proc/{}/task", self.holder.pid))? {
            let task = task?;
            let raw = match fs::read_to_string(task.path().join("children")) {
                Ok(raw) => raw,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            children.extend(
                raw.split_whitespace()
                    .filter_map(|pid| pid.parse::<i32>().ok()),
            );
        }
        let zombies = children
            .into_iter()
            .filter(|pid| {
                fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|stat| {
                        stat.rsplit_once(')').map(|(_, fields)| {
                            let mut fields = fields.split_whitespace();
                            fields.next() == Some("Z")
                                && fields.next().and_then(|parent| parent.parse::<i32>().ok())
                                    == Some(self.holder.pid)
                        })
                    })
                    .unwrap_or(false)
            })
            .count();
        if !self.holder.is_current() {
            return Err(io::Error::other("holder process identity changed"));
        }
        Ok(zombies)
    }

    fn supported_version(&self) -> bool {
        match self.kind {
            Kind::Terminal => matches!(self.version, 3 | 4 | 5),
            Kind::Native => matches!(self.version, 2 | 3 | 4),
        }
    }

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

    /// Retire discovery after the owner has independently unlinked its exact
    /// socket inode. Never touch a successor endpoint published in between.
    pub fn remove_record(&self, directory: &Path) -> io::Result<()> {
        let path = self.path(directory);
        match read(&path) {
            Ok(record) if record == *self => match fs::remove_file(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            },
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn read(path: &Path) -> io::Result<Record> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    read_file(file, false)
}

fn read_file(file: std::fs::File, allow_unlinked: bool) -> io::Result<Record> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
        || (metadata.nlink() != 1 && !(allow_unlinked && metadata.nlink() == 0))
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

/// Check discovery retirement only after the caller authenticated an exact
/// teardown receipt. A just-opened record may already be unlinked when fstat
/// runs; reading that held inode still reports pending until the next poll.
/// It never grants adoption authority or turns unsafe evidence into absence.
pub(crate) fn retirement_pending(expected: &Record) -> io::Result<bool> {
    let path = expected.path(&directory(expected.kind).map_err(io::Error::other)?);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    read_file(file, true).map(|actual| actual == *expected)
}

pub(crate) fn evidence(socket: &Path, kind: Kind) -> Result<Option<Record>, String> {
    evidence_inner(socket, kind, false)
}

/// Untrusted routing evidence for the recovery Inspect handshake only. Ordinary
/// discovery stays application-exact; no caller may infer adoption from this.
pub(crate) fn recovery_candidate(socket: &Path, kind: Kind) -> Result<Option<Record>, String> {
    evidence_inner(socket, kind, true)
}

fn evidence_inner(socket: &Path, kind: Kind, recovery: bool) -> Result<Option<Record>, String> {
    let path = socket.with_extension("json");
    let record = match read(&path) {
        Ok(record) => record,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let boot = crate::daemon::read_boot_id().map_err(|error| error.to_string())?;
    if !record.supported_version()
        || record.kind != kind
        || (record.application != crate::instance::APPLICATION_ID
            && !(recovery && recovery_role(&record.application)))
        || record.boot != boot
        || socket.file_name() != Some(std::ffi::OsStr::new(&record.terminal.to_string()))
    {
        return Err("holder discovery evidence does not match this endpoint".into());
    }
    Ok(Some(record))
}

/// Read-only runtime evidence for diagnostics. Unlike recovery discovery this
/// never creates directories, removes stale files, or contacts a helper.
pub(crate) fn observed(kind: Kind) -> io::Result<Vec<Record>> {
    let directory = directory(kind).map_err(io::Error::other)?;
    crate::secure_file::reject_symlink_components(&directory)?;
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe holder directory",
        ));
    }
    let boot = crate::daemon::read_boot_id()?;
    let mut records = Vec::new();
    for (index, entry) in fs::read_dir(&directory)?.enumerate() {
        if index >= 8192 {
            return Err(io::Error::other("holder discovery limit exceeded"));
        }
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let record = read(&path)?;
        if record.kind == kind
            && record.supported_version()
            && record.application == crate::instance::APPLICATION_ID
            && record.boot == boot
            && record.path(&directory) == path
            && record.holder.is_current()
        {
            records.push(record);
        }
    }
    Ok(records)
}

/// Strict, read-only ownership census. Unlike recovery intake, an unreadable,
/// unsupported or unproved current-boot row cannot disappear from the answer.
/// Callers still authenticate each returned holder before using its source.
pub(crate) fn recovery_census(
    directory: &Path,
    boot: &BootId,
    kind: Kind,
) -> Result<Vec<Record>, String> {
    crate::secure_file::reject_symlink_components(directory).map_err(|error| error.to_string())?;
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_dir()
        || metadata.uid() != crate::daemon::current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err("Unsafe holder census directory".into());
    }
    let mut records = Vec::new();
    for (index, entry) in fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .enumerate()
    {
        if index >= 8192 {
            return Err("Holder census exceeds its directory bound".into());
        }
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let record = read(&path)
            .map_err(|error| format!("Holder census has unreadable ownership: {error}"))?;
        if &record.boot != boot {
            // Authenticated private records from a prior kernel boot cannot
            // own a current process; their durable source provenance is separate.
            continue;
        }
        if !record.supported_version()
            || record.kind != kind
            || (record.application != crate::instance::APPLICATION_ID
                && !recovery_role(&record.application))
            || record.path(directory) != path
        {
            return Err("Holder census has unsupported or inconsistent ownership".into());
        }
        records.push(record);
        if records.len() > 4096 {
            return Err("Holder census exceeds its session bound".into());
        }
    }
    Ok(records)
}

pub(crate) fn discover(directory: &Path, boot: &BootId, kind: Kind) -> Result<Vec<Record>, String> {
    discover_inner(directory, boot, kind, false)
}

pub(crate) fn recovery_candidates(
    directory: &Path,
    boot: &BootId,
    kind: Kind,
) -> Result<Vec<Record>, String> {
    discover_inner(directory, boot, kind, true)
}

fn discover_inner(
    directory: &Path,
    boot: &BootId,
    kind: Kind,
    recovery: bool,
) -> Result<Vec<Record>, String> {
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
        if !record.supported_version()
            || record.kind != kind
            || (record.application != crate::instance::APPLICATION_ID
                && !(recovery && recovery_role(&record.application)))
            || record.path(directory) != path
            || &record.boot != boot
        {
            tracing::warn!(path = %path.display(), "ignored unrelated holder record");
            continue;
        }
        if !record.holder.is_current() {
            // Candidate intake grants no mutation authority over the other
            // package's evidence, including an unresolved orphan root.
            if record.application == crate::instance::APPLICATION_ID {
                record.remove(directory);
            }
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

    #[test]
    fn strict_recovery_census_preserves_dead_and_malformed_ownership() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let boot = crate::daemon::read_boot_id().unwrap();
        let mut dead = Process::read(std::process::id() as i32).unwrap();
        dead.start += 1;
        let record = Record {
            version: 4,
            build: None,
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: boot.clone(),
            terminal: TerminalId::new(),
            holder: dead.clone(),
            root: dead,
        };
        let path = record.path(directory.path());
        fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            recovery_census(directory.path(), &boot, Kind::Native).unwrap(),
            vec![record]
        );
        assert!(
            path.exists(),
            "census never consumes dead discovery evidence"
        );
        fs::write(&path, b"malformed ownership").unwrap();
        assert!(recovery_census(directory.path(), &boot, Kind::Native).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"malformed ownership");
    }

    #[test]
    fn retained_claude_family_visibility_keeps_original_record_and_codex_isolation() {
        const FULL: &str = "dev.cagents.Cagents";
        const BROWSERLESS: &str = "dev.cagents.Cagents.Browserless";
        for (current, prior) in [(FULL, BROWSERLESS), (BROWSERLESS, FULL)] {
            let mut actor = record();
            actor.version = 4;
            actor.kind = Kind::Native;
            actor.application = prior.into();
            actor.build = Some(BuildIdentity::current(Kind::Native));
            let original = serde_json::to_vec(&actor).unwrap();
            assert!(
                !record_application_matches_for(&actor, current, false),
                "native Codex stays exact"
            );
            actor
                .build
                .as_mut()
                .unwrap()
                .capabilities
                .push("claude-actor-v2".into());
            let original_actor = serde_json::to_vec(&actor).unwrap();
            assert!(record_application_matches_for(&actor, current, false));
            assert!(
                !record_application_matches_for(&actor, current, true),
                "development cannot acquire production actors"
            );
            assert!(!record_application_matches_for(
                &actor,
                "another.installation",
                false
            ));
            assert_eq!(
                serde_json::to_vec(&actor).unwrap(),
                original_actor,
                "visibility never retags application/process provenance"
            );
            actor.build.as_mut().unwrap().capabilities.pop();
            assert_eq!(serde_json::to_vec(&actor).unwrap(), original);
            assert!(!record_application_matches_for(&actor, current, false));
            actor.build = None;
            assert!(
                !record_application_matches_for(&actor, current, false),
                "legacy records cannot infer the actor protocol"
            );
        }
        let mut terminal = record();
        terminal.application = BROWSERLESS.into();
        terminal.kind = Kind::Terminal;
        terminal
            .build
            .as_mut()
            .unwrap()
            .capabilities
            .push(RETAINED_CLAUDE_CAPABILITY.into());
        assert!(
            !record_application_matches_for(&terminal, FULL, false),
            "Claude capability cannot widen terminal discovery"
        );
        terminal.application = FULL.into();
        assert!(record_application_matches_for(&terminal, FULL, false));
        let mut foreign = record();
        foreign.kind = Kind::Native;
        foreign.application = "dev.cagents.Cagents.Development".into();
        foreign
            .build
            .as_mut()
            .unwrap()
            .capabilities
            .push("claude-actor-v2".into());
        assert!(!record_application_matches_for(&foreign, FULL, false));
        foreign.application = "another.installation".into();
        assert!(!record_application_matches_for(
            &foreign,
            BROWSERLESS,
            false
        ));
    }

    #[test]
    fn recovery_roles_are_only_the_two_distinct_production_packages() {
        let full = "dev.cagents.Cagents";
        let browserless = "dev.cagents.Cagents.Browserless";
        assert!(production_role_pair(full, browserless));
        assert!(production_role_pair(browserless, full));
        for unrelated in [
            "dev.cagents.Cagents.Development",
            "another-installation",
            "",
            full,
        ] {
            assert!(!production_role_pair(full, unrelated));
            assert!(!production_role_pair(unrelated, full));
        }
    }

    #[test]
    fn retained_role_capability_cannot_move_to_another_record_or_daemon() {
        // Pure capability/data oracle. This does not impersonate a production
        // executable or claim the real cross-package/native qualification.
        let mut original = record();
        original.application = "dev.cagents.Cagents.Browserless".into();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("original-role-test"),
            host_incarnation: crate::model::HostIncarnation::new("successor"),
            boot_id: original.boot.clone(),
        };
        let role = OriginalRole {
            record: original.clone(),
            original: proof.clone(),
            successor: proof.clone(),
            application: "dev.cagents.Cagents",
        };
        assert!(role.matches_original(&original, &proof));
        let mut changed = original.clone();
        changed.root.start += 1;
        assert!(!role.matches_original(&changed, &proof));
        changed = original.clone();
        changed.holder.start += 1;
        assert!(!role.matches_original(&changed, &proof));
        changed = original.clone();
        changed.terminal = TerminalId::new();
        assert!(!role.matches_original(&changed, &proof));
        changed = original.clone();
        changed.application = "dev.cagents.Cagents".into();
        assert!(!role.matches_original(&changed, &proof));
        let mut other = proof.clone();
        other.host_incarnation = crate::model::HostIncarnation::new("different-daemon");
        assert!(!role.matches_original(&original, &other));
        if crate::instance::IS_DEVELOPMENT {
            assert!(
                !role.matches(&original, &proof),
                "production capability entered development"
            );
            assert!(OriginalRole::authenticated(&original, &proof, &proof).is_err());
        }
    }

    #[test]
    fn ordinary_discovery_refuses_other_production_role_without_mutating_it() {
        // This is on-disk untrusted evidence, never a spawned/renamed helper.
        // Actual cross-role adoption requires separately built real products.
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut foreign = record();
        foreign.application =
            if crate::instance::APPLICATION_ID == "dev.cagents.Cagents.Browserless" {
                "dev.cagents.Cagents"
            } else {
                "dev.cagents.Cagents.Browserless"
            }
            .into();
        assert_ne!(foreign.application, crate::instance::APPLICATION_ID);
        foreign.create(directory.path()).unwrap();
        let endpoint = socket(directory.path(), foreign.terminal);
        let path = foreign.path(directory.path());
        let before = fs::read(&path).unwrap();
        assert!(evidence(&endpoint, Kind::Terminal).is_err());
        assert!(
            discover(directory.path(), &foreign.boot, Kind::Terminal)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        if crate::instance::IS_DEVELOPMENT {
            assert!(recovery_candidate(&endpoint, Kind::Terminal).is_err());
            assert!(
                recovery_candidates(directory.path(), &foreign.boot, Kind::Terminal)
                    .unwrap()
                    .is_empty()
            );
        } else {
            assert_eq!(
                recovery_candidate(&endpoint, Kind::Terminal).unwrap(),
                Some(foreign.clone())
            );
            assert_eq!(
                recovery_candidates(directory.path(), &foreign.boot, Kind::Terminal).unwrap(),
                vec![foreign.clone()]
            );
            let missing = crate::model::HostSessionProof {
                host_session_id: crate::model::HostSessionId::new(""),
                host_incarnation: crate::model::HostIncarnation::new(""),
                boot_id: foreign.boot.clone(),
            };
            assert!(
                OriginalRole::authenticated(&foreign, &missing, &missing).is_err(),
                "candidate became authority without issued proof"
            );
        }
        assert_eq!(read(&path).unwrap(), foreign);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    fn record() -> Record {
        let process = Process::read(std::process::id() as i32).unwrap();
        Record {
            version: 3,
            build: Some(BuildIdentity::current(Kind::Terminal)),
            kind: Kind::Terminal,
            application: crate::instance::APPLICATION_ID.into(),
            boot: crate::daemon::read_boot_id().unwrap(),
            terminal: TerminalId::new(),
            holder: process.clone(),
            root: process,
        }
    }

    fn endpoint_with_byte_length(root: &Path, length: usize) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        let prefix = root.as_os_str().as_bytes().len() + 1;
        // A multibyte component makes byte accounting distinct from characters.
        let directory = root.join(format!("é{}", "x".repeat(length - prefix - 2 - 1 - 36)));
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        let endpoint = socket(&directory, TerminalId::new());
        assert_eq!(endpoint.as_os_str().as_bytes().len(), length);
        endpoint
    }

    #[test]
    fn unix_address_boundary_preserves_full_endpoint_and_discovery_identity() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        // SAFETY: plain data, only the array length is used.
        let capacity = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
            .sun_path
            .len();
        for length in [capacity - 1, capacity, capacity + 1, capacity + 100] {
            let endpoint = endpoint_with_byte_length(root.path(), length);
            assert_eq!(socket_fits(&endpoint), length < capacity);
            let listener = bind(&endpoint).unwrap();
            assert!(std::os::unix::fs::FileTypeExt::is_socket(
                &fs::symlink_metadata(&endpoint).unwrap().file_type()
            ));
            let client = connect(&endpoint).unwrap();
            let (server, _) = listener.accept().unwrap();
            assert!(client.peer_addr().unwrap().as_pathname().is_some());
            drop((client, server, listener));
            let mut proof = record();
            proof.terminal = endpoint
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            proof.create(endpoint.parent().unwrap()).unwrap();
            assert_eq!(
                evidence(&endpoint, Kind::Terminal).unwrap(),
                Some(proof.clone())
            );
            assert_eq!(
                discover(endpoint.parent().unwrap(), &proof.boot, Kind::Terminal).unwrap(),
                vec![proof.clone()]
            );
            proof.remove(endpoint.parent().unwrap());
            assert!(!endpoint.exists());
        }
    }

    #[test]
    fn long_endpoints_reject_public_or_symlinked_directories() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        let endpoint = endpoint_with_byte_length(root.path(), 160);
        let parent = endpoint.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            bind(&endpoint).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.path().join("l".repeat(100));
        std::os::unix::fs::symlink(parent, &link).unwrap();
        assert_eq!(
            bind(&link.join(endpoint.file_name().unwrap()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn authenticated_retirement_tolerates_only_a_just_unlinked_record() {
        let directory = tempfile::tempdir().unwrap();
        let proof = record();
        proof.create(directory.path()).unwrap();
        let path = proof.path(directory.path());
        let held = fs::File::open(&path).unwrap();
        // Deterministically retire after open and before metadata inspection.
        fs::remove_file(&path).unwrap();
        assert_eq!(held.metadata().unwrap().nlink(), 0);
        assert_eq!(
            read_file(held.try_clone().unwrap(), false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(read_file(held, true).unwrap(), proof);
        proof.create(directory.path()).unwrap();
        fs::hard_link(&path, directory.path().join("extra-link")).unwrap();
        assert_eq!(
            read_file(fs::File::open(&path).unwrap(), true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::remove_file(directory.path().join("extra-link")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let unsafe_held = fs::File::open(&path).unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(
            read_file(unsafe_held, true).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn record_only_retirement_preserves_replacement_endpoint_and_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let old = record();
        old.create(directory.path()).unwrap();
        let endpoint = socket(directory.path(), old.terminal);
        fs::write(&endpoint, "replacement endpoint").unwrap();
        old.remove_record(directory.path()).unwrap();
        assert_eq!(
            fs::read_to_string(&endpoint).unwrap(),
            "replacement endpoint"
        );
        let mut successor = old.clone();
        successor.holder.start += 1;
        successor.create(directory.path()).unwrap();
        old.remove_record(directory.path()).unwrap();
        assert_eq!(read(&successor.path(directory.path())).unwrap(), successor);
        assert!(endpoint.exists());
    }

    #[test]
    fn terminal_receipt_protocol_and_known_native_build_are_discovered_but_unknown_native_is_refused()
     {
        let directory = tempfile::tempdir().unwrap();
        let mut terminal = record();
        terminal.version = 4;
        terminal.create(directory.path()).unwrap();
        assert_eq!(
            evidence(&socket(directory.path(), terminal.terminal), Kind::Terminal).unwrap(),
            Some(terminal.clone())
        );
        assert_eq!(
            discover(directory.path(), &terminal.boot, Kind::Terminal).unwrap(),
            vec![terminal.clone()]
        );
        let mut native = record();
        native.kind = Kind::Native;
        native.version = 4;
        native.create(directory.path()).unwrap();
        assert_eq!(
            evidence(&socket(directory.path(), native.terminal), Kind::Native).unwrap(),
            Some(native.clone())
        );
        assert_eq!(
            discover(directory.path(), &native.boot, Kind::Native).unwrap(),
            vec![native.clone()]
        );
        native.remove(directory.path());
        native.version = 5;
        native.create(directory.path()).unwrap();
        assert!(evidence(&socket(directory.path(), native.terminal), Kind::Native).is_err());
        assert!(
            discover(directory.path(), &native.boot, Kind::Native)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn terminal_directory_pin_protocol_is_discovered_but_future_evidence_is_preserved() {
        let directory = tempfile::tempdir().unwrap();
        for version in [3, 4, 5] {
            let mut terminal = record();
            terminal.version = version;
            terminal.create(directory.path()).unwrap();
            assert_eq!(
                evidence(&socket(directory.path(), terminal.terminal), Kind::Terminal).unwrap(),
                Some(terminal.clone())
            );
            assert_eq!(
                discover(directory.path(), &terminal.boot, Kind::Terminal).unwrap(),
                vec![terminal.clone()]
            );
            terminal.remove(directory.path());
        }

        let mut future = record();
        future.version = 6;
        future.create(directory.path()).unwrap();
        let before = fs::read(future.path(directory.path())).unwrap();
        assert!(evidence(&socket(directory.path(), future.terminal), Kind::Terminal).is_err());
        assert!(
            discover(directory.path(), &future.boot, Kind::Terminal)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(future.path(directory.path())).unwrap(), before);
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
            discover(directory.path(), &valid.boot, Kind::Terminal).unwrap(),
            vec![valid]
        );
        assert!(foreign.path(directory.path()).exists());
        assert!(old_boot.path(directory.path()).exists());
        assert!(!stale.path(directory.path()).exists());
        assert!(!socket(directory.path(), stale.terminal).exists());
        assert_eq!(fs::read_to_string(target).unwrap(), "foreign file");
    }

    #[test]
    fn discovery_keeps_native_and_legacy_terminal_ownership_separate() {
        let directory = tempfile::tempdir().unwrap();
        let terminal = record();
        let mut legacy = serde_json::to_value(&terminal).unwrap();
        legacy.as_object_mut().unwrap().remove("kind");
        let path = terminal.path(directory.path());
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut native = record();
        native.kind = Kind::Native;
        native.create(directory.path()).unwrap();
        assert_eq!(
            discover(directory.path(), &terminal.boot, Kind::Terminal).unwrap(),
            vec![terminal.clone()]
        );
        assert_eq!(
            discover(directory.path(), &terminal.boot, Kind::Native).unwrap(),
            vec![native.clone()]
        );
        assert!(evidence(&socket(directory.path(), terminal.terminal), Kind::Native).is_err());
        assert!(evidence(&socket(directory.path(), native.terminal), Kind::Terminal).is_err());
        assert!(native.path(directory.path()).exists());
        assert!(terminal.path(directory.path()).exists());
    }
    #[test]
    fn legacy_native_record_remains_adoptable_and_is_outdated() {
        let mut value = serde_json::to_value(record()).unwrap();
        value["version"] = serde_json::json!(2);
        value["kind"] = serde_json::json!("Native");
        value.as_object_mut().unwrap().remove("build");
        let legacy: Record = serde_json::from_value(value).unwrap();
        assert!(legacy.supported_version());
        assert!(legacy.is_outdated());
        let mut current = legacy;
        current.version = 4;
        current.build = Some(BuildIdentity::current(Kind::Native));
        assert!(!current.is_outdated());
        current.build.as_mut().unwrap().capabilities.clear();
        assert!(current.is_outdated());
    }

    #[test]
    fn zombie_count_observes_children_without_consuming_status() {
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::waitid(
                        libc::P_PID,
                        child.id(),
                        &mut status,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                },
                0
            );
            if unsafe { status.si_pid() } != 0 {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(record().zombie_children().unwrap() >= 1);
        assert!(
            child.wait().unwrap().success(),
            "diagnostics consumed the exit status"
        );
    }
}
