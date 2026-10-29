//! Read-only, per-removal observations. Current ancestry never proves that the
//! original family is complete: an unseen child may already have reparented.
use super::{CleanupRefusal, Process, check_directory, uncertain};
use crate::model::BootId;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_PROCESSES: usize = 4096;
const MAX_TASKS: usize = 4096;
const MAX_READ: usize = 65536;
const PASSES: usize = 3;

// Local observation identity only; do not change persisted holder authority.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Identity {
    boot: BootId,
    pid: i32,
    start: u64,
}

impl Identity {
    pub(super) fn new(boot: BootId, process: Process) -> Self {
        Self {
            boot,
            pid: process.pid,
            start: process.start,
        }
    }
}

/// The observer can read evidence only. It has no process-control or removal
/// operation, and observation never grants migration or adoption authority.
trait Observer {
    fn boot(&mut self) -> io::Result<BootId>;
    fn stat(&mut self, pid: i32) -> io::Result<Vec<u8>>;
    fn cwd(&mut self, pid: i32) -> io::Result<PathBuf>;
    fn children(&mut self, pid: i32) -> io::Result<Vec<i32>>;
}

pub(super) struct Procfs;

fn bounded_read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    read_limited(fs::File::open(path)?)
}

fn read_limited(reader: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take((MAX_READ + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_READ {
        return Err(io::Error::other("process evidence exceeded its read bound"));
    }
    Ok(bytes)
}

fn collect_children<T>(
    tasks: impl IntoIterator<Item = io::Result<T>>,
    mut read: impl FnMut(T) -> io::Result<Vec<u8>>,
) -> io::Result<Vec<i32>> {
    let mut children = BTreeSet::new();
    let mut task_count = 0;
    for (index, task) in tasks.into_iter().enumerate() {
        if index >= MAX_TASKS {
            return Err(io::Error::other(
                "process task census exceeded its safety bound",
            ));
        }
        task_count += 1;
        // Missing tasks and children are incomplete evidence, not absence.
        for child in parse_children(&read(task?)?)? {
            children.insert(child);
            if children.len() > MAX_PROCESSES {
                return Err(io::Error::other(
                    "process child census exceeded its safety bound",
                ));
            }
        }
    }
    if task_count == 0 {
        return Err(io::Error::other("process task census is empty"));
    }
    Ok(children.into_iter().collect())
}

fn parse_children(bytes: &[u8]) -> io::Result<Vec<i32>> {
    let mut children = Vec::new();
    for field in std::str::from_utf8(bytes)
        .map_err(io::Error::other)?
        .split_whitespace()
    {
        let pid = field.parse::<i32>().map_err(io::Error::other)?;
        if pid <= 0 || children.len() >= MAX_PROCESSES {
            return Err(io::Error::other("invalid or unbounded child list"));
        }
        children.push(pid);
    }
    Ok(children)
}

impl Observer for Procfs {
    fn boot(&mut self) -> io::Result<BootId> {
        crate::daemon::read_boot_id()
    }

    fn stat(&mut self, pid: i32) -> io::Result<Vec<u8>> {
        bounded_read(format!("/proc/{pid}/stat"))
    }

    fn cwd(&mut self, pid: i32) -> io::Result<PathBuf> {
        fs::read_link(format!("/proc/{pid}/cwd"))
    }

    fn children(&mut self, pid: i32) -> io::Result<Vec<i32>> {
        collect_children(
            fs::read_dir(format!("/proc/{pid}/task"))?
                .map(|task| Ok(task?.path().join("children"))),
            bounded_read,
        )
    }
}

fn read_identity(observer: &mut impl Observer, pid: i32) -> io::Result<Identity> {
    if pid <= 0 {
        return Err(io::Error::other("invalid process PID"));
    }
    let boot = observer.boot()?;
    let bytes = observer.stat(pid)?;
    let stat = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let (prefix, fields) = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("invalid process stat"))?;
    let (actual_pid, _) = prefix
        .split_once('(')
        .ok_or_else(|| io::Error::other("invalid process PID"))?;
    if actual_pid.trim().parse::<i32>().map_err(io::Error::other)? != pid {
        return Err(io::Error::other("process stat PID changed"));
    }
    let state = fields
        .split_whitespace()
        .next()
        .ok_or_else(|| io::Error::other("missing process state"))?;
    if state.len() != 1 || !b"RSDZTtXxKWPI".contains(&state.as_bytes()[0]) {
        return Err(io::Error::other("invalid process state"));
    }
    let (_, start) = crate::model::agent_recovery::process_identity(&bytes)
        .ok_or_else(|| io::Error::other("invalid process identity"))?;
    Ok(Identity { boot, pid, start })
}

fn verify(observer: &mut impl Observer, expected: &Identity) -> io::Result<()> {
    if read_identity(observer, expected.pid)? != *expected {
        return Err(io::Error::other("process identity changed"));
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct Census {
    retained: BTreeSet<Identity>,
}

impl Census {
    pub(super) fn retain(&mut self, identity: Identity) -> Result<(), CleanupRefusal> {
        if identity.pid <= 0 || identity.boot.as_str().is_empty() {
            return Err(uncertain("Invalid hosted process identity"));
        }
        if !self.retained.contains(&identity) && self.retained.len() >= MAX_PROCESSES {
            return Err(uncertain("Hosted process census exceeded its safety bound"));
        }
        // Retain every incarnation; a new start time never overwrites the old.
        self.retained.insert(identity);
        Ok(())
    }

    fn pass(
        &mut self,
        observer: &mut impl Observer,
        candidate: &Path,
    ) -> Result<(), CleanupRefusal> {
        let observation_error = |error| {
            uncertain(format!(
                "Cannot establish hosted process ownership: {error}"
            ))
        };
        let mut pending = self.retained.clone();
        let mut inspected = BTreeSet::new();
        while let Some(identity) = pending.pop_first() {
            if !inspected.insert(identity.clone()) {
                continue;
            }
            verify(observer, &identity).map_err(observation_error)?;
            let cwd = observer.cwd(identity.pid).map_err(observation_error)?;
            let directory_result = check_directory(&cwd, candidate);
            verify(observer, &identity).map_err(observation_error)?;
            directory_result?;
            let children = observer.children(identity.pid).map_err(observation_error)?;
            if children.len() > MAX_PROCESSES {
                return Err(uncertain("Hosted child census exceeded its safety bound"));
            }
            for pid in children {
                let child = read_identity(observer, pid).map_err(observation_error)?;
                if child.boot != identity.boot {
                    return Err(uncertain(
                        "Hosted process boot identity changed during cleanup",
                    ));
                }
                self.retain(child.clone())?;
                if !inspected.contains(&child) {
                    pending.insert(child);
                }
            }
            verify(observer, &identity).map_err(observation_error)?;
        }
        Ok(())
    }

    pub(super) fn check(
        &mut self,
        observer: &mut Procfs,
        candidate: &Path,
    ) -> Result<(), CleanupRefusal> {
        self.check_observed(observer, candidate)
    }

    fn check_observed(
        &mut self,
        observer: &mut impl Observer,
        candidate: &Path,
    ) -> Result<(), CleanupRefusal> {
        for _ in 0..PASSES {
            self.pass(observer, candidate)?;
        }
        // Equal current trees, empty sets and retained exact identities all
        // leave pre-observation escapes unknown. They cannot grant removal.
        Err(uncertain(
            "Original hosted process family completeness is unknown",
        ))
    }
}

#[cfg(test)]
mod tests {
    // Scripted read-only observations: no provider, process or signal fixture.
    use super::*;
    use crate::device_service::CleanupRefusalKind as Reason;
    use std::{collections::BTreeMap, os::unix::fs::MetadataExt};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ReadKind {
        Boot,
        Stat,
        Cwd,
        Children,
    }

    struct Script {
        boot: BootId,
        starts: BTreeMap<i32, u64>,
        outside: PathBuf,
        candidate: PathBuf,
        root_children: Vec<Vec<i32>>,
        calls: Vec<(ReadKind, i32)>,
        child_cwd_reads: usize,
        child_enters_candidate: bool,
        root_children_reads: usize,
        fail: Option<(ReadKind, usize, io::ErrorKind)>,
        changed_stat: Option<usize>,
        changed_boot: Option<usize>,
        malformed_stat: Option<Vec<u8>>,
    }

    impl Script {
        fn new(outside: PathBuf, candidate: PathBuf) -> Self {
            Self {
                boot: BootId::new("boot"),
                starts: [(10, 1), (20, 2)].into(),
                outside,
                candidate,
                root_children: vec![vec![]],
                calls: Vec::new(),
                child_cwd_reads: 0,
                child_enters_candidate: false,
                root_children_reads: 0,
                fail: None,
                changed_stat: None,
                changed_boot: None,
                malformed_stat: None,
            }
        }

        fn read(&mut self, kind: ReadKind, pid: i32) -> io::Result<usize> {
            self.calls.push((kind, pid));
            let count = self
                .calls
                .iter()
                .filter(|(observed, _)| *observed == kind)
                .count();
            if let Some((expected, at, error)) = self.fail
                && expected == kind
                && count == at
            {
                return Err(io::Error::from(error));
            }
            Ok(count)
        }
    }

    fn stat(pid: i32, start: u64) -> Vec<u8> {
        // Fields after state: ppid, pgrp, then 16 fields before starttime.
        let mut fields = vec!["S".to_owned(), "1".into(), "1".into()];
        fields.extend(std::iter::repeat_n("0".to_owned(), 16));
        fields.push(start.to_string());
        format!("{pid} (fixture) {}", fields.join(" ")).into_bytes()
    }

    impl Observer for Script {
        fn boot(&mut self) -> io::Result<BootId> {
            let count = self.read(ReadKind::Boot, 0)?;
            Ok(if self.changed_boot.is_some_and(|at| count >= at) {
                BootId::new("rebooted")
            } else {
                self.boot.clone()
            })
        }

        fn stat(&mut self, pid: i32) -> io::Result<Vec<u8>> {
            let count = self.read(ReadKind::Stat, pid)?;
            if let Some(stat) = &self.malformed_stat {
                return Ok(stat.clone());
            }
            let start = self
                .starts
                .get(&pid)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            Ok(stat(
                pid,
                start + u64::from(self.changed_stat.is_some_and(|at| count >= at)),
            ))
        }

        fn cwd(&mut self, pid: i32) -> io::Result<PathBuf> {
            self.read(ReadKind::Cwd, pid)?;
            if pid == 20 {
                self.child_cwd_reads += 1;
                if self.child_enters_candidate && self.child_cwd_reads >= 2 {
                    return Ok(self.candidate.clone());
                }
            }
            Ok(self.outside.clone())
        }

        fn children(&mut self, pid: i32) -> io::Result<Vec<i32>> {
            self.read(ReadKind::Children, pid)?;
            if pid != 10 {
                return Ok(Vec::new());
            }
            let index = self.root_children_reads.min(self.root_children.len() - 1);
            self.root_children_reads += 1;
            Ok(self.root_children[index].clone())
        }
    }

    fn identity(pid: i32, start: u64) -> Identity {
        Identity::new(BootId::new("boot"), Process { pid, start })
    }

    fn fixture() -> (tempfile::TempDir, Script, Census) {
        let directory = tempfile::tempdir().unwrap();
        let candidate = directory.path().join("candidate");
        let outside = directory.path().join("outside");
        fs::create_dir(&candidate).unwrap();
        fs::create_dir(&outside).unwrap();
        let mut census = Census::default();
        census.retain(identity(10, 1)).unwrap();
        (directory, Script::new(outside, candidate), census)
    }

    fn refused_without_removal(census: &mut Census, observer: &mut Script, reason: Reason) {
        let sentinel = observer.candidate.join("sentinel");
        fs::write(&sentinel, b"retained candidate").unwrap();
        let before = fs::metadata(&sentinel).unwrap();
        let candidate = observer.candidate.clone();
        let mut removal_continuations = 0;
        let result = census.check_observed(observer, &candidate).map(|()| {
            removal_continuations += 1;
        });
        assert_eq!(result.unwrap_err().kind, reason);
        assert_eq!(removal_continuations, 0);
        assert_eq!(fs::read(&sentinel).unwrap(), b"retained candidate");
        let after = fs::metadata(&sentinel).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
    }

    #[test]
    fn known_child_escaped_after_first_pass_is_still_observed() {
        let (_directory, mut observer, mut census) = fixture();
        observer.root_children = vec![vec![20], vec![]];
        observer.child_enters_candidate = true;
        let candidate = observer.candidate.clone();
        census.pass(&mut observer, &candidate).unwrap();
        assert_eq!(observer.child_cwd_reads, 1);
        assert_eq!(census.retained, [identity(10, 1), identity(20, 2)].into());
        refused_without_removal(&mut census, &mut observer, Reason::BusySession);
        assert_eq!(observer.child_cwd_reads, 2);
        assert_eq!(observer.root_children_reads, 2);
        assert!(census.retained.contains(&identity(20, 2)));
    }

    #[test]
    fn pre_first_observation_escape_and_equal_or_empty_sets_never_prove_absence() {
        let (_directory, mut observer, mut census) = fixture();
        refused_without_removal(&mut census, &mut observer, Reason::OwnershipUncertain);
        assert_eq!(observer.root_children_reads, PASSES);
        assert_eq!(census.retained, [identity(10, 1)].into());
        let mut empty = Census::default();
        let previous_reads = observer.calls.len();
        refused_without_removal(&mut empty, &mut observer, Reason::OwnershipUncertain);
        assert_eq!(observer.calls.len(), previous_reads);
        assert!(empty.retained.is_empty());
    }

    #[test]
    fn detached_known_root_is_checked_even_when_holder_has_no_children() {
        let (_directory, mut observer, mut census) = fixture();
        // Model the independently authenticated original root of holder 10.
        census.retain(identity(20, 2)).unwrap();
        observer.child_enters_candidate = true;
        observer.child_cwd_reads = 1;
        refused_without_removal(&mut census, &mut observer, Reason::BusySession);
        assert_eq!(observer.root_children_reads, 1);
        assert!(observer.calls.contains(&(ReadKind::Cwd, 10)));
        assert!(observer.calls.contains(&(ReadKind::Cwd, 20)));
        assert_eq!(census.retained, [identity(10, 1), identity(20, 2)].into());
    }

    #[test]
    fn pid_reuse_keeps_both_incarnations_and_refuses_the_old_one() {
        let (_directory, mut observer, mut census) = fixture();
        census.retain(identity(20, 1)).unwrap();
        observer.root_children = vec![vec![20]];
        refused_without_removal(&mut census, &mut observer, Reason::OwnershipUncertain);
        assert!(census.retained.contains(&identity(20, 1)));
        assert!(census.retained.contains(&identity(20, 2)));
        assert!(!observer.calls.contains(&(ReadKind::Cwd, 20)));
    }

    #[test]
    fn identity_is_checked_before_and_after_cwd_and_traversal() {
        for at in 1..=3 {
            let (_directory, mut observer, mut census) = fixture();
            observer.changed_stat = Some(at);
            let candidate = observer.candidate.clone();
            // Require this pass to fail: a later pass's initial identity read
            // must not conceal a missing final traversal check.
            assert_eq!(
                census.pass(&mut observer, &candidate).unwrap_err().kind,
                Reason::OwnershipUncertain
            );
            assert_eq!(census.retained, [identity(10, 1)].into());
            assert_eq!(
                observer
                    .calls
                    .iter()
                    .filter(|(kind, _)| *kind == ReadKind::Cwd)
                    .count(),
                usize::from(at > 1)
            );
            assert_eq!(observer.root_children_reads, usize::from(at > 2));
            assert_eq!(observer.calls.last(), Some(&(ReadKind::Stat, 10)));
            refused_without_removal(&mut census, &mut observer, Reason::OwnershipUncertain);
        }
        let (_directory, mut observer, mut census) = fixture();
        observer.changed_boot = Some(2);
        let candidate = observer.candidate.clone();
        assert_eq!(
            census.pass(&mut observer, &candidate).unwrap_err().kind,
            Reason::OwnershipUncertain
        );
        assert_eq!(observer.calls.last(), Some(&(ReadKind::Stat, 10)));
        assert_eq!(census.retained, [identity(10, 1)].into());
        refused_without_removal(&mut census, &mut observer, Reason::OwnershipUncertain);
    }

    #[test]
    fn missing_denied_and_incomplete_observations_refuse_without_losing_identity() {
        for error in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::UnexpectedEof,
        ] {
            for (kind, at) in [
                (ReadKind::Boot, 1),
                (ReadKind::Stat, 1),
                (ReadKind::Cwd, 1),
                (ReadKind::Stat, 2),
                (ReadKind::Children, 1),
                (ReadKind::Stat, 3),
            ] {
                let (_directory, mut observer, mut census) = fixture();
                observer.fail = Some((kind, at, error));
                let candidate = observer.candidate.clone();
                assert_eq!(
                    census.pass(&mut observer, &candidate).unwrap_err().kind,
                    Reason::OwnershipUncertain
                );
                assert_eq!(
                    observer.calls.last(),
                    Some(&(kind, if kind == ReadKind::Boot { 0 } else { 10 }))
                );
                assert_eq!(
                    observer
                        .calls
                        .iter()
                        .filter(|(observed, _)| *observed == kind)
                        .count(),
                    at
                );
                assert_eq!(census.retained, [identity(10, 1)].into());
                refused_without_removal(&mut census, &mut observer, Reason::OwnershipUncertain);
            }
            let (_directory, mut observer, mut census) = fixture();
            observer.root_children = vec![vec![20]];
            // Third stat is the newly observed child's identity.
            observer.fail = Some((ReadKind::Stat, 3, error));
            let candidate = observer.candidate.clone();
            assert_eq!(
                census.pass(&mut observer, &candidate).unwrap_err().kind,
                Reason::OwnershipUncertain
            );
            assert_eq!(observer.calls.last(), Some(&(ReadKind::Stat, 20)));
            assert_eq!(
                observer
                    .calls
                    .iter()
                    .filter(|(kind, _)| *kind == ReadKind::Stat)
                    .count(),
                3
            );
            assert!(census.retained.contains(&identity(10, 1)));
            refused_without_removal(&mut census, &mut observer, Reason::OwnershipUncertain);
        }
    }

    #[test]
    fn malformed_stat_pid_state_and_child_tokens_are_not_absence() {
        for bytes in [
            b"malformed".to_vec(),
            stat(99, 1),
            stat(10, 1)
                .split(|byte| *byte == b'S')
                .collect::<Vec<_>>()
                .join(&b'?'),
        ] {
            let (_directory, mut observer, mut census) = fixture();
            observer.malformed_stat = Some(bytes);
            let candidate = observer.candidate.clone();
            assert_eq!(
                census.pass(&mut observer, &candidate).unwrap_err().kind,
                Reason::OwnershipUncertain
            );
            assert_eq!(observer.calls, [(ReadKind::Boot, 0), (ReadKind::Stat, 10)]);
            assert!(census.retained.contains(&identity(10, 1)));
            refused_without_removal(&mut census, &mut observer, Reason::OwnershipUncertain);
        }
        for bytes in [b"garbage".as_slice(), b"0", b"-1", b"20 invalid", b"\xff"] {
            assert!(parse_children(bytes).is_err());
        }
        for pid in [0, -1] {
            assert!(Census::default().retain(identity(pid, 1)).is_err());
        }
    }

    #[test]
    fn observation_and_retention_bounds_and_incomplete_task_iteration_refuse() {
        assert!(read_limited(io::Cursor::new(vec![0; MAX_READ + 1])).is_err());
        assert!(
            collect_children(
                std::iter::repeat_with(|| Ok(())).take(MAX_TASKS + 1),
                |_| Ok(Vec::new())
            )
            .is_err()
        );
        assert!(
            collect_children(std::iter::empty::<io::Result<()>>(), |_| Ok(Vec::new())).is_err()
        );
        for error in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert!(
                collect_children(
                    [Ok(()), Err(io::Error::from(error))],
                    |_| Ok(b"20".to_vec())
                )
                .is_err()
            );
            assert!(collect_children([Ok(())], |_| Err(io::Error::from(error))).is_err());
        }
        let child_bytes = std::iter::repeat_n("20", MAX_PROCESSES + 1)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(parse_children(child_bytes.as_bytes()).is_err());
        let (_directory, mut observer, mut census) = fixture();
        for pid in 100..(100 + MAX_PROCESSES as i32 - 1) {
            census.retain(identity(pid, 1)).unwrap();
        }
        let old = census.retained.clone();
        assert_eq!(
            census.retain(identity(9000, 1)).unwrap_err().kind,
            Reason::OwnershipUncertain
        );
        assert_eq!(census.retained, old);
        observer.root_children = vec![vec![20; MAX_PROCESSES + 1]];
        refused_without_removal(&mut census, &mut observer, Reason::OwnershipUncertain);
        assert_eq!(census.retained, old);
    }
}
