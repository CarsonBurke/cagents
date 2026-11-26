use super::{PinnedProcess, Process, Survivor, command_for_display};
use crate::process::table::{self, Record};
use std::{io, os::unix::ffi::OsStrExt, path::Path, time::Duration};

/// A reused leader PID excludes its new group or session. An absent leader
/// leaves the numeric boundary visible, but only the original leader can
/// authenticate it. A successor could have used the PID and exited too.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leader {
    Original,
    Unknown,
    Reused,
}

fn read(pid: i32) -> io::Result<Option<Record>> {
    match table::record(pid) {
        Ok(record) => Ok(Some(record)),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_error(pid: i32, error: io::Error) -> String {
    format!("Cannot read process {pid}: {error}")
}

fn leader(original: &Process) -> Result<Leader, String> {
    Ok(
        match read(original.pid)
            .map_err(|error| read_error(original.pid, error))?
            .and_then(|record| record.start)
        {
            Some(start) if start == original.start => Leader::Original,
            Some(_) => Leader::Reused,
            None => Leader::Unknown,
        },
    )
}

/// Every observable member of the recorded helper session or provider group.
/// An empty list does not prove the absence of work that left both.
pub fn census(helper: &Process, provider: &Process) -> Result<Vec<Survivor>, String> {
    let helper_leader = leader(helper)?;
    let provider_leader = leader(provider)?;
    scan(|record| {
        let in_session = helper_leader != Leader::Reused && record.session == Some(helper.pid);
        let in_group =
            provider_leader != Leader::Reused && record.bsd.pbsi_pgid as i32 == provider.pid;
        if !in_session && !in_group {
            return Ok(None);
        }
        let proved = (in_session && still_proves(record, helper, helper_leader)?)
            || (in_group && still_proves(record, provider, provider_leader)?);
        Ok(Some(survivor(record, proved)))
    })
}

/// Processes previously identified by their private listener arguments, and
/// the observable members of the process groups they lead.
pub fn listener_census(named: &[Process]) -> Result<Vec<Survivor>, String> {
    let leaders = named
        .iter()
        .map(|process| leader(process).map(|state| (process, state)))
        .collect::<Result<Vec<_>, _>>()?;
    scan(|record| {
        let mut member = false;
        for &(process, observed) in &leaders {
            if observed != Leader::Reused
                && (record.bsd.pbsi_pid as i32 == process.pid
                    || record.bsd.pbsi_pgid as i32 == process.pid)
            {
                member = true;
                if still_proves(record, process, observed)? {
                    return Ok(Some(survivor(record, true)));
                }
            }
        }
        Ok(member.then(|| survivor(record, false)))
    })
}

/// Positive candidates for a private Codex listener. Restricted processes or
/// unpublished argv can hide a match, so an empty result grants no close or
/// launch authority.
pub fn listener_providers(
    pane: crate::model::TerminalId,
    socket: &Path,
) -> Result<Vec<Process>, String> {
    let uid = crate::daemon::current_uid();
    let pids = table::list(uid).map_err(|error| format!("Cannot enumerate processes: {error}"))?;
    let mut marker = b"cagents-codex-".to_vec();
    marker.extend_from_slice(pane.to_string().as_bytes());
    let mut endpoint = b"unix://".to_vec();
    endpoint.extend_from_slice(socket.as_os_str().as_bytes());
    let mut joined = b"--listen=".to_vec();
    joined.extend_from_slice(&endpoint);
    let mut candidates = Vec::new();
    for pid in pids {
        let Ok(pin) = PinnedProcess::open(pid) else {
            continue;
        };
        if pin.uid().ok() != Some(uid) {
            continue;
        }
        let Ok(argv) = pin.command_line() else {
            continue;
        };
        if argv.windows(marker.len()).any(|window| window == marker) {
            let arguments = argv.split(|byte| *byte == 0).collect::<Vec<_>>();
            if arguments.contains(&joined.as_slice())
                || arguments
                    .windows(2)
                    .any(|pair| pair[0] == b"--listen" && pair[1] == endpoint)
            {
                candidates.push(Process {
                    pid: pid as i32,
                    start: pin.started_ticks(),
                });
            }
        }
    }
    Ok(candidates)
}

fn still_proves(record: &Record, original: &Process, before: Leader) -> Result<bool, String> {
    Ok(before == Leader::Original
        && record.bsd.pbsi_ruid == crate::daemon::current_uid()
        && record.start.is_some_and(|start| start >= original.start)
        // The original must have held its PID across the candidate read.
        && leader(original)? == Leader::Original)
}

fn scan(
    member: impl FnMut(&Record) -> Result<Option<Survivor>, String>,
) -> Result<Vec<Survivor>, String> {
    let pids = table::list_all()
        .map_err(|error| format!("Cannot enumerate processes for exact absence: {error}"))?;
    scan_records(pids.into_iter().map(|pid| pid as i32), read, member)
}

fn scan_records(
    pids: impl IntoIterator<Item = i32>,
    mut read: impl FnMut(i32) -> io::Result<Option<Record>>,
    mut member: impl FnMut(&Record) -> Result<Option<Survivor>, String>,
) -> Result<Vec<Survivor>, String> {
    let mut found = Vec::new();
    let mut pending = Vec::new();
    let mut visit = |pid| match read(pid) {
        Ok(Some(record)) => {
            found.extend(member(&record)?);
            Ok(false)
        }
        Ok(None) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(read_error(pid, error)),
    };
    for pid in pids {
        if visit(pid)? {
            pending.push(pid);
        }
    }
    // XNU can change a row between BSD, session, and Mach reads. Revisit
    // only those rows, after a brief settle, instead of restarting the table
    // among unrelated process churn. A row that remains uncertain refuses.
    for delay in [5, 10, 20, 40] {
        if pending.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(delay));
        let mut still_pending = Vec::new();
        for pid in pending {
            if visit(pid)? {
                still_pending.push(pid);
            }
        }
        pending = still_pending;
    }
    if let Some(pid) = pending.first() {
        return Err(format!(
            "Cannot read process {pid}: process changed during census"
        ));
    }
    found.sort_by_key(|survivor| survivor.pid);
    Ok(found)
}

fn survivor(record: &Record, proved: bool) -> Survivor {
    let pid = record.bsd.pbsi_pid;
    let zombie = record.bsd.pbsi_status == libc::SZOMB;
    let name = record
        .bsd
        .pbsi_comm
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect::<Vec<_>>();
    let command = record.start.filter(|_| !zombie).and_then(|start| {
        let pin = PinnedProcess::open_started(pid, start).ok()?;
        pin.command_line()
            .ok()
            .map(|line| command_for_display(&line))
    });
    Survivor {
        pid: pid as i32,
        start: record.start.unwrap_or_default(),
        group: record.bsd.pbsi_pgid as i32,
        // getsid cannot read a zombie's session. Its process group from the
        // BSD record still locates it, and it is never a signal target.
        session: record.session.unwrap_or_default(),
        zombie,
        name: String::from_utf8_lossy(&name).into_owned(),
        command: command.unwrap_or_default(),
        proved,
    }
}

pub(super) fn pin_matching(survivor: &Survivor) -> Result<Option<PinnedProcess>, String> {
    let process = PinnedProcess::open_started(survivor.pid as u32, survivor.start)
        .map_err(|error| format!("Cannot hold process {}: {error}", survivor.pid))?;
    let still_matches = read(survivor.pid)
        .map_err(|error| read_error(survivor.pid, error))?
        .is_some_and(|record| {
            record.start == Some(survivor.start)
                && record.bsd.pbsi_pgid as i32 == survivor.group
                && record.bsd.pbsi_ruid == crate::daemon::current_uid()
                && record.session == Some(survivor.session)
                && record.bsd.pbsi_status != libc::SZOMB
        });
    Ok(still_matches.then_some(process))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn changing_process_rows_settle_without_restarting_the_table() {
        let mut reads = Vec::new();
        let found = scan_records(
            [11, 12],
            |pid| {
                reads.push(pid);
                if pid == 11 && reads.len() == 1 {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock));
                }
                Ok(None)
            },
            |_| panic!("absent rows cannot describe a survivor"),
        )
        .unwrap();
        assert!(found.is_empty());
        assert_eq!(reads, [11, 12, 11]);

        let error = scan_records(
            [13],
            |_| Err(io::Error::from(io::ErrorKind::WouldBlock)),
            |_| panic!("unsettled rows cannot describe a survivor"),
        )
        .unwrap_err();
        assert!(error.contains("process 13"), "{error}");
    }

    #[test]
    fn observes_an_exact_session_and_refuses_a_reused_leader() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "read line"]).stdin(Stdio::piped());
        // SAFETY: setsid is async-signal-safe. The test only controls its own
        // child, and the new session excludes the other test processes.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = ChildGuard(command.spawn().unwrap());
        let root = Process::read(child.0.id() as i32).unwrap();
        let found = census(&root, &root).unwrap();
        let [survivor] = found.as_slice() else {
            panic!("unexpected session census: {found:?}");
        };
        assert_eq!(survivor.pid, root.pid);
        assert_eq!(survivor.start, root.start);
        assert!(survivor.proved && !survivor.zombie);
        assert!(pin_matching(survivor).unwrap().is_some());

        let earlier = Process {
            pid: root.pid,
            start: root.start - 1,
        };
        assert!(census(&earlier, &earlier).unwrap().is_empty());
        assert!(listener_census(&[earlier]).unwrap().is_empty());
    }

    #[test]
    fn discovers_a_private_listener_by_its_exact_arguments() {
        let pane = crate::model::TerminalId::new();
        let socket = format!("/tmp/cagents-codex-{pane}/server");
        let endpoint = format!("unix://{socket}");
        let child = ChildGuard(
            Command::new("/bin/sh")
                .args(["-c", "read line", "provider", "--listen", &endpoint])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let process = Process::read(child.0.id() as i32).unwrap();
        let recorded = listener_providers(pane, Path::new(&socket)).unwrap();
        assert!(recorded.contains(&process));
        assert!(
            listener_census(&recorded)
                .unwrap()
                .iter()
                .any(|survivor| survivor.pid == process.pid
                    && survivor.start == process.start
                    && survivor.proved)
        );
        assert!(
            listener_providers(pane, Path::new(&format!("{socket}-other")))
                .unwrap()
                .is_empty()
        );
    }
}
