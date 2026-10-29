//! Test-only custody for explicitly created fixture owners.
//!
//! A pid/start record is validation metadata. Signals use only descriptors
//! retained after a live owner handshake; this module never discovers owners.
//! Pins do not reap. The original Child owner remains the sole waiter.
use crate::{chat::runtime::Handle, daemon::holder_registry::Process, model::HostSessionProof};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
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

fn connect(path: &Path) -> io::Result<UnixStream> {
    let descriptor = packet_socket()?;
    let (address, length) = address(path)?;
    if unsafe {
        libc::connect(
            descriptor.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(descriptor);
    stream.set_read_timeout(Some(IO_BOUND))?;
    stream.set_write_timeout(Some(IO_BOUND))?;
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
    let mut stream = connect(Path::new(&path))?;
    let credentials = peer(&stream)?;
    if credentials.pid != parent || credentials.uid != unsafe { libc::getuid() } {
        return Err(refused("fixture endpoint is not its original parent"));
    }
    let bytes = serde_json::to_vec(&Registration { case, owners }).map_err(io::Error::other)?;
    send_registration(&stream, &bytes, &pins.descriptors)?;
    let (ack, unexpected) = receive_packet(&stream)?;
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
impl OriginalChild {
    fn new(mut child: Child) -> io::Result<Self> {
        // No waiter has been given this original Child. Even an immediate exit
        // remains unreaped until this sole owner collects the original status.
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
    let mut child = OriginalChild::new(command.spawn()?)?;
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
        OriginalChild::new(
            Command::new("/bin/sh")
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
        .unwrap()
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
        let sender = connect(&path).unwrap();
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
}
