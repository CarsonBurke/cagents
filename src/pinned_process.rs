//! A process addressed by its identity rather than by its reusable number.
//!
//! A numeric pid names whichever process holds it *now*. Between reading a pid
//! from a lease and signalling it, the process can exit and the number can be
//! handed to an unrelated one, so every decision here is made through a pin
//! taken once and checked again before it is acted on.
//!
//! * On Linux the pin is a pidfd. It refers to one process for its whole
//!   lifetime, polls readable once that process exits, and `pidfd_send_signal`
//!   never reaches a successor.
//! * Darwin has no pidfd. The pin is a kqueue `EVFILT_PROC` registration, which
//!   the kernel binds to the process itself and fires `NOTE_EXIT` for exactly
//!   once, together with the process's start time. A signal is sent only while
//!   the registration has not fired and the start time still matches; the one
//!   gap left is the instant between that check and `kill`, which would need the
//!   process to exit and its pid to be reissued within it. Darwin issues pids
//!   sequentially across a space of 99 999, so that would take the whole space
//!   wrapping in microseconds.

#[cfg(target_os = "linux")]
use std::fs::{self, File};
#[cfg(target_os = "macos")]
use std::os::fd::OwnedFd;
use std::{
    fs::Metadata,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::MetadataExt, net::UnixStream},
    },
    path::PathBuf,
};

/// The credentials the kernel recorded for the other end of a local socket
/// when it connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeerCredentials {
    pub(crate) uid: u32,
    pub(crate) pid: u32,
}

/// The peer's credentials, as the kernel recorded them at `connect` time.
///
/// # Errors
///
/// Returns the underlying error when the socket cannot be queried, and
/// `InvalidData` when the kernel's answer is not the expected record.
#[cfg(target_os = "linux")]
pub(crate) fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
    // SAFETY: all-zero bytes are a valid `ucred`, which getsockopt overwrites.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .expect("ucred fits socklen_t");
    // SAFETY: both buffers are writable, sized as declared, and the stream owns
    // a live socket.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the kernel returned a short peer credential",
        ));
    }
    let pid = u32::try_from(credentials.pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid peer pid"))?;
    Ok(PeerCredentials {
        uid: credentials.uid,
        pid,
    })
}

/// The peer's credentials.
///
/// Darwin splits them across two options. `LOCAL_PEERCRED` answers the
/// effective uid recorded at `connect`, as Linux does. `LOCAL_PEERPID` does
/// not: it names the last process to use the peer's socket, which a server
/// socket takes from its listener, and it fails with `ENOTCONN` once the peer
/// closes. The pid is therefore only a candidate; every caller compares it to
/// an owner it has already proven, or pins it and then verifies the peer's
/// proof.
///
/// # Errors
///
/// Returns the underlying error when the socket cannot be queried, and
/// `InvalidData` when the kernel's answer is not the expected record.
#[cfg(target_os = "macos")]
pub(crate) fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCredentials> {
    let uid = peer_uid(stream)?;
    let mut pid: libc::pid_t = 0;
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::pid_t>())
        .expect("pid_t fits socklen_t");
    // SAFETY: both buffers are writable, sized as declared, and the stream owns
    // a live socket.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let pid = u32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid peer pid"))?;
    Ok(PeerCredentials { uid, pid })
}

/// The peer's effective uid, as the kernel recorded it at `connect` time. It
/// reads only `LOCAL_PEERCRED`, so it still answers after the peer closes.
///
/// # Errors
///
/// Returns the underlying error when the socket cannot be queried, and
/// `InvalidData` when the kernel's answer is not the expected record.
#[cfg(target_os = "macos")]
pub(crate) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    /// `XUCRED_VERSION` from `<sys/ucred.h>`.
    const XUCRED_VERSION: libc::c_uint = 0;
    // SAFETY: all-zero bytes are a valid `xucred`, which getsockopt overwrites.
    let mut credentials: libc::xucred = unsafe { std::mem::zeroed() };
    let mut length = libc::socklen_t::try_from(std::mem::size_of::<libc::xucred>())
        .expect("xucred fits socklen_t");
    // SAFETY: both buffers are writable, sized as declared, and the stream owns
    // a live socket.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERCRED,
            (&raw mut credentials).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let version = credentials.cr_version;
    if length as usize != std::mem::size_of::<libc::xucred>() || version != XUCRED_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the kernel returned an unexpected peer credential",
        ));
    }
    Ok(credentials.cr_uid)
}

/// The peer's effective uid, as the kernel recorded it at `connect` time.
///
/// # Errors
///
/// As [`peer_credentials`].
#[cfg(all(target_os = "linux", feature = "host"))]
pub(crate) fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    peer_credentials(stream).map(|credentials| credentials.uid)
}

/// Which of two processes [`peer_credentials`] names for the end of a
/// socketpair that `creator` made and handed to `recipient`, once `recipient`
/// has used it. Linux records the creator when the pair is made and never
/// changes it; Darwin names the last process to use the end, which is then
/// `recipient`.
#[cfg(feature = "host")]
pub(crate) fn handed_end_peer<T>(creator: T, recipient: T) -> T {
    #[cfg(target_os = "linux")]
    {
        let _ = recipient;
        creator
    }
    #[cfg(target_os = "macos")]
    {
        let _ = creator;
        recipient
    }
}

/// One process, pinned when it was opened.
#[derive(Debug)]
pub(crate) struct PinnedProcess {
    pid: u32,
    pin: imp::Pin,
}

impl PinnedProcess {
    /// Pins the process that holds `pid` now.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when no such process exists (or it has already
    /// exited), `InvalidInput` for pid 0 or 1, and the underlying error for
    /// every other failure.
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        if pid <= 1 || libc::pid_t::try_from(pid).is_err() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"));
        }
        let pin = imp::Pin::open(pid)?;
        let process = Self { pid, pin };
        if process.exited()? {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(process)
    }

    /// The number the process held when it was pinned.
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the pinned process has exited. A zombie has exited.
    ///
    /// # Errors
    ///
    /// Returns the underlying error when the pin cannot be queried.
    pub(crate) fn exited(&self) -> io::Result<bool> {
        self.pin.exited(self.pid)
    }

    /// Waits up to `timeout` for the pinned process to exit, answering whether
    /// it did. A signal interrupting the wait does not cut it short.
    ///
    /// # Errors
    ///
    /// Returns the underlying error when the pin cannot be waited on.
    pub(crate) fn wait_for_exit(&self, timeout: std::time::Duration) -> io::Result<bool> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.exited()? {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            match self.pin.wait(remaining) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Signals exactly the pinned process; a successor to its pid is never
    /// addressed. `Ok(false)` means it had already exited.
    ///
    /// # Errors
    ///
    /// Returns the underlying error for any failure other than the process
    /// being gone.
    pub(crate) fn signal(&self, signal: libc::c_int) -> io::Result<bool> {
        self.pin.signal(self.pid, signal)
    }

    /// The effective uid of the pinned process.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when the process has exited, so the answer can never
    /// describe a successor.
    pub(crate) fn uid(&self) -> io::Result<u32> {
        self.while_running(self.pin.uid(self.pid))
    }

    /// The executable the pinned process is running.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when the process has exited, so the answer can never
    /// describe a successor.
    pub(crate) fn executable(&self) -> io::Result<PathBuf> {
        self.while_running(self.pin.executable(self.pid))
    }

    /// Whether the pinned process holds a descriptor open on the inode
    /// `expected` describes.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when the process has exited, so the answer can never
    /// describe a successor.
    pub(crate) fn holds_open(&self, expected: &Metadata) -> io::Result<bool> {
        self.while_running(self.pin.holds_open(self.pid, expected))
    }

    /// Whether `other` pins the same process as this pin. Both must still be
    /// running: a number reused after an exit names a different process, and
    /// neither pin can vouch for the other once its own process is gone.
    ///
    /// # Errors
    ///
    /// Returns the underlying error when either pin cannot be queried.
    pub(crate) fn is_same_process(&self, other: &Self) -> io::Result<bool> {
        Ok(self.pid == other.pid && !self.exited()? && !other.exited()?)
    }

    /// When the pinned process started, in `mach_absolute_time` ticks since
    /// boot: with the pid, an identity no later process can share.
    #[cfg(target_os = "macos")]
    pub(crate) fn started_ticks(&self) -> u64 {
        self.pin.started_ticks()
    }

    /// What `read` found, while the pinned process is still running. A read
    /// that fails because the process exited under it reports `NotFound`,
    /// whatever error the platform gave for the vanished process.
    fn while_running<T>(&self, read: io::Result<T>) -> io::Result<T> {
        match read {
            Ok(value) => {
                self.still_running()?;
                Ok(value)
            }
            Err(_) if self.exited()? => Err(io::Error::from(io::ErrorKind::NotFound)),
            Err(error) => Err(error),
        }
    }

    fn still_running(&self) -> io::Result<()> {
        if self.exited()? {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(())
    }
}

/// When `pid` started, in `mach_absolute_time` ticks since boot, read without
/// pinning it: with the pid, an identity no later process can share. As with
/// Linux's start ticks, a zombie still reads until its parent reaps it, and a
/// change of the wall clock moves nothing.
///
/// # Errors
///
/// Returns `NotFound` when no process holds `pid`, and the underlying error
/// for every other failure.
#[cfg(target_os = "macos")]
pub(crate) fn started_ticks_of(pid: u32) -> io::Result<u64> {
    imp::start_of(pid).map(imp::Start::ticks)
}

/// When, by the wall clock, the process that holds `pid` and started at
/// `ticks` (as [`started_ticks_of`] reads it) began. The kernel recorded the
/// time at fork, so a later change of the wall clock does not move it.
///
/// # Errors
///
/// Returns `NotFound` when that process no longer holds `pid`, including when
/// it has become a zombie, which keeps no record of its wall-clock start.
#[cfg(all(target_os = "macos", any(feature = "host", test)))]
pub(crate) fn wall_clock_start_of(pid: u32, ticks: u64) -> io::Result<std::time::SystemTime> {
    let info = imp::bsd_info(pid)?;
    // The caller read `ticks` from this pid before the record was read, and
    // the same process still holds the pid after: a pid can't be reused while
    // held, so no successor can have answered in between.
    if imp::start_of(pid)?.ticks() != ticks {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    let offset = std::time::Duration::from_secs(info.pbi_start_tvsec)
        + std::time::Duration::from_micros(info.pbi_start_tvusec);
    std::time::UNIX_EPOCH
        .checked_add(offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "process start is out of range"))
}

/// How many of `pid`'s children have exited but not been reaped. XNU lists a
/// zombie among its parent's children and still accepts a null signal for it,
/// but keeps no process record for it.
///
/// # Errors
///
/// Returns the underlying error when the children cannot be listed.
#[cfg(target_os = "macos")]
pub(crate) fn zombie_children_of(pid: u32) -> io::Result<usize> {
    Ok(imp::children(pid)?
        .into_iter()
        .filter(|&child| {
            matches!(imp::bsd_info(child), Err(error) if error.kind() == io::ErrorKind::NotFound)
                // SAFETY: a null signal only asks whether the pid is in use.
                && (unsafe { libc::kill(child as libc::pid_t, 0) } == 0
                    || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
        })
        .count())
}

/// Reports `NotFound` for the errno a vanished process answers.
fn vanished(error: io::Error) -> io::Error {
    if error.raw_os_error() == Some(libc::ESRCH) {
        io::Error::from(io::ErrorKind::NotFound)
    } else {
        error
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    #[derive(Debug)]
    pub(super) struct Pin {
        pidfd: File,
    }

    impl Pin {
        pub(super) fn open(pid: u32) -> io::Result<Self> {
            // SAFETY: pidfd_open takes scalar arguments and returns a new
            // owned descriptor or -1 without writing through any pointer.
            let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if descriptor < 0 {
                return Err(vanished(io::Error::last_os_error()));
            }
            // SAFETY: a successful pidfd_open returned a fresh descriptor.
            let pidfd = unsafe { File::from_raw_fd(descriptor as libc::c_int) };
            Ok(Self { pidfd })
        }

        pub(super) fn exited(&self, _pid: u32) -> io::Result<bool> {
            let mut poll = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one initialized pollfd referring to the pinned process.
            if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(poll.revents != 0)
        }

        /// Blocks until the pidfd is readable — the process exited — or the
        /// timeout passes, rounding up so a sub-millisecond rest cannot spin.
        pub(super) fn wait(&self, timeout: std::time::Duration) -> io::Result<()> {
            let mut poll = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let milliseconds = timeout
                .as_nanos()
                .div_ceil(1_000_000)
                .min(libc::c_int::MAX as u128) as libc::c_int;
            // SAFETY: one initialized pollfd referring to the pinned process.
            if unsafe { libc::poll(&mut poll, 1, milliseconds) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(super) fn signal(&self, _pid: u32, signal: libc::c_int) -> io::Result<bool> {
            // SAFETY: the pidfd is owned here, and a null siginfo with zero
            // flags is the documented kill(2)-equivalent form.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(false);
                }
                return Err(error);
            }
            Ok(true)
        }

        pub(super) fn uid(&self, pid: u32) -> io::Result<u32> {
            Ok(fs::symlink_metadata(format!("/proc/{pid}"))?.uid())
        }

        pub(super) fn executable(&self, pid: u32) -> io::Result<PathBuf> {
            fs::read_link(format!("/proc/{pid}/exe"))
        }

        pub(super) fn holds_open(&self, pid: u32, expected: &Metadata) -> io::Result<bool> {
            Ok(fs::read_dir(format!("/proc/{pid}/fd"))?
                .filter_map(Result::ok)
                .filter_map(|entry| fs::metadata(entry.path()).ok())
                .any(|actual| actual.dev() == expected.dev() && actual.ino() == expected.ino()))
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// `PROC_PIDFDVNODEINFO` from `<sys/proc_info.h>`.
    const PROC_PIDFDVNODEINFO: libc::c_int = 1;

    /// `struct proc_fileinfo` from `<sys/proc_info.h>`.
    #[repr(C)]
    struct ProcFileInfo {
        fi_openflags: u32,
        fi_status: u32,
        fi_offset: libc::off_t,
        fi_type: i32,
        fi_guardflags: u32,
    }

    /// `struct vnode_fdinfo` from `<sys/proc_info.h>`.
    #[repr(C)]
    struct VnodeFdInfo {
        pfi: ProcFileInfo,
        pvi: libc::vnode_info,
    }

    /// When a process started, in `mach_absolute_time` ticks since boot, which
    /// together with its pid names it uniquely. Unlike the wall-clock start
    /// in `proc_bsdinfo`, it is readable for a zombie, cannot be moved by a
    /// clock change, and orders processes as Linux's start ticks do.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct Start(u64);

    #[derive(Debug)]
    pub(super) struct Pin {
        /// A kqueue holding one `EVFILT_PROC`/`NOTE_EXIT` registration.
        watch: OwnedFd,
        start: Start,
        /// The exit event is delivered once; remember having seen it.
        exited: AtomicBool,
    }

    pub(super) fn bsd_info(pid: u32) -> io::Result<libc::proc_bsdinfo> {
        // SAFETY: all-zero bytes are a valid `proc_bsdinfo`.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>())
            .expect("proc_bsdinfo fits c_int");
        // SAFETY: the buffer is writable and exactly `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        if written <= 0 {
            let error = io::Error::last_os_error();
            // A vanished process answers ESRCH, or nothing at all.
            return Err(match error.raw_os_error() {
                Some(0) | None => io::Error::from(io::ErrorKind::NotFound),
                _ => vanished(error),
            });
        }
        if written != size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the kernel returned a short process record",
            ));
        }
        Ok(info)
    }

    pub(super) fn start_of(pid: u32) -> io::Result<Start> {
        let pid =
            libc::c_int::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::NotFound))?;
        // SAFETY: all-zero bytes are a valid `rusage_info_v0`.
        let mut usage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
        // SAFETY: the flavor names exactly the record the pointer addresses,
        // which lives for the call; the kernel only writes it.
        let status = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V0,
                (&raw mut usage).cast::<libc::rusage_info_t>(),
            )
        };
        if status != 0 {
            return Err(vanished(io::Error::last_os_error()));
        }
        Ok(Start(usage.ri_proc_start_abstime))
    }

    impl Start {
        pub(super) fn ticks(self) -> u64 {
            self.0
        }
    }

    /// The pids of `pid`'s children, zombies included.
    pub(super) fn children(pid: u32) -> io::Result<Vec<u32>> {
        let mut capacity = 64;
        loop {
            let mut pids = vec![0 as libc::pid_t; capacity];
            let size = libc::c_int::try_from(pids.len() * std::mem::size_of::<libc::pid_t>())
                .map_err(|_| io::Error::other("too many child processes"))?;
            // SAFETY: the buffer is writable and exactly `size` bytes; the call
            // answers how many pids it wrote.
            let count = unsafe {
                libc::proc_listchildpids(pid as libc::pid_t, pids.as_mut_ptr().cast(), size)
            };
            let count = usize::try_from(count).map_err(|_| vanished(io::Error::last_os_error()))?;
            // A full buffer may have been cut short; ask again with more room.
            if count < capacity {
                pids.truncate(count);
                return Ok(pids
                    .into_iter()
                    .filter_map(|pid| u32::try_from(pid).ok())
                    .collect());
            }
            capacity *= 4;
        }
    }

    impl Pin {
        pub(super) fn started_ticks(&self) -> u64 {
            self.start.ticks()
        }

        pub(super) fn open(pid: u32) -> io::Result<Self> {
            // SAFETY: kqueue takes no arguments and returns a new owned
            // descriptor or -1.
            let queue = unsafe { libc::kqueue() };
            if queue < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a successful kqueue returned a fresh descriptor.
            let watch = unsafe { OwnedFd::from_raw_fd(queue) };
            // kqueue descriptors are not inherited across fork, but mark it
            // close-on-exec as well so no exec'd child sees it.
            crate::descriptor::set_close_on_exec(std::os::fd::AsFd::as_fd(&watch))?;
            // Register first, then read the start time: the registration binds
            // to whichever process held the pid at that instant, and if that
            // process exits before the start time is read, the exit check in
            // `PinnedProcess::open` sees the event and refuses the pin.
            let change = libc::kevent {
                ident: pid as libc::uintptr_t,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD | libc::EV_RECEIPT,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: an all-zero `kevent` is a valid output record.
            let mut receipt: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: one input and one output record, both live for the call,
            // and a null timeout is not consulted for an EV_RECEIPT change.
            let count = unsafe {
                libc::kevent(
                    watch.as_raw_fd(),
                    &change,
                    1,
                    &mut receipt,
                    1,
                    std::ptr::null(),
                )
            };
            if count < 0 {
                return Err(vanished(io::Error::last_os_error()));
            }
            // EV_RECEIPT reports the registration's own result in `data`.
            if count == 1 && receipt.flags & libc::EV_ERROR != 0 && receipt.data != 0 {
                return Err(vanished(io::Error::from_raw_os_error(receipt.data as i32)));
            }
            let start = start_of(pid)?;
            Ok(Self {
                watch,
                start,
                exited: AtomicBool::new(false),
            })
        }

        /// Whether the pinned process has exited. The kernel reports the exit
        /// event once, so a pin also reads the process table: a number held by
        /// nothing, or by a process that started at another time, has exited
        /// too.
        pub(super) fn exited(&self, pid: u32) -> io::Result<bool> {
            if self.collect(std::time::Duration::ZERO)? {
                return Ok(true);
            }
            let exited = match start_of(pid) {
                Ok(start) => start != self.start,
                Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                Err(error) => return Err(error),
            };
            if exited {
                self.exited.store(true, Ordering::Release);
            }
            Ok(exited)
        }

        pub(super) fn wait(&self, timeout: std::time::Duration) -> io::Result<()> {
            self.collect(timeout).map(drop)
        }

        /// Takes the exit event if it arrives within `timeout`, remembering it,
        /// since the kernel delivers it only once.
        fn collect(&self, timeout: std::time::Duration) -> io::Result<bool> {
            if self.exited.load(Ordering::Acquire) {
                return Ok(true);
            }
            // SAFETY: an all-zero `kevent` is a valid output record.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let timeout = libc::timespec {
                tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
                tv_nsec: libc::c_long::from(timeout.subsec_nanos() as i32),
            };
            // SAFETY: no changes, one output record and a timeout, all live
            // for the call.
            let count = unsafe {
                libc::kevent(
                    self.watch.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    &mut event,
                    1,
                    &timeout,
                )
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if count > 0 && event.fflags & libc::NOTE_EXIT != 0 {
                self.exited.store(true, Ordering::Release);
                return Ok(true);
            }
            Ok(false)
        }

        /// Whether `pid` still names the pinned process: it has not exited and
        /// the process holding the number started when the pinned one did.
        fn current(&self, pid: u32) -> io::Result<bool> {
            self.exited(pid).map(|exited| !exited)
        }

        pub(super) fn signal(&self, pid: u32, signal: libc::c_int) -> io::Result<bool> {
            if !self.current(pid)? {
                return Ok(false);
            }
            // SAFETY: kill takes scalar arguments.
            if unsafe { libc::kill(pid as libc::pid_t, signal) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(false);
                }
                return Err(error);
            }
            Ok(true)
        }

        pub(super) fn uid(&self, pid: u32) -> io::Result<u32> {
            // The caller proves the record still describes the pinned process.
            bsd_info(pid).map(|info| info.pbi_uid)
        }

        pub(super) fn executable(&self, pid: u32) -> io::Result<PathBuf> {
            use std::os::unix::ffi::OsStringExt;
            let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            // SAFETY: the buffer is writable and exactly as long as declared.
            let written = unsafe {
                libc::proc_pidpath(
                    pid as libc::c_int,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                )
            };
            if written <= 0 {
                return Err(vanished(io::Error::last_os_error()));
            }
            buffer.truncate(written as usize);
            Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
        }

        pub(super) fn holds_open(&self, pid: u32, expected: &Metadata) -> io::Result<bool> {
            let descriptors = descriptors(pid)?;
            let found = descriptors
                .iter()
                .filter(|descriptor| descriptor.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
                .any(|descriptor| {
                    vnode_identity(pid, descriptor.proc_fd).is_ok_and(|(device, inode)| {
                        device == expected.dev() && inode == expected.ino()
                    })
                });
            Ok(found)
        }
    }

    /// Every descriptor `pid` holds, as `PROC_PIDLISTFDS` reports them.
    fn descriptors(pid: u32) -> io::Result<Vec<libc::proc_fdinfo>> {
        let entry = std::mem::size_of::<libc::proc_fdinfo>();
        // A zero-length query answers the size the list needs now; it can
        // grow before the list is read, so leave room and retry if it filled.
        let mut capacity = 64;
        loop {
            let mut list: Vec<libc::proc_fdinfo> = Vec::with_capacity(capacity);
            let bytes = libc::c_int::try_from(capacity * entry)
                .map_err(|_| io::Error::other("descriptor list too large"))?;
            // SAFETY: the buffer has room for `capacity` records, and the
            // kernel writes at most `bytes` bytes of them.
            let written = unsafe {
                libc::proc_pidinfo(
                    pid as libc::c_int,
                    libc::PROC_PIDLISTFDS,
                    0,
                    list.as_mut_ptr().cast(),
                    bytes,
                )
            };
            if written <= 0 {
                let error = io::Error::last_os_error();
                return Err(match error.raw_os_error() {
                    Some(0) | None => io::Error::from(io::ErrorKind::NotFound),
                    _ => vanished(error),
                });
            }
            let count = written as usize / entry;
            if count < capacity {
                // SAFETY: the kernel initialized the first `count` records.
                unsafe { list.set_len(count) };
                return Ok(list);
            }
            capacity *= 2;
        }
    }

    /// The (device, inode) of the vnode behind `pid`'s descriptor `fd`.
    fn vnode_identity(pid: u32, fd: i32) -> io::Result<(u64, u64)> {
        // SAFETY: all-zero bytes are a valid `vnode_fdinfo`.
        let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<VnodeFdInfo>())
            .expect("vnode_fdinfo fits c_int");
        // SAFETY: the buffer is writable and exactly `size` bytes.
        let written = unsafe {
            libc::proc_pidfdinfo(
                pid as libc::c_int,
                fd,
                PROC_PIDFDVNODEINFO,
                (&raw mut info).cast(),
                size,
            )
        };
        if written != size {
            return Err(io::Error::last_os_error());
        }
        // `vst_dev` is the same `dev_t` `stat` reports, widened as
        // `MetadataExt::dev` widens it.
        let stat = &info.pvi.vi_stat;
        Ok((stat.vst_dev as i32 as u64, stat.vst_ino))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Command, time::Duration};

    /// A child that holds `held` open on descriptor 9 until it is signalled.
    fn holder(held: &std::path::Path) -> std::process::Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg("exec 9<\"$1\"; exec sleep 30")
            .arg("sh")
            .arg(held)
            .spawn()
            .unwrap()
    }

    /// Waits until the child has replaced its shell with `sleep`, so its
    /// descriptors are the ones the test set up.
    fn settled(process: &PinnedProcess) {
        for _ in 0..200 {
            if process
                .executable()
                .is_ok_and(|path| path.file_name().is_some_and(|name| name == "sleep"))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the holder never started sleeping");
    }

    #[test]
    fn a_pin_answers_for_its_process_until_it_exits_and_never_after() {
        let directory = tempfile::tempdir().unwrap();
        let held = directory.path().join("held");
        let other = directory.path().join("other");
        std::fs::write(&held, b"").unwrap();
        std::fs::write(&other, b"").unwrap();
        let mut child = holder(&held);
        let process = PinnedProcess::open(child.id()).unwrap();
        settled(&process);

        // SAFETY: geteuid has no preconditions.
        assert_eq!(process.uid().unwrap(), unsafe { libc::geteuid() });
        assert!(
            process
                .holds_open(&std::fs::metadata(&held).unwrap())
                .unwrap()
        );
        assert!(
            !process
                .holds_open(&std::fs::metadata(&other).unwrap())
                .unwrap()
        );
        assert!(!process.exited().unwrap());

        assert!(!process.wait_for_exit(Duration::from_millis(20)).unwrap());
        assert!(process.signal(libc::SIGTERM).unwrap());
        // A child is a zombie until reaped, and a zombie has exited.
        assert!(process.wait_for_exit(Duration::from_secs(10)).unwrap());
        child.wait().unwrap();
        assert!(process.exited().unwrap());
        assert!(process.exited().unwrap(), "an exit is remembered");
        assert!(!process.signal(libc::SIGTERM).unwrap());
        for answer in [
            process.uid().map(drop),
            process.executable().map(drop),
            process
                .holds_open(&std::fs::metadata(&held).unwrap())
                .map(drop),
        ] {
            assert_eq!(answer.unwrap_err().kind(), io::ErrorKind::NotFound);
        }
    }

    #[test]
    fn an_exited_process_cannot_be_pinned() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        let pid = child.id();
        // Unreaped, it is a zombie: either refused or pinned as exited.
        std::thread::sleep(Duration::from_millis(100));
        match PinnedProcess::open(pid) {
            Ok(process) => assert!(process.exited().unwrap()),
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
        }
        child.wait().unwrap();
        assert_eq!(
            PinnedProcess::open(pid).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        for reserved in [0, 1] {
            assert_eq!(
                PinnedProcess::open(reserved).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    /// A zombie keeps the start it had while it ran, as Linux's `/proc` entry
    /// does, so a recorded identity still names it until its parent reaps it,
    /// and a process started later reads a later start.
    #[test]
    #[cfg(target_os = "macos")]
    fn start_ticks_name_a_process_until_it_is_reaped() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("read line")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let running = started_ticks_of(pid).unwrap();
        // Pinned before its input closes, since it may exit at once after.
        let process = PinnedProcess::open(pid).unwrap();
        assert_eq!(process.started_ticks(), running);
        drop(child.stdin.take());
        assert!(process.wait_for_exit(Duration::from_secs(10)).unwrap());
        // The exit event can precede the zombie; waiting without reaping
        // returns only once it is one.
        // SAFETY: an all-zero `siginfo_t` is a valid output record.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: the child is ours and unreaped; WNOWAIT leaves it so.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(waited, 0, "{}", io::Error::last_os_error());
        assert_eq!(started_ticks_of(pid).unwrap(), running, "the zombie");
        child.wait().unwrap();
        assert_eq!(
            started_ticks_of(pid).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let mut later = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .unwrap();
        assert!(started_ticks_of(later.id()).unwrap() > running);
        later.wait().unwrap();
    }

    #[test]
    fn peer_credentials_name_the_connecting_process() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        assert_eq!(
            peer_credentials(&ours).unwrap(),
            PeerCredentials {
                // SAFETY: geteuid has no preconditions.
                uid: unsafe { libc::geteuid() },
                pid: std::process::id(),
            }
        );
    }

    #[cfg(feature = "host")]
    #[test]
    fn a_handed_socketpair_end_names_the_peer_its_platform_promises() {
        use std::{io::Read, os::unix::process::CommandExt};
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let handed = theirs.as_raw_fd();
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("printf z >&3; exec sleep 30");
        // SAFETY: dup2 is async-signal-safe, and `handed` stays open in the
        // parent until spawn returns.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(handed, 3) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(theirs);
        let mut byte = [0];
        ours.read_exact(&mut byte).unwrap();
        assert_eq!(
            peer_credentials(&ours).unwrap().pid,
            handed_end_peer(std::process::id(), child.id())
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
