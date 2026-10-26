//! Verified, separately provisioned speech assets. Discovery never downloads or
//! executes an arbitrary program from PATH, and a marker alone is not readiness.
use crate::process::CommandSpec;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path},
    sync::Arc,
};

/// Official GitHub release API metadata observed 2026-10-25; the installer must
/// check these archives before extracting and publishing the file manifest.
#[derive(Debug, Clone, Copy)]
pub struct Archive {
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}
pub const RUNTIME: Archive = Archive {
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.0/sherpa-onnx-v2.0.0-linux-x64-static-no-tts.tar.bz2",
    bytes: 373_684_222,
    sha256: "af422f903c5a244457f2651bf7f0c9f499868083324739b0110a722a96602bc3",
};
pub const PARAKEET: Archive = Archive {
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
    bytes: 487_170_055,
    sha256: "5793d0fd397c5778d2cf2126994d58e9d56b1be7c04d13c7a15bb1b4eafb16bf",
};
const INSTALL_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const REQUIRED: [&str; 5] = [
    "runtime/bin/sherpa-onnx-offline",
    "model/encoder.int8.onnx",
    "model/decoder.int8.onnx",
    "model/joiner.int8.onnx",
    "model/tokens.txt",
];

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledManifest {
    pub version: u32,
    pub runtime_sha256: String,
    pub model_sha256: String,
    pub files: BTreeMap<String, InstalledFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledFile {
    pub bytes: u64,
    pub sha256: String,
}

/// Retained file descriptors, not later path lookups, supply the recognizer's
/// executable, ONNX models and vocabulary. The first supported runtime statically
/// links its ASR libraries. These descriptors pin identity, not protection from
/// deliberate in-place writes by the same OS user. Own one per prepared session.
pub struct Installation {
    files: BTreeMap<String, Arc<File>>,
    _directory: tempfile::TempDir,
}
impl Installation {
    pub(super) fn from_extraction(
        extraction: super::provision::VerifiedExtraction,
        cancel: &crate::process::CancellationToken,
    ) -> io::Result<Self> {
        if extraction.files.len() != REQUIRED.len()
            || REQUIRED
                .iter()
                .any(|name| !extraction.files.contains_key(*name))
        {
            return Err(invalid("Speech installation is incomplete"));
        }
        let mut total = 0u64;
        let mut files = BTreeMap::new();
        for (name, member) in extraction.files {
            if cancel.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Speech setup cancelled",
                ));
            }
            let metadata = member.file.metadata()?;
            if !metadata.is_file() || metadata.len() != member.receipt.bytes {
                return Err(invalid("Speech member changed during preparation"));
            }
            total = total
                .checked_add(member.receipt.bytes)
                .filter(|total| *total <= INSTALL_LIMIT)
                .ok_or_else(|| invalid("Speech installation exceeds its size limit"))?;
            files.insert(name, member.file);
        }
        Ok(Self {
            files,
            _directory: extraction.directory,
        })
    }
    pub(super) fn command(&self, wav: Arc<File>) -> io::Result<CommandSpec> {
        let file_path = |file: &File| format!("/proc/self/fd/{}", file.as_raw_fd());
        let mut spec = CommandSpec::new(file_path(&self.files[REQUIRED[0]]))
            .args([
                "--model-type=nemo_transducer",
                "--num-threads=2",
                "--provider=cpu",
                "--debug=false",
            ])
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD")
            .env("LC_ALL", "C");
        for (option, name) in [
            ("--encoder", REQUIRED[1]),
            ("--decoder", REQUIRED[2]),
            ("--joiner", REQUIRED[3]),
            ("--tokens", REQUIRED[4]),
        ] {
            spec = spec.arg(format!("{option}={}", file_path(&self.files[name])));
        }
        spec = spec.arg(file_path(&wav)).inherit_file(wav)?;
        for file in self.files.values() {
            spec = spec.inherit_file(file.clone())?;
        }
        Ok(spec)
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Local trusted assets deliberately do not consult the selected workspace
/// device. Walk each component relative to its retained parent without links.
pub(super) fn open_beneath(root: &File, relative: &Path, directory: bool) -> io::Result<File> {
    let components: Vec<_> = relative.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(invalid("Speech asset path leaves its installation"));
    }
    let mut parent = root.try_clone()?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            unreachable!()
        };
        let name =
            CString::new(name.as_bytes()).map_err(|_| invalid("Speech asset path contains NUL"))?;
        let is_directory = index + 1 < components.len() || directory;
        let flags = libc::O_RDONLY
            | libc::O_NONBLOCK
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if is_directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: the parent descriptor and CString are owned across openat;
        // the returned descriptor is fresh and adopted exactly once below.
        let raw = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        parent = unsafe { File::from_raw_fd(raw) };
    }
    Ok(parent)
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;
    #[test]
    fn private_extraction_directory_lives_until_last_session_owner() {
        // Exercise lifetime ownership only, never prepare fake executable/model
        // readiness. Production construction still requires VerifiedExtraction.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        std::fs::write(path.join("owned"), b"fixture").unwrap();
        let prepared_owner = Arc::new(Installation {
            files: BTreeMap::new(),
            _directory: directory,
        });
        let session_owner = prepared_owner.clone();
        drop(prepared_owner);
        assert!(path.join("owned").exists());
        drop(session_owner);
        assert!(!path.exists());
    }
}
