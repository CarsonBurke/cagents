// Shared by the library's sparse-index contract and the standalone scale contract.
use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Bound stream summaries within the parent's 1 MiB cap. ProcessFailure keeps
/// only a 64 KiB prefix, so also retain the latest tiny records independently.
/// Never log argv, stdin payloads or environment values here.
pub(super) fn diagnostic(message: std::fmt::Arguments<'_>) {
    static REMAINING: Mutex<usize> = Mutex::new(256 * 1024);
    let record = format!("{message}\n");
    assert!(record.len() <= 4096, "oversized scale diagnostic record");
    let journal = if record.starts_with("PATHSPEC_PHASE_") {
        Some("phase-progress")
    } else if record.starts_with("PATHSPEC_GIT_") {
        Some("git-progress")
    } else {
        None
    };
    if let Some(name) = journal {
        let directory = PathBuf::from(
            std::env::var_os("CAGENTS_LARGE_PATHSPEC_ROOT")
                .expect("owning private contract namespace"),
        );
        // Publish atomically before entering the operation. A timeout during
        // this write leaves the previous complete record, never a partial one.
        let mut temporary = tempfile::NamedTempFile::new_in(&directory).unwrap();
        temporary.write_all(record.as_bytes()).unwrap();
        temporary.flush().unwrap();
        temporary.persist(directory.join(name)).unwrap();
    }
    let mut remaining = REMAINING.lock().unwrap_or_else(|p| p.into_inner());
    if *remaining == 0 {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    if record.len() >= *remaining {
        *remaining = 0;
        stderr
            .write_all(b"PATHSPEC_DIAGNOSTICS_TRUNCATED\n")
            .unwrap();
    } else {
        *remaining -= record.len();
        stderr.write_all(record.as_bytes()).unwrap();
    }
    stderr.flush().unwrap();
}

/// An unfinished BEGIN remains in the private journal if the deadline wins.
pub(super) fn phase<T>(name: &str, items: usize, operation: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    diagnostic(format_args!(
        "PATHSPEC_PHASE_BEGIN phase={name} items={items} elapsed_ms=0"
    ));
    let result = operation();
    diagnostic(format_args!(
        "PATHSPEC_PHASE_END phase={name} items={items} elapsed_ms={}",
        started.elapsed().as_millis()
    ));
    result
}

fn report_latest_progress(directory: &Path) {
    for name in ["phase-progress", "git-progress"] {
        let record = (|| -> std::io::Result<String> {
            let mut record = String::new();
            fs::File::open(directory.join(name))?
                .take(4097)
                .read_to_string(&mut record)?;
            Ok(record)
        })();
        match record {
            Ok(record) if record.len() <= 4096 => {
                eprintln!("Latest scale {name} (diagnostic only, not custody proof):\n{record}");
            }
            Ok(_) => eprintln!("Latest scale {name}: oversized record refused"),
            Err(error) => eprintln!("Latest scale {name}: unavailable ({:?})", error.kind()),
        }
    }
}

/// Runs `contract` as the only test of a private libtest subprocess,
/// against a repository in a 0700 namespace under `/var/tmp`; the
/// fixture repositories are too large for a RAM-backed temporary
/// directory.
///
/// The namespace is removed only once the owner has proven that no
/// member of the child's process family can still write into it. The
/// executor's successful result is not that proof: it means the child
/// was reaped and every holder of its output pipes closed them, but the
/// Git processes the child starts hold only the child's own pipes, and
/// lead process groups of their own. One that outlives its call, as
/// Git's detached auto-maintenance can, is invisible to the owner. So
/// the child makes itself the subreaper of its whole family and reaps
/// until none remains before its test reports ok; the owner removes the
/// namespace only after asserting that report, and a removal failure
/// then fails the test: it means a broken proof or a host fault, and
/// staying silent would leak the fixture. On failure or timeout the
/// namespace is retained, with its path printed.
pub(super) fn isolated_large_pathspec_test(name: &str, contract: impl FnOnce(&Path)) {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let name = name
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(name);
    if std::env::var("CAGENTS_LARGE_PATHSPEC_CONTRACT").as_deref() == Ok(name) {
        // SAFETY: prctl takes no pointers here. Adopting orphans is
        // confined to this dedicated single-test subprocess.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        let root = Path::new(
            &std::env::var_os("CAGENTS_LARGE_PATHSPEC_ROOT")
                .expect("owning private contract namespace"),
        )
        .join("repository");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        contract(&root);
        eprintln!("Contract passed; settling its process family");
        // The kernel reparents an orphan to this subreaper as its
        // parent exits, so while any descendant lives this process
        // still has a child of its own; ECHILD is settlement.
        loop {
            // SAFETY: a null status pointer is valid for waitpid.
            if unsafe { libc::waitpid(-1, std::ptr::null_mut(), 0) } > 0 {
                continue;
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ECHILD) => break,
                Some(libc::EINTR) => {}
                _ => panic!("settling the contract's process family: {error}"),
            }
        }
        return;
    }

    /// Retained and named if the owner panics, removed if it returns.
    struct Namespace(PathBuf);
    impl Drop for Namespace {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("Retained pathspec contract root: {}", self.0.display());
            } else if let Err(error) = fs::remove_dir_all(&self.0)
                // A manual sweep of these namespaces may have won.
                && error.kind() != std::io::ErrorKind::NotFound
            {
                panic!(
                    "removing settled pathspec contract root {}: {error}",
                    self.0.display()
                );
            }
        }
    }
    let directory = Namespace(
        tempfile::Builder::new()
            .prefix("cagents-pathspec-contract-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/var/tmp")
            .unwrap()
            .keep(),
    );
    let mut command = CommandSpec::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--nocapture",
            "--test-threads=1",
            "--color=never",
        ])
        .timeout(Duration::from_secs(240))
        .output_limits(1024 * 1024, 1024 * 1024);
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command = command.env_remove(key);
        }
    }
    command = command
        .env("CAGENTS_LARGE_PATHSPEC_CONTRACT", name)
        .env("CAGENTS_LARGE_PATHSPEC_ROOT", &directory.0)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "4")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_1", "/dev/null")
        .env("GIT_CONFIG_KEY_2", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env("GIT_CONFIG_KEY_3", "init.templateDir")
        .env("GIT_CONFIG_VALUE_3", "");
    let (success, stdout, stderr, status) =
        match SystemProcessExecutor.execute(&command, &CancellationToken::new()) {
            Ok(output) => (
                output.success,
                output.stdout,
                output.stderr,
                format!("exit {:?}, signal {:?}", output.exit_code, output.signal),
            ),
            Err(failure) => (
                false,
                failure.stdout,
                failure.stderr,
                format!("{:?}: {}", failure.kind, failure.message),
            ),
        };
    for (name, bytes) in [("stdout", &stdout), ("stderr", &stderr)] {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.0.join(name))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    if !success {
        // Print outside ProcessFailure's prefix so a late phase remains visible
        // even when the captured child stderr excerpt omitted it.
        report_latest_progress(&directory.0);
    }
    assert!(success, "{status}\n{stdout}\n{stderr}");
    assert!(
        stdout
            .lines()
            .any(|line| line.contains(&format!("test {name} ... ok"))),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "{stdout}"
    );
    print!("{stdout}");
    eprint!("{stderr}");
}
