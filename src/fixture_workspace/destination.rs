//! Where a seed may write, decided before it writes anything.
//!
//! A seed fills an instance that has never been used. It never merges into
//! real state, so the check is per instance, not per store: the instance's
//! data directory may hold exactly what earlier seeds of the same plan left
//! there, file for file, which `fixture-seed.json` records. The daemon and
//! each window are separate executables and seed one after another, so each
//! claims its own part and records it, and the whole data directory as it then
//! stands, once its store is in place.
//!
//! A store is written beside its final name and renamed into place, so a seed
//! that fails leaves nothing a later launch would open. A seed killed between
//! the rename and the record leaves a store the ledger does not name, which
//! every later seed of the instance refuses.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Write as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::Plan;

/// The ledger file, in the instance's data directory.
pub const LEDGER: &str = "fixture-seed.json";

const DAEMON_STORE: &str = "workspace-model.sqlite3";
const PRIMARY_STORE: &str = "sessions.sqlite3";

/// What a seed writes into the instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// The daemon's workspace-model store.
    Daemon,
    /// A window slot's session store.
    Window(String),
}

impl Part {
    fn label(&self) -> String {
        match self {
            Self::Daemon => "daemon".into(),
            Self::Window(slot) => format!("window:{slot}"),
        }
    }
}

/// The instance's directories, as the executable resolves them.
#[derive(Clone, Debug)]
pub struct Directories {
    pub data: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
    /// `XDG_RUNTIME_DIR`, which holds the daemon's lease and each window's.
    pub runtime: Option<PathBuf>,
    /// The development instance's name; `None` for the unnamed instance.
    pub instance: Option<String>,
}

impl Directories {
    /// This executable's instance.
    pub fn current() -> Result<Self, String> {
        let dirs = crate::instance::project_dirs()
            .ok_or("this instance's data directory is unavailable")?;
        Ok(Self {
            data: dirs.data_dir().to_owned(),
            config: dirs.config_dir().to_owned(),
            cache: dirs.cache_dir().to_owned(),
            runtime: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            instance: crate::instance::development_instance().map(str::to_owned),
        })
    }
}

/// One entry of the data directory as a seed left it. A changed size or
/// modification time means something used it since.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Entry {
    Directory,
    File { length: u64, modified: (i64, i64) },
    Symlink { target: PathBuf },
    Other,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    plan: PathBuf,
    /// [`Plan::digest`]: a plan generated again at the same path is another
    /// workspace.
    digest: String,
    parts: Vec<String>,
    /// The data directory, relative to it, as the last seed left it: their
    /// stores, and whatever the executables create as they start.
    entries: BTreeMap<PathBuf, Entry>,
}

/// An instance a seed may write into, held against other seeds until the
/// [`Destination`] it is claimed as is written.
#[derive(Debug)]
pub struct Instance {
    directories: Directories,
    /// The data directory, locked exclusively: the ledger is read, checked and
    /// rewritten under this lock.
    _lock: File,
}

impl Instance {
    /// Admits the instance in `directories` for `plan`, before the seed takes
    /// any lease there, or says why it cannot be seeded.
    pub fn admit(plan: &Plan, directories: Directories) -> Result<Self, String> {
        if directories.instance.is_none() {
            refuse_unnamed(plan, &directories)?;
        }
        std::fs::create_dir_all(&directories.data)
            .map_err(|error| format!("Cannot create {}: {error}", directories.data.display()))?;
        let lock = File::open(&directories.data)
            .map_err(|error| format!("Cannot open {}: {error}", directories.data.display()))?;
        // SAFETY: flock receives a live descriptor and scalar flags.
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            return Err(if error.kind() == io::ErrorKind::WouldBlock {
                format!(
                    "Another seed is writing this instance ({}). Let it finish first.",
                    directories.data.display()
                )
            } else {
                format!("Cannot lock {}: {error}", directories.data.display())
            });
        }
        Ok(Self {
            directories,
            _lock: lock,
        })
    }

    /// Claims `part` of this instance for `plan`. Take every lease the part's
    /// executable holds while it writes before claiming, so nothing else can
    /// change the instance between this check and the write.
    pub fn claim(self, plan: &Plan, part: Part) -> Result<Destination, String> {
        let data = &self.directories.data;
        let ledger_path = data.join(LEDGER);
        let ledger = match std::fs::read(&ledger_path) {
            Ok(bytes) => serde_json::from_slice::<Ledger>(&bytes).map_err(|error| {
                format!(
                    "{} is not a seed's ledger ({error}). Delete this instance's data directory ({}) \
                     to seed it.",
                    ledger_path.display(),
                    data.display()
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ledger {
                plan: plan.path.clone(),
                digest: plan.digest.clone(),
                parts: Vec::new(),
                entries: BTreeMap::new(),
            },
            Err(error) => return Err(format!("Cannot read {}: {error}", ledger_path.display())),
        };
        if ledger.plan != plan.path || ledger.digest != plan.digest {
            return Err(format!(
                "This instance was seeded from another workspace ({}{}). A seed never merges two \
                 workspaces; delete its data directory ({}) to seed it again.",
                ledger.plan.display(),
                if ledger.plan == plan.path {
                    ", since generated again"
                } else {
                    ""
                },
                data.display()
            ));
        }
        if ledger.parts.contains(&part.label()) {
            return Err(format!(
                "This instance's {} is already seeded. Delete its data directory ({}) to seed it \
                 again.",
                part.label(),
                data.display()
            ));
        }
        let destination = Destination {
            instance: self,
            part,
            ledger,
        };
        destination.refuse_changed()?;
        Ok(destination)
    }
}

/// A part of an instance this seed may write, until it is
/// [`write`](Destination::write)n.
#[derive(Debug)]
pub struct Destination {
    instance: Instance,
    part: Part,
    ledger: Ledger,
}

impl Destination {
    fn data(&self) -> &Path {
        &self.instance.directories.data
    }

    /// The store this part ends up at.
    pub fn store(&self) -> PathBuf {
        store_path(self.data(), &self.part)
    }

    /// Where the store is written before it is renamed into place.
    pub fn staging(&self) -> PathBuf {
        let store = self.store();
        let mut name = store.file_name().unwrap().to_owned();
        name.push(".seeding");
        store.with_file_name(name)
    }

    /// The instance's cache directory, which only seeds write into.
    pub fn cache(&self) -> &Path {
        &self.instance.directories.cache
    }

    /// Writes this part's store with `write`, which receives the staging path
    /// and must have closed every connection to it when it returns. The store
    /// is renamed into place only if `write` succeeds, and stays there only
    /// once the ledger records it; otherwise nothing of it is left behind.
    pub fn write<T>(mut self, write: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String> {
        let staging = self.staging();
        let store = self.store();
        let parent = store.parent().unwrap().to_owned();
        // The directories below the data directory this part's store needs.
        let created = parent
            .ancestors()
            .take_while(|directory| *directory != self.data())
            .filter(|directory| !directory.exists())
            .map(Path::to_owned)
            .collect::<Vec<_>>();
        remove_store(&staging);
        let result = write(&staging)
            .and_then(|value| self.place(&staging, &store).map(|()| value))
            .and_then(|value| {
                self.ledger.parts.push(self.part.label());
                self.ledger.entries = entries(self.data())?;
                match self.record() {
                    Ok(()) => Ok(value),
                    Err(error) => {
                        remove_store(&store);
                        Err(error)
                    }
                }
            });
        let Err(error) = result else {
            return result;
        };
        remove_store(&staging);
        for directory in &created {
            let _ = std::fs::remove_dir(directory);
        }
        let recorded = self.ledger.entries.clone();
        let left = entries(self.data())
            .unwrap_or_default()
            .into_keys()
            .filter(|entry| !recorded.contains_key(entry) && entry != Path::new(LEDGER))
            .map(|entry| entry.display().to_string())
            .collect::<Vec<_>>();
        if left.is_empty() {
            Err(error)
        } else {
            Err(format!(
                "{error}. The failed seed left {} in {}; delete that directory before seeding \
                 again.",
                left.join(", "),
                self.data().display()
            ))
        }
    }

    /// Moves the closed store at `staging` to `store`, durably.
    fn place(&self, staging: &Path, store: &Path) -> Result<(), String> {
        if store.exists() {
            return Err(format!(
                "{} appeared while it was being seeded",
                store.display()
            ));
        }
        // Closed stores leave a WAL folded back into the database and
        // truncated, and its index. Anything else is unwritten data.
        let wal = with_suffix(staging, "-wal");
        if std::fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 0)
            || with_suffix(staging, "-journal").exists()
        {
            return Err(format!(
                "{} is not checkpointed: the seed did not close its store",
                staging.display()
            ));
        }
        for suffix in ["-wal", "-shm"] {
            let sidecar = with_suffix(staging, suffix);
            match std::fs::remove_file(&sidecar) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(format!("Cannot remove {}: {error}", sidecar.display()));
                }
                _ => {}
            }
        }
        let moved = File::open(staging)
            .and_then(|file| file.sync_all())
            .and_then(|()| std::fs::rename(staging, store))
            .and_then(|()| sync_directory(store.parent().unwrap()));
        moved.map_err(|error| format!("Cannot move {} into place: {error}", store.display()))
    }

    fn record(&self) -> Result<(), String> {
        let path = self.data().join(LEDGER);
        let staging = self.data().join(format!("{LEDGER}.seeding"));
        let bytes = serde_json::to_vec_pretty(&self.ledger).map_err(|error| error.to_string())?;
        let written = File::create(&staging)
            .and_then(|mut file| {
                file.write_all(&bytes)?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&staging, &path))
            .and_then(|()| sync_directory(self.data()));
        written.map_err(|error| {
            let _ = std::fs::remove_file(&staging);
            format!("Cannot record the seed in {}: {error}", path.display())
        })
    }

    /// Refuses a data directory that differs in any way from what earlier
    /// seeds of this plan left: any state of a used instance, including every
    /// window's session store ([`crate::instance::window_database_paths`]), and
    /// any store a seeded instance's daemon or window has since opened.
    fn refuse_changed(&self) -> Result<(), String> {
        let data = self.data();
        let found = entries(data)?;
        let mut changed = found
            .iter()
            .filter(|(path, entry)| self.ledger.entries.get(*path) != Some(entry))
            .map(|(path, _)| data.join(path).display().to_string())
            .chain(
                self.ledger
                    .entries
                    .keys()
                    .filter(|path| !found.contains_key(*path))
                    .map(|path| format!("{} (gone)", data.join(path).display())),
            )
            .collect::<Vec<_>>();
        if changed.is_empty() {
            return Ok(());
        }
        changed.sort();
        let shown = changed
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "This instance has state a seed did not write, or that changed since ({shown}{}). A \
             seed only fills an instance that has never been used, and never merges into real \
             state. Seed a fresh instance, or delete its data directory ({}) first.",
            if changed.len() > 5 { ", ..." } else { "" },
            data.display()
        ))
    }
}

/// Everything in the data directory but the ledger, relative to it. Symbolic
/// links are recorded, never followed.
fn entries(data: &Path) -> Result<BTreeMap<PathBuf, Entry>, String> {
    fn walk(
        data: &Path,
        relative: &Path,
        found: &mut BTreeMap<PathBuf, Entry>,
    ) -> Result<(), String> {
        let directory = data.join(relative);
        let listing = match std::fs::read_dir(&directory) {
            Ok(listing) => listing,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("Cannot read {}: {error}", directory.display())),
        };
        for item in listing {
            let item =
                item.map_err(|error| format!("Cannot read {}: {error}", directory.display()))?;
            let path = relative.join(item.file_name());
            if path == Path::new(LEDGER) {
                continue;
            }
            let full = data.join(&path);
            let metadata = std::fs::symlink_metadata(&full)
                .map_err(|error| format!("Cannot read {}: {error}", full.display()))?;
            let kind = metadata.file_type();
            let entry = if kind.is_dir() {
                walk(data, &path, found)?;
                Entry::Directory
            } else if kind.is_file() {
                Entry::File {
                    length: metadata.len(),
                    modified: (metadata.mtime(), metadata.mtime_nsec()),
                }
            } else if kind.is_symlink() {
                Entry::Symlink {
                    target: std::fs::read_link(&full)
                        .map_err(|error| format!("Cannot read {}: {error}", full.display()))?,
                }
            } else {
                Entry::Other
            };
            found.insert(path, entry);
        }
        Ok(())
    }
    let mut found = BTreeMap::new();
    walk(data, Path::new(""), &mut found)?;
    Ok(found)
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

/// The unnamed development instance belongs to the login session's daemon.
/// Without a name, a seed only writes to directories inside the plan's root,
/// and takes leases only in a runtime directory there, as
/// `scripts/fixture-workspace apply` gives it.
fn refuse_unnamed(plan: &Plan, directories: &Directories) -> Result<(), String> {
    let root = plan
        .root
        .canonicalize()
        .map_err(|error| format!("Cannot resolve {}: {error}", plan.root.display()))?;
    let runtime = directories
        .runtime
        .as_ref()
        .ok_or("XDG_RUNTIME_DIR is unset, so this seed cannot take this instance's leases")?;
    for directory in [
        &directories.data,
        &directories.config,
        &directories.cache,
        runtime,
    ] {
        if !resolved(directory).starts_with(&root) {
            return Err(format!(
                "This is the unnamed development instance, whose {} belongs to the login session's \
                 development daemon. Seed a named instance (CAGENTS_DEVELOPMENT_INSTANCE, or \
                 `scripts/fixture-workspace apply --instance NAME`), or point every XDG directory, \
                 XDG_RUNTIME_DIR included, inside {} as `apply` without --instance does.",
                directory.display(),
                root.display()
            ));
        }
    }
    Ok(())
}

/// `path` with its longest existing ancestor resolved, as it will be once
/// created.
fn resolved(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return rest.iter().rev().fold(real, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

fn store_path(data: &Path, part: &Part) -> PathBuf {
    match part {
        Part::Daemon => data.join(DAEMON_STORE),
        Part::Window(slot) if slot == crate::instance::PRIMARY_WINDOW => data.join(PRIMARY_STORE),
        Part::Window(slot) => data.join("windows").join(slot).join(PRIMARY_STORE),
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap().to_owned();
    name.push(suffix);
    path.with_file_name(name)
}

fn remove_store(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(with_suffix(path, suffix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_workspace::test_support;

    fn directories(root: &Path, instance: Option<&str>) -> Directories {
        Directories {
            data: root.join("data/cagents-development"),
            config: root.join("config/cagents-development"),
            cache: root.join("cache/cagents-development"),
            runtime: Some(root.join("runtime")),
            instance: instance.map(str::to_owned),
        }
    }

    fn window(slot: &str) -> Part {
        Part::Window(slot.into())
    }

    fn claim(plan: &Plan, part: Part, directories: &Directories) -> Result<Destination, String> {
        Instance::admit(plan, directories.clone())?.claim(plan, part)
    }

    fn seed(plan: &Plan, part: Part, directories: &Directories) -> Result<(), String> {
        claim(plan, part, directories)?.write(|path| {
            std::fs::create_dir_all(path.parent().unwrap()).map_err(|error| error.to_string())?;
            std::fs::write(path, b"store").map_err(|error| error.to_string())
        })
    }

    fn listing(directory: &Path) -> Vec<PathBuf> {
        entries(directory).unwrap().into_keys().collect()
    }

    #[test]
    fn each_part_seeds_once_in_any_order() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        seed(&plan, Part::Daemon, &dirs).unwrap();
        seed(&plan, window("window-2"), &dirs).unwrap();
        seed(&plan, window("primary"), &dirs).unwrap();
        assert!(dirs.data.join("workspace-model.sqlite3").exists());
        assert!(dirs.data.join("sessions.sqlite3").exists());
        assert!(dirs.data.join("windows/window-2/sessions.sqlite3").exists());
        let again = seed(&plan, window("window-2"), &dirs).unwrap_err();
        assert!(again.contains("already seeded"), "{again}");
    }

    /// What a seed's executable leaves as it writes belongs to the seed; what
    /// appears after it does not.
    #[test]
    fn what_a_seeding_executable_creates_belongs_to_the_seed() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        claim(&plan, Part::Daemon, &dirs)
            .unwrap()
            .write(|path| {
                std::fs::create_dir_all(dirs.data.join("daemon-upgrade")).unwrap();
                std::fs::write(path, b"store").map_err(|error| error.to_string())
            })
            .unwrap();
        seed(&plan, window("primary"), &dirs).unwrap();
        std::fs::create_dir_all(dirs.data.join("daemon-upgrade/attempts")).unwrap();
        let error = seed(&plan, window("window-2"), &dirs).unwrap_err();
        assert!(error.contains("daemon-upgrade/attempts"), "{error}");
    }

    /// A used instance has state no seed recorded: another window's store,
    /// or anything else in its data directory, however deep.
    #[test]
    fn a_used_instance_is_refused_whatever_it_holds() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        for used in [
            "sessions.sqlite3",
            "workspace-model.sqlite3",
            "windows/window-3/sessions.sqlite3",
            "accounts/registry.sqlite3",
            "accounts/claude/home/.claude.json",
        ] {
            let dirs = directories(&plan.root.join(used.replace('/', "-")), Some("seeded"));
            let path = dirs.data.join(used);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"real").unwrap();
            for part in [Part::Daemon, window("primary"), window("window-2")] {
                let error = seed(&plan, part.clone(), &dirs).unwrap_err();
                assert!(
                    error.contains("never merges into real state"),
                    "{used} {part:?}: {error}"
                );
            }
            assert_eq!(std::fs::read(&path).unwrap(), b"real");
        }
    }

    /// A seeded store that a daemon or window has since opened differs from
    /// what the seed left: its WAL appears, or its contents change.
    #[test]
    fn a_seeded_store_that_was_used_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        seed(&plan, window("window-2"), &dirs).unwrap();
        std::fs::write(dirs.data.join("windows/window-2/sessions.sqlite3-wal"), b"").unwrap();
        let error = seed(&plan, Part::Daemon, &dirs).unwrap_err();
        assert!(
            error.contains("windows/window-2/sessions.sqlite3-wal"),
            "{error}"
        );

        let dirs = directories(&plan.root.join("changed"), None);
        seed(&plan, Part::Daemon, &dirs).unwrap();
        std::fs::write(dirs.data.join("workspace-model.sqlite3"), b"edited").unwrap();
        let error = seed(&plan, window("primary"), &dirs).unwrap_err();
        assert!(error.contains("workspace-model.sqlite3"), "{error}");
    }

    #[test]
    fn a_failed_seed_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        for part in [Part::Daemon, window("window-2"), window("primary")] {
            let destination = claim(&plan, part, &dirs).unwrap();
            let error = destination
                .write(|path| -> Result<(), String> {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, b"half").unwrap();
                    std::fs::write(with_suffix(path, "-wal"), b"half").unwrap();
                    Err("the seed failed".into())
                })
                .unwrap_err();
            assert_eq!(error, "the seed failed");
        }
        assert!(listing(&dirs.data).is_empty(), "{:?}", listing(&dirs.data));
        // The primary window's store sits in the data directory itself, which
        // the cleanup keeps, along with everything above it.
        assert!(dirs.data.is_dir());
        seed(&plan, Part::Daemon, &dirs).unwrap();
    }

    /// A store already moved into place is taken back out when the ledger
    /// cannot record it.
    #[test]
    fn a_store_the_ledger_cannot_record_is_removed() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        let destination = claim(&plan, Part::Daemon, &dirs).unwrap();
        // The ledger's staging name is taken by a directory, so it cannot be
        // written.
        std::fs::create_dir(dirs.data.join(format!("{LEDGER}.seeding"))).unwrap();
        let error = destination
            .write(|path| std::fs::write(path, b"store").map_err(|error| error.to_string()))
            .unwrap_err();
        assert!(error.contains("Cannot record the seed"), "{error}");
        assert!(!dirs.data.join(DAEMON_STORE).exists());
        assert!(!dirs.data.join(LEDGER).exists());
    }

    #[test]
    fn another_plan_never_seeds_the_rest() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        seed(&plan, Part::Daemon, &dirs).unwrap();
        let other = tempfile::tempdir().unwrap();
        let mut other_plan = test_support::small(other.path());
        // Its directories are this instance's: only the plan differs.
        other_plan.root = plan.root.clone();
        let error = seed(&other_plan, window("primary"), &dirs).unwrap_err();
        assert!(error.contains("never merges two workspaces"), "{error}");
    }

    /// `generate --replace` writes another workspace at the same path.
    #[test]
    fn a_plan_generated_again_in_place_is_another_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, Some("seeded"));
        seed(&plan, Part::Daemon, &dirs).unwrap();
        let mut again = Plan::load(&plan.path).unwrap();
        again.digest = "0".repeat(64);
        let error = seed(&again, window("primary"), &dirs).unwrap_err();
        assert!(error.contains("since generated again"), "{error}");
    }

    #[test]
    fn one_seed_at_a_time_writes_an_instance() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        let first = Instance::admit(&plan, dirs.clone()).unwrap();
        let error = Instance::admit(&plan, dirs.clone()).unwrap_err();
        assert!(error.contains("Another seed is writing"), "{error}");
        drop(first);
        Instance::admit(&plan, dirs).unwrap();
    }

    /// The unnamed instance is the login daemon's: only directories inside the
    /// plan's root may be seeded without a name, leases included.
    #[test]
    fn the_unnamed_instance_seeds_only_inside_the_root() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let outside = tempfile::tempdir().unwrap();
        for escape in ["config", "runtime"] {
            let mut dirs = directories(&plan.root, None);
            match escape {
                "config" => dirs.config = outside.path().join("config/cagents-development"),
                _ => dirs.runtime = Some(outside.path().to_owned()),
            }
            let error = seed(&plan, Part::Daemon, &dirs).unwrap_err();
            assert!(error.contains("unnamed development instance"), "{error}");
            assert!(!dirs.data.exists(), "nothing was written");
        }
        let mut dirs = directories(&plan.root, None);
        dirs.runtime = Some(outside.path().to_owned());
        dirs.instance = Some("seeded".into());
        seed(&plan, Part::Daemon, &dirs).unwrap();
    }
}
