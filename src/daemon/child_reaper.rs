//! Exact children preserved for a successor remain children of this daemon.
//! Reserve before spawning, then transfer the original Child and its existing
//! pidfd here if the holder is relinquished. One event-driven worker retains
//! them until exit; it neither signals them nor reaps any unrelated child.
use std::{
    fs::{self, File},
    io,
    os::fd::{AsRawFd, FromRawFd},
    process::Child,
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread::{self, JoinHandle},
};

#[cfg(test)]
pub(crate) use tests::TestService;

const CAPACITY: usize = 4096;
static SERVICE: ServiceInitializer = ServiceInitializer::new();

/// Only a successfully started service is process-owned. Transient startup
/// failure refuses this admission without poisoning later attempts. An already
/// installed service keeps its own failure/custody policy; it is never replaced.
struct ServiceInitializer {
    owner: OnceLock<Arc<Service>>,
    // Monotonic even when no service has ever started.
    initialize: Mutex<bool>,
}
impl ServiceInitializer {
    const fn new() -> Self {
        Self {
            owner: OnceLock::new(),
            initialize: Mutex::new(false),
        }
    }
    fn get(&self) -> Option<&Arc<Service>> {
        self.owner.get()
    }
    fn get_or_start(
        &self,
        start: impl FnOnce() -> io::Result<Arc<Service>>,
    ) -> io::Result<Arc<Service>> {
        if let Some(service) = self.get() {
            return Ok(service.clone());
        }
        let closed = self.initialize.lock().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(io::Error::other(
                "Exact-child spawn admission is closed for daemon exit",
            ));
        }
        if let Some(service) = self.get() {
            return Ok(service.clone());
        }
        let service = start()?;
        // The sole setter is serialized. In particular, a failed factory
        // installs nothing and cannot race a second successful worker start.
        assert!(self.owner.set(service.clone()).is_ok());
        Ok(service)
    }

    fn begin_exit_drain(&self) -> ExitDrain {
        // Serialize closure with the sole installer, including its first-start
        // path. A closed, never-initialized service cannot later mint a credit.
        let mut closed = self.initialize.lock().unwrap_or_else(|p| p.into_inner());
        *closed = true;
        let service = self.get().cloned();
        if let Some(service) = &service {
            service.close_admission();
        }
        ExitDrain { service }
    }
}

/// Ordinary-exit permit. Existing original-child owners keep their credits and
/// may still publish late custody; only new spawn admission is closed. Dropping
/// or cloning this observer never reopens admission or discards an owner.
#[derive(Clone)]
pub(crate) struct ExitDrain {
    service: Option<Arc<Service>>,
}

pub(crate) fn begin_exit_drain() -> ExitDrain {
    SERVICE.begin_exit_drain()
}

impl ExitDrain {
    pub(crate) fn try_seal_absence(&self) -> bool {
        let Some(service) = &self.service else {
            return true;
        };
        let mut state = service.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.admission_closed
            && state.free.len() == state.entries.len()
            && state.entries.iter().all(Option::is_none)
            && state.quarantined.iter().all(Option::is_none)
        {
            state.exit_sealed = true;
        }
        state.exit_sealed
    }

    pub(crate) fn pending_description(&self) -> String {
        let Some(service) = &self.service else {
            return "Exact-child spawn admission closed; no service ownership".into();
        };
        let state = service.state.lock().unwrap_or_else(|p| p.into_inner());
        let outstanding = state.entries.len() - state.free.len();
        let retained = state.entries.iter().flatten().count();
        let suspended = state
            .entries
            .iter()
            .flatten()
            .filter(|e| e.suspended)
            .count();
        let quarantined = state.quarantined.iter().flatten().count();
        format!(
            "Waiting for {outstanding} exact-child ownership ticket(s): {retained} retained, {quarantined} quarantined, {suspended} suspended{}",
            state
                .failure
                .as_ref()
                .map(|e| format!("; {e}"))
                .unwrap_or_default()
        )
    }

    /// Read BEFORE checking absence. Used only if the host listener is unusable;
    /// normal drain progress uses the existing daemon wake/listener path.
    pub(crate) fn progress_epoch(&self) -> u64 {
        self.service.as_ref().map_or(0, |service| {
            service
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .progress_epoch
        })
    }

    /// Lost-wake safe: a settlement between the caller's absence check and this
    /// wait changes the epoch under the SAME mutex. No retry timer or new worker.
    pub(crate) fn wait_for_progress(&self, observed: u64) {
        let Some(service) = &self.service else {
            return;
        };
        let mut state = service.state.lock().unwrap_or_else(|p| p.into_inner());
        while state.progress_epoch == observed && state.free.len() != state.entries.len() {
            state = service
                .progress
                .wait(state)
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// A spawn-time credit. Hold this alongside the original Child until normal
/// reaping or transfer. Dropping an unused reservation returns its credit.
pub(crate) struct Reservation {
    service: Arc<Service>,
    slot: Option<usize>,
    ticket: u64,
}
impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildReaperReservation")
            .field("slot", &self.slot)
            .field("ticket", &self.ticket)
            .finish_non_exhaustive()
    }
}

pub(crate) fn reserve() -> io::Result<Reservation> {
    let service =
        SERVICE.get_or_start(|| Service::start(CAPACITY).map(|(service, _worker)| service))?;
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
        None => Ok(RetryReport {
            rearmed: 0,
            retained: 0,
        }),
        Some(service) => service.retry_preserved(),
    }
}

/// Pending spawn credits and retained original children are ownership even
/// before a helper publishes its registry record. Empty-daemon cleanup must
/// not discard them just because filesystem discovery found no helper.
pub(crate) fn has_outstanding_ownership() -> bool {
    match SERVICE.get() {
        Some(service) => service.has_outstanding_ownership(),
        // Initialization failure cannot have handed out a spawn credit.
        None => false,
    }
}

/// Observe settlement in an isolated fixture without starting the service or
/// closing admission. This is an idle observation, not a fence against later
/// reservations; the fixture must exclude unrelated producers itself.
#[cfg(test)]
pub(crate) fn wait_until_idle_for_test(timeout: std::time::Duration) -> bool {
    SERVICE
        .get()
        .is_none_or(|service| service.wait_until_idle_for_test(timeout))
}

/// Preserved originals can outlive their former terminal record. This excludes
/// ordinary caller-held spawn reservations, which remain covered by the normal
/// terminal inventory and its explicit shutdown confirmation contract.
pub(crate) fn has_retained_ownership() -> bool {
    match SERVICE.get() {
        Some(service) => service.has_retained_ownership(),
        None => false,
    }
}

/// Report only retained ownership faults. This observes an existing service;
/// healthy preservation and initialization failures with no children are not
/// shutdown errors, and this query never initializes, retries or waits.
pub(crate) fn retained_custody_error() -> Option<String> {
    match SERVICE.get() {
        Some(service) => service.retained_custody_error(),
        None => None,
    }
}

pub(crate) type RetainError = (io::Error, Child, File, Reservation);
impl Reservation {
    /// Final cleanup may hold the original Child outside this service's map.
    /// Mark that ownership before publishing the handoff or cancelling the
    /// spawn reservation. Its preallocated slot stays fenced through rollback.
    pub(crate) fn quarantine(&self) {
        let slot = self.slot.expect("a live reservation owns one credit");
        let mut state = self.service.state.lock().unwrap_or_else(|p| p.into_inner());
        debug_assert!(state.quarantined[slot].is_none_or(|ticket| ticket == self.ticket));
        state.quarantined[slot] = Some(self.ticket);
        self.service.progress(&mut state);
    }

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
            return Err((
                io::Error::other("Exact-child reaper is stopping"),
                child,
                pidfd,
                self,
            ));
        }
        let slot = self.slot.expect("a live reservation owns one credit");
        debug_assert!(state.entries[slot].is_none());
        state.entries[slot] = Some(Entry {
            child,
            pidfd: Some(pidfd),
            ticket: self.ticket,
            suspended: false,
        });
        if let Err(error) = service.wake() {
            let entry = state.entries[slot]
                .take()
                .expect("transfer remains private under its lock");
            drop(state);
            return Err((
                error,
                entry.child,
                entry.pidfd.expect("normal transfer retains a pidfd"),
                self,
            ));
        }
        self.slot = None;
        service.progress(&mut state);
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
        let slot = self
            .slot
            .take()
            .expect("a live reservation owns one credit");
        debug_assert!(state.entries[slot].is_none());
        let pid = child.id();
        state.entries[slot] = Some(Entry {
            child,
            pidfd,
            ticket: self.ticket,
            suspended: true,
        });
        if state.failure.is_none() {
            state.failure = Some(format!("Exact child {pid} remains suspended: {reason}"));
        }
        service.progress(&mut state);
        drop(state);
        crate::daemon::wake();
        tracing::error!(pid, %reason, "Preserved original child and admission credit without reaping authority; new helper admission is unavailable");
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            let empty = {
                let mut state = self.service.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.quarantined[slot] == Some(self.ticket) {
                    state.quarantined[slot] = None;
                }
                state.free.push(slot);
                self.service.progress(&mut state);
                state.free.len() == state.entries.len()
            };
            // No timer retries the idle census. Publish eligibility before
            // waking its owner, without borrowing the reaper's mutex.
            if empty {
                crate::daemon::wake();
            }
        }
    }
}

struct Entry {
    child: Child,
    pidfd: Option<File>,
    ticket: u64,
    suspended: bool,
}
struct State {
    entries: Vec<Option<Entry>>,
    quarantined: Vec<Option<u64>>,
    free: Vec<usize>,
    next_ticket: u64,
    failure: Option<String>,
    fatal: bool,
    worker_alive: bool,
    stopping: bool,
    admission_closed: bool,
    exit_sealed: bool,
    progress_epoch: u64,
}
struct Service {
    event: File,
    state: Mutex<State>,
    progress: Condvar,
    #[cfg(test)]
    fail_wake: std::sync::atomic::AtomicBool,
}
impl Service {
    fn progress(&self, state: &mut State) {
        state.progress_epoch = state.progress_epoch.wrapping_add(1);
        self.progress.notify_all();
    }

    fn close_admission(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.admission_closed = true;
        self.progress(&mut state);
    }

    fn has_outstanding_ownership(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.free.len() != state.entries.len()
    }

    #[cfg(test)]
    fn wait_until_idle_for_test(&self, timeout: std::time::Duration) -> bool {
        self.wait_until_idle_with(timeout, || {})
    }

    #[cfg(test)]
    fn wait_until_idle_with(
        &self,
        timeout: std::time::Duration,
        mut before_wait: impl FnMut(),
    ) -> bool {
        let started = std::time::Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if state.free.len() == state.entries.len()
                && state.entries.iter().all(Option::is_none)
                && state.quarantined.iter().all(Option::is_none)
            {
                return true;
            }
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return false;
            };
            if remaining.is_zero() {
                return false;
            }
            // The predicate and every ownership-release notification share
            // this mutex, so settlement cannot fall between checking and
            // waiting. Recheck after timeout and spurious progress wakes.
            before_wait();
            let (next, _) = self
                .progress
                .wait_timeout(state, remaining)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
        }
    }

    fn has_retained_ownership(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.entries.iter().any(Option::is_some) || state.quarantined.iter().any(Option::is_some)
    }

    fn retained_custody_error(&self) -> Option<String> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let owns_children = state.entries.iter().any(Option::is_some)
            || state.quarantined.iter().any(Option::is_some);
        if !owns_children {
            return None;
        }
        let suspended = state.entries.iter().flatten().any(|entry| entry.suspended);
        if suspended || state.fatal || !state.worker_alive || state.failure.is_some() {
            Some(state.failure.clone().unwrap_or_else(|| {
                "Retained exact-child custody is suspended or its worker is unavailable".into()
            }))
        } else {
            None
        }
    }

    fn start(capacity: usize) -> io::Result<(Arc<Self>, JoinHandle<()>)> {
        if capacity == 0 || capacity > CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid exact-child reaper capacity",
            ));
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(io::Error::other)?;
        entries.resize_with(capacity, || None);
        let mut quarantined = Vec::new();
        quarantined
            .try_reserve_exact(capacity)
            .map_err(io::Error::other)?;
        quarantined.resize(capacity, None);
        let mut free = Vec::new();
        free.try_reserve_exact(capacity).map_err(io::Error::other)?;
        free.extend((0..capacity).rev());
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let service = Arc::new(Self {
            event: unsafe { File::from_raw_fd(fd) },
            state: Mutex::new(State {
                entries,
                quarantined,
                free,
                next_ticket: 1,
                failure: None,
                fatal: false,
                worker_alive: true,
                stopping: false,
                admission_closed: false,
                exit_sealed: false,
                progress_epoch: 0,
            }),
            progress: Condvar::new(),
            #[cfg(test)]
            fail_wake: std::sync::atomic::AtomicBool::new(false),
        });
        let owner = service.clone();
        let worker = thread::Builder::new()
            .name("cagentsd-child-reaper".into())
            .spawn(move || owner.run())?;
        Ok((service, worker))
    }
    fn reserve(self: &Arc<Self>) -> io::Result<Reservation> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(error) = state.failure.as_ref() {
            return Err(io::Error::other(error.clone()));
        }
        if state.stopping {
            return Err(io::Error::other("Exact-child reaper is stopping"));
        }
        if state.admission_closed {
            return Err(io::Error::other(
                "Exact-child spawn admission is closed for daemon exit",
            ));
        }
        let ticket = state.next_ticket;
        let next = ticket
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Exact-child reaper tickets exhausted"))?;
        let capacity = state.entries.len();
        let slot = state.free.pop().ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock,
            format!("Helper child ownership capacity is full ({capacity}/{capacity}); existing helpers were retained")))?;
        debug_assert!(state.quarantined[slot].is_none());
        state.next_ticket = next;
        Ok(Reservation {
            service: self.clone(),
            slot: Some(slot),
            ticket,
        })
    }
    fn retry_preserved(&self) -> io::Result<RetryReport> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopping || state.fatal || !state.worker_alive {
            return Err(io::Error::other(state.failure.clone().unwrap_or_else(
                || "Exact-child reaper worker is unavailable".into(),
            )));
        }
        let mut rearmed = Vec::new();
        for (slot, entry) in state.entries.iter_mut().enumerate() {
            let Some(entry) = entry.as_mut().filter(|entry| entry.suspended) else {
                continue;
            };
            if entry
                .pidfd
                .as_ref()
                .is_some_and(|pidfd| validate_pidfd(&entry.child, pidfd).is_ok())
            {
                entry.suspended = false;
                rearmed.push(slot);
            }
        }
        // As with retain, the worker cannot see a rearm that failed its wake.
        if !rearmed.is_empty() {
            if let Err(error) = self.wake() {
                for slot in rearmed {
                    state.entries[slot]
                        .as_mut()
                        .expect("retained under the state lock")
                        .suspended = true;
                }
                state.failure = Some(format!("Exact-child recovery wake failed: {error}"));
                self.progress(&mut state);
                drop(state);
                crate::daemon::wake();
                return Err(error);
            }
        }
        let retained = state
            .entries
            .iter()
            .flatten()
            .filter(|entry| entry.suspended)
            .count();
        if retained == 0 {
            state.failure = None;
        } else {
            state.failure = Some(format!(
                "{retained} original helper child owner(s) remain suspended without validated reaping authority"
            ));
        }
        self.progress(&mut state);
        drop(state);
        crate::daemon::wake();
        Ok(RetryReport {
            rearmed: rearmed.len(),
            retained,
        })
    }

    fn wake(&self) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_wake.load(std::sync::atomic::Ordering::Acquire) {
            return Err(io::Error::other("injected exact-child wake failure"));
        }
        let value = 1_u64;
        loop {
            let written =
                unsafe { libc::write(self.event.as_raw_fd(), (&value as *const u64).cast(), 8) };
            if written == 8 {
                return Ok(());
            }
            if written >= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Incomplete exact-child eventfd wake",
                ));
            }
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
            let read =
                unsafe { libc::read(self.event.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
            if read == 8 {
                return Ok(());
            }
            if read >= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Incomplete exact-child eventfd read",
                ));
            }
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
        self.progress(&mut state);
        drop(state);
        crate::daemon::wake();
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
                    state
                        .failure
                        .get_or_insert_with(|| "Exact-child reaper worker stopped".into());
                }
                self.0.progress(&mut state);
                drop(state);
                crate::daemon::wake();
            }
        }
        let _lifetime = WorkerLifetime(self);
        loop {
            let snapshot = {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.stopping {
                    return;
                }
                state
                    .entries
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, entry)| {
                        let entry = entry.as_ref().filter(|entry| !entry.suspended)?;
                        Some((slot, entry.ticket, entry.pidfd.as_ref()?.as_raw_fd()))
                    })
                    .collect::<Vec<_>>()
            };
            let mut poll = Vec::with_capacity(snapshot.len() + 1);
            poll.push(libc::pollfd {
                fd: self.event.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            poll.extend(snapshot.iter().map(|(_, _, fd)| libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            }));
            let result = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as _, -1) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                self.fail(error);
                return;
            }
            if poll[0].revents != 0 {
                if let Err(error) = self.drain_wake() {
                    self.fail(error);
                    return;
                }
            }
            for ((slot, ticket, _), ready) in snapshot.iter().zip(&poll[1..]) {
                if ready.revents == 0 {
                    continue;
                }
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                let Some(entry) = state.entries[*slot]
                    .as_mut()
                    .filter(|entry| entry.ticket == *ticket)
                else {
                    continue;
                };
                // Only this original Child is queried after its pinned pidfd
                // became ready. No process-wide wait or numeric PID signal.
                let reaped = match entry.child.try_wait() {
                    Ok(Some(_)) => {
                        state.entries[*slot].take();
                        if state.quarantined[*slot] == Some(*ticket) {
                            state.quarantined[*slot] = None;
                        }
                        state.free.push(*slot);
                        true
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => {
                        entry.suspended = true;
                        let error = match result {
                            Ok(None) => {
                                "pidfd readiness did not confirm the exact child's exit".into()
                            }
                            Err(error) => format!("exact child could not be reaped: {error}"),
                            _ => unreachable!(),
                        };
                        state.failure = Some(error.clone());
                        tracing::error!(%error, "Retained exact child and its credit; no polling retry is armed");
                        false
                    }
                };
                let empty = reaped && state.free.len() == state.entries.len();
                self.progress(&mut state);
                drop(state);
                if empty || !reaped {
                    crate::daemon::wake();
                }
            }
        }
    }
}

fn validate_pidfd(child: &Child, pidfd: &File) -> io::Result<()> {
    let fd = pidfd.as_raw_fd();
    if fs::read_link(format!("/proc/self/fd/{fd}"))? != std::path::Path::new("anon_inode:[pidfd]") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Child reaping requires its original pidfd",
        ));
    }
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?;
    let actual = info.lines().find_map(|line| {
        line.strip_prefix("Pid:")
            .and_then(|pid| pid.trim().parse::<u32>().ok())
    });
    if actual != Some(child.id()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The retained pidfd does not identify this exact Child",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::Stdio,
        sync::atomic::Ordering,
        time::{Duration, Instant},
    };

    pub(crate) struct TestService {
        service: Arc<Service>,
        worker: Option<JoinHandle<()>>,
    }
    impl TestService {
        pub(crate) fn new(capacity: usize) -> Self {
            let (service, worker) = Service::start(capacity).unwrap();
            Self {
                service,
                worker: Some(worker),
            }
        }
        pub(crate) fn free(&self) -> usize {
            self.service.state.lock().unwrap().free.len()
        }
        pub(crate) fn await_free(&self, expected: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.free() != expected {
                assert!(
                    Instant::now() < deadline,
                    "exact child was not reaped; credit remains retained"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    impl TestService {
        pub(crate) fn reserve(&self) -> io::Result<Reservation> {
            self.service.reserve()
        }
        pub(crate) fn fail_wake(&self, fail: bool) {
            self.service.fail_wake.store(fail, Ordering::Release);
        }
        pub(crate) fn is_suspended(&self, pid: u32) -> bool {
            self.service
                .state
                .lock()
                .unwrap()
                .entries
                .iter()
                .flatten()
                .any(|entry| entry.child.id() == pid && entry.suspended)
        }
        pub(crate) fn retry_preserved(&self) -> io::Result<RetryReport> {
            self.service.retry_preserved()
        }
        pub(crate) fn retained_custody_error(&self) -> Option<String> {
            self.service.retained_custody_error()
        }
    }
    impl Drop for TestService {
        fn drop(&mut self) {
            self.service.fail_wake.store(false, Ordering::Release);
            self.service
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .stopping = true;
            let _ = self.service.wake();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            let entries = self
                .service
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entries
                .iter_mut()
                .filter_map(Option::take)
                .collect::<Vec<_>>();
            for mut entry in entries {
                // Test teardown alone may stop its own fixture child after a
                // failing assertion; production service never sends signals.
                // Suspended entries may contain an absent or untrusted fd.
                // Test teardown owns the original Child and may kill it here.
                if !entry.suspended {
                    let pidfd = entry.pidfd.as_ref().unwrap();
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            pidfd.as_raw_fd(),
                            libc::SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                    }
                } else {
                    let _ = entry.child.kill();
                }
                let _ = entry.child.wait();
            }
        }
    }

    struct Probe {
        child: Option<Child>,
        pidfd: File,
    }
    impl Probe {
        fn spawn(arguments: &[&str]) -> Self {
            let mut child = std::process::Command::new("/bin/sh")
                .args(arguments)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) as i32 };
            if fd < 0 {
                let error = io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                panic!("fixture pidfd capture failed: {error}");
            }
            Self {
                child: Some(child),
                pidfd: unsafe { File::from_raw_fd(fd) },
            }
        }
        fn take_with_pidfd(&mut self, pidfd: io::Result<File>) -> io::Result<(Child, File)> {
            // Descriptor duplication may fail. Keep the original Child in its
            // cleanup owner until every fallible preparation step succeeds.
            let pidfd = pidfd?;
            Ok((self.child.take().unwrap(), pidfd))
        }
        fn handoff(&mut self, reservation: Reservation) {
            let (child, pidfd) = self.take_with_pidfd(self.pidfd.try_clone()).unwrap();
            match reservation.retain(child, pidfd) {
                Ok(()) => {}
                Err((error, child, pidfd, reservation)) => {
                    self.child = Some(child);
                    drop(pidfd);
                    drop(reservation);
                    panic!("fixture child transfer failed: {error}");
                }
            }
        }
        fn signal(&self, signal: i32) {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            assert!(result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            // Only this fixture's pinned exact child is signalled, including
            // panic cleanup after the original Child moved into the service.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait();
            }
        }
    }

    /// Only new drain tests use this owner: successful natural EOF/reaping does
    /// not send a cleanup signal. A failing test still owns the exact pidfd.
    struct DrainChild {
        child: Option<Child>,
        stdin: Option<std::process::ChildStdin>,
        pidfd: File,
        armed: bool,
    }
    impl DrainChild {
        fn spawn() -> Self {
            let mut child = std::process::Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) as i32 };
            if fd < 0 {
                let error = io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                panic!("fixture original pidfd capture failed: {error}");
            }
            let stdin = child.stdin.take();
            Self {
                child: Some(child),
                stdin,
                pidfd: unsafe { File::from_raw_fd(fd) },
                armed: true,
            }
        }
        fn assert_original_reaped(&self) {
            let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    self.pidfd.as_raw_fd() as libc::id_t,
                    &mut status,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            assert_eq!(result, -1);
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }
    impl Drop for DrainChild {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            self.stdin.take();
            let mut poll = libc::pollfd {
                fd: self.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 0) };
            if ready <= 0 || poll.revents == 0 {
                // Failure-only cleanup by the original capability, never a
                // remembered PID. Normal test completion has already reaped.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn idle_observer_counts_caller_reservations_without_closing_admission() {
        let reaper = TestService::new(1);
        assert!(reaper.service.wait_until_idle_for_test(Duration::ZERO));
        let reservation = reaper.reserve().unwrap();
        assert!(
            !reaper
                .service
                .wait_until_idle_for_test(Duration::from_millis(10)),
            "an unpublished caller-owned original still holds its credit"
        );
        assert_eq!(reaper.free(), 0);
        drop(reservation);
        assert!(reaper.service.wait_until_idle_for_test(Duration::ZERO));

        let later = reaper.reserve().unwrap();
        assert!(
            !reaper.service.wait_until_idle_for_test(Duration::ZERO),
            "an idle observation must not seal absence or hide later ownership"
        );
        drop(later);
        assert!(reaper.service.wait_until_idle_for_test(Duration::ZERO));
    }

    #[test]
    fn idle_observer_keeps_quarantined_original_until_wait_and_credit_release() {
        let reaper = TestService::new(1);
        let reservation = reaper.reserve().unwrap();
        let mut original = DrainChild::spawn();
        reservation.quarantine();
        assert!(
            !reaper
                .service
                .wait_until_idle_for_test(Duration::from_millis(10))
        );
        drop(original.stdin.take());
        assert!(original.child.as_mut().unwrap().wait().unwrap().success());
        original.assert_original_reaped();
        assert!(
            !reaper.service.wait_until_idle_for_test(Duration::ZERO),
            "the exact wait does not release a caller-held quarantine credit"
        );
        drop(reservation);
        assert!(reaper.service.wait_until_idle_for_test(Duration::ZERO));
        original.armed = false;
    }

    #[test]
    fn idle_observer_waits_for_retained_original_reap_and_credit_release() {
        let reaper = TestService::new(1);
        let reservation = reaper.reserve().unwrap();
        let mut original = DrainChild::spawn();
        let pidfd = original.pidfd.try_clone().unwrap();
        let child = original.child.take().unwrap();
        if let Err((error, child, _, reservation)) = reservation.retain(child, pidfd) {
            original.child = Some(child);
            drop(reservation);
            panic!("fixture original transfer failed: {error}");
        }
        assert!(
            !reaper
                .service
                .wait_until_idle_for_test(Duration::from_millis(10)),
            "observation must neither signal nor release a live retained child"
        );
        thread::scope(|scope| {
            let (waiting, ready) = std::sync::mpsc::channel();
            let observed = scope.spawn(|| {
                let mut waiting = Some(waiting);
                reaper
                    .service
                    .wait_until_idle_with(Duration::from_secs(5), || {
                        if let Some(waiting) = waiting.take() {
                            waiting.send(()).unwrap();
                        }
                    })
            });
            // The observer still owns the predicate mutex at this signal.
            // EOF therefore forces the reaper to wake an actual waiting
            // observer, rather than allowing an already-idle fast path.
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(original.stdin.take());
            assert!(observed.join().unwrap(), "original custody did not settle");
        });
        original.assert_original_reaped();
        assert_eq!(reaper.free(), 1);
        original.armed = false;
        drop(reaper.reserve().unwrap());
    }

    #[test]
    fn exit_drain_before_initialization_permanently_refuses_a_new_worker() {
        let initializer = ServiceInitializer::new();
        let drain = initializer.begin_exit_drain();
        assert!(drain.try_seal_absence());
        assert!(
            initializer
                .get_or_start(|| panic!("closed initializer must not start a worker"))
                .is_err()
        );
        drop(drain.clone());
        assert!(initializer.begin_exit_drain().try_seal_absence());
        assert!(
            initializer
                .get_or_start(|| panic!("dropping drain cannot reopen admission"))
                .is_err()
        );
    }

    #[test]
    fn exit_drain_serializes_with_inflight_first_initialization_and_fastpath_reserve() {
        use std::sync::mpsc;
        let initializer = ServiceInitializer::new();
        let (entered, entered_rx) = mpsc::channel();
        let (continue_start, continue_rx) = mpsc::channel();
        let (reaper, drain) = thread::scope(|scope| {
            let initializer_ref = &initializer;
            let startup = scope.spawn(move || {
                let mut cleanup = None;
                initializer_ref
                    .get_or_start(|| {
                        entered.send(()).unwrap();
                        continue_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                        let reaper = TestService::new(1);
                        let service = reaper.service.clone();
                        cleanup = Some(reaper);
                        Ok(service)
                    })
                    .unwrap();
                cleanup.unwrap()
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let closer = scope.spawn(|| initializer.begin_exit_drain());
            continue_start.send(()).unwrap();
            (startup.join().unwrap(), closer.join().unwrap())
        });
        assert!(drain.try_seal_absence());
        let installed = initializer
            .get_or_start(|| panic!("installed service is never replaced"))
            .unwrap();
        assert!(Arc::ptr_eq(&installed, &reaper.service));
        assert!(
            installed
                .reserve()
                .unwrap_err()
                .to_string()
                .contains("closed")
        );
        assert_eq!(reaper.free(), 1);
    }

    #[test]
    fn exit_drain_counts_unpublished_original_until_late_transfer_and_actual_reap() {
        use std::sync::mpsc;
        let reaper = TestService::new(1);
        let reservation = reaper.reserve().unwrap();
        let mut original = DrainChild::spawn();
        let pid = original.child.as_ref().unwrap().id();
        assert!(!reaper.service.has_retained_ownership());
        assert!(reaper.service.has_outstanding_ownership());
        let (publish, publish_rx) = mpsc::channel();
        let pidfd = original.pidfd.try_clone().unwrap();
        let mut late_original = DrainChild {
            child: original.child.take(),
            stdin: None,
            pidfd,
            armed: true,
        };
        let drain = thread::scope(|scope| {
            let late_owner = scope.spawn(move || -> io::Result<()> {
                if let Err(error) = publish_rx.recv_timeout(Duration::from_secs(5)) {
                    drop(late_original);
                    drop(reservation);
                    return Err(io::Error::other(error));
                }
                reservation.quarantine();
                let pidfd = match late_original.pidfd.try_clone() {
                    Ok(pidfd) => pidfd,
                    Err(error) => {
                        drop(late_original);
                        drop(reservation);
                        return Err(error);
                    }
                };
                let child = late_original.child.take().unwrap();
                match reservation.retain(child, pidfd) {
                    Ok(()) => {
                        late_original.armed = false;
                        Ok(())
                    }
                    Err((error, child, _, reservation)) => {
                        late_original.child = Some(child);
                        // Reap via the original cleanup owner before returning
                        // its credit, including assertion/channel failures.
                        drop(late_original);
                        drop(reservation);
                        Err(error)
                    }
                }
            });
            reaper.service.close_admission();
            let drain = ExitDrain {
                service: Some(reaper.service.clone()),
            };
            assert!(
                !drain.try_seal_absence(),
                "unpublished original credit must fence exit"
            );
            assert!(
                drain
                    .pending_description()
                    .contains("1 exact-child ownership")
            );
            assert!(reaper.reserve().is_err());
            publish.send(()).unwrap();
            late_owner
                .join()
                .unwrap()
                .expect("preexisting original must remain transferable after admission closure");
            drain
        });
        {
            let state = reaper.service.state.lock().unwrap();
            assert_eq!(
                state.entries.iter().flatten().next().unwrap().child.id(),
                pid
            );
        }
        assert!(!drain.try_seal_absence());
        original.stdin.take(); // Actual child exits naturally on EOF.
        reaper.await_free(1);
        original.assert_original_reaped();
        assert!(drain.try_seal_absence());
        assert!(drain.clone().try_seal_absence());
        assert!(reaper.reserve().is_err());
    }

    #[test]
    fn exit_drain_keeps_exited_suspended_original_until_explicit_exact_recovery() {
        let reaper = TestService::new(1);
        let reservation = reaper.reserve().unwrap();
        let mut original = DrainChild::spawn();
        let pidfd = original.pidfd.try_clone().unwrap();
        reaper.service.close_admission();
        let drain = ExitDrain {
            service: Some(reaper.service.clone()),
        };
        let observed = drain.progress_epoch();
        reservation.preserve_suspended(
            original.child.take().unwrap(),
            Some(pidfd),
            "fixture suspended original",
        );
        drain.wait_for_progress(observed);
        assert!(!drain.try_seal_absence());
        assert!(drain.pending_description().contains("1 suspended"));
        original.stdin.take();
        let mut poll = libc::pollfd {
            fd: original.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 5000) }, 1);
        assert_ne!(poll.revents & libc::POLLIN, 0);
        assert!(
            !drain.try_seal_absence(),
            "observed death is not original-child reaping"
        );
        assert_eq!(reaper.free(), 0);
        let report = reaper.retry_preserved().unwrap();
        assert_eq!(
            report,
            RetryReport {
                rearmed: 1,
                retained: 0
            }
        );
        reaper.await_free(1);
        original.assert_original_reaped();
        assert!(drain.try_seal_absence());
        assert!(
            reaper.reserve().is_err(),
            "recovery cannot reopen spawn admission"
        );
    }

    #[test]
    fn exit_drain_progress_wait_cannot_lose_final_credit_release_or_custody_fault() {
        use std::sync::mpsc;
        let reaper = TestService::new(1);
        let reservation = reaper.reserve().unwrap();
        reaper.service.close_admission();
        let drain = ExitDrain {
            service: Some(reaper.service.clone()),
        };
        let observed = drain.progress_epoch();
        assert!(!drain.try_seal_absence());
        drop(reservation); // Release BEFORE entering wait: must not sleep forever.
        drain.wait_for_progress(observed);
        assert!(drain.try_seal_absence());

        let reaper = TestService::new(1);
        let reservation = reaper.reserve().unwrap();
        reaper.service.close_admission();
        let drain = ExitDrain {
            service: Some(reaper.service.clone()),
        };
        let observed = drain.progress_epoch();
        let (finished, finished_rx) = mpsc::channel();
        thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                drain.wait_for_progress(observed);
                finished.send(()).unwrap();
            });
            reaper
                .service
                .fail("fixture fatal worker with caller-held original credit");
            let result = finished_rx.recv_timeout(Duration::from_secs(5));
            // Even if the tested fault notification regresses, release wakes
            // the waiter before asserting, so this control cannot strand it.
            drop(reservation);
            waiter.join().unwrap();
            result.expect("custody failure must wake the pending-exit observer");
        });
        assert!(
            drain.try_seal_absence(),
            "a failed worker with no remaining owner is absent"
        );
    }

    #[test]
    fn initialization_failure_can_retry_and_the_real_worker_reaps_its_original_child() {
        use std::sync::atomic::AtomicUsize;

        let initializer = ServiceInitializer::new();
        let attempts = AtomicUsize::new(0);
        let failure = initializer
            .get_or_start(|| {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::from_raw_os_error(libc::EAGAIN))
            })
            .err()
            .expect("the failed admission must remain a failure");
        assert_eq!(failure.raw_os_error(), Some(libc::EAGAIN));
        assert!(initializer.get().is_none());

        let mut cleanup = None;
        let service = initializer
            .get_or_start(|| {
                attempts.fetch_add(1, Ordering::SeqCst);
                let reaper = TestService::new(1);
                let service = reaper.service.clone();
                cleanup = Some(reaper);
                Ok(service)
            })
            .unwrap();
        let reaper = cleanup.unwrap();
        let same = initializer
            .get_or_start(|| panic!("a successful service must not be restarted"))
            .unwrap();
        assert!(Arc::ptr_eq(&service, &same));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);

        let reservation = same.reserve().unwrap();
        let mut child = Probe::spawn(&["-c", "exit 23"]);
        child.handoff(reservation);
        reaper.await_free(1);
        assert!(child.child.is_none());
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // WNOWAIT never consumes a status, and this fd cannot name a reused
        // PID. ECHILD proves the service already reaped its exact original.
        let result = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                child.pidfd.as_raw_fd() as libc::id_t,
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        assert_eq!(result, -1);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        assert!(!service.has_outstanding_ownership());
        assert!(!service.has_retained_ownership());
    }

    #[test]
    fn concurrent_initialization_installs_one_worker_and_shares_its_admission_budget() {
        use std::sync::{Barrier, atomic::AtomicUsize};

        const CALLERS: usize = 8;
        let initializer = ServiceInitializer::new();
        let attempts = AtomicUsize::new(0);
        let cleanup = Mutex::new(None);
        let start = Barrier::new(CALLERS + 1);
        let owners = thread::scope(|scope| {
            let mut callers = Vec::new();
            for _ in 0..CALLERS {
                callers.push(scope.spawn(|| {
                    start.wait();
                    let owner = initializer
                        .get_or_start(|| {
                            attempts.fetch_add(1, Ordering::SeqCst);
                            let reaper = TestService::new(CALLERS);
                            let service = reaper.service.clone();
                            *cleanup.lock().unwrap() = Some(reaper);
                            Ok(service)
                        })
                        .unwrap();
                    let reservation = owner.reserve().unwrap();
                    (owner, reservation)
                }));
            }
            start.wait();
            callers
                .into_iter()
                .map(|caller| caller.join().unwrap())
                .collect::<Vec<_>>()
        });
        let reaper = cleanup.into_inner().unwrap().unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let installed = initializer.get().unwrap();
        assert!(
            owners
                .iter()
                .all(|(owner, _)| Arc::ptr_eq(owner, installed))
        );
        assert_eq!(reaper.free(), 0);
        assert_eq!(
            installed.reserve().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(owners);
        reaper.await_free(CALLERS);
        assert!(!installed.has_outstanding_ownership());
    }

    #[test]
    fn installed_service_failure_is_not_replaced_and_keeps_original_child_custody() {
        let initializer = ServiceInitializer::new();
        let mut cleanup = None;
        let service = initializer
            .get_or_start(|| {
                let reaper = TestService::new(1);
                let service = reaper.service.clone();
                cleanup = Some(reaper);
                Ok(service)
            })
            .unwrap();
        let reaper = cleanup.unwrap();
        let reservation = service.reserve().unwrap();
        let mut child = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let original_pid = child.child.as_ref().unwrap().id();
        child.handoff(reservation);
        service.fail("fixture installed worker failed with retained custody");

        let same = initializer
            .get_or_start(|| panic!("installed failure must not start a replacement worker"))
            .unwrap();
        assert!(Arc::ptr_eq(&service, &same));
        assert!(same.has_retained_ownership());
        assert!(same.has_outstanding_ownership());
        assert!(
            same.reserve()
                .unwrap_err()
                .to_string()
                .contains("fixture installed")
        );
        assert!(same.retry_preserved().is_err());
        assert_eq!(reaper.free(), 0);
        assert_eq!(
            service.state.lock().unwrap().entries[0]
                .as_ref()
                .unwrap()
                .child
                .id(),
            original_pid,
            "failed installed service must retain the SAME original child"
        );
        assert!(child.child.is_none());
        // Existing Probe/TestService owners retain failure-only exact child
        // cleanup and actual worker join; production never signals here.
    }

    #[test]
    fn failed_worker_without_retained_custody_is_not_a_shutdown_ownership_error() {
        let reaper = TestService::new(1);
        let reservation = reaper.reserve().unwrap();
        reaper
            .service
            .fail("fixture failed worker without retained child");
        // An ordinary spawn credit alone is not retained child custody.
        assert_eq!(reaper.retained_custody_error(), None);
        drop(reservation);
        assert_eq!(reaper.retained_custody_error(), None);
    }

    #[test]
    fn failed_descriptor_preparation_keeps_the_original_child_in_its_cleanup_owner() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let pid = owned.child.as_ref().unwrap().id();
        let error = owned
            .take_with_pidfd(Err(io::Error::from_raw_os_error(libc::EMFILE)))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
        assert_eq!(owned.child.as_ref().unwrap().id(), pid);
        assert!(owned.child.as_mut().unwrap().try_wait().unwrap().is_none());
        assert_eq!(reaper.free(), 0);
        assert!(reaper.service.state.lock().unwrap().entries[0].is_none());

        // The original owner and its existing credit remain usable after the
        // preparation failure; the exact original wait must release the slot.
        owned.handoff(reservation);
        owned.signal(libc::SIGKILL);
        reaper.await_free(1);
        assert!(!reaper.service.has_outstanding_ownership());
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
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the original child status must be consumed, not merely observed"
        );
        assert_eq!(
            unrelated.child.as_mut().unwrap().wait().unwrap().code(),
            Some(19),
            "the reaper must never wait on another owner's child"
        );
        assert!(reaper.service.reserve().is_ok());
    }

    #[test]
    fn capacity_is_reserved_before_spawn_and_stays_owned_until_retained_child_exit() {
        let reaper = TestService::new(1);
        assert!(!reaper.service.has_outstanding_ownership());
        let reservation = reaper.service.reserve().unwrap();
        assert!(
            reaper.service.has_outstanding_ownership(),
            "pre-registry spawn ownership must fence idle exit"
        );
        assert!(
            !reaper.service.has_retained_ownership(),
            "a caller-held credit is not an independent retained child"
        );
        assert_eq!(
            reaper.service.reserve().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        owned.handoff(reservation);
        assert!(reaper.service.has_retained_ownership());
        assert_eq!(reaper.free(), 0);
        assert_eq!(
            reaper.service.reserve().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        owned.signal(libc::SIGKILL);
        reaper.await_free(1);
        assert!(
            !reaper.service.has_outstanding_ownership(),
            "only exact reaping releases the idle ownership fence"
        );
        assert!(!reaper.service.has_retained_ownership());
        assert!(reaper.service.reserve().is_ok());
    }

    #[test]
    fn quarantined_original_child_stays_visible_through_rollback_handoff_and_slot_reuse() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let ticket = reservation.ticket;
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        assert!(!reaper.service.has_retained_ownership());
        reservation.quarantine();
        reservation.quarantine();
        assert!(
            reaper.service.has_retained_ownership(),
            "the final-cleanup queue must not hide this original child"
        );
        reaper.service.fail_wake.store(true, Ordering::Release);
        let (child, pidfd) = owned.take_with_pidfd(owned.pidfd.try_clone()).unwrap();
        let (error, child, pidfd, reservation) = reservation.retain(child, pidfd).unwrap_err();
        owned.child = Some(child);
        assert!(error.to_string().contains("injected"));
        assert!(
            reaper.service.has_retained_ownership(),
            "a rolled-back transfer still belongs to final cleanup"
        );
        assert_eq!(
            reaper.service.state.lock().unwrap().quarantined[0],
            Some(ticket)
        );
        drop(pidfd);
        reaper.service.fail_wake.store(false, Ordering::Release);
        owned.handoff(reservation);
        assert!(reaper.service.has_retained_ownership());
        owned.signal(libc::SIGKILL);
        reaper.await_free(1);
        assert!(!reaper.service.has_retained_ownership());
        let next = reaper.service.reserve().unwrap();
        assert_ne!(next.ticket, ticket);
        assert!(
            !reaper.service.has_retained_ownership(),
            "a recycled slot must not inherit the previous owner's quarantine"
        );
    }

    #[test]
    fn direct_original_child_wait_then_reservation_drop_releases_quarantined_ownership() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exit 23"]);
        reservation.quarantine();
        assert!(reaper.service.has_retained_ownership());
        assert_eq!(
            owned.child.as_mut().unwrap().wait().unwrap().code(),
            Some(23)
        );
        assert!(
            reaper.service.has_retained_ownership(),
            "the caller must publish its completed ownership release"
        );
        drop(reservation);
        assert!(!reaper.service.has_retained_ownership());
        assert!(!reaper.service.has_outstanding_ownership());
        assert_eq!(reaper.free(), 1);
    }

    #[test]
    fn failed_spawn_and_unused_reservation_drop_return_their_credit() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        assert!(
            std::process::Command::new("/nonexistent/cagents-fixture-child")
                .spawn()
                .is_err()
        );
        drop(reservation);
        assert_eq!(reaper.free(), 1);
        assert!(!reaper.service.has_outstanding_ownership());
        drop(reaper.service.reserve().unwrap());
        assert_eq!(reaper.free(), 1);
    }

    #[test]
    fn wrong_pidfd_returns_original_child_descriptor_and_reservation_without_transfer() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let mut unrelated = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let (child, wrong) = owned.take_with_pidfd(unrelated.pidfd.try_clone()).unwrap();
        let pid = child.id();
        let fd = wrong.as_raw_fd();
        let (error, child, returned, reservation) = reservation.retain(child, wrong).unwrap_err();
        owned.child = Some(child);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(owned.child.as_ref().unwrap().id(), pid);
        assert_eq!(returned.as_raw_fd(), fd);
        assert_eq!(reaper.free(), 0);
        assert!(
            unrelated
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        );
        drop(returned);
        drop(reservation);
        assert_eq!(reaper.free(), 1);
    }

    #[test]
    fn failed_wakeup_rolls_back_transfer_and_preserves_all_exact_ownership_objects() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let ticket = reservation.ticket;
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let (child, pidfd) = owned.take_with_pidfd(owned.pidfd.try_clone()).unwrap();
        let pid = child.id();
        let fd = pidfd.as_raw_fd();
        reaper.service.fail_wake.store(true, Ordering::Release);
        let (error, child, returned, reservation) = reservation.retain(child, pidfd).unwrap_err();
        owned.child = Some(child);
        assert!(error.to_string().contains("injected"));
        assert_eq!(owned.child.as_ref().unwrap().id(), pid);
        assert_eq!(returned.as_raw_fd(), fd);
        assert_eq!(reservation.ticket, ticket);
        assert_eq!(reaper.free(), 0);
        assert!(
            reaper
                .service
                .state
                .lock()
                .unwrap()
                .entries
                .iter()
                .all(Option::is_none)
        );
        drop(returned);
        drop(reservation);
        assert_eq!(reaper.free(), 1);
    }

    #[test]
    fn final_drop_after_wake_failure_preserves_originals_without_returning_credit() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let (child, pidfd) = owned.take_with_pidfd(owned.pidfd.try_clone()).unwrap();
        let pid = child.id();
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
        assert!(
            reaper.service.reserve().is_err(),
            "suspended ownership must stop new admission"
        );
    }

    #[test]
    fn failed_pidfd_capture_preserves_original_child_without_inventing_reaping_authority() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let child = owned.child.take().unwrap();
        let pid = child.id();
        reservation.preserve_suspended(child, None, "spawn-time pidfd capture failed");
        assert!(
            reaper.service.has_outstanding_ownership(),
            "missing registry/pidfd cannot make an owned child disappear from the idle census"
        );
        assert!(reaper.service.has_retained_ownership());
        let state = reaper.service.state.lock().unwrap();
        let entry = state.entries[0].as_ref().unwrap();
        assert_eq!(entry.child.id(), pid);
        assert!(entry.suspended && entry.pidfd.is_none());
        assert!(state.free.is_empty());
        drop(state);
        assert!(reaper.service.reserve().is_err());
        assert!(
            owned.pidfd.metadata().is_ok(),
            "the fixture retains its own independent cleanup authority"
        );
    }

    #[test]
    fn explicit_retry_rearms_only_retained_valid_pidfds_and_reaps_after_exit() {
        let reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let (child, pidfd) = owned.take_with_pidfd(owned.pidfd.try_clone()).unwrap();
        reservation.quarantine();
        reservation.preserve_suspended(child, Some(pidfd), "fixture transfer failure");
        reaper.service.fail_wake.store(true, Ordering::Release);
        assert!(reaper.service.retry_preserved().is_err());
        assert!(
            reaper.service.state.lock().unwrap().entries[0]
                .as_ref()
                .unwrap()
                .suspended
        );
        assert_eq!(reaper.free(), 0);
        reaper.service.fail_wake.store(false, Ordering::Release);
        assert_eq!(
            reaper.service.retry_preserved().unwrap(),
            RetryReport {
                rearmed: 1,
                retained: 0
            }
        );
        assert_eq!(
            reaper.service.reserve().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        owned.signal(libc::SIGKILL);
        reaper.await_free(1);
        assert!(!reaper.service.has_retained_ownership());
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
        let (child, pidfd) = wrong.take_with_pidfd(unrelated.pidfd.try_clone()).unwrap();
        reservation.preserve_suspended(child, Some(pidfd), "wrong fd");
        assert_eq!(
            reaper.service.retry_preserved().unwrap(),
            RetryReport {
                rearmed: 0,
                retained: 2
            }
        );
        assert!(reaper.service.reserve().is_err());
        assert!(
            reaper
                .service
                .state
                .lock()
                .unwrap()
                .entries
                .iter()
                .flatten()
                .all(|entry| entry.suspended)
        );
        assert!(
            unrelated
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_retry_refuses_a_stopped_worker_even_with_a_valid_retained_pidfd() {
        let mut reaper = TestService::new(1);
        let reservation = reaper.service.reserve().unwrap();
        let mut owned = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
        let (child, pidfd) = owned.take_with_pidfd(owned.pidfd.try_clone()).unwrap();
        reservation.preserve_suspended(child, Some(pidfd), "fixture failure");
        reaper.service.state.lock().unwrap().stopping = true;
        reaper.service.wake().unwrap();
        reaper.worker.take().unwrap().join().unwrap();
        reaper.service.state.lock().unwrap().stopping = false;
        assert!(reaper.service.retry_preserved().is_err());
        assert_eq!(reaper.free(), 0);
        assert!(
            reaper.service.state.lock().unwrap().entries[0]
                .as_ref()
                .unwrap()
                .suspended
        );
    }

    #[test]
    fn many_retained_children_share_one_service_and_reuse_only_reaped_credits() {
        let reaper = TestService::new(8);
        let mut children = Vec::new();
        for _ in 0..8 {
            let reservation = reaper.service.reserve().unwrap();
            let mut child = Probe::spawn(&["-c", "exec /bin/sleep 600"]);
            child.handoff(reservation);
            children.push(child);
        }
        assert_eq!(reaper.free(), 0);
        for child in &children {
            child.signal(libc::SIGKILL);
        }
        reaper.await_free(8);
        assert!(
            reaper
                .service
                .state
                .lock()
                .unwrap()
                .entries
                .iter()
                .all(Option::is_none)
        );
    }
}
