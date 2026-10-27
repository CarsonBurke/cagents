//! Native checkout fixtures run before any application state is captured and
//! outside the parallel libtest process. FD_CLOEXEC cannot stop an unrelated
//! fork from briefly inheriting a live operation lease before that child execs.
use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    time::Duration,
};

// Isolate before application code snapshots GIT_CONFIG_PARAMETERS/COUNT. A
// launch-only executor scrub cannot undo their later controlled re-export.
pub(crate) fn isolated_fixture_case(qualified: &str) -> bool {
    let prefix = concat!(env!("CARGO_CRATE_NAME"), "::");
    let case = qualified.strip_prefix(prefix).unwrap_or(qualified);
    if std::env::var("CAGENTS_PR_FIXTURE_CASE").as_deref() == Ok(case) {
        assert!(
            std::env::vars_os().all(|(name, _)| !name.as_encoded_bytes().starts_with(b"GIT_")),
            "owning child must start before any ambient Git configuration is captured"
        );
        for (marker, setting) in [
            ("CAGENTS_PR_FIXTURE_RUNTIME", "XDG_RUNTIME_DIR"),
            ("CAGENTS_PR_FIXTURE_DATA", "XDG_DATA_HOME"),
        ] {
            let path = std::env::var_os(marker).expect("owned isolated fixture directory");
            assert_eq!(std::env::var_os(setting), Some(path.clone()));
            let metadata = fs::metadata(path).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
            assert_eq!(metadata.mode() & 0o777, 0o700);
        }
        return false;
    }
    let private = |prefix: &str| {
        tempfile::Builder::new()
            .prefix(prefix)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    };
    let runtime = private("cagents-pr-runtime-");
    let data = private("cagents-pr-data-");
    let mut command = CommandSpec::new(std::env::current_exe().unwrap())
        .args([
            case,
            "--exact",
            "--nocapture",
            "--test-threads=1",
            "--color=never",
        ])
        .env("CAGENTS_PR_FIXTURE_CASE", case)
        .env("CAGENTS_PR_FIXTURE_RUNTIME", runtime.path())
        .env("CAGENTS_PR_FIXTURE_DATA", data.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("XDG_DATA_HOME", data.path())
        .timeout(Duration::from_secs(90))
        .output_limits(1024 * 1024, 1024 * 1024);
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            command = command.env_remove(name);
        }
    }
    // Reuse the existing bounded executor instead of writing an unbounded
    // child stream to disk. Private files contain only the bounded result.
    let (success, stdout, stderr, status) =
        match SystemProcessExecutor.execute(&command, &CancellationToken::default()) {
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
    for (name, bytes) in [("case.stdout", &stdout), ("case.stderr", &stderr)] {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(runtime.path().join(name))
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    let named = stdout
        .lines()
        .any(|line| line.contains(&format!("test {case} ... ok")));
    let one = stdout
        .lines()
        .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;"));
    if !success || !named || !one {
        // A controlled holder may still retain these exact authority files.
        // Preserve original evidence on every failed owning-child outcome;
        // outer execution custody decides when cleanup is actually proved.
        let runtime = runtime.keep();
        let data = data.keep();
        panic!(
            "owning case {case}: {status}; named={named}, exact_one={one}; retained runtime={}, data={}\n{stdout}\n{stderr}",
            runtime.display(),
            data.display()
        );
    }
    print!("{stdout}");
    eprint!("{stderr}");
    true
}
