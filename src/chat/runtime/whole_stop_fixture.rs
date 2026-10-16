//! Original process custody for the whole-session Stop fixtures.
//! No numeric PID lookup can create cleanup authority here.
use super::Handle;
use std::{
    io::Write,
    os::fd::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

struct Capture {
    ticket: uuid::Uuid,
    root: PathBuf,
    reply: crossbeam_channel::Sender<Result<OwnedFd, String>>,
}
static CAPTURE: OnceLock<Mutex<Option<Capture>>> = OnceLock::new();
// Keep the one-shot original-spawn capture exclusive even in the default
// parallel test harness. This lock never enters production or actor threads.
static SERIAL: Mutex<()> = Mutex::new(());

/// Called immediately after the original ChildGuard captures its own pidfd,
/// before scope enrollment or actor handoff. Other fixture roots are ignored.
pub(super) fn capture(root: &Path, identity: &OwnedFd) {
    let Some(slot) = CAPTURE.get() else {
        return;
    };
    let capture = {
        let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_some_and(|capture| capture.root == root) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(capture) = capture {
        let original = identity.try_clone().map_err(|error| error.to_string());
        let _ = capture.reply.try_send(original);
    }
}

pub(super) struct Custody {
    _serial: MutexGuard<'static, ()>,
    directory: Option<tempfile::TempDir>,
    ticket: uuid::Uuid,
    captured: crossbeam_channel::Receiver<Result<OwnedFd, String>>,
    pin: Option<OwnedFd>,
    handle: Option<Handle>,
    release: Option<crossbeam_channel::Sender<()>>,
}
impl Custody {
    pub(super) fn arm(
        directory: tempfile::TempDir,
        release: crossbeam_channel::Sender<()>,
    ) -> Self {
        let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let ticket = uuid::Uuid::new_v4();
        let (reply, captured) = crossbeam_channel::bounded(1);
        let mut slot = CAPTURE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            slot.is_none(),
            "the dedicated Stop fixture capture is already armed"
        );
        *slot = Some(Capture {
            ticket,
            root: directory.path().to_owned(),
            reply,
        });
        Self {
            _serial: serial,
            directory: Some(directory),
            ticket,
            captured,
            pin: None,
            handle: None,
            release: Some(release),
        }
    }
    pub(super) fn root(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
    pub(super) fn own(&mut self, handle: &Handle) {
        self.handle = Some(handle.clone());
    }
    pub(super) fn observe_original_root(&mut self) {
        self.pin = Some(
            self.captured
                .recv_timeout(Duration::from_secs(3))
                .expect("original provider spawn did not publish its pidfd")
                .expect("original provider pidfd duplication failed"),
        );
    }
    pub(super) fn release_adapter(&mut self) -> Result<(), String> {
        match self.release.take() {
            Some(release) => release.try_send(()).map_err(|error| error.to_string()),
            None => Ok(()),
        }
    }
}
impl Drop for Custody {
    fn drop(&mut self) {
        // Release the existing adapter latch before any close/join attempt.
        let _ = self.release_adapter();
        if let Some(slot) = CAPTURE.get() {
            let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
            if slot
                .as_ref()
                .is_some_and(|capture| capture.ticket == self.ticket)
            {
                slot.take();
            }
        }
        if self.pin.is_none() {
            self.pin = self.captured.try_recv().ok().and_then(Result::ok);
        }
        let result = self.handle.as_ref().map_or_else(
            || Err("provider Handle was not returned".to_owned()),
            |handle| handle.close_until(Instant::now() + Duration::from_secs(3)),
        );
        if result.is_err() {
            if let Some(pin) = &self.pin {
                // This descriptor was duplicated from the original sole-reaper
                // ChildGuard. Never reopen by PID or broaden to a process group.
                let sent = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pin.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                if sent != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        let _ = writeln!(
                            std::io::stderr(),
                            "private Stop fixture signal unconfirmed: {error}"
                        );
                    }
                }
            }
            if let Some(handle) = self.handle.take() {
                if let Err(error) = handle.close_until(Instant::now() + Duration::from_secs(3)) {
                    let _ = writeln!(
                        std::io::stderr(),
                        "private Stop fixture cleanup unconfirmed; retaining original Handle: {error}"
                    );
                    // Keep both the exact runtime owner and its original
                    // signal capability while cleanup remains unconfirmed.
                    std::mem::forget((handle, self.pin.take()));
                }
            } else if let Some(pin) = self.pin.take() {
                let _ = writeln!(
                    std::io::stderr(),
                    "private Stop fixture has no returned Handle; original root remains unconfirmed"
                );
                std::mem::forget(pin);
            }
        }
        // Keep the ORIGINAL namespace on both outcomes. Exact Child/Handle
        // cleanup is not full-family proof; the external strict guardian owns it.
        if let Some(directory) = self.directory.take() {
            let _retained = directory.keep();
        }
    }
}
