#![cfg(target_os = "linux")]
//! Exercise the standalone command and real generated completions offline.
use cagents::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
use std::{path::Path, time::Duration};

fn cli(arguments: &[&str], directory: &Path) -> Vec<u8> {
    let executable = Path::new(env!("CARGO_BIN_EXE_cagents"));
    let command = CommandSpec::new(executable)
        .args(arguments.iter().copied())
        .env("HOME", directory)
        .env("XDG_CONFIG_HOME", directory.join("config"))
        .env("XDG_DATA_HOME", directory.join("data"))
        .env("XDG_CACHE_HOME", directory.join("cache"))
        .env("XDG_RUNTIME_DIR", directory.join("runtime"))
        .timeout(Duration::from_secs(10))
        .output_limits(1024 * 1024, 64 * 1024);
    SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .unwrap()
        .require_success(executable)
        .unwrap()
        .stdout
}

#[test]
fn standalone_help_and_shell_completions_work_without_a_device_or_daemon() {
    let directory = tempfile::tempdir().unwrap();
    let help = String::from_utf8(cli(
        &["--device", "missing-device", "agent", "spawn", "--help"],
        directory.path(),
    ))
    .unwrap();
    for value in [
        "--account",
        "--model",
        "--permission-mode",
        "acceptEdits",
        "on-request",
        "always-ask",
    ] {
        assert!(help.contains(value), "missing {value}: {help}");
    }
    let mut fish_script = Vec::new();
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let script = cli(
            &["--device", "missing-device", "completions", shell],
            directory.path(),
        );
        assert!(!script.is_empty(), "{shell} completion script is empty");
        assert!(String::from_utf8_lossy(&script).contains("spawn"));
        if shell == "fish" {
            fish_script = script;
        }
    }
    assert!(
        !directory.path().join("runtime").exists(),
        "offline commands created a runtime directory"
    );
    let fish = Path::new("/usr/bin/fish");
    if fish.exists() {
        let mut failing_script = fish_script.clone();
        failing_script.extend_from_slice(b"\nfalse\n");
        let command = CommandSpec::new(fish)
            .args([
                "--no-config",
                "-c",
                "source /dev/stdin; and complete -C 'cagents agent sp'",
            ])
            .env("HOME", directory.path())
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_CACHE_HOME", directory.path().join("cache"))
            .stdin(fish_script)
            .timeout(Duration::from_secs(5))
            .output_limits(64 * 1024, 64 * 1024);
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap()
            .require_success(fish)
            .unwrap();
        let completions = String::from_utf8(output.stdout).unwrap();
        assert!(
            completions
                .lines()
                .any(|line| line.split('\t').next() == Some("spawn")),
            "{completions}"
        );

        // Registration succeeds before the appended false. Its source failure
        // must remain a failure instead of being hidden by a later completion.
        let failed = SystemProcessExecutor
            .execute(
                &command.clone().stdin(failing_script),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(failed.exit_code, Some(1), "{failed:?}");
        assert!(!failed.success);
        assert!(failed.stdout.is_empty(), "{failed:?}");
        eprintln!("coverage: fish completion and failed-source guard exercised");
    } else {
        eprintln!("coverage: /usr/bin/fish unavailable; fish source-status checks UNEXECUTED");
    }
}
