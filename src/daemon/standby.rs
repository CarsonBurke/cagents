//! Start the exact replacement process before handing off any live ownership.
//! The private channel stays open until that process is ready to serve.

#[cfg(feature = "host")]
use super::lease_handoff::DAEMON_LEASE_ENV;
#[cfg(any(feature = "host", all(target_os = "linux", test)))]
use super::lease_handoff::DAEMON_LEASE_FD;
use super::lease_handoff::LeaseHandoff;
#[cfg(target_os = "linux")]
use crate::upgrade::recovery::RECEIPT_DESCRIPTORS;
#[cfg(all(feature = "host", target_os = "linux"))]
use crate::upgrade::recovery::RECOVERY_FD_ENV;
#[cfg(target_os = "macos")]
use crate::upgrade::recovery::{STANDBY_RECEIPT_DESCRIPTORS, STANDBY_RECEIPT_LIMIT};
use crate::{
    descriptor_frame::{FrameByteOrder, receive_json_with_fds_until, send_json_with_fds_until},
    instance::UiUpdateGuard,
    upgrade::recovery::Receipt,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::{fs::MetadataExt, net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
#[cfg(feature = "host")]
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
};

pub const ARGUMENT: &str = "--await-update-handoff";
const LOCAL_PREVIOUS_CURRENT: &str = "CAGENTS_UPDATE_PREVIOUS_CURRENT";
const FRAME_LIMIT: usize = 4096;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "linux")]
const DESCRIPTOR_LIMIT: usize = 1 + RECEIPT_DESCRIPTORS.len();
#[cfg(target_os = "macos")]
const DESCRIPTOR_LIMIT: usize = 1 + STANDBY_RECEIPT_DESCRIPTORS;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Activation {
    Fresh,
    Lease,
    Checkpoint,
}

#[cfg(any(feature = "host", test))]
impl Activation {
    #[cfg(target_os = "linux")]
    fn slots(&self) -> Vec<i32> {
        if matches!(self, Self::Fresh) {
            return Vec::new();
        }
        let mut slots = vec![DAEMON_LEASE_FD];
        if matches!(self, Self::Checkpoint) {
            slots.extend(RECEIPT_DESCRIPTORS);
        }
        slots
    }

    fn descriptor_count(&self) -> usize {
        #[cfg(target_os = "linux")]
        return self.slots().len();
        #[cfg(target_os = "macos")]
        match self {
            Self::Fresh => 0,
            Self::Lease => 1,
            Self::Checkpoint => DESCRIPTOR_LIMIT,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Status {
    Parked,
    Started,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct AdmissionRejected(String);

#[cfg(any(feature = "host", test))]
fn send_status(mut stream: &UnixStream, status: Status) -> io::Result<()> {
    match status {
        Status::Parked => stream.write_all(&[1]),
        Status::Started => stream.write_all(&[2]),
    }
}

fn send(
    stream: &UnixStream,
    frame: &impl Serialize,
    descriptors: &[BorrowedFd<'_>],
) -> io::Result<()> {
    send_json_with_fds_until(
        stream,
        frame,
        descriptors,
        Instant::now() + TRANSFER_TIMEOUT,
        FRAME_LIMIT,
        DESCRIPTOR_LIMIT,
        FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)
}

fn expect(mut stream: &UnixStream, status: Status) -> io::Result<()> {
    // One byte means a missed deadline consumes no portion of the reply.
    let mut reply = [0];
    stream.read_exact(&mut reply)?;
    let actual = match reply[0] {
        1 => Status::Parked,
        2 => Status::Started,
        3 => {
            let (reason, _): (String, _) = receive_json_with_fds_until(
                stream,
                Instant::now() + TRANSFER_TIMEOUT,
                FRAME_LIMIT,
                0,
                FrameByteOrder::LittleEndian,
            )
            .map_err(io::Error::other)?;
            return Err(io::Error::other(AdmissionRejected(reason)));
        }
        _ => return Err(io::Error::other("invalid replacement daemon startup reply")),
    };
    if actual != status {
        return Err(io::Error::other(
            "unexpected replacement daemon startup state",
        ));
    }
    Ok(())
}

fn wait_for_start(
    stream: &UnixStream,
    timeout: Duration,
    overdue: impl FnOnce(),
) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    match expect(stream, Status::Started) {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            overdue();
            // Keep the original lease and window fence while this exact
            // process settles. A deadline alone grants no rollback right.
            stream.set_read_timeout(None)?;
            expect(stream, Status::Started)
        }
        result => result,
    }
}

#[cfg_attr(
    target_os = "macos",
    allow(dead_code, reason = "Darwin updater wiring is separate")
)]
pub(crate) struct Parked {
    channel: UnixStream,
    child: Option<Child>,
    transferred: bool,
    #[cfg(target_os = "macos")]
    image: Option<ParkedImage>,
    #[cfg(target_os = "macos")]
    _publication: fs::File,
}

#[cfg(target_os = "macos")]
#[derive(Deserialize, Serialize)]
struct ImageAttestation {
    start: u64,
    address: u64,
}

#[cfg(target_os = "macos")]
struct ParkedImage {
    process: crate::pinned_process::PinnedProcess,
    expected: (u64, u64),
    address: u64,
}

#[cfg(target_os = "macos")]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "Darwin updater wiring is separate")
)]
impl ParkedImage {
    fn verify(&self) -> io::Result<()> {
        let path = fs::metadata(self.process.executable()?)?;
        let mapped = crate::process::mapped_region_identity(self.process.pid(), self.address)?;
        if (path.dev(), path.ino()) != self.expected
            || mapped != self.expected
            || self.process.exited()?
        {
            return Err(io::Error::other(
                "the replacement process is not the verified daemon executable",
            ));
        }
        Ok(())
    }
}

#[cfg_attr(
    target_os = "macos",
    allow(dead_code, reason = "Darwin updater wiring is separate")
)]
impl Parked {
    pub(crate) fn launch(
        executable: &Path,
        expected: (u64, u64),
        guard: &UiUpdateGuard,
        previous_current: Option<&Path>,
    ) -> io::Result<Self> {
        let (channel, receiver) = UnixStream::pair()?;
        crate::descriptor::keep_options_after_shutdown(channel.as_fd())?;
        let mut command = Command::new(executable);
        command
            .arg(ARGUMENT)
            .stdin(Stdio::from(OwnedFd::from(receiver)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .env_remove(crate::instance::UI_LAUNCH_HANDSHAKE_ENV)
            .env_remove(crate::instance::UI_LAUNCH_RECOVERY_PROGRESS_ENV);
        crate::install_guard::sanitize_child_environment(&mut command);
        command.env_remove(LOCAL_PREVIOUS_CURRENT);
        if let Some(expected) = previous_current {
            command.env(LOCAL_PREVIOUS_CURRENT, expected);
        }
        // SAFETY: the hook marks unrelated descriptors close-on-exec.
        unsafe { command.pre_exec(super::seal_inherited_descriptors) };
        guard.inherit_for_install(&mut command)?;
        #[cfg(target_os = "macos")]
        let publication = crate::install_guard::reserve_darwin_publication()?;
        #[cfg(target_os = "macos")]
        crate::install_guard::inherit_darwin_publication(&mut command, &publication)?;
        let child = command.spawn()?;
        // Command retains its socket copy for possible reuse. Only the
        // replacement should own the other end after this launch.
        drop(command);
        let pid = child.id();
        let mut parked = Self {
            channel,
            child: Some(child),
            transferred: false,
            #[cfg(target_os = "macos")]
            image: None,
            #[cfg(target_os = "macos")]
            _publication: publication,
        };
        parked.channel.set_read_timeout(Some(TRANSFER_TIMEOUT))?;
        parked.verify_launch(pid, expected)?;
        Ok(parked)
    }

    #[cfg(target_os = "linux")]
    fn verify_launch(&mut self, pid: u32, expected: (u64, u64)) -> io::Result<()> {
        expect(&self.channel, Status::Parked)?;
        let actual = fs::metadata(format!("/proc/{pid}/exe"))?;
        if (actual.dev(), actual.ino()) != expected {
            return Err(io::Error::other(
                "the replacement process is not the verified daemon executable",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn verify_launch(&mut self, pid: u32, expected: (u64, u64)) -> io::Result<()> {
        let process = crate::pinned_process::PinnedProcess::open(pid)?;
        expect(&self.channel, Status::Parked)?;
        let (attestation, _): (ImageAttestation, _) = receive_json_with_fds_until(
            &self.channel,
            Instant::now() + TRANSFER_TIMEOUT,
            FRAME_LIMIT,
            0,
            FrameByteOrder::LittleEndian,
        )
        .map_err(io::Error::other)?;
        let peer = crate::pinned_process::peer_credentials(&self.channel)?;
        if peer.uid != super::current_uid()
            || peer.pid != pid
            || attestation.start != process.started_ticks()
        {
            return Err(io::Error::other(
                "the standby report does not belong to the launched process epoch",
            ));
        }
        let image = ParkedImage {
            process,
            expected,
            address: attestation.address,
        };
        image.verify()?;
        self.image = Some(image);
        Ok(())
    }

    pub(crate) fn check_running(&mut self) -> io::Result<()> {
        if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
            return Err(io::Error::other(format!(
                "replacement daemon exited before activation ({status})"
            )));
        }
        #[cfg(target_os = "macos")]
        self.image
            .as_ref()
            .expect("verified standby image")
            .verify()?;
        Ok(())
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    /// The public lock file can still describe the former owner during
    /// activation. An unreaped exact child is the authority in that gap.
    pub(crate) fn may_own_runtime(&mut self) -> bool {
        self.transferred
            && !self
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .is_ok_and(|status| status.is_some())
    }

    pub(crate) fn activate(
        &mut self,
        executable: &Path,
        lease: Option<&LeaseHandoff>,
        receipt: Option<&Receipt>,
        timeout: Duration,
        overdue: impl FnOnce(),
    ) -> io::Result<()> {
        self.check_running()?;
        #[cfg(target_os = "macos")]
        if lease.is_none() {
            return Err(io::Error::other(
                "Darwin standby requires a runtime lease transfer",
            ));
        }
        let mut descriptors = lease
            .into_iter()
            .map(LeaseHandoff::descriptor)
            .collect::<Vec<_>>();
        #[cfg(target_os = "macos")]
        let prepared = receipt
            .map(|receipt| receipt.standby_proof(executable))
            .transpose()?;
        let request = if let Some(receipt) = receipt {
            if lease.is_none() {
                return Err(io::Error::other(
                    "a checkpoint activation requires the source lease",
                ));
            }
            #[cfg(target_os = "linux")]
            {
                let files = receipt.files()?;
                crate::upgrade::recovery::validate_forward_target(executable, files[0])?;
                descriptors.extend(files.map(AsFd::as_fd));
            }
            #[cfg(target_os = "macos")]
            {
                let _ = receipt;
                let (_, files) = prepared.as_ref().expect("validated Prepared authority");
                descriptors.extend(files.iter().map(|file| file.as_fd()));
            }
            Activation::Checkpoint
        } else if lease.is_some() {
            Activation::Lease
        } else {
            Activation::Fresh
        };
        // From the first transfer byte onward the child might receive
        // authority. Never kill it on loss of communication after this point.
        self.transferred = true;
        self.channel.write_all(&[0])?;
        send(&self.channel, &request, &descriptors)?;
        #[cfg(target_os = "macos")]
        if let Some((authority, _)) = prepared {
            send_json_with_fds_until(
                &self.channel,
                authority,
                &[],
                Instant::now() + TRANSFER_TIMEOUT,
                STANDBY_RECEIPT_LIMIT,
                0,
                FrameByteOrder::LittleEndian,
            )
            .map_err(io::Error::other)?;
        }
        let result = wait_for_start(&self.channel, timeout, overdue);
        if let Err(error) = &result
            && error
                .get_ref()
                .is_some_and(|cause| cause.is::<AdmissionRejected>())
        {
            // The reject is sent before the daemon can open the store or
            // publish ownership. Settle that exact child before rolling back.
            let child = self.child.as_mut().unwrap();
            let _ = child.kill();
            child.wait()?;
        }
        result
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if self.transferred {
                // After transfer the process may be hosting the workspace.
                let _ = std::thread::Builder::new()
                    .name("cagentsd-update-reaper".into())
                    .spawn(move || {
                        let _ = child.wait();
                    });
            } else {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(feature = "host")]
pub struct Activated {
    channel: UnixStream,
    #[cfg(target_os = "linux")]
    publication: Option<crate::update::LocalPublication>,
    #[cfg(target_os = "macos")]
    _legacy_fence: crate::instance::OwnedLock,
    #[cfg(target_os = "macos")]
    _publication: Option<File>,
}

#[cfg(feature = "host")]
impl Activated {
    /// The host accepted custody and can now answer its authenticated socket.
    pub fn started(self) -> io::Result<()> {
        send_status(&self.channel, Status::Started)
    }

    pub fn failed(&self, reason: &impl std::fmt::Display) {
        let mut stream = &self.channel;
        let reason: String = reason.to_string().chars().take(512).collect();
        let _ = stream
            .write_all(&[3])
            .and_then(|()| send(stream, &reason, &[]));
    }

    #[cfg(target_os = "linux")]
    pub fn take_local_publication(&mut self) -> Option<crate::update::LocalPublication> {
        self.publication.take()
    }
}

/// Consume the installed updater's exact account fence at standby entry.
/// The caller must still be single-threaded.
#[cfg(all(target_os = "macos", feature = "host"))]
unsafe fn take_darwin_publication() -> io::Result<Option<File>> {
    use crate::install_guard::{PUBLICATION_FD_ENV, inherited_darwin_publication};
    let Some((file, fd)) = inherited_darwin_publication()? else {
        return Ok(None);
    };
    // SAFETY: the caller is single-threaded and supplied the inherited
    // descriptor. `file` retains the same lock when the original closes.
    unsafe {
        std::env::remove_var(PUBLICATION_FD_ENV);
        libc::close(fd);
    }
    Ok(Some(file))
}

#[cfg(feature = "host")]
/// Wait for activation without opening the store, lease, or public socket.
///
/// # Safety
/// The caller is the single-threaded daemon entry point. It has not read
/// inherited lease/receipt state and will run normal admission afterward.
pub unsafe fn await_activation() -> io::Result<Activated> {
    // The original process still needs its inherited fence. Helpers such as
    // scope enrollment must never inherit a copy across their own exec.
    super::seal_inherited_descriptors()?;
    crate::instance::validate_stage_guard()?;
    #[cfg(target_os = "macos")]
    let legacy_fence = crate::instance::reserve_standby_legacy_fence()?;
    // SAFETY: this is the single-threaded standby entry. The sender's copy
    // remains held until publication, even after the successor sends Started.
    #[cfg(target_os = "macos")]
    let publication = unsafe { take_darwin_publication()? };
    #[cfg(target_os = "linux")]
    let publication = std::env::var_os(LOCAL_PREVIOUS_CURRENT)
        .map(|expected| {
            crate::update::LocalPublication::prepare(Path::new(&expected)).map_err(io::Error::other)
        })
        .transpose()?;
    #[cfg(target_os = "macos")]
    if std::env::var_os(LOCAL_PREVIOUS_CURRENT).is_some() {
        return Err(io::Error::other(
            "the Darwin coordinator owns installation publication",
        ));
    }
    // SAFETY: activation runs before the daemon starts any threads.
    unsafe { std::env::remove_var(LOCAL_PREVIOUS_CURRENT) };
    let stdin = io::stdin();
    let mut control = UnixStream::from(stdin.as_fd().try_clone_to_owned()?);
    control.peer_addr()?;
    #[cfg(target_os = "macos")]
    {
        let coordinator = crate::pinned_process::peer_credentials(&control)?;
        // SAFETY: getppid takes no arguments and does not mutate state.
        let parent = unsafe { libc::getppid() };
        if parent <= 1
            || coordinator.pid != parent as u32
            || coordinator.uid != super::current_uid()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid standby coordinator",
            ));
        }
    }
    let null = File::open("/dev/null")?;
    // SAFETY: stdin is the private channel supplied by the coordinator. Only
    // the retained copy is needed, and no other thread can observe the remap.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // Give the replacement a scope separate from its updater before it
    // accepts custody. This transfers no handoff authority to the manager.
    // The direct process-group launch remains safe without systemd.
    let _ = super::process_scope::enroll_current_process();
    send_status(&control, Status::Parked)?;
    #[cfg(target_os = "macos")]
    send(
        &control,
        &ImageAttestation {
            start: crate::pinned_process::started_ticks_of(std::process::id())?,
            address: crate::process::own_code_address(),
        },
        &[],
    )?;
    // Waiting for the originating worker to close the source can take as
    // long as its reviewed work. EOF abandons the standby if that worker exits.
    let mut begin = [0];
    control.read_exact(&mut begin)?;
    if begin != [0] {
        return Err(io::Error::other("invalid replacement daemon activation"));
    }
    let (request, descriptors) = receive_activation(&control)?;
    #[cfg(target_os = "linux")]
    if matches!(request, Activation::Fresh) && publication.is_none() {
        return Err(io::Error::other(
            "fresh standby activation requires a local update",
        ));
    }
    #[cfg(target_os = "linux")]
    install_descriptors(descriptors, &request.slots())?;
    #[cfg(target_os = "macos")]
    install_darwin_activation(&control, &request, descriptors)?;
    // SAFETY: the inherited authority readers run immediately afterward in
    // this same thread, consuming their normal authenticated descriptors.
    unsafe {
        if !matches!(request, Activation::Fresh) {
            std::env::set_var(DAEMON_LEASE_ENV, DAEMON_LEASE_FD.to_string());
        }
        #[cfg(target_os = "linux")]
        if matches!(request, Activation::Checkpoint) {
            std::env::set_var(RECOVERY_FD_ENV, RECEIPT_DESCRIPTORS[0].to_string());
        }
    }
    Ok(Activated {
        channel: control,
        #[cfg(target_os = "linux")]
        publication,
        #[cfg(target_os = "macos")]
        _legacy_fence: legacy_fence,
        #[cfg(target_os = "macos")]
        _publication: publication,
    })
}

#[cfg(all(feature = "host", target_os = "macos"))]
fn install_darwin_activation(
    control: &UnixStream,
    request: &Activation,
    descriptors: Vec<OwnedFd>,
) -> io::Result<()> {
    use crate::upgrade::recovery::{Authority, accept_standby_authority};

    let mut descriptors = descriptors.into_iter();
    let lease = descriptors
        .next()
        .ok_or_else(|| io::Error::other("Darwin standby requires a runtime lease transfer"))?;
    if *request == Activation::Checkpoint {
        let (authority, _): (Authority, _) = receive_json_with_fds_until(
            control,
            Instant::now() + TRANSFER_TIMEOUT,
            STANDBY_RECEIPT_LIMIT,
            0,
            FrameByteOrder::LittleEndian,
        )
        .map_err(io::Error::other)?;
        let files = descriptors
            .map(File::from)
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| io::Error::other("Prepared transfer has incomplete custody"))?;
        accept_standby_authority(authority, files, File::from(lease.try_clone()?))?;
    }
    install_descriptors(vec![lease], &[DAEMON_LEASE_FD])
}

#[cfg(any(feature = "host", test))]
fn receive_activation(control: &UnixStream) -> io::Result<(Activation, Vec<OwnedFd>)> {
    let (request, descriptors): (Activation, _) = receive_json_with_fds_until(
        control,
        Instant::now() + TRANSFER_TIMEOUT,
        FRAME_LIMIT,
        DESCRIPTOR_LIMIT,
        FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)?;
    if descriptors.len() != request.descriptor_count() {
        return Err(io::Error::other(
            "replacement daemon activation has incomplete custody",
        ));
    }
    Ok((request, descriptors))
}

#[cfg(feature = "host")]
fn install_descriptors(descriptors: Vec<OwnedFd>, slots: &[i32]) -> io::Result<()> {
    // Copy every received right above the reserved range before remapping:
    // the kernel may have assigned one of those numbers to another right.
    let copies = descriptors
        .iter()
        .map(|descriptor| {
            // SAFETY: the source is live, and the lower bound is beyond the reserved slots.
            let fd = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: F_DUPFD_CLOEXEC returned a fresh owned descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        })
        .collect::<io::Result<Vec<_>>>()?;
    // Close the received numbers before reusing any reserved slot.
    drop(descriptors);
    let mut installed = Vec::with_capacity(slots.len());
    for (descriptor, &slot) in copies.iter().zip(slots) {
        // SAFETY: activation is single-threaded and F_GETFD only probes the slot.
        if unsafe { libc::fcntl(slot, libc::F_GETFD) } != -1 {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        installed.push(crate::descriptor::duplicate_to(descriptor.as_fd(), slot)?);
    }
    for descriptor in installed {
        // The startup readers take ownership of every slot after this returns.
        let _ = descriptor.into_raw_fd();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_activation_has_no_existing_daemon_lease() {
        let (worker, daemon) = UnixStream::pair().unwrap();
        send(&worker, &Activation::Fresh, &[]).unwrap();
        let (activation, files) = receive_activation(&daemon).unwrap();
        assert_eq!(activation, Activation::Fresh);
        assert!(files.is_empty());
    }

    #[test]
    fn accepting_the_checkpoint_requires_its_complete_custody() {
        let (worker, daemon) = UnixStream::pair().unwrap();
        let lease = tempfile::tempfile().unwrap();
        send(&worker, &Activation::Checkpoint, &[lease.as_fd()]).unwrap();
        let error = receive_activation(&daemon).unwrap_err();
        assert_eq!(
            error.to_string(),
            "replacement daemon activation has incomplete custody"
        );
    }

    #[test]
    fn activation_transfers_the_same_open_file_description() {
        use std::io::{Read, Seek, SeekFrom};
        let (worker, daemon) = UnixStream::pair().unwrap();
        let mut lease = tempfile::tempfile().unwrap();
        lease.write_all(b"abc").unwrap();
        lease.seek(SeekFrom::Start(0)).unwrap();
        send(&worker, &Activation::Lease, &[lease.as_fd()]).unwrap();
        let (request, mut rights) = receive_activation(&daemon).unwrap();
        assert_eq!(request, Activation::Lease);
        let mut received = fs::File::from(rights.pop().unwrap());
        let mut bytes = [0; 2];
        received.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ab");
        assert_eq!(lease.stream_position().unwrap(), 2);
    }

    #[cfg(all(target_os = "macos", feature = "host"))]
    #[test]
    fn coordinator_disconnect_leaves_only_complete_transfers_with_custody() {
        let (worker, daemon) = UnixStream::pair().unwrap();
        drop(worker);
        assert!(receive_activation(&daemon).is_err());

        let (mut worker, daemon) = UnixStream::pair().unwrap();
        worker.write_all(&[4, 0, 0, 0, b'{']).unwrap();
        drop(worker);
        assert!(receive_activation(&daemon).is_err());

        let (worker, daemon) = UnixStream::pair().unwrap();
        let lease = tempfile::NamedTempFile::new().unwrap();
        let owner = lease.reopen().unwrap();
        // SAFETY: owner is a live regular file and the probe never blocks.
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        send(&worker, &Activation::Lease, &[owner.as_fd()]).unwrap();
        drop(worker);
        drop(owner);
        let (_, mut rights) = receive_activation(&daemon).unwrap();
        let received = File::from(rights.pop().unwrap());
        crate::flock::verify_inherited_exclusive(&received).unwrap();
        assert!(crate::flock::verify_inherited_exclusive(&lease.reopen().unwrap()).is_err());
    }

    #[cfg(all(target_os = "macos", feature = "host"))]
    #[test]
    fn expected_image_must_match_the_vnode_mapped_at_the_attested_address() {
        let process = crate::pinned_process::PinnedProcess::open(std::process::id()).unwrap();
        let metadata = fs::metadata(process.executable().unwrap()).unwrap();
        let image = ParkedImage {
            process,
            expected: (metadata.dev(), metadata.ino()),
            address: crate::process::own_code_address(),
        };
        image.verify().unwrap();
        // SAFETY: dlsym looks up a valid C string. The address belongs to
        // libSystem's mapping and is never dereferenced here.
        let other_code = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"getpid".as_ptr()) };
        assert!(!other_code.is_null());
        let mut image = image;
        let own_code = image.address;
        image.address = other_code as usize as u64;
        assert!(image.verify().is_err());
        image.address = own_code;
        let wrong = tempfile::tempfile().unwrap().metadata().unwrap();
        ParkedImage {
            expected: (wrong.dev(), wrong.ino()),
            ..image
        }
        .verify()
        .unwrap_err();
    }

    #[test]
    fn the_parked_acknowledgement_cannot_confirm_startup() {
        let (worker, daemon) = UnixStream::pair().unwrap();
        send_status(&daemon, Status::Parked).unwrap();
        assert!(expect(&worker, Status::Started).is_err());
    }

    #[test]
    fn startup_progress_keeps_waiting_for_the_exact_replacement() {
        let (worker, daemon) = UnixStream::pair().unwrap();
        wait_for_start(&worker, Duration::from_millis(5), || {
            send_status(&daemon, Status::Started).unwrap();
        })
        .unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_buffered_start_ack_survives_peer_close() {
        let (worker, daemon) = UnixStream::pair().unwrap();
        crate::descriptor::keep_options_after_shutdown(worker.as_fd()).unwrap();
        send_status(&daemon, Status::Started).unwrap();
        drop(daemon);
        wait_for_start(&worker, TRANSFER_TIMEOUT, || {
            panic!("buffered acknowledgement overdue")
        })
        .unwrap();
    }

    #[test]
    fn a_daemon_admission_refusal_has_a_typed_acknowledgement() {
        let (worker, mut daemon) = UnixStream::pair().unwrap();
        daemon.write_all(&[3]).unwrap();
        send(&daemon, &"cannot acquire the launch fence", &[]).unwrap();
        let error = wait_for_start(&worker, TRANSFER_TIMEOUT, || {}).unwrap_err();
        assert!(error.get_ref().unwrap().is::<AdmissionRejected>());
    }
}
