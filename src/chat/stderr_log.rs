//! Bounded raw provider stderr. An exclusive writer or shared reader lease
//! protects a log from retention cleanup. Text decoding belongs to the caller:
//! invalid UTF-8 and terminal control bytes are preserved here without a claim
//! that a lossy display representation is an exact copy of the original.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

pub(crate) const MAX_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const PAGE_BYTES: usize = 256 * 1024;
const CLOSED_BYTES: u64 = 128 * 1024 * 1024;
const CLOSED_FILES: usize = 512;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

// One file-only worker per process. Producers never perform filesystem I/O.
const ACTIVE_CAPTURES: usize = 1024;
const CHUNK_BYTES: usize = 16 * 1024;
const DATA_QUEUE: usize = 64;
const CONTROL_QUEUE: usize = ACTIVE_CAPTURES * 2;

#[derive(Clone)]
struct Service {
    control: crossbeam_channel::Sender<Control>,
    data: crossbeam_channel::Sender<Data>,
    active: Arc<std::sync::atomic::AtomicUsize>,
    preparation: crossbeam_channel::Sender<Prepare>,
    preparing: Arc<std::sync::atomic::AtomicBool>,
    prepared_bytes: Arc<std::sync::atomic::AtomicU64>,
    prepared_count: Arc<std::sync::atomic::AtomicUsize>,
}
struct Shared {
    total: std::sync::atomic::AtomicU64,
    queued: std::sync::atomic::AtomicUsize,
    sealed: std::sync::atomic::AtomicBool,
    queue_full: std::sync::atomic::AtomicBool,
}
struct Registration(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for Registration {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
enum Control {
    Begin {
        id: uuid::Uuid,
        shared: Arc<Shared>,
        reply: crossbeam_channel::Sender<io::Result<Lease>>,
        registration: Registration,
    },
    Finish(uuid::Uuid),
}
struct Data {
    id: uuid::Uuid,
    bytes: Vec<u8>,
}
struct Writing {
    capture: io::Result<Capture>,
    shared: Arc<Shared>,
    reply: crossbeam_channel::Sender<io::Result<Lease>>,
    finishing: bool,
    _registration: Registration,
}

/// Offers raw bytes without waiting for disk, a writer, or queue capacity.
/// Full/failed admission seals the contiguous prefix permanently. The caller
/// continues draining provider stderr and accounts for all omitted bytes.
pub(crate) struct Recorder {
    service: Service,
    id: uuid::Uuid,
    shared: Arc<Shared>,
    admitted: u64,
    completion: Option<Completion>,
    failure: crossbeam_channel::Sender<io::Result<Lease>>,
}
pub(crate) enum Status {
    Pending,
    Ready(Lease),
    Unavailable,
}
pub(crate) struct Completion {
    receive: crossbeam_channel::Receiver<io::Result<Lease>>,
    settled: Option<io::Result<Lease>>,
}
impl Completion {
    pub fn poll(&mut self) -> Status {
        if self.settled.is_none() {
            self.settled = match self.receive.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Empty) => return Status::Pending,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err(unavailable())),
            };
        }
        match self.settled.as_ref().unwrap() {
            Ok(lease) => Status::Ready(lease.clone()),
            Err(_) => Status::Unavailable,
        }
    }
}
impl Recorder {
    pub fn start() -> io::Result<Self> {
        process_service()?.begin()
    }
    pub fn offered_bytes(&self) -> u64 {
        self.shared.total.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};
        let _ = self.shared.total.fetch_update(AcqRel, Acquire, |total| {
            Some(total.saturating_add(bytes.len() as u64))
        });
        if self.shared.sealed.load(Acquire) {
            return false;
        }
        let offered = bytes.len().min((MAX_BYTES - self.admitted) as usize);
        for chunk in bytes[..offered].chunks(CHUNK_BYTES) {
            // Count before publication so a very fast writer cannot underflow.
            // Finish is sent only after the producer has completed this offer.
            self.shared.queued.fetch_add(1, AcqRel);
            match self.service.data.try_send(Data {
                id: self.id,
                bytes: chunk.to_vec(),
            }) {
                Ok(()) => self.admitted += chunk.len() as u64,
                Err(error) => {
                    self.shared.queued.fetch_sub(1, AcqRel);
                    self.shared.sealed.store(true, Release);
                    if error.is_full() {
                        self.shared.queue_full.store(true, Release);
                    }
                    return false;
                }
            }
        }
        true
    }
    /// No join/fsync/receive: only one separately bounded Finish admission.
    pub fn finish(mut self) -> Completion {
        let completion = self.completion.take().unwrap();
        self.request_finish();
        completion
    }
    fn request_finish(&self) {
        // There are at most1024 registrations, each sends Begin+Finish once.
        // Registration is held by the worker until real finalization, so the
        // separate2048 control queue cannot fill behind the64 data entries.
        if self
            .service
            .control
            .try_send(Control::Finish(self.id))
            .is_err()
        {
            // Full is excluded by the registration/control cardinality proof;
            // even an invariant violation or disconnected worker is explicit,
            // never a silently pending completion or provider lifecycle error.
            let _ = self.failure.try_send(Err(unavailable()));
        }
        // A disconnected worker makes Completion explicitly Unavailable; it
        // never causes a provider lifecycle error or a replacement thread.
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        if self.completion.is_some() {
            self.request_finish();
        }
    }
}
impl Service {
    fn spawn(mut create: impl FnMut() -> io::Result<Capture> + Send + 'static) -> io::Result<Self> {
        let (control, mut controls) = crossbeam_channel::bounded(CONTROL_QUEUE);
        let (data, mut chunks) = crossbeam_channel::bounded(DATA_QUEUE);
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (preparation, mut preparations) = crossbeam_channel::bounded(1);
        let preparing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prepared_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let prepared_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::Builder::new().name("provider-stderr-log".into()).spawn(move || {
            let mut entries = std::collections::HashMap::<uuid::Uuid, Writing>::new();
            let mut open = 3;
            while open != 0 {
                crossbeam_channel::select_biased! {
                    recv(controls) -> request => match request {
                        Ok(Control::Begin { id, shared, reply, registration }) => {
                            let capture = create();
                            if capture.is_err() { shared.sealed.store(true, std::sync::atomic::Ordering::Release); }
                            entries.insert(id, Writing { capture, shared, reply, finishing: false, _registration: registration });
                        }
                        Ok(Control::Finish(id)) => {
                            if let Some(entry) = entries.get_mut(&id) { entry.finishing = true; }
                            finish_ready(&mut entries, id);
                        }
                        Err(_) => { controls = crossbeam_channel::never(); open -= 1; }
                    },
                    recv(chunks) -> chunk => match chunk {
                        Ok(Data { id, bytes }) => {
                            if let Some(entry) = entries.get_mut(&id) {
                                if let Ok(capture) = &mut entry.capture {
                                    if !capture.push(&bytes) { entry.shared.sealed.store(true, std::sync::atomic::Ordering::Release); }
                                }
                                entry.shared.queued.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                            }
                            finish_ready(&mut entries, id);
                        }
                        Err(_) => { chunks = crossbeam_channel::never(); open -= 1; }
                    },
                    recv(preparations) -> job => match job {
                        Ok(Prepare { source, reply, reservation, _busy, notify }) => {
                            let result = prepare_text(source, reservation);
                            drop(_busy); // work ended before publishing Ready to its consumer
                            let _ = reply.try_send(result);
                            if let Some(notify) = notify { notify(); }
                        }
                        Err(_) => { preparations = crossbeam_channel::never(); open -= 1; }
                    },
                }
            }
        })?;
        Ok(Self {
            control,
            data,
            active,
            preparation,
            preparing,
            prepared_bytes,
            prepared_count,
        })
    }
    fn begin(&self) -> io::Result<Recorder> {
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        self.active
            .fetch_update(AcqRel, Acquire, |active| {
                (active < ACTIVE_CAPTURES).then_some(active + 1)
            })
            .map_err(|_| unavailable())?;
        let registration = Registration(self.active.clone());
        let id = uuid::Uuid::new_v4();
        let shared = Arc::new(Shared {
            total: 0.into(),
            queued: 0.into(),
            sealed: false.into(),
            queue_full: false.into(),
        });
        let (reply, receive) = crossbeam_channel::bounded(1);
        self.control
            .try_send(Control::Begin {
                id,
                shared: shared.clone(),
                reply: reply.clone(),
                registration,
            })
            .map_err(|_| unavailable())?;
        Ok(Recorder {
            service: self.clone(),
            id,
            shared,
            admitted: 0,
            completion: Some(Completion {
                receive,
                settled: None,
            }),
            failure: reply,
        })
    }
}
fn finish_ready(entries: &mut std::collections::HashMap<uuid::Uuid, Writing>, id: uuid::Uuid) {
    use std::sync::atomic::Ordering::Acquire;
    if !entries
        .get(&id)
        .is_some_and(|entry| entry.finishing && entry.shared.queued.load(Acquire) == 0)
    {
        return;
    }
    let entry = entries.remove(&id).unwrap();
    let result = entry.capture.and_then(|mut capture| {
        capture.total = entry.shared.total.load(Acquire);
        capture.descriptor.info.omitted =
            capture.total.saturating_sub(capture.descriptor.info.bytes);
        capture.descriptor.info.queue_full = entry.shared.queue_full.load(Acquire);
        capture.finish()
    });
    let _ = entry.reply.try_send(result);
}

// Tests using the process-wide service must serialize their actual preparation
// and deliberately stalled-worker fixtures. Raw helper tests own local services.
#[cfg(test)]
pub(super) fn test_process_service_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn process_service() -> io::Result<&'static Service> {
    static SERVICE: std::sync::OnceLock<io::Result<Service>> = std::sync::OnceLock::new();
    SERVICE
        .get_or_init(|| Service::spawn(Capture::new))
        .as_ref()
        .map_err(|_| unavailable())
}

const PREPARED_LIMIT: u64 = 128 * 1024 * 1024;
const MAX_PREPARED_BYTES: u64 = MAX_BYTES * 3;
const PREVIEW_BYTES: usize = 4096;

/// Only locally owned capture results enter preparation, never Delivery prose.
pub(crate) enum PreparationSource {
    Owned(Lease),
    Descriptor(Descriptor),
    Finishing(Completion),
}
struct Prepare {
    source: PreparationSource,
    reply: crossbeam_channel::Sender<io::Result<Prepared>>,
    reservation: PreparedReservation,
    _busy: Preparing,
    notify: Option<Box<dyn FnOnce() + Send>>,
}
struct Preparing(Arc<std::sync::atomic::AtomicBool>);
impl Drop for Preparing {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}
struct PreparedReservation {
    bytes: Arc<std::sync::atomic::AtomicU64>,
    _count: Registration,
    amount: u64,
}
impl Drop for PreparedReservation {
    fn drop(&mut self) {
        self.bytes
            .fetch_sub(self.amount, std::sync::atomic::Ordering::AcqRel);
    }
}
pub(crate) struct Preparation {
    receive: crossbeam_channel::Receiver<io::Result<Prepared>>,
    settled: Option<io::Result<Prepared>>,
}
pub(crate) enum PreparedStatus {
    Pending,
    Ready(Prepared),
    Unavailable,
}
impl Preparation {
    pub fn start(source: PreparationSource) -> io::Result<Self> {
        process_service()?.prepare(source)
    }
    pub fn start_notified(
        source: PreparationSource,
        notify: impl FnOnce() + Send + 'static,
    ) -> io::Result<Self> {
        process_service()?.prepare_notified(source, Some(Box::new(notify)))
    }
    pub fn poll(&mut self) -> PreparedStatus {
        if self.settled.is_none() {
            self.settled = match self.receive.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Empty) => return PreparedStatus::Pending,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Some(Err(unavailable())),
            };
        }
        match self.settled.as_ref().unwrap() {
            Ok(prepared) => PreparedStatus::Ready(prepared.clone()),
            Err(_) => PreparedStatus::Unavailable,
        }
    }
}
impl Service {
    fn prepare(&self, source: PreparationSource) -> io::Result<Preparation> {
        self.prepare_notified(source, None)
    }
    fn prepare_notified(
        &self,
        source: PreparationSource,
        notify: Option<Box<dyn FnOnce() + Send>>,
    ) -> io::Result<Preparation> {
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        self.preparing
            .compare_exchange(false, true, AcqRel, Acquire)
            .map_err(|_| unavailable())?;
        let busy = Preparing(self.preparing.clone());
        self.prepared_count
            .fetch_update(AcqRel, Acquire, |count| {
                (count < ACTIVE_CAPTURES).then_some(count + 1)
            })
            .map_err(|_| unavailable())?;
        let count = Registration(self.prepared_count.clone());
        self.prepared_bytes
            .fetch_update(AcqRel, Acquire, |bytes| {
                (bytes <= PREPARED_LIMIT - MAX_PREPARED_BYTES).then_some(bytes + MAX_PREPARED_BYTES)
            })
            .map_err(|_| unavailable())?;
        let reservation = PreparedReservation {
            bytes: self.prepared_bytes.clone(),
            _count: count,
            amount: MAX_PREPARED_BYTES,
        };
        let (reply, receive) = crossbeam_channel::bounded(1);
        self.preparation
            .try_send(Prepare {
                source,
                reply,
                reservation,
                _busy: busy,
                notify,
            })
            .map_err(|_| unavailable())?;
        Ok(Preparation {
            receive,
            settled: None,
        })
    }
}
#[derive(Clone)]
pub(crate) struct Prepared(Arc<PreparedFile>);
struct PreparedFile {
    file: File,
    device: u64,
    inode: u64,
    bytes: u64,
    preview: String,
    replacement_decoded: bool,
    raw: Lease,
    _reservation: PreparedReservation,
}
impl Prepared {
    pub fn bytes(&self) -> u64 {
        self.0.bytes
    }
    pub fn preview(&self) -> &str {
        &self.0.preview
    }
    pub fn raw_info(&self) -> &Info {
        &self.0.raw.descriptor.info
    }
    pub fn encoding_label(&self) -> &'static str {
        if self.0.replacement_decoded {
            "UTF-8 text with replacement characters; original raw bytes retained"
        } else {
            "UTF-8 text; original captured bytes"
        }
    }
    pub fn read(&self, offset: u64) -> io::Result<(String, Option<u64>)> {
        validate_prepared(&self.0.file, self.0.device, self.0.inode, self.0.bytes)?;
        if offset > self.0.bytes {
            return Err(unavailable());
        }
        let count = (self.0.bytes - offset).min(crate::chat::TEXT_COPY_PAGE_BYTES as u64) as usize;
        let mut bytes = vec![0; count];
        self.0.file.read_exact_at(&mut bytes, offset)?;
        let mut end = offset + bytes.len() as u64;
        if end < self.0.bytes {
            while std::str::from_utf8(&bytes).is_err_and(|error| error.error_len().is_none()) {
                bytes.pop();
                end -= 1;
            }
        }
        let text = String::from_utf8(bytes).map_err(|_| unavailable())?;
        if text.is_empty() && offset < self.0.bytes {
            return Err(unavailable());
        }
        Ok((text, (end < self.0.bytes).then_some(end)))
    }
}
fn validate_prepared(file: &File, device: u64, inode: u64, bytes: u64) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !private_file(&metadata)
        || metadata.dev() != device
        || metadata.ino() != inode
        || metadata.len() != bytes
        || bytes > crate::chat::TEXT_COPY_LIMIT_BYTES as u64
    {
        return Err(unavailable());
    }
    Ok(())
}
fn prepare_text(
    source: PreparationSource,
    mut reservation: PreparedReservation,
) -> io::Result<Prepared> {
    let raw = match source {
        PreparationSource::Owned(lease) => lease,
        PreparationSource::Descriptor(descriptor) => Lease::open(descriptor)?,
        PreparationSource::Finishing(mut completion) => match completion.poll() {
            Status::Ready(lease) => lease,
            _ => return Err(unavailable()), // never wait/retry on a missing receipt
        },
    };
    validate(&raw.file, &raw.descriptor)?;
    let directory = raw.descriptor.path.parent().ok_or_else(unavailable)?;
    crate::secure_file::reject_symlink_components(directory)?;
    crate::secure_file::validate_owned_directory(directory)?;
    if fs::metadata(directory)?.mode() & 0o777 != 0o700 {
        return Err(unavailable());
    }
    let path = directory.join(format!("{}.log", uuid::Uuid::new_v4()));
    let file = {
        let _gate = gate(directory)?;
        prune_locked(directory, CLOSED_BYTES, RETENTION)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        lock(&file, libc::LOCK_EX | libc::LOCK_NB)?;
        file
    };
    let mut bytes = 0u64;
    let mut preview = String::new();
    let mut preview_sealed = false;
    let mut replacement_decoded = false;
    let mut pending = Vec::new();
    let mut offset = 0;
    loop {
        let (page, next) = raw.read(offset)?;
        pending.extend(page);
        let mut cursor = 0;
        while cursor < pending.len() {
            match std::str::from_utf8(&pending[cursor..]) {
                Ok(text) => {
                    append_prepared(&file, text, &mut bytes, &mut preview, &mut preview_sealed)?;
                    cursor = pending.len();
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid != 0 {
                        let text = std::str::from_utf8(&pending[cursor..cursor + valid])
                            .map_err(|_| unavailable())?;
                        append_prepared(
                            &file,
                            text,
                            &mut bytes,
                            &mut preview,
                            &mut preview_sealed,
                        )?;
                        cursor += valid;
                    }
                    match error.error_len() {
                        Some(length) => {
                            append_prepared(
                                &file,
                                "\u{fffd}",
                                &mut bytes,
                                &mut preview,
                                &mut preview_sealed,
                            )?;
                            replacement_decoded = true;
                            cursor += length;
                        }
                        None if next.is_none() => {
                            append_prepared(
                                &file,
                                "\u{fffd}",
                                &mut bytes,
                                &mut preview,
                                &mut preview_sealed,
                            )?;
                            replacement_decoded = true;
                            cursor = pending.len();
                        }
                        None => break, // <=3 incomplete UTF-8 bytes cross raw pages
                    }
                }
            }
        }
        pending.drain(..cursor);
        if let Some(next) = next {
            offset = next;
        } else {
            break;
        }
    }
    file.sync_data()?;
    let metadata = file.metadata()?;
    {
        let _gate = gate(directory)?;
        validate_prepared(&file, metadata.dev(), metadata.ino(), bytes)?;
        lock(&file, libc::LOCK_SH | libc::LOCK_NB)?;
        validate_prepared(&file, metadata.dev(), metadata.ino(), bytes)?;
    }
    let unused = reservation
        .amount
        .checked_sub(bytes)
        .ok_or_else(unavailable)?;
    reservation
        .bytes
        .fetch_sub(unused, std::sync::atomic::Ordering::AcqRel);
    reservation.amount = bytes;
    Ok(Prepared(Arc::new(PreparedFile {
        file,
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes,
        preview,
        replacement_decoded,
        raw,
        _reservation: reservation,
    })))
}
fn append_prepared(
    file: &File,
    text: &str,
    bytes: &mut u64,
    preview: &mut String,
    preview_sealed: &mut bool,
) -> io::Result<()> {
    let end = bytes
        .checked_add(text.len() as u64)
        .ok_or_else(unavailable)?;
    if end > MAX_PREPARED_BYTES || end > crate::chat::TEXT_COPY_LIMIT_BYTES as u64 {
        return Err(unavailable());
    }
    let mut left = text.as_bytes();
    while !left.is_empty() {
        match file.write_at(left, *bytes) {
            Ok(0) => return Err(unavailable()),
            Ok(count) => {
                *bytes += count as u64;
                left = &left[count..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    if !*preview_sealed {
        let mut take = (PREVIEW_BYTES - preview.len()).min(text.len());
        while !text.is_char_boundary(take) {
            take -= 1;
        }
        preview.push_str(&text[..take]);
        *preview_sealed = take < text.len();
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Info {
    pub id: uuid::Uuid,
    pub bytes: u64,
    pub omitted: u64,
    /// Only a prefix was retained because a write failed, not merely the cap.
    pub write_failed: bool,
    /// Admission stopped permanently because the shared bounded data queue filled.
    pub queue_full: bool,
}

/// Private holder-to-owner metadata, not permission to publish a UI source.
/// Opening it verifies the original file identity as well as its private mode.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Descriptor {
    pub path: PathBuf,
    pub info: Info,
    device: u64,
    inode: u64,
}

// Filesystem layer: private so runtime/control callers must use Recorder.
struct Capture {
    file: File,
    descriptor: Descriptor,
    total: u64,
}

impl Capture {
    fn new() -> io::Result<Self> {
        #[cfg(not(test))]
        let directory = crate::instance::project_dirs()
            .ok_or_else(|| io::Error::other("Provider diagnostic cache is unavailable"))?
            .cache_dir()
            .join("provider-stderr");
        #[cfg(test)]
        let directory = {
            // Respect the owning test's TMPDIR and never prune production logs.
            static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
            ROOT.get_or_init(|| tempfile::tempdir().expect("private stderr fixture"))
                .path()
                .join("provider-stderr")
        };
        Self::at(&directory, CLOSED_BYTES, RETENTION)
    }

    fn at(directory: &Path, budget: u64, retention: Duration) -> io::Result<Self> {
        crate::secure_file::prepare_private_directory(directory)?;
        // Close the create-before-flock race with another owner's pruning.
        let _gate = gate(directory)?;
        prune_locked(directory, budget, retention)?;
        let id = uuid::Uuid::new_v4();
        let path = directory.join(format!("{id}.log"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        lock(&file, libc::LOCK_EX | libc::LOCK_NB)?;
        let metadata = file.metadata()?;
        Ok(Self {
            file,
            descriptor: Descriptor {
                path,
                info: Info {
                    id,
                    bytes: 0,
                    omitted: 0,
                    write_failed: false,
                    queue_full: false,
                },
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            total: 0,
        })
    }

    /// Retain an exact prefix; every received byte still contributes to omitted
    /// after the cap or a write failure. False requests one bounded diagnostic
    /// warning from the caller; it must not become a provider lifecycle error.
    fn push(&mut self, bytes: &[u8]) -> bool {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let mut left = &bytes[..bytes
            .len()
            .min((MAX_BYTES - self.descriptor.info.bytes) as usize)];
        while !self.descriptor.info.write_failed && !left.is_empty() {
            match self.file.write_at(left, self.descriptor.info.bytes) {
                Ok(0) => self.descriptor.info.write_failed = true,
                Ok(count) => {
                    self.descriptor.info.bytes += count as u64;
                    left = &left[count..];
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => self.descriptor.info.write_failed = true,
            }
        }
        self.descriptor.info.omitted = self.total.saturating_sub(self.descriptor.info.bytes);
        !self.descriptor.info.write_failed
    }

    #[cfg(test)]
    fn info(&self) -> &Info {
        &self.descriptor.info
    }

    /// Consume the only writer before issuing an immutable source. The holder
    /// keeps this lease until a successor has opened its own shared lease.
    fn finish(self) -> io::Result<Lease> {
        self.finish_transition(|_| {})
    }

    fn finish_transition(self, transition: impl FnOnce(&File)) -> io::Result<Lease> {
        self.file.sync_data()?;
        // flock conversion may release EX before acquiring SH. Pruners must
        // not enter that gap; both creation and every unlink use this gate.
        let _gate = gate(self.descriptor.path.parent().ok_or_else(unavailable)?)?;
        validate(&self.file, &self.descriptor)?;
        transition(&self.file);
        lock(&self.file, libc::LOCK_SH | libc::LOCK_NB)?;
        validate(&self.file, &self.descriptor)?;
        Ok(Lease {
            file: Arc::new(self.file),
            descriptor: self.descriptor,
        })
    }
}

#[derive(Clone)]
pub(crate) struct Lease {
    file: Arc<File>,
    descriptor: Descriptor,
}
impl Lease {
    pub fn open(descriptor: Descriptor) -> io::Result<Self> {
        let parent = descriptor.path.parent().ok_or_else(unavailable)?;
        crate::secure_file::reject_symlink_components(parent)?;
        crate::secure_file::validate_owned_directory(parent)?;
        if fs::metadata(parent)?.mode() & 0o777 != 0o700
            || descriptor.path.file_name().and_then(|s| s.to_str())
                != Some(format!("{}.log", descriptor.info.id).as_str())
        {
            return Err(unavailable());
        }
        let file = open(&descriptor.path)?;
        lock(&file, libc::LOCK_SH | libc::LOCK_NB)?;
        validate(&file, &descriptor)?;
        Ok(Self {
            file: Arc::new(file),
            descriptor,
        })
    }
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    pub fn read(&self, offset: u64) -> io::Result<(Vec<u8>, Option<u64>)> {
        validate(&self.file, &self.descriptor)?;
        if offset > self.descriptor.info.bytes {
            return Err(unavailable());
        }
        let count = (self.descriptor.info.bytes - offset).min(PAGE_BYTES as u64) as usize;
        let mut bytes = vec![0; count];
        self.file.read_exact_at(&mut bytes, offset)?;
        let next = offset + count as u64;
        Ok((bytes, (next < self.descriptor.info.bytes).then_some(next)))
    }
}

fn unavailable() -> io::Error {
    io::Error::other("Retained provider stderr is unavailable")
}
fn lock(file: &File, operation: i32) -> io::Result<()> {
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
}
fn private_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o777 == 0o600
        && metadata.nlink() == 1
}
fn validate(file: &File, descriptor: &Descriptor) -> io::Result<()> {
    let m = file.metadata()?;
    if !private_file(&m)
        || m.dev() != descriptor.device
        || m.ino() != descriptor.inode
        || descriptor.info.bytes > MAX_BYTES
        || m.len() != descriptor.info.bytes
    {
        return Err(unavailable());
    }
    Ok(())
}
fn gate(directory: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(directory.join(".creation.lock"))?;
    if !private_file(&file.metadata()?) {
        return Err(unavailable());
    }
    lock(&file, libc::LOCK_EX | libc::LOCK_NB)?;
    Ok(file)
}
struct Closed {
    path: PathBuf,
    device: u64,
    inode: u64,
    bytes: u64,
    modified: SystemTime,
}
fn remove_closed(entry: &Closed) -> io::Result<bool> {
    let file = match open(&entry.path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let m = file.metadata()?;
    if !private_file(&m)
        || m.dev() != entry.device
        || m.ino() != entry.inode
        || m.len() != entry.bytes
        || lock(&file, libc::LOCK_EX | libc::LOCK_NB).is_err()
    {
        return Ok(false);
    }
    let named = fs::symlink_metadata(&entry.path)?;
    if (named.dev(), named.ino()) != (m.dev(), m.ino()) {
        return Ok(false);
    }
    fs::remove_file(&entry.path)?;
    Ok(true)
}
fn prune_locked(directory: &Path, budget: u64, retention: Duration) -> io::Result<()> {
    let mut closed: Vec<Closed> = Vec::new();
    let mut bytes = 0u64;
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log")
            || path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .is_none()
        {
            continue;
        }
        let Ok(file) = open(&path) else { continue };
        let m = file.metadata()?;
        if !private_file(&m) || lock(&file, libc::LOCK_EX | libc::LOCK_NB).is_err() {
            continue;
        }
        let candidate = Closed {
            path,
            device: m.dev(),
            inode: m.ino(),
            bytes: m.len(),
            modified: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        };
        drop(file);
        if SystemTime::now()
            .duration_since(candidate.modified)
            .unwrap_or_default()
            >= retention
        {
            remove_closed(&candidate)?;
            continue;
        }
        bytes = bytes.saturating_add(candidate.bytes);
        closed.push(candidate);
        // At most513 small identity records, regardless of directory size.
        while bytes > budget || closed.len() > CLOSED_FILES {
            let oldest = closed
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.modified)
                .unwrap()
                .0;
            let candidate = closed.swap_remove(oldest);
            remove_closed(&candidate)?;
            bytes = bytes.saturating_sub(candidate.bytes);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prune(path: &Path, budget: u64, retention: Duration) {
        let _gate = gate(path).unwrap();
        prune_locked(path, budget, retention).unwrap();
    }
    fn settled(completion: &mut Completion) -> io::Result<Lease> {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match completion.poll() {
                Status::Ready(lease) => return Ok(lease),
                Status::Unavailable => return Err(unavailable()),
                Status::Pending if std::time::Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Status::Pending => panic!("actual writer did not settle within5s"),
            }
        }
    }
    fn idle(service: &Service) {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while service.active.load(std::sync::atomic::Ordering::Acquire) != 0 {
            assert!(
                std::time::Instant::now() < until,
                "actual writer registration remained occupied"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    struct Release(crossbeam_channel::Sender<()>);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }
    fn prepared_result(preparation: &mut Preparation) -> io::Result<Prepared> {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match preparation.poll() {
                PreparedStatus::Ready(prepared) => return Ok(prepared),
                PreparedStatus::Unavailable => return Err(unavailable()),
                PreparedStatus::Pending if std::time::Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                PreparedStatus::Pending => {
                    panic!("actual text preparation did not settle within5s")
                }
            }
        }
    }
    fn prepared_idle(service: &Service) {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while service.preparing.load(std::sync::atomic::Ordering::Acquire)
            || service
                .prepared_count
                .load(std::sync::atomic::Ordering::Acquire)
                != 0
        {
            assert!(
                std::time::Instant::now() < until,
                "actual preparation/leases still reserve capacity"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            service
                .prepared_bytes
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }
    fn prepared_text(prepared: &Prepared) -> String {
        let mut text = String::new();
        let mut offset = 0;
        loop {
            let (page, next) = prepared.read(offset).unwrap();
            assert!(page.len() <= crate::chat::TEXT_COPY_PAGE_BYTES);
            text.push_str(&page);
            if let Some(next) = next {
                offset = next;
            } else {
                break;
            }
        }
        text
    }
    #[test]
    fn preparation_streams_exact_utf8_pages_and_holds_budget_until_the_last_owned_lease() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut raw = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        let expected = format!("{}🦀tail", "a".repeat(PAGE_BYTES - 1));
        assert!(raw.push(expected.as_bytes()));
        let source = raw.finish().unwrap();
        let raw_id = source.descriptor.info.id;
        let service = Service::spawn(move || Err(unavailable())).unwrap();
        let mut preparation = service.prepare(PreparationSource::Owned(source)).unwrap();
        let prepared = prepared_result(&mut preparation).unwrap();
        assert_eq!(prepared_text(&prepared), expected);
        assert_eq!(prepared.bytes(), expected.len() as u64);
        assert_eq!(prepared.raw_info().id, raw_id);
        assert!(!prepared.0.replacement_decoded);
        assert!(prepared.preview().len() <= PREVIEW_BYTES);
        assert!(prepared.read(prepared.bytes() + 1).is_err());
        assert_eq!(
            service
                .prepared_bytes
                .load(std::sync::atomic::Ordering::Acquire),
            prepared.bytes()
        );
        let successor = prepared.clone();
        drop(prepared);
        drop(preparation);
        assert_eq!(prepared_text(&successor), expected);
        assert_eq!(
            service
                .prepared_count
                .load(std::sync::atomic::Ordering::Acquire),
            1
        );
        drop(successor);
        prepared_idle(&service);
    }
    #[test]
    fn invalid_utf8_is_labeled_and_raw_bytes_are_preserved_across_streaming_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut bytes = vec![b'a'; PAGE_BYTES - 1];
        bytes.extend_from_slice("🦀".as_bytes());
        bytes.extend_from_slice(b"\xff\xfe trailing-incomplete:\xe2\x82");
        let mut raw = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        assert!(raw.push(&bytes));
        let source = raw.finish().unwrap();
        let original = source.clone();
        let service = Service::spawn(move || Err(unavailable())).unwrap();
        let mut preparation = service.prepare(PreparationSource::Owned(source)).unwrap();
        let prepared = prepared_result(&mut preparation).unwrap();
        assert_eq!(prepared_text(&prepared), String::from_utf8_lossy(&bytes));
        assert!(prepared.0.replacement_decoded);
        assert!(prepared.encoding_label().contains("replacement"));
        assert!(prepared.bytes() <= bytes.len() as u64 * 3);
        let mut actual = original.read(0).unwrap().0;
        actual.extend(original.read(PAGE_BYTES as u64).unwrap().0);
        assert_eq!(
            actual, bytes,
            "preparation must not rewrite the original capture"
        );
        drop(prepared);
        drop(preparation);
        drop(original);
        prepared_idle(&service);
    }
    #[test]
    fn blocked_service_refuses_a_second_preparation_and_cancellation_releases_real_reservations() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut raw = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        raw.push(b"private capture");
        let source = raw.finish().unwrap();
        let (entered, entry) = crossbeam_channel::bounded(1);
        let (release, resume) = crossbeam_channel::bounded(1);
        let _release_on_failure = Release(release.clone());
        let service = Service::spawn(move || {
            entered.try_send(()).unwrap();
            resume.recv().unwrap();
            Err(unavailable())
        })
        .unwrap();
        let recorder = service.begin().unwrap();
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut pending = service
            .prepare(PreparationSource::Owned(source.clone()))
            .unwrap();
        assert!(matches!(pending.poll(), PreparedStatus::Pending));
        assert!(service.prepare(PreparationSource::Owned(source)).is_err());
        drop(pending); // cancelled while queued; never waits on disk or writer
        drop(recorder); // independent Finish must remain admitted
        release.send(()).unwrap();
        prepared_idle(&service);
        idle(&service);
    }
    #[test]
    fn preparation_waits_for_actual_queued_capture_finish_without_polling_or_replaying_chunks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let service = Service::spawn(move || Capture::at(&path, CLOSED_BYTES, RETENTION)).unwrap();
        let mut recorder = service.begin().unwrap();
        for value in 0..16 {
            assert!(recorder.push(format!("piece{value}\n").as_bytes()));
        }
        let completion = recorder.finish();
        let mut preparation = service
            .prepare(PreparationSource::Finishing(completion))
            .unwrap();
        let prepared = prepared_result(&mut preparation).unwrap();
        assert_eq!(
            prepared_text(&prepared),
            (0..16)
                .map(|value| format!("piece{value}\n"))
                .collect::<String>()
        );
        assert_eq!(prepared.raw_info().omitted, 0);
        drop(prepared);
        drop(preparation);
        prepared_idle(&service);
        idle(&service);
    }
    #[test]
    fn release_before_descriptor_acquisition_is_unavailable_not_a_claimed_full_log() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut raw = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        raw.push(b"original retained bytes");
        let owner = raw.finish().unwrap();
        let descriptor = owner.descriptor().clone();
        drop(owner); // no successor opened a lease before the owner released
        prune(&path, 0, Duration::ZERO);
        assert!(!descriptor.path.exists());
        let service = Service::spawn(move || Err(unavailable())).unwrap();
        let mut preparation = service
            .prepare(PreparationSource::Descriptor(descriptor))
            .unwrap();
        assert!(prepared_result(&mut preparation).is_err());
        assert!(matches!(preparation.poll(), PreparedStatus::Unavailable));
        drop(preparation);
        prepared_idle(&service);
    }
    #[test]
    fn preparation_rejects_replaced_descriptor_and_releases_its_reserved_budget() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut raw = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        raw.push(b"actual original");
        let original = raw.finish().unwrap();
        let descriptor = original.descriptor().clone();
        std::fs::rename(&descriptor.path, path.join("saved")).unwrap();
        std::fs::write(&descriptor.path, b"forged replacement").unwrap();
        let service = Service::spawn(move || Err(unavailable())).unwrap();
        let mut preparation = service
            .prepare(PreparationSource::Descriptor(descriptor))
            .unwrap();
        assert!(prepared_result(&mut preparation).is_err());
        drop(preparation);
        prepared_idle(&service);
    }
    #[test]
    fn stalled_file_worker_does_not_block_offers_or_finish_and_full_seals_the_exact_prefix() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let (entered, entry) = crossbeam_channel::bounded(1);
        let (release, resume) = crossbeam_channel::bounded(1);
        let _release_on_failure = Release(release.clone());
        let service = Service::spawn(move || {
            entered.try_send(()).unwrap();
            resume.recv().unwrap();
            Capture::at(&path, CLOSED_BYTES, RETENTION)
        })
        .unwrap();
        let mut recorder = service.begin().unwrap();
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        let started = std::time::Instant::now();
        for byte in 0..DATA_QUEUE {
            assert!(recorder.push(&vec![byte as u8; CHUNK_BYTES]));
        }
        assert!(!recorder.push(b"first omitted"));
        assert!(!recorder.push(b"later suffix must never be admitted"));
        let mut completion = recorder.finish();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "offer/finish waited on the blocked file worker"
        );
        assert!(matches!(completion.poll(), Status::Pending));
        release.send(()).unwrap();
        let lease = settled(&mut completion).unwrap();
        assert!(lease.descriptor.info.queue_full);
        assert!(!lease.descriptor.info.write_failed);
        assert_eq!(
            lease.descriptor.info.bytes,
            (DATA_QUEUE * CHUNK_BYTES) as u64
        );
        assert_eq!(
            lease.descriptor.info.omitted,
            (b"first omitted".len() + b"later suffix must never be admitted".len()) as u64
        );
        let mut bytes = Vec::new();
        let mut offset = 0;
        loop {
            let (page, next) = lease.read(offset).unwrap();
            bytes.extend(page);
            if let Some(next) = next {
                offset = next;
            } else {
                break;
            }
        }
        for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            assert!(chunk.iter().all(|byte| *byte == index as u8));
        }
        idle(&service);
    }
    #[test]
    fn full_admission_never_resumes_after_the_real_file_worker_drains() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let (entered, entry) = crossbeam_channel::bounded(1);
        let (release, resume) = crossbeam_channel::bounded(1);
        let _release_on_failure = Release(release.clone());
        let service = Service::spawn(move || {
            entered.try_send(()).unwrap();
            resume.recv().unwrap();
            Capture::at(&path, CLOSED_BYTES, RETENTION)
        })
        .unwrap();
        let mut recorder = service.begin().unwrap();
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        for _ in 0..DATA_QUEUE {
            assert!(recorder.push(b"prefix"));
        }
        assert!(!recorder.push(b"gap"));
        release.send(()).unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while recorder
            .shared
            .queued
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
        {
            assert!(
                std::time::Instant::now() < until,
                "real queued writes did not drain"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !recorder.push(b"later suffix"),
            "a freed queue cannot make a noncontiguous suffix exact"
        );
        let lease = settled(&mut recorder.finish()).unwrap();
        assert_eq!(lease.read(0).unwrap().0, b"prefix".repeat(DATA_QUEUE));
        assert_eq!(
            lease.descriptor.info.omitted,
            (b"gap".len() + b"later suffix".len()) as u64
        );
        idle(&service);
    }
    #[test]
    fn many_captures_share_one_real_writer_and_finish_only_after_their_own_fifo_chunks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let threads = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let observed = threads.clone();
        let service = Service::spawn(move || {
            observed.lock().unwrap().insert(std::thread::current().id());
            Capture::at(&path, CLOSED_BYTES, RETENTION)
        })
        .unwrap();
        let mut captures = (0..16)
            .map(|_| service.begin().unwrap())
            .collect::<Vec<_>>();
        for round in 0..2 {
            for (index, recorder) in captures.iter_mut().enumerate() {
                assert!(recorder.push(format!("capture{index}:round{round}\n").as_bytes()));
            }
        }
        let mut completions = captures
            .into_iter()
            .map(Recorder::finish)
            .collect::<Vec<_>>();
        for (index, completion) in completions.iter_mut().enumerate() {
            let lease = settled(completion).unwrap();
            assert_eq!(
                lease.read(0).unwrap().0,
                format!("capture{index}:round0\ncapture{index}:round1\n").as_bytes()
            );
            assert_eq!(lease.descriptor.info.omitted, 0);
            assert!(!lease.descriptor.info.queue_full);
        }
        idle(&service);
        assert_eq!(
            threads.lock().unwrap().len(),
            1,
            "sixteen captures must not spawn sixteen writers"
        );
    }
    #[test]
    fn capture_cardinality_and_finish_admission_are_independent_of_a_stalled_data_queue() {
        let (entered, entry) = crossbeam_channel::bounded(1);
        let (release, resume) = crossbeam_channel::bounded(1);
        let _release_on_failure = Release(release.clone());
        let mut first = true;
        let service = Service::spawn(move || {
            if first {
                first = false;
                entered.try_send(()).unwrap();
                resume.recv().unwrap();
            }
            Err(unavailable()) // explicit unavailable storage; no1024-file setup
        })
        .unwrap();
        let first = service.begin().unwrap();
        entry.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut recorders = vec![first];
        recorders.extend((1..ACTIVE_CAPTURES).map(|_| service.begin().unwrap()));
        assert!(
            service.begin().is_err(),
            "the1025th registration must be explicit Unavailable"
        );
        for _ in 0..DATA_QUEUE {
            assert!(recorders[0].push(b"queued"));
        }
        assert!(!recorders[0].push(b"full"));
        let mut completions = recorders
            .into_iter()
            .map(Recorder::finish)
            .collect::<Vec<_>>();
        assert_eq!(service.control.len(), ACTIVE_CAPTURES * 2 - 1);
        assert!(
            completions
                .iter_mut()
                .all(|completion| matches!(completion.poll(), Status::Pending))
        );
        release.send(()).unwrap();
        for completion in &mut completions {
            assert!(settled(completion).is_err());
        }
        idle(&service);
        assert!(
            service.begin().is_ok(),
            "finished/unavailable storage must release actual registration capacity"
        );
    }
    #[test]
    fn dropped_receipt_still_finishes_and_releases_capacity_without_waiting_on_its_consumer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let service = Service::spawn(move || Capture::at(&path, CLOSED_BYTES, RETENTION)).unwrap();
        let mut recorder = service.begin().unwrap();
        assert!(recorder.push(b"consumer cancelled"));
        drop(recorder); // sends exactly one Finish; receiver is deliberately gone
        idle(&service);
    }
    #[test]
    fn exact_raw_prefix_pages_and_omission_survive_the_real_eight_mib_cap() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut capture = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        let prefix = b"first\n\x1b[31mfailed\x1b[0m\xff\xfe\n";
        assert!(capture.push(prefix));
        let fill = vec![b'x'; PAGE_BYTES];
        for _ in 0..32 {
            assert!(capture.push(&fill));
        }
        assert!(capture.push(b"discarded after cap"));
        let lease = capture.finish().unwrap();
        assert_eq!(lease.descriptor.info.bytes, MAX_BYTES);
        assert_eq!(lease.descriptor.info.omitted, prefix.len() as u64 + 19);
        assert!(!lease.descriptor.info.write_failed);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        let mut all = Vec::new();
        let mut offset = 0;
        loop {
            let (bytes, next) = lease.read(offset).unwrap();
            assert!(bytes.len() <= PAGE_BYTES);
            all.extend(bytes);
            if let Some(next) = next {
                offset = next
            } else {
                break;
            }
        }
        assert_eq!(all.len(), MAX_BYTES as usize);
        assert_eq!(&all[..prefix.len()], prefix);
        assert!(all[prefix.len()..].iter().all(|b| *b == b'x'));
        assert!(
            String::from_utf8(all).is_err(),
            "raw invalidUTF8 is not silently rewritten"
        );
        assert!(lease.read(MAX_BYTES + 1).is_err());
    }
    #[test]
    fn writers_and_shared_successor_leases_survive_pruning_until_the_last_reader_drops() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut capture = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        capture.push(b"original stderr");
        let original = capture.descriptor.path.clone();
        fs::write(path.join("not-owned.log"), b"leave this alone").unwrap();
        prune(&path, 0, Duration::ZERO);
        assert!(original.exists());
        let owner = capture.finish().unwrap();
        let successor = Lease::open(owner.descriptor().clone()).unwrap();
        drop(owner);
        prune(&path, 0, Duration::ZERO);
        assert!(original.exists());
        assert_eq!(successor.read(0).unwrap().0, b"original stderr");
        drop(successor);
        prune(&path, 0, Duration::ZERO);
        assert!(!original.exists());
        assert_eq!(
            fs::read(path.join("not-owned.log")).unwrap(),
            b"leave this alone"
        );
    }
    #[test]
    fn retained_descriptor_cannot_name_a_replaced_or_linked_or_resized_file() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut capture = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        capture.push(b"exact");
        let lease = capture.finish().unwrap();
        let descriptor = lease.descriptor().clone();
        let original = path.join("saved");
        fs::rename(&descriptor.path, &original).unwrap();
        fs::copy(&original, &descriptor.path).unwrap();
        assert!(Lease::open(descriptor.clone()).is_err());
        fs::remove_file(&descriptor.path).unwrap();
        symlink(&original, &descriptor.path).unwrap();
        assert!(Lease::open(descriptor.clone()).is_err());
        fs::remove_file(&descriptor.path).unwrap();
        fs::rename(&original, &descriptor.path).unwrap();
        let link = path.join("hardlink");
        fs::hard_link(&descriptor.path, &link).unwrap();
        assert!(Lease::open(descriptor.clone()).is_err());
        fs::remove_file(link).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&descriptor.path)
            .unwrap()
            .set_len(2)
            .unwrap();
        assert!(Lease::open(descriptor).is_err());
        assert!(lease.read(0).is_err());
    }
    #[test]
    fn write_failure_keeps_the_original_prefix_and_accounts_for_every_later_received_byte() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut capture = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        capture.push(b"kept");
        // Real OS write failure, not a mirrored count model. Retain the old
        // exclusive descriptor while installing a read-only descriptor.
        let writer = std::mem::replace(
            &mut capture.file,
            File::open(&capture.descriptor.path).unwrap(),
        );
        assert!(!capture.push(b"lost"));
        assert!(!capture.push(b"later"));
        assert_eq!(capture.info().bytes, 4);
        assert_eq!(capture.info().omitted, 9);
        assert!(capture.info().write_failed);
        assert_eq!(fs::read(&capture.descriptor.path).unwrap(), b"kept");
        drop(writer);
    }
    #[test]
    fn creation_gate_prevents_pruning_a_new_writer_before_its_log_lock_is_installed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        crate::secure_file::prepare_private_directory(&path).unwrap();
        let held = gate(&path).unwrap();
        assert!(Capture::at(&path, CLOSED_BYTES, RETENTION).is_err());
        drop(held);
        assert!(Capture::at(&path, CLOSED_BYTES, RETENTION).is_ok());
    }
    #[test]
    fn writer_to_shared_transition_keeps_pruners_out_of_the_real_unlocked_gap() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut capture = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        assert!(capture.push(b"original prefix"));
        let original = capture.descriptor.path.clone();
        let lease = capture
            .finish_transition(|file| {
                // Force the documented conversion gap. The independent pruner
                // could lock this log EX now, but must first own the creation gate.
                lock(file, libc::LOCK_UN).unwrap();
                let competing = path.clone();
                std::thread::spawn(move || {
                    assert!(
                        gate(&competing).is_err(),
                        "pruner entered the conversion gap"
                    );
                })
                .join()
                .unwrap();
                assert!(original.exists());
            })
            .unwrap();
        let successor = Lease::open(lease.descriptor().clone()).unwrap();
        drop(lease);
        prune(&path, 0, Duration::ZERO);
        assert_eq!(successor.read(0).unwrap().0, b"original prefix");
        assert!(original.exists());
        drop(successor);
        prune(&path, 0, Duration::ZERO);
        assert!(!original.exists());
    }
    #[test]
    fn closed_budget_and_age_prune_only_the_eligible_oldest_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("logs");
        let mut old = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        old.push(b"old!");
        let old = old.finish().unwrap();
        let old_path = old.descriptor.path.clone();
        drop(old);
        let earlier = SystemTime::now() - Duration::from_secs(120);
        File::open(&old_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(earlier))
            .unwrap();
        let mut new = Capture::at(&path, CLOSED_BYTES, RETENTION).unwrap();
        new.push(b"new!");
        let new = new.finish().unwrap();
        let new_path = new.descriptor.path.clone();
        drop(new);
        prune(&path, 4, RETENTION);
        assert!(
            !old_path.exists(),
            "budget removes the actual oldest closed log"
        );
        assert_eq!(fs::read(&new_path).unwrap(), b"new!");
        File::open(&new_path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(earlier))
            .unwrap();
        prune(&path, CLOSED_BYTES, Duration::from_secs(60));
        assert!(!new_path.exists(), "age applies even below the byte budget");
        let empty = Capture::at(&path, CLOSED_BYTES, RETENTION)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(empty.read(0).unwrap(), (Vec::new(), None));
    }
}
