//! The system's process table, where Linux reads `/proc`.
//!
//! Only the census Darwin callers need so far lives here; Linux callers still
//! walk `/proc` directly so their tests can inject a fixture root.

use std::io;

/// `PROC_UID_ONLY` from `<sys/proc_info.h>`: processes whose effective uid is
/// the one given. libc does not bind the `proc_listpids` selectors.
const PROC_UID_ONLY: u32 = 4;

/// Every process whose effective uid is `uid`, zombies included, as the
/// kernel lists them now. A pid is only a candidate: it may exit or be reused
/// at once, so a caller pins it before trusting anything it reads about it.
///
/// A census that comes back empty is refused, as is one of this process's
/// own user that leaves this process out: libproc reports a failure as zero
/// bytes, and a caller that takes an empty list for "no such process" would
/// fail open.
pub(crate) fn list(uid: u32) -> io::Result<Vec<u32>> {
    let entry = std::mem::size_of::<libc::pid_t>();
    // A null buffer answers the size the list needs now. It can grow before
    // the list is read, so leave room and retry whenever the list filled it.
    // SAFETY: a null buffer of length zero is the documented size query.
    let needed = unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, std::ptr::null_mut(), 0) };
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
            unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), bytes) };
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
            // SAFETY: geteuid has no preconditions and no failure state.
            let own_user = uid == unsafe { libc::geteuid() };
            if own_user && !pids.contains(&std::process::id()) {
                return Err(io::Error::other(
                    "the process table left out this process, so it cannot be trusted",
                ));
            }
            return Ok(pids);
        }
        capacity *= 2;
    }
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
