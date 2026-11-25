//! Start the exact replacement process before handing off any live ownership.
//! The private channel stays open until that process is ready to serve.

#[cfg(any(feature = "host", test))]
use super::lease_handoff::DAEMON_LEASE_FD;
use super::lease_handoff::LeaseHandoff;
use crate::{
    descriptor_frame::{FrameByteOrder, receive_json_with_fds_until, send_json_with_fds_until},
    instance::UiUpdateGuard,
    upgrade::recovery::{RECEIPT_DESCRIPTORS, Receipt},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
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
    io::Read,
    os::fd::{AsRawFd, FromRawFd},
};
#[cfg(feature = "host")]
use {super::lease_handoff::DAEMON_LEASE_ENV, crate::upgrade::recovery::RECOVERY_FD_ENV};

pub const ARGUMENT: &str = "--await-update-handoff";
const FRAME_LIMIT: usize = 4096;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Activation {
    Lease,
    Checkpoint,
}

#[cfg(any(feature = "host", test))]
impl Activation {
    fn slots(&self) -> Vec<i32> {
        let mut slots = vec![DAEMON_LEASE_FD];
        if matches!(self, Self::Checkpoint) {
            slots.extend(RECEIPT_DESCRIPTORS);
        }
        slots
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Status {
    Parked,
    Started,
    Failed(String),
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
        1 + RECEIPT_DESCRIPTORS.len(),
        FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)
}

fn expect(stream: &UnixStream, status: Status, timeout: Duration) -> io::Result<()> {
    let (actual, _): (Status, _) = receive_json_with_fds_until(
        stream,
        Instant::now() + timeout,
        FRAME_LIMIT,
        0,
        FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)?;
    if let Status::Failed(reason) = actual {
        return Err(io::Error::other(reason));
    }
    if actual != status {
        return Err(io::Error::other(
            "unexpected replacement daemon startup state",
        ));
    }
    Ok(())
}

pub(crate) struct Parked {
    channel: UnixStream,
    child: Option<Child>,
}

impl Parked {
    pub(crate) fn launch(
        executable: &Path,
        expected: (u64, u64),
        guard: &UiUpdateGuard,
    ) -> io::Result<Self> {
        let (channel, receiver) = UnixStream::pair()?;
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
        // SAFETY: the hook marks unrelated descriptors close-on-exec.
        unsafe { command.pre_exec(super::seal_inherited_descriptors) };
        guard.inherit_for_install(&mut command)?;
        let child = command.spawn()?;
        // Command retains its socket copy for possible reuse. Only the
        // replacement should own the other end after this launch.
        drop(command);
        let pid = child.id();
        let parked = Self {
            channel,
            child: Some(child),
        };
        expect(&parked.channel, Status::Parked, TRANSFER_TIMEOUT)?;
        let actual = fs::metadata(format!("/proc/{pid}/exe"))?;
        if (actual.dev(), actual.ino()) != expected {
            return Err(io::Error::other(
                "the replacement process is not the verified daemon executable",
            ));
        }
        Ok(parked)
    }

    pub(crate) fn check_running(&mut self) -> io::Result<()> {
        if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
            return Err(io::Error::other(format!(
                "replacement daemon exited before activation ({status})"
            )));
        }
        Ok(())
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    pub(crate) fn activate(
        mut self,
        executable: &Path,
        lease: &LeaseHandoff,
        receipt: Option<&Receipt>,
        timeout: Duration,
    ) -> io::Result<()> {
        self.check_running()?;
        let mut descriptors = vec![lease.descriptor()];
        let request = if let Some(receipt) = receipt {
            let files = receipt.files()?;
            crate::upgrade::recovery::validate_forward_target(executable, files[0])?;
            descriptors.extend(files.map(AsFd::as_fd));
            Activation::Checkpoint
        } else {
            Activation::Lease
        };
        // From the first transfer byte onward the child might receive
        // authority. Never kill it on loss of communication after this point.
        let mut child = self.child.take().unwrap();
        let result = (|| {
            self.channel.write_all(&[0])?;
            send(&self.channel, &request, &descriptors)?;
            expect(&self.channel, Status::Started, timeout)
        })();
        // A successful child serves indefinitely. A failed child is reaped
        // without delaying independent inventory verification or shutdown.
        let _ = std::thread::Builder::new()
            .name("cagentsd-update-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
        result
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // We still own an unreaped direct child and have sent no authority.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(feature = "host")]
pub struct Activated(UnixStream);

#[cfg(feature = "host")]
impl Activated {
    /// The host accepted custody and can now answer its authenticated socket.
    pub fn started(self) -> io::Result<()> {
        send(&self.0, &Status::Started, &[])
    }

    pub fn failed(&self, reason: &impl std::fmt::Display) {
        let reason = reason.to_string().chars().take(512).collect();
        let _ = send(&self.0, &Status::Failed(reason), &[]);
    }
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
    let stdin = io::stdin();
    let mut control = UnixStream::from(stdin.as_fd().try_clone_to_owned()?);
    control.peer_addr()?;
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
    send(&control, &Status::Parked, &[])?;
    // Waiting for the originating worker to close the source can take as
    // long as its reviewed work. EOF abandons the standby if that worker exits.
    let mut begin = [0];
    control.read_exact(&mut begin)?;
    if begin != [0] {
        return Err(io::Error::other("invalid replacement daemon activation"));
    }
    let (request, descriptors) = receive_activation(&control)?;
    install_descriptors(descriptors, &request.slots())?;
    // SAFETY: the inherited authority readers run immediately afterward in
    // this same thread, consuming their normal authenticated descriptors.
    unsafe {
        std::env::set_var(DAEMON_LEASE_ENV, DAEMON_LEASE_FD.to_string());
        if matches!(request, Activation::Checkpoint) {
            std::env::set_var(RECOVERY_FD_ENV, RECEIPT_DESCRIPTORS[0].to_string());
        }
    }
    Ok(Activated(control))
}

#[cfg(any(feature = "host", test))]
fn receive_activation(control: &UnixStream) -> io::Result<(Activation, Vec<OwnedFd>)> {
    let (request, descriptors): (Activation, _) = receive_json_with_fds_until(
        control,
        Instant::now() + TRANSFER_TIMEOUT,
        FRAME_LIMIT,
        1 + RECEIPT_DESCRIPTORS.len(),
        FrameByteOrder::LittleEndian,
    )
    .map_err(io::Error::other)?;
    if descriptors.len() != request.slots().len() {
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
    for (descriptor, &slot) in copies.iter().zip(slots) {
        if unsafe { libc::fcntl(slot, libc::F_GETFD) } != -1 {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        if unsafe { libc::dup3(descriptor.as_raw_fd(), slot, libc::O_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_parked_acknowledgement_cannot_confirm_startup() {
        let (worker, daemon) = UnixStream::pair().unwrap();
        send(&daemon, &Status::Parked, &[]).unwrap();
        assert!(expect(&worker, Status::Started, TRANSFER_TIMEOUT).is_err());
    }
}
