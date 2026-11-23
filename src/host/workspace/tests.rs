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
    sync::{Mutex, OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

mod delta_delivery_tests;
mod removal_tests;

type BusyGate = (mpsc::Sender<()>, mpsc::Receiver<()>);

// The registry connection invokes this from its actual SQLite busy loop. The
// test below uses it to distinguish an editor blocked in BEGIN IMMEDIATE from
// one that merely acquired the Rust writer mutex but has not reached SQLite.
static TITLE_BUSY_GATE: OnceLock<Mutex<Option<BusyGate>>> = OnceLock::new();

fn title_busy_gate(attempt: i32) -> bool {
    let gate = TITLE_BUSY_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some((entered, proceed)) = gate
        && (entered.send(()).is_err() || proceed.recv_timeout(Duration::from_secs(5)).is_err())
    {
        return false;
    }
    thread::sleep(Duration::from_millis(1));
    attempt < 5_000
}

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

/// A title suggestion has to authenticate against a resident conversation,
/// rather than trusting a pane's stale serialized launch recipe.
fn title_chat(host: &SharedHost, root: &std::path::Path) -> crate::chat::Session {
    use crate::chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec};

    struct Quiet;
    impl Adapter for Quiet {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }

        fn command(&mut self, _: crate::chat::Command) -> Result<Step, String> {
            Ok(Step::default())
        }

        fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }

    let recipe = LaunchRecipe {
        program: "claude".into(),
        arguments: Vec::new(),
        working_directory: root.to_owned(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let mut spec = ProcessSpec::from(&recipe);
    spec.transport = TransportSpec::History;
    let mut initial = Step::default();
    let store = crate::chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle =
        crate::chat::runtime::Handle::start(spec, Box::new(Quiet), initial, store, None, |_| {})
            .unwrap();
    let session = crate::chat::Session {
        id: TerminalId::new(),
        proof: Some(host.lock().unwrap().proof().clone()),
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe,
            session_id: None,
            draft_model: None,
        },
    };
    host.lock().unwrap().chats.insert(
        session.id,
        crate::host::chat::Record {
            execution_instance: uuid::Uuid::new_v4(),
            console_recipe: None,
            paired_terminal: None,
            session: session.clone(),
            handle,
            source_process: None,
            subscribers: Default::default(),
            observations: Default::default(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
    session
}

#[test]
fn sqlite_writer_contention_keeps_terminal_input_and_inventory_responsive() {
    assert_sqlite_progress(false);
}

#[test]
fn title_suggestion_sqlite_contention_keeps_the_host_lock_available() {
    let (directory, host, _) = fixture();
    let session = title_chat(&host, directory.path());
    let channel = crate::model::ChannelId::new();
    process_request(
        &host,
        1,
        ClientRequest::ApplyWorkspaceEdit {
            edit: open_project(directory.path()),
        },
    )
    .unwrap();
    process_request(
        &host,
        1,
        ClientRequest::ApplyWorkspaceEdit {
            edit: ModelEdit::CreateChannel {
                id: channel,
                repository: directory.path().to_owned(),
                title: String::new(),
                title_source: Default::default(),
                checkout: None,
            },
        },
    )
    .unwrap();
    let card = CardId::new();
    let response = process_request(
        &host,
        1,
        ClientRequest::ApplyWorkspaceEdit {
            edit: ModelEdit::SeedView {
                scope: Scope::Channel(channel),
                panes: vec![Pane {
                    id: card,
                    title: "Chat".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 1.,
                    scope: Some(directory.path().to_owned()),
                    kind: CardKind::Chat(session.clone()),
                }],
                closed: Vec::new(),
            },
        },
    )
    .unwrap();
    let ServerResponse::WorkspaceEdit(receipt) = response.0 else {
        panic!("seeding the conversation pane must return its delta receipt")
    };
    let revision = receipt.effects.views[0]
        .after
        .expect("newly seeded view has a revision");
    let blocker = rusqlite::Connection::open(directory.path().join("model.sqlite3")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let registry = writer(&host).unwrap();
    let (entered, busy_wait) = mpsc::channel();
    let (continue_busy_wait, proceed) = mpsc::channel();
    *TITLE_BUSY_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((entered, proceed));
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .set_busy_handler_for_test(Some(title_busy_gate))
        .unwrap();
    let editor = {
        let host = host.clone();
        thread::spawn(move || {
            process_request(
                &host,
                1,
                ClientRequest::ApplyWorkspaceEdit {
                    edit: ModelEdit::SuggestConversationTitle {
                        id: channel,
                        card,
                        expected_revision: revision,
                        title: "A prompt title".into(),
                        source: crate::model::ChannelTitleSource::Prompt,
                    },
                },
            )
        })
    };
    busy_wait
        .recv_timeout(Duration::from_secs(5))
        .expect("title publication did not enter SQLite's BEGIN IMMEDIATE busy wait");
    let (listed, response) = mpsc::channel();
    let inspector = {
        let host = host.clone();
        thread::spawn(move || {
            listed
                .send(process_request(&host, 2, ClientRequest::List))
                .unwrap();
        })
    };
    let during_contention = response.recv_timeout(Duration::from_secs(1));
    continue_busy_wait.send(()).unwrap();
    blocker.execute_batch("ROLLBACK").unwrap();
    let result = editor.join().unwrap();
    inspector.join().unwrap();
    let (reply, _, _) = during_contention
        .expect("terminal inventory blocked behind a title suggestion's SQLite wait")
        .unwrap();
    assert!(matches!(reply, ServerResponse::Listed { .. }));
    assert!(matches!(
        result.unwrap().0,
        ServerResponse::WorkspaceEdit(_)
    ));
    host.lock()
        .unwrap()
        .chats
        .remove(&session.id)
        .unwrap()
        .handle
        .close()
        .unwrap();
}

#[test]
fn source_checked_removal_sqlite_contention_keeps_terminal_input_and_inventory_responsive() {
    assert_sqlite_progress(true);
}

fn assert_sqlite_progress(removing: bool) {
    let (directory, host, _) = fixture();
    let (session, _terminal) = terminal(
        &host,
        directory.path(),
        "while IFS= read -r line; do printf '%s' \"$line\" > received; done",
    );
    let edit = if removing {
        edit(&host, 1, open_project(directory.path())).unwrap();
        let scope = Scope::Worktree {
            repository: directory.path().to_owned(),
            path: directory.path().to_owned(),
        };
        let source = host
            .lock()
            .unwrap()
            .descriptor(1, session.id)
            .unwrap()
            .session;
        let pane = Pane {
            id: CardId::new(),
            title: "Contention fixture".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: CardKind::Terminal(source),
            scope: None,
        };
        let model = edit(
            &host,
            1,
            ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            },
        )
        .unwrap();
        ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: model.views[0].revision,
            operation: uuid::Uuid::new_v4(),
            change: Change::RemoveSource {
                registry: model.identity,
                repository: directory.path().to_owned(),
                before: pane.clone(),
                terminal: Some(
                    crate::workspace_model::views::TerminalRemovalBasis::Worker {
                        owner: host.lock().unwrap().proof().clone(),
                        instance: _terminal.0.execution_instance(),
                    },
                ),
                capture: Some(crate::workspace_model::views::CapturedTerminalSource {
                    registry: model.identity,
                    repository: directory.path().to_owned(),
                    scope,
                    view_revision: model.views[0].revision,
                    model_sequence: model.sequence,
                    before: Box::new(pane),
                }),
            },
        }
    } else {
        open_project(directory.path())
    };
    let blocker = rusqlite::Connection::open(directory.path().join("model.sqlite3")).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let registry = writer(&host).unwrap();
    let (finished, completion) = mpsc::channel();
    let editor = {
        let host = host.clone();
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

/// `project close` sends the channels it showed the user. A window that
/// creates a channel after that read, before the close commits, must keep it:
/// the cascade may only take what was confirmed. Once the confirmed set
/// matches, watchers are told of the close as the plain close they fold.
#[test]
fn a_confirmed_project_close_cannot_take_a_channel_created_after_confirmation() {
    let (_directory, host, delivery) = fixture();
    // A removed checkout: the key alone must be enough to close it.
    let repository = std::path::PathBuf::from("/cagents-removed-checkout/.git");
    process_request(&host, 2, ClientRequest::WatchWorkspaceModel).unwrap();
    process_request(
        &host,
        1,
        ClientRequest::EditWorkspaceModel {
            edit: open_project(&repository),
        },
    )
    .unwrap();
    let confirmed = snapshot(&host, None).unwrap();
    assert!(
        confirmed.channels.is_empty(),
        "the CLI read an empty project"
    );
    let late = crate::model::ChannelId::new();
    process_request(
        &host,
        2,
        ClientRequest::EditWorkspaceModel {
            edit: ModelEdit::CreateChannel {
                id: late,
                repository: repository.clone(),
                title: "created by another window".into(),
                title_source: Default::default(),
                checkout: None,
            },
        },
    )
    .unwrap();
    let before = snapshot(&host, None).unwrap();
    let close = |channels: &[crate::model::ChannelId]| ClientRequest::EditWorkspaceModel {
        edit: ModelEdit::CloseConfirmedProject {
            repository: repository.clone(),
            channels: channels.iter().copied().collect(),
        },
    };
    let error = process_request(&host, 1, close(&[])).unwrap_err();
    assert_eq!(error.code, ProtocolErrorCode::ViewConflict);
    assert!(error.retryable, "the CLI reads the project again");
    assert_eq!(
        snapshot(&host, None).unwrap(),
        before,
        "a refused close removes nothing"
    );
    process_request(&host, 1, close(&[late, crate::model::ChannelId::new()])).unwrap_err();
    assert_eq!(snapshot(&host, None).unwrap(), before);

    process_request(&host, 1, close(&[late])).unwrap();
    let after = snapshot(&host, None).unwrap();
    assert!(after.project(&repository).is_none());
    assert!(after.channel(late).is_none());
    delivery.close_after_drain();
    let last = std::iter::from_fn(|| delivery.pop())
        .filter_map(|envelope| match envelope.message {
            ServerMessage::WorkspaceModelEdited { edit, effects, .. } => Some((edit, effects)),
            _ => None,
        })
        .last()
        .expect("the watching window is told of the close");
    assert_eq!(last.0, ModelEdit::CloseProject { repository });
    assert_eq!(last.1.removed_channels, [late]);
}
