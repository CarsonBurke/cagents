//! Where models are kept, and whether one is installed.
//!
//! Models live in `$XDG_DATA_HOME/cagents-speech-models/<id>@<revision>/`,
//! shared by every cagents instance, production and development alike. This
//! is the one deliberate exception to instance isolation: a model is
//! immutable, verified content rather than state, and isolating it would
//! download 650 MB again for each instance. `CAGENTS_DICTATION_MODELS`
//! names another directory, for tests and fixtures.
//!
//! An install writes `installed.json` last, after every file has been
//! verified, so its presence with the expected contents is what "installed"
//! means. Checking it costs a few `stat`s, not a hash of 650 MB.

use std::{
    ffi::OsString,
    fs::{self, File, TryLockError},
    io,
    path::{Path, PathBuf},
};

use crate::catalog::Model;

/// Overrides where models are kept.
pub const MODELS_VARIABLE: &str = "CAGENTS_DICTATION_MODELS";

/// The file whose contents mark a complete install.
pub const MANIFEST: &str = "installed.json";

/// The suffix of a file still downloading.
pub const PARTIAL_SUFFIX: &str = ".part";

/// Where every model is kept, or `None` when the environment names no home.
pub fn root() -> Option<PathBuf> {
    root_from(|name| std::env::var_os(name))
}

fn root_from(variable: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let absolute = |name: &str| {
        variable(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if let Some(path) = absolute(MODELS_VARIABLE) {
        return Some(path);
    }
    let data = absolute("XDG_DATA_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".local").join("share")))?;
    Some(data.join("cagents-speech-models"))
}

/// The directory `model` is installed in.
pub fn directory(root: &Path, model: &Model) -> PathBuf {
    root.join(model.directory_name())
}

/// What is on disk for a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Every file is there and verified.
    Installed,
    /// An install began: `downloaded` bytes are kept and a new install
    /// resumes from them.
    Partial { downloaded: u64 },
    /// Nothing is there.
    Absent,
}

/// What is on disk for `model` under `root`.
pub fn status(root: &Path, model: &Model) -> Status {
    let directory = directory(root, model);
    let installed = fs::read_to_string(directory.join(MANIFEST))
        .is_ok_and(|manifest| manifest == self::manifest(model))
        && model.files.iter().all(|file| {
            fs::metadata(directory.join(file.name)).is_ok_and(|meta| meta.len() == file.size)
        });
    if installed {
        return Status::Installed;
    }
    let downloaded: u64 = model
        .files
        .iter()
        .map(|file| {
            let done = fs::metadata(directory.join(file.name))
                .ok()
                .filter(|meta| meta.len() == file.size)
                .map(|meta| meta.len());
            done.or_else(|| {
                fs::metadata(partial(&directory, file.name))
                    .ok()
                    .map(|meta| meta.len().min(file.size))
            })
            .unwrap_or(0)
        })
        .sum();
    if downloaded > 0 {
        Status::Partial { downloaded }
    } else {
        Status::Absent
    }
}

/// Where `name` is written while it downloads.
pub fn partial(directory: &Path, name: &str) -> PathBuf {
    let mut file = OsString::from(name);
    file.push(PARTIAL_SUFFIX);
    directory.join(file)
}

/// The manifest an install of `model` writes last.
pub fn manifest(model: &Model) -> String {
    let files: Vec<_> = model
        .files
        .iter()
        .map(|file| {
            serde_json::json!({
                "name": file.name,
                "size": file.size,
                "sha256": file.sha256,
            })
        })
        .collect();
    let mut manifest = serde_json::to_string_pretty(&serde_json::json!({
        "model": model.id,
        "revision": model.revision,
        "files": files,
    }))
    .expect("a manifest serializes");
    manifest.push('\n');
    manifest
}

/// Exclusive use of one model's directory, for installing or removing it.
/// Held until dropped.
#[derive(Debug)]
pub struct Lease {
    _file: File,
}

/// Why a model's directory could not be taken.
#[derive(Debug)]
pub enum LeaseError {
    /// Another process holds it.
    Busy,
    Io(io::Error),
}

/// Takes `model`'s directory under `root` without waiting. The lock file
/// sits beside the directory, so removing the directory leaves it in place.
pub fn lease(root: &Path, model: &Model) -> Result<Lease, LeaseError> {
    fs::create_dir_all(root).map_err(LeaseError::Io)?;
    let mut name = OsString::from(".");
    name.push(model.directory_name());
    name.push(".lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(name))
        .map_err(LeaseError::Io)?;
    match file.try_lock() {
        Ok(()) => Ok(Lease { _file: file }),
        Err(TryLockError::WouldBlock) => Err(LeaseError::Busy),
        Err(TryLockError::Error(error)) => Err(LeaseError::Io(error)),
    }
}

/// Deletes `model` from `root`, finished or partial. Other revisions of the
/// same model, which another build may be using, stay.
pub fn remove(root: &Path, model: &Model) -> Result<(), LeaseError> {
    let _lease = lease(root, model)?;
    match fs::remove_dir_all(directory(root, model)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(LeaseError::Io(error)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    fn model() -> &'static Model {
        catalog::find(catalog::DEFAULT).unwrap()
    }

    #[test]
    fn the_root_prefers_the_override_then_xdg_then_home() {
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            root_from(lookup(&[
                (MODELS_VARIABLE, "/fixtures/models"),
                ("XDG_DATA_HOME", "/data"),
            ])),
            Some(PathBuf::from("/fixtures/models"))
        );
        assert_eq!(
            root_from(lookup(&[("XDG_DATA_HOME", "/data"), ("HOME", "/home/a")])),
            Some(PathBuf::from("/data/cagents-speech-models"))
        );
        // Relative values are ignored, as the XDG specification says.
        assert_eq!(
            root_from(lookup(&[
                (MODELS_VARIABLE, "models"),
                ("XDG_DATA_HOME", "data"),
                ("HOME", "/home/a"),
            ])),
            Some(PathBuf::from("/home/a/.local/share/cagents-speech-models"))
        );
        assert_eq!(root_from(lookup(&[])), None);
    }

    #[test]
    fn a_model_is_installed_only_with_its_manifest_and_every_file() {
        let root = tempfile::tempdir().unwrap();
        let model = model();
        assert_eq!(status(root.path(), model), Status::Absent);

        let directory = directory(root.path(), model);
        fs::create_dir_all(&directory).unwrap();
        let first = model.files[0];
        File::create(partial(&directory, first.name))
            .unwrap()
            .set_len(1000)
            .unwrap();
        assert_eq!(
            status(root.path(), model),
            Status::Partial { downloaded: 1000 }
        );

        for file in model.files {
            File::create(directory.join(file.name))
                .unwrap()
                .set_len(file.size)
                .unwrap();
        }
        fs::remove_file(partial(&directory, first.name)).unwrap();
        // Every file but no manifest: an install that has not finished.
        assert_eq!(
            status(root.path(), model),
            Status::Partial {
                downloaded: model.size()
            }
        );
        fs::write(directory.join(MANIFEST), manifest(model)).unwrap();
        assert_eq!(status(root.path(), model), Status::Installed);

        // A file of the wrong size is not an install.
        File::create(directory.join(first.name))
            .unwrap()
            .set_len(1)
            .unwrap();
        assert!(matches!(
            status(root.path(), model),
            Status::Partial { .. }
        ));
    }

    #[test]
    fn a_lease_excludes_a_second_and_remove_keeps_other_revisions() {
        let root = tempfile::tempdir().unwrap();
        let model = model();
        let held = lease(root.path(), model).unwrap();
        assert!(matches!(lease(root.path(), model), Err(LeaseError::Busy)));
        assert!(matches!(remove(root.path(), model), Err(LeaseError::Busy)));
        drop(held);

        let directory = directory(root.path(), model);
        fs::create_dir_all(&directory).unwrap();
        let other = root.path().join(format!("{}@000000000000", model.id));
        fs::create_dir_all(&other).unwrap();
        remove(root.path(), model).unwrap();
        assert!(!directory.exists());
        assert!(other.exists());
        // Removing what is not there succeeds.
        remove(root.path(), model).unwrap();
    }
}
