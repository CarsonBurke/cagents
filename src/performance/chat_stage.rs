//! Opt-in development attribution of inclusive wall time, never CPU time.
//!
//! Stage indices: 0 refresh, 1 parent preparation, 2 child preparation, 3 parse,
//! 4 highlight, 5 turn build, 6 shape, 7 retained clone. Schema 1 clock 1 denotes
//! Linux CLOCK_MONOTONIC nanoseconds. Unmatched started/ended counts expose
//! scopes still open at flush or a failed end clock; clock failures are explicit.
//! Bins are half-open one-second
//! intervals from `origin_monotonic_ns`. Elapsed time is split at bin boundaries;
//! started/ended count endpoints independently. `crossing_calls` counts calls
//! touching a bin that are not wholly contained in it. Nested stages overlap,
//! so neither stage nor bin elapsed sums represent exclusive work. Recording
//! stops at 1024 bins; summaries retain elapsed time beyond that finite horizon.
use std::io;

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub enum Stage {
    Refresh,
    PrepareParent,
    PrepareChild,
    Parse,
    Highlight,
    TurnBuild,
    Shape,
    RetainedClone,
}

/// Deliberately thread-bound: a scope must end on the UI thread that began it.
#[must_use]
pub struct Scope {
    #[cfg(any(feature = "development", test))]
    started: Option<(Stage, u64)>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[inline]
pub fn scope(stage: Stage) -> Scope {
    #[cfg(any(feature = "development", test))]
    {
        recording::begin(stage)
    }
    #[cfg(not(any(feature = "development", test)))]
    {
        let _ = stage;
        Scope {
            _thread_bound: std::marker::PhantomData,
        }
    }
}

/// Call once on the UI thread during startup. Ordinary production is inert.
pub fn initialize_from_environment() -> io::Result<()> {
    #[cfg(feature = "development")]
    return recording::initialize();
    #[cfg(not(feature = "development"))]
    Ok(())
}

/// Explicit normal-close output only. Scope and recorder destructors do no I/O.
pub fn flush() -> io::Result<()> {
    #[cfg(feature = "development")]
    return recording::flush();
    #[cfg(not(feature = "development"))]
    Ok(())
}

#[cfg(any(feature = "development", test))]
impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((stage, started)) = self.started {
            recording::end(stage, started);
        }
    }
}

#[cfg(any(feature = "development", test))]
mod recording {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    const STAGES: usize = 8;
    const BINS: usize = 1024;
    const SECOND: u64 = 1_000_000_000;
    static ENABLED: AtomicBool = AtomicBool::new(false);
    #[cfg(feature = "development")]
    static INITIALIZED: AtomicBool = AtomicBool::new(false);

    #[derive(Clone, Copy, Default, serde::Serialize)]
    struct Summary {
        started: u64,
        ended: u64,
        inclusive_ns: u64,
        max_call_ns: u64,
        outside_horizon_ns: u64,
        endpoints_outside_horizon: u64,
        backward_clock: u64,
    }
    #[derive(Clone, Copy, Default, serde::Serialize)]
    struct Bin {
        started: u64,
        ended: u64,
        crossing_calls: u64,
        inclusive_overlap_ns: u64,
        max_overlap_ns: u64,
    }
    struct Recorder {
        origin: u64,
        summaries: [Summary; STAGES],
        bins: [[Bin; STAGES]; BINS],
        clock_failures: u64,
        #[cfg(feature = "development")]
        output: Option<Output>,
    }
    impl Recorder {
        fn new(origin: u64) -> Self {
            Self {
                origin,
                summaries: [Summary::default(); STAGES],
                bins: [[Bin::default(); STAGES]; BINS],
                clock_failures: 0,
                #[cfg(feature = "development")]
                output: None,
            }
        }
        fn endpoint(&mut self, stage: Stage, at: u64, start: bool) {
            let index = at.checked_sub(self.origin).map(|n| n / SECOND);
            let summary = &mut self.summaries[stage as usize];
            let count = if start {
                &mut summary.started
            } else {
                &mut summary.ended
            };
            *count = count.saturating_add(1);
            if let Some(index) = index.filter(|&n| n < BINS as u64) {
                let bin = &mut self.bins[index as usize][stage as usize];
                let count = if start {
                    &mut bin.started
                } else {
                    &mut bin.ended
                };
                *count = count.saturating_add(1);
            } else {
                summary.endpoints_outside_horizon =
                    summary.endpoints_outside_horizon.saturating_add(1);
            }
        }
        fn finish(&mut self, stage: Stage, start: u64, end: u64) {
            self.endpoint(stage, end, false);
            let summary = &mut self.summaries[stage as usize];
            let Some(elapsed) = end.checked_sub(start) else {
                summary.backward_clock = summary.backward_clock.saturating_add(1);
                return;
            };
            summary.inclusive_ns = summary.inclusive_ns.saturating_add(elapsed);
            summary.max_call_ns = summary.max_call_ns.max(elapsed);
            // Relative, clamped endpoints avoid overflow even near u64::MAX.
            let lower = start.saturating_sub(self.origin).min(BINS as u64 * SECOND);
            let upper = end.saturating_sub(self.origin).min(BINS as u64 * SECOND);
            let covered = upper.saturating_sub(lower);
            summary.outside_horizon_ns = summary
                .outside_horizon_ns
                .saturating_add(elapsed.saturating_sub(covered));
            if covered == 0 {
                return;
            }
            let first = (lower / SECOND) as usize;
            let last = ((upper - 1) / SECOND) as usize;
            for index in first..=last {
                let bin_start = index as u64 * SECOND;
                let bin_end = bin_start + SECOND;
                let overlap = upper.min(bin_end) - lower.max(bin_start);
                let bin = &mut self.bins[index][stage as usize];
                bin.inclusive_overlap_ns = bin.inclusive_overlap_ns.saturating_add(overlap);
                bin.max_overlap_ns = bin.max_overlap_ns.max(overlap);
                // Endpoints exactly at the right boundary belong to the next
                // bin, even though all elapsed time may lie in this bin.
                if start.checked_sub(self.origin).is_none_or(|n| n < bin_start)
                    || end.saturating_sub(self.origin) >= bin_end
                {
                    bin.crossing_calls = bin.crossing_calls.saturating_add(1);
                }
            }
        }
    }
    thread_local! {
        static RECORDER: RefCell<Option<Box<Recorder>>> = const { RefCell::new(None) };
    }
    fn monotonic_ns() -> io::Result<u64> {
        let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: clock_gettime initializes a timespec on success.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, value.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let value = unsafe { value.assume_init() };
        let seconds =
            u64::try_from(value.tv_sec).map_err(|_| io::Error::other("negative monotonic time"))?;
        let nanos = u64::try_from(value.tv_nsec)
            .map_err(|_| io::Error::other("negative monotonic fraction"))?;
        if nanos >= SECOND {
            return Err(io::Error::other("invalid monotonic fraction"));
        }
        seconds
            .checked_mul(SECOND)
            .and_then(|n| n.checked_add(nanos))
            .ok_or_else(|| io::Error::other("monotonic time overflow"))
    }
    fn begin_with_clock(
        enabled: bool,
        stage: Stage,
        clock: impl FnOnce() -> io::Result<u64>,
    ) -> Scope {
        let mut started = None;
        if enabled {
            RECORDER.with(|cell| {
                let mut state = cell.borrow_mut();
                if let Some(recorder) = state.as_mut() {
                    match clock() {
                        Ok(at) => {
                            recorder.endpoint(stage, at, true);
                            started = Some((stage, at));
                        }
                        Err(_) => {
                            recorder.clock_failures = recorder.clock_failures.saturating_add(1)
                        }
                    }
                }
            });
        }
        Scope {
            started,
            _thread_bound: std::marker::PhantomData,
        }
    }
    pub(super) fn begin(stage: Stage) -> Scope {
        begin_with_clock(ENABLED.load(Ordering::Relaxed), stage, monotonic_ns)
    }
    pub(super) fn end(stage: Stage, started: u64) {
        if !ENABLED.load(Ordering::Relaxed) {
            return;
        }
        RECORDER.with(|cell| {
            if let Some(recorder) = cell.borrow_mut().as_mut() {
                match monotonic_ns() {
                    Ok(at) => recorder.finish(stage, started, at),
                    Err(_) => recorder.clock_failures = recorder.clock_failures.saturating_add(1),
                }
            }
        });
    }

    #[cfg(feature = "development")]
    struct Output {
        directory: std::os::fd::OwnedFd,
        leaf: std::ffi::CString,
    }

    #[cfg(feature = "development")]
    fn output(path: &std::path::Path) -> io::Result<Output> {
        use std::os::{fd::FromRawFd, unix::ffi::OsStrExt};
        if !path.is_absolute() {
            return Err(io::Error::other("stage output must be absolute"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("missing output parent"))?;
        let leaf = path
            .file_name()
            .ok_or_else(|| io::Error::other("missing output filename"))?;
        let directory = std::ffi::CString::new(parent.as_os_str().as_bytes())?;
        let leaf = std::ffi::CString::new(leaf.as_bytes())?;
        // SAFETY: CString remains alive, flags require an existing directory.
        let fd = unsafe {
            libc::open(
                directory.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let directory = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        validate_directory(&directory)?;
        // Freshness is enforced atomically at flush, never by a check-then-write.
        Ok(Output { directory, leaf })
    }
    #[cfg(feature = "development")]
    fn validate_directory(directory: &std::os::fd::OwnedFd) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat initializes stat on success; the owned FD stays live.
        if unsafe { libc::fstat(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_uid != unsafe { libc::geteuid() }
            || stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || stat.st_mode & 0o7777 != 0o700
        {
            return Err(io::Error::other(
                "stage output parent must be owned by effective user and mode 0700",
            ));
        }
        Ok(())
    }
    #[cfg(feature = "development")]
    fn write_output(output: &Output, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};
        validate_directory(&output.directory)?;
        // SAFETY: retained directory FD and CString remain live. Creation is
        // exclusive, cannot follow the leaf, and grants no group/other access.
        let fd = unsafe {
            libc::openat(
                output.directory.as_raw_fd(),
                output.leaf.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        // File has no user-space buffer and performs no implicit writes on Drop.
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()
    }
    #[cfg(feature = "development")]
    pub(super) fn initialize() -> io::Result<()> {
        if INITIALIZED.swap(true, Ordering::Relaxed) {
            return Err(io::Error::other("stage recorder already initialized"));
        }
        let Some(path) = std::env::var_os("CAGENTS_CHAT_STAGE_OUTPUT") else {
            return Ok(());
        };
        let output = output(std::path::Path::new(&path))?;
        let mut recorder = Box::new(Recorder::new(monotonic_ns()?));
        recorder.output = Some(output);
        RECORDER.with(|cell| *cell.borrow_mut() = Some(recorder));
        ENABLED.store(true, Ordering::Relaxed);
        Ok(())
    }
    #[cfg(feature = "development")]
    pub(super) fn flush() -> io::Result<()> {
        RECORDER.with(|cell| {
            let mut state = cell.borrow_mut();
            let Some(recorder) = state.as_mut() else {
                return if ENABLED.load(Ordering::Relaxed) {
                    Err(io::Error::other(
                        "stage flush must run on initializing UI thread",
                    ))
                } else {
                    Ok(())
                };
            };
            let Some(output) = recorder.output.take() else {
                return Ok(());
            };
            ENABLED.store(false, Ordering::Relaxed);
            let finished = monotonic_ns()?;
            #[derive(serde::Serialize)]
            struct Row {
                index: usize,
                stages: [Bin; STAGES],
            }
            #[derive(serde::Serialize)]
            struct Report<'a> {
                schema: u32,
                clock: u32,
                inclusive_wall: bool,
                origin_monotonic_ns: u64,
                finished_monotonic_ns: u64,
                bucket_width_ns: u64,
                bucket_capacity: usize,
                clock_failures: u64,
                stages: &'a [Summary; STAGES],
                buckets: Vec<Row>,
            }
            let used = finished
                .saturating_sub(recorder.origin)
                .checked_div(SECOND)
                .unwrap_or(0)
                .saturating_add(1)
                .min(BINS as u64) as usize;
            let buckets = (0..used)
                .map(|index| Row {
                    index,
                    stages: recorder.bins[index],
                })
                .collect();
            let bytes = serde_json::to_vec(&Report {
                schema: 1,
                clock: 1,
                inclusive_wall: true,
                origin_monotonic_ns: recorder.origin,
                finished_monotonic_ns: finished,
                bucket_width_ns: SECOND,
                bucket_capacity: BINS,
                clock_failures: recorder.clock_failures,
                stages: &recorder.summaries,
                buckets,
            })?;
            write_output(&output, &bytes)
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn disabled_does_not_read_injected_clock() {
            let scope = begin_with_clock(false, Stage::Parse, || panic!("disabled clock"));
            assert!(scope.started.is_none());
        }
        #[test]
        fn intersections_preserve_partial_calls_and_boundary_endpoints() {
            let mut recorder = Recorder::new(10);
            recorder.endpoint(Stage::Parse, 10 + SECOND / 2, true);
            recorder.finish(Stage::Parse, 10 + SECOND / 2, 10 + SECOND * 2);
            let stage = Stage::Parse as usize;
            assert_eq!(recorder.bins[0][stage].inclusive_overlap_ns, SECOND / 2);
            assert_eq!(recorder.bins[1][stage].inclusive_overlap_ns, SECOND);
            assert_eq!(recorder.bins[0][stage].crossing_calls, 1);
            assert_eq!(recorder.bins[1][stage].crossing_calls, 1);
            assert_eq!(recorder.bins[2][stage].ended, 1);
            assert_eq!(recorder.bins[2][stage].inclusive_overlap_ns, 0);
            assert_eq!(recorder.summaries[stage].inclusive_ns, SECOND + SECOND / 2);
        }
        #[test]
        fn backward_clock_does_not_invent_elapsed_time() {
            let mut recorder = Recorder::new(0);
            recorder.endpoint(Stage::Shape, 20, true);
            recorder.finish(Stage::Shape, 20, 19);
            assert_eq!(recorder.summaries[Stage::Shape as usize].backward_clock, 1);
            assert_eq!(recorder.summaries[Stage::Shape as usize].inclusive_ns, 0);
        }
        #[test]
        fn zero_duration_at_boundary_has_only_endpoints() {
            let mut recorder = Recorder::new(0);
            recorder.endpoint(Stage::Parse, SECOND, true);
            recorder.finish(Stage::Parse, SECOND, SECOND);
            let bin = recorder.bins[1][Stage::Parse as usize];
            assert_eq!(bin.started, 1);
            assert_eq!(bin.ended, 1);
            assert_eq!(bin.inclusive_overlap_ns, 0);
            assert_eq!(bin.crossing_calls, 0);
        }
        #[test]
        fn near_integer_limit_does_not_invent_a_crossing() {
            let mut recorder = Recorder::new(u64::MAX - 10);
            recorder.endpoint(Stage::Shape, u64::MAX - 5, true);
            recorder.finish(Stage::Shape, u64::MAX - 5, u64::MAX);
            let bin = recorder.bins[0][Stage::Shape as usize];
            assert_eq!(bin.inclusive_overlap_ns, 5);
            assert_eq!(bin.crossing_calls, 0);
        }
        #[test]
        fn finite_horizon_and_saturation_are_bounded() {
            let mut recorder = Recorder::new(0);
            let stage = Stage::Refresh as usize;
            recorder.endpoint(Stage::Refresh, SECOND * (BINS as u64 - 1), true);
            recorder.finish(Stage::Refresh, SECOND * (BINS as u64 - 1), u64::MAX);
            assert_eq!(recorder.bins[BINS - 1][stage].inclusive_overlap_ns, SECOND);
            assert_eq!(recorder.summaries[stage].endpoints_outside_horizon, 1);
            assert_eq!(
                recorder.summaries[stage].outside_horizon_ns,
                u64::MAX - SECOND * BINS as u64
            );
            recorder.summaries[stage].inclusive_ns = u64::MAX;
            recorder.finish(Stage::Refresh, 1, 2);
            assert_eq!(recorder.summaries[stage].inclusive_ns, u64::MAX);
            recorder.bins[0][stage].started = u64::MAX;
            recorder.endpoint(Stage::Refresh, 0, true);
            assert_eq!(recorder.bins[0][stage].started, u64::MAX);
        }

        #[cfg(feature = "development")]
        mod output_tests {
            use super::*;
            use std::os::unix::fs::{PermissionsExt, symlink};
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            struct Directory(std::path::PathBuf);
            impl Directory {
                fn new() -> Self {
                    // No environment mutation, shared recorder state or reused
                    // paths. Existing paths make the test fail rather than write.
                    let path = std::env::temp_dir().join(format!(
                        "cagents-stage-output-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed)
                    ));
                    std::fs::create_dir(&path).unwrap();
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                    Self(path)
                }
            }
            impl Drop for Directory {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            #[test]
            fn exclusive_leaf_never_overwrites_or_follows_symlink() {
                let directory = Directory::new();
                let path = directory.0.join("metrics");
                let handle = output(&path).unwrap();
                write_output(&handle, b"{} ").unwrap();
                assert!(write_output(&handle, b"changed").is_err());
                assert_eq!(std::fs::read(&path).unwrap(), b"{} \n");
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                let linked = directory.0.join("linked");
                symlink(&path, &linked).unwrap();
                assert!(write_output(&output(&linked).unwrap(), b"changed").is_err());
                assert_eq!(std::fs::read(&path).unwrap(), b"{} \n");
            }
            #[test]
            fn retained_parent_survives_name_replacement() {
                let directory = Directory::new();
                let parent = directory.0.join("parent");
                std::fs::create_dir(&parent).unwrap();
                std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
                let handle = output(&parent.join("metrics")).unwrap();
                let pinned = directory.0.join("pinned");
                std::fs::rename(&parent, &pinned).unwrap();
                std::fs::create_dir(&parent).unwrap();
                write_output(&handle, b"{}").unwrap();
                assert_eq!(std::fs::read(pinned.join("metrics")).unwrap(), b"{}\n");
                assert!(!parent.join("metrics").exists());
            }
            #[test]
            fn changed_parent_mode_is_rejected_before_creation() {
                let directory = Directory::new();
                let path = directory.0.join("metrics");
                let handle = output(&path).unwrap();
                std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o750))
                    .unwrap();
                assert!(write_output(&handle, b"{}").is_err());
                assert!(!path.exists());
            }
        }
    }
}
