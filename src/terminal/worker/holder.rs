//! Terminal holders: one small process per terminal that owns its PTY and its
//! root process.
//!
//! The daemon keeps the emulator, but a holder is the root's parent and keeps
//! the PTY master open, which is what lets a terminal's life stop depending on
//! the daemon's. The daemon starts a holder by executing its own image with
//! [`ARGUMENT`]. The holder opens the PTY, spawns the root on it, and hands back
//! the master and a pidfd for the root over its control socket. From then on
//! the daemon reads, writes, resizes and signals through those descriptors
//! directly, so no terminal byte is ever relayed. The holder only reaps the
//! root, reports its wait status, and waits to be released.
//!
//! A holder that loses its daemon without being released releases itself. Its
//! master closes with it, which hangs the terminal up exactly as the daemon's
//! own exit did before holders existed.
//!
//! Control messages are length-prefixed JSON, so a field added later is
//! ignored by a holder that predates it, and descriptors travel as
//! `SCM_RIGHTS` on a message's first byte.

use std::{
    fs::File,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crossbeam_channel::Sender;
use portable_pty::{CommandBuilder, ExitStatus, PtySize, native_pty_system};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{ChildExit, close_processes::signal};

/// The argument that makes an image serve as a holder. Deliberately not a
/// word a test filter or a person would type.
const ARGUMENT: &str = "--cagents-terminal-holder";
const PROTOCOL: u32 = 1;
/// A spawn request carries the root's whole environment; nothing legitimate
/// comes near this.
const MAX_FRAME: u32 = 16 << 20;
/// Descriptors any one message may carry.
const MAX_DESCRIPTORS: usize = 4;
/// How long a released holder has to exit before it is killed. It exits as
/// soon as it reads the release, so this only bounds a wedged one.
const RELEASE_GRACE: Duration = Duration::from_millis(500);
/// How long a new holder has to report its terminal. The root's own exec is
/// the slow part, and a terminal worker waits on this; it only bounds a holder
/// that wedged, so it is generous to a slow filesystem.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a released holder's watch waits to see its release, which a
/// concurrent fork can hold up.
const RELEASE_SETTLE: Duration = Duration::from_millis(100);

/// What process listings show for a holder, within the kernel's 15 bytes.
const PROCESS_NAME: &std::ffi::CStr = c"cagents-holder";

#[derive(Debug, Serialize, Deserialize)]
enum Request {
    Spawn {
        /// A postcard-encoded [`CommandBuilder`], base64 in the JSON. Its
        /// environment is keyed by `OsString`, which a JSON object cannot key
        /// by. Only a daemon sends this, and only to a holder it just started
        /// from its own image, so a binary encoding cannot straddle versions.
        command: String,
        size: PtySize,
    },
    /// The daemon no longer needs the terminal. The holder exits, closing its
    /// master; any process still on the terminal is hung up.
    Release,
}

#[derive(Debug, Serialize, Deserialize)]
enum Report {
    /// Carries the PTY master, then a pidfd for the root.
    Started {
        protocol: u32,
        pid: libc::pid_t,
    },
    Failed(String),
    /// The root's raw wait status, reported once, after the holder reaps it.
    Exited {
        wait_status: libc::c_int,
    },
}

/// A terminal whose PTY and root a holder owns.
pub(super) struct Held {
    pub(super) master: PtyMaster,
    pub(super) child: HeldChild,
    pub(super) holder: Holder,
}

/// Starts a holder that spawns `command` on a new PTY of `size`. The root's
/// exit is delivered to `exits`, once.
pub(super) fn spawn(
    command: CommandBuilder,
    size: PtySize,
    exits: Sender<ChildExit>,
) -> Result<Held, String> {
    // Every image that can start a holder has to be able to serve as one.
    // Referencing the entry here keeps the linker from discarding it.
    std::hint::black_box(&SERVE);

    let (control, remote) =
        UnixStream::pair().map_err(|error| format!("create terminal holder socket: {error}"))?;
    let remote_fd = remote.as_raw_fd();
    let mut process = Command::new("/proc/self/exe");
    process
        .arg0(PROCESS_NAME.to_str().expect("the holder name is ASCII"))
        .arg(ARGUMENT)
        .arg(remote_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe calls run between fork and exec. The
    // holder leads its own session, so nothing aimed at the daemon's process
    // group or terminal reaches it, and it inherits exactly one descriptor:
    // a holder can outlive whatever else the daemon had open.
    unsafe {
        process.pre_exec(move || {
            crate::daemon::seal_inherited_descriptors()?;
            if libc::setsid() == -1 || libc::fcntl(remote_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // The root inherits the holder's limit.
    crate::host::descriptor_limit::inherit(&mut process);
    // `/proc/self/exe` runs this image even after an upgrade has replaced the
    // file it was loaded from, so a holder always matches its daemon.
    let process = process
        .spawn()
        .map_err(|error| format!("start terminal holder: {error}"))?;
    drop(remote);
    // The holder is this daemon's unreaped child until `Holder` reaps it.
    let pidfd = open_pidfd(process.id() as libc::pid_t);
    let mut holder = Holder {
        control,
        process: Some(process),
        pidfd: None,
        watch: None,
        release: None,
    };
    holder.pidfd = Some(pidfd.map_err(|error| format!("watch terminal holder: {error}"))?);

    let command = postcard::to_stdvec(&command)
        .map(|command| STANDARD.encode(command))
        .map_err(|error| format!("encode terminal command: {error}"))?;
    let started = holder
        .control
        .set_read_timeout(Some(START_TIMEOUT))
        .and_then(|()| holder.control.set_write_timeout(Some(START_TIMEOUT)))
        .and_then(|()| send(&holder.control, &Request::Spawn { command, size }, &[]))
        .and_then(|()| receive::<Report>(&holder.control))
        .and_then(|report| {
            holder.control.set_read_timeout(None)?;
            holder.control.set_write_timeout(None)?;
            Ok(report)
        });
    let (report, descriptors) = match started {
        Ok(Some(report)) => report,
        Ok(None) => return Err("terminal holder exited before starting the terminal".into()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(format!(
                "terminal holder did not start the terminal within {}s",
                START_TIMEOUT.as_secs()
            ));
        }
        Err(error) => return Err(format!("start terminal holder: {error}")),
    };
    let pid = match report {
        Report::Started { protocol, pid } if protocol == PROTOCOL => pid,
        Report::Started { protocol, .. } => {
            return Err(format!(
                "terminal holder speaks protocol {protocol}, expected {PROTOCOL}"
            ));
        }
        Report::Failed(message) => return Err(message),
        Report::Exited { .. } => return Err("terminal holder reported an exit first".into()),
    };
    let [master, pidfd]: [OwnedFd; 2] = descriptors
        .try_into()
        .map_err(|_| "terminal holder sent the wrong descriptors")?;

    let root = File::from(pidfd);
    let watched = (|| {
        let reader = holder.control.try_clone()?;
        let root = root.try_clone()?;
        let (released, release) = std::io::pipe()?;
        holder.release = Some(release);
        thread::Builder::new()
            .name(format!("terminal-holder-{pid}"))
            .spawn(move || watch(&reader, &root, &released, &exits))
    })();
    holder.watch = Some(watched.map_err(|error| format!("watch terminal holder: {error}"))?);
    Ok(Held {
        master: PtyMaster(master),
        child: HeldChild { pid, pidfd: root },
        holder,
    })
}

/// Delivers the root's exit, then waits for the holder to go away.
///
/// A holder that vanishes first - killed, or crashed - leaves its root
/// running on a terminal this daemon still reads. That root has not exited,
/// so its exit is reported only once its pidfd says so, with the status the
/// holder took with it unknown. `released` hanging up ends the wait.
fn watch(
    control: &UnixStream,
    root: &File,
    released: &std::io::PipeReader,
    exits: &Sender<ChildExit>,
) {
    use std::os::unix::process::ExitStatusExt;

    loop {
        match receive::<Report>(control) {
            Ok(Some((Report::Exited { wait_status }, _))) => {
                let status = std::process::ExitStatus::from_raw(wait_status);
                let _ = exits.try_send(Ok(ExitStatus::from(status)));
                // Nothing further matters; wait for the holder to go away.
                while let Ok(Some(_)) = receive::<Report>(control) {}
                return;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    // A holder this daemon released ends its connection too.
    if hung_up(released) {
        return;
    }
    tracing::warn!("a terminal holder ended before its root process; the terminal stays attached");
    let mut targets = [
        libc::pollfd {
            fd: root.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: released.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: both pollfds live for the duration of the call.
        match unsafe { libc::poll(targets.as_mut_ptr(), 2, -1) } {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            -1 => return,
            _ if targets[1].revents != 0 => return,
            _ if targets[0].revents != 0 => {
                let _ = exits.try_send(Err(
                    "the terminal's holder ended before it, so its exit status is unknown".into(),
                ));
                return;
            }
            _ => return,
        }
    }
}

/// The daemon's end of a holder: its control socket and its process, which
/// this daemon reaps. Dropping it releases the holder.
pub(super) struct Holder {
    control: UnixStream,
    process: Option<Child>,
    pidfd: Option<File>,
    watch: Option<JoinHandle<()>>,
    /// Dropping this ends a watch still waiting on a root its holder left.
    release: Option<std::io::PipeWriter>,
}

/// Whether the write end of `pipe` closes within [`RELEASE_SETTLE`].
///
/// A release closes it before the holder can exit, but a fork elsewhere in
/// the daemon can briefly hold a copy until that child execs.
fn hung_up(pipe: &std::io::PipeReader) -> bool {
    let mut target = libc::pollfd {
        fd: pipe.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = RELEASE_SETTLE.as_millis() as libc::c_int;
    loop {
        // SAFETY: one live pollfd for the duration of the call.
        match unsafe { libc::poll(&mut target, 1, timeout) } {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            ready => return ready == 1,
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        // First, so the watch takes the holder's exit for the release it is.
        drop(self.release.take());
        let _ = send(&self.control, &Request::Release, &[]);
        if let Some(mut process) = self.process.take() {
            let exited = self
                .pidfd
                .as_ref()
                .is_some_and(|pidfd| exits_within(pidfd, RELEASE_GRACE).unwrap_or(false));
            if !exited {
                tracing::warn!(
                    pid = process.id(),
                    "killing a terminal holder that did not exit when released"
                );
                let _ = process.kill();
            }
            let _ = process.wait();
        }
        // The holder is gone, so its socket has hung up.
        if let Some(watch) = self.watch.take() {
            let _ = watch.join();
        }
    }
}

/// A holder's PTY master, shared with the holder that opened it.
pub(super) struct PtyMaster(OwnedFd);

impl PtyMaster {
    pub(super) fn resize(&self, size: PtySize) -> io::Result<()> {
        let size = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: the descriptor is owned and `size` outlives the call.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::TIOCSWINSZ, &size) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// A further descriptor for the same master, closed on exec.
    pub(super) fn duplicate(&self) -> io::Result<OwnedFd> {
        self.0.try_clone()
    }
}

impl AsRawFd for PtyMaster {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// The root process of a held terminal. Its holder is the parent that reaps
/// it; the daemon signals it through a pidfd, which can never reach a
/// process that reused the pid.
pub(super) struct HeldChild {
    pid: libc::pid_t,
    pidfd: File,
}

impl HeldChild {
    pub(super) fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Whether the root has not been reaped yet. Anything read about its pid
    /// before this answers true was read about the root itself.
    pub(super) fn unreaped(&self) -> bool {
        signal(&self.pidfd, 0).is_ok()
    }

    /// Hangs the root up, as closing its terminal would. A root that already
    /// exited counts as hung up; one that ignores the hangup keeps running.
    pub(super) fn kill(&self) -> io::Result<()> {
        match signal(&self.pidfd, libc::SIGHUP) {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            result => result,
        }
    }
}

fn open_pidfd(pid: libc::pid_t) -> io::Result<File> {
    // SAFETY: pidfd_open takes scalars; a success is a fresh owned descriptor.
    match unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } {
        -1 => Err(io::Error::last_os_error()),
        fd => Ok(unsafe { File::from_raw_fd(fd as RawFd) }),
    }
}

/// Whether a pidfd's process exits within `grace`: a pidfd becomes readable
/// when it does.
fn exits_within(pidfd: &File, grace: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + grace;
    let mut target = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // SAFETY: one live pollfd for the duration of the call.
        match unsafe { libc::poll(&mut target, 1, remaining.as_millis() as libc::c_int) } {
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            -1 => return Err(io::Error::last_os_error()),
            ready => return Ok(ready > 0),
        }
    }
}

/// Serves as a holder when this image was executed as one.
///
/// It runs from `.init_array`, before `main`, so every image that can start a
/// holder can also be one: the daemon, and any test harness that links the
/// host library. None of them needs a hook in its own `main`, and a holder
/// never initializes anything it does not use. Any other invocation costs one
/// read of its own command line.
#[used]
#[unsafe(link_section = ".init_array")]
static SERVE: extern "C" fn() = serve_if_requested;

extern "C" fn serve_if_requested() {
    let Ok(command_line) = std::fs::read("/proc/self/cmdline") else {
        return;
    };
    let mut arguments = command_line.split(|&byte| byte == 0).skip(1);
    if arguments.next() != Some(ARGUMENT.as_bytes()) {
        return;
    }
    let control = arguments
        .next()
        .and_then(|fd| std::str::from_utf8(fd).ok()?.parse::<RawFd>().ok())
        .filter(|&fd| fd > libc::STDERR_FILENO);
    std::process::exit(match control {
        // SAFETY: the daemon passed exactly this descriptor, and nothing else
        // in this process has claimed it this early.
        Some(fd) => serve(unsafe { UnixStream::from_raw_fd(fd) }),
        None => 2,
    });
}

fn serve(control: UnixStream) -> i32 {
    // SAFETY: plain process attribute changes, made before anything else
    // runs. A write to a daemon that went away must fail rather than kill the
    // holder, the holder has no terminal whose hangup could concern it, and
    // the name is a static C string.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        // Reaping the root is this holder's job, which an inherited
        // SIGCHLD disposition of "ignore" would do behind its back.
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
        libc::fcntl(control.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        libc::prctl(libc::PR_SET_NAME, PROCESS_NAME.as_ptr());
    }
    let fail = |message: String| {
        let _ = send(&control, &Report::Failed(message), &[]);
        1
    };
    let (command, size) = match receive::<Request>(&control) {
        Ok(Some((Request::Spawn { command, size }, _))) => (command, size),
        _ => return 1,
    };
    let command = match STANDARD
        .decode(command)
        .map_err(|error| error.to_string())
        .and_then(|command| {
            postcard::from_bytes::<CommandBuilder>(&command).map_err(|error| error.to_string())
        }) {
        Ok(command) => command,
        Err(error) => return fail(format!("decode terminal command: {error}")),
    };
    let program = command
        .get_argv()
        .first()
        .map(|program| program.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pair = match native_pty_system().openpty(size) {
        Ok(pair) => pair,
        Err(error) => return fail(format!("open PTY: {error:#}")),
    };
    let child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(error) => return fail(format!("spawn {program}: {error:#}")),
    };
    drop(pair.slave);
    let Some(pid) = child.process_id().map(|pid| pid as libc::pid_t) else {
        return fail(format!("spawn {program}: no process id"));
    };
    // The root is this holder's unreaped child, so its pid is its own.
    let pidfd = match open_pidfd(pid) {
        Ok(pidfd) => pidfd,
        Err(error) => {
            // SAFETY: as above; the root cannot be handed over, so it must
            // not outlive this failure.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            return fail(format!("open pidfd for {program}: {error}"));
        }
    };
    let Some(master) = pair.master.as_raw_fd() else {
        return fail("PTY descriptor unavailable".into());
    };
    let started = Report::Started {
        protocol: PROTOCOL,
        pid,
    };
    if send(&control, &started, &[master, pidfd.as_raw_fd()]).is_err() {
        return 1;
    }

    let mut reaped = false;
    loop {
        let mut targets = [
            libc::pollfd {
                fd: control.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                // A negative descriptor is skipped once the root is reaped.
                fd: if reaped { -1 } else { pidfd.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both pollfds live for the duration of the call.
        if unsafe { libc::poll(targets.as_mut_ptr(), 2, -1) } == -1 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return 1;
        }
        if targets[1].revents != 0 {
            // A root this holder cannot reap leaves nothing to report: ending
            // the holder lets the daemon report that exit as unknown instead
            // of polling a pidfd that stays ready forever.
            let Some(wait_status) = reap(pid) else {
                break;
            };
            reaped = true;
            let _ = send(&control, &Report::Exited { wait_status }, &[]);
        }
        if targets[0].revents != 0 {
            // A release, a hangup and anything unintelligible all end the
            // holder: only a daemon that can still talk to it may keep it.
            break;
        }
    }
    drop(child);
    // The master closes with the holder, hanging up whatever is left on it.
    drop(pair.master);
    0
}

/// Reaps the root once its pidfd says it exited, returning its wait status.
fn reap(pid: libc::pid_t) -> Option<libc::c_int> {
    let mut wait_status = 0;
    loop {
        // SAFETY: the root is this holder's child, and its pidfd reported
        // its exit.
        match unsafe { libc::waitpid(pid, &mut wait_status, 0) } {
            reaped if reaped == pid => return Some(wait_status),
            -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            _ => return None,
        }
    }
}

fn send(control: &UnixStream, message: &impl Serialize, descriptors: &[RawFd]) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    let length = u32::try_from(body.len())
        .ok()
        .filter(|&length| length <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "message too large"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&body);
    let mut sent = 0;
    while sent < frame.len() {
        // Descriptors ride on the frame's first byte, so only the first call
        // carries them.
        let attached = if sent == 0 { descriptors } else { &[] };
        match send_with_descriptors(control, &frame[sent..], attached) {
            Ok(count) => sent += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reads one message, or `None` when the peer hung up between messages.
fn receive<T: DeserializeOwned>(control: &UnixStream) -> io::Result<Option<(T, Vec<OwnedFd>)>> {
    let mut header = [0u8; 4];
    let mut descriptors = Vec::new();
    let mut filled = 0;
    while filled < header.len() {
        match receive_with_descriptors(control, &mut header[filled..], &mut descriptors) {
            Ok(0) if filled == 0 && descriptors.is_empty() => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let length = u32::from_le_bytes(header);
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut body = vec![0; length as usize];
    (&*control).read_exact(&mut body)?;
    let message = serde_json::from_slice(&body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some((message, descriptors)))
}

fn send_with_descriptors(
    control: &UnixStream,
    bytes: &[u8],
    descriptors: &[RawFd],
) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    // u64 words keep the control buffer aligned for cmsghdr.
    let mut space = [0u64; 8];
    // SAFETY: a zeroed msghdr is valid; every pointer set below outlives
    // sendmsg, and the control message is written within `space`, which
    // CMSG_SPACE of at most MAX_DESCRIPTORS descriptors fits.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        if !descriptors.is_empty() {
            assert!(descriptors.len() <= MAX_DESCRIPTORS);
            let payload = std::mem::size_of_val(descriptors) as libc::c_uint;
            message.msg_control = space.as_mut_ptr().cast();
            message.msg_controllen = libc::CMSG_SPACE(payload) as _;
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(payload) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr(),
                libc::CMSG_DATA(header).cast::<RawFd>(),
                descriptors.len(),
            );
        }
        match libc::sendmsg(control.as_raw_fd(), &message, libc::MSG_NOSIGNAL) {
            -1 => Err(io::Error::last_os_error()),
            sent => Ok(sent as usize),
        }
    }
}

fn receive_with_descriptors(
    control: &UnixStream,
    bytes: &mut [u8],
    descriptors: &mut Vec<OwnedFd>,
) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut space = [0u64; 8];
    // SAFETY: as in `send_with_descriptors`; the kernel writes at most
    // `msg_controllen` bytes of control data into `space`, and each
    // descriptor it reports is new to this process and owned from here on.
    unsafe {
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = space.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&space) as _;
        let received = libc::recvmsg(control.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
        if received == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                let count = ((*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                for index in 0..count {
                    descriptors.push(OwnedFd::from_raw_fd(data.add(index).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control message truncated",
            ));
        }
        Ok(received as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(script: &str) -> CommandBuilder {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        command.cwd("/");
        command
    }

    fn size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    fn read_until(master: &PtyMaster, needle: &str) -> String {
        let mut file = File::from(master.duplicate().unwrap());
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        while !String::from_utf8_lossy(&output).contains(needle) {
            match file.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => output.extend_from_slice(&buffer[..read]),
            }
        }
        String::from_utf8_lossy(&output).into_owned()
    }

    fn holder_pid(held: &Held) -> u32 {
        held.holder.process.as_ref().unwrap().id()
    }

    fn alive(pid: libc::pid_t) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let state = stat
                    .rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .next()?
                    .to_owned();
                Some(state != "Z" && state != "X")
            })
            .unwrap_or(false)
    }

    fn wait_gone(pid: libc::pid_t) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(
                Instant::now() < deadline,
                "process {pid} outlived its terminal"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_message_carries_its_descriptors() {
        let (left, right) = UnixStream::pair().unwrap();
        let (read, write) = std::io::pipe().unwrap();
        send(
            &left,
            &Report::Started {
                protocol: PROTOCOL,
                pid: 7,
            },
            &[write.as_raw_fd()],
        )
        .unwrap();
        send(&left, &Report::Exited { wait_status: 3 }, &[]).unwrap();
        drop(write);

        let (report, descriptors) = receive::<Report>(&right).unwrap().unwrap();
        assert!(matches!(report, Report::Started { pid: 7, .. }));
        let [passed]: [OwnedFd; 1] = descriptors.try_into().unwrap();
        let mut passed = std::io::PipeWriter::from(passed);
        std::io::Write::write_all(&mut passed, b"through").unwrap();
        drop(passed);
        let mut text = String::new();
        (&read).read_to_string(&mut text).unwrap();
        assert_eq!(text, "through");

        let (report, descriptors) = receive::<Report>(&right).unwrap().unwrap();
        assert!(matches!(report, Report::Exited { wait_status: 3 }));
        assert!(descriptors.is_empty());
        drop(left);
        assert!(receive::<Report>(&right).unwrap().is_none());
    }

    #[test]
    fn a_held_root_runs_on_the_passed_pty_and_reports_its_exit() {
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn(command("stty size; read line; exit 7"), size(), exits).unwrap();
        assert!(held.child.unreaped());
        assert!(read_until(&held.master, "24 80").contains("24 80"));

        held.master
            .resize(PtySize {
                rows: 30,
                cols: 100,
                ..size()
            })
            .unwrap();
        let mut writer = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"\n").unwrap();

        let status = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(status.exit_code(), 7);
        assert!(!held.child.unreaped());
    }

    #[test]
    fn the_root_leads_its_own_session_apart_from_its_holder() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let held = spawn(command("read line"), size(), exits).unwrap();
        let session = |pid: u32| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let fields: Vec<_> = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .collect();
            (
                fields[1].parse::<i32>().unwrap(),
                fields[3].parse::<i32>().unwrap(),
            )
        };
        let holder = holder_pid(&held);
        let (holder_parent, holder_session) = session(holder);
        assert_eq!(holder_parent, std::process::id() as i32);
        assert_eq!(holder_session, holder as i32);
        let root = held.child.pid();
        let (root_parent, root_session) = session(root as u32);
        assert_eq!(root_parent, holder as i32);
        assert_eq!(root_session, root);
    }

    #[test]
    fn releasing_a_holder_hangs_up_its_terminal() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let Held {
            master,
            child,
            holder,
        } = spawn(command("while :; do sleep 1; done"), size(), exits).unwrap();
        let root = child.pid();
        let holder_pid = holder.process.as_ref().unwrap().id() as libc::pid_t;
        // The terminal hangs up once its last master closes.
        drop(master);
        drop(holder);
        assert!(!alive(holder_pid));
        wait_gone(root);
    }

    #[test]
    fn a_holder_whose_daemon_vanishes_releases_itself() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let Held {
            master,
            child,
            mut holder,
        } = spawn(command("while :; do sleep 1; done"), size(), exits).unwrap();
        let root = child.pid();
        let mut process = holder.process.take().unwrap();
        // A daemon that dies sends nothing; its descriptors simply close.
        drop(master);
        let _ = holder.control.shutdown(std::net::Shutdown::Both);
        drop(holder);
        let status = process.wait().unwrap();
        assert_eq!(status.code(), Some(0));
        wait_gone(root);
    }

    #[test]
    fn killing_a_held_root_hangs_it_up() {
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn(
            command("trap 'exit 3' HUP; echo ready; while :; do sleep 1; done"),
            size(),
            exits,
        )
        .unwrap();
        read_until(&held.master, "ready");
        held.child.kill().unwrap();
        let status = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(status.exit_code(), 3);
        // A root that is already gone counts as hung up.
        held.child.kill().unwrap();
    }

    #[test]
    fn a_root_outliving_its_holder_reports_only_its_own_exit() {
        let (exits, exited) = crossbeam_channel::bounded(1);
        let held = spawn(command("echo ready; read line; exit 4"), size(), exits).unwrap();
        read_until(&held.master, "ready");
        let holder = holder_pid(&held) as libc::pid_t;
        // SAFETY: signals a child this test spawned.
        assert_eq!(unsafe { libc::kill(holder, libc::SIGKILL) }, 0);
        // The root keeps its terminal, so its exit is not the holder's.
        assert!(exited.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(held.child.unreaped());

        let mut writer = File::from(held.master.duplicate().unwrap());
        std::io::Write::write_all(&mut writer, b"\n").unwrap();
        let error = exited
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("exit status is unknown"), "{error}");
        drop(held);
    }

    #[test]
    fn a_root_that_cannot_start_reports_why() {
        let (exits, _exited) = crossbeam_channel::bounded(1);
        let error = match spawn(
            CommandBuilder::new("/nonexistent/cagents-holder-test"),
            size(),
            exits,
        ) {
            Ok(_) => panic!("a missing program must not start"),
            Err(error) => error,
        };
        assert!(
            error.starts_with("spawn /nonexistent/cagents-holder-test:"),
            "{error}"
        );
    }
}
