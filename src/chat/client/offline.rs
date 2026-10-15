//! Last viewed publication, on private disk rather than in every hidden pane.
//!
//! The unlinked file is neither durable provider history nor a reattachment
//! capability. It only keeps an evicted reader usable after transport EOF.
use super::*;
use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::PathBuf,
    sync::{OnceLock, Weak},
};

const BUFFER: usize = 64 * 1024;

struct Publication {
    snapshot: Arc<Snapshot>,
    clock: Option<SourceClock>,
    epoch: u64,
}
struct Cached {
    file: File,
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
            directory: crate::instance::project_dirs().map(|dirs| dirs.cache_dir().join("transcript-readers")),
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
    pub(super) fn capture(self: &Arc<Self>, snapshot: Arc<Snapshot>, clock: Option<SourceClock>) {
        {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if data.revoked { return; }
            data.epoch = data.epoch.wrapping_add(1);
            let epoch = data.epoch;
            data.pending = Some(Publication { snapshot, clock, epoch });
            data.fallback = None;
        }
        self.schedule();
        self.trace();
    }

    pub(super) fn available(&self) -> bool {
        let data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        !data.revoked && (data.pending.is_some() || data.fallback.is_some() || data.cached.is_some()
            || self.in_flight_bytes.load(Ordering::Acquire) != 0)
    }

    pub(super) fn revoked(&self) -> bool {
        self.data.lock().unwrap_or_else(|p| p.into_inner()).revoked
    }

    pub(super) fn request(self: &Arc<Self>, show: bool) {
        {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if data.requested_show == show { return; }
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
        let queued = data.pending.iter().chain(data.fallback.iter())
            .map(|record| record.snapshot.transcript_bytes()).sum();
        (queued, self.in_flight_bytes.load(Ordering::Acquire) as usize,
         self.buffer_bytes.load(Ordering::Acquire) as usize,
         data.cached.as_ref().map_or(0, |cached| cached.bytes))
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
        if self.fail_writes.load(Ordering::Acquire) { return Err("injected disk write failure".into()); }
        let directory = self.directory.as_ref().ok_or("No private disk cache directory is available")?;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(directory).map_err(|e| e.to_string())?;
        let metadata = fs::symlink_metadata(directory).map_err(|e| e.to_string())?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err("Transcript reader cache must be a private owned directory".into());
        }
        let mut file = tempfile::tempfile_in(directory).map_err(|e| e.to_string())?;
        {
            let checked = CheckedWriter { state: self, epoch: record.epoch, file: &mut file };
            let mut writer = BufWriter::with_capacity(BUFFER, checked);
            serde_json::to_writer(&mut writer, &record.snapshot).map_err(|e| e.to_string())?;
            writer.flush().map_err(|e| e.to_string())?;
        }
        let bytes = file.metadata().map_err(|e| e.to_string())?.len();
        if bytes > ipc::MAX_FRAME_BYTES as u64 {
            return Err("Cached publication exceeds its bounded wire window".into());
        }
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        Ok(Cached { file, bytes, clock: record.clock, epoch: record.epoch })
    }

    fn work(self: &Arc<Self>) {
        let record = {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            let record = data.pending.take();
            if let Some(record) = &record {
                self.in_flight_bytes.store(record.snapshot.transcript_bytes() as u64, Ordering::Release);
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
                    Ok(cached) => { data.cached = Some(cached); data.error = None; drop(record); }
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
        if pending { self.schedule(); }
    }

    fn restore(&self) {
        let Some(owner) = self.owner.get().and_then(Weak::upgrade) else { return; };
        // Keep the snapshot -> cache ordering used by visibility transitions.
        let expected_generation = owner.snapshot.lock().unwrap_or_else(|p| p.into_inner()).generation;
        let (request, epoch, cached, fallback) = {
            let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if !data.restore || data.revoked || data.pending.is_some() { return; }
            data.restore = false;
            let file = data.cached.as_ref().and_then(|cached| cached.file.try_clone().ok().map(|file|
                (file, cached.epoch, cached.clock)));
            let fallback = data.fallback.as_ref().map(|record| (record.snapshot.clone(), record.epoch, record.clock));
            (data.request, data.epoch, file, fallback)
        };
        if owner.alive.load(Ordering::Acquire) || !owner.body_requested.load(Ordering::Acquire) { return; }
        // A disk reader must not keep the client alive after its pane closes.
        drop(owner);
        let restored = if let Some((snapshot, record_epoch, clock)) = fallback {
            Ok((snapshot, record_epoch, clock))
        } else if let Some((mut file, record_epoch, clock)) = cached {
            self.buffer_bytes.store(BUFFER as u64, Ordering::Release);
            self.trace();
            let result = file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string()).and_then(|_| {
                let checked = CheckedReader { state: self, epoch, request, file: file.take(ipc::MAX_FRAME_BYTES as u64 + 1) };
                serde_json::from_reader::<_, Snapshot>(BufReader::with_capacity(BUFFER, checked))
                    .map(|snapshot| (Arc::new(snapshot), record_epoch, clock)).map_err(|e| e.to_string())
            });
            self.buffer_bytes.store(0, Ordering::Release);
            result
        } else { return; };
        let Ok((mut restored, record_epoch, clock)) = restored else { return; };
        self.in_flight_bytes.store(restored.transcript_bytes() as u64, Ordering::Release);
        self.trace();
        #[cfg(test)]
        if let Some((ready, proceed)) = self.before_install.lock().unwrap_or_else(|p| p.into_inner()).take() {
            ready.send(()).unwrap();
            proceed.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let Some(owner) = self.owner.get().and_then(Weak::upgrade) else {
            self.in_flight_bytes.store(0, Ordering::Release);
            return;
        };
        let mut snapshot = owner.snapshot.lock().unwrap_or_else(|p| p.into_inner());
        let data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        if data.revoked || data.request != request || data.epoch != epoch || record_epoch != epoch
            || owner.alive.load(Ordering::Acquire) || !owner.body_requested.load(Ordering::Acquire)
            || snapshot.generation != expected_generation {
            self.in_flight_bytes.store(0, Ordering::Release);
            return;
        }
        Arc::make_mut(&mut restored).read_only_reason = Some("Connection lost; showing the last cached view. Reconnect to refresh history or send messages.".into());
        *snapshot = restored;
        *owner.source_clock.lock().unwrap_or_else(|p| p.into_inner()) = clock;
        owner.transcript_resident.store(true, Ordering::Release);
        self.in_flight_bytes.store(0, Ordering::Release);
        drop(data);
        drop(snapshot);
        (owner.wake)(Wake::Changed);
    }
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
/// Partial decoder allocations are bounded by two wire windows but opaque;
/// these counters report complete source owners and fixed I/O buffer capacity.
pub(super) fn global_metrics() -> (usize, usize, usize, u64) {
    let mut states = registry().lock().unwrap_or_else(|p| p.into_inner());
    let mut total = (0, 0, 0, 0);
    states.retain(|state| {
        let Some(state) = state.upgrade() else { return false; };
        let values = state.metrics();
        total.0 += values.0; total.1 += values.1;
        total.2 += values.2; total.3 += values.3;
        true
    });
    total
}

struct CheckedWriter<'a> {
    state: &'a State,
    epoch: u64,
    file: &'a mut File,
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
        if data.revoked || data.epoch != self.epoch || data.request != self.request || !data.requested_show {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Cached view read was revoked or replaced"));
        }
        drop(data);
        self.file.read(bytes)
    }
}
impl Write for CheckedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let data = self.state.data.lock().unwrap_or_else(|p| p.into_inner());
        if data.revoked || data.epoch != self.epoch {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Cached view was revoked or replaced"));
        }
        drop(data);
        self.file.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> { self.file.flush() }
}

fn executor() -> &'static crossbeam_channel::Sender<Weak<State>> {
    static EXECUTOR: OnceLock<crossbeam_channel::Sender<Weak<State>>> = OnceLock::new();
    EXECUTOR.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded::<Weak<State>>();
        for index in 0..2 {
            let rx = rx.clone();
            let _ = thread::Builder::new().name(format!("chat-view-cache-{index}")).spawn(move || {
                trace_worker_stack("chat_offline_cache");
                while let Ok(state) = rx.recv() {
                    if let Some(state) = state.upgrade() { state.work(); }
                }
            });
        }
        tx
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> ClientHandle {
        let mut snapshot = Snapshot { generation: 12, ready: true, session_id: Some("private-offline".into()), ..Snapshot::default() };
        snapshot.items.push(Arc::new(super::super::super::Item {
            id: "offline".into(), role: super::super::super::Role::Assistant,
            content: super::super::super::ItemContent::Text {
                text: "cached source".repeat(16384), thinking: false, phase: Default::default(),
            }, started_at: None, at: None,
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

    fn pause(handle: &ClientHandle) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel();
        *handle.inner.offline.before_install.lock().unwrap() = Some((ready_tx, proceed_rx));
        (ready_rx, proceed_tx)
    }

    fn hidden_on_disk(handle: &ClientHandle) {
        handle.set_visible(false).unwrap();
        wait("disk spill did not finish", || handle.offline_view_metrics().3 > 0 && handle.offline_view_metrics().0 == 0);
        handle.disconnect_for_test();
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
        wait("revoked reader did not settle", || !handle.inner.offline.queued.load(Ordering::Acquire));
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
        let next = Snapshot { generation: 99, title: Some("New live publication".into()), ..Snapshot::default() };
        handle.install_snapshot_for_test(next);
        proceed.send(()).unwrap();
        wait("stale reader did not settle", || !handle.inner.offline.queued.load(Ordering::Acquire));
        assert_eq!(handle.snapshot().generation, 99);
        assert_eq!(handle.snapshot().title.as_deref(), Some("New live publication"));
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
        wait("hidden reader did not settle", || !handle.inner.offline.queued.load(Ordering::Acquire));
        assert_eq!(handle.snapshot().transcript_bytes(), 0);
        assert!(handle.resident_snapshot_with_clock().is_none());
        handle.set_visible(true).unwrap();
        wait("new reveal did not restore", || handle.resident_snapshot_with_clock().is_some());
    }

    #[test]
    fn disk_failure_preserves_a_bounded_memory_fallback() {
        let handle = handle();
        handle.inner.offline.fail_writes.store(true, Ordering::Release);
        let body = Arc::downgrade(&handle.snapshot().items[0]);
        handle.set_visible(false).unwrap();
        wait("failed write did not settle", || !handle.inner.offline.queued.load(Ordering::Acquire));
        assert!(body.upgrade().is_some(), "failed I/O must not destroy the only readable body");
        assert!(handle.offline_view_metrics().0 > 0);
        assert_eq!(handle.offline_view_metrics().3, 0);
        handle.disconnect_for_test();
        handle.set_visible(true).unwrap();
        wait("fallback did not restore", || handle.resident_snapshot_with_clock().is_some());
        assert!(handle.snapshot().read_only_reason.is_some());
        handle.revoke_local_history();
        wait("revoked fallback did not release", || body.upgrade().is_none());
    }
}
