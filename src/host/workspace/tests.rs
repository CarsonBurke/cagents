use super::*;
use crate::{
    host::{
        client::process_request,
        delivery::DeliveryQueue,
        model::Registry,
        state::{HostLock, HostState, Retirement},
    },
    model::{
        BootId, CardId, CardKind, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
        TerminalContinuity, TerminalId, TerminalSession,
    },
    terminal::TerminalHandle,
    terminal_ipc::{ClientRequest, ServerMessage, ServerResponse},
    workspace_model::views::{Pane, Scope},
};
use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

mod delta_delivery_tests;

fn fixture() -> (tempfile::TempDir, SharedHost, Arc<DeliveryQueue>) {
    let directory = tempfile::tempdir().unwrap();
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("workspace-progress"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    });
    state.install_model(Registry::at(&directory.path().join("model.sqlite3")).unwrap());
    state.register_client(1);
    let delivery = state.register_client(2);
    (directory, Arc::new(HostLock::new(state)), delivery)
}

struct Terminal(TerminalHandle);
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.0.shutdown();
    }
}

fn terminal(
    host: &SharedHost,
    root: &std::path::Path,
    script: &str,
) -> (TerminalSession, Terminal) {
    let launch = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), script.into()],
        working_directory: root.to_owned(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let session = TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    };
    let handle =
        Terminal(TerminalHandle::spawn(session.clone(), Default::default(), |_| {}).unwrap());
    host.lock()
        .unwrap()
        .finish_spawn(1, session.clone(), handle.0.clone(), false)
        .unwrap();
    (session, handle)
}

fn open_project(root: &std::path::Path) -> ModelEdit {
    ModelEdit::OpenProject {
        repository: root.to_owned(),
        name: "Fixture".into(),
        kind: Default::default(),
    }
}

#[test]
fn sqlite_writer_contention_keeps_terminal_input_and_inventory_responsive() {
    let (directory, host, _) = fixture();
    let (session, _terminal) = terminal(
        &host,
        directory.path(),
        "while IFS= read -r line; do printf '%s' \"$line\" > received; done",
    );
    let blocker = rusqlite::Connection::open(directory.path().join("model.sqlite3")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let registry = writer(&host).unwrap();
    let (finished, completion) = mpsc::channel();
    let editor = {
        let host = host.clone();
        let edit = open_project(directory.path());
        thread::spawn(move || {
            let result = process_request(&host, 2, ClientRequest::EditWorkspaceModel { edit });
            finished.send(()).unwrap();
            result
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let entered = loop {
        if matches!(
            registry.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ) {
            break true;
        }
        if Instant::now() >= deadline || completion.try_recv().is_ok() {
            break false;
        }
        thread::sleep(Duration::from_millis(1));
    };
    thread::sleep(Duration::from_millis(30));
    let (responsive, response) = mpsc::channel();
    let inspector = {
        let host = host.clone();
        let received = directory.path().join("received");
        thread::spawn(move || {
            let input = process_request(
                &host,
                1,
                ClientRequest::Input {
                    terminal_id: session.id,
                    bytes: b"responsive\n".to_vec(),
                },
            );
            let listed = process_request(&host, 1, ClientRequest::List);
            let deadline = Instant::now() + Duration::from_secs(1);
            while std::fs::read(&received).ok().as_deref() != Some(b"responsive")
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(5));
            }
            responsive
                .send((input, listed, std::fs::read(received)))
                .unwrap();
        })
    };
    let during_contention = response.recv_timeout(Duration::from_secs(2));
    let still_waiting = completion.try_recv().is_err();
    blocker.execute_batch("ROLLBACK").unwrap();
    let edited = editor.join().unwrap();
    inspector.join().unwrap();
    assert!(
        entered && still_waiting,
        "edit must be waiting for the real SQLite writer"
    );
    edited.unwrap();
    let (input, listed, received) =
        during_contention.expect("terminal traffic blocked behind SQLite");
    assert!(matches!(input.unwrap().0, ServerResponse::Accepted));
    assert!(
        matches!(listed.unwrap().0, ServerResponse::Listed { terminals } if terminals.len() == 1)
    );
    assert_eq!(received.unwrap(), b"responsive");
}

#[test]
fn watching_before_snapshot_cannot_lose_a_concurrent_execution_writer_edge() {
    let (directory, host, delivery) = fixture();
    let mut execution = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
    let (reading, reached_read) = mpsc::channel();
    let (resume, proceed) = mpsc::channel();
    let observer = {
        let host = host.clone();
        thread::spawn(move || {
            crate::host::model::pause_at(crate::host::model::Pause::SequenceRead, move || {
                reading.send(()).unwrap();
                proceed.recv_timeout(Duration::from_secs(10)).unwrap();
            });
            snapshot(&host, Some(2))
        })
    };
    reached_read.recv_timeout(Duration::from_secs(10)).unwrap();
    let edit = open_project(directory.path());
    let applied = execution.apply(&edit);
    if let Ok(applied) = &applied {
        host.lock().unwrap().announce_model_edit(
            1,
            applied.model.sequence,
            &edit,
            &applied.effects,
        );
    }
    resume.send(()).unwrap();
    let observed = observer.join().unwrap().unwrap();
    let applied = applied.unwrap();
    delivery.close_after_drain();
    let edges = std::iter::from_fn(|| delivery.pop())
        .filter_map(|envelope| match envelope.message {
            ServerMessage::WorkspaceModelEdited { sequence, edit, .. } => Some((sequence, edit)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        observed.sequence, 0,
        "the snapshot was established before the concurrent commit"
    );
    assert!(
        observed.projects.is_empty(),
        "snapshot rows must match its sequence"
    );
    assert_eq!(edges, vec![(applied.model.sequence, edit)]);
}

#[test]
fn shared_view_publication_preserves_the_exact_worker_retirement_fence() {
    let (directory, host, _) = fixture();
    let edit = open_project(directory.path());
    process_request(&host, 1, ClientRequest::EditWorkspaceModel { edit }).unwrap();
    let (session, terminal) = terminal(&host, directory.path(), "exit 0");
    host.lock()
        .unwrap()
        .terminal_retirements
        .insert(session.id, Retirement::submitting(terminal.0.clone()));
    let edit = ModelEdit::SeedView {
        scope: Scope::Worktree {
            repository: directory.path().to_owned(),
            path: directory.path().to_owned(),
        },
        panes: vec![Pane {
            id: CardId::new(),
            title: "terminal".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Terminal(session.clone()),
        }],
        closed: vec![],
    };
    let before = snapshot(&host, None).unwrap();
    let error = process_request(
        &host,
        1,
        ClientRequest::EditWorkspaceModel { edit: edit.clone() },
    )
    .unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
    assert!(error.retryable);
    assert_eq!(snapshot(&host, None).unwrap(), before);
    host.lock()
        .unwrap()
        .terminal_retirements
        .get_mut(&session.id)
        .unwrap()
        .failure = Some("retained worker".into());
    process_request(&host, 1, ClientRequest::EditWorkspaceModel { edit }).unwrap();
    assert_eq!(snapshot(&host, None).unwrap().views.len(), 1);
}
