//! Real SQLite obstruction must not serialize an unrelated framed PTY client.
use super::{pending_tests::Cleanup, *};
use crate::{
    control::ControlClient,
    host::{
        model::Registry,
        state::{HostLock, HostState},
    },
    model::{
        HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalContinuity,
        TerminalId, TerminalSession,
    },
    terminal_ipc::{ClientRequest, TerminalCloseResult, TerminalSpawnOptions},
};
use std::{
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

#[test]
fn sqlite_write_lock_keeps_real_terminal_input_and_framed_inventory_progressing() {
    let name = thread::current().name().unwrap().to_owned();
    let Some(root) = super::pending_tests::isolated(&name) else {
        return;
    };
    let path = root.join("model.sqlite3");
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("writer-progress"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("writer-progress"),
    });
    state.install_model(Registry::at(&path).unwrap());
    let host = Arc::new(HostLock::new(state));
    let _cleanup = Cleanup(host.clone());
    let (stream, server) = UnixStream::pair().unwrap();
    let serving_host = host.clone();
    let serving = thread::spawn(move || crate::host::client::serve(server, 1, serving_host));
    let mut client = ControlClient::from_stream(stream).unwrap();
    let recipe = LaunchRecipe {
        program: "/usr/bin/python3".into(),
        arguments: vec!["-u".into(), "-c".into(), "import sys\nprint('PTY_READY',flush=True)\nfor line in sys.stdin: print('ACTUAL_REPLY:'+line.strip(),flush=True)".into()],
        working_directory: root.clone(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    };
    client
        .spawn(session.clone(), TerminalSpawnOptions::default())
        .unwrap();
    client
        .wait_for_text(session.id, "PTY_READY", Duration::from_secs(5))
        .unwrap();
    let original = host.lock().unwrap().terminals[&session.id].handle.clone();
    let root_pidfd = original.original_root_pidfd_for_test().unwrap();
    let fdinfo =
        std::fs::read_to_string(format!("/proc/self/fdinfo/{}", root_pidfd.as_raw_fd())).unwrap();
    let pid = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    let original_process = crate::daemon::holder_registry::Process::read(pid).unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let registry = writer(&host).unwrap();
    let editing = host.clone();
    let repository = root.clone();
    let (done, answer) = mpsc::channel();
    let editing_thread = thread::spawn(move || {
        let result = crate::host::client::process_request(
            &editing,
            2,
            ClientRequest::EditWorkspaceModel {
                edit: ModelEdit::OpenProject {
                    repository,
                    name: "committed-after-unlock".into(),
                    kind: Default::default(),
                },
            },
        );
        done.send(result).unwrap();
    });
    // The actual writer mutex is held while the separate SQLite connection
    // owns BEGIN IMMEDIATE. No sleep guesses whether the edit thread ran.
    let deadline = Instant::now() + Duration::from_secs(2);
    while registry.try_lock().is_ok() {
        assert!(
            Instant::now() < deadline,
            "writer never entered its SQL obstruction"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let (advanced, progress) = mpsc::channel();
    let terminal_id = session.id;
    let terminal_thread = thread::spawn(move || {
        assert!(
            client
                .list()
                .unwrap()
                .iter()
                .any(|item| item.session.id == terminal_id)
        );
        client
            .write(terminal_id, b"while-sql-locked\n".to_vec())
            .unwrap();
        let rendered = client
            .wait_for_text(
                terminal_id,
                "ACTUAL_REPLY:while-sql-locked",
                Duration::from_secs(5),
            )
            .unwrap();
        assert!(rendered.text.contains("ACTUAL_REPLY:while-sql-locked"));
        advanced.send(()).unwrap();
        client
    });
    let responsive = progress.recv_timeout(Duration::from_millis(500)).is_ok();
    // Release the SQL owner before joining/asserting, even on a regression.
    blocker.execute_batch("ROLLBACK").unwrap();
    let result = answer.recv_timeout(Duration::from_secs(5)).unwrap();
    editing_thread.join().unwrap();
    let mut client = terminal_thread.join().unwrap();
    assert!(
        responsive,
        "a blocked model write serialized the real framed PTY client"
    );
    assert!(result.is_ok());
    assert!(
        host.lock().unwrap().terminals[&session.id]
            .handle
            .same_worker(&original)
    );
    let model = client.read_workspace_model().unwrap();
    assert!(
        model
            .projects
            .iter()
            .any(|project| project.name == "committed-after-unlock")
    );
    assert_eq!(
        client.close_terminal_checked(session.id, true).unwrap(),
        TerminalCloseResult::Closed
    );
    assert!(original.is_completed());
    assert!(!original_process.is_current());
    let mut ready = libc::pollfd {
        fd: root_pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut ready, 1, 0) }, 1);
    assert_ne!(ready.revents & libc::POLLIN, 0);
    drop(client);
    serving.join().unwrap();
    assert!(host.lock().unwrap().clients.is_empty());
}
