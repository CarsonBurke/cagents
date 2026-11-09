//! When and where each terminal lineage was born, so a process whose
//! environment can't be read can still be ruled out of one.
//!
//! The process scan ties a process to a terminal through the markers every
//! terminal launch sets in its environment. A process that made itself
//! undumpable (ssh-agent) or changed credentials (an SSH login's
//! `sshd-session`) doesn't show its environment, so the scan can't read them.
//! Two facts about the lineage can rule such a process out without them:
//!
//! - A process started before the lineage's first launch on this boot can't
//!   descend from it. Every launch records the time before its holder forks,
//!   and the kernel dates each process from its fork on the same clock.
//! - Every process the lineage starts is born in the cgroup of the daemon that
//!   launched it. When that is inside the user manager's tree, nothing the
//!   lineage starts can move out of it: the user can't write the root-owned
//!   cgroups beside it, such as an SSH login's `session-N.scope`.
//!
//! A launch is recorded under both lineages its processes are marked with,
//! its own terminal and its recovery chain's origin, and a scan reads both.
//!
//! Both facts need every launch on this boot to be recorded. The first daemon
//! of an instance on a boot opens an epoch before launching anything. A daemon
//! that isn't the first, or that finds launches of this boot on record without
//! an epoch (an older version, or one before the end of the last login), opens
//! none, and the records then prove nothing until the next boot. A launch that
//! can't be recorded closes the epoch for the rest of the boot.
//!
//! What this doesn't see, each of which can make the scan pass over an
//! unreadable process that is the lineage's:
//!
//! - a version without these records started after a newer one on the same
//!   boot and instance;
//! - root moving a lineage's process into a root-owned cgroup on its behalf
//!   (`su` through `pam_systemd`, or a launch recipe that runs a system-level
//!   `systemd-run --scope`);
//! - a daemon and a scanner of one instance that don't share a state
//!   directory, or a daemon that runs with a fresh runtime directory but an
//!   existing state directory, and so believes it is the boot's first;
//! - a daemon and a scanner in different time namespaces.
//!
//! Close then removes a pane and Retry starts a terminal beside that process;
//! neither signals it, and an agent conversation is still found by its
//! arguments, which are always readable.
use crate::model::{BootId, TerminalId};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// What the scan may assume about one lineage on the current boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LineageBirth {
    /// No launch of this lineage has happened on this boot.
    Unborn,
    Born {
        /// Clock ticks after boot, before the first launch on this boot.
        since: u64,
        /// The cgroup namespace every launch was recorded in, when every launch
        /// was inside the user manager's tree. `None` when any wasn't.
        contained_in: Option<String>,
    },
}

impl LineageBirth {
    /// What holds for processes of either lineage.
    pub(crate) fn either(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unborn, birth) | (birth, Self::Unborn) => birth,
            (
                Self::Born {
                    since,
                    contained_in,
                },
                Self::Born {
                    since: other_since,
                    contained_in: other_contained_in,
                },
            ) => Self::Born {
                since: since.min(other_since),
                contained_in: contained_in
                    .filter(|namespace| other_contained_in.as_ref() == Some(namespace)),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Epoch {
    boot: BootId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    boot: BootId,
    since: u64,
    /// See [`LineageBirth::Born::contained_in`].
    contained_in: Option<String>,
}

/// The ledger of one instance, under its state directory.
#[derive(Debug, Clone)]
pub(crate) struct Ledger {
    directory: PathBuf,
}

const EPOCH: &str = "epoch";
/// The epoch, renamed once a launch on its boot went unrecorded. It stays, so
/// no later daemon of that boot opens the epoch again. A rename needs no room
/// on a full disk.
#[cfg(any(feature = "host", test))]
const CLOSED: &str = "closed-epoch";
#[cfg(any(feature = "host", test))]
const LOCK: &str = ".lock";

impl Ledger {
    pub(crate) fn of_instance() -> io::Result<Self> {
        let state = crate::diagnostics::state_path()?;
        let instance = state
            .parent()
            .ok_or_else(|| io::Error::other("state directory unavailable"))?;
        Ok(Self::at(instance.join("lineages")))
    }

    pub(crate) fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn lineage(&self, origin: TerminalId) -> PathBuf {
        self.directory.join(origin.to_string())
    }

    /// Run by a daemon after it holds its instance's lease and before it
    /// launches anything. `first` says no daemon of this instance ran since its
    /// runtime directory was made, which a reboot clears. Only then, and with
    /// no launch of this boot on record, is every launch on this boot still to
    /// come. The end of the user's last login clears the runtime directory too,
    /// which is why records of this boot are kept until it ends.
    #[cfg(any(feature = "host", test))]
    pub(crate) fn open_epoch(&self, boot: &BootId, first: bool) -> io::Result<()> {
        let _lock = self.lock()?;
        let epoch_path = self.directory.join(EPOCH);
        let closed_path = self.directory.join(CLOSED);
        for (path, open) in [(&epoch_path, true), (&closed_path, false)] {
            match self.read::<Epoch>(path) {
                Ok(Some(epoch)) if epoch.boot == *boot => {
                    if !open {
                        remove_if_present(&epoch_path)?;
                    }
                    return Ok(());
                }
                Ok(_) => {}
                // It could be this boot's, closed: keep it closed for this boot.
                Err(_) => {
                    let closed = Epoch { boot: boot.clone() };
                    self.write(&closed_path, &closed)?;
                    return remove_if_present(&epoch_path);
                }
            }
        }
        // An earlier boot's records describe processes that no longer exist.
        // A record from this boot without its epoch means a daemon launched
        // something this boot that the epoch wouldn't cover, and its processes
        // can remain. It stays until the boot ends, so no later daemon that
        // believes it's the boot's first opens an epoch over it either.
        let mut launched_this_boot = false;
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            // A temporary file is a write that never finished, so the launch it
            // was for never happened.
            if name == EPOCH
                || name == CLOSED
                || path.extension().is_some_and(|extension| extension == "tmp")
            {
                remove_if_present(&path)?;
                continue;
            }
            if name == LOCK {
                continue;
            }
            match self.read::<Record>(&path) {
                Ok(Some(Record { boot: earlier, .. })) if earlier != *boot => {
                    remove_if_present(&path)?;
                }
                Ok(_) => launched_this_boot = true,
                // It could be from this boot. Date it this boot, from its start,
                // so it holds the epoch closed until the boot ends.
                Err(_) => {
                    launched_this_boot = true;
                    let record = Record {
                        boot: boot.clone(),
                        since: 0,
                        contained_in: None,
                    };
                    self.write(&path, &record)?;
                }
            }
        }
        if first && !launched_this_boot {
            self.write(&epoch_path, &Epoch { boot: boot.clone() })?;
        }
        Ok(())
    }

    /// Run before a launch, for each lineage the launch's processes are
    /// marked with. `Err` means the launch can't be counted on: the caller
    /// closes the epoch.
    #[cfg(any(feature = "host", test))]
    pub(crate) fn record_launch(
        &self,
        boot: &BootId,
        lineages: &[TerminalId],
        since: u64,
        contained_in: Option<String>,
    ) -> io::Result<()> {
        let _lock = self.lock()?;
        for &lineage in lineages {
            let path = self.lineage(lineage);
            let record = match self.read::<Record>(&path)? {
                Some(earlier) if earlier.boot == *boot => Record {
                    boot: boot.clone(),
                    since: earlier.since.min(since),
                    contained_in: earlier
                        .contained_in
                        .filter(|earlier| contained_in.as_ref() == Some(earlier)),
                },
                _ => Record {
                    boot: boot.clone(),
                    since,
                    contained_in: contained_in.clone(),
                },
            };
            self.write(&path, &record)?;
        }
        Ok(())
    }

    /// No later launch can be vouched for on the epoch's boot. Takes no lock
    /// and no room, so it works when recording didn't: launches come only
    /// from the daemon holding the lease, which opened the epoch before any.
    #[cfg(any(feature = "host", test))]
    pub(crate) fn close_epoch(&self) -> io::Result<()> {
        let epoch = self.directory.join(EPOCH);
        match fs::rename(&epoch, self.directory.join(CLOSED)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            // Without the tombstone, a later daemon of the boot that believes
            // it's the first could open an epoch again; the records it keeps
            // still stop that while there are any.
            Err(_) => remove_if_present(&epoch),
        }
    }

    /// `None` when the ledger can't vouch for every launch on `boot`.
    pub(crate) fn birth(&self, boot: &BootId, lineage: TerminalId) -> Option<LineageBirth> {
        let epoch = self.read::<Epoch>(&self.directory.join(EPOCH)).ok()??;
        if epoch.boot != *boot {
            return None;
        }
        let birth = match self.read::<Record>(&self.lineage(lineage)).ok()? {
            Some(record) if record.boot == *boot => LineageBirth::Born {
                since: record.since,
                contained_in: record.contained_in,
            },
            _ => LineageBirth::Unborn,
        };
        // A launch recorded before the epoch closed is vouched for only while
        // the epoch is still open after reading it.
        (self.read::<Epoch>(&self.directory.join(EPOCH)).ok()?? == epoch).then_some(birth)
    }

    #[cfg(any(feature = "host", test))]
    fn lock(&self) -> io::Result<crate::instance::OwnedLock> {
        crate::secure_file::prepare_private_directory(&self.directory)?;
        crate::instance::owned_lock(&self.directory.join(LOCK), libc::LOCK_EX)
    }

    fn read<T: for<'de> Deserialize<'de>>(&self, path: &Path) -> io::Result<Option<T>> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Readers see the old record or the new one, never part of one.
    #[cfg(any(feature = "host", test))]
    fn write(&self, path: &Path, value: &impl Serialize) -> io::Result<()> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        let temporary = path.with_extension("tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    }
}

#[cfg(any(feature = "host", test))]
fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The current time on the clock `/proc/<pid>/stat` dates processes by, in
/// its ticks, rounded down so a process forked after this reads no earlier.
#[cfg(any(feature = "host", test))]
pub(crate) fn now_ticks() -> io::Result<u64> {
    // SAFETY: sysconf has no pointer arguments; clock_gettime writes to ts.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if ticks <= 0 || unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let nanos = u128::try_from(ts.tv_sec).map_err(io::Error::other)? * 1_000_000_000
        + u128::try_from(ts.tv_nsec).map_err(io::Error::other)?;
    u64::try_from(nanos * ticks as u128 / 1_000_000_000).map_err(io::Error::other)
}

/// The cgroup namespace this process sees cgroup paths through.
pub(crate) fn cgroup_namespace() -> Option<String> {
    fs::read_link("/proc/self/ns/cgroup")
        .ok()?
        .to_str()
        .map(str::to_owned)
}

/// The unified-hierarchy cgroup in a `/proc/<pid>/cgroup` listing. `None`
/// when there are several, which only a crafted v1 hierarchy name makes.
pub(crate) fn unified_cgroup(listing: &str) -> Option<&str> {
    let mut unified = listing.lines().filter_map(|line| line.strip_prefix("0::"));
    let cgroup = unified.next()?;
    unified.next().is_none().then_some(cgroup)
}

/// Where `cgroup` is relative to this user's user manager, whose tree is the
/// only part of the hierarchy the user can move processes within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// The user manager's cgroup or one below it.
    Inside,
    /// Somewhere the user can't move a process out of the user manager to.
    Outside,
    /// Not a path from the root of the hierarchy this process sees: outside
    /// its cgroup namespace, or not a cgroup path at all.
    Unknown,
}

pub(crate) fn placement(cgroup: &str, uid: u32) -> Placement {
    if !cgroup.starts_with('/') || cgroup.split('/').any(|part| part == "..") {
        return Placement::Unknown;
    }
    let manager = format!("/user.slice/user-{uid}.slice/user@{uid}.service");
    match cgroup.strip_prefix(&manager) {
        Some("") => Placement::Inside,
        Some(below) if below.starts_with('/') => Placement::Inside,
        _ => Placement::Outside,
    }
}

/// Where a launch from this process is born, for [`Ledger::record_launch`].
#[cfg(feature = "host")]
pub(crate) fn launch_containment() -> Option<String> {
    let listing = fs::read_to_string("/proc/self/cgroup").ok()?;
    (placement(unified_cgroup(&listing)?, crate::daemon::current_uid()) == Placement::Inside)
        .then(cgroup_namespace)
        .flatten()
}

/// Opens this boot's epoch for the instance, when `first`. A failure leaves the
/// records proving nothing, which costs only the ruling out they allow.
#[cfg(feature = "host")]
pub(crate) fn open_instance_epoch(boot: &BootId, first: bool) {
    if let Err(error) = Ledger::of_instance().and_then(|ledger| ledger.open_epoch(boot, first)) {
        tracing::warn!(%error, "terminal launches on this boot won't rule out unreadable processes");
    }
}

/// Records a launch of `session`, before its holder forks. `Err` means the
/// launch can't go ahead: it could be recorded neither as a launch nor by
/// closing the epoch, so the records would vouch for a lineage they miss.
#[cfg(feature = "host")]
pub(crate) fn record_instance_launch(
    session: &crate::model::TerminalSession,
) -> Result<(), String> {
    if cfg!(test) {
        // Unit tests launch terminals without an instance of their own.
        return Ok(());
    }
    // With no state directory there are no records for a scan to go by.
    let Ok(ledger) = Ledger::of_instance() else {
        return Ok(());
    };
    let recorded = (|| {
        let boot = crate::daemon::read_boot_id()?;
        ledger.record_launch(
            &boot,
            &lineages(session),
            now_ticks()?,
            launch_containment(),
        )
    })();
    let Err(error) = recorded else {
        return Ok(());
    };
    tracing::warn!(%error, terminal = %session.id, "couldn't record a terminal launch; unreadable processes won't be ruled out until the next boot");
    ledger.close_epoch().map_err(|closing| {
        format!("record terminal launch: {error}; close the lineage epoch: {closing}")
    })
}

/// The lineages a session's processes are marked with: its own ID, and the
/// origin of its recovery chain.
fn lineages(session: &crate::model::TerminalSession) -> Vec<TerminalId> {
    let origin = session.recovery_origin.unwrap_or(session.id);
    if origin == session.id {
        vec![session.id]
    } else {
        vec![session.id, origin]
    }
}

/// What the instance's records say about every process the scan for `session`
/// looks for: one marked with its ID or with its recovery chain's origin. Each
/// marker is recorded under its own lineage by whichever launch set it, so a
/// session saved with a different chain than it was launched with still
/// finds the right records.
pub(crate) fn instance_birth(session: &crate::model::TerminalSession) -> Option<LineageBirth> {
    if cfg!(test) {
        // Tests scan the real process table without launching through a ledger.
        return None;
    }
    let boot = crate::daemon::read_boot_id().ok()?;
    let ledger = Ledger::of_instance().ok()?;
    lineages(session)
        .into_iter()
        .try_fold(LineageBirth::Unborn, |birth, lineage| {
            Some(birth.either(ledger.birth(&boot, lineage)?))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(name: &str) -> BootId {
        BootId::new(name)
    }

    #[test]
    fn only_the_first_daemon_of_a_boot_opens_an_epoch() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(directory.path().join("lineages"));
        let origin = TerminalId::new();

        // A daemon that isn't the boot's first can't vouch for what came before.
        ledger.open_epoch(&boot("a"), false).unwrap();
        assert_eq!(ledger.birth(&boot("a"), origin), None);

        ledger.open_epoch(&boot("a"), true).unwrap();
        assert_eq!(ledger.birth(&boot("a"), origin), Some(LineageBirth::Unborn));
        assert_eq!(ledger.birth(&boot("b"), origin), None, "another boot");

        ledger
            .record_launch(&boot("a"), &[origin], 50, Some("cgroup:[1]".into()))
            .unwrap();
        ledger
            .record_launch(&boot("a"), &[origin], 40, Some("cgroup:[1]".into()))
            .unwrap();
        ledger
            .record_launch(&boot("a"), &[origin], 60, Some("cgroup:[1]".into()))
            .unwrap();
        assert_eq!(
            ledger.birth(&boot("a"), origin),
            Some(LineageBirth::Born {
                since: 40,
                contained_in: Some("cgroup:[1]".into())
            })
        );
        // One launch outside the user manager, or seen through another
        // namespace, and the lineage is no longer contained.
        ledger
            .record_launch(&boot("a"), &[origin], 70, None)
            .unwrap();
        ledger
            .record_launch(&boot("a"), &[origin], 80, Some("cgroup:[1]".into()))
            .unwrap();
        assert_eq!(
            ledger.birth(&boot("a"), origin),
            Some(LineageBirth::Born {
                since: 40,
                contained_in: None
            })
        );
        let other = TerminalId::new();
        ledger
            .record_launch(&boot("a"), &[other], 90, Some("cgroup:[1]".into()))
            .unwrap();
        ledger
            .record_launch(&boot("a"), &[other], 95, Some("cgroup:[2]".into()))
            .unwrap();
        assert!(matches!(
            ledger.birth(&boot("a"), other),
            Some(LineageBirth::Born {
                contained_in: None,
                ..
            })
        ));

        // A later daemon on the same boot keeps the epoch and the records.
        ledger.open_epoch(&boot("a"), false).unwrap();
        assert!(matches!(
            ledger.birth(&boot("a"), origin),
            Some(LineageBirth::Born { since: 40, .. })
        ));

        // A launch that couldn't be recorded closes the epoch for the boot.
        ledger.close_epoch().unwrap();
        assert_eq!(ledger.birth(&boot("a"), origin), None);
        ledger.open_epoch(&boot("a"), true).unwrap();
        assert_eq!(
            ledger.birth(&boot("a"), origin),
            None,
            "the boot's epoch stays closed"
        );

        // The next boot starts clean.
        ledger.open_epoch(&boot("b"), true).unwrap();
        assert_eq!(ledger.birth(&boot("b"), origin), Some(LineageBirth::Unborn));
    }

    /// A launch whose record failed leaves nothing behind but the closed epoch,
    /// which must keep a daemon that thinks it's the boot's first from
    /// vouching for that launch.
    #[test]
    fn a_closed_epoch_without_records_stays_closed() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(directory.path().join("lineages"));
        ledger.open_epoch(&boot("a"), true).unwrap();
        ledger.close_epoch().unwrap();
        ledger.open_epoch(&boot("a"), true).unwrap();
        assert_eq!(ledger.birth(&boot("a"), TerminalId::new()), None);

        // Records from this boot without its epoch can't be vouched for either.
        let other = Ledger::at(directory.path().join("other"));
        other.open_epoch(&boot("a"), false).unwrap();
        other
            .record_launch(&boot("a"), &[TerminalId::new()], 10, None)
            .unwrap();
        other.open_epoch(&boot("a"), true).unwrap();
        assert_eq!(other.birth(&boot("a"), TerminalId::new()), None);
        // Asked again, it still finds the record it kept.
        other.open_epoch(&boot("a"), true).unwrap();
        assert_eq!(other.birth(&boot("a"), TerminalId::new()), None);

        // A record that can't be read could be this boot's; a write that never
        // finished is no launch at all.
        let damaged = Ledger::at(directory.path().join("damaged"));
        damaged.open_epoch(&boot("b"), false).unwrap();
        fs::write(
            directory
                .path()
                .join("damaged")
                .join(TerminalId::new().to_string()),
            "{",
        )
        .unwrap();
        damaged.open_epoch(&boot("a"), true).unwrap();
        assert_eq!(damaged.birth(&boot("a"), TerminalId::new()), None);
        damaged.open_epoch(&boot("b"), true).unwrap();
        assert_eq!(
            damaged.birth(&boot("b"), TerminalId::new()),
            Some(LineageBirth::Unborn),
            "the next boot expires it"
        );
        let unfinished = Ledger::at(directory.path().join("unfinished"));
        unfinished.open_epoch(&boot("a"), false).unwrap();
        fs::write(directory.path().join("unfinished").join("x.tmp"), "{").unwrap();
        unfinished.open_epoch(&boot("a"), true).unwrap();
        assert_eq!(
            unfinished.birth(&boot("a"), TerminalId::new()),
            Some(LineageBirth::Unborn)
        );
        // An epoch that can't be read could be this boot's closed one.
        fs::write(directory.path().join("unfinished").join(EPOCH), "{").unwrap();
        unfinished.open_epoch(&boot("a"), true).unwrap();
        assert_eq!(unfinished.birth(&boot("a"), TerminalId::new()), None);
    }

    #[test]
    fn the_launch_clock_never_dates_a_later_process_earlier() {
        let before = now_ticks().unwrap();
        let stat = fs::read_to_string("/proc/thread-self/stat").unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let own_start: u64 = fields[19].parse().unwrap();
        assert!(
            own_start <= before,
            "this process started before the reading"
        );
        let child = std::process::Command::new("sleep")
            .arg("1")
            .spawn()
            .unwrap();
        let stat = fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let child_start: u64 = fields[19].parse().unwrap();
        assert!(child_start >= before, "{child_start} < {before}");
        let mut child = child;
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn only_the_user_managers_tree_contains_a_lineage() {
        for inside in [
            "/user.slice/user-1000.slice/user@1000.service/app.slice/cagents-session-x.scope",
            // A process can sit in the user manager's own cgroup where nothing
            // stops it, as on a hybrid hierarchy.
            "/user.slice/user-1000.slice/user@1000.service",
        ] {
            assert_eq!(placement(inside, 1000), Placement::Inside, "{inside}");
        }
        for outside in [
            "/user.slice/user-1000.slice/session-4.scope",
            "/user.slice/user-1000.slice/user@1000.service.d",
            "/user.slice/user-1000.slice/user@1001.service/app.slice",
            "/system.slice/sshd.service",
            "/app.slice/cagents-session-x.scope",
        ] {
            assert_eq!(placement(outside, 1000), Placement::Outside, "{outside}");
        }
        for unknown in [
            // Outside this cgroup namespace's root, which could be anywhere.
            "/../../user.slice/user-1000.slice/session-4.scope",
            "/user.slice/../system.slice",
            "app.slice",
            "",
        ] {
            assert_eq!(placement(unknown, 1000), Placement::Unknown, "{unknown:?}");
        }
        assert_eq!(
            unified_cgroup("1:name=systemd:/a\n0::/user.slice/b\n"),
            Some("/user.slice/b")
        );
        assert_eq!(unified_cgroup("1:name=x\n0::/a\n0::/b\n"), None, "spoofed");
        assert_eq!(unified_cgroup("1:name=systemd:/a\n"), None);
    }

    #[test]
    fn a_scan_covers_the_lineages_of_both_markers() {
        let born = |since, contained_in: Option<&str>| LineageBirth::Born {
            since,
            contained_in: contained_in.map(str::to_owned),
        };
        assert_eq!(
            LineageBirth::Unborn.either(LineageBirth::Unborn),
            LineageBirth::Unborn
        );
        assert_eq!(
            LineageBirth::Unborn.either(born(5, Some("a"))),
            born(5, Some("a"))
        );
        assert_eq!(
            born(5, Some("a")).either(born(3, Some("a"))),
            born(3, Some("a"))
        );
        assert_eq!(born(5, Some("a")).either(born(7, Some("b"))), born(5, None));
        assert_eq!(born(5, None).either(born(7, Some("a"))), born(5, None));

        // A launch records both of the markers it sets, so a session saved
        // with another recovery chain still finds the records of its own ID.
        let directory = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(directory.path().join("lineages"));
        ledger.open_epoch(&boot("a"), true).unwrap();
        let (id, origin) = (TerminalId::new(), TerminalId::new());
        ledger
            .record_launch(&boot("a"), &[id, origin], 20, None)
            .unwrap();
        for lineage in [id, origin] {
            assert_eq!(ledger.birth(&boot("a"), lineage), Some(born(20, None)));
        }
    }
}
