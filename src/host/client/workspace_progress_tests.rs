use super::*;
use crate::{
    host::{
        model::Registry,
        state::{HostLock, HostState},
    },
    model::{BootId, HostIncarnation, HostSessionId},
    workspace_model::ModelEdit,
};
use std::sync::mpsc;

#[test]
fn terminal_inventory_progresses_while_workspace_commit_is_paused() {
    let directory = tempfile::tempdir().unwrap();
    let host = Arc::new(HostLock::new(HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("workspace-commit-test"),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new("incarnation"),
    })));
    {
        let mut state = host.lock().unwrap();
        state.install_model(Registry::at(&directory.path().join("model.sqlite3")).unwrap());
        state.register_client(1);
        state.register_client(2);
    }
    let (committing, reached_commit) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    let editor = {
        let host = host.clone();
        let repository = directory.path().to_owned();
        thread::spawn(move || {
            crate::host::model::pause_at(crate::host::model::Pause::BeforeCommit, move || {
                committing.send(()).unwrap();
                proceed.recv_timeout(Duration::from_secs(20)).unwrap();
            });
            process_request(
                &host,
                1,
                ClientRequest::EditWorkspaceModel {
                    edit: ModelEdit::OpenProject {
                        repository,
                        name: "Fixture".into(),
                        kind: Default::default(),
                    },
                },
            )
        })
    };
    reached_commit
        .recv_timeout(Duration::from_secs(20))
        .unwrap();
    let (listed, response) = mpsc::channel();
    let inspector = {
        let host = host.clone();
        thread::spawn(move || {
            listed
                .send(process_request(&host, 2, ClientRequest::List))
                .unwrap();
        })
    };
    let during_commit = response.recv_timeout(Duration::from_secs(1));
    // Always release and join our threads, including on the unfixed control.
    release.send(()).unwrap();
    editor.join().unwrap().unwrap();
    inspector.join().unwrap();
    let (reply, _, _) = during_commit
        .expect("terminal inventory blocked behind workspace commit")
        .unwrap();
    assert!(matches!(reply, ServerResponse::Listed { .. }));
}
