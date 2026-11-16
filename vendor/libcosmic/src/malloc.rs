// Copyright 2025 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

use std::os::raw::c_int;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

const M_MMAP_THRESHOLD: c_int = -3;

/// How long the application must go without an update or view before the
/// heap is trimmed.
const QUIET: Duration = Duration::from_secs(1);

unsafe extern "C" {
    fn malloc_trim(pad: usize) -> c_int;

    fn mallopt(param: c_int, value: c_int) -> c_int;
}

/// Prevents glibc from hoarding memory via memory fragmentation.
#[inline]
pub fn limit_mmap_threshold(threshold: i32) {
    unsafe {
        mallopt(M_MMAP_THRESHOLD, threshold as c_int);
    }
}

/// Records UI activity, so that freed heap memory is returned to the system
/// once the application goes quiet.
///
/// `malloc_trim` walks every arena and takes milliseconds on a large heap, so
/// trimming after every update and view made it one of the UI thread's
/// largest costs. A helper thread trims instead, after [`QUIET`] without
/// activity: memory is still returned after a burst of work, and the UI
/// thread never waits on the walk. An idle application causes no wakeups.
#[inline]
pub fn note_activity() {
    let trimmer = TRIMMER.get_or_init(Trimmer::spawn);
    trimmer
        .last_activity
        .store(trimmer.elapsed_millis(), Ordering::Relaxed);
    if !trimmer.pending.swap(true, Ordering::AcqRel) {
        if let Some(thread) = &trimmer.thread {
            thread.unpark();
        }
    }
}

static TRIMMER: OnceLock<Trimmer> = OnceLock::new();

struct Trimmer {
    epoch: Instant,
    /// Milliseconds since `epoch` of the latest update or view.
    last_activity: AtomicU64,
    /// Whether there has been activity since the last trim.
    pending: AtomicBool,
    /// `None` if the helper thread could not be started, in which case the
    /// heap is never trimmed, as with any other allocator.
    thread: Option<Thread>,
}

impl Trimmer {
    fn spawn() -> Self {
        let thread = thread::Builder::new()
            .name("cosmic-heap-trim".into())
            .spawn(|| {
                let trimmer = TRIMMER.wait();
                loop {
                    while !trimmer.pending.load(Ordering::Acquire) {
                        thread::park();
                    }
                    trimmer.wait_for_quiet();
                    // Clear before trimming: activity during the walk marks
                    // the heap dirty again and is trimmed after its own
                    // quiet period.
                    trimmer.pending.store(false, Ordering::Release);
                    unsafe {
                        malloc_trim(0);
                    }
                }
            })
            .ok()
            .map(|handle| handle.thread().clone());
        Self {
            epoch: Instant::now(),
            last_activity: AtomicU64::new(0),
            pending: AtomicBool::new(false),
            thread,
        }
    }

    fn elapsed_millis(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Sleeps until no activity has been noted for [`QUIET`], waking at most
    /// once per quiet period while the application stays busy.
    fn wait_for_quiet(&self) {
        let quiet = QUIET.as_millis() as u64;
        loop {
            let since = self
                .elapsed_millis()
                .saturating_sub(self.last_activity.load(Ordering::Relaxed));
            if since >= quiet {
                return;
            }
            thread::sleep(Duration::from_millis(quiet - since));
        }
    }
}

