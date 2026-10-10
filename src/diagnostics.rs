//! Private, bounded production evidence. Free-form application data cannot be
//! passed to this API: messages, errors, payloads and paths stay outside records.
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const SCHEMA_VERSION: u16 = 1;
const GENERATIONS: usize = 5;
const FILE_BYTES: u64 = 2 * 1024 * 1024;
const RECORD_BYTES: usize = 1024;
const RECENT_RECORDS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    Ui,
    Daemon,
    Editor,
}
impl Product {
    fn filename(self, generation: usize) -> String {
        format!("{}.{generation}.jsonl", self.name())
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Ui => "cagents-ui",
            Self::Daemon => "cagentsd",
            Self::Editor => "ceditor",
        }
    }
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "cagents-ui" => Some(Self::Ui),
            "cagentsd" => Some(Self::Daemon),
            "ceditor" | "ceditor-development" => Some(Self::Editor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Busy,
    ReadOnly,
    DiskFull,
    PermissionDenied,
    StorageUnavailable,
    Corrupt,
    Conflict,
    Serialization,
    InvalidData,
    Io,
    Database,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Startup,
    Ready,
    StartupFailed,
    Exit {
        success: bool,
    },
    Panic {
        recovered: bool,
    },
    PersistenceStarted,
    PersistenceFailed {
        category: Failure,
        exhausted: bool,
    },
    PersistenceRecovered,
    WriterUnavailable,
    FinalFlush {
        complete: bool,
        unsaved: u32,
        refused: u32,
        category: Option<Failure>,
    },
    SettingsFailed {
        category: Failure,
    },
    ProtocolMismatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema_version: u16,
    pub product: Product,
    pub run: Uuid,
    pub pid: u32,
    pub at_unix_ms: u64,
    pub version: String,
    pub protocol: u16,
    pub event: Event,
}

static SINK: OnceLock<Mutex<Option<Recorder>>> = OnceLock::new();
static FAILED: AtomicBool = AtomicBool::new(false);
static FAILURE_PENDING: AtomicBool = AtomicBool::new(false);
pub const FAILURE_NOTICE: &str =
    "Private diagnostics are unavailable. Failures from this process may not be retained.";

fn fail_sink() {
    if !FAILED.swap(true, Ordering::AcqRel) {
        FAILURE_PENDING.store(true, Ordering::Release);
        // A fixed message, never the sink error (which may contain a path).
        eprintln!("cagents: {FAILURE_NOTICE}");
    }
}

pub fn take_failure_notice() -> bool {
    FAILURE_PENDING.swap(false, Ordering::AcqRel)
}
pub fn local_debug() -> bool {
    std::env::var_os("CAGENTS_LOCAL_DEBUG").as_deref() == Some(std::ffi::OsStr::new("1"))
}

pub fn state_path() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(".local/state"))
        })
        .ok_or_else(|| io::Error::other("state directory unavailable"))?;
    Ok(base.join(crate::instance::STORAGE_NAME).join("diagnostics"))
}

pub fn initialize(product: Product, version: &'static str) {
    SINK.get_or_init(|| {
        Mutex::new(
            match state_path().and_then(|path| Recorder::open_version(&path, product, version)) {
                Ok(recorder) => Some(recorder),
                Err(_) => {
                    fail_sink();
                    None
                }
            },
        )
    });
}

pub fn record(event: Event) {
    if FAILED.load(Ordering::Acquire) {
        return;
    }
    // Tests and library consumers without a process entrypoint have no sink.
    let Some(sink) = SINK.get() else {
        return;
    };
    let Ok(mut sink) = sink.lock() else {
        fail_sink();
        return;
    };
    if let Some(recorder) = sink.as_mut()
        && recorder.append(event).is_err()
    {
        *sink = None;
        fail_sink();
    }
}

/// A panic may interrupt a thread holding the ordinary writer mutex. Never
/// block that thread on its own lock or recursively format its payload.
pub fn record_panic(recovered: bool) {
    let Some(sink) = SINK.get() else {
        return;
    };
    if FAILED.load(Ordering::Acquire) {
        return;
    }
    let Ok(mut sink) = sink.try_lock() else {
        fail_sink();
        return;
    };
    if let Some(recorder) = sink.as_mut()
        && recorder.append(Event::Panic { recovered }).is_err()
    {
        *sink = None;
        fail_sink();
    }
}

fn unsafe_path() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe diagnostic storage")
}
fn c_name(name: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| unsafe_path())
}
fn open_at(parent: &File, name: &std::ffi::OsStr, flags: i32, mode: u32) -> io::Result<File> {
    let name = c_name(name)?;
    // SAFETY: owned descriptor and terminated name; the returned fd is uniquely owned.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

/// Hold every ancestor while descending; a renamed ancestor cannot redirect
/// subsequent operations. Existing shared sticky root directories are allowed
/// for private test/cache roots, but application directories are always 0700.
fn directory(path: &Path, create: bool) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(unsafe_path());
    }
    let mut parent = File::open("/")?;
    let parts: Vec<_> = path.components().collect();
    for (index, part) in parts.iter().enumerate().skip(1) {
        let Component::Normal(name) = part else {
            return Err(unsafe_path());
        };
        let child = match open_at(&parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
            Ok(file) => file,
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                let name_c = c_name(name)?;
                let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) };
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                    return Err(io::Error::last_os_error());
                }
                if result == 0 {
                    parent.sync_all()?;
                }
                open_at(&parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?
            }
            Err(error) => return Err(error),
        };
        let m = child.metadata()?;
        let uid = unsafe { libc::geteuid() };
        let sticky_root = m.uid() == 0 && m.mode() & 0o1000 != 0;
        if !m.is_dir()
            || (m.uid() != uid && m.uid() != 0)
            || (m.mode() & 0o022 != 0 && !sticky_root)
        {
            return Err(unsafe_path());
        }
        if index + 2 >= parts.len() && (m.uid() != uid || m.mode() & 0o777 != 0o700) {
            return Err(unsafe_path());
        }
        parent = child;
    }
    Ok(parent)
}

fn private_file(dir: &File, name: &str, flags: i32) -> io::Result<File> {
    let file = open_at(dir, name.as_ref(), flags | libc::O_NONBLOCK, 0o600)?;
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o777 != 0o600
        || m.nlink() != 1
    {
        return Err(unsafe_path());
    }
    Ok(file)
}
struct Lock(File);
impl Lock {
    fn acquire(file: File, exclusive: bool) -> io::Result<Self> {
        for _ in 0..10 {
            if unsafe {
                libc::flock(
                    file.as_raw_fd(),
                    (if exclusive {
                        libc::LOCK_EX
                    } else {
                        libc::LOCK_SH
                    }) | libc::LOCK_NB,
                )
            } == 0
            {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "diagnostic lock busy",
        ))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

struct Recorder {
    dir: File,
    product: Product,
    run: Uuid,
    limit: u64,
    version: &'static str,
}
impl Recorder {
    #[cfg(test)]
    fn open(path: &Path, product: Product) -> io::Result<Self> {
        Self::open_version(path, product, crate::VERSION)
    }
    fn open_version(path: &Path, product: Product, version: &'static str) -> io::Result<Self> {
        if version.len() > 128
            || semver::Version::parse(version).is_err()
            || !version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
        {
            return Err(unsafe_path());
        }
        Ok(Self {
            dir: directory(path, true)?,
            product,
            run: Uuid::new_v4(),
            limit: FILE_BYTES,
            version,
        })
    }
    fn append(&mut self, event: Event) -> io::Result<()> {
        let directory = self.dir.metadata()?;
        if directory.uid() != unsafe { libc::geteuid() } || directory.mode() & 0o777 != 0o700 {
            return Err(unsafe_path());
        }
        let _lock = Lock::acquire(
            private_file(
                &self.dir,
                &format!("{}.lock", self.product.name()),
                libc::O_RDWR | libc::O_CREAT,
            )?,
            true,
        )?;
        let record = Record {
            schema_version: SCHEMA_VERSION,
            product: self.product,
            run: self.run,
            pid: std::process::id(),
            at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            version: self.version.into(),
            protocol: crate::terminal::ipc::PROTOCOL_VERSION,
            event,
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        if bytes.len() > RECORD_BYTES || bytes.len() as u64 > self.limit {
            return Err(unsafe_path());
        }
        let mut lengths = [0; GENERATIONS];
        for (i, length) in lengths.iter_mut().enumerate() {
            match private_file(&self.dir, &self.product.filename(i), libc::O_RDONLY) {
                Ok(file) => {
                    *length = file.metadata()?.len();
                    if *length > self.limit {
                        return Err(unsafe_path());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if lengths[0] + bytes.len() as u64 > self.limit {
            for i in (0..GENERATIONS).rev() {
                if lengths[i] == 0 {
                    continue;
                }
                let source = CString::new(self.product.filename(i)).unwrap();
                let result = if i == GENERATIONS - 1 {
                    unsafe { libc::unlinkat(self.dir.as_raw_fd(), source.as_ptr(), 0) }
                } else {
                    let dest = CString::new(self.product.filename(i + 1)).unwrap();
                    unsafe {
                        libc::renameat(
                            self.dir.as_raw_fd(),
                            source.as_ptr(),
                            self.dir.as_raw_fd(),
                            dest.as_ptr(),
                        )
                    }
                };
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        // Never retain an append descriptor across lock acquisition/rotation.
        let mut file = private_file(
            &self.dir,
            &self.product.filename(0),
            libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT,
        )?;
        if file.metadata()?.len() + bytes.len() as u64 > self.limit {
            return Err(unsafe_path());
        }
        file.write_all(&bytes)?;
        file.sync_data()?;
        self.dir.sync_all()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Unknown,
    Started,
    Ready,
    StartupFailed,
    Panicked,
    Exited,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Persistence {
    Unknown,
    Pending,
    Failed,
    Recovered,
    Complete,
}
#[derive(Debug, Serialize)]
pub struct Observation {
    pub run: Uuid,
    pub pid: u32,
    pub at_unix_ms: u64,
    pub version: String,
    pub protocol: u16,
    pub lifecycle: Lifecycle,
    pub clean_shutdown: Option<bool>,
    pub persistence: Persistence,
    pub final_flush: Option<Event>,
}
#[derive(Debug, Serialize)]
pub struct ProductReport {
    pub product: Product,
    pub available: bool,
    /// False means malformed, truncated or unsafe storage; health is unknown.
    pub readable: bool,
    pub last_observed: Option<Observation>,
    pub records: Vec<Record>,
}
impl ProductReport {
    fn from_records(product: Product, readable: bool, records: Vec<Record>) -> Self {
        let last_observed = records.last().map(|last| {
            let mut observed = Observation {
                run: last.run,
                pid: last.pid,
                at_unix_ms: last.at_unix_ms,
                version: last.version.clone(),
                protocol: last.protocol,
                lifecycle: Lifecycle::Unknown,
                clean_shutdown: None,
                persistence: Persistence::Unknown,
                final_flush: None,
            };
            let mut started = false;
            let mut writer_seen = false;
            let mut settings_failed = false;
            let mut unrecovered_failure = false;
            for record in records.iter().filter(|record| record.run == last.run) {
                match record.event {
                    Event::Startup => {
                        started = true;
                        observed.lifecycle = Lifecycle::Started;
                        observed.clean_shutdown = None;
                    }
                    Event::Ready => observed.lifecycle = Lifecycle::Ready,
                    Event::StartupFailed => {
                        unrecovered_failure = true;
                        observed.lifecycle = Lifecycle::StartupFailed;
                        observed.clean_shutdown = Some(false);
                    }
                    Event::Panic { recovered: false } => {
                        unrecovered_failure = true;
                        observed.lifecycle = Lifecycle::Panicked;
                        observed.clean_shutdown = Some(false);
                    }
                    Event::Exit { success } => {
                        observed.lifecycle = Lifecycle::Exited;
                        observed.clean_shutdown = Some(success);
                    }
                    Event::PersistenceStarted => {
                        writer_seen = true;
                        observed.persistence = Persistence::Pending;
                        observed.final_flush = None;
                    }
                    Event::PersistenceFailed { .. } | Event::WriterUnavailable => {
                        writer_seen = true;
                        observed.persistence = Persistence::Failed
                    }
                    Event::PersistenceRecovered => {
                        writer_seen = true;
                        observed.persistence = Persistence::Recovered;
                    }
                    event @ Event::FinalFlush { complete, .. } => {
                        writer_seen = true;
                        observed.persistence = if complete {
                            Persistence::Complete
                        } else {
                            Persistence::Failed
                        };
                        observed.final_flush = Some(event);
                    }
                    Event::SettingsFailed { .. } => settings_failed = true,
                    _ => {}
                }
            }
            if observed.clean_shutdown == Some(true) {
                observed.clean_shutdown = if settings_failed || unrecovered_failure {
                    Some(false)
                } else if writer_seen {
                    match observed.persistence {
                        Persistence::Complete => Some(true),
                        Persistence::Failed => Some(false),
                        _ => None,
                    }
                } else if started {
                    Some(true)
                } else {
                    None
                };
            }
            observed
        });
        Self {
            product,
            available: !records.is_empty(),
            readable,
            last_observed,
            records,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u16,
    pub cli_version: &'static str,
    pub cli_protocol: u16,
    pub path: PathBuf,
    pub products: Vec<ProductReport>,
}
impl Report {
    pub fn human(&self) -> String {
        let mut lines = vec![format!("cagents {} (protocol {})", self.cli_version, self.cli_protocol),
            format!("Private diagnostics: {}", self.path.display()),
            "Recorded evidence only; sibling versions and health are last observed, not a live check.".into()];
        for product in &self.products {
            match &product.last_observed {
                Some(last) => lines.push(format!("{}: last observed {} (protocol {}), at_unix_ms={}, lifecycle={:?}, clean_shutdown={:?}, persistence={:?}, run={}", product.product.name(), last.version, last.protocol, last.at_unix_ms, last.lifecycle, last.clean_shutdown, last.persistence, last.run)),
                None => lines.push(format!("{}: health unknown ({})", product.product.name(), if product.readable { "no retained evidence" } else { "evidence unavailable or invalid" })),
            }
        }
        lines.join("\n")
    }
}

/// This intentionally reads only recorded evidence, never runtime sockets,
/// installation routes or databases. Absence is not proof of a clean shutdown.
pub fn inspect() -> io::Result<Report> {
    inspect_at(state_path()?)
}
fn inspect_at(path: PathBuf) -> io::Result<Report> {
    let (dir, readable) = match directory(&path, false) {
        Ok(dir) => (Some(dir), true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (None, true),
        Err(_) => (None, false),
    };
    let products = [Product::Ui, Product::Daemon, Product::Editor]
        .into_iter()
        .map(|product| match &dir {
            Some(dir) => match read_product(dir, product) {
                Ok(records) => ProductReport::from_records(product, true, records),
                Err(_) => ProductReport::from_records(product, false, Vec::new()),
            },
            None => ProductReport::from_records(product, readable, Vec::new()),
        })
        .collect();
    Ok(Report {
        schema_version: SCHEMA_VERSION,
        cli_version: crate::VERSION,
        cli_protocol: crate::terminal::ipc::PROTOCOL_VERSION,
        path,
        products,
    })
}

fn read_product(dir: &File, product: Product) -> io::Result<Vec<Record>> {
    let lock = match private_file(dir, &format!("{}.lock", product.name()), libc::O_RDONLY) {
        Ok(file) => Some(Lock::acquire(file, false)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut records = std::collections::VecDeque::new();
    for generation in (0..GENERATIONS).rev() {
        let file = match private_file(dir, &product.filename(generation), libc::O_RDONLY) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if lock.is_none() || file.metadata()?.len() > FILE_BYTES {
            return Err(unsafe_path());
        }
        let mut bytes = Vec::new();
        file.take(FILE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > FILE_BYTES || bytes.last() != Some(&b'\n') {
            return Err(unsafe_path());
        }
        for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            if line.len() > RECORD_BYTES {
                return Err(unsafe_path());
            }
            let record: Record = serde_json::from_slice(line).map_err(|_| unsafe_path())?;
            if record.schema_version != SCHEMA_VERSION
                || record.product != product
                || semver::Version::parse(&record.version).is_err()
                || record.version.len() > 128
                || !record
                    .version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
            {
                return Err(unsafe_path());
            }
            if records.len() == RECENT_RECORDS {
                records.pop_front();
            }
            records.push_back(record);
        }
    }
    Ok(records.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cagents/diagnostics");
        (root, path)
    }
    #[test]
    fn absence_is_unknown_and_inspection_creates_nothing() {
        let (root, path) = fixture();
        let report = inspect_at(path.clone()).unwrap();
        assert_eq!(report.schema_version, 1);
        assert!(
            report
                .products
                .iter()
                .all(|p| p.readable && p.last_observed.is_none())
        );
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn after_exit_evidence_and_restart_do_not_invent_health() {
        let (_root, path) = fixture();
        let mut recorder = Recorder::open(&path, Product::Ui).unwrap();
        recorder.append(Event::Startup).unwrap();
        recorder.append(Event::PersistenceStarted).unwrap();
        recorder
            .append(Event::FinalFlush {
                complete: false,
                unsaved: 7,
                refused: 2,
                category: Some(Failure::DiskFull),
            })
            .unwrap();
        recorder.append(Event::Exit { success: true }).unwrap();
        drop(recorder);
        let report = inspect_at(path.clone()).unwrap();
        let previous = report.products[0].last_observed.as_ref().unwrap();
        assert_eq!(previous.persistence, Persistence::Failed);
        assert_eq!(previous.clean_shutdown, Some(false));
        assert!(matches!(
            previous.final_flush,
            Some(Event::FinalFlush {
                unsaved: 7,
                refused: 2,
                ..
            })
        ));
        assert_eq!(previous.version, crate::VERSION);
        let run = previous.run;
        let mut next = Recorder::open(&path, Product::Ui).unwrap();
        next.append(Event::Startup).unwrap();
        let report = inspect_at(path).unwrap();
        let current = report.products[0].last_observed.as_ref().unwrap();
        assert_ne!(current.run, run);
        assert_eq!(current.clean_shutdown, None);
        assert_eq!(current.persistence, Persistence::Unknown);
        assert_eq!(current.final_flush, None);
    }
    #[test]
    fn normal_exit_requires_final_writer_evidence_before_claiming_clean() {
        for (final_event, expected) in [
            (None, None),
            (
                Some(Event::FinalFlush {
                    complete: true,
                    unsaved: 0,
                    refused: 0,
                    category: None,
                }),
                Some(true),
            ),
            (Some(Event::WriterUnavailable), Some(false)),
        ] {
            let (_root, path) = fixture();
            let mut recorder = Recorder::open(&path, Product::Ui).unwrap();
            recorder.append(Event::Startup).unwrap();
            recorder.append(Event::PersistenceStarted).unwrap();
            if let Some(event) = final_event {
                recorder.append(event).unwrap();
            }
            recorder.append(Event::Exit { success: true }).unwrap();
            assert_eq!(
                inspect_at(path).unwrap().products[0]
                    .last_observed
                    .as_ref()
                    .unwrap()
                    .clean_shutdown,
                expected
            );
        }
        // A retained exit with no retained startup/writer context is unknown.
        let (_root, path) = fixture();
        let mut recorder = Recorder::open(&path, Product::Daemon).unwrap();
        recorder.append(Event::Exit { success: true }).unwrap();
        assert_eq!(
            inspect_at(path).unwrap().products[1]
                .last_observed
                .as_ref()
                .unwrap()
                .clean_shutdown,
            None
        );
    }

    #[test]
    fn unrecovered_failure_is_not_erased_by_a_later_normal_return() {
        for failure in [
            Event::Panic { recovered: false },
            Event::StartupFailed,
            Event::SettingsFailed {
                category: Failure::Unknown,
            },
        ] {
            let (_root, path) = fixture();
            let mut recorder = Recorder::open(&path, Product::Ui).unwrap();
            recorder.append(Event::Startup).unwrap();
            recorder.append(failure).unwrap();
            recorder.append(Event::Exit { success: true }).unwrap();
            assert_eq!(
                inspect_at(path).unwrap().products[0]
                    .last_observed
                    .as_ref()
                    .unwrap()
                    .clean_shutdown,
                Some(false)
            );
        }
    }
    #[test]
    fn editor_records_its_own_compiled_version_and_unknown_products_do_not_alias() {
        let (_root, path) = fixture();
        let mut editor = Recorder::open_version(&path, Product::Editor, "0.9.1").unwrap();
        editor.append(Event::Startup).unwrap();
        let report = inspect_at(path).unwrap();
        assert_eq!(
            report.products[2].last_observed.as_ref().unwrap().version,
            "0.9.1"
        );
        assert_eq!(
            Product::from_name("ceditor-development"),
            Some(Product::Editor)
        );
        assert_eq!(Product::from_name("unknown"), None);
        assert_eq!(Product::from_name("cagents"), None);
    }

    #[test]
    fn truncated_or_unknown_fields_are_not_health_evidence() {
        for invalid in [
            b"{\"kind\":\"startup\"}".as_slice(),
            b"{\"prompt\":\"TOKEN=secret\"}\n".as_slice(),
        ] {
            let (_root, path) = fixture();
            let mut recorder = Recorder::open(&path, Product::Ui).unwrap();
            recorder.append(Event::Exit { success: true }).unwrap();
            let mut file = private_file(
                &recorder.dir,
                &Product::Ui.filename(0),
                libc::O_WRONLY | libc::O_APPEND,
            )
            .unwrap();
            file.write_all(invalid).unwrap();
            let report = inspect_at(path).unwrap();
            assert!(!report.products[0].readable);
            assert!(report.products[0].last_observed.is_none());
            assert!(!serde_json::to_string(&report).unwrap().contains("TOKEN"));
        }
    }
    #[test]
    fn redaction_is_structural_even_for_oversized_secret_errors() {
        let (_root, path) = fixture();
        let sensitive = format!(
            "\x1b[31m terminal --prompt private environment TOKEN=secret https://user:password@example.test/path?token=value#secret {}",
            "X".repeat(3 * 1024 * 1024)
        );
        let error = io::Error::new(io::ErrorKind::PermissionDenied, sensitive);
        let category = match error.kind() {
            io::ErrorKind::PermissionDenied => Failure::PermissionDenied,
            _ => Failure::Io,
        };
        let mut recorder = Recorder::open(&path, Product::Ui).unwrap();
        recorder
            .append(Event::PersistenceFailed {
                category,
                exhausted: true,
            })
            .unwrap();
        recorder.append(Event::Panic { recovered: false }).unwrap();
        let data = serde_json::to_string(&inspect_at(path.clone()).unwrap()).unwrap();
        for forbidden in [
            "terminal",
            "--prompt",
            "TOKEN",
            "user:password",
            "example.test",
            "token=value",
            "XXX",
            "\\u001b",
        ] {
            assert!(!data.contains(forbidden), "{forbidden}");
        }
        assert!(
            std::fs::metadata(path.join(Product::Ui.filename(0)))
                .unwrap()
                .len()
                < 1024
        );
    }
    #[test]
    fn unsafe_ancestors_files_and_hardlinks_are_rejected() {
        let (root, path) = fixture();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, root.path().join("cagents")).unwrap();
        assert!(Recorder::open(&path, Product::Ui).is_err());
        assert!(
            inspect_at(path.clone())
                .unwrap()
                .products
                .iter()
                .all(|p| !p.readable && p.last_observed.is_none())
        );
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
        std::fs::remove_file(root.path().join("cagents")).unwrap();
        let mut recorder = Recorder::open(&path, Product::Ui).unwrap();
        recorder.append(Event::Startup).unwrap();
        let file = path.join(Product::Ui.filename(0));
        std::fs::hard_link(&file, root.path().join("alias")).unwrap();
        let before = std::fs::read(&file).unwrap();
        assert!(recorder.append(Event::Startup).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert!(!inspect_at(path.clone()).unwrap().products[0].readable);
        std::fs::remove_file(root.path().join("alias")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(recorder.append(Event::Startup).is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(recorder.append(Event::Startup).is_err());
    }
    #[test]
    fn cooperating_writers_and_crash_loops_share_five_bounded_generations() {
        let (_root, path) = fixture();
        Recorder::open(&path, Product::Daemon).unwrap();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..40 {
                        let mut recorder = Recorder::open(&path, Product::Daemon).unwrap();
                        recorder.limit = 1024;
                        recorder.append(Event::Startup).unwrap();
                        recorder.append(Event::Panic { recovered: false }).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let files: Vec<_> = std::fs::read_dir(&path)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(files.len(), GENERATIONS + 1); // Five data files and the empty stable lock.
        for generation in 0..GENERATIONS {
            let file = path.join(Product::Daemon.filename(generation));
            let metadata = file.metadata().unwrap();
            assert!(metadata.len() <= 1024);
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::read(file).unwrap().last(), Some(&b'\n'));
        }
        assert!(inspect_at(path).unwrap().products[1].readable);
    }
    #[test]
    fn rotation_or_oversize_failure_never_appends_past_cap() {
        for oversized in [false, true] {
            let (_root, path) = fixture();
            let mut recorder = Recorder::open(&path, Product::Ui).unwrap();
            recorder.limit = 1024;
            recorder.append(Event::Startup).unwrap();
            let current = path.join(Product::Ui.filename(0));
            if oversized {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&current)
                    .unwrap();
                file.write_all(&vec![b'X'; 1025]).unwrap();
            } else {
                // A replaced rotation destination fails validation before any write.
                std::fs::create_dir(path.join(Product::Ui.filename(4))).unwrap();
            }
            let before = std::fs::read(&current).unwrap();
            for _ in 0..10 {
                assert!(recorder.append(Event::Startup).is_err());
            }
            assert_eq!(std::fs::read(current).unwrap(), before);
        }
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    const TEST_STATE: &str = "CAGENTS_DIAGNOSTICS_TEST_STATE";
    const TEST_MODE: &str = "CAGENTS_DIAGNOSTICS_TEST_MODE";
    #[test]
    fn subprocess_fixture() {
        let Some(path) = std::env::var_os(TEST_STATE) else {
            return;
        };
        // The child has a private XDG state root; HOME/authority are untouched.
        crate::instance::install_panic_hook("cagents-ui");
        if std::env::var_os(TEST_MODE).as_deref() == Some(std::ffi::OsStr::new("panic")) {
            panic!(
                "TOKEN=secret prompt --api-key private https://user:password@example.test/?token=value#fragment \x1b[31m"
            );
        }
        let file = PathBuf::from(path)
            .join(crate::instance::STORAGE_NAME)
            .join("diagnostics")
            .join(Product::Ui.filename(0));
        use std::os::unix::fs::PermissionsExt;
        let before = std::fs::read(&file).unwrap();
        if std::env::var_os(TEST_MODE).as_deref() == Some(std::ffi::OsStr::new("write_failure")) {
            let limit = libc::rlimit {
                rlim_cur: before.len() as libc::rlim_t,
                rlim_max: before.len() as libc::rlim_t,
            };
            // This exact subprocess owns no production state; force a real
            // write failure without filling storage or affecting the parent.
            unsafe {
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            }
        } else {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        for _ in 0..32 {
            record(Event::Ready);
        }
        assert!(take_failure_notice());
        assert!(!take_failure_notice());
        assert_eq!(std::fs::read(file).unwrap(), before);
    }
    #[test]
    fn sink_failure_is_nonfatal_and_reported_once_without_recursive_logging() {
        for mode in ["unsafe_file", "write_failure"] {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "diagnostics::process_tests::subprocess_fixture",
                    "--nocapture",
                ])
                .env(TEST_STATE, root.path())
                .env(TEST_MODE, mode)
                .env("XDG_STATE_HOME", root.path())
                .env("CAGENTS_LOCAL_DEBUG", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stderr)
                    .matches(FAILURE_NOTICE)
                    .count(),
                1
            );
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains(root.path().to_str().unwrap())
            );
        }
    }
    #[test]
    fn real_panic_hook_retains_only_typed_evidence_after_exit() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::process_tests::subprocess_fixture",
                "--nocapture",
            ])
            .env(TEST_STATE, root.path())
            .env(TEST_MODE, "panic")
            .env("XDG_STATE_HOME", root.path())
            .env("CAGENTS_LOCAL_DEBUG", "0")
            .env("RUST_LOG", "trace")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let report = inspect_at(
            root.path()
                .join(crate::instance::STORAGE_NAME)
                .join("diagnostics"),
        )
        .unwrap();
        let last = report.products[0].last_observed.as_ref().unwrap();
        assert_eq!(last.lifecycle, Lifecycle::Panicked);
        assert_eq!(last.clean_shutdown, Some(false));
        let evidence = serde_json::to_string(&report).unwrap();
        for forbidden in [
            "TOKEN",
            "--api-key",
            "user:password",
            "example.test",
            "token=value",
            "fragment",
        ] {
            assert!(!evidence.contains(forbidden));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(forbidden));
        }
    }
}
