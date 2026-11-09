//! Where a seed may write, decided before it writes anything.
//!
//! A seed fills an instance that has never been used. It never merges into
//! real state, so the check is per instance, not per store: the instance's
//! data directory may hold nothing but what earlier seeds of the same plan
//! wrote, which `fixture-seed.json` there records. The daemon and each window
//! are separate executables and seed one after another, so each claims its own
//! part and adds it to that ledger once its store is in place.
//!
//! A store is written beside its final name and renamed into place, so a seed
//! that fails leaves nothing a later launch would open.

use std::{
    io,
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
            instance: crate::instance::development_instance().map(str::to_owned),
        })
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    plan: PathBuf,
    parts: Vec<String>,
}

/// A part of an instance this seed may write, until it is [`complete`]d.
///
/// [`complete`]: Destination::complete
#[derive(Debug)]
pub struct Destination {
    directories: Directories,
    part: Part,
    ledger: Ledger,
}

impl Destination {
    /// Claims `part` of the instance in `directories` for `plan`, or says why
    /// it cannot be seeded.
    pub fn claim(plan: &Plan, part: Part, directories: Directories) -> Result<Self, String> {
        if directories.instance.is_none() {
            refuse_unnamed(plan, &directories)?;
        }
        let ledger_path = directories.data.join(LEDGER);
        let ledger = match std::fs::read(&ledger_path) {
            Ok(bytes) => serde_json::from_slice::<Ledger>(&bytes).map_err(|error| {
                format!(
                    "{} is not a seed's ledger ({error}). Delete this instance's data directory ({}) \
                     to seed it.",
                    ledger_path.display(),
                    directories.data.display()
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ledger {
                plan: plan.path.clone(),
                parts: Vec::new(),
            },
            Err(error) => return Err(format!("Cannot read {}: {error}", ledger_path.display())),
        };
        if ledger.plan != plan.path {
            return Err(format!(
                "This instance was seeded from {}, not {}. A seed never merges two workspaces; \
                 delete its data directory ({}) to seed it again.",
                ledger.plan.display(),
                plan.path.display(),
                directories.data.display()
            ));
        }
        if ledger.parts.contains(&part.label()) {
            return Err(format!(
                "This instance's {} is already seeded. Delete its data directory ({}) to seed it \
                 again.",
                part.label(),
                directories.data.display()
            ));
        }
        let destination = Self {
            directories,
            part,
            ledger,
        };
        destination.refuse_unrecorded()?;
        Ok(destination)
    }

    /// The store this part ends up at.
    pub fn store(&self) -> PathBuf {
        store_path(&self.directories.data, &self.part)
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
        &self.directories.cache
    }

    /// Writes this part's store with `write`, which receives the staging path
    /// and must have closed every connection to it when it returns. The store
    /// is renamed into place only if `write` succeeds; otherwise nothing of it
    /// is left behind.
    pub fn write<T>(mut self, write: impl FnOnce(&Path) -> Result<T, String>) -> Result<T, String> {
        let staging = self.staging();
        let store = self.store();
        let parent = store.parent().unwrap().to_owned();
        let created = !parent.exists();
        remove_store(&staging);
        let result = write(&staging).and_then(|value| {
            if store.exists() {
                return Err(format!(
                    "{} appeared while it was being seeded",
                    store.display()
                ));
            }
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = with_suffix(&staging, suffix);
                if sidecar.exists() {
                    return Err(format!(
                        "{} is still open: the seed did not close its store",
                        sidecar.display()
                    ));
                }
            }
            std::fs::rename(&staging, &store)
                .map_err(|error| format!("Cannot move {} into place: {error}", store.display()))?;
            Ok(value)
        });
        match result {
            Ok(value) => {
                self.ledger.parts.push(self.part.label());
                self.record()?;
                Ok(value)
            }
            Err(error) => {
                remove_store(&staging);
                if created {
                    let _ = std::fs::remove_dir(&parent);
                    if let Part::Window(_) = self.part {
                        let _ = std::fs::remove_dir(parent.parent().unwrap());
                    }
                }
                Err(error)
            }
        }
    }

    fn record(&self) -> Result<(), String> {
        let path = self.directories.data.join(LEDGER);
        let staging = self.directories.data.join(format!("{LEDGER}.seeding"));
        let bytes = serde_json::to_vec_pretty(&self.ledger).map_err(|error| error.to_string())?;
        std::fs::write(&staging, bytes)
            .and_then(|()| std::fs::rename(&staging, &path))
            .map_err(|error| format!("Cannot record the seed in {}: {error}", path.display()))
    }

    /// Refuses anything in the data directory that earlier seeds of this plan
    /// did not write: any state of a used instance, including every window's
    /// session store ([`crate::instance::window_database_paths`]).
    fn refuse_unrecorded(&self) -> Result<(), String> {
        let data = &self.directories.data;
        let seeded = |part: &Part| self.ledger.parts.contains(&part.label());
        let mut unrecorded = Vec::new();
        let entries = match std::fs::read_dir(data) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("Cannot read {}: {error}", data.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let recorded = match name.as_ref() {
                LEDGER => !self.ledger.parts.is_empty(),
                DAEMON_STORE => seeded(&Part::Daemon),
                PRIMARY_STORE => seeded(&Part::Window(crate::instance::PRIMARY_WINDOW.into())),
                "windows" => {
                    for slot in
                        std::fs::read_dir(entry.path()).map_err(|error| error.to_string())?
                    {
                        let slot = slot.map_err(|error| error.to_string())?.file_name();
                        let slot = slot.to_string_lossy();
                        let path = data.join("windows").join(slot.as_ref());
                        let only_store = std::fs::read_dir(&path)
                            .map_err(|error| error.to_string())?
                            .map(|entry| entry.map(|entry| entry.file_name()))
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|error| error.to_string())?
                            == [std::ffi::OsString::from(PRIMARY_STORE)];
                        if !(only_store && seeded(&Part::Window(slot.clone().into_owned()))) {
                            unrecorded.push(path);
                        }
                    }
                    true
                }
                _ => false,
            };
            if !recorded {
                unrecorded.push(entry.path());
            }
        }
        if unrecorded.is_empty() {
            return Ok(());
        }
        unrecorded.sort();
        let shown = unrecorded
            .iter()
            .take(5)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "This instance already has state a seed did not write ({shown}{}). A seed only fills an \
             instance that has never been used, and never merges into real state. Seed a fresh \
             instance, or delete its data directory ({}) first.",
            if unrecorded.len() > 5 { ", ..." } else { "" },
            data.display()
        ))
    }
}

/// The unnamed development instance belongs to the login session's daemon.
/// Without a name, a seed only writes to directories inside the plan's root,
/// as `scripts/fixture-workspace apply` and native-evidence give it.
fn refuse_unnamed(plan: &Plan, directories: &Directories) -> Result<(), String> {
    let root = plan
        .root
        .canonicalize()
        .map_err(|error| format!("Cannot resolve {}: {error}", plan.root.display()))?;
    for directory in [&directories.data, &directories.config, &directories.cache] {
        if !resolved(directory).starts_with(&root) {
            return Err(format!(
                "This is the unnamed development instance, whose {} belongs to the login session's \
                 development daemon. Seed a named instance (CAGENTS_DEVELOPMENT_INSTANCE, or \
                 `scripts/fixture-workspace apply --instance NAME`), or point every XDG directory \
                 inside {} as `apply` without --instance does.",
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
            instance: instance.map(str::to_owned),
        }
    }

    fn window(slot: &str) -> Part {
        Part::Window(slot.into())
    }

    fn seed(plan: &Plan, part: Part, directories: &Directories) -> Result<(), String> {
        Destination::claim(plan, part, directories.clone())?
            .write(|path| std::fs::write(path, b"store").map_err(|error| error.to_string()))
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

    /// A used instance has state no seed recorded: another window's store,
    /// or anything else in its data directory.
    #[test]
    fn a_used_instance_is_refused_whatever_it_holds() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        for used in [
            "sessions.sqlite3",
            "workspace-model.sqlite3",
            "windows/window-3/sessions.sqlite3",
            "accounts/registry.sqlite3",
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
        }
    }

    /// Once a window is used it holds more than its seeded store.
    #[test]
    fn a_seeded_window_that_was_used_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        seed(&plan, window("window-2"), &dirs).unwrap();
        std::fs::write(dirs.data.join("windows/window-2/sessions.sqlite3-wal"), b"").unwrap();
        let error = seed(&plan, Part::Daemon, &dirs).unwrap_err();
        assert!(error.contains("windows/window-2"), "{error}");
    }

    #[test]
    fn a_failed_seed_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let dirs = directories(&plan.root, None);
        for part in [Part::Daemon, window("window-2")] {
            let destination = Destination::claim(&plan, part, dirs.clone()).unwrap();
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
        let left: Vec<_> = std::fs::read_dir(&dirs.data)
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
        seed(&plan, Part::Daemon, &dirs).unwrap();
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

    /// The unnamed instance is the login daemon's: only directories inside the
    /// plan's root may be seeded without a name.
    #[test]
    fn the_unnamed_instance_seeds_only_inside_the_root() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let outside = tempfile::tempdir().unwrap();
        let mut dirs = directories(&plan.root, None);
        dirs.config = outside.path().join("config/cagents-development");
        let error = seed(&plan, Part::Daemon, &dirs).unwrap_err();
        assert!(error.contains("unnamed development instance"), "{error}");
        assert!(!dirs.data.exists(), "nothing was written");
        dirs.instance = Some("seeded".into());
        seed(&plan, Part::Daemon, &dirs).unwrap();
    }
}
