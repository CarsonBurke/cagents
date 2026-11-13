use super::*;
use crate::{
    host::{
        model::Registry,
        state::{HostLock, HostState},
    },
    model::{
        BootId, CardId, CardKind, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
        TerminalContinuity, TerminalId, TerminalSession,
    },
    terminal_ipc::{ClientRequest, TerminalSpawnOptions},
    workspace_model::views::{Pane, Scope, View},
};
use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("pending-completion-test"),
        boot_id: BootId::new("pending-completion-test"),
        host_incarnation: HostIncarnation::new("pending-completion-test"),
    }
}

fn session(directory: &std::path::Path) -> TerminalSession {
    let launch = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf 'started\\n' >> launches; exec cat".into(),
        ],
        working_directory: directory.into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
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
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    path: PathBuf,
    scope: Scope,
    pane: Pane,
    session: TerminalSession,
    host: SharedHost,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.sqlite3");
        let mut registry = Registry::at(&path).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: directory.path().into(),
                name: "fixture".into(),
                kind: Default::default(),
            })
            .unwrap();
        let scope = Scope::Worktree {
            repository: directory.path().into(),
            path: directory.path().into(),
        };
        let session = session(directory.path());
        let pane = Pane {
            id: CardId::new(),
            title: "terminal".into(),
            user_title: None,
            pending_terminal_start: true,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Terminal(session.clone()),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![pane.clone()],
                closed: vec![],
            })
            .unwrap();
        let mut state = HostState::new(proof());
        state.install_model(registry);
        state.register_client(1);
        let host = Arc::new(HostLock::new(state));
        Self {
            directory,
            path,
            scope,
            pane,
            session,
            host,
        }
    }

    fn complete(&self) -> Result<(), ProtocolError> {
        crate::host::client::process_request(
            &self.host,
            1,
            ClientRequest::CompletePendingTerminal {
                pane: self.pane.id,
                terminal_id: self.session.id,
                options: TerminalSpawnOptions::default(),
            },
        )
        .map(|_| ())
    }

    fn view(&self) -> View {
        snapshot(&self.host, None)
            .unwrap()
            .views
            .into_iter()
            .find(|view| view.scope == self.scope)
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let (handles, _) = self
            .host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .graceful_shutdown();
        for handle in handles {
            let _ = handle.shutdown();
        }
    }
}

#[test]
fn failed_pending_save_still_publishes_the_worker_and_retry_reuses_it() {
    let fixture = Fixture::new();
    // A real schema object refuses only the completion save; reading the
    // reservation and starting the worker are unaffected.
    rusqlite::Connection::open(&fixture.path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_pending BEFORE UPDATE ON view_scopes BEGIN SELECT RAISE(ABORT, 'fixture'); END;",
        )
        .unwrap();
    fixture.complete().unwrap();
    let original = fixture.host.lock().unwrap().terminals[&fixture.session.id]
        .handle
        .clone();
    assert!(!original.is_completed());
    assert!(fixture.view().panes[0].pending_terminal_start);
    rusqlite::Connection::open(&fixture.path)
        .unwrap()
        .execute_batch("DROP TRIGGER reject_pending")
        .unwrap();
    fixture.complete().unwrap();
    let state = fixture.host.lock().unwrap();
    assert_eq!(state.terminals.len(), 1);
    assert!(
        state.terminals[&fixture.session.id]
            .handle
            .same_worker(&original)
    );
    drop(state);
    let deadline = Instant::now() + Duration::from_secs(3);
    let launches = loop {
        if let Ok(launches) = fs::read_to_string(fixture.directory.path().join("launches")) {
            if !launches.is_empty() {
                break launches;
            }
        }
        assert!(Instant::now() < deadline, "original process did not start");
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(launches, "started\n");
}
