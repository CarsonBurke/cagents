//! Timing of a compositor's presentation of one frame, for an application's
//! frame tracing.
//!
//! A shell that traces frames brackets each [`Compositor::present`] with
//! [`begin`] and [`end`]; a compositor that supports it marks the end of each
//! [`Stage`] with [`lap`]. Until a shell calls [`enable`] on the presenting
//! thread, every call reads one thread-local flag and no clock.
//!
//! [`Compositor::present`]: crate::compositor::Compositor::present
use std::cell::Cell;
use std::time::{Duration, Instant};

/// One part of presenting a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Waiting for the surface to hand out an image to draw into.
    Acquire,
    /// Uploading the frame's layers to the GPU.
    Prepare,
    /// Recording the render passes that draw the layers. A backend that
    /// defers encoding, as wgpu does, only queues the commands here.
    Render,
    /// Trimming the caches and atlases the frame no longer uses.
    Trim,
    /// Finishing the command buffer, which is where wgpu validates and
    /// encodes the recorded passes.
    Finish,
    /// Submitting the command buffer to the GPU queue.
    Submit,
    /// Handing the drawn image back to the surface for display.
    Swap,
}

/// How long presenting one frame took, by [`Stage`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Timing {
    /// See [`Stage::Acquire`].
    pub acquire: Duration,
    /// See [`Stage::Prepare`].
    pub prepare: Duration,
    /// See [`Stage::Render`].
    pub render: Duration,
    /// See [`Stage::Trim`].
    pub trim: Duration,
    /// See [`Stage::Finish`].
    pub finish: Duration,
    /// See [`Stage::Submit`].
    pub submit: Duration,
    /// See [`Stage::Swap`].
    pub swap: Duration,
    /// How many layers the frame drew, where the compositor reports it.
    pub layers: Option<usize>,
    /// Wall-clock time from [`begin`] to [`end`].
    pub total: Duration,
    /// CPU time the presenting thread used from [`begin`] to [`end`], where
    /// the platform reports it. Its shortfall from [`Timing::total`] is time
    /// the thread spent blocked, such as on the GPU or the compositor, or
    /// runnable but preempted.
    pub cpu: Option<Duration>,
}

struct Presenting {
    started: Instant,
    cpu: Option<Duration>,
    last: Instant,
    timing: Timing,
}

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static PRESENTING: Cell<Option<Presenting>> = const { Cell::new(None) };
    static PRESENTED: Cell<Option<Timing>> = const { Cell::new(None) };
}

/// Times presentations on this thread from now on.
pub fn enable() {
    ENABLED.set(true);
}

/// Starts timing a presentation, if timing is enabled on this thread.
pub fn begin() {
    if ENABLED.get() {
        let now = Instant::now();
        PRESENTING.set(Some(Presenting {
            started: now,
            cpu: thread_cpu_time(),
            last: now,
            timing: Timing::default(),
        }));
    }
}

/// Counts the time since the previous lap, or since [`begin`], as `stage`.
/// Does nothing outside a timed presentation.
pub fn lap(stage: Stage) {
    let Some(mut presenting) = PRESENTING.take() else {
        return;
    };
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(presenting.last);
    presenting.last = now;
    let timing = &mut presenting.timing;
    *match stage {
        Stage::Acquire => &mut timing.acquire,
        Stage::Prepare => &mut timing.prepare,
        Stage::Render => &mut timing.render,
        Stage::Trim => &mut timing.trim,
        Stage::Finish => &mut timing.finish,
        Stage::Submit => &mut timing.submit,
        Stage::Swap => &mut timing.swap,
    } += elapsed;
    PRESENTING.set(Some(presenting));
}

/// Notes how many layers the timed presentation draws. Does nothing outside
/// a timed presentation.
pub fn layers(count: usize) {
    let Some(mut presenting) = PRESENTING.take() else {
        return;
    };
    presenting.timing.layers = Some(count);
    PRESENTING.set(Some(presenting));
}

/// Finishes timing the presentation [`begin`] started, for [`take`].
pub fn end() {
    let Some(presenting) = PRESENTING.take() else {
        return;
    };
    let mut timing = presenting.timing;
    timing.total = presenting.started.elapsed();
    timing.cpu = presenting
        .cpu
        .zip(thread_cpu_time())
        .map(|(start, end)| end.saturating_sub(start));
    PRESENTED.set(Some(timing));
}

/// The timing of the last presentation on this thread, once.
pub fn take() -> Option<Timing> {
    PRESENTED.take()
}

#[cfg(unix)]
fn thread_cpu_time() -> Option<Duration> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid, writable `timespec` for the call's duration.
    let status = unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time)
    };
    (status == 0).then(|| {
        Duration::new(
            u64::try_from(time.tv_sec).unwrap_or_default(),
            u32::try_from(time.tv_nsec).unwrap_or_default(),
        )
    })
}

#[cfg(not(unix))]
fn thread_cpu_time() -> Option<Duration> {
    None
}
