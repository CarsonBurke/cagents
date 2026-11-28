// Shared by the library's sparse-index contract and the standalone scale contract.
use super::isolated_git;
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

// The scale contract's 26 fixed phase pairs total at most 4,114 bytes with
// maximal u128 elapsed digits and "panicking" results. Its executor documents
// the combined diagnostic bound against the 64 KiB failure capture limit.
pub(super) struct Phase {
    name: &'static str,
    started: Instant,
}

pub(super) fn phase(name: &'static str) -> Phase {
    eprintln!("pathspec phase={name} begin elapsed_ms=0");
    Phase {
        name,
        started: Instant::now(),
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        eprintln!(
            "pathspec phase={} end elapsed_ms={} result={}",
            self.name,
            self.started.elapsed().as_millis(),
            if std::thread::panicking() {
                "panicking"
            } else {
                "finished"
            }
        );
    }
}

/// Run one scale contract in a disk-backed private namespace.
pub(super) fn isolated_large_pathspec_test(name: &str, contract: impl FnOnce(&Path)) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if let Some(child) = isolated_git::IsolatedGitChild::begin(name) {
        let entry = phase("child entry");
        let root = isolated_git::child_root().join("repository");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        drop(entry);
        let running = phase("contract");
        contract(&root);
        drop(running);
        eprintln!("Contract passed; settling its process family");
        let settlement = phase("settlement");
        drop(child);
        drop(settlement);
        return;
    }

    let directory = tempfile::Builder::new()
        .prefix("cagents-pathspec-contract-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in("/var/tmp")
        .unwrap();
    isolated_git::run(name, directory, Duration::from_secs(240));
}
