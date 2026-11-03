//! Read-only source preflight in private namespaces, without installer or
//! daemon lifecycle entrypoints. Caller authority remains inherited unchanged.
use super::inspect_source_panes_mode;
use crate::instance::{self, UiUpdateGuard};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::PathBuf,
    process::Command,
    thread,
};

#[test]
fn source_preflight_without_companion_preserves_empty_saved_and_starting_sources() {
    const CASE: &str = "CAGENTS_TEST_SOURCE_INSPECTION_CASE";
    let selector = thread::current().name().unwrap().to_owned();
    let Some(case) = std::env::var_os(CASE) else {
        let root = tempfile::Builder::new().prefix("u").tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::DirBuilder::new().mode(0o700).create(&bin).unwrap();
        let executable = bin.join("source-inspection-test");
        // A sibling left by any other Cargo feature lane cannot make the
        // missing-companion regression pass accidentally.
        fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        assert!(!bin.join("cagentsd").exists());
        for case in ["empty", "primary", "secondary", "starting"] {
            let directory = root.path().join(case);
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .unwrap();
            let mut child = Command::new(&executable);
            child
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .env(CASE, case)
                .current_dir(&directory);
            for (key, leaf) in [
                ("XDG_RUNTIME_DIR", "runtime"),
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_CACHE_HOME", "cache"),
                ("XDG_STATE_HOME", "state"),
                ("TMPDIR", "tmp"),
            ] {
                let path = directory.join(leaf);
                fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
                child.env(key, path);
            }
            // HOME and operational authority are inherited. The test calls
            // only read-only preflight against this child's private runtime.
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains(&format!("test {selector} ... ok"))
                    && stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
                "{case}: exact source preflight fixture did not pass: {stdout}"
            );
        }
        return;
    };

    let case = case.to_str().unwrap();
    assert!(matches!(
        case,
        "empty" | "primary" | "secondary" | "starting"
    ));
    let executable = std::env::current_exe().unwrap();
    assert!(!executable.parent().unwrap().join("cagentsd").exists());
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let socket = crate::terminal_ipc::daemon_socket_path().unwrap();
    assert!(socket.starts_with(&runtime));
    assert!(!socket.exists());
    let saved = match case {
        "primary" => Some(instance::window_database_path(instance::PRIMARY_WINDOW).unwrap()),
        "secondary" => Some(instance::window_database_path("inactive-copy").unwrap()),
        _ => None,
    };
    if let Some(path) = &saved {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Existing zero-byte stores still need a qualified reader. Their
        // absence, rather than their apparent contents, permits the shortcut.
        fs::write(path, []).unwrap();
    }
    let lease = if case == "starting" {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(socket.parent().unwrap())
            .unwrap();
        let lease = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(socket.with_file_name("cagentsd.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        Some(lease)
    } else {
        None
    };
    let guard = UiUpdateGuard::acquire().unwrap();
    assert!(!guard.has_running_windows().unwrap());
    let result = inspect_source_panes_mode(&guard, None, false, false);
    match case {
        "empty" => result.unwrap(),
        "primary" | "secondary" => {
            let error = result.unwrap_err().to_string();
            for message in [
                "matched source inspector is unavailable",
                "reinstall the matching cagents and cagentsd",
                "before retrying",
                "source windows remain usable",
            ] {
                assert!(error.contains(message), "missing {message:?}: {error}");
            }
            assert!(fs::read(saved.unwrap()).unwrap().is_empty());
        }
        "starting" => {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("source service is starting or stopping"),
                "{error}"
            );
            let path = socket.with_file_name("cagentsd.lock");
            assert!(fs::read(&path).unwrap().is_empty());
            let probe = File::open(path).unwrap();
            assert_eq!(
                unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                -1,
                "source preflight released the starting owner's lease"
            );
            assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
        }
        _ => unreachable!(),
    }
    assert!(!socket.exists(), "source preflight must not start a daemon");
    assert!(!executable.parent().unwrap().join("cagentsd").exists());
    let fence =
        File::open(runtime.join(format!("{}-ui.lock.update", instance::STORAGE_NAME))).unwrap();
    assert_eq!(
        unsafe { libc::flock(fence.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1,
        "preflight must retain the caller's launch fence"
    );
    assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
    drop(guard);
    assert_eq!(
        unsafe { libc::flock(fence.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "dropping the preflight guard must release its launch fence"
    );
    drop(lease);
}
