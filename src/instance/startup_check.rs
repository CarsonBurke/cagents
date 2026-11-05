//! A positive, read-only proof about one explicitly selected UI owner.
//!
//! Failure never proves that a slot is absent or permits another launch. This
//! capability has no signal, wait, update-fence or inventory operation. App
//! startup qualification remains the server's responsibility.
use std::{io, path::Path, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupDisposition {
    Ready,
    Pending,
    RecoveryIncomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartupCheckToken(uuid::Uuid);

impl StartupCheckToken {
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// The activation parser passes a capped line without its newline here.
    /// Unrelated commands remain available to their existing handlers.
    pub(crate) fn parse_request(line: &str) -> io::Result<Option<Self>> {
        if line == "startup-check-v1" {
            return Err(protocol_error());
        }
        let Some(nonce) = line.strip_prefix("startup-check-v1 ") else {
            return Ok(None);
        };
        let value = uuid::Uuid::parse_str(nonce).map_err(|_| protocol_error())?;
        if value.to_string() != nonce {
            return Err(protocol_error());
        }
        Ok(Some(Self(value)))
    }

    pub(crate) fn request_line(self) -> String {
        format!("startup-check-v1 {}\n", self.0)
    }

    pub(crate) fn response_line(self, disposition: StartupDisposition) -> String {
        let status = match disposition {
            StartupDisposition::Ready => "ready",
            StartupDisposition::Pending => "pending",
            StartupDisposition::RecoveryIncomplete => "recovery-incomplete",
        };
        format!("startup-v1 {} {status}\n", self.0)
    }

    fn parse_response(self, line: &str) -> io::Result<StartupDisposition> {
        let prefix = format!("startup-v1 {} ", self.0);
        match line.strip_prefix(&prefix) {
            Some("ready") => Ok(StartupDisposition::Ready),
            Some("pending") => Ok(StartupDisposition::Pending),
            Some("recovery-incomplete") => Ok(StartupDisposition::RecoveryIncomplete),
            _ => Err(protocol_error()),
        }
    }
}

fn protocol_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid startup-check response or token",
    )
}

#[cfg(target_os = "linux")]
pub(crate) use linux::{ExactStartupTarget, TrustedUiExecutable};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::instance::{self, ExistingWindowDeviceIdentity};
    use serde::Deserialize;
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{
                ffi::OsStrExt,
                fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt},
                net::UnixStream,
            },
        },
        path::PathBuf,
        time::Instant,
    };

    const MAX_CAPABILITY_BYTES: usize = 512;
    const MAX_PROC_BYTES: usize = 16 * 1024;
    const MAX_OWNER_DESCRIPTORS: usize = 4096;

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct FileStamp {
        device: u64,
        inode: u64,
        uid: u32,
        mode: u32,
        links: u64,
        length: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    }

    impl FileStamp {
        fn read(metadata: &fs::Metadata) -> Self {
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                uid: metadata.uid(),
                mode: metadata.mode(),
                links: metadata.nlink(),
                length: metadata.len(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            }
        }
    }

    /// Only the caller's known matched/current/staged UI paths belong here.
    /// Keeping the opened file prevents reuse of its executable inode.
    pub(crate) struct TrustedUiExecutable {
        file: File,
        stamp: FileStamp,
    }

    impl TrustedUiExecutable {
        pub(crate) fn open(path: &Path) -> io::Result<Self> {
            if !path.is_absolute() {
                return Err(unavailable("trusted UI executable path is not absolute"));
            }
            crate::secure_file::reject_symlink_components(path)?;
            let named = fs::symlink_metadata(path)?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(path)?;
            let opened = file.metadata()?;
            if !opened.is_file()
                || (opened.uid() != 0 && opened.uid() != instance::current_uid())
                || opened.mode() & 0o111 == 0
                || opened.mode() & 0o022 != 0
                || FileStamp::read(&named) != FileStamp::read(&opened)
            {
                return Err(unavailable("trusted UI executable identity is unavailable"));
            }
            Ok(Self {
                stamp: FileStamp::read(&opened),
                file,
            })
        }

        fn matches(&self, owner: &ProcessIdentity) -> io::Result<bool> {
            Ok(self.stamp == FileStamp::read(&self.file.metadata()?)
                && owner.executable_device == self.stamp.device
                && owner.executable_inode == self.stamp.inode)
        }
    }

    // Keep this immutable decoder aligned with daemon_admission's published
    // WindowCapability. It grants neither daemon admission nor launch authority.
    #[derive(Debug, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct ProcessIdentity {
        pid: u32,
        started_ticks: u64,
        boot_id: crate::model::BootId,
        executable_device: u64,
        executable_inode: u64,
    }

    impl ProcessIdentity {
        fn read(pid: u32) -> io::Result<Self> {
            let root = Path::new("/proc").join(pid.to_string());
            let stat = read_small(&root.join("stat"), MAX_PROC_BYTES)?;
            let started_ticks = stat
                .rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().nth(19))
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| unavailable("window process start identity is unavailable"))?;
            let executable = fs::metadata(root.join("exe"))?;
            Ok(Self {
                pid,
                started_ticks,
                boot_id: crate::daemon::read_boot_id()?,
                executable_device: executable.dev(),
                executable_inode: executable.ino(),
            })
        }
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WindowCapability {
        format: u8,
        // This read-only endpoint does not perform daemon admission. Parse
        // the published protocol strictly and pin its bytes without imposing
        // the caller's current daemon protocol on a trusted existing window.
        #[serde(rename = "protocol")]
        _daemon_protocol: u16,
        owner: ProcessIdentity,
    }

    pub(crate) struct ExactStartupTarget {
        stream: UnixStream,
        pidfd: OwnedFd,
        owner: ProcessIdentity,
        runtime: PathBuf,
        runtime_inode: (u64, u64),
        socket_path: PathBuf,
        socket_stamp: FileStamp,
        lease: File,
        lease_path: PathBuf,
        lease_stamp: FileStamp,
        capability: Vec<u8>,
        device: Option<String>,
        device_path: Option<PathBuf>,
        device_stamp: Option<FileStamp>,
        device_identity: ExistingWindowDeviceIdentity,
        // Pins the trusted executable beyond the caller's input slice lifetime.
        trusted_executable: File,
        trusted_stamp: FileStamp,
        deadline: Instant,
    }

    impl ExactStartupTarget {
        pub(crate) fn connect(
            slot: &str,
            device: Option<&str>,
            trusted: &[TrustedUiExecutable],
            timeout: Duration,
        ) -> io::Result<Self> {
            let deadline = deadline(timeout)?;
            instance::validate_window_slot(slot)?;
            if trusted.is_empty() {
                return Err(unavailable("no trusted UI executable was supplied"));
            }
            let runtime = instance::runtime_directory()?;
            crate::secure_file::reject_symlink_components(&runtime)?;
            let runtime_metadata = fs::symlink_metadata(&runtime)?;
            let socket_path = runtime.join(instance::slot_socket_name(slot));
            crate::secure_file::reject_symlink_components(&socket_path)?;
            let socket_metadata = fs::symlink_metadata(&socket_path)?;
            require_socket(&socket_metadata)?;
            let socket_stamp = FileStamp::read(&socket_metadata);
            let device_identity =
                instance::existing_window_device_identity(&runtime, slot, device)?
                    .ok_or_else(|| unavailable("window device identity does not match"))?;
            let (device_path, device_stamp) = if let Some(guard) = &device_identity._read_guard {
                let path = runtime.join(format!(
                    "{}.{slot}.device",
                    instance::ACTIVATION_SOCKET_FILE
                ));
                (Some(path), Some(FileStamp::read(&guard.metadata()?)))
            } else {
                (None, None)
            };
            remaining(deadline)?;
            let stream = connect_nonblocking(&socket_path, deadline)?;
            let (pid, pidfd) = connected_peer(&stream)?;
            verify_peer_lifetime(&pidfd, pid)?;
            let lease_path = runtime.join(instance::slot_lock_name(slot));
            crate::secure_file::reject_symlink_components(&lease_path)?;
            let named = fs::symlink_metadata(&lease_path)?;
            require_private_file(&named)?;
            let lease = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(&lease_path)?;
            let opened = lease.metadata()?;
            require_private_file(&opened)?;
            let lease_stamp = FileStamp::read(&opened);
            if lease_stamp != FileStamp::read(&named) {
                return Err(unavailable("window lease changed while opening"));
            }
            let capability = read_capability(&lease, &opened)?;
            let record: WindowCapability = serde_json::from_slice(&capability)
                .map_err(|_| unavailable("window owner capability is invalid"))?;
            let owner = ProcessIdentity::read(pid)?;
            if record.format != 1 || record.owner != owner {
                return Err(unavailable(
                    "window capability does not match the connected owner",
                ));
            }
            let trusted = trusted
                .iter()
                .find_map(|candidate| match candidate.matches(&owner) {
                    Ok(true) => Some(Ok(candidate)),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                })
                .transpose()?
                .ok_or_else(|| unavailable("connected window executable is not trusted"))?;
            let target = Self {
                stream,
                pidfd,
                owner,
                runtime,
                runtime_inode: (runtime_metadata.dev(), runtime_metadata.ino()),
                socket_path,
                socket_stamp,
                lease,
                lease_path,
                lease_stamp,
                capability,
                device: device.map(str::to_owned),
                device_path,
                device_stamp,
                device_identity,
                trusted_executable: trusted.file.try_clone()?,
                trusted_stamp: trusted.stamp,
                deadline,
            };
            target.revalidate(deadline)?;
            Ok(target)
        }

        /// One request on the retained connection; this never rebinds a slot.
        /// The initial connect budget bounds the entire operation as well.
        pub(crate) fn check(mut self, timeout: Duration) -> io::Result<StartupDisposition> {
            let deadline = self.deadline.min(deadline(timeout)?);
            self.revalidate(deadline)?;
            let nonce = StartupCheckToken::new();
            write_bounded(&mut self.stream, nonce.request_line().as_bytes(), deadline)?;
            let response = read_line_bounded(&mut self.stream, deadline)?;
            let disposition = nonce.parse_response(&response)?;
            self.revalidate(deadline)?;
            Ok(disposition)
        }

        /// The caller must retain its sole, unreaped Child under the monitor's
        /// mutex through connect, this comparison and check. The PID compares
        /// two retained capabilities; it never constructs process authority.
        pub(crate) fn require_original_child(
            self,
            child: &std::process::Child,
        ) -> io::Result<Self> {
            self.revalidate(self.deadline)?;
            if child.id() != self.owner.pid {
                return Err(unavailable(
                    "connected window is not the original launched child",
                ));
            }
            verify_peer_lifetime(&self.pidfd, self.owner.pid)?;
            Ok(self)
        }

        fn revalidate(&self, deadline: Instant) -> io::Result<()> {
            remaining(deadline)?;
            verify_peer_lifetime(&self.pidfd, self.owner.pid)?;
            self.revalidate_files()?;
            if ProcessIdentity::read(self.owner.pid)? != self.owner
                || !owns_lease(self.owner.pid, &self.lease.metadata()?, deadline)?
            {
                return Err(unavailable("connected window ownership changed"));
            }
            verify_peer_lifetime(&self.pidfd, self.owner.pid)?;
            if ProcessIdentity::read(self.owner.pid)? != self.owner {
                return Err(unavailable("connected window process identity changed"));
            }
            // A descriptor scan must not allow a named successor to inherit
            // the old peer's proof while the original lease remains open.
            self.revalidate_files()?;
            verify_peer_lifetime(&self.pidfd, self.owner.pid)?;
            remaining(deadline).map(|_| ())
        }

        fn revalidate_files(&self) -> io::Result<()> {
            crate::secure_file::reject_symlink_components(&self.socket_path)?;
            instance::require_owned_runtime_directory(&self.runtime)?;
            let runtime = fs::symlink_metadata(&self.runtime)?;
            let socket = fs::symlink_metadata(&self.socket_path)?;
            require_socket(&socket)?;
            let lease_named = fs::symlink_metadata(&self.lease_path)?;
            let lease_opened = self.lease.metadata()?;
            require_private_file(&lease_named)?;
            require_private_file(&lease_opened)?;
            if (runtime.dev(), runtime.ino()) != self.runtime_inode
                || FileStamp::read(&socket) != self.socket_stamp
                || FileStamp::read(&lease_named) != self.lease_stamp
                || FileStamp::read(&lease_opened) != self.lease_stamp
                || read_capability(&self.lease, &lease_opened)? != self.capability
                || FileStamp::read(&self.trusted_executable.metadata()?) != self.trusted_stamp
            {
                return Err(unavailable("connected window ownership changed"));
            }
            match (
                &self.device_path,
                self.device_stamp,
                &self.device_identity._read_guard,
            ) {
                (None, None, None) if self.device.is_none() => {}
                (Some(path), Some(stamp), Some(guard)) => {
                    let named = fs::symlink_metadata(path)?;
                    let opened = guard.metadata()?;
                    require_private_file(&named)?;
                    require_private_file(&opened)?;
                    let mut bytes = [0; 129];
                    let count = guard.read_at(&mut bytes, 0)?;
                    if FileStamp::read(&named) != stamp
                        || FileStamp::read(&opened) != stamp
                        || count > 128
                        || std::str::from_utf8(&bytes[..count]).ok() != self.device.as_deref()
                    {
                        return Err(unavailable("connected window device identity changed"));
                    }
                }
                _ => {
                    return Err(unavailable(
                        "connected window device identity is unavailable",
                    ));
                }
            }
            Ok(())
        }
    }

    fn connected_peer(stream: &UnixStream) -> io::Result<(u32, OwnedFd)> {
        let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the live socket and correctly sized output are valid.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if size as usize != std::mem::size_of::<libc::ucred>() {
            return Err(unavailable("incomplete window peer credentials"));
        }
        // SAFETY: getsockopt initialized a complete ucred above.
        let credentials = unsafe { credentials.assume_init() };
        if credentials.pid <= 0 || credentials.uid != instance::current_uid() {
            return Err(unavailable("window peer is not visible and owned"));
        }
        let mut descriptor = -1;
        let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: SO_PEERPIDFD returns a new descriptor for this exact socket
        // peer. There is deliberately no pidfd_open(numeric_pid) fallback.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut descriptor as *mut i32).cast(),
                &mut size,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(libc::ENOPROTOOPT) {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "exact window peer pidfd is unavailable",
                )
            } else {
                error
            });
        }
        if descriptor < 0 {
            return Err(unavailable("window peer has no pidfd"));
        }
        // SAFETY: successful getsockopt transferred this owned descriptor.
        let pidfd = unsafe { OwnedFd::from_raw_fd(descriptor) };
        if size as usize != std::mem::size_of::<i32>() {
            return Err(unavailable("incomplete window peer pidfd"));
        }
        // SAFETY: fcntl only sets close-on-exec on the owned pidfd.
        if unsafe { libc::fcntl(pidfd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok((credentials.pid as u32, pidfd))
    }

    fn verify_peer_lifetime(pidfd: &OwnedFd, pid: u32) -> io::Result<()> {
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live initialized descriptor; this never blocks.
        if unsafe { libc::poll(&mut poll, 1, 0) } != 0 || poll.revents != 0 {
            return Err(unavailable("connected window peer is no longer live"));
        }
        // The pidfd's proc-view PID must match SO_PEERCRED before any numeric
        // /proc lookup. A foreign proc mount or invisible peer fails closed.
        let info = read_small(
            &Path::new("/proc/self/fdinfo").join(pidfd.as_raw_fd().to_string()),
            MAX_PROC_BYTES,
        )?;
        let mut values = info.lines().filter_map(|line| line.strip_prefix("Pid:"));
        if values
            .next()
            .and_then(|value| value.trim().parse::<u32>().ok())
            != Some(pid)
            || values.next().is_some()
        {
            return Err(unavailable("window peer proc identity is unavailable"));
        }
        // SAFETY: the same retained descriptor remains live across fdinfo.
        if unsafe { libc::poll(&mut poll, 1, 0) } != 0 || poll.revents != 0 {
            return Err(unavailable(
                "connected window peer exited during verification",
            ));
        }
        Ok(())
    }

    fn owns_lease(pid: u32, expected: &fs::Metadata, deadline: Instant) -> io::Result<bool> {
        let process = Path::new("/proc").join(pid.to_string());
        for (index, entry) in fs::read_dir(process.join("fdinfo"))?.enumerate() {
            remaining(deadline)?;
            if index >= MAX_OWNER_DESCRIPTORS {
                return Err(unavailable("window descriptor proof exceeds its limit"));
            }
            let entry = entry?;
            let descriptor = process.join("fd").join(entry.file_name());
            let matches = |metadata: fs::Metadata| {
                metadata.dev() == expected.dev() && metadata.ino() == expected.ino()
            };
            if fs::metadata(&descriptor).is_ok_and(&matches)
                && read_small(&entry.path(), MAX_PROC_BYTES)
                    .is_ok_and(|info| instance::fdinfo_owns_slot(&info, pid, expected))
                && fs::metadata(&descriptor).is_ok_and(matches)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn read_capability(file: &File, metadata: &fs::Metadata) -> io::Result<Vec<u8>> {
        if metadata.len() == 0 || metadata.len() > MAX_CAPABILITY_BYTES as u64 {
            return Err(unavailable("window owner capability is unavailable"));
        }
        let mut bytes = vec![0; metadata.len() as usize];
        file.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }

    fn read_small(path: &Path, limit: usize) -> io::Result<String> {
        let mut bytes = Vec::new();
        File::open(path)?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(unavailable("window ownership record exceeds its limit"));
        }
        String::from_utf8(bytes).map_err(|_| unavailable("window ownership record is invalid"))
    }

    fn require_private_file(metadata: &fs::Metadata) -> io::Result<()> {
        if !metadata.is_file()
            || metadata.uid() != instance::current_uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(unavailable(
                "window lease or device file is not private and owned",
            ));
        }
        Ok(())
    }

    fn require_socket(metadata: &fs::Metadata) -> io::Result<()> {
        if !metadata.file_type().is_socket()
            || metadata.uid() != instance::current_uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(unavailable("window socket is not private and owned"));
        }
        Ok(())
    }

    fn connect_nonblocking(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
        let bytes = path.as_os_str().as_bytes();
        // SAFETY: sockaddr_un is initialized before use; socket returns an owned fd.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window socket path is too long",
            ));
        }
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
            *destination = *source as libc::c_char;
        }
        // SAFETY: socket has no pointer arguments and requests CLOEXEC/nonblocking.
        let descriptor = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socket transferred this new owned descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let size = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            as libc::socklen_t;
        // SAFETY: the initialized address spans exactly size bytes.
        if unsafe {
            libc::connect(
                descriptor.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                size,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINPROGRESS) {
                // A full Unix backlog can return EAGAIN. Refuse immediately,
                // rather than repeating connect or blocking beyond the budget.
                return Err(error);
            }
            wait_ready(descriptor.as_raw_fd(), libc::POLLOUT, deadline)?;
            let mut error = 0i32;
            let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
            // SAFETY: getsockopt writes one correctly sized integer.
            if unsafe {
                libc::getsockopt(
                    descriptor.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut error as *mut i32).cast(),
                    &mut size,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            if size as usize != std::mem::size_of::<i32>() {
                return Err(unavailable("incomplete window socket error"));
            }
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
        }
        remaining(deadline)?;
        Ok(UnixStream::from(descriptor))
    }

    fn write_bounded(
        stream: &mut UnixStream,
        mut bytes: &[u8],
        deadline: Instant,
    ) -> io::Result<()> {
        while !bytes.is_empty() {
            remaining(deadline)?;
            match stream.write(bytes) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "window startup request was not written",
                    ));
                }
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait_ready(stream.as_raw_fd(), libc::POLLOUT, deadline)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn read_line_bounded(stream: &mut UnixStream, deadline: Instant) -> io::Result<String> {
        let mut bytes = Vec::new();
        let mut buffer = [0; 256];
        loop {
            remaining(deadline)?;
            match stream.read(&mut buffer) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "window startup response ended early",
                    ));
                }
                Ok(count) => {
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.len() > instance::MAX_ACTIVATION_LINE_BYTES {
                        return Err(protocol_error());
                    }
                    if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
                        if newline + 1 != bytes.len() {
                            return Err(protocol_error());
                        }
                        bytes.pop();
                        return String::from_utf8(bytes).map_err(|_| protocol_error());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait_ready(stream.as_raw_fd(), libc::POLLIN, deadline)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn wait_ready(descriptor: i32, events: i16, deadline: Instant) -> io::Result<()> {
        loop {
            let milliseconds = remaining(deadline)?
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let mut poll = libc::pollfd {
                fd: descriptor,
                events,
                revents: 0,
            };
            // SAFETY: the single initialized borrowed descriptor remains live.
            let result = unsafe { libc::poll(&mut poll, 1, milliseconds) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            remaining(deadline)?;
            if result == 0 {
                continue;
            }
            if poll.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
                return Err(unavailable("window startup socket is unavailable"));
            }
            if poll.revents & (events | libc::POLLHUP) != 0 {
                return Ok(());
            }
            return Err(unavailable("unexpected window startup socket state"));
        }
    }

    fn deadline(timeout: Duration) -> io::Result<Instant> {
        Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| unavailable("invalid startup-check deadline"))
    }

    fn remaining(deadline: Instant) -> io::Result<Duration> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|time| !time.is_zero())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "window startup check exceeded its deadline",
                )
            })
    }

    fn unavailable(message: &'static str) -> io::Error {
        io::Error::other(message)
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct TrustedUiExecutable;
#[cfg(not(target_os = "linux"))]
pub(crate) struct ExactStartupTarget;

#[cfg(not(target_os = "linux"))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "exact connected UI owner proof is unavailable on this platform",
    )
}

#[cfg(not(target_os = "linux"))]
impl TrustedUiExecutable {
    pub(crate) fn open(_path: &Path) -> io::Result<Self> {
        Err(unsupported())
    }
}

#[cfg(not(target_os = "linux"))]
impl ExactStartupTarget {
    pub(crate) fn connect(
        _slot: &str,
        _device: Option<&str>,
        _trusted: &[TrustedUiExecutable],
        _timeout: Duration,
    ) -> io::Result<Self> {
        Err(unsupported())
    }
    pub(crate) fn check(self, _timeout: Duration) -> io::Result<StartupDisposition> {
        Err(unsupported())
    }
    pub(crate) fn require_original_child(self, _child: &std::process::Child) -> io::Result<Self> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_reply_requires_the_original_nonce_and_exact_status() {
        let original = StartupCheckToken::new();
        let other = StartupCheckToken::new();
        for status in [
            StartupDisposition::Ready,
            StartupDisposition::Pending,
            StartupDisposition::RecoveryIncomplete,
        ] {
            let response = original.response_line(status);
            assert_eq!(
                original
                    .parse_response(response.trim_end_matches('\n'))
                    .unwrap(),
                status
            );
            assert!(
                other
                    .parse_response(response.trim_end_matches('\n'))
                    .is_err()
            );
        }
        for response in ["ok", "ready", "startup-v1", "startup-v1 wrong ready"] {
            assert!(original.parse_response(response).is_err());
        }
        let response = original.response_line(StartupDisposition::Ready);
        assert!(
            original
                .parse_response(&format!("{} extra", response.trim_end()))
                .is_err()
        );
    }

    #[test]
    fn startup_request_parser_refuses_noncanonical_or_extended_tokens() {
        let token = StartupCheckToken::new();
        let request = token.request_line();
        assert_eq!(
            StartupCheckToken::parse_request(request.trim_end_matches('\n')).unwrap(),
            Some(token)
        );
        assert_eq!(StartupCheckToken::parse_request("activate").unwrap(), None);
        for malformed in [
            "startup-check-v1".to_owned(),
            "startup-check-v1 ".to_owned(),
            format!("{} extra", request.trim_end()),
            "startup-check-v1 ABCDEF12-3456-4789-ABCD-123456789ABC".to_owned(),
        ] {
            assert!(StartupCheckToken::parse_request(&malformed).is_err());
        }
    }
}
