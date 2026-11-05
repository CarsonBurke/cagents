#![cfg(feature = "host")]

use std::{
    collections::BTreeMap,
    fs,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use cagents::{
    control::ControlClient,
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{HostedTerminalHandle, TerminalConfig, TerminalEvent},
};

const DEADLINE: Duration = Duration::from_secs(15);

struct PrivateDaemon(Child);

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        // Only this test's child is signalled; production runtime/state are
        // never used. Give the daemon its ordinary terminal-cleanup path.
        if self.0.try_wait().ok().flatten().is_none() {
            // An unreaped child keeps its PID reserved even if it exits just
            // after try_wait, so the signal cannot reach a reused PID.
            unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn hosted_bell_storm_preserves_attention_input_and_completion() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let home = directory.path().join("home");
    fs::create_dir(&runtime).unwrap();
    fs::create_dir(&home).unwrap();
    let log = directory.path().join("daemon.log");
    let mut daemon = PrivateDaemon(
        Command::new(env!("CARGO_BIN_EXE_cagentsd"))
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", directory.path().join("state"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .env("XDG_CONFIG_HOME", directory.path().join("config"))
            .env("XDG_CACHE_HOME", directory.path().join("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let socket = runtime
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let deadline = Instant::now() + DEADLINE;
    let mut control = loop {
        if let Ok(control) = ControlClient::connect(&socket) {
            break control;
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "private daemon exited: {}",
            fs::read_to_string(&log).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "private daemon never became ready"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            r#"
printf 'READY\n'
IFS= read -r line
head -c 1048576 /dev/zero | tr '\000' '\007'
printf 'DONE\n'
IFS= read -r line
printf 'INPUT_ACK\n'
IFS= read -r line
exit 7
"#
            .into(),
        ],
        working_directory: directory.path().into(),
        environment: BTreeMap::from([("HOME".into(), home.to_string_lossy().into_owned())]),
        unset_environment: Default::default(),
    };
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        owned_codex_listener: None,
        predecessor: None,
        recovery_origin: None,
        transcript_path: None,
        resume_hint: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    let id = session.id;
    control.spawn(session.clone(), Default::default()).unwrap();
    let hosted = HostedTerminalHandle::attach_or_spawn_at(
        &socket,
        session,
        TerminalConfig::default(),
        Some(control.proof().clone()),
        |_| {},
    )
    .unwrap();
    hosted.wait_until_attached(DEADLINE).unwrap();
    control.wait_for_text(id, "READY", DEADLINE).unwrap();
    control.write(id, b"go\n".to_vec()).unwrap();
    // The original failure disconnected this exact snapshot-reading client
    // at 512 queued Bell events, before DONE could be observed.
    control.wait_for_text(id, "DONE", DEADLINE).unwrap();
    control.write(id, b"probe\n".to_vec()).unwrap();
    control.wait_for_text(id, "INPUT_ACK", DEADLINE).unwrap();
    control.write(id, b"exit\n".to_vec()).unwrap();
    let events = hosted.events();
    let deadline = Instant::now() + DEADLINE;
    let mut bells = 0;
    loop {
        hosted.acknowledge_redraw(hosted.generation());
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(TerminalEvent::Bell) => bells += 1,
            Ok(TerminalEvent::Completed { exit_code, .. }) => {
                assert_eq!(exit_code, 7);
                break;
            }
            Ok(TerminalEvent::Error { operation, message }) => panic!("{operation}: {message}"),
            Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("hosted connection closed before completion: {error}"),
        }
        assert!(Instant::now() < deadline, "bell burst stranded completion");
    }
    assert!(bells > 0, "the burst must still signal attention");
    assert!(
        bells < 256,
        "bell delivery must fit the hosted semantic queue: {bells}"
    );
    let completed = control
        .list()
        .unwrap()
        .into_iter()
        .find(|terminal| terminal.session.id == id)
        .unwrap();
    assert_eq!(
        control
            .release_completed_terminal(completed.session, false)
            .unwrap(),
        cagents::terminal_ipc::TerminalCloseResult::Closed
    );
}

/// This covers the complete production observer -> daemon descriptor -> built
/// CLI path. The pure CLI peer and in-process Host tests cover other failures.
#[cfg(target_os = "linux")]
#[test]
fn cli_inventory_reads_real_pty_titles_without_exporting_launch_environment() {
    use cagents::process::{
        CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor,
    };
    use std::{os::unix::fs::DirBuilderExt, path::Path};

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let runtime = root.join("runtime");
    let home = root.join("home");
    fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
    fs::DirBuilder::new().mode(0o700).create(&home).unwrap();
    let log = root.join("inventory-daemon.log");
    // Preserve inherited delegation. This fixture never reads, rewrites or
    // clears an agent authority and never launches a provider.
    let mut daemon = PrivateDaemon(
        Command::new(env!("CARGO_BIN_EXE_cagentsd"))
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let socket = runtime
        .join(cagents::instance::RUNTIME_SUBDIRECTORY)
        .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
    let deadline = Instant::now() + DEADLINE;
    let observed = (|| -> Result<_, String> {
        let mut control = loop {
            if let Ok(control) = ControlClient::connect(&socket) {
                break control;
            }
            if daemon.0.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Err(format!(
                    "private daemon exited: {}",
                    fs::read_to_string(&log).unwrap_or_default()
                ));
            }
            if Instant::now() >= deadline {
                return Err("private daemon readiness timed out".into());
            }
            thread::sleep(Duration::from_millis(10));
        };
        let recipe = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                r#"printf '\033]2;CLI inventory title\007'; exec /bin/cat"#.into(),
            ],
            working_directory: root.into(),
            environment: BTreeMap::from([
                ("HOME".into(), home.to_str().unwrap().into()),
                ("FIXTURE_DISPLAY_ONLY".into(), "full descriptor only".into()),
            ]),
            unset_environment: Default::default(),
        };
        let id = TerminalId::new();
        let session = TerminalSession {
            id,
            launch: recipe.clone(),
            owned_codex_listener: None,
            predecessor: None,
            recovery_origin: None,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe,
                last_exit: None,
            },
        };
        control
            .spawn(session, Default::default())
            .map_err(|e| e.to_string())?;
        // Spawn subscribes this connection to redraw snapshots. Inventory reads
        // use a fresh transaction, as the actual CLI does; keep strict reply
        // correlation and require the same original private daemon proof.
        let original_proof = control.proof().clone();
        drop(control);
        let mut control = ControlClient::connect(&socket).map_err(|e| e.to_string())?;
        if control.proof() != &original_proof {
            return Err("private daemon changed between spawn and inventory".into());
        }
        loop {
            let rows = control.list().map_err(|e| e.to_string())?;
            if rows.iter().any(|row| {
                row.session.id == id && row.title.as_deref() == Some("CLI inventory title")
            }) {
                break;
            }
            if Instant::now() >= deadline {
                return Err("production daemon never published its PTY title".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        let cli = |args: &[&str], json: bool| -> Result<(String, String), String> {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("inventory CLI deadline expired".into());
            }
            let executable = Path::new(env!("CARGO_BIN_EXE_cagents"));
            let mut command = CommandSpec::new(executable)
                .args(["--device", "local"])
                .args(args.iter().copied())
                .env("HOME", &home)
                .env("XDG_RUNTIME_DIR", &runtime)
                .env("XDG_STATE_HOME", root.join("state"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .timeout(remaining.min(Duration::from_secs(5)))
                .output_limits(64 * 1024, 64 * 1024);
            if json {
                command = command.arg("--json");
            }
            let output = SystemProcessExecutor
                .execute(&command, &CancellationToken::new())
                .and_then(|output| output.require_success(executable))
                .map_err(|e| e.to_string())?;
            let stdout = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            Ok((stdout, stderr))
        };
        let human = cli(&["terminal", "list", "--all"], false)?;
        let listed_output = cli(&["terminal", "list"], true)?;
        let listed: serde_json::Value =
            serde_json::from_str(&listed_output.0).map_err(|e| e.to_string())?;
        let shown_output = cli(&["terminal", "show", &id.to_string()], true)?;
        let shown: serde_json::Value =
            serde_json::from_str(&shown_output.0).map_err(|e| e.to_string())?;
        Ok((id, human, listed_output, listed, shown))
    })();
    // Close original control/CLI connections and reap the exact private daemon
    // before assertions. The outer family guardian checks descendant settlement.
    drop(daemon);
    let (id, (human, human_stderr), (listed_stdout, listed_stderr), listed, shown) =
        observed.unwrap();
    assert!(human.starts_with("TERMINAL"), "{human}");
    assert!(human.contains("CLI inventory title"), "{human}");
    assert!(human.contains("running"), "{human}");
    // Inspect both original streams: parsing JSON can discard duplicate fields,
    // and a diagnostic on stderr is still visible to the CLI user. Retain the
    // parsed check as well so JSON escapes cannot conceal the fixture canaries.
    let listed_json = listed.to_string();
    for (kind, output) in [
        ("human stdout", &human),
        ("human stderr", &human_stderr),
        ("JSON stdout", &listed_stdout),
        ("JSON stderr", &listed_stderr),
        ("JSON decoded", &listed_json),
    ] {
        for canary in ["FIXTURE_DISPLAY_ONLY", "full descriptor only"] {
            assert!(
                !output.contains(canary),
                "{kind} terminal list exposed launch-environment fixture canary {canary:?}"
            );
        }
    }
    let row = &listed["data"]["terminals"][0];
    assert_eq!(row["id"], id.to_string());
    assert_eq!(row["state"], "running");
    assert_eq!(row["title"], "CLI inventory title");
    for field in ["session", "launch", "environment", "recipe", "resume_hint"] {
        assert!(row.get(field).is_none());
    }
    assert_eq!(shown["data"]["terminal"]["session"]["id"], id.to_string());
    assert_eq!(
        shown["data"]["terminal"]["session"]["launch"]["environment"]["FIXTURE_DISPLAY_ONLY"],
        "full descriptor only"
    );
}
