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
            .env_remove("CAGENTS_AGENT_AUTHORITY")
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
