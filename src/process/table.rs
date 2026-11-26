//! The system's process table, where Linux reads `/proc`.
//!
//! Linux callers still walk `/proc` directly so their tests can inject a
//! fixture root.

use std::io;

/// `PROC_UID_ONLY` from `<sys/proc_info.h>`: processes whose effective uid is
/// the one given. libc does not bind the `proc_listpids` selectors.
const PROC_UID_ONLY: u32 = 4;
/// `PROC_ALL_PIDS` from `<sys/proc_info.h>`.
#[cfg(feature = "host")]
const PROC_ALL_PIDS: u32 = 1;

/// Every process whose effective uid is `uid`, zombies included, as the
/// kernel lists them now. A pid is only a candidate: it may exit or be reused
/// at once, so a caller pins it before trusting anything it reads about it.
///
/// # Errors
///
/// A census that comes back empty is refused, as is one of this process's
/// own user that leaves this process out: libproc reports a failure as zero
/// bytes, and a caller that takes an empty list for "no such process" would
/// fail open.
pub fn list(uid: u32) -> io::Result<Vec<u32>> {
    // SAFETY: geteuid has no preconditions and no failure state.
    let own_user = uid == unsafe { libc::geteuid() };
    list_pids(PROC_UID_ONLY, uid, own_user)
}

/// The full census, including other users' processes. A foreign process may
/// occupy a session or group and must be visible even when we cannot signal it.
#[cfg(feature = "host")]
pub(crate) fn list_all() -> io::Result<Vec<u32>> {
    list_pids(PROC_ALL_PIDS, 0, true)
}

fn list_pids(selector: u32, argument: u32, includes_self: bool) -> io::Result<Vec<u32>> {
    let entry = std::mem::size_of::<libc::pid_t>();
    // A null buffer answers the size the list needs now. It can grow before
    // the list is read, so leave room and retry whenever the list filled it.
    // SAFETY: a null buffer of length zero is the documented size query.
    let needed = unsafe { libc::proc_listpids(selector, argument, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(failure());
    }
    let mut capacity = needed as usize / entry + 64;
    loop {
        let mut pids: Vec<libc::pid_t> = vec![0; capacity];
        let bytes = libc::c_int::try_from(capacity * entry)
            .map_err(|_| io::Error::other("process list too large"))?;
        // SAFETY: the buffer is writable for `bytes` bytes, and the kernel
        // writes at most that many.
        let written =
            unsafe { libc::proc_listpids(selector, argument, pids.as_mut_ptr().cast(), bytes) };
        if written <= 0 {
            return Err(failure());
        }
        let count = written as usize / entry;
        if count < capacity {
            pids.truncate(count);
            let pids: Vec<u32> = pids
                .into_iter()
                .filter_map(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid != 0)
                .collect();
            if includes_self && !pids.contains(&std::process::id()) {
                return Err(io::Error::other(
                    "the process table left out this process, so it cannot be trusted",
                ));
            }
            return Ok(pids);
        }
        capacity *= 2;
    }
}

/// The process-table fields available even across user IDs. A zombie retains
/// its process group, but `getsid` can no longer read its session. A missing
/// start time means the row can block an operation but cannot authorize a signal.
#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) struct Record {
    pub(crate) bsd: libc::proc_bsdshortinfo,
    pub(crate) session: Option<libc::pid_t>,
    pub(crate) start: Option<u64>,
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn record(pid: libc::pid_t) -> io::Result<Record> {
    if pid <= 0 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    // A process can exit or its PID can change hands between the kernel
    // reads. Retry that particular row before deciding it has gone. A
    // changing row is not evidence of absence.
    for _ in 0..2 {
        let result = read_record(pid);
        if !matches!(&result, Err(error) if error.kind() == io::ErrorKind::WouldBlock) {
            return result;
        }
    }
    Err(process_changed())
}

/// Read a fixed list of candidate PIDs, retrying only rows whose BSD,
/// session, and start observations disagreed. Pending rows receive fresh
/// evidence. Unresolved rows and all other errors still refuse the census.
#[cfg(feature = "host")]
pub(crate) fn settled_rows<T>(
    pids: impl IntoIterator<Item = libc::pid_t>,
    deadline: Option<std::time::Instant>,
    mut read: impl FnMut(libc::pid_t) -> io::Result<Option<T>>,
) -> Result<Vec<T>, (libc::pid_t, io::Error)> {
    use std::time::{Duration, Instant};

    let mut found = Vec::new();
    let mut pending = Vec::new();
    let timed_out = |pid| (pid, io::Error::from(io::ErrorKind::TimedOut));
    let mut visit = |pid| {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(timed_out(pid));
        }
        match read(pid) {
            Ok(record) => {
                found.extend(record);
                Ok(false)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err((pid, error)),
        }
    };
    for pid in pids {
        if visit(pid)? {
            pending.push(pid);
        }
    }
    for millis in [5, 10, 20, 40] {
        let Some(&first) = pending.first() else {
            break;
        };
        let pause = Duration::from_millis(millis);
        let pause = deadline.map_or(pause, |deadline| {
            pause.min(deadline.saturating_duration_since(Instant::now()))
        });
        if pause.is_zero() {
            return Err(timed_out(first));
        }
        std::thread::sleep(pause);
        let mut still_pending = Vec::new();
        for pid in pending {
            if visit(pid)? {
                still_pending.push(pid);
            }
        }
        pending = still_pending;
    }
    match pending.first() {
        Some(&pid) => Err((pid, process_changed())),
        None => Ok(found),
    }
}

#[cfg(any(feature = "host", feature = "ui"))]
fn process_changed() -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, "process changed during census")
}

#[cfg(any(feature = "host", feature = "ui"))]
fn read_record(pid: libc::pid_t) -> io::Result<Record> {
    let before = crate::pinned_process::started_ticks_of(pid as u32);
    let bsd = short_bsd_info(pid)?;
    // SAFETY: the positive PID asks for the corresponding process's session.
    let session = unsafe { libc::getsid(pid) };
    let session = if session < 0 {
        let error = io::Error::last_os_error();
        match (error.raw_os_error(), bsd.pbsi_status) {
            (Some(libc::ESRCH), libc::SZOMB) => None,
            (Some(libc::ESRCH), _) => return Err(process_changed()),
            _ => return Err(error),
        }
    } else {
        Some(session)
    };
    let start = match before {
        Ok(start) => match crate::pinned_process::started_ticks_of(pid as u32) {
            Ok(after) if start == after => Some(start),
            Ok(_) => return Err(process_changed()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(process_changed()),
            Err(error) => return Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(process_changed()),
        Err(error) if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM)) => None,
        Err(error) => return Err(error),
    };
    Ok(Record {
        bsd,
        session,
        start,
    })
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(super) fn short_bsd_info(pid: libc::pid_t) -> io::Result<libc::proc_bsdshortinfo> {
    // SAFETY: the kernel record contains only integers and byte arrays.
    let mut bsd: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&bsd) as libc::c_int;
    // SAFETY: bsd is writable for exactly size bytes. A nonzero arg asks
    // XNU to include the zombie list for this flavor.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            1,
            (&raw mut bsd).cast(),
            size,
        )
    };
    check_record_size(written, size)?;
    if bsd.pbsi_pid != pid as u32 {
        return Err(io::Error::other("the kernel returned an unexpected PID"));
    }
    Ok(bsd)
}

#[cfg(any(feature = "host", feature = "ui"))]
fn check_record_size(written: libc::c_int, expected: libc::c_int) -> io::Result<()> {
    if written <= 0 {
        return Err(failure());
    }
    if written != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the kernel returned a short process record",
        ));
    }
    Ok(())
}

/// Whether the kernel has installed a task-wide suspension and no thread
/// remains runnable. XNU publishes SSTOP before acquiring the task hold,
/// and reports some suspended threads as waiting, so neither BSD status
/// nor a thread's run state alone proves the stop.
#[cfg(feature = "host")]
pub(crate) fn task_stopped(pid: libc::pid_t) -> io::Result<bool> {
    unsafe extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn task_name_for_pid(
            caller: libc::mach_port_t,
            pid: libc::pid_t,
            name: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
        fn task_info(
            name: libc::mach_port_t,
            flavor: libc::task_flavor_t,
            info: libc::task_info_t,
            count: *mut libc::mach_msg_type_number_t,
        ) -> libc::kern_return_t;
        safe fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    // SAFETY: libSystem initializes the task's self port before Rust starts.
    let caller = unsafe { mach_task_self_ };
    let mut name = 0;
    // SAFETY: name is a writable port output and caller is our task port.
    let status = unsafe { task_name_for_pid(caller, pid, &raw mut name) };
    if status != libc::KERN_SUCCESS || name == 0 {
        return Err(io::Error::other(format!(
            "cannot name terminal task: Mach error {status}"
        )));
    }

    // SAFETY: the record contains only integers and integer aggregates.
    let mut info: libc::mach_task_basic_info_data_t = unsafe { std::mem::zeroed() };
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;
    // SAFETY: name is a live task-name right and info has exactly count
    // natural words available for this flavor.
    let status = unsafe {
        task_info(
            name,
            libc::MACH_TASK_BASIC_INFO,
            (&raw mut info).cast(),
            &raw mut count,
        )
    };
    mach_port_deallocate(caller, name);
    if status != libc::KERN_SUCCESS || count != libc::MACH_TASK_BASIC_INFO_COUNT {
        return Err(io::Error::other(format!(
            "cannot inspect terminal task suspension: Mach error {status}"
        )));
    }
    if info.suspend_count == 0 {
        return Ok(false);
    }

    // SAFETY: the record contains only integers.
    let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&task) as libc::c_int;
    // SAFETY: task is writable for exactly size bytes.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut task).cast(), size) };
    check_record_size(written, size)?;
    Ok(task.pti_threadnum > 0 && task.pti_numrunning == 0)
}

/// libproc returns zero bytes on failure, usually with errno set.
fn failure() -> io::Error {
    let error = io::Error::last_os_error();
    if error.raw_os_error().is_some_and(|code| code != 0) {
        error
    } else {
        io::Error::other("the process table could not be listed")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_this_process_for_its_own_user() {
        // SAFETY: geteuid has no preconditions and no failure state.
        let uid = unsafe { libc::geteuid() };
        assert!(super::list(uid).unwrap().contains(&std::process::id()));
    }
}
