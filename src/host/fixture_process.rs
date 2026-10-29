//! Test-only custody for explicitly created fixture owners.
//!
//! A pid/start record is validation metadata. Signals use only descriptors
//! retained after a live owner handshake; this module never discovers owners.
//! Pins do not reap. The original Child owner remains the sole waiter.
use crate::{
    chat::runtime::Handle,
    daemon::holder_registry::Process,
    model::{HostSessionProof, TerminalId},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    mem,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::OpenOptionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const ENDPOINT: &str = "CAGENTS_TEST_FIXTURE_CUSTODY_ENDPOINT";
const CASE: &str = "CAGENTS_TEST_FIXTURE_CUSTODY_CASE";
const PARENT: &str = "CAGENTS_TEST_FIXTURE_CUSTODY_PARENT";
const HOLDER_TERMINAL: &str = "CAGENTS_TEST_FIXTURE_HOLDER_TERMINAL";
const HOLDER_STAGE: &str = "CAGENTS_TEST_FIXTURE_HOLDER_STAGE";
const MAX_PACKET: usize = 1024;
const MAX_PAIRS: usize = 32;
const IO_BOUND: Duration = Duration::from_secs(2);
const CLEANUP_BOUND: Duration = Duration::from_secs(10);

fn refused(message: &str) -> io::Error {
    io::Error::other(message)
}

fn peer(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut value: libc::ucred = unsafe { mem::zeroed() };
    let mut length = mem::size_of_val(&value) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut value as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != mem::size_of_val(&value) {
        return Err(refused("incomplete fixture peer credentials"));
    }
    Ok(value)
}

fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_bytes();
    let mut value: libc::sockaddr_un = unsafe { mem::zeroed() };
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= value.sun_path.len() {
        return Err(refused("fixture custody socket path is outside its bounds"));
    }
    value.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (to, from) in value.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    let length =
        (mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    Ok((value, length))
}

fn packet_socket() -> io::Result<OwnedFd> {
    let descriptor =
        unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn remaining_io(stream: &UnixStream, deadline: Instant) -> io::Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "fixture custody handoff deadline expired",
        ));
    }
    stream.set_read_timeout(Some(remaining))?;
    stream.set_write_timeout(Some(remaining))
}

fn connect(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "fixture custody connection deadline expired",
        ));
    }
    let descriptor = packet_socket()?;
    let (address, length) = address(path)?;
    let stream = UnixStream::from(descriptor);
    stream.set_nonblocking(true)?;
    if unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        // AF_UNIX EAGAIN can mean a full backlog without a pending connect.
        // Refuse it; only EINPROGRESS permits observing this one attempt.
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture custody connection deadline expired",
                ));
            }
            let mut readiness = libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let timeout = remaining
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32;
            let result = unsafe { libc::poll(&mut readiness, 1, timeout) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue; // Readiness only; never repeat connect or reset the deadline.
                }
                return Err(error);
            }
            if result == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture custody connection deadline expired",
                ));
            }
            if readiness.revents & libc::POLLNVAL != 0 {
                return Err(refused(
                    "fixture custody connection descriptor became invalid",
                ));
            }
            break;
        }
    }
    // Writable/error readiness alone does not prove connection success.
    // take_error reads SO_ERROR; peer_addr also refuses an unconnected socket.
    if let Some(error) = stream.take_error()? {
        return Err(error);
    }
    stream.peer_addr()?;
    stream.set_nonblocking(false)?;
    remaining_io(&stream, deadline)?;
    Ok(stream)
}

fn listener(path: &Path) -> io::Result<UnixListener> {
    let descriptor = packet_socket()?;
    let (address, length) = address(path)?;
    if unsafe {
        libc::bind(
            descriptor.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::listen(descriptor.as_raw_fd(), 4) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let listener = UnixListener::from(descriptor);
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn exited(descriptor: &OwnedFd) -> io::Result<bool> {
    loop {
        let mut poll = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
            return Err(refused("fixture pidfd became unavailable"));
        }
        return Ok(poll.revents & libc::POLLIN != 0);
    }
}

fn signal(descriptor: &OwnedFd) -> io::Result<()> {
    if exited(descriptor)? {
        return Ok(());
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            descriptor.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) && exited(descriptor)? {
        return Ok(());
    }
    Err(error)
}

// The returned descriptor is close-only. Callers must complete their live
// attachment authentication before putting it into armed cleanup custody.
fn candidate_after_open(
    process: &Process,
    read: impl FnOnce() -> io::Result<Process>,
) -> io::Result<OwnedFd> {
    if process.pid <= 0
        || process.start == 0
        || process.pid == std::process::id() as i32
        || !process.is_current()
    {
        return Err(refused("fixture-issued process changed before capture"));
    }
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, process.pid, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor as RawFd) };
    if read()? != *process {
        return Err(refused("fixture-issued process changed during capture"));
    }
    Ok(descriptor)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    case: uuid::Uuid,
    owners: [Process; 2],
}

fn send_registration(stream: &UnixStream, bytes: &[u8], descriptors: &[OwnedFd]) -> io::Result<()> {
    if bytes.len() > MAX_PACKET || descriptors.len() != 2 {
        return Err(refused("fixture registration exceeds its bounds"));
    }
    let mut control = [0usize; 16];
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE((2 * mem::size_of::<RawFd>()) as u32) } as usize;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN((2 * mem::size_of::<RawFd>()) as u32) as usize;
        let values = libc::CMSG_DATA(header).cast::<RawFd>();
        for (index, descriptor) in descriptors.iter().enumerate() {
            values.add(index).write(descriptor.as_raw_fd());
        }
    }
    let length = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != bytes.len() {
        return Err(refused("incomplete fixture registration packet"));
    }
    Ok(())
}

fn receive_packet(stream: &UnixStream) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut bytes = [0u8; MAX_PACKET + 1];
    let mut control = [0usize; 16];
    let mut vector = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = mem::size_of_val(&control);
    let length = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    // Acquire every delivered fd before inspecting the payload or refusing it.
    // On any error they remain close-only and RAII closes all of them.
    let mut descriptors = Vec::new();
    let mut ancillary_ok = true;
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            let base = libc::CMSG_LEN(0) as usize;
            let offset = header
                .cast::<u8>()
                .offset_from(message.msg_control.cast::<u8>()) as usize;
            if offset > message.msg_controllen
                || (*header).cmsg_len < base
                || (*header).cmsg_len > message.msg_controllen - offset
            {
                ancillary_ok = false;
                break;
            }
            if (*header).cmsg_level != libc::SOL_SOCKET || (*header).cmsg_type != libc::SCM_RIGHTS {
                ancillary_ok = false;
            } else {
                let count = (*header).cmsg_len - base;
                if count % mem::size_of::<RawFd>() != 0 {
                    ancillary_ok = false;
                }
                let values = libc::CMSG_DATA(header).cast::<RawFd>();
                for index in 0..count / mem::size_of::<RawFd>() {
                    descriptors.push(OwnedFd::from_raw_fd(values.add(index).read()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if !ancillary_ok
        || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0
        || length == 0
        || length as usize > MAX_PACKET
        || descriptors.len() > 2
    {
        return Err(refused("malformed or excessive fixture registration"));
    }
    Ok((bytes[..length as usize].to_vec(), descriptors))
}

fn receive_registration(stream: &UnixStream) -> io::Result<(Registration, Vec<OwnedFd>)> {
    let (bytes, descriptors) = receive_packet(stream)?;
    if descriptors.len() != 2 {
        return Err(refused(
            "fixture actor registration needs exactly two descriptors",
        ));
    }
    let registration = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok((registration, descriptors))
}

fn descriptor_matches(descriptor: &OwnedFd, process: &Process) -> io::Result<()> {
    let mut info = String::new();
    File::open(format!("/proc/self/fdinfo/{}", descriptor.as_raw_fd()))?
        .take(4097)
        .read_to_string(&mut info)?;
    if info.len() > 4096 {
        return Err(refused("fixture pidfd metadata exceeds its bound"));
    }
    let pid = info
        .lines()
        .find_map(|line| line.strip_prefix("Pid:").map(str::trim))
        .and_then(|value| value.parse::<i32>().ok());
    if pid != Some(process.pid) || !process.is_current() {
        return Err(refused(
            "transferred fixture descriptor does not match its admitted owner",
        ));
    }
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::FD_CLOEXEC == 0 {
        return Err(refused("fixture descriptor must not survive exec"));
    }
    Ok(())
}

struct Pins {
    descriptors: Vec<OwnedFd>,
    armed: bool,
}
impl Pins {
    fn settle(&mut self, force: bool) -> io::Result<()> {
        if force {
            // Provider first, then helper. These are only the two explicitly
            // enrolled leaders, never a census-derived descendant or group.
            for descriptor in &self.descriptors {
                signal(descriptor)?;
            }
        }
        let deadline = Instant::now() + CLEANUP_BOUND;
        loop {
            if self
                .descriptors
                .iter()
                .map(exited)
                .collect::<io::Result<Vec<_>>>()?
                .into_iter()
                .all(|value| value)
            {
                self.armed = false;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(refused(
                    "original fixture leader exits remain unconfirmed; outer custody required",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Pins {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self.settle(true)
        {
            eprintln!("fixture leader cleanup remains unconfirmed: {error}");
        }
    }
}

fn register(pins: &Pins, owners: [Process; 2]) -> io::Result<()> {
    let path = std::env::var_os(ENDPOINT)
        .ok_or_else(|| refused("owning fixture parent endpoint is required"))?;
    let case = std::env::var(CASE)
        .map_err(io::Error::other)?
        .parse::<uuid::Uuid>()
        .map_err(io::Error::other)?;
    let parent = std::env::var(PARENT)
        .map_err(io::Error::other)?
        .parse::<i32>()
        .map_err(io::Error::other)?;
    if parent <= 0 || unsafe { libc::getppid() } != parent {
        return Err(refused("fixture original parent changed"));
    }
    let deadline = Instant::now() + IO_BOUND;
    let stream = connect(Path::new(&path), deadline)?;
    let credentials = peer(&stream)?;
    if credentials.pid != parent || credentials.uid != unsafe { libc::getuid() } {
        return Err(refused("fixture endpoint is not its original parent"));
    }
    let bytes = serde_json::to_vec(&Registration { case, owners }).map_err(io::Error::other)?;
    remaining_io(&stream, deadline)?;
    send_registration(&stream, &bytes, &pins.descriptors)?;
    remaining_io(&stream, deadline)?;
    let (ack, unexpected) = receive_packet(&stream)?;
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "fixture custody acknowledgment arrived after the handoff deadline",
        ));
    }
    if !unexpected.is_empty() || ack.as_slice() != case.as_bytes() {
        return Err(refused(
            "fixture custody acknowledgment is missing or different",
        ));
    }
    Ok(())
}

struct StartupClose(Option<Handle>);
impl Drop for StartupClose {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            close_actor(&handle);
        }
    }
}
fn close_actor(handle: &Handle) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.close().and_then(|()| handle.finish_retained_close())
    })) {
        Ok(Ok(())) => true,
        result => {
            eprintln!(
                "fixture semantic actor close did not confirm retirement: {}",
                match result {
                    Ok(Err(error)) => error,
                    Err(_) => "close panicked".into(),
                    Ok(Ok(())) => unreachable!(),
                }
            );
            false
        }
    }
}

/// Created only from this fixture's returned live handle, never disk discovery.
/// Keeps startup Close ownership until authentication, capture and parent ack.
pub(super) struct ActorGuard {
    handle: Handle,
    pins: Pins,
}
impl ActorGuard {
    pub(super) fn created(handle: Handle, proof: HostSessionProof) -> io::Result<Self> {
        let mut startup = StartupClose(Some(handle.clone()));
        let identity = handle
            .actor_identity()
            .ok_or_else(|| refused("fixture did not return a real actor owner"))?;
        let inspect = || -> io::Result<()> {
            let observed = Handle::inspect_remote(&identity.record, proof.clone())
                .map_err(io::Error::other)?;
            if observed.identity != identity || handle.actor_identity().as_ref() != Some(&identity)
            {
                return Err(refused(
                    "fixture live actor attachment changed during enrollment",
                ));
            }
            Ok(())
        };
        inspect()?;
        let owners = [identity.record.root.clone(), identity.record.holder.clone()];
        let mut candidates = Vec::new();
        for process in &owners {
            candidates.push(candidate_after_open(process, || {
                Process::read(process.pid)
            })?);
        }
        // All candidates remain close-only until the original live endpoint is
        // authenticated again. A failed second capture cannot arm its fd.
        inspect()?;
        let guard = Self {
            handle,
            pins: Pins {
                descriptors: candidates,
                armed: true,
            },
        };
        // Transfer semantic Close ownership before a fallible handoff. If ack
        // fails, guard Drop tries that Close before its pinned failure cleanup.
        startup.0 = None;
        register(&guard.pins, owners)?;
        Ok(guard)
    }
}
impl Drop for ActorGuard {
    fn drop(&mut self) {
        let closed = close_actor(&self.handle);
        if let Err(error) = self.pins.settle(!closed) {
            eprintln!("fixture actor cleanup remains unconfirmed: {error}");
        }
    }
}

struct OriginalChild {
    child: Child,
    descriptor: OwnedFd,
    status_path: Option<PathBuf>,
}

fn validate_wait_policy(action: &libc::sigaction) -> io::Result<()> {
    if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(refused(
            "fixture requires default SIGCHLD without SA_NOCLDWAIT; caller policy is preserved",
        ));
    }
    Ok(())
}

fn require_waitable_children() -> io::Result<()> {
    // Observe only. Auto-reaping can release an immediately exited child's PID
    // before pidfd capture, and a custom handler may compete for its wait.
    let mut action: libc::sigaction = unsafe { mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } != 0 {
        return Err(io::Error::last_os_error());
    }
    validate_wait_policy(&action)
}

impl OriginalChild {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        require_waitable_children()?;
        Self::new(command.spawn()?)
    }

    fn new(mut child: Child) -> io::Result<Self> {
        // spawn refused auto-reaping or competing inherited signal policy
        // before creating this Child. No other waiter receives its handle.
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        if descriptor < 0 {
            let error = io::Error::last_os_error();
            // Original, still unreaped Child ownership is retained on capture
            // failure. This is not a PID-record or discovery fallback.
            let killed = child.kill();
            let status = child.wait();
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "fixture pidfd capture failed: {error}; original Child kill={killed:?}; original Child wait={status:?}"
                ),
            ));
        }
        Ok(Self {
            child,
            descriptor: unsafe { OwnedFd::from_raw_fd(descriptor as RawFd) },
            status_path: None,
        })
    }
}
impl Drop for OriginalChild {
    fn drop(&mut self) {
        use std::os::unix::process::ExitStatusExt;
        let result = match self.child.try_wait() {
            Ok(Some(status)) => Ok(status),
            Ok(None) => {
                if let Err(error) = signal(&self.descriptor) {
                    eprintln!("original fixture Child signal failed: {error}");
                }
                self.child.wait()
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(status) => {
                if let Some(path) = &self.status_path {
                    let retain = || -> io::Result<()> {
                        let mut file = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(0o600)
                            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                            .open(path)?;
                        serde_json::to_writer(&mut file, &serde_json::json!({
                            "raw_status": status.into_raw(), "code": status.code(),
                            "signal": status.signal(), "core_dumped": status.core_dumped(),
                            "basis": "sole original Child wait/try_wait; never a cleanup replacement exit"
                        })).map_err(io::Error::other)?;
                        file.flush()
                    };
                    if let Err(error) = retain() {
                        eprintln!("original fixture Child status retention failed: {error}");
                    }
                }
            }
            Err(error) => eprintln!("original fixture Child wait ownership is unknown: {error}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum OriginalHolderRole {
    #[serde(rename = "original_terminal_holder")]
    OriginalTerminalHolder,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum HolderStage {
    #[serde(rename = "0")]
    Stage0,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HolderBinding {
    case: uuid::Uuid,
    terminal: TerminalId,
    stage: HolderStage,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HolderRegistration {
    role: OriginalHolderRole,
    binding: HolderBinding,
    owner: Process,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HolderAck {
    role: OriginalHolderRole,
    binding: HolderBinding,
}

fn holder_context() -> io::Result<(PathBuf, i32, HolderBinding)> {
    let path = std::env::var_os(ENDPOINT)
        .ok_or_else(|| refused("original holder parent endpoint is required"))?;
    let case = std::env::var(CASE)
        .map_err(io::Error::other)?
        .parse::<uuid::Uuid>()
        .map_err(io::Error::other)?;
    let terminal = std::env::var(HOLDER_TERMINAL)
        .map_err(io::Error::other)?
        .parse::<TerminalId>()
        .map_err(io::Error::other)?;
    let parent = std::env::var(PARENT)
        .map_err(io::Error::other)?
        .parse::<i32>()
        .map_err(io::Error::other)?;
    if case.is_nil()
        || terminal.as_uuid().is_nil()
        || std::env::var(HOLDER_STAGE).as_deref() != Ok("0")
        || parent <= 0
        || unsafe { libc::getppid() } != parent
    {
        return Err(refused(
            "original holder case/stage0/terminal/parent binding is invalid",
        ));
    }
    Ok((
        PathBuf::from(path),
        parent,
        HolderBinding {
            case,
            terminal,
            stage: HolderStage::Stage0,
        },
    ))
}

/// Fixture metadata only. This terminal ID does not authorize a signal or mint
/// an owner; enrollment additionally requires the original Child and its fd.
pub(crate) fn terminal_holder_terminal_from_environment() -> io::Result<TerminalId> {
    Ok(holder_context()?.2.terminal)
}

// Separate exact-one transport. Actor send/receive still require their original
// exact two descriptors and their unchanged Registration/16-byte ACK grammar.
fn send_holder_registration(
    stream: &UnixStream,
    bytes: &[u8],
    descriptor: &OwnedFd,
) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_PACKET {
        return Err(refused(
            "original holder registration exceeds its packet bound",
        ));
    }
    let mut control = [0usize; 16];
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as usize;
        libc::CMSG_DATA(header)
            .cast::<RawFd>()
            .write(descriptor.as_raw_fd());
    }
    let length = unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != bytes.len() {
        return Err(refused("incomplete original holder registration packet"));
    }
    Ok(())
}

fn receive_holder_registration(stream: &UnixStream) -> io::Result<(HolderRegistration, OwnedFd)> {
    let (bytes, descriptors) = receive_packet(stream)?;
    let [descriptor]: [OwnedFd; 1] = descriptors.try_into().map_err(|_| {
        refused("original terminal holder registration needs exactly one descriptor")
    })?;
    // The candidate remains close-only through role, stage and binding checks.
    let registration = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok((registration, descriptor))
}

fn original_holder_candidate(
    child: &mut Child,
    original_pidfd: BorrowedFd<'_>,
) -> io::Result<(OwnedFd, Process)> {
    require_waitable_children()?;
    let descriptor = original_pidfd.try_clone_to_owned()?;
    let owner = Process::read(child.id() as i32)?;
    descriptor_matches(&descriptor, &owner)?;
    if fs::read_link(format!("/proc/self/fd/{}", descriptor.as_raw_fd()))?
        != Path::new("anon_inode:[pidfd]")
    {
        return Err(refused(
            "original holder requires its already captured pidfd",
        ));
    }
    // This observes the caller's kernel Child relationship without collecting
    // any status. No Process/JSON-only constructor can enter this path.
    let mut information: libc::siginfo_t = unsafe { mem::zeroed() };
    loop {
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                descriptor.as_raw_fd() as libc::id_t,
                &mut information,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } == 0
        {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    if unsafe { information.si_pid() } != 0 || exited(&descriptor)? {
        return Err(refused(
            "original holder exited before its spawn-time enrollment",
        ));
    }
    descriptor_matches(&descriptor, &owner)?;
    Ok((descriptor, owner))
}

fn holder_acknowledgment(
    stream: &UnixStream,
    binding: HolderBinding,
    deadline: Instant,
) -> io::Result<()> {
    remaining_io(stream, deadline)?;
    let (bytes, unexpected) = receive_packet(stream)?;
    if Instant::now() >= deadline || !unexpected.is_empty() {
        return Err(refused(
            "original holder acknowledgment is late or carries descriptors",
        ));
    }
    let ack: HolderAck = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if ack.role != OriginalHolderRole::OriginalTerminalHolder || ack.binding != binding {
        return Err(refused(
            "original holder acknowledgment has a foreign case/stage0/terminal",
        ));
    }
    Ok(())
}

/// Called at the original holder's spawn owner before Request::Spawn and before
/// the original Child can be reaped or transferred. Borrow both that SAME Child
/// and its already captured pidfd. Never reconstruct either from discovery.
/// Failure leaves the caller's original Child/fd and startup cleanup intact.
/// Success only acknowledges parent-held signal custody, never a wait transfer.
pub(crate) fn register_original_terminal_holder(
    child: &mut Child,
    original_pidfd: BorrowedFd<'_>,
    terminal: TerminalId,
    deadline: Instant,
) -> io::Result<()> {
    let (path, parent, binding) = holder_context()?;
    if binding.terminal != terminal {
        return Err(refused(
            "original holder terminal differs from its issued stage0 binding",
        ));
    }
    let (candidate, owner) = original_holder_candidate(child, original_pidfd)?;
    let deadline = deadline.min(Instant::now() + IO_BOUND);
    let stream = connect(&path, deadline)?;
    let credentials = peer(&stream)?;
    if credentials.pid != parent || credentials.uid != unsafe { libc::getuid() } {
        return Err(refused(
            "original holder endpoint is not its original parent",
        ));
    }
    let bytes = serde_json::to_vec(&HolderRegistration {
        role: OriginalHolderRole::OriginalTerminalHolder,
        binding,
        owner,
    })
    .map_err(io::Error::other)?;
    remaining_io(&stream, deadline)?;
    send_holder_registration(&stream, &bytes, &candidate)?;
    holder_acknowledgment(&stream, binding, deadline)
}

struct OriginalHolderPin {
    descriptor: OwnedFd,
    owner: Process,
    binding: HolderBinding,
    armed: bool,
}
impl OriginalHolderPin {
    fn confirm_exit_until(&mut self, deadline: Instant) -> io::Result<()> {
        loop {
            if exited(&self.descriptor)? {
                self.armed = false;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(refused("original terminal holder exit remains unconfirmed"));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for OriginalHolderPin {
    fn drop(&mut self) {
        if self.armed {
            let result = signal(&self.descriptor)
                .and_then(|()| self.confirm_exit_until(Instant::now() + CLEANUP_BOUND));
            if let Err(error) = result {
                eprintln!(
                    "original holder cleanup is unconfirmed; outer custody required: {error}"
                );
            }
        }
    }
}

/// Owns only stage0's original daemon Child and one explicitly issued holder
/// descriptor. Keeps that descriptor across later adopter stages; never recasts
/// an adopter or discovery Record as a newly issued holder. Does not reap it.
pub(crate) struct TerminalHolderParent {
    original: Option<OriginalChild>,
    holder: Option<OriginalHolderPin>,
    listener: Option<UnixListener>,
    socket_path: PathBuf,
    binding: HolderBinding,
    acknowledged: bool,
}
impl TerminalHolderParent {
    pub(crate) fn spawn_stage0(
        root: &Path,
        terminal: TerminalId,
        command: &mut Command,
    ) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        if [ENDPOINT, CASE, PARENT, HOLDER_TERMINAL, HOLDER_STAGE]
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return Err(refused(
                "inherited original-holder endpoint metadata cannot be replaced",
            ));
        }
        require_waitable_children()?;
        let metadata = fs::symlink_metadata(root)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::getuid() }
            || metadata.mode() & 0o077 != 0
            || terminal.as_uuid().is_nil()
        {
            return Err(refused(
                "original holder needs an owned private fixture root and nonnil terminal",
            ));
        }
        let case = uuid::Uuid::new_v4();
        let socket_path = root.join(format!("holder-{}.sock", case.simple()));
        let listener = listener(&socket_path)?;
        command
            .env(ENDPOINT, &socket_path)
            .env(CASE, case.to_string())
            .env(PARENT, std::process::id().to_string())
            .env(HOLDER_STAGE, "0")
            .env(HOLDER_TERMINAL, terminal.to_string());
        let original = OriginalChild::spawn(command)?;
        Ok(Self {
            original: Some(original),
            holder: None,
            listener: Some(listener),
            socket_path,
            binding: HolderBinding {
                case,
                terminal,
                stage: HolderStage::Stage0,
            },
            acknowledged: false,
        })
    }

    fn accept_with_before_ack(
        &mut self,
        mut stream: UnixStream,
        deadline: Instant,
        before_ack: impl FnOnce(&OriginalHolderPin) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.holder.is_some() || self.acknowledged {
            return Err(refused(
                "original holder is already enrolled; adopter re-registration is forbidden",
            ));
        }
        let original = self
            .original
            .as_ref()
            .ok_or_else(|| refused("original stage0 Child custody is unavailable"))?;
        let credentials = peer(&stream)?;
        if credentials.pid != original.child.id() as i32
            || credentials.uid != unsafe { libc::getuid() }
        {
            return Err(refused(
                "original holder registration peer is not the owned stage0 Child",
            ));
        }
        let deadline = deadline.min(Instant::now() + IO_BOUND);
        remaining_io(&stream, deadline)?;
        let (registration, descriptor) = receive_holder_registration(&stream)?;
        if registration.binding != self.binding
            || registration.owner.pid == credentials.pid
            || registration.owner.pid == std::process::id() as i32
        {
            return Err(refused(
                "original holder registration has a foreign case/stage0/terminal/owner",
            ));
        }
        descriptor_matches(&descriptor, &registration.owner)?;
        if exited(&descriptor)? {
            return Err(refused("original holder exited before parent enrollment"));
        }
        // Commit original custody BEFORE any ACK callback or socket write.
        self.holder = Some(OriginalHolderPin {
            descriptor,
            owner: registration.owner,
            binding: registration.binding,
            armed: true,
        });
        before_ack(self.holder.as_ref().unwrap())?;
        remaining_io(&stream, deadline)?;
        let bytes = serde_json::to_vec(&HolderAck {
            role: OriginalHolderRole::OriginalTerminalHolder,
            binding: self.binding,
        })
        .map_err(io::Error::other)?;
        stream.write_all(&bytes)?;
        if Instant::now() >= deadline {
            return Err(refused("original holder ACK write exceeded its deadline"));
        }
        self.acknowledged = true;
        Ok(())
    }

    fn await_with_before_ack(
        &mut self,
        deadline: Instant,
        before_ack: impl FnOnce(&OriginalHolderPin) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.holder.is_some() || self.acknowledged {
            return Err(refused("original holder can enroll exactly once in stage0"));
        }
        // Enrollment is one-shot even after a peer/parser/Child-exit refusal.
        // Closing the listener on return prevents a retry after the original
        // daemon Child's PID could have been reaped.
        let listener = self
            .listener
            .take()
            .ok_or_else(|| refused("original holder enrollment cannot be retried"))?;
        loop {
            if Instant::now() >= deadline {
                return Err(refused("original stage0 enrollment deadline expired"));
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    return self.accept_with_before_ack(stream, deadline, before_ack);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            let original = self
                .original
                .as_mut()
                .ok_or_else(|| refused("original stage0 Child custody is unavailable"))?;
            if original.child.try_wait()?.is_some() {
                return Err(refused(
                    "original stage0 Child exited before holder enrollment",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub(crate) fn await_registration_until(&mut self, deadline: Instant) -> io::Result<()> {
        self.await_with_before_ack(deadline, |_| Ok(()))
    }

    /// The same original Child, available for deliberate crash/wait only AFTER
    /// holder acknowledgment. No raw PID signal or replacement wait handle.
    pub(crate) fn stage0_child_after_ack(&mut self) -> io::Result<&mut Child> {
        if !self.acknowledged {
            return Err(refused(
                "stage0 crash is forbidden before holder acknowledgment",
            ));
        }
        Ok(&mut self
            .original
            .as_mut()
            .ok_or_else(|| refused("original stage0 Child is unavailable"))?
            .child)
    }

    /// Caller first proves its stage2 semantic Close and unchanged discovery
    /// oracles. This passive observation cannot manufacture those successes.
    pub(crate) fn confirm_holder_exit_until(&mut self, deadline: Instant) -> io::Result<()> {
        if !self.acknowledged {
            return Err(refused(
                "unacknowledged holder cannot satisfy the normal close oracle",
            ));
        }
        self.holder
            .as_mut()
            .ok_or_else(|| refused("original holder pin is unavailable"))?
            .confirm_exit_until(deadline)
    }
}
impl Drop for TerminalHolderParent {
    fn drop(&mut self) {
        // First settle this exact daemon Child, then fallback only to the
        // stored original holder descriptor. The holder is not our Child.
        drop(self.original.take());
        drop(self.holder.take());
        let _ = fs::remove_file(&self.socket_path);
    }
}

struct Parent {
    owners: Vec<Pins>,
    identities: Vec<[Process; 2]>,
    socket_path: PathBuf,
    original: OriginalChild,
}
impl Drop for Parent {
    fn drop(&mut self) {
        for pins in &mut self.owners {
            if let Err(error) = pins.settle(true) {
                eprintln!("parent fixture leader cleanup is unconfirmed: {error}");
            }
        }
        // OriginalChild drops last and owns its wait. No raw wait consumes a
        // runtime/helper Child's status; this is not a family-reaping proof.
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// Runs one exact fixture Child and services original-descriptor registrations
/// while it is alive. Command authority/environment setup stays with the caller.
/// Retains raw output files; no descendant discovery or broad readiness claim.
pub(super) fn run_fixture(
    command: &mut Command,
    root: &Path,
    timeout: Duration,
) -> io::Result<Output> {
    use std::os::unix::fs::MetadataExt;
    if [ENDPOINT, CASE, PARENT]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return Err(refused(
            "inherited fixture custody endpoint metadata cannot be replaced",
        ));
    }
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(refused(
            "fixture custody root must be an owned private directory",
        ));
    }
    let case = uuid::Uuid::new_v4();
    let socket_path = root.join(format!("custody-{}.sock", case.simple()));
    let listener = listener(&socket_path)?;
    let output_file = |name: &str| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join(name))
    };
    command
        .env(ENDPOINT, &socket_path)
        .env(CASE, case.to_string())
        .env(PARENT, std::process::id().to_string())
        .stdout(Stdio::from(output_file("fixture.stdout")?))
        .stderr(Stdio::from(output_file("fixture.stderr")?));
    let mut child = OriginalChild::spawn(command)?;
    child.status_path = Some(root.join("fixture-child-status.json"));
    let mut parent = Parent {
        original: child,
        owners: Vec::new(),
        identities: Vec::new(),
        socket_path,
    };
    let deadline = Instant::now() + timeout;
    let status = loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(IO_BOUND))?;
                stream.set_write_timeout(Some(IO_BOUND))?;
                let credentials = peer(&stream)?;
                if credentials.pid != parent.original.child.id() as i32
                    || credentials.uid != unsafe { libc::getuid() }
                {
                    return Err(refused(
                        "registration peer is not the original unreaped fixture Child",
                    ));
                }
                let (registration, descriptors) = receive_registration(&stream)?;
                if registration.case != case
                    || parent.owners.len() >= MAX_PAIRS
                    || parent.identities.contains(&registration.owners)
                {
                    return Err(refused(
                        "fixture registration case or owner count is invalid",
                    ));
                }
                for (descriptor, process) in descriptors.iter().zip(&registration.owners) {
                    if process.pid == credentials.pid || process.pid == std::process::id() as i32 {
                        return Err(refused(
                            "fixture registration cannot enroll its parent or source Child",
                        ));
                    }
                    descriptor_matches(descriptor, process)?;
                }
                parent.identities.push(registration.owners);
                parent.owners.push(Pins {
                    descriptors,
                    armed: true,
                });
                // The owning parent holds both kernel descriptors BEFORE ack.
                stream.write_all(case.as_bytes())?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if let Some(status) = parent.original.child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err(refused(
                "original fixture Child exceeded its deadline; raw output and sole-owner status are retained",
            ));
        }
        thread::sleep(Duration::from_millis(5));
    };
    for pins in &mut parent.owners {
        pins.settle(true)?;
    }
    let read_output = |name: &str| -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        File::open(root.join(name))?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(refused(
                "fixture output exceeds its bound; original file retained",
            ));
        }
        Ok(bytes)
    };
    Ok(Output {
        status,
        stdout: read_output("fixture.stdout")?,
        stderr: read_output("fixture.stderr")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(script: &str) -> OriginalChild {
        OriginalChild::spawn(
            Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .unwrap()
    }

    #[test]
    fn inherited_auto_reap_or_custom_handler_is_refused_without_resetting_policy() {
        let mut action: libc::sigaction = unsafe { mem::zeroed() };
        action.sa_sigaction = libc::SIG_DFL;
        assert!(validate_wait_policy(&action).is_ok());
        action.sa_sigaction = libc::SIG_IGN;
        assert!(validate_wait_policy(&action).is_err());
        assert_eq!(action.sa_sigaction, libc::SIG_IGN);
        action.sa_sigaction = libc::SIG_DFL;
        action.sa_flags = libc::SA_NOCLDWAIT;
        assert!(validate_wait_policy(&action).is_err());
        assert_eq!(action.sa_flags, libc::SA_NOCLDWAIT);
        action.sa_flags = 0;
        action.sa_sigaction = 123;
        assert!(validate_wait_policy(&action).is_err());
        assert_eq!(action.sa_sigaction, 123);
    }

    #[test]
    fn expired_connection_deadline_refuses_before_contacting_an_endpoint() {
        let error = connect(Path::new("/unused-fixture-endpoint"), Instant::now()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn rejected_candidate_stays_close_only_and_original_child_keeps_its_wait() {
        let mut owner = child("exec /bin/sleep 30");
        let identity = Process::read(owner.child.id() as i32).unwrap();
        let mut changed = identity.clone();
        changed.start = changed.start.checked_add(1).unwrap();
        let error = candidate_after_open(&identity, || Ok(changed)).unwrap_err();
        assert!(error.to_string().contains("during capture"));
        assert!(
            owner.child.try_wait().unwrap().is_none(),
            "a rejected candidate cannot signal its target"
        );
        signal(&owner.descriptor).unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(owner.child.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn pidfd_exit_observation_does_not_consume_the_original_child_status() {
        let mut owner = child("exit 7");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exited(&owner.descriptor).unwrap() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        signal(&owner.descriptor).unwrap();
        assert_eq!(owner.child.wait().unwrap().code(), Some(7));
    }

    #[test]
    fn malformed_registration_descriptors_close_without_signaling() {
        let mut owner = child("exec /bin/sleep 30");
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("custody.sock");
        let listener = listener(&path).unwrap();
        let sender = connect(&path, Instant::now() + IO_BOUND).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        let descriptors = vec![
            owner.descriptor.try_clone().unwrap(),
            owner.descriptor.try_clone().unwrap(),
        ];
        send_registration(&sender, b"not JSON", &descriptors).unwrap();
        assert!(receive_registration(&receiver).is_err());
        assert!(
            owner.child.try_wait().unwrap().is_none(),
            "rejected transferred fds are close-only"
        );
    }

    fn holder_packet_pair() -> (UnixStream, UnixStream) {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    descriptors.as_mut_ptr(),
                )
            },
            0
        );
        let pair = unsafe {
            (
                UnixStream::from_raw_fd(descriptors[0]),
                UnixStream::from_raw_fd(descriptors[1]),
            )
        };
        pair.0.set_read_timeout(Some(IO_BOUND)).unwrap();
        pair.1.set_read_timeout(Some(IO_BOUND)).unwrap();
        pair
    }

    // Deliberately permits malformed/excessive packets only in these rejection
    // tests. Neither production-shaped sender uses this function.
    fn send_holder_test_packet(stream: &UnixStream, bytes: &[u8], descriptors: &[OwnedFd]) {
        assert!(descriptors.len() <= 40 && bytes.len() <= MAX_PACKET + 32);
        let descriptor_bytes = descriptors.len() * mem::size_of::<RawFd>();
        let control_size = unsafe { libc::CMSG_SPACE(descriptor_bytes as u32) } as usize;
        let mut control = vec![0usize; control_size.div_ceil(mem::size_of::<usize>())];
        let mut vector = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        if !descriptors.is_empty() {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = control_size;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(descriptor_bytes as u32) as usize;
                let values = libc::CMSG_DATA(header).cast::<RawFd>();
                for (index, descriptor) in descriptors.iter().enumerate() {
                    values.add(index).write(descriptor.as_raw_fd());
                }
            }
        }
        assert_eq!(
            unsafe { libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL) },
            bytes.len() as isize
        );
    }

    fn close_only_witnesses(count: usize) -> (Vec<OwnedFd>, Vec<UnixStream>) {
        let mut descriptors = Vec::new();
        let mut witnesses = Vec::new();
        for _ in 0..count {
            let (delivered, witness) = UnixStream::pair().unwrap();
            witness.set_read_timeout(Some(IO_BOUND)).unwrap();
            descriptors.push(delivered.into());
            witnesses.push(witness);
        }
        (descriptors, witnesses)
    }
    fn assert_all_closed(witnesses: Vec<UnixStream>) {
        for mut witness in witnesses {
            assert_eq!(
                witness.read(&mut [0]).unwrap(),
                0,
                "rejected descriptor must close without enrollment"
            );
        }
    }
    fn holder_test_binding() -> HolderBinding {
        HolderBinding {
            case: uuid::Uuid::new_v4(),
            terminal: TerminalId::new(),
            stage: HolderStage::Stage0,
        }
    }
    fn holder_test_registration(binding: HolderBinding) -> Vec<u8> {
        serde_json::to_vec(&HolderRegistration {
            role: OriginalHolderRole::OriginalTerminalHolder,
            binding,
            owner: Process { pid: 1, start: 1 },
        })
        .unwrap()
    }

    #[test]
    fn single_holder_candidate_rejects_another_child_and_preserves_original_wait_status() {
        let mut original = child("exec /bin/sleep 30");
        let mut other = child("exec /bin/sleep 30");
        assert!(original_holder_candidate(&mut original.child, other.descriptor.as_fd()).is_err());
        assert!(original.child.try_wait().unwrap().is_none());
        assert!(other.child.try_wait().unwrap().is_none());
        let mut completed = child("exit 7");
        let deadline = Instant::now() + IO_BOUND;
        while !exited(&completed.descriptor).unwrap() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            original_holder_candidate(&mut completed.child, completed.descriptor.as_fd()).is_err()
        );
        assert_eq!(
            completed.child.wait().unwrap().code(),
            Some(7),
            "WNOWAIT must leave original wait custody intact"
        );
    }

    #[test]
    fn single_holder_parser_rejects_malformed_extra_and_truncated_descriptors_close_only() {
        let valid = holder_test_registration(holder_test_binding());
        let actor = serde_json::to_vec(&Registration {
            case: uuid::Uuid::new_v4(),
            owners: [Process { pid: 1, start: 1 }, Process { pid: 2, start: 2 }],
        })
        .unwrap();
        let mut wrong_stage: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        wrong_stage["binding"]["stage"] = serde_json::json!("1");
        let cases = [
            (b"not JSON".to_vec(), 1),
            (actor, 1),
            (serde_json::to_vec(&wrong_stage).unwrap(), 1),
            (valid.clone(), 0),
            (valid.clone(), 2),
            (vec![b'x'; MAX_PACKET + 32], 1),
            (valid.clone(), 40),
        ];
        for (bytes, count) in cases {
            let (sender, receiver) = holder_packet_pair();
            let (descriptors, witnesses) = close_only_witnesses(count);
            send_holder_test_packet(&sender, &bytes, &descriptors);
            drop(descriptors);
            assert!(receive_holder_registration(&receiver).is_err());
            assert_all_closed(witnesses);
        }
        // The old exact-two actor parser still rejects the new role/grammar,
        // even when a negative counterexample supplies two distinct fds.
        let (sender, receiver) = holder_packet_pair();
        let (descriptors, witnesses) = close_only_witnesses(2);
        send_holder_test_packet(&sender, &valid, &descriptors);
        drop(descriptors);
        assert!(receive_registration(&receiver).is_err());
        assert_all_closed(witnesses);
    }

    #[test]
    fn single_holder_ack_rejects_foreign_binding_extra_descriptors_and_eof() {
        let binding = holder_test_binding();
        for foreign_terminal in [false, true] {
            let (sender, receiver) = holder_packet_pair();
            let mut changed = binding;
            if foreign_terminal {
                changed.terminal = TerminalId::new();
            } else {
                changed.case = uuid::Uuid::new_v4();
            }
            let bytes = serde_json::to_vec(&HolderAck {
                role: OriginalHolderRole::OriginalTerminalHolder,
                binding: changed,
            })
            .unwrap();
            send_holder_test_packet(&sender, &bytes, &[]);
            assert!(holder_acknowledgment(&receiver, binding, Instant::now() + IO_BOUND).is_err());
        }
        let (sender, receiver) = holder_packet_pair();
        let bytes = serde_json::to_vec(&HolderAck {
            role: OriginalHolderRole::OriginalTerminalHolder,
            binding,
        })
        .unwrap();
        let (descriptors, witnesses) = close_only_witnesses(1);
        send_holder_test_packet(&sender, &bytes, &descriptors);
        drop(descriptors);
        assert!(holder_acknowledgment(&receiver, binding, Instant::now() + IO_BOUND).is_err());
        assert_all_closed(witnesses);
        let (sender, receiver) = holder_packet_pair();
        drop(sender);
        assert!(holder_acknowledgment(&receiver, binding, Instant::now() + IO_BOUND).is_err());
    }

    const HOLDER_TEST_CHILD: &str = "CAGENTS_TEST_ORIGINAL_HOLDER_CHILD";
    const HOLDER_TEST_ROOT: &str = "CAGENTS_TEST_ORIGINAL_HOLDER_ROOT";
    fn holder_test_child_body(expect_ack: bool) {
        let root = PathBuf::from(std::env::var_os(HOLDER_TEST_ROOT).unwrap());
        let terminal = terminal_holder_terminal_from_environment().unwrap();
        let mut original = child("exec /bin/sleep 30");
        let result = register_original_terminal_holder(
            &mut original.child,
            original.descriptor.as_fd(),
            terminal,
            Instant::now() + Duration::from_secs(15),
        );
        if expect_ack {
            result.unwrap();
            assert_eq!(
                fs::read(root.join("parent-stored-before-ack")).unwrap(),
                b"stored"
            );
        } else {
            assert!(
                result.is_err(),
                "the child must not accept an ACK before the parent's failing pre-ACK callback"
            );
            assert!(
                original.child.try_wait().unwrap().is_none(),
                "handoff failure leaves the original Child untouched"
            );
        }
        signal(&original.descriptor).unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(original.child.wait().unwrap().signal(), Some(libc::SIGKILL));
        fs::write(root.join("child-original-wait-completed"), b"waited").unwrap();
    }
    fn holder_child_command(root: &Path, mode: &str) -> Command {
        let selected = thread::current().name().unwrap().to_owned();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                &selected,
                "--nocapture",
                "--test-threads=1",
                "--color=never",
            ])
            .env(HOLDER_TEST_CHILD, mode)
            .env(HOLDER_TEST_ROOT, root)
            .stdin(Stdio::null())
            .stdout(File::create(root.join("holder-child.stdout")).unwrap())
            .stderr(File::create(root.join("holder-child.stderr")).unwrap());
        command
    }
    fn assert_holder_child_success(
        parent: &mut TerminalHolderParent,
        root: &Path,
        deadline: Instant,
    ) {
        loop {
            if let Some(status) = parent.original.as_mut().unwrap().child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{}",
                    fs::read_to_string(root.join("holder-child.stderr")).unwrap()
                );
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            fs::read(root.join("child-original-wait-completed")).unwrap(),
            b"waited"
        );
        assert!(
            fs::read_to_string(root.join("holder-child.stdout"))
                .unwrap()
                .contains("test result: ok. 1 passed; 0 failed; 0 ignored;")
        );
    }

    #[test]
    fn single_holder_parent_stores_original_before_ack_and_preserves_child_wait() {
        if std::env::var(HOLDER_TEST_CHILD).as_deref() == Ok("ack") {
            holder_test_child_body(true);
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let terminal = TerminalId::new();
        let mut command = holder_child_command(root.path(), "ack");
        let mut parent =
            TerminalHolderParent::spawn_stage0(root.path(), terminal, &mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        parent
            .await_with_before_ack(deadline, |stored| {
                assert!(stored.armed && !exited(&stored.descriptor).unwrap());
                assert_eq!(stored.binding.terminal, terminal);
                descriptor_matches(&stored.descriptor, &stored.owner).unwrap();
                fs::write(root.path().join("parent-stored-before-ack"), b"stored")
            })
            .unwrap();
        assert!(parent.acknowledged && parent.holder.is_some());
        assert_holder_child_success(&mut parent, root.path(), deadline);
        parent.confirm_holder_exit_until(deadline).unwrap();
        assert!(!parent.holder.as_ref().unwrap().armed);
        assert!(
            parent.await_registration_until(deadline).is_err(),
            "an adopter cannot register the original again"
        );
    }

    #[test]
    fn single_holder_no_ack_keeps_stored_pin_and_original_child_cleanup_owner() {
        if std::env::var(HOLDER_TEST_CHILD).as_deref() == Ok("no-ack") {
            holder_test_child_body(false);
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut command = holder_child_command(root.path(), "no-ack");
        let mut parent =
            TerminalHolderParent::spawn_stage0(root.path(), TerminalId::new(), &mut command)
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let error = parent
            .await_with_before_ack(deadline, |stored| {
                assert!(stored.armed);
                Err(refused(
                    "injected pre-ACK refusal after original pin storage",
                ))
            })
            .unwrap_err();
        assert!(error.to_string().contains("pre-ACK"));
        assert!(!parent.acknowledged && parent.holder.as_ref().unwrap().armed);
        assert!(parent.stage0_child_after_ack().is_err());
        assert!(parent.confirm_holder_exit_until(deadline).is_err());
        assert_holder_child_success(&mut parent, root.path(), deadline);
        assert!(exited(&parent.holder.as_ref().unwrap().descriptor).unwrap());
    }

    #[test]
    fn single_holder_foreign_peer_closes_queued_descriptor_without_enrollment() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exec /bin/sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut parent =
            TerminalHolderParent::spawn_stage0(root.path(), TerminalId::new(), &mut command)
                .unwrap();
        let sender = connect(&parent.socket_path, Instant::now() + IO_BOUND).unwrap();
        let (descriptors, witnesses) = close_only_witnesses(1);
        send_holder_registration(
            &sender,
            &holder_test_registration(parent.binding),
            &descriptors[0],
        )
        .unwrap();
        drop(descriptors);
        assert!(
            parent
                .await_registration_until(Instant::now() + IO_BOUND)
                .is_err()
        );
        assert!(parent.holder.is_none() && !parent.acknowledged);
        assert!(parent.listener.is_none());
        assert!(
            parent
                .await_registration_until(Instant::now() + IO_BOUND)
                .is_err(),
            "a refused stage0 peer cannot reopen enrollment"
        );
        assert!(
            parent
                .original
                .as_mut()
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        assert_all_closed(witnesses);
    }

    fn rejected_stage0_body(mode: &str) {
        use std::os::unix::process::ExitStatusExt;
        let root = PathBuf::from(std::env::var_os(HOLDER_TEST_ROOT).unwrap());
        let (path, parent, mut binding) = holder_context().unwrap();
        let mut original = child("exec /bin/sleep 30");
        let mut other = child("exec /bin/sleep 30");
        let (candidate, owner) =
            original_holder_candidate(&mut original.child, original.descriptor.as_fd()).unwrap();
        let mut close_witness = None;
        let descriptor = match mode {
            "wrong-case" => {
                binding.case = uuid::Uuid::new_v4();
                candidate
            }
            "wrong-terminal" => {
                binding.terminal = TerminalId::new();
                candidate
            }
            "wrong-pidfd" => other.descriptor.try_clone().unwrap(),
            "not-pidfd" => {
                let (delivered, witness) = UnixStream::pair().unwrap();
                witness.set_read_timeout(Some(IO_BOUND)).unwrap();
                close_witness = Some(witness);
                OwnedFd::from(delivered)
            }
            _ => panic!("unknown rejection mode"),
        };
        let deadline = Instant::now() + IO_BOUND;
        let stream = connect(&path, deadline).unwrap();
        let credentials = peer(&stream).unwrap();
        assert_eq!(credentials.pid, parent);
        assert_eq!(credentials.uid, unsafe { libc::getuid() });
        let bytes = serde_json::to_vec(&HolderRegistration {
            role: OriginalHolderRole::OriginalTerminalHolder,
            binding,
            owner,
        })
        .unwrap();
        send_holder_registration(&stream, &bytes, &descriptor).unwrap();
        drop(descriptor);
        assert!(holder_acknowledgment(&stream, binding, deadline).is_err());
        if let Some(mut witness) = close_witness {
            assert_eq!(witness.read(&mut [0]).unwrap(), 0);
        }
        assert!(original.child.try_wait().unwrap().is_none());
        assert!(other.child.try_wait().unwrap().is_none());
        for issued in [&mut original, &mut other] {
            signal(&issued.descriptor).unwrap();
            assert_eq!(issued.child.wait().unwrap().signal(), Some(libc::SIGKILL));
        }
        fs::write(root.join("child-original-wait-completed"), b"waited").unwrap();
    }

    #[test]
    fn single_holder_owned_stage0_rejects_wrong_binding_and_descriptor_close_only() {
        let modes = ["wrong-case", "wrong-terminal", "wrong-pidfd", "not-pidfd"];
        match std::env::var(HOLDER_TEST_CHILD) {
            Ok(mode) => {
                assert!(modes.contains(&mode.as_str()));
                rejected_stage0_body(&mode);
                return;
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(error) => panic!("invalid rejection test role: {error}"),
        }
        for mode in modes {
            let root = tempfile::tempdir().unwrap();
            let mut command = holder_child_command(root.path(), mode);
            let mut parent =
                TerminalHolderParent::spawn_stage0(root.path(), TerminalId::new(), &mut command)
                    .unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut reached_storage = false;
            let result = parent.await_with_before_ack(deadline, |_| {
                reached_storage = true;
                // A regression must still let the real source Child unwind
                // and wait its own originals before the parent assertions.
                Err(refused("unexpected storage of rejected registration"))
            });
            assert_holder_child_success(&mut parent, root.path(), deadline);
            let error = result.unwrap_err().to_string();
            if mode.starts_with("wrong-") && mode != "wrong-pidfd" {
                assert!(error.contains("foreign case/stage0/terminal/owner"));
            } else {
                assert!(error.contains("descriptor does not match its admitted owner"));
            }
            assert!(!reached_storage, "{mode} reached the pre-ACK callback");
            assert!(parent.holder.is_none() && !parent.acknowledged);
            assert!(parent.listener.is_none());
            assert!(parent.await_registration_until(deadline).is_err());
        }
    }

    // This test-only waiter is installed from the already stored original pin
    // BEFORE ACK. It never signals, opens a PID, or constructs another Child.
    // Only the private coordinator becomes the orphan's kernel parent; after
    // the original source Child is killed/waited, it collects this exact pidfd.
    struct AdoptedHolderWait {
        descriptor: OwnedFd,
        owner: Process,
        collected: bool,
    }
    impl AdoptedHolderWait {
        fn collect_until(&mut self, deadline: Instant) -> io::Result<()> {
            Self::collect_descriptor(&self.descriptor, &self.owner, &mut self.collected, deadline)
        }

        fn collect_descriptor(
            descriptor: &OwnedFd,
            owner: &Process,
            collected: &mut bool,
            deadline: Instant,
        ) -> io::Result<()> {
            if *collected {
                return Ok(());
            }
            loop {
                let mut information: libc::siginfo_t = unsafe { mem::zeroed() };
                if unsafe {
                    libc::waitid(
                        libc::P_PIDFD,
                        descriptor.as_raw_fd() as libc::id_t,
                        &mut information,
                        libc::WEXITED | libc::WNOHANG,
                    )
                } != 0
                {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                } else if unsafe { information.si_pid() } != 0 {
                    // The sole exact wait has consumed its status even if a
                    // subsequent oracle fails. Never retry a consumed status.
                    *collected = true;
                    if unsafe { information.si_pid() } != owner.pid
                        || information.si_code != libc::CLD_KILLED
                        || unsafe { information.si_status() } != libc::SIGKILL
                    {
                        return Err(refused(
                            "adopted original holder has unexpected exit status",
                        ));
                    }
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(refused(
                        "adopted original holder exact wait exceeded deadline",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    struct HolderCrashCustody {
        parent: Option<TerminalHolderParent>,
        wait: Option<AdoptedHolderWait>,
    }
    impl Drop for HolderCrashCustody {
        fn drop(&mut self) {
            if self.wait.is_none() {
                if let Some(parent) = &mut self.parent {
                    if parent.holder.is_some() {
                        // If the test's extra fd clone failed before ACK, no
                        // independent waiter exists. Keep the admitted fd open
                        // through exact cleanup/wait instead of leaking status.
                        // The successful oracle below uses the real unmodified
                        // TerminalHolderParent Drop with its extra wait fd.
                        drop(parent.original.take());
                        let holder = parent.holder.as_mut().unwrap();
                        let result = signal(&holder.descriptor).and_then(|()| {
                            holder.confirm_exit_until(Instant::now() + CLEANUP_BOUND)
                        });
                        if let Err(error) = result {
                            eprintln!("private coordinator holder fallback unconfirmed: {error}");
                        }
                        let mut collected = false;
                        if let Err(error) = AdoptedHolderWait::collect_descriptor(
                            &holder.descriptor,
                            &holder.owner,
                            &mut collected,
                            Instant::now() + CLEANUP_BOUND,
                        ) {
                            eprintln!(
                                "private coordinator fallback exact wait unconfirmed: {error}"
                            );
                        }
                    }
                }
            }
            // Drop the actual startup owner first, then its original holder's
            // failure pin; only afterwards may our separate kernel wait run.
            drop(self.parent.take());
            if let Some(wait) = &mut self.wait {
                if let Err(error) = wait.collect_until(Instant::now() + CLEANUP_BOUND) {
                    eprintln!("private coordinator original holder wait unconfirmed: {error}");
                }
            }
        }
    }

    const HOLDER_TEST_LIFETIME_READER: &str = "CAGENTS_TEST_ORIGINAL_HOLDER_LIFETIME_READER";
    const HOLDER_TEST_CANCEL_READER: &str = "CAGENTS_TEST_ORIGINAL_HOLDER_CANCEL_READER";

    fn coordinator_lifetime_pipe() -> (OwnedFd, OwnedFd) {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        }
    }

    fn inherit_lifetime_reader(command: &mut Command, reader: &OwnedFd, environment: &'static str) {
        use std::os::unix::process::CommandExt;
        assert!(std::env::var_os(environment).is_none());
        let descriptor = reader.as_raw_fd();
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        command.env(environment, descriptor.to_string());
        // Only the read end survives this exec. The sole owning parent writer
        // retains pipe2's CLOEXEC, so this child cannot keep that writer alive.
        // The owning reader remains alive until Command::spawn has returned.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    struct CoordinatorLifetime(OwnedFd);
    impl CoordinatorLifetime {
        fn inherited(environment: &'static str) -> io::Result<Self> {
            let descriptor = std::env::var(environment)
                .map_err(io::Error::other)?
                .parse::<RawFd>()
                .map_err(io::Error::other)?;
            let descriptor_flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            if descriptor < 3 || descriptor_flags < 0 {
                return Err(refused("missing inherited coordinator lifetime reader"));
            }
            // This exact selfexec command issued one owned inherited read fd;
            // its number is metadata, never a process ownership constructor.
            let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
            let mut metadata: libc::stat = unsafe { mem::zeroed() };
            if unsafe { libc::fstat(descriptor.as_raw_fd(), &mut metadata) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
            if metadata.st_mode & libc::S_IFMT != libc::S_IFIFO
                || flags < 0
                || flags & libc::O_ACCMODE != libc::O_RDONLY
                || flags & libc::O_NONBLOCK == 0
            {
                return Err(refused("invalid inherited coordinator lifetime reader"));
            }
            // Restore close-on-exec BEFORE creating any fixture child. Later
            // execs inherit neither the issuing parent's writer nor this reader.
            if unsafe {
                libc::fcntl(
                    descriptor.as_raw_fd(),
                    libc::F_SETFD,
                    descriptor_flags | libc::FD_CLOEXEC,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }

        fn alive(&self) -> io::Result<bool> {
            let mut byte = 0u8;
            let result =
                unsafe { libc::read(self.0.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
            if result == 0 {
                return Ok(false);
            }
            if result > 0 {
                return Err(refused(
                    "coordinator lifetime pipe unexpectedly carried data",
                ));
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                Ok(true)
            } else {
                // Even repeated interruption cannot create an unbounded retry
                // here: errors unwind through the actual original Child guard.
                Err(error)
            }
        }

        fn wait_until(&self, deadline: Instant) -> io::Result<&'static str> {
            loop {
                if !self.alive()? {
                    return Ok("coordinator-eof");
                }
                if Instant::now() >= deadline {
                    return Ok("stage0-deadline");
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let mut descriptor = libc::pollfd {
                    fd: self.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let result = unsafe {
                    libc::poll(
                        &mut descriptor,
                        1,
                        remaining.as_millis().min(50).max(1) as i32,
                    )
                };
                if result < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                } else if descriptor.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
                    return Err(refused("coordinator lifetime reader became unavailable"));
                }
            }
        }
    }

    // This owner must NEVER force-kill the coordinator: that process owns an
    // admitted holder pin and its exclusive adopted wait after stage0 dies.
    // Timeout/panic closes cancellation, then keeps this actual Child until
    // its original wait completes. No second hard-kill deadline or fake wait
    // handle may discard that custody to make the outer test return sooner.
    struct CooperativeCoordinator {
        child: Child,
        cancellation_writer: Option<OwnedFd>,
        status_path: PathBuf,
    }
    impl CooperativeCoordinator {
        fn spawn(root: &Path, mode: &str) -> Self {
            let (reader, writer) = coordinator_lifetime_pipe();
            let mut command = holder_child_command(root, mode);
            inherit_lifetime_reader(&mut command, &reader, HOLDER_TEST_CANCEL_READER);
            let status_path = root.join("coordinator-original-child-status.json");
            require_waitable_children().unwrap();
            let child = command.spawn().unwrap();
            // Install the real Child owner before any further fallible work.
            let owner = Self {
                child,
                cancellation_writer: Some(writer),
                status_path,
            };
            drop(reader);
            owner
        }
    }
    impl Drop for CooperativeCoordinator {
        fn drop(&mut self) {
            use std::os::unix::process::ExitStatusExt;
            drop(self.cancellation_writer.take());
            // Deliberately retains Child.wait's existing blocking limitation.
            // On an OS wait error, report unknown; never signal or invent a
            // replacement owner/status to turn the test into a cleanup pass.
            let mut retain = || -> io::Result<()> {
                let status = self.child.wait()?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&self.status_path)?;
                serde_json::to_writer(
                    &mut file,
                    &serde_json::json!({
                        "raw_status": status.into_raw(), "code": status.code(),
                        "signal": status.signal(), "core_dumped": status.core_dumped(),
                        "basis": "cooperative EOF cancellation then sole original coordinator Child.wait; no forced signal"
                    }),
                )
                .map_err(io::Error::other)?;
                file.flush()
            };
            if let Err(error) = retain() {
                eprintln!("original cooperative coordinator wait/status unconfirmed: {error}");
            }
        }
    }

    fn acknowledged_stage0_body() {
        let root = PathBuf::from(std::env::var_os(HOLDER_TEST_ROOT).unwrap());
        let lifetime = CoordinatorLifetime::inherited(HOLDER_TEST_LIFETIME_READER).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        if !lifetime.alive().unwrap() {
            return;
        }
        let terminal = terminal_holder_terminal_from_environment().unwrap();
        let mut original = child("exec /bin/sleep 30");
        original.status_path = Some(root.join("lifetime-original-child-status.json"));
        register_original_terminal_holder(
            &mut original.child,
            original.descriptor.as_fd(),
            terminal,
            Instant::now() + IO_BOUND,
        )
        .unwrap();
        fs::write(root.join("stage0-observed-ack"), b"acknowledged").unwrap();
        // Keep the actual startup guard on this stack until the deliberate
        // crash. If the coordinator dies, its sole writer closes and we return
        // through the original kill/wait. An ordinary outer timeout instead
        // cancels and retains that coordinator through its actual cleanup.
        // The finite local deadline also prevents indefinite orphan waiting.
        let reason = lifetime.wait_until(deadline).unwrap();
        drop(original);
        fs::write(root.join("child-original-wait-completed"), b"waited").unwrap();
        fs::write(root.join("stage0-lifetime-ended"), reason).unwrap();
    }

    fn holder_crash_coordinator(root: &Path, cancel_after_stage0_death: bool) {
        use std::os::unix::{fs::DirBuilderExt, process::ExitStatusExt};
        require_waitable_children().unwrap();
        // This body is reached only after reexec of this one exact test. Never
        // change process-wide subreaping beside unrelated libtest parents.
        assert_eq!(
            std::env::var(HOLDER_TEST_CHILD).as_deref(),
            Ok(if cancel_after_stage0_death {
                "cancel-coordinator"
            } else {
                "crash-coordinator"
            })
        );
        let cancellation = CoordinatorLifetime::inherited(HOLDER_TEST_CANCEL_READER).unwrap();
        if !cancellation.alive().unwrap() {
            return;
        }
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        let mut sentinel = child("exec /bin/sleep 30");
        let stage0 = root.join("stage0");
        fs::DirBuilder::new().mode(0o700).create(&stage0).unwrap();
        let (reader, lifetime_writer) = coordinator_lifetime_pipe();
        let mut command = holder_child_command(
            &stage0,
            if cancel_after_stage0_death {
                "cancel-stage0"
            } else {
                "crash-stage0"
            },
        );
        inherit_lifetime_reader(&mut command, &reader, HOLDER_TEST_LIFETIME_READER);
        let parent =
            TerminalHolderParent::spawn_stage0(&stage0, TerminalId::new(), &mut command).unwrap();
        drop(reader);
        let mut custody = HolderCrashCustody {
            parent: Some(parent),
            wait: None,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        let HolderCrashCustody { parent, wait } = &mut custody;
        parent
            .as_mut()
            .unwrap()
            .await_with_before_ack(deadline, |stored| {
                *wait = Some(AdoptedHolderWait {
                    descriptor: stored.descriptor.try_clone()?,
                    owner: stored.owner.clone(),
                    collected: false,
                });
                Ok(())
            })
            .unwrap();
        let parent = custody.parent.as_mut().unwrap();
        while !stage0.join("stage0-observed-ack").exists() {
            assert!(
                parent
                    .stage0_child_after_ack()
                    .unwrap()
                    .try_wait()
                    .unwrap()
                    .is_none()
            );
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let descriptor = parent.holder.as_ref().unwrap().descriptor.as_raw_fd();
        let original_owner = parent.holder.as_ref().unwrap().owner.clone();
        let original_binding = parent.holder.as_ref().unwrap().binding;
        let daemon = parent.stage0_child_after_ack().unwrap();
        daemon.kill().unwrap();
        assert_eq!(daemon.wait().unwrap().signal(), Some(libc::SIGKILL));
        let stored = parent.holder.as_ref().unwrap();
        assert_eq!(stored.descriptor.as_raw_fd(), descriptor);
        assert_eq!(stored.owner, original_owner);
        assert_eq!(stored.binding, original_binding);
        assert!(stored.armed && !exited(&stored.descriptor).unwrap());
        let error = parent
            .confirm_holder_exit_until(Instant::now())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("holder exit remains unconfirmed")
        );
        let stored = parent.holder.as_ref().unwrap();
        assert_eq!(stored.descriptor.as_raw_fd(), descriptor);
        assert!(stored.armed && !exited(&stored.descriptor).unwrap());
        assert!(sentinel.child.try_wait().unwrap().is_none());
        if cancel_after_stage0_death {
            fs::write(
                root.join("post-stage0-death-cut-reached"),
                b"original-waited-live-pin",
            )
            .unwrap();
            assert_eq!(
                cancellation.wait_until(deadline).unwrap(),
                "coordinator-eof",
                "the exact post-stage0-death fault must observe outer cancellation"
            );
            assert!(parent.holder.as_ref().unwrap().armed);
            assert!(!exited(&parent.holder.as_ref().unwrap().descriptor).unwrap());
        }
        // Exercise the actual failure Drop, keeping only the independently
        // owned wait descriptor. No test-side holder signal precedes this.
        drop(custody.parent.take());
        let wait = custody.wait.as_mut().unwrap();
        assert!(exited(&wait.descriptor).unwrap());
        wait.collect_until(deadline).unwrap();
        assert!(wait.collected);
        assert!(sentinel.child.try_wait().unwrap().is_none());
        signal(&sentinel.descriptor).unwrap();
        assert_eq!(sentinel.child.wait().unwrap().signal(), Some(libc::SIGKILL));
        fs::write(
            root.join("post-ack-crash-oracles-completed"),
            b"original-only",
        )
        .unwrap();
        if cancel_after_stage0_death {
            fs::write(
                root.join("post-stage0-death-cancellation-settled"),
                b"cancel-eof-original-holder-waited-sentinel-survived",
            )
            .unwrap();
        }
        drop(lifetime_writer);
    }

    #[test]
    fn single_holder_post_ack_daemon_death_retains_pin_until_failure_drop() {
        match std::env::var(HOLDER_TEST_CHILD).as_deref() {
            Ok("crash-stage0") => acknowledged_stage0_body(),
            Ok("crash-coordinator") => {
                let root = PathBuf::from(std::env::var_os(HOLDER_TEST_ROOT).unwrap());
                holder_crash_coordinator(&root, false);
            }
            Err(std::env::VarError::NotPresent) => {
                assert!(std::env::var_os(HOLDER_TEST_LIFETIME_READER).is_none());
                let root = tempfile::tempdir().unwrap();
                let mut coordinator =
                    CooperativeCoordinator::spawn(root.path(), "crash-coordinator");
                let deadline = Instant::now() + Duration::from_secs(15);
                let status = loop {
                    if let Some(status) = coordinator.child.try_wait().unwrap() {
                        break status;
                    }
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(5));
                };
                assert!(
                    status.success(),
                    "{}",
                    fs::read_to_string(root.path().join("holder-child.stderr")).unwrap()
                );
                assert_eq!(
                    fs::read(root.path().join("post-ack-crash-oracles-completed")).unwrap(),
                    b"original-only"
                );
                assert!(
                    fs::read_to_string(root.path().join("holder-child.stdout"))
                        .unwrap()
                        .contains("test result: ok. 1 passed; 0 failed; 0 ignored;")
                );
            }
            _ => panic!("unknown post-ACK crash test role"),
        }
    }

    #[test]
    fn single_holder_coordinator_eof_returns_through_original_child_cleanup() {
        match std::env::var(HOLDER_TEST_CHILD).as_deref() {
            Ok("eof-stage0") => {
                acknowledged_stage0_body();
                return;
            }
            Err(std::env::VarError::NotPresent) => {}
            _ => panic!("unknown coordinator EOF test role"),
        }
        let root = tempfile::tempdir().unwrap();
        let (reader, lifetime_writer) = coordinator_lifetime_pipe();
        let mut command = holder_child_command(root.path(), "eof-stage0");
        inherit_lifetime_reader(&mut command, &reader, HOLDER_TEST_LIFETIME_READER);
        let mut parent =
            TerminalHolderParent::spawn_stage0(root.path(), TerminalId::new(), &mut command)
                .unwrap();
        drop(reader);
        let deadline = Instant::now() + Duration::from_secs(15);
        parent.await_registration_until(deadline).unwrap();
        while !root.path().join("stage0-observed-ack").exists() {
            assert!(
                parent
                    .stage0_child_after_ack()
                    .unwrap()
                    .try_wait()
                    .unwrap()
                    .is_none()
            );
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        // Closing the ONLY writer models its owning coordinator's death. No
        // test-side holder signal or parent fallback precedes source cleanup.
        drop(lifetime_writer);
        assert_holder_child_success(&mut parent, root.path(), deadline);
        assert_eq!(
            fs::read(root.path().join("stage0-lifetime-ended")).unwrap(),
            b"coordinator-eof",
            "the finite deadline must not masquerade as observed coordinator EOF"
        );
        let status: serde_json::Value = serde_json::from_slice(
            &fs::read(root.path().join("lifetime-original-child-status.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(status["signal"], libc::SIGKILL);
        assert!(status["code"].is_null());
        parent.confirm_holder_exit_until(deadline).unwrap();
        assert!(!parent.holder.as_ref().unwrap().armed);
    }

    #[test]
    fn single_holder_post_stage0_death_outer_cancellation_retains_exact_cleanup_owner() {
        match std::env::var(HOLDER_TEST_CHILD).as_deref() {
            Ok("cancel-stage0") => {
                acknowledged_stage0_body();
                return;
            }
            Ok("cancel-coordinator") => {
                let root = PathBuf::from(std::env::var_os(HOLDER_TEST_ROOT).unwrap());
                holder_crash_coordinator(&root, true);
                return;
            }
            Err(std::env::VarError::NotPresent) => {}
            _ => panic!("unknown post-stage0-death cancellation role"),
        }
        assert!(std::env::var_os(HOLDER_TEST_LIFETIME_READER).is_none());
        let root = tempfile::tempdir().unwrap();
        struct PostStage0OuterTimeout;
        let fault = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> () {
            let mut coordinator = CooperativeCoordinator::spawn(root.path(), "cancel-coordinator");
            let deadline = Instant::now() + Duration::from_secs(15);
            while !root.path().join("post-stage0-death-cut-reached").exists() {
                assert!(coordinator.child.try_wait().unwrap().is_none());
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
            // Actual outer unwinding at the admitted-pin cut must close
            // cancellation and retain the SAME original Child through its
            // cleanup/wait. A pre-cut failure cannot satisfy this typed fault.
            std::panic::panic_any(PostStage0OuterTimeout);
        }));
        let payload = fault.expect_err("injected post-stage0-death outer timeout must unwind");
        assert!(payload.is::<PostStage0OuterTimeout>());
        assert_eq!(
            fs::read(root.path().join("post-stage0-death-cancellation-settled")).unwrap(),
            b"cancel-eof-original-holder-waited-sentinel-survived"
        );
        assert_eq!(
            fs::read(root.path().join("post-ack-crash-oracles-completed")).unwrap(),
            b"original-only"
        );
        let status: serde_json::Value = serde_json::from_slice(
            &fs::read(root.path().join("coordinator-original-child-status.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(status["code"], 0);
        assert!(status["signal"].is_null());
        assert!(
            fs::read_to_string(root.path().join("holder-child.stdout"))
                .unwrap()
                .contains("test result: ok. 1 passed; 0 failed; 0 ignored;")
        );
    }
}
