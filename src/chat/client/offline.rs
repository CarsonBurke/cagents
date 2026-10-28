//! Last viewed publication, on private disk rather than in every hidden pane.
//!
//! The unlinked file is neither durable provider history nor a reattachment
//! capability. It only keeps an evicted reader usable after transport EOF.
use super::*;
mod changes;
mod raw;
mod sources;
use crate::chat::source_owners::{ResidentChanges, ResidentRawCalls};
use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::PathBuf,
    sync::{OnceLock, Weak},
};

const BUFFER: usize = 64 * 1024;
// Complete source views can exceed the bounded inline IPC representation.
// Decoding also requires source-owner admission before allocation (union gate).
const MAX_VIEW_BYTES: u64 = 512 * 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
struct View {
    snapshot: Arc<Snapshot>,
    changes: ResidentChanges,
    raw: ResidentRawCalls,
}

struct Publication {
    snapshot: Arc<Snapshot>,
    clock: Option<SourceClock>,
    epoch: u64,
    sources: sources::Manifest,
    changes: ResidentChanges,
    raw: ResidentRawCalls,
}
struct Cached {
    file: File,
    sources: sources::Manifest,
    bytes: u64,
    clock: Option<SourceClock>,
    epoch: u64,
}
#[derive(Default)]
struct Data {
    epoch: u64,
    request: u64,
    pending: Option<Publication>,
    fallback: Option<Publication>,
    cached: Option<Cached>,
    restore: bool,
    requested_show: bool,
    revoked: bool,
    error: Option<String>,
    rendered: bool,
    unavailable: bool,
    restored_changes: Option<ResidentChanges>,
    restored_raw: Option<ResidentRawCalls>,
}

pub(super) struct State {
    id: u64,
    data: Mutex<Data>,
    owner: OnceLock<Weak<Inner>>,
    directory: Option<PathBuf>,
    queued: AtomicBool,
    in_flight_bytes: AtomicU64,
    buffer_bytes: AtomicU64,
    #[cfg(test)]
    fail_writes: AtomicBool,
    #[cfg(test)]
    before_install: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl State {
    pub(super) fn new() -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let state = Arc::new(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            data: Mutex::new(Data::default()),
            owner: OnceLock::new(),
            directory: crate::instance::project_dirs()
                .map(|dirs| dirs.cache_dir().join("transcript-readers")),
            queued: AtomicBool::new(false),
            in_flight_bytes: AtomicU64::new(0),
            buffer_bytes: AtomicU64::new(0),
            #[cfg(test)]
            fail_writes: AtomicBool::new(false),
            #[cfg(test)]
            before_install: Mutex::new(None),
        });
        let mut states = registry().lock().unwrap_or_else(|p| p.into_inner());
        states.retain(|state| state.strong_count() != 0);
        states.push(Arc::downgrade(&state));
        state
    }

    pub(super) fn bind(&self, owner: &Arc<Inner>) {
        let _ = self.owner.set(Arc::downgrade(owner));
    }

    // Called under the owner's snapshot lock. This transfers an existing Arc;
    // it does not clone transcript bodies or perform disk I/O on the UI thread.
    pub(super) fn capture(
        self: &Arc<Self>,
        snapshot: Arc<Snapshot>,
        clock: Option<SourceClock>,
        rendered: bool,
    ) {
        self.capture_changes(snapshot, clock, rendered, ResidentChanges::default());
    }

    pub(super) fn capture_changes(
        self: &Arc<Self>,
        snapshot: Arc<Snapshot>,
        clock: Option<SourceClock>,
        rendered: bool,
        changes: ResidentChanges,
    ) {
        self.capture_sources(
            snapshot,
            clock,
            rendered,
            changes,
            ResidentRawCalls::default(),
        );
    }

    pub(super) fn capture_sources(
        self: &Arc<Self>,
        snapshot: Arc<Snapshot>,
        clock: Option<SourceClock>,
        rendered: bool,
        changes: ResidentChanges,
        raw: ResidentRawCalls,
    ) {
        // Rejected raw/control publications need no body/provenance census.
        // Recheck after capture: deletion or another rendered view can race
        // this short preflight without granting an obsolete view authority.
        {
            let data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if data.revoked || (!rendered && data.rendered) {
                return;
            }
        }
        let sources = match sources::Manifest::capture_all(&snapshot, &changes, &raw) {
            Ok(sources) => sources,
            Err(error) => {
                tracing::warn!(target: "cagents::memory", %error, "Cannot capture an unqualified chat view");
                return;
            }
        };
        {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if data.revoked
                || (!rendered && data.rendered)
                || (rendered && data.unavailable && snapshot.transcript_bytes() == 0)
            {
                return;
            }
            data.rendered |= rendered;
            data.unavailable = false;
            data.epoch = data.epoch.wrapping_add(1);
            let epoch = data.epoch;
            data.pending = Some(Publication {
                snapshot,
                clock,
                epoch,
                sources,
                changes,
                raw,
            });
            data.fallback = None;
        }
        self.schedule();
        self.trace();
    }

    pub(super) fn take_restored_changes(&self) -> Option<ResidentChanges> {
        let restored = self
            .data
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .restored_changes
            .take();
        self.trace();
        restored
    }

    pub(super) fn take_restored_raw_calls(&self) -> Option<ResidentRawCalls> {
        let restored = self
            .data
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .restored_raw
            .take();
        self.trace();
        restored
    }

    pub(super) fn available(&self) -> bool {
        let data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        !data.revoked
            && (data.pending.is_some()
                || data.fallback.is_some()
                || data.cached.is_some()
                || self.in_flight_bytes.load(Ordering::Acquire) != 0)
    }

    pub(super) fn revoked(&self) -> bool {
        self.data.lock().unwrap_or_else(|p| p.into_inner()).revoked
    }

    pub(super) fn request(self: &Arc<Self>, show: bool) {
        {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if data.requested_show == show {
                return;
            }
            if !show {
                data.restored_changes = None;
                data.restored_raw = None;
            }
            data.requested_show = show;
            data.request = data.request.wrapping_add(1);
            data.restore = show && !data.revoked;
        }
        self.schedule();
    }

    /// Invalidates all existing publications and in-flight restores. A file
    /// being written is already unlinked and cannot be installed after this.
    pub(super) fn revoke(&self) {
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        data.revoked = true;
        Self::clear_data(&mut data);
        drop(data);
        self.trace();
    }

    fn clear_data(data: &mut Data) {
        data.epoch = data.epoch.wrapping_add(1);
        data.request = data.request.wrapping_add(1);
        data.pending = None;
        data.fallback = None;
        data.cached = None;
        data.restore = false;
        data.requested_show = false;
        data.rendered = false;
        data.unavailable = false;
        data.restored_changes = None;
        data.restored_raw = None;
    }

    /// Content reset keeps the client usable but removes the previous view.
    pub(super) fn clear(&self) {
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        Self::clear_data(&mut data);
        drop(data);
        self.trace();
    }

    pub(super) fn observe(&self, previous: &Snapshot, next: &Snapshot) {
        if previous.session_id.is_some() && previous.session_id != next.session_id {
            self.clear();
        }
    }

    // Logical Arc payload references, worker payload references, fixed I/O
    // buffer capacity, anonymous file bytes. These are not unique heap/RSS.
    pub(super) fn metrics(&self) -> (usize, usize, usize, u64) {
        let data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        let queued = data
            .pending
            .iter()
            .chain(data.fallback.iter())
            .map(|record| {
                record.snapshot.transcript_bytes()
                    + patch_bytes(&record.changes)
                    + raw_bytes(&record.raw)
            })
            .sum::<usize>()
            + data.restored_changes.as_ref().map_or(0, patch_bytes)
            + data.restored_raw.as_ref().map_or(0, raw_bytes);
        (
            queued,
            self.in_flight_bytes.load(Ordering::Acquire) as usize,
            self.buffer_bytes.load(Ordering::Acquire) as usize,
            data.cached.as_ref().map_or(0, |cached| cached.bytes),
        )
    }

    fn trace(&self) {
        if tracing::enabled!(target: "cagents::memory", tracing::Level::TRACE) {
            let (pending_bytes, in_flight_bytes, io_buffer_capacity, disk_bytes) = self.metrics();
            tracing::trace!(target: "cagents::memory", process_id = std::process::id(),
                owner = "chat_offline", offline_id = self.id, pending_bytes,
                in_flight_bytes, io_buffer_capacity, disk_bytes);
        }
    }

    fn schedule(self: &Arc<Self>) {
        if !self.queued.swap(true, Ordering::AcqRel) {
            if !executor().send(Arc::downgrade(self)).is_ok() {
                self.queued.store(false, Ordering::Release);
            }
        }
    }

    fn write(&self, record: &Publication) -> Result<Cached, String> {
        #[cfg(test)]
        if self.fail_writes.load(Ordering::Acquire) {
            return Err("injected disk write failure".into());
        }
        let directory = self
            .directory
            .as_ref()
            .ok_or("No private disk cache directory is available")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(directory).map_err(|e| e.to_string())?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err("Transcript reader cache must be a private owned directory".into());
        }
        let mut file = tempfile::tempfile_in(directory).map_err(|e| e.to_string())?;
        {
            let checked = CheckedWriter {
                state: self,
                epoch: record.epoch,
                file: &mut file,
                written: 0,
                limit: MAX_VIEW_BYTES,
            };
            let mut writer = BufWriter::with_capacity(BUFFER, checked);
            serde_json::to_writer(
                &mut writer,
                &View {
                    snapshot: record.snapshot.clone(),
                    changes: record.changes.clone(),
                    raw: record.raw.clone(),
                },
            )
            .map_err(|e| e.to_string())?;
            writer.flush().map_err(|e| e.to_string())?;
        }
        let bytes = file.metadata().map_err(|e| e.to_string())?.len();
        if bytes > MAX_VIEW_BYTES {
            return Err("Cached publication exceeds its bounded view file limit".into());
        }
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        Ok(Cached {
            file,
            bytes,
            sources: record.sources.clone(),
            clock: record.clock,
            epoch: record.epoch,
        })
    }

    fn work(self: &Arc<Self>) {
        let record = {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            let record = data.pending.take();
            if let Some(record) = &record {
                self.in_flight_bytes.store(
                    (record.snapshot.transcript_bytes()
                        + patch_bytes(&record.changes)
                        + raw_bytes(&record.raw)) as u64,
                    Ordering::Release,
                );
                self.buffer_bytes.store(BUFFER as u64, Ordering::Release);
            }
            record
        };
        if let Some(record) = record {
            self.trace();
            let written = self.write(&record);
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if !data.revoked && data.epoch == record.epoch {
                match written {
                    Ok(cached) => {
                        data.cached = Some(cached);
                        data.error = None;
                        drop(record);
                    }
                    Err(error) => {
                        tracing::warn!(target: "cagents::memory", %error, "Cannot spill last chat view; keeping its bounded body readable");
                        data.error = Some(error);
                        data.fallback = Some(record);
                    }
                }
            } else {
                drop(record);
            }
            self.in_flight_bytes.store(0, Ordering::Release);
            self.buffer_bytes.store(0, Ordering::Release);
            drop(data);
        }
        self.restore();
        self.trace();
        self.queued.store(false, Ordering::Release);
        let pending = {
            let data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            data.pending.is_some() || data.restore
        };
        if pending {
            self.schedule();
        }
    }

    fn restore_failed(&self, epoch: u64, request: u64, generation: u64, error: String) {
        let Some(owner) = self.owner.get().and_then(Weak::upgrade) else {
            return;
        };
        let mut snapshot = owner.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        if data.revoked
            || data.epoch != epoch
            || data.request != request
            || owner.alive.load(Ordering::Acquire)
            || !owner.body_requested.load(Ordering::Acquire)
            || snapshot.generation != generation
        {
            return;
        }
        data.error = Some(error.clone());
        data.unavailable = true;
        // A failed sidecar restore cannot invalidate an authoritative final
        // transcript that arrived while hidden. Only an evicted body needs the
        // cache-unavailable placeholder; hide/show can retry its private file.
        if !owner.transcript_resident.load(Ordering::Acquire) {
            Arc::make_mut(&mut snapshot).read_only_reason = Some(format!(
                "Connection lost; the last cached view is temporarily unavailable: {error}. Hide and reopen this pane to retry."
            ));
            owner.transcript_resident.store(true, Ordering::Release);
        }
        drop(data);
        drop(snapshot);
        tracing::warn!(target: "cagents::memory", %error, "Cannot restore cached chat view");
        (owner.wake)(Wake::Changed);
    }

    fn restore(&self) {
        let Some(owner) = self.owner.get().and_then(Weak::upgrade) else {
            return;
        };
        // Keep the snapshot -> cache ordering used by visibility transitions.
        let expected_generation = owner
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .generation;
        let (request, epoch, cached, fallback) = {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if !data.restore || data.revoked || data.pending.is_some() {
                return;
            }
            data.restore = false;
            let file = data.cached.as_ref().and_then(|cached| {
                cached
                    .file
                    .try_clone()
                    .ok()
                    .map(|file| (file, cached.epoch, cached.clock, cached.sources.clone()))
            });
            let fallback = data.fallback.as_ref().map(|record| {
                (
                    record.snapshot.clone(),
                    record.epoch,
                    record.clock,
                    record.sources.clone(),
                    record.changes.clone(),
                    record.raw.clone(),
                )
            });
            (data.request, data.epoch, file, fallback)
        };
        if owner.alive.load(Ordering::Acquire) || !owner.body_requested.load(Ordering::Acquire) {
            return;
        }
        // A disk reader must not keep the client alive after its pane closes.
        drop(owner);
        let restored =
            if let Some((mut snapshot, record_epoch, clock, sources, changes, raw)) = fallback {
                // These original Item owners already carry their shared leases.
                sources
                    .normalize_all(Arc::make_mut(&mut snapshot), &changes, &raw)
                    .map(|()| (snapshot, changes, raw, record_epoch, clock))
            } else if let Some((mut file, record_epoch, clock, sources)) = cached {
                self.buffer_bytes.store(BUFFER as u64, Ordering::Release);
                self.trace();
                let result = sources.reserve().and_then(|reservation| {
                    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
                    let checked = CheckedReader {
                        state: self,
                        epoch,
                        request,
                        file: file.take(MAX_VIEW_BYTES + 1),
                    };
                    serde_json::from_reader::<_, View>(BufReader::with_capacity(BUFFER, checked))
                        .map_err(|e| e.to_string())
                        .and_then(|mut view| {
                            sources.adopt_all(
                                Arc::make_mut(&mut view.snapshot),
                                &view.changes,
                                &view.raw,
                                reservation,
                            )?;
                            Ok((view.snapshot, view.changes, view.raw, record_epoch, clock))
                        })
                });
                self.buffer_bytes.store(0, Ordering::Release);
                result
            } else {
                return;
            };
        let (mut restored, mut restored_changes, mut restored_raw, record_epoch, _cached_clock) =
            match restored {
                Ok(restored) => restored,
                Err(error) => {
                    // Preserve the private file for a later reveal. Capacity denial
                    // must never decode a large uncharged body or launch a provider.
                    self.restore_failed(epoch, request, expected_generation, error);
                    return;
                }
            };
        self.in_flight_bytes.store(
            (restored.transcript_bytes()
                + patch_bytes(&restored_changes)
                + raw_bytes(&restored_raw)) as u64,
            Ordering::Release,
        );
        self.trace();
        #[cfg(test)]
        if let Some((ready, proceed)) = self
            .before_install
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            ready.send(()).unwrap();
            proceed.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let Some(owner) = self.owner.get().and_then(Weak::upgrade) else {
            self.in_flight_bytes.store(0, Ordering::Release);
            return;
        };
        let mut snapshot = owner.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        if data.revoked
            || data.request != request
            || data.epoch != epoch
            || record_epoch != epoch
            || owner.alive.load(Ordering::Acquire)
            || !owner.body_requested.load(Ordering::Acquire)
            || snapshot.generation != expected_generation
        {
            self.in_flight_bytes.store(0, Ordering::Release);
            return;
        }
        // An explicit close can publish a complete final transcript while
        // hidden. Its body and source descriptors outrank the older disk view;
        // the cache can still contribute qualified source sidecars.
        let resident = owner.transcript_resident.load(Ordering::Acquire);
        let mut publication = (**snapshot).clone();
        if !resident {
            let cached = Arc::make_mut(&mut restored);
            publication.items = std::mem::take(&mut cached.items);
            // Cached body ranges keep their original epochs rather than being
            // relabelled as the newer control publication.
            publication.source_revision = cached.source_revision;
            publication.source_parts = std::mem::take(&mut cached.source_parts);
            publication.source_parts.retain(|part| {
                part.source
                    .agent_id
                    .as_ref()
                    .is_none_or(|id| publication.subagents.iter().any(|child| &child.id == id))
            });
            for child in &mut publication.subagents {
                if let Some(previous) = cached
                    .subagents
                    .iter_mut()
                    .find(|previous| previous.id == child.id)
                {
                    child.items = std::mem::take(&mut previous.items);
                }
            }
            publication.read_only_reason = Some("Connection lost; showing the last cached view. Reconnect to refresh history or send messages.".into());
        }
        let retained_child = |source: &crate::chat::cache::SourceKey| {
            source
                .agent_id
                .as_ref()
                .is_none_or(|id| publication.subagents.iter().any(|child| &child.id == id))
        };
        restored_changes
            .manifests
            .retain(|page| retained_child(&page.source));
        restored_changes
            .patches
            .retain(|(_, record)| retained_child(&record.source));
        if resident && changes::Manifest::capture(&publication, &restored_changes).is_err() {
            restored_changes = ResidentChanges::default();
        }
        // Raw call authority follows the latest qualified descriptors, not the
        // prose revision or an older cached projection. A hidden removal or
        // replacement must not be undone by disconnected history restoration.
        raw::retain_current(&publication, &mut restored_raw);
        if !resident {
            *snapshot = Arc::new(publication);
        }
        data.restored_raw = Some(restored_raw);
        data.restored_changes = Some(restored_changes);
        data.unavailable = false;
        data.error = None;
        owner.transcript_resident.store(true, Ordering::Release);
        self.in_flight_bytes.store(0, Ordering::Release);
        drop(data);
        drop(snapshot);
        (owner.wake)(Wake::Changed);
    }
}

fn raw_bytes(view: &ResidentRawCalls) -> usize {
    view.calls.iter().map(|(text, _)| text.len()).sum()
}

fn patch_bytes(view: &ResidentChanges) -> usize {
    view.patches.iter().map(|(text, _)| text.len()).sum()
}

impl Drop for State {
    fn drop(&mut self) {
        tracing::trace!(target: "cagents::memory", process_id = std::process::id(),
            owner = "chat_offline", offline_id = self.id, released = true,
            pending_bytes = 0usize, in_flight_bytes = 0usize,
            io_buffer_capacity = 0usize, disk_bytes = 0u64);
    }
}

fn registry() -> &'static Mutex<Vec<Weak<State>>> {
    static STATES: OnceLock<Mutex<Vec<Weak<State>>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Includes the at-most-two executing jobs even after their client closes.
/// Partial decoder allocations include reserved full sources and two inline
/// windows; allocator/serde metadata amplification remains opaque;
/// these counters report complete source owners and fixed I/O buffer capacity.
pub(super) fn global_metrics() -> (usize, usize, usize, u64) {
    let mut states = registry().lock().unwrap_or_else(|p| p.into_inner());
    let mut total = (0, 0, 0, 0);
    states.retain(|state| {
        let Some(state) = state.upgrade() else {
            return false;
        };
        let values = state.metrics();
        total.0 += values.0;
        total.1 += values.1;
        total.2 += values.2;
        total.3 += values.3;
        true
    });
    total
}

struct CheckedWriter<'a> {
    state: &'a State,
    epoch: u64,
    file: &'a mut File,
    written: u64,
    limit: u64,
}
struct CheckedReader<'a> {
    state: &'a State,
    epoch: u64,
    request: u64,
    file: std::io::Take<File>,
}
impl Read for CheckedReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let data = self.state.data.lock().unwrap_or_else(|p| p.into_inner());
        if data.revoked
            || data.epoch != self.epoch
            || data.request != self.request
            || !data.requested_show
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Cached view read was revoked or replaced",
            ));
        }
        drop(data);
        self.file.read(bytes)
    }
}
impl Write for CheckedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let data = self.state.data.lock().unwrap_or_else(|p| p.into_inner());
        if data.revoked || data.epoch != self.epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Cached view was revoked or replaced",
            ));
        }
        drop(data);
        if bytes.len() as u64 > self.limit.saturating_sub(self.written) {
            return Err(io::Error::from_raw_os_error(libc::EFBIG));
        }
        let written = self.file.write(bytes)?;
        self.written += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn executor() -> &'static crossbeam_channel::Sender<Weak<State>> {
    static EXECUTOR: OnceLock<crossbeam_channel::Sender<Weak<State>>> = OnceLock::new();
    EXECUTOR.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded::<Weak<State>>();
        for index in 0..2 {
            let rx = rx.clone();
            let _ = thread::Builder::new()
                .name(format!("chat-view-cache-{index}"))
                .spawn(move || {
                    while let Ok(state) = rx.recv() {
                        if let Some(state) = state.upgrade() {
                            state.work();
                        }
                    }
                });
        }
        tx
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source_epoch(incarnation: u128, revision: u64) -> crate::chat::cache::SourceEpoch {
        crate::chat::cache::SourceEpoch {
            incarnation: uuid::Uuid::from_u128(incarnation),
            revision,
        }
    }

    fn handle() -> ClientHandle {
        let mut snapshot = Snapshot {
            generation: 12,
            ready: true,
            session_id: Some("private-offline".into()),
            ..Snapshot::default()
        };
        snapshot.items.push(Arc::new(super::super::super::Item {
            id: "offline".into(),
            role: super::super::super::Role::Assistant,
            content: super::super::super::ItemContent::Text {
                text: "cached source".repeat(16384),
                thinking: false,
                phase: Default::default(),
            },
            started_at: None,
            at: None,
        }));
        ClientHandle::fixture(super::super::tests::omp_card(None, None), snapshot)
    }

    fn wait(description: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "{description}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn pause(
        handle: &ClientHandle,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        *handle.inner.offline.before_install.lock().unwrap() = Some((ready_tx, proceed_rx));
        (ready_rx, proceed_tx)
    }

    fn hidden_on_disk(handle: &ClientHandle) {
        handle.set_visible(false).unwrap();
        wait("disk spill did not finish", || {
            handle.offline_view_metrics().3 > 0 && handle.offline_view_metrics().0 == 0
        });
        handle.disconnect_for_test();
    }

    #[test]
    fn original_call_spills_restores_once_and_delete_revokes_unconsumed_owner() {
        let (mut snapshot, raw) = raw::tests::fixture();
        snapshot.generation = 17;
        snapshot.ready = true;
        snapshot.session_id = Some("private-offline-original".into());
        let handle = ClientHandle::fixture(super::super::tests::omp_card(None, None), snapshot);
        let original = Arc::downgrade(&raw.calls[0].0);
        handle.remember_visible_view_with_sources(handle.snapshot(), None, Default::default(), raw);
        hidden_on_disk(&handle);
        wait("original call remained in memory after spill", || {
            original.upgrade().is_none()
        });
        handle.set_visible(true).unwrap();
        wait("original call restore did not complete", || {
            handle.resident_snapshot().is_some()
        });
        let restored = handle
            .take_restored_raw_calls()
            .expect("decoded original call");
        assert!(restored.calls[0].0.contains("complete original call"));
        assert_eq!(
            crate::chat::source_owners::resident_raw(&restored.calls[0].0),
            Some(restored.calls[0].1.clone())
        );
        assert!(handle.take_restored_raw_calls().is_none());
        drop(restored);
        handle.set_visible(false).unwrap();
        handle.set_visible(true).unwrap();
        wait("second original restore did not complete", || {
            handle.resident_snapshot().is_some()
        });
        handle.revoke_local_history();
        assert!(handle.take_restored_raw_calls().is_none());
        assert_eq!(handle.offline_view_metrics().3, 0);
    }

    #[test]
    fn hidden_raw_removal_with_unchanged_prose_revision_cannot_restore_old_call() {
        let (mut snapshot, raw) = raw::tests::fixture();
        snapshot.generation = 19;
        snapshot.session_id = Some("private-offline-removed-original".into());
        let handle = ClientHandle::fixture(super::super::tests::omp_card(None, None), snapshot);
        handle.remember_visible_view_with_sources(handle.snapshot(), None, Default::default(), raw);
        hidden_on_disk(&handle);
        let prose_revision = handle.snapshot().source_revision;
        {
            let mut current = handle.inner.snapshot.lock().unwrap();
            let next = Arc::make_mut(&mut current);
            next.generation += 1;
            next.raw_source_parts.clear();
        }
        handle.set_visible(true).unwrap();
        wait("cached prose restoration did not settle", || {
            handle.resident_snapshot().is_some()
        });
        assert_eq!(handle.snapshot().source_revision, prose_revision);
        assert!(handle.snapshot().raw_source_parts.is_empty());
        assert!(
            handle.take_restored_raw_calls().unwrap().calls.is_empty(),
            "latest Raw authority wins even without a prose revision change"
        );
    }

    #[test]
    fn a_view_file_limit_is_enforced_before_oversized_output_hits_disk() {
        let state = State::new();
        let mut file = tempfile::tempfile().unwrap();
        let mut writer = CheckedWriter {
            state: &state,
            epoch: 0,
            file: &mut file,
            written: 0,
            limit: 8,
        };
        writer.write_all(b"1234").unwrap();
        let error = writer.write_all(b"56789").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
        writer.write_all(b"5678").unwrap();
        drop(writer);
        assert_eq!(file.metadata().unwrap().len(), 8);
    }

    #[test]
    fn revoked_account_cannot_install_a_completed_disk_read() {
        let handle = handle();
        hidden_on_disk(&handle);
        let (ready, proceed) = pause(&handle);
        handle.set_visible(true).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.revoke_local_history();
        proceed.send(()).unwrap();
        wait("revoked reader did not settle", || {
            !handle.inner.offline.queued.load(Ordering::Acquire)
        });
        assert_eq!(handle.snapshot().transcript_bytes(), 0);
        assert!(handle.resident_snapshot_with_clock().is_none());
        assert_eq!(handle.offline_view_metrics().3, 0);
    }

    #[test]
    fn late_disk_read_never_replaces_a_new_live_publication() {
        let handle = handle();
        hidden_on_disk(&handle);
        let (ready, proceed) = pause(&handle);
        handle.set_visible(true).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.inner.alive.store(true, Ordering::Release);
        let next = Snapshot {
            generation: 99,
            title: Some("New live publication".into()),
            ..Snapshot::default()
        };
        handle.install_snapshot_for_test(next);
        proceed.send(()).unwrap();
        wait("stale reader did not settle", || {
            !handle.inner.offline.queued.load(Ordering::Acquire)
        });
        assert_eq!(handle.snapshot().generation, 99);
        assert_eq!(
            handle.snapshot().title.as_deref(),
            Some("New live publication")
        );
        assert!(handle.snapshot().read_only_reason.is_none());
    }

    #[test]
    fn hiding_again_fences_an_inflight_restore() {
        let handle = handle();
        hidden_on_disk(&handle);
        let (ready, proceed) = pause(&handle);
        handle.set_visible(true).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.set_visible(false).unwrap();
        proceed.send(()).unwrap();
        wait("hidden reader did not settle", || {
            !handle.inner.offline.queued.load(Ordering::Acquire)
        });
        assert_eq!(handle.snapshot().transcript_bytes(), 0);
        assert!(handle.resident_snapshot_with_clock().is_none());
        handle.set_visible(true).unwrap();
        wait("new reveal did not restore", || {
            handle.resident_snapshot_with_clock().is_some()
        });
    }

    #[test]
    fn disk_failure_preserves_a_bounded_memory_fallback() {
        let handle = handle();
        handle
            .inner
            .offline
            .fail_writes
            .store(true, Ordering::Release);
        let body = Arc::downgrade(&handle.snapshot().items[0]);
        handle.set_visible(false).unwrap();
        wait("failed write did not settle", || {
            !handle.inner.offline.queued.load(Ordering::Acquire)
        });
        assert!(
            body.upgrade().is_some(),
            "failed I/O must not destroy the only readable body"
        );
        assert!(handle.offline_view_metrics().0 > 0);
        assert_eq!(handle.offline_view_metrics().3, 0);
        handle.disconnect_for_test();
        handle.set_visible(true).unwrap();
        wait("fallback did not restore", || {
            handle.resident_snapshot_with_clock().is_some()
        });
        assert!(handle.snapshot().read_only_reason.is_some());
        handle.revoke_local_history();
        wait("revoked fallback did not release", || {
            body.upgrade().is_none()
        });
    }

    #[test]
    fn truncated_view_restore_preserves_resident_final_and_explains_evicted_failure() {
        for (resident_final, final_reason) in [
            (true, Some("Authoritative closed transcript")),
            (true, None),
            (false, None),
        ] {
            let handle = handle();
            hidden_on_disk(&handle);
            wait("hidden cache worker did not settle", || {
                !handle.inner.offline.queued.load(Ordering::Acquire)
            });
            {
                let data = handle.inner.offline.data.lock().unwrap();
                let cached = data.cached.as_ref().expect("completed disk spill");
                assert_eq!(cached.file.metadata().unwrap().nlink(), 0);
                assert!(cached.bytes > 0);
                // Damage the actual owned anonymous file, not a decoder stub.
                cached.file.set_len(0).unwrap();
            }
            if resident_final {
                // Model the final reader publication under its snapshot lock;
                // the wire test separately drives authenticated ChatClosed.
                let final_snapshot = Snapshot {
                    generation: 99,
                    ready: true,
                    session_id: Some("private-offline".into()),
                    exited: true,
                    finished_at: Some(99),
                    read_only_reason: final_reason.map(str::to_owned),
                    items: vec![Arc::new(super::super::super::Item {
                        id: "authoritative-final".into(),
                        role: super::super::super::Role::Assistant,
                        content: super::super::super::ItemContent::Text {
                            text: "authoritative final body".into(),
                            thinking: false,
                            phase: Default::default(),
                        },
                        started_at: None,
                        at: None,
                    })],
                    ..Snapshot::default()
                };
                let mut current = handle.inner.snapshot.lock().unwrap();
                *current = Arc::new(final_snapshot);
                handle
                    .inner
                    .transcript_resident
                    .store(true, Ordering::Release);
            }
            let before = handle.snapshot();
            let (epoch, request) = {
                let data = handle.inner.offline.data.lock().unwrap();
                (data.epoch, data.request)
            };
            handle.set_visible(true).unwrap();
            wait("truncated cache read did not fail", || {
                handle.inner.offline.data.lock().unwrap().unavailable
            });
            wait("failed restore worker did not settle", || {
                !handle.inner.offline.queued.load(Ordering::Acquire)
            });
            let after = handle
                .resident_snapshot()
                .expect("readable final or status");
            if resident_final {
                assert!(Arc::ptr_eq(&before, &after));
                assert_eq!(after.as_ref(), before.as_ref());
                assert_eq!(after.read_only_reason.as_deref(), final_reason);
                assert!(after.exited);
                assert_eq!(after.finished_at, Some(99));
                let super::super::super::ItemContent::Text { text, .. } = &after.items[0].content
                else {
                    panic!("final text");
                };
                assert_eq!(text, "authoritative final body");
            } else {
                assert!(!Arc::ptr_eq(&before, &after));
                assert_eq!(after.transcript_bytes(), 0);
                let reason = after.read_only_reason.as_deref().unwrap();
                assert!(reason.contains("last cached view is temporarily unavailable"));
                assert!(reason.contains("Hide and reopen this pane to retry"));
                assert_eq!(after.generation, before.generation);
                assert_eq!(after.exited, before.exited);
            }
            let data = handle.inner.offline.data.lock().unwrap();
            assert!(data.error.as_ref().is_some_and(|error| !error.is_empty()));
            assert!(data.unavailable);
            assert_eq!(data.epoch, epoch);
            assert_eq!(data.request, request.wrapping_add(1));
            assert!(!data.restore);
            assert!(!data.revoked);
            assert!(data.cached.is_some(), "retain the file for a later retry");
            assert!(data.restored_raw.is_none());
            assert!(data.restored_changes.is_none());
        }
    }

    #[test]
    fn last_projected_view_wins_over_a_later_bounded_raw_eviction() {
        let handle = handle();
        let mut view = (*handle.snapshot()).clone();
        let mut item = (*view.items[0]).clone();
        let super::super::super::ItemContent::Text { text, .. } = &mut item.content else {
            panic!("text");
        };
        *text = "complete projected prefix\n".repeat(64 * 1024);
        let expected = text.len();
        view.items[0] = Arc::new(item);
        let source = crate::chat::cache::SourceKey {
            agent_id: None,
            source_id: view.items[0].id.clone(),
        };
        assert_eq!(
            crate::chat::source_owners::admit(&view.items[0], &source, source_epoch(1, 7), None),
            crate::chat::source_owners::Admission::Ready
        );
        let original_metadata =
            crate::chat::cache::SourceMetadata::from_item(&view.items[0]).unwrap();
        // A replacement can be known while the last rendered prefix still has
        // its original frozen epoch and source timestamps.
        view.source_parts.push(crate::chat::cache::SourcePart {
            item_id: "new-raw-part".into(),
            source,
            start: 0,
            end: 8,
            source_len: expected + 99,
            replacement: false,
            epoch: source_epoch(2, 7),
            metadata: crate::chat::cache::SourceMetadata {
                at: Some(999),
                ..original_metadata.clone()
            },
        });
        handle.remember_visible_view(Arc::new(view), None);
        hidden_on_disk(&handle);
        handle.set_visible(true).unwrap();
        wait("projected cache did not restore", || {
            handle.resident_snapshot_with_clock().is_some()
        });
        let snapshot = handle.snapshot();
        let super::super::super::ItemContent::Text { text, .. } = &snapshot.items[0].content else {
            panic!("text");
        };
        assert_eq!(
            text.len(),
            expected,
            "raw inline eviction must not replace the displayed view"
        );
        assert_eq!(snapshot.source_parts.len(), 1);
        let part = &snapshot.source_parts[0];
        assert_eq!(
            (part.start, part.end, part.source_len, part.epoch),
            (0, expected, expected, source_epoch(1, 7))
        );
        assert_eq!(part.metadata, original_metadata);
        assert_eq!(part.item_id, snapshot.items[0].id);
        assert!(!part.replacement);
    }

    #[test]
    fn a_late_view_from_another_native_conversation_is_not_cached() {
        let handle = handle();
        let mut late = (*handle.snapshot()).clone();
        late.session_id = Some("different-conversation".into());
        late.items.clear();
        handle.remember_visible_view(Arc::new(late), None);
        hidden_on_disk(&handle);
        handle.set_visible(true).unwrap();
        wait("current cache did not restore", || {
            handle.resident_snapshot_with_clock().is_some()
        });
        assert!(handle.snapshot().transcript_bytes() > 0);
        assert_eq!(
            handle.snapshot().session_id.as_deref(),
            Some("private-offline")
        );
    }
}
