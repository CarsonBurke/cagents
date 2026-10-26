//! Bounded private CLI capture, including responses larger than a pipe buffer.
use cagents::process::{
    CancellationToken, CommandSpec, ProcessExecutor, ProcessFailure, ProcessFailureKind,
    SystemProcessExecutor,
};
use std::{
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{ExitStatus, Output},
    time::Instant,
};

const STREAM_LIMIT: usize = 4 * 1024 * 1024;

pub fn private_cli(root: &Path) -> CommandSpec {
    let mut spec = CommandSpec::new(env!("CARGO_BIN_EXE_cagents"))
        .arg("--json")
        .cwd(root);
    // Match the fixture's env_clear Command without inheriting the invoking
    // provider session, account overrides, or application profile.
    for (name, _) in std::env::vars_os() {
        spec = spec.env_remove(name);
    }
    for (name, leaf) in [
        ("HOME", "home"),
        ("XDG_RUNTIME_DIR", "run"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
    ] {
        spec = spec.env(name, root.join(leaf));
    }
    spec.env(
        "PATH",
        format!("{}:/usr/bin:/bin", root.join("bin").display()),
    )
}

pub fn capture(spec: CommandSpec, deadline: Instant) -> Result<Output, ProcessFailure> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ProcessFailure {
            kind: ProcessFailureKind::TimedOut,
            program: spec.program,
            exit_code: None,
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            message: "private CLI deadline expired before spawn".into(),
            started: false,
        });
    }
    // The shared executor drains both streams while waiting, bounds capture
    // and teardown, and reaps the original child on timeout or output overflow.
    let output = SystemProcessExecutor.execute(
        &spec
            .timeout(remaining)
            .output_limits(STREAM_LIMIT, STREAM_LIMIT),
        &CancellationToken::new(),
    )?;
    let status = match (output.exit_code, output.signal) {
        (Some(code), None) => ExitStatus::from_raw(code << 8),
        (None, Some(signal)) => ExitStatus::from_raw(signal),
        _ => unreachable!("executor returned without an exit code or signal"),
    };
    Ok(Output {
        status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn private_program(root: &Path, program: &str) -> CommandSpec {
        let mut spec = private_cli(root);
        spec.program = program.into();
        spec.arguments.clear();
        spec
    }

    #[test]
    fn captures_both_large_streams_and_preserves_nonzero_exit() {
        const BYTES: usize = 1024 * 1024 + 65536;
        let root = tempfile::tempdir().unwrap();
        let spec = private_program(root.path(), "/bin/sh").args([
            "-c",
            "head -c 1114112 /dev/zero; head -c 1114112 /dev/zero >&2; exit 7",
        ]);
        let output = capture(spec, Instant::now() + Duration::from_secs(10)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, vec![0; BYTES]);
        assert_eq!(output.stderr, vec![0; BYTES]);
    }

    #[test]
    fn private_profile_and_directory_survive_capture() {
        let root = tempfile::tempdir().unwrap();
        let spec = private_program(root.path(), "/usr/bin/env");
        let output = capture(spec, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        let environment = String::from_utf8(output.stdout).unwrap();
        let actual: std::collections::BTreeMap<_, _> = environment
            .lines()
            .map(|line| line.split_once('=').unwrap())
            .collect();
        assert_eq!(actual.len(), 7, "inherited environment entered private CLI");
        assert_eq!(actual["HOME"], root.path().join("home").to_str().unwrap());
        assert_eq!(actual["TMPDIR"], root.path().join("tmp").to_str().unwrap());

        let spec = private_program(root.path(), "/bin/pwd");
        let output = capture(spec, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            root.path().canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[test]
    fn expired_deadline_does_not_spawn_and_overflow_is_bounded() {
        let failure = capture(CommandSpec::new("/must-not-spawn"), Instant::now()).unwrap_err();
        assert_eq!(failure.kind, ProcessFailureKind::TimedOut);
        assert!(!failure.started);

        let root = tempfile::tempdir().unwrap();
        let spec = private_program(root.path(), "/bin/sh")
            .args(["-c", "head -c 4194305 /dev/zero; exec sleep 300"]);
        let failure = capture(spec, Instant::now() + Duration::from_secs(10)).unwrap_err();
        assert_eq!(failure.kind, ProcessFailureKind::OutputLimit);
        assert!(failure.started);
        assert!(failure.stdout.len() <= 64 * 1024);
        assert!(failure.stderr.len() <= 64 * 1024);
    }

    #[test]
    fn live_child_timeout_keeps_bounded_output_and_original_deadline() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("ready");
        let spec = private_program(root.path(), "/bin/sh")
            .args([
                "-c",
                "printf ready; printf diagnostic >&2; printf ready > \"$READY\"; exec sleep 300",
            ])
            .env("READY", &marker);
        let failure = capture(spec, Instant::now() + Duration::from_secs(2)).unwrap_err();
        assert_eq!(failure.kind, ProcessFailureKind::TimedOut);
        assert!(failure.started);
        assert!(marker.exists(), "fixture never reached the stalled command");
        assert_eq!(failure.stdout, b"ready");
        assert_eq!(failure.stderr, b"diagnostic");
    }
}
