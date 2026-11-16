//! Real socket clients and daemon delivery; these tests do not run a native UI.

use super::*;
use crate::{
    control::{ControlClient, ControlError},
    host::{client::serve, delivery::Outbound},
    model::{BrowserId, BrowserRestoreFidelity, BrowserSession, ChannelId},
    terminal_ipc::{CANDIDATE_LINE_PROTOCOL, older_client},
    workspace_model::{
        delta::{EditReceipt, Mirror},
        views::{Change, View},
    },
};
use std::{os::unix::net::UnixStream, sync::Barrier};

struct Connection {
    client: ControlClient,
    server: Option<thread::JoinHandle<()>>,
}

impl Connection {
    fn new(host: &SharedHost, id: ClientId) -> Self {
        let (stream, peer) = UnixStream::pair().unwrap();
        let host = Arc::clone(host);
        let server = thread::spawn(move || serve(peer, id, host));
        Self {
            client: ControlClient::from_stream(stream).unwrap(),
            server: Some(server),
        }
    }

    fn next_edge(&mut self) -> crate::control::ModelEdge {
        // `next_workspace_model_edit` intentionally has no transport timeout.
        // A missing publication must fail this test rather than hang its lane.
        let hangup = self.client.hangup().unwrap();
        let (cancel, wait) = mpsc::channel();
        let watchdog = thread::spawn(move || {
            if wait.recv_timeout(Duration::from_secs(10)).is_err() {
                hangup.hang_up();
            }
        });
        let edge = self.client.next_workspace_model_edit();
        let _ = cancel.send(());
        watchdog.join().unwrap();
        edge.expect("the other window's committed edit must be delivered")
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.client.hangup().unwrap().hang_up();
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

fn browser() -> Pane {
    Pane {
        id: CardId::new(),
        title: "Fixture browser".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: Vec::new(),
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    }
}

fn apply(host: &SharedHost, edit: ModelEdit) -> EditReceipt {
    match process_request(host, 1, ClientRequest::ApplyWorkspaceEdit { edit })
        .unwrap()
        .0
    {
        ServerResponse::WorkspaceEdit(receipt) => receipt,
        response => panic!("expected a delta receipt, got {response:?}"),
    }
}

fn seeded_scopes(host: &SharedHost, root: &std::path::Path) -> (Scope, Scope) {
    apply(host, open_project(root));
    let mut scopes = Vec::new();
    for title in ["Edited", "Unrelated"] {
        let id = ChannelId::new();
        apply(
            host,
            ModelEdit::CreateChannel {
                id,
                repository: root.to_owned(),
                title: title.into(),
                title_source: Default::default(),
            },
        );
        let scope = Scope::Channel(id);
        apply(
            host,
            ModelEdit::SeedView {
                scope: scope.clone(),
                panes: vec![browser()],
                closed: Vec::new(),
            },
        );
        scopes.push(scope);
    }
    (scopes.remove(0), scopes.remove(0))
}

fn change(scope: &Scope, expected_revision: u64, change: Change) -> ModelEdit {
    ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision,
        operation: uuid::Uuid::new_v4(),
        change,
    }
}

fn views(mirror: &Mirror) -> Vec<View> {
    let mut views = mirror.views().cloned().collect::<Vec<_>>();
    views.sort_by_key(|view| format!("{:?}", view.scope));
    views
}

#[test]
fn concurrent_windows_rebase_one_scope_and_reconnect_with_closed_identities() {
    let (directory, host, _) = fixture();
    let (scope, unrelated) = seeded_scopes(&host, directory.path());
    let mut first = Connection::new(&host, 3);
    let mut second = Connection::new(&host, 4);
    let (initial, closed) = first.client.watch_workspace_state().unwrap();
    assert!(closed.is_empty());
    assert_eq!(second.client.watch_workspace_state().unwrap().0, initial);
    let unrelated_before = initial
        .views
        .iter()
        .find(|view| view.scope == unrelated)
        .unwrap()
        .clone();
    let revision = initial
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap()
        .revision;
    let barrier = Arc::new(Barrier::new(2));
    let inserted = [browser(), browser()];
    let workers = [first, second]
        .into_iter()
        .zip(inserted.iter().cloned())
        .map(|(mut connection, pane)| {
            let barrier = Arc::clone(&barrier);
            let scope = scope.clone();
            let mut mirror = Mirror::new(&initial.views);
            thread::spawn(move || {
                let mut edit = change(&scope, revision, Change::Insert { pane, before: None });
                barrier.wait();
                let mut conflicts = 0;
                let mut reads = Vec::new();
                let receipt = match connection.client.apply_workspace_edit(edit.clone()) {
                    Ok(receipt) => receipt,
                    Err(ControlError::Remote {
                        code: ProtocolErrorCode::ViewConflict,
                        retryable: true,
                        ..
                    }) => {
                        conflicts += 1;
                        let (view, _) = connection
                            .client
                            .read_workspace_view(scope.clone())
                            .unwrap();
                        let revision = view.as_ref().unwrap().revision;
                        mirror.install(scope.clone(), view);
                        reads.push(scope.clone());
                        let ModelEdit::ChangeView {
                            expected_revision, ..
                        } = &mut edit
                        else {
                            unreachable!()
                        };
                        *expected_revision = revision;
                        connection
                            .client
                            .apply_workspace_edit(edit.clone())
                            .unwrap()
                    }
                    Err(error) => panic!("unexpected edit refusal: {error}"),
                };
                let own = mirror.apply(&edit, &receipt.effects);
                assert!(
                    own.unresolved.is_empty(),
                    "own receipt uses the rebased revision"
                );
                let edge = connection.next_edge();
                for scope in mirror.apply(&edge.edit, &edge.effects).unresolved {
                    let (view, _) = connection
                        .client
                        .read_workspace_view(scope.clone())
                        .unwrap();
                    mirror.install(scope.clone(), view);
                    reads.push(scope);
                }
                (connection, mirror, conflicts, reads)
            })
        })
        .collect::<Vec<_>>();
    let mut completed = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        completed
            .iter()
            .map(|(_, _, conflicts, _)| conflicts)
            .sum::<usize>(),
        1
    );
    assert!(
        completed
            .iter()
            .flat_map(|(_, _, _, reads)| reads)
            .all(|read| read == &scope)
    );
    let canonical = snapshot(&host, None).unwrap();
    let expected = views(&Mirror::new(&canonical.views));
    for (_, mirror, _, _) in &completed {
        assert_eq!(
            views(mirror),
            expected,
            "both windows converge through actual socket delivery"
        );
        assert_eq!(
            mirror.views().find(|view| view.scope == unrelated),
            Some(&unrelated_before)
        );
    }

    // One window disconnects. It misses a close and must obtain the closed-id
    // fence and canonical membership atomically when its replacement watches.
    let (mut online, _, _, _) = completed.remove(0);
    drop(completed);
    let revision = canonical
        .views
        .iter()
        .find(|view| view.scope == scope)
        .unwrap()
        .revision;
    let receipt = online
        .client
        .apply_workspace_edit(change(
            &scope,
            revision,
            Change::Remove { id: inserted[1].id },
        ))
        .unwrap();
    assert_eq!(receipt.effects.closed, [inserted[1].id]);
    let mut reconnected = Connection::new(&host, 5);
    let (model, closed) = reconnected.client.watch_workspace_state().unwrap();
    assert_eq!(model.identity, initial.identity);
    assert_eq!(model.sequence, receipt.sequence);
    assert!(closed.contains(&inserted[1].id));
    assert!(
        model
            .views
            .iter()
            .all(|view| view.panes.iter().all(|pane| pane.id != inserted[1].id))
    );
    assert_eq!(model, snapshot(&host, None).unwrap());
}

#[test]
fn a_delivery_gap_recovers_only_its_scope_and_retains_unrelated_views() {
    let (directory, host, _) = fixture();
    let (scope, unrelated) = seeded_scopes(&host, directory.path());
    let mut observer = Connection::new(&host, 3);
    let (initial, _) = observer.client.watch_workspace_state().unwrap();
    let mut mirror = Mirror::new(&initial.views);
    let unrelated_before = mirror
        .views()
        .find(|view| view.scope == unrelated)
        .unwrap()
        .clone();
    let added = browser();
    let first = apply(
        &host,
        change(
            &scope,
            mirror.revision(&scope).unwrap(),
            Change::Insert {
                pane: added.clone(),
                before: None,
            },
        ),
    );
    let skipped = observer.next_edge();
    assert_eq!(skipped.sequence, first.sequence);
    // Deliberately omit this delivered edge from the window's mirror.
    let second = apply(
        &host,
        change(
            &scope,
            first.effects.views[0].after.unwrap(),
            Change::Remove { id: added.id },
        ),
    );
    let edge = observer.next_edge();
    assert_eq!(edge.sequence, second.sequence);
    let missing = mirror.apply(&edge.edit, &edge.effects);
    assert_eq!(missing.unresolved, [scope.clone()]);
    let (view, sequence) = observer.client.read_workspace_view(scope.clone()).unwrap();
    assert!(sequence >= edge.sequence);
    mirror.install(scope, view);
    assert_eq!(
        views(&mirror),
        views(&Mirror::new(&snapshot(&host, None).unwrap().views))
    );
    assert_eq!(
        mirror.views().find(|view| view.scope == unrelated),
        Some(&unrelated_before)
    );
}

#[test]
fn an_older_client_keeps_legacy_reads_and_edits_but_receives_no_delta_notifications() {
    let (directory, host, delivery) = fixture();
    let older = older_client(CANDIDATE_LINE_PROTOCOL).unwrap();
    delivery.speak_older(older);
    // The serve loop checks this predicate before executing the watch handler,
    // so an old watch cannot subscribe or receive the changed snapshot shape.
    assert!(!older.admits_request(&ClientRequest::WatchWorkspaceModel));
    let edit = open_project(directory.path());
    assert!(older.admits_request(&ClientRequest::EditWorkspaceModel { edit: edit.clone() }));
    let edited = process_request(&host, 2, ClientRequest::EditWorkspaceModel { edit })
        .unwrap()
        .0;
    let ServerResponse::WorkspaceModel { model } = edited else {
        panic!("a carried CLI must retain its whole-model edit reply");
    };
    assert_eq!(model.projects.len(), 1);
    assert!(older.admits_request(&ClientRequest::ReadWorkspaceModel));
    let read = process_request(&host, 2, ClientRequest::ReadWorkspaceModel)
        .unwrap()
        .0;
    assert!(matches!(read, ServerResponse::WorkspaceModel { model: read } if read == model));
    // Defense in depth: even an accidentally queued changed notification is
    // withheld by the actual delivery queue for this old protocol.
    let proof = host.lock().unwrap().proof_handle();
    delivery
        .push(Outbound::broadcast(
            &proof,
            ServerMessage::WorkspaceModelEdited {
                sequence: model.sequence,
                edit: ModelEdit::AgentsChanged,
                effects: Default::default(),
            },
        ))
        .unwrap();
    delivery.close_after_drain();
    assert!(delivery.pop().is_none());
}
