//! Darwin descendant observations for a dedicated provider owner.
//!
//! XNU coalesces NOTE_FORK and does not deliver the child PID or support
//! recursive NOTE_TRACK. A census can recover current descendants and
//! members of their groups and sessions. It cannot recover a child that
//! changed session and was orphaned before any observation. A watched root
//! that never forked is the one case that can prove exact absence.
use super::holder_registry::Process;
use crate::{pinned_process::PinnedProcess, process::table};
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command},
    time::Duration,
};

pub(super) struct Tracker {
    queue: OwnedFd,
    root: Process,
    tracked: BTreeMap<i32, Process>,
    reaped_exit_event: bool,
    lost_coverage: bool,
}

impl Tracker {
    pub(super) fn fd(&self) -> RawFd {
        self.queue.as_raw_fd()
    }

    /// Install the watch after fork and before exec. Command::spawn waits
    /// for pre_exec to finish, so perform that spawn on a scoped thread
    /// while this thread receives the child PID and opens its kqueue watch.
    pub(super) fn spawn(mut command: Command) -> io::Result<(Child, Self)> {
        let (mut owner, child) = UnixStream::pair()?;
        owner.set_read_timeout(Some(Duration::from_secs(5)))?;
        owner.set_write_timeout(Some(Duration::from_secs(5)))?;
        let owner_fd = owner.as_raw_fd();
        let child_fd = child.as_raw_fd();
        let enabled: libc::c_int = 1;
        // SAFETY: child_fd is live and enabled is an integer of the declared size.
        if unsafe {
            libc::setsockopt(
                child_fd,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const enabled).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the hook only calls async-signal-safe functions, on
        // descriptors both threads keep open until this spawn finishes.
        unsafe {
            command.pre_exec(move || child_gate(owner_fd, child_fd));
        }
        std::thread::scope(|scope| {
            let spawning = std::thread::Builder::new()
                .name("darwin-provider-spawn".into())
                .spawn_scoped(scope, move || {
                    let result = command.spawn();
                    drop(child);
                    result
                })?;
            let tracker = (|| {
                let mut bytes = [0; std::mem::size_of::<libc::pid_t>()];
                owner.read_exact(&mut bytes)?;
                let pid = libc::pid_t::from_ne_bytes(bytes);
                let tracker = Self::attach(pid)?;
                owner.write_all(&[1])?;
                Ok(tracker)
            })();
            // Closing the direction wakes a failed spawn without letting its
            // numeric descriptor be reused before a possible fork.
            let _ = owner.shutdown(Shutdown::Both);
            let child = spawning
                .join()
                .map_err(|_| io::Error::other("provider spawn worker panicked"))?;
            match (child, tracker) {
                (Ok(child), Ok(tracker)) => Ok((child, tracker)),
                (Err(error), Ok(_)) => Err(error),
                (Err(spawn), Err(gate)) if gate.kind() == io::ErrorKind::UnexpectedEof => {
                    Err(spawn)
                }
                (Err(_), Err(error)) => Err(error),
                (Ok(mut child), Err(error)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    Err(error)
                }
            }
        })
    }

    fn attach(pid: i32) -> io::Result<Self> {
        if pid <= 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let root = Process::read(pid)?;
        // SAFETY: kqueue creates a new descriptor with no preconditions.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: kqueue returned a new owned descriptor.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        crate::descriptor::set_close_on_exec(queue.as_fd())?;
        let tracker = Self {
            queue,
            root,
            tracked: BTreeMap::new(),
            reaped_exit_event: false,
            lost_coverage: false,
        };
        tracker.watch(pid)?;
        Ok(tracker)
    }

    fn watch(&self, pid: i32) -> io::Result<()> {
        self.change_watch(pid, libc::EV_ADD | libc::EV_CLEAR)
    }

    fn change_watch(&self, pid: i32, flags: u16) -> io::Result<()> {
        let event = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags,
            fflags: libc::NOTE_FORK | libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one initialized event and a live queue. With zero output
        // slots the call registers the watch without waiting.
        if unsafe {
            libc::kevent(
                self.queue.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn events(&mut self) -> io::Result<bool> {
        let immediate = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a zeroed array of plain kernel event records is valid.
        let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
        let mut saw_fork = false;
        for _ in 0..64 {
            // SAFETY: events holds the advertised writable output array.
            // No registrations change and the timeout is nonblocking.
            let count = unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as libc::c_int,
                    &immediate,
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                self.lost_coverage = true;
                return Err(error);
            }
            if count == 0 {
                return Ok(saw_fork);
            }
            for event in &events[..count as usize] {
                if event.flags & libc::EV_ERROR != 0 {
                    self.lost_coverage = true;
                    return Err(io::Error::from_raw_os_error(event.data as i32));
                }
                saw_fork |= event.fflags & libc::NOTE_FORK != 0;
                if event.ident == self.root.pid as libc::uintptr_t {
                    self.lost_coverage |= event.fflags & libc::NOTE_FORK != 0;
                    self.reaped_exit_event |= event.fflags & libc::NOTE_EXIT != 0;
                }
            }
        }
        self.lost_coverage = true;
        Err(io::Error::other(
            "provider descendant event queue did not settle",
        ))
    }

    /// A control or child wake can give us a pending fork to inspect. Idle
    /// service does not scan the process table without an observed fork.
    pub(super) fn observe(&mut self) -> io::Result<()> {
        if self.events()? {
            self.capture().inspect_err(|_| self.lost_coverage = true)?;
        }
        Ok(())
    }

    /// Capture the current descendant, group and session closure while an
    /// exact member can still authenticate each numeric boundary. Keep
    /// observed epochs so later reparenting cannot hide them.
    pub(super) fn census(&mut self) -> io::Result<()> {
        self.events()?;
        self.capture().inspect_err(|_| self.lost_coverage = true)
    }

    fn capture(&mut self) -> io::Result<()> {
        self.tracked
            .retain(|_, previous| match Process::read(previous.pid) {
                Ok(current) => current == *previous,
                Err(error) => error.kind() != io::ErrorKind::NotFound,
            });
        let owner = super::current_uid();
        let mut records = Vec::new();
        let mut failure = None;
        for pid in table::list_all()? {
            match table::record(pid as i32) {
                Ok(record) => records.push(record),
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        let mut boundaries = self.tracked.clone();
        boundaries.insert(self.root.pid, self.root.clone());
        boundaries.retain(|_, process| process.is_current());
        loop {
            let before = boundaries.len();
            for record in &records {
                let pid = record.bsd.pbsi_pid as i32;
                if pid == self.root.pid
                    || self
                        .tracked
                        .get(&pid)
                        .is_some_and(|process| Some(process.start) == record.start)
                {
                    continue;
                }
                let ids = [
                    Some(record.bsd.pbsi_ppid as i32),
                    Some(record.bsd.pbsi_pgid as i32),
                    record.session,
                ];
                if !ids.into_iter().flatten().any(|id| {
                    boundaries.get(&id).is_some_and(|anchor| {
                        record.start.is_none_or(|start| start >= anchor.start)
                            && anchor.is_current()
                    })
                }) {
                    continue;
                }
                let Some(start) = record.start.filter(|_| record.bsd.pbsi_ruid == owner) else {
                    failure.get_or_insert_with(|| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "cannot prove the ownership of a provider descendant",
                        )
                    });
                    continue;
                };
                let process = Process { pid, start };
                // If a descendant disappears, keep coverage uncertain and
                // do not register against a successor PID.
                self.lost_coverage = true;
                if self.tracked.len() >= 4096 {
                    failure.get_or_insert_with(|| {
                        io::Error::other("provider descendant census exceeded its safety bound")
                    });
                    continue;
                }
                if process.is_current() {
                    match self.watch(pid) {
                        Ok(()) => {}
                        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                        Err(error) => {
                            failure.get_or_insert(error);
                        }
                    }
                    if process.is_current() {
                        boundaries.insert(pid, process.clone());
                        self.tracked.insert(pid, process);
                    } else {
                        let _ = self.change_watch(pid, libc::EV_DELETE);
                    }
                }
            }
            if boundaries.len() == before {
                return failure.map_or(Ok(()), Err);
            }
        }
    }

    /// Stop all still-observable exact descendants. Darwin's process pin
    /// documents the residual check-to-signal interval for non-children.
    pub(super) fn stop(&mut self) -> io::Result<bool> {
        let mut failure = self.census().err();
        let mut live = false;
        for process in self.tracked.values() {
            match PinnedProcess::open_started(process.pid as u32, process.start)
                .and_then(|pin| pin.signal(libc::SIGKILL))
            {
                Ok(signalled) => live |= signalled,
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        failure.map_or(Ok(!live), Err)
    }

    /// Called only after the owner has reaped the root. Observation of all
    /// known exits is weaker than a watch that saw no fork the entire time.
    pub(super) fn prove_empty(&mut self) -> bool {
        self.events().is_ok()
            && self.reaped_exit_event
            && !self.lost_coverage
            && self.tracked.is_empty()
    }
}

/// Runs between fork and exec. Only close, read, write and getpid are used.
fn child_gate(owner: RawFd, child: RawFd) -> io::Result<()> {
    // SAFETY: owner is the copy of the gate's peer in this child, and
    // getpid has no preconditions.
    let pid = unsafe {
        libc::close(owner);
        libc::getpid()
    };
    let bytes = pid.to_ne_bytes();
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: the remaining bytes are initialized and child is live.
        let count = unsafe {
            libc::write(
                child,
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if count <= 0 {
            let error = io::Error::last_os_error();
            if count < 0 && error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(if count == 0 {
                io::Error::from_raw_os_error(libc::EPIPE)
            } else {
                error
            });
        }
        written += count as usize;
    }
    let mut release = 0_u8;
    loop {
        // SAFETY: release is writable for the one byte the parent sends.
        let count = unsafe { libc::read(child, (&raw mut release).cast(), 1) };
        if count == 1 && release == 1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if count < 0 && error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(if count <= 0 {
            io::Error::from_raw_os_error(libc::ECANCELED)
        } else {
            io::Error::from_raw_os_error(libc::EINVAL)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::BufRead, process::Stdio};

    fn launch(script: &str) -> (Child, Tracker) {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        Tracker::spawn(command).unwrap()
    }

    #[test]
    fn an_exit_without_a_fork_proves_empty_after_the_root_is_reaped() {
        let (mut child, mut tracker) = launch("exit 0");
        assert!(child.wait().unwrap().success());
        assert!(tracker.prove_empty());
    }

    #[test]
    fn coalesced_fork_and_exit_keep_cleanup_unproved() {
        let (mut child, mut tracker) = launch("sleep 0 & wait");
        assert!(child.wait().unwrap().success());
        assert!(!tracker.prove_empty());
        assert!(tracker.reaped_exit_event && tracker.lost_coverage);
    }

    #[test]
    fn stop_signals_an_observed_descendant_in_the_owned_group() {
        struct OwnedGroup(Child);
        impl Drop for OwnedGroup {
            fn drop(&mut self) {
                if matches!(self.0.try_wait(), Ok(None)) {
                    // SAFETY: this unreaped test child reserves the group it
                    // leads. The group was created specifically for this test.
                    unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
                    let _ = self.0.wait();
                }
            }
        }

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30 & echo $!; wait"])
            .process_group(0)
            .stdout(Stdio::piped());
        let (child, mut tracker) = Tracker::spawn(command).unwrap();
        let mut child = OwnedGroup(child);
        let mut reader = io::BufReader::new(child.0.stdout.take().unwrap());
        let mut pid = String::new();
        reader.read_line(&mut pid).unwrap();
        let process = Process::read(pid.trim().parse().unwrap()).unwrap();
        assert!(!tracker.stop().unwrap());
        assert_eq!(tracker.tracked.get(&process.pid), Some(&process));
        child.0.wait().unwrap();
        assert!(!tracker.prove_empty());
    }
}
