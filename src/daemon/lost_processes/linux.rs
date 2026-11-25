use super::{PinnedProcess, Process, Survivor, command_for_display};
use std::path::Path;

/// What `/proc/<pid>/stat` says of one process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    zombie: bool,
    group: i32,
    session: i32,
    start: u64,
}

/// Reads one process's stat entry. `None` when it has gone.
fn read_stat(process: &Path) -> Result<Option<Stat>, String> {
    let pid = process.display();
    let stat = match std::fs::read(process.join("stat")) {
        Ok(stat) => stat,
        // A process that exited during the scan holds no group: its entry is
        // gone, or was reaped between opening and reading it.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("Cannot read process {pid}: {error}")),
    };
    let Some(close) = stat.iter().rposition(|&byte| byte == b')') else {
        return Err(format!("Process {pid} has an unreadable identity"));
    };
    let fields = String::from_utf8_lossy(&stat[close + 1..]);
    let fields: Vec<&str> = fields.split_ascii_whitespace().collect();
    let field = |index: usize| fields.get(index).copied();
    let (Some(state), Some(group), Some(session)) = (
        field(0),
        field(2).and_then(|field| field.parse::<i32>().ok()),
        field(3).and_then(|field| field.parse::<i32>().ok()),
    ) else {
        return Err(format!("Process {pid} has an unreadable group identity"));
    };
    Ok(Some(Stat {
        zombie: state == "Z" || state == "X",
        group,
        session,
        // A test's synthetic entry may stop after the session.
        start: field(19)
            .and_then(|field| field.parse().ok())
            .unwrap_or_default(),
    }))
}

/// Whether the ID `original.pid` names still belongs to `original`: it is
/// still that process, or no process has that PID, so nothing has reused it.
fn id_kept(proc: &Path, original: &Process) -> Result<bool, String> {
    Ok(match read_stat(&proc.join(original.pid.to_string()))? {
        None => true,
        Some(stat) => stat.start == original.start,
    })
}

/// Every process in `helper`'s session or `provider`'s group, proved or not.
pub fn census(helper: &Process, provider: &Process) -> Result<Vec<Survivor>, String> {
    census_in(Path::new("/proc"), helper, provider)
}

pub(super) fn census_in(
    proc: &Path,
    helper: &Process,
    provider: &Process,
) -> Result<Vec<Survivor>, String> {
    let session_kept = id_kept(proc, helper)?;
    let group_kept = id_kept(proc, provider)?;
    let uid = crate::daemon::current_uid();
    scan(proc, |process, pid, stat| {
        // A reused ID's session or group is its new owner's.
        let in_session = session_kept && stat.session == helper.pid;
        let in_group = group_kept && stat.group == provider.pid;
        if !in_session && !in_group {
            return Ok(None);
        }
        let proved = owned_by(process, uid)?
            && ((in_session && stat.start >= helper.start)
                || (in_group && stat.start >= provider.start));
        Ok(Some(survivor(process, pid, stat, proved)))
    })
}

/// Every process proved to be `pane`'s Codex provider because its arguments
/// name the conversation's own listener directory, and the members of each
/// one's process group when it leads it. `named` lists those providers.
pub fn listener_census(named: &[Process]) -> Result<Vec<Survivor>, String> {
    let proc = Path::new("/proc");
    let uid = crate::daemon::current_uid();
    scan(proc, |process, pid, stat| {
        let provider = named
            .iter()
            .find(|named| named.pid == pid || named.pid == stat.group);
        let Some(provider) = provider else {
            return Ok(None);
        };
        let proved = owned_by(process, uid)?
            && stat.start >= provider.start
            && (pid != provider.pid || stat.start == provider.start)
            && id_kept(proc, provider)?;
        Ok(Some(survivor(process, pid, stat, proved)))
    })
}

/// Whether `process` runs as `uid`, by its real user ID, which is what
/// permits signalling it. Its `/proc` directory's owner is no proof: a
/// process that made itself undumpable, as ssh does, shows root there.
fn owned_by(process: &Path, uid: u32) -> Result<bool, String> {
    let status = match std::fs::read_to_string(process.join("status")) {
        Ok(status) => status,
        // Gone since it was listed: nothing to prove or end.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(format!(
                "Cannot read process {}: {error}",
                process.display()
            ));
        }
    };
    Ok(status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_ascii_whitespace().next())
        .and_then(|real| real.parse::<u32>().ok())
        == Some(uid))
}

fn scan(
    proc: &Path,
    mut member: impl FnMut(&Path, i32, Stat) -> Result<Option<Survivor>, String>,
) -> Result<Vec<Survivor>, String> {
    let entries = std::fs::read_dir(proc)
        .map_err(|error| format!("Cannot enumerate processes for exact absence: {error}"))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("Cannot enumerate processes: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let process = entry.path();
        let Some(stat) = read_stat(&process)? else {
            continue;
        };
        if let Some(survivor) = member(&process, pid, stat)? {
            found.push(survivor);
        }
    }
    found.sort_by_key(|survivor| survivor.pid);
    Ok(found)
}

fn survivor(process: &Path, pid: i32, stat: Stat, proved: bool) -> Survivor {
    let name = std::fs::read_to_string(process.join("comm"))
        .map(|name| name.trim_end().to_owned())
        .unwrap_or_default();
    let command = std::fs::read(process.join("cmdline"))
        .map(|argv| command_for_display(&argv))
        .unwrap_or_default();
    Survivor {
        pid,
        start: stat.start,
        group: stat.group,
        session: stat.session,
        zombie: stat.zombie,
        name,
        command,
        proved,
    }
}

pub(super) fn pin_matching(survivor: &Survivor) -> Result<Option<PinnedProcess>, String> {
    let process = match PinnedProcess::open_unreaped(survivor.pid as u32) {
        Ok(process) => process,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot hold process {}: {error}", survivor.pid)),
    };
    // Read after pinning: only the same process can still show the same
    // start time under this PID, so the pin names the survivor.
    let same = read_stat(&Path::new("/proc").join(survivor.pid.to_string()))?.is_some_and(|stat| {
        stat.start == survivor.start
            && stat.group == survivor.group
            && stat.session == survivor.session
            && !stat.zombie
    });
    Ok(same.then_some(process))
}
