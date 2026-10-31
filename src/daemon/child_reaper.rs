//! Exact children preserved for a successor remain children of this daemon.
//! Reserve before spawning, then transfer the original Child and its existing
//! pidfd here if the holder is relinquished. One event-driven worker retains
//! them until exit; it neither signals them nor reaps any unrelated child.
use std::{
    fs::{self, File},
    io,
    os::fd::{AsRawFd, FromRawFd},
    process::Child,
    sync::{Arc, Mutex, OnceLock},
    thread::{self, JoinHandle},
};

const CAPACITY: usize = 4096;
static SERVICE: OnceLock<Result<Arc<Service>, String>> = OnceLock::new();

/// A spawn-time credit. Hold this alongside the original Child until normal
/// reaping or transfer. Dropping an unused reservation returns its credit.
pub(crate) struct Reservation {
    service: Arc<Service>,
    slot: Option<usize>,
    ticket: u64,
}
impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildReaperReservation").field("slot", &self.slot)
            .field("ticket", &self.ticket).finish_non_exhaustive()
    }
}

pub(crate) fn reserve() -> io::Result<Reservation> {
    let service = SERVICE.get_or_init(|| Service::start(CAPACITY)
        .map(|(service, _worker)| service).map_err(|error| error.to_string()))
        .as_ref().map_err(|error| io::Error::other(error.clone()))?;
    service.reserve()
}

/// Explicit recovery uses only already retained original ownership; it never
/// opens a new pidfd by a remembered numeric PID.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RetryReport {
    pub(crate) rearmed: usize,
    pub(crate) retained: usize,
}
pub(crate) fn retry_preserved() -> io::Result<RetryReport> {
    match SERVICE.get() {
        None => Ok(RetryReport { rearmed: 0, retained: 0 }),
        Some(Ok(service)) => service.retry_preserved(),
        Some(Err(error)) => Err(io::Error::other(error.clone())),
    }
}

pub(crate) type RetainError = (io::Error, Child, File, Reservation);
impl Reservation {
    /// Failure returns every ownership object intact. Insertion and wakeup are
    /// atomic to the worker: it cannot observe a transfer that is rolled back.
    pub(crate) fn retain(mut self, child: Child, pidfd: File) -> Result<(), RetainError> {
        if let Err(error) = validate_pidfd(&child, &pidfd) {
            return Err((error, child, pidfd, self));
        }
        let service = self.service.clone();
        let mut state = service.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(error) = state.failure.as_ref() {
            let error = io::Error::other(error.clone());
            drop(state);
            return Err((error, child, pidfd, self));
        }
        if state.stopping {
            drop(state);
            return Err((io::Error::other("Exact-child reaper is stopping"), child, pidfd, self));
        }
        let slot = self.slot.expect("a live reservation owns one credit");
        debug_assert!(state.entries[slot].is_none());
        state.entries[slot] = Some(Entry { child, pidfd: Some(pidfd), ticket: self.ticket, suspended: false });
        if let Err(error) = service.wake() {
            let entry = state.entries[slot].take().expect("transfer remains private under its lock");
            drop(state);
            return Err((error, entry.child, entry.pidfd.expect("normal transfer retains a pidfd"), self));
        }
        self.slot = None;
        Ok(())
    }

    /// Final-drop fallback when normal transfer or spawn-time pidfd capture
    /// fails. The existing credit guarantees storage without allocation or a
    /// successful wake. No descriptor in this suspended entry authorizes a
    /// wait or signal. Admission stops until this ownership fault is repaired;
    /// the original Child and credit remain owned even if the worker stopped.
    pub(crate) fn preserve_suspended(mut self, child: Child, pidfd: Option<File>, reason: &str) {
        let service = self.service.clone();
        let mut state = service.state.lock().unwrap_or_else(|p| p.into_inner());
        let slot = self.slot.take().expect("a live reservation owns one credit");
        debug_assert!(state.entries[slot].is_none());
        let pid = child.id();
        state.entries[slot] = Some(Entry { child, pidfd, ticket: self.ticket, suspended: true });
        if state.failure.is_none() {
            state.failure = Some(format!("Exact child {pid} remains suspended: {reason}"));
        }
        tracing::error!(pid, %reason, "Preserved original child and admission credit without reaping authority; new helper admission is unavailable");
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            self.service.state.lock().unwrap_or_else(|p| p.into_inner()).free.push(slot);
        }
    }
}

struct Entry { child: Child, pidfd: Option<File>, ticket: u64, suspended: bool }
struct State {
    entries: Vec<Option<Entry>>,
    free: Vec<usize>,
    next_ticket: u64,
    failure: Option<String>,
    fatal: bool,
    worker_alive: bool,
    stopping: bool,
}
struct Service {
    event: File,
    state: Mutex<State>,
    #[cfg(test)]
    fail_wake: std::sync::atomic::AtomicBool,
}
impl Service {
    fn start(capacity: usize) -> io::Result<(Arc<Self>, JoinHandle<()>)> {
        if capacity == 0 || capacity > CAPACITY {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid exact-child reaper capacity"));
        }
        let mut entries = Vec::new();
        entries.try_reserve_exact(capacity).map_err(io::Error::other)?;
        entries.resize_with(capacity, || None);
        let mut free = Vec::new();
        free.try_reserve_exact(capacity).map_err(io::Error::other)?;
        free.extend((0..capacity).rev());
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 { return Err(io::Error::last_os_error()); }
        let service = Arc::new(Self {
            event: unsafe { File::from_raw_fd(fd) },
            state: Mutex::new(State { entries, free, next_ticket: 1, failure: None, fatal: false, worker_alive: true, stopping: false }),
            #[cfg(test)] fail_wake: std::sync::atomic::AtomicBool::new(false),
        });
        let owner = service.clone();
        let worker = thread::Builder::new().name("cagentsd-child-reaper".into()).spawn(move || owner.run())?;
        Ok((service, worker))
    }
    fn reserve(self: &Arc<Self>) -> io::Result<Reservation> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(error) = state.failure.as_ref() { return Err(io::Error::other(error.clone())); }
        if state.stopping { return Err(io::Error::other("Exact-child reaper is stopping")); }
        let ticket = state.next_ticket;
        let next = ticket.checked_add(1).ok_or_else(|| io::Error::other("Exact-child reaper tickets exhausted"))?;
        let capacity = state.entries.len();
        let slot = state.free.pop().ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock,
            format!("Helper child ownership capacity is full ({capacity}/{capacity}); existing helpers were retained")))?;
        state.next_ticket = next;
        Ok(Reservation { service: self.clone(), slot: Some(slot), ticket })
    }
    fn retry_preserved(&self) -> io::Result<RetryReport> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopping || state.fatal || !state.worker_alive {
            return Err(io::Error::other(state.failure.clone()
                .unwrap_or_else(|| "Exact-child reaper worker is unavailable".into())));
        }
        let mut rearmed = Vec::new();
        for (slot, entry) in state.entries.iter_mut().enumerate() {
            let Some(entry) = entry.as_mut().filter(|entry| entry.suspended) else { continue; };
            if entry.pidfd.as_ref().is_some_and(|pidfd| validate_pidfd(&entry.child, pidfd).is_ok()) {
                entry.suspended = false;
                rearmed.push(slot);
            }
        }
        // As with retain, the worker cannot see a rearm that failed its wake.
        if !rearmed.is_empty() {
            if let Err(error) = self.wake() {
                for slot in rearmed {
                    state.entries[slot].as_mut().expect("retained under the state lock").suspended = true;
                }
                state.failure = Some(format!("Exact-child recovery wake failed: {error}"));
                return Err(error);
            }
        }
        let retained = state.entries.iter().flatten().filter(|entry| entry.suspended).count();
        if retained == 0 { state.failure = None; }
        else { state.failure = Some(format!("{retained} original helper child owner(s) remain suspended without validated reaping authority")); }
        Ok(RetryReport { rearmed: rearmed.len(), retained })
    }

    fn wake(&self) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_wake.load(std::sync::atomic::Ordering::Acquire) {
            return Err(io::Error::other("injected exact-child wake failure"));
        }
        let value = 1_u64;
        loop {
            let written = unsafe { libc::write(self.event.as_raw_fd(), (&value as *const u64).cast(), 8) };
            if written == 8 { return Ok(()); }
            if written >= 0 { return Err(io::Error::new(io::ErrorKind::WriteZero, "Incomplete exact-child eventfd wake")); }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                // Another wake is already pending; no ownership is lost.
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }
    fn drain_wake(&self) -> io::Result<()> {
        let mut value = 0_u64;
        loop {
            let read = unsafe { libc::read(self.event.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
            if read == 8 { return Ok(()); }
            if read >= 0 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Incomplete exact-child eventfd read")); }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }
    fn fail(&self, error: impl ToString) {
        let error = error.to_string();
        tracing::error!(%error, "Exact-child reaping became unavailable; original children and admission credits remain owned");
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.failure = Some(error);
        state.fatal = true;
        state.worker_alive = false;
    }
    fn run(&self) {
        // Includes an unexpected worker panic: retained originals remain in
        // State, and explicit recovery cannot report a dead worker as armed.
        struct WorkerLifetime<'a>(&'a Service);
        impl Drop for WorkerLifetime<'_> {
            fn drop(&mut self) {
                let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
                state.worker_alive = false;
                if !state.stopping {
                    state.fatal = true;
                    state.failure.get_or_insert_with(|| "Exact-child reaper worker stopped".into());
                }
            }
        }
        let _lifetime = WorkerLifetime(self);
        loop {
            let snapshot = {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.stopping { return; }
                state.entries.iter().enumerate().filter_map(|(slot, entry)| {
                    let entry = entry.as_ref().filter(|entry| !entry.suspended)?;
                    Some((slot, entry.ticket, entry.pidfd.as_ref()?.as_raw_fd()))
                }).collect::<Vec<_>>()
            };
            let mut poll = Vec::with_capacity(snapshot.len() + 1);
            poll.push(libc::pollfd { fd: self.event.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            poll.extend(snapshot.iter().map(|(_, _, fd)| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 }));
            let result = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as _, -1) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted { continue; }
                self.fail(error); return;
            }
            if poll[0].revents != 0 {
                if let Err(error) = self.drain_wake() { self.fail(error); return; }
            }
            for ((slot, ticket, _), ready) in snapshot.iter().zip(&poll[1..]) {
                if ready.revents == 0 { continue; }
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                let Some(entry) = state.entries[*slot].as_mut().filter(|entry| entry.ticket == *ticket) else { continue; };
                // Only this original Child is queried after its pinned pidfd
                // became ready. No process-wide wait or numeric PID signal.
                match entry.child.try_wait() {
                    Ok(Some(_)) => { state.entries[*slot].take(); state.free.push(*slot); }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => {
                        entry.suspended = true;
                        let error = match result { Ok(None) => "pidfd readiness did not confirm the exact child's exit".into(),
                            Err(error) => format!("exact child could not be reaped: {error}"), _ => unreachable!() };
                        state.failure = Some(error.clone());
                        tracing::error!(%error, "Retained exact child and its credit; no polling retry is armed");
                    }
                }
            }
        }
    }
}

fn validate_pidfd(child: &Child, pidfd: &File) -> io::Result<()> {
    let fd = pidfd.as_raw_fd();
    if fs::read_link(format!("/proc/self/fd/{fd}"))? != std::path::Path::new("anon_inode:[pidfd]") {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Child reaping requires its original pidfd"));
    }
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?;
    let actual = info.lines().find_map(|line| line.strip_prefix("Pid:").and_then(|pid| pid.trim().parse::<u32>().ok()));
    if actual != Some(child.id()) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "The retained pidfd does not identify this exact Child"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Stdio, sync::atomic::Ordering, time::{Duration, Instant}};

    struct TestService { service: Arc<Service>, worker: Option<JoinHandle<()>> }
    impl TestService {
        fn new(capacity: usize) -> Self {
            let (service, worker) = Service::start(capacity).unwrap();
            Self { service, worker: Some(worker) }
        }
        fn free(&self) -> usize { self.service.state.lock().unwrap().free.len() }
        fn await_free(&self, expected: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.free() != expected {
                assert!(Instant::now() < deadline, "exact child was not reaped; credit remains retained");
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    impl Drop for TestService {
        fn drop(&mut self) {
            self.service.fail_wake.store(false, Ordering::Release);
            self.service.state.lock().unwrap_or_else(|p| p.into_inner()).stopping = true;
            let _ = self.service.wake();
            if let Some(worker) = self.worker.take() { let _ = worker.join(); }
            let entries = self.service.state.lock().unwrap_or_else(|p| p.into_inner()).entries.iter_mut()
                .filter_map(Option::take).collect::<Vec<_>>();
            for mut entry in entries {
                // Test teardown alone may stop its own fixture child after a
                // failing assertion; production service never sends signals.
                // Suspended entries may contain an absent or untrusted fd.
                // Test teardown owns the original Child and may kill it here.
                if !entry.suspended {
                    let pidfd = entry.pidfd.as_ref().unwrap();
                    unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd.as_raw_fd(), libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(), 0); }
                } else {
                    let _ = entry.child.kill();
                }
                let _ = entry.child.wait();
            }
        }
    }

    struct Probe { child: Option<Child>, pidfd: File }
    impl Probe {
        fn spawn(arguments: &[&str]) -> Self {
            let mut child = std::process::Command::new("/bin/sh").args(arguments)
                .env_clear().env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) as i32 };
            if fd < 0 {
                let error = io::Error::last_os_error();
                let _ = child.kill(); let _ = child.wait();
                panic!("fixture pidfd capture failed: {error}");
            }
            Self { child: Some(child), pidfd: unsafe { File::from_raw_fd(fd) } }
        }
        fn handoff(&mut self, reservation: Reservation) {
            // Keep the original wait owner intact if descriptor duplication fails.
            let pidfd = self.pidfd.try_clone().unwrap();
            let child = self.child.take().unwrap();
            match reservation.retain(child, pidfd) {
                Ok(()) => {}
                Err((error, child, pidfd, reservation)) => {
                    self.child = Some(child); drop(pidfd); drop(reservation);
                    panic!("fixture child transfer failed: {error}");
                }
            }
        }
        fn signal(&self, signal: i32) {
            let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, self.pidfd.as_raw_fd(), signal,
                std::ptr::null::<libc::siginfo_t>(), 0) };
            assert!(result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            // Only this fixture's pinned exact child is signalled, including
            // panic cleanup after the original Child moved into the service.
            unsafe { libc::syscall(libc::SYS_pidfd_send_signal, self.pidfd.as_raw_fd(), libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(), 0); }
            if let Some(child) = self.child.as_mut() { let _ = child.wait(); }
        }
    }

    #[test]
    fn exited_exact_child_is_reaped_without_consuming_an_unrelated_status() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut unrelated = Probe::spawn(&["-c", "exit 19"]);
        let mut owned = Probe::spawn(&["-c", "exit 7"]);
        let pid = owned.child.as_ref().unwrap().id();
        owned.handoff(reservation);
        reaper.await_free(1);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "the original child status must be consumed, not merely observed");
        assert_eq!(unrelated.child.as_mut().unwrap().wait().unwrap().code(), Some(19), "the reaper must never wait on another owner's child");
        assert!(reaper.service.reserve().is_ok());
    }

    #[test]
    fn capacity_is_reserved_before_spawn_and_stays_owned_until_retained_child_exit() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        assert_eq!(reaper.service.reserve().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        owned.handoff(reservation);
        assert_eq!(reaper.free(), 0);
        assert_eq!(reaper.service.reserve().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        owned.signal(libc::SIGKILL);
        reaper.await_free(1);
        assert!(reaper.service.reserve().is_ok());
    }

    #[test]
    fn failed_spawn_and_unused_reservation_drop_return_their_credit() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        assert!(std::process::Command::new("/nonexistent/cagents-fixture-child").spawn().is_err());
        drop(reservation);
        assert_eq!(reaper.free(), 1);
        drop(reaper.service.reserve().unwrap());
        assert_eq!(reaper.free(), 1);
    }

    #[test]
    fn wrong_pidfd_returns_original_child_descriptor_and_reservation_without_transfer() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let mut unrelated = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let wrong = unrelated.pidfd.try_clone().unwrap(); let fd = wrong.as_raw_fd();
        let child = owned.child.take().unwrap(); let pid = child.id();
        let (error, child, returned, reservation) = reservation.retain(child, wrong).unwrap_err();
        owned.child = Some(child);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(owned.child.as_ref().unwrap().id(), pid);
        assert_eq!(returned.as_raw_fd(), fd);
        assert_eq!(reaper.free(), 0);
        assert!(unrelated.child.as_mut().unwrap().try_wait().unwrap().is_none());
        drop(returned); drop(reservation);
        assert_eq!(reaper.free(), 1);
    }

    #[test]
    fn failed_wakeup_rolls_back_transfer_and_preserves_all_exact_ownership_objects() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let ticket = reservation.ticket;
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let pidfd = owned.pidfd.try_clone().unwrap(); let fd = pidfd.as_raw_fd();
        let child = owned.child.take().unwrap(); let pid = child.id();
        reaper.service.fail_wake.store(true, Ordering::Release);
        let (error, child, returned, reservation) = reservation.retain(child, pidfd).unwrap_err();
        owned.child = Some(child);
        assert!(error.to_string().contains("injected"));
        assert_eq!(owned.child.as_ref().unwrap().id(), pid);
        assert_eq!(returned.as_raw_fd(), fd);
        assert_eq!(reservation.ticket, ticket);
        assert_eq!(reaper.free(), 0);
        assert!(reaper.service.state.lock().unwrap().entries.iter().all(Option::is_none));
        drop(returned); drop(reservation);
        assert_eq!(reaper.free(), 1);
    }

    #[test]
    fn final_drop_after_wake_failure_preserves_originals_without_returning_credit() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let pidfd = owned.pidfd.try_clone().unwrap();
        let child = owned.child.take().unwrap(); let pid = child.id();
        reaper.service.fail_wake.store(true, Ordering::Release);
        let (error, child, pidfd, reservation) = reservation.retain(child, pidfd).unwrap_err();
        reservation.preserve_suspended(child, Some(pidfd), &error.to_string());
        let state = reaper.service.state.lock().unwrap();
        let entry = state.entries[0].as_ref().unwrap();
        assert_eq!(entry.child.id(), pid);
        assert!(entry.suspended && entry.pidfd.is_some());
        assert!(state.free.is_empty());
        assert!(state.failure.as_ref().unwrap().contains("injected"));
        drop(state);
        assert!(reaper.service.reserve().is_err(), "suspended ownership must stop new admission");
    }

    #[test]
    fn failed_pidfd_capture_preserves_original_child_without_inventing_reaping_authority() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let child = owned.child.take().unwrap(); let pid = child.id();
        reservation.preserve_suspended(child, None, "spawn-time pidfd capture failed");
        let state = reaper.service.state.lock().unwrap();
        let entry = state.entries[0].as_ref().unwrap();
        assert_eq!(entry.child.id(), pid);
        assert!(entry.suspended && entry.pidfd.is_none());
        assert!(state.free.is_empty());
        drop(state);
        assert!(reaper.service.reserve().is_err());
        assert!(owned.pidfd.metadata().is_ok(), "the fixture retains its own independent cleanup authority");
    }

    #[test]
    fn explicit_retry_rearms_only_retained_valid_pidfds_and_reaps_after_exit() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let pidfd = owned.pidfd.try_clone().unwrap();
        let child = owned.child.take().unwrap();
        reservation.preserve_suspended(child, Some(pidfd), "fixture transfer failure");
        reaper.service.fail_wake.store(true, Ordering::Release);
        assert!(reaper.service.retry_preserved().is_err());
        assert!(reaper.service.state.lock().unwrap().entries[0].as_ref().unwrap().suspended);
        assert_eq!(reaper.free(), 0);
        reaper.service.fail_wake.store(false, Ordering::Release);
        assert_eq!(reaper.service.retry_preserved().unwrap(), RetryReport { rearmed: 1, retained: 0 });
        assert_eq!(reaper.service.reserve().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        owned.signal(libc::SIGKILL);
        reaper.await_free(1);
        assert!(reaper.service.reserve().is_ok());
    }

    #[test]
    fn explicit_retry_keeps_missing_and_wrong_pidfds_suspended_without_new_admission() {
        let reaper = TestService::new(2);
        let mut unrelated = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let mut missing = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let mut wrong = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let missing_reservation = reaper.service.reserve().unwrap();
        let reservation = reaper.service.reserve().unwrap();
        missing_reservation.preserve_suspended(missing.child.take().unwrap(), None, "missing fd");
        reaper.service.reserve().unwrap_err();
        let wrong_pidfd = unrelated.pidfd.try_clone().unwrap();
        reservation.preserve_suspended(wrong.child.take().unwrap(), Some(wrong_pidfd), "wrong fd");
        assert_eq!(reaper.service.retry_preserved().unwrap(), RetryReport { rearmed: 0, retained: 2 });
        assert!(reaper.service.reserve().is_err());
        assert!(reaper.service.state.lock().unwrap().entries.iter().flatten().all(|entry| entry.suspended));
        assert!(unrelated.child.as_mut().unwrap().try_wait().unwrap().is_none());
    }

    #[test]
    fn explicit_retry_refuses_a_stopped_worker_even_with_a_valid_retained_pidfd() {
        let mut reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let pidfd = owned.pidfd.try_clone().unwrap();
        reservation.preserve_suspended(owned.child.take().unwrap(), Some(pidfd), "fixture failure");
        reaper.service.state.lock().unwrap().stopping = true;
        reaper.service.wake().unwrap();
        reaper.worker.take().unwrap().join().unwrap();
        reaper.service.state.lock().unwrap().stopping = false;
        assert!(reaper.service.retry_preserved().is_err());
        assert_eq!(reaper.free(), 0);
        assert!(reaper.service.state.lock().unwrap().entries[0].as_ref().unwrap().suspended);
    }

    #[test]
    fn many_retained_children_share_one_service_and_reuse_only_reaped_credits() {
        let reaper = TestService::new(8);
        let mut children = Vec::new();
        for _ in 0..8 {
            let reservation = reaper.service.reserve().unwrap();
            let mut child = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
            child.handoff(reservation); children.push(child);
        }
        assert_eq!(reaper.free(), 0);
        for child in &children { child.signal(libc::SIGKILL); }
        reaper.await_free(8);
        assert!(reaper.service.state.lock().unwrap().entries.iter().all(Option::is_none));
    }
}
