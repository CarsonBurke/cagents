//! What it costs a watching window to bring its model up to date after another
//! client edits a view, on a copy of a real store: from the edge leaving the
//! daemon to the window holding the edited view.
//!
//! ```text
//! CAGENTS_STORE_COPY=/path/to/workspace-model.sqlite3 \
//!     scripts/cargo-gate test --locked --release --no-default-features --features host --lib \
//!     edge_cost_bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Each edit is measured both ways in one build. The full read is exactly what
//! a window did for every view edge before edges carried their effects: the
//! daemon reads the model, and both sides frame it. A revision gap now reads
//! only the affected scope. The delta is the edge with its effects, applied to the
//! window's copy of the views. Frames are encoded and decoded as on the socket;
//! the socket itself and the window's reconcile, which both paths share, are
//! not measured.
//!
//! The named file is only read; every measurement runs on a scratch copy.
use super::Registry;
use crate::model::{
    BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, HostSessionProof,
};
use crate::terminal::ipc::{
    ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage, ServerResponse,
    read_frame, write_frame,
};
use crate::workspace_model::{
    ModelEdit,
    delta::{EditEffects, Mirror},
    views::{Change, Pane},
};
use std::time::{Duration, Instant};
use uuid::Uuid;

const REPETITIONS: usize = 25;

fn median<T: Ord + Copy>(mut samples: Vec<T>) -> T {
    samples.sort();
    samples[samples.len() / 2]
}

fn browser(title: &str) -> Pane {
    Pane {
        id: CardId::new(),
        title: title.into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    }
}

fn proof() -> HostSessionProof {
    HostSessionProof {
        host_session_id: "bench".into(),
        boot_id: "boot".into(),
        host_incarnation: "host".into(),
    }
}

/// Frames `message` as the daemon sends it and decodes it as a window does;
/// returns the decoded message and the frame's size.
fn over_the_wire(request_id: Option<u64>, message: ServerMessage) -> (ServerMessage, u64) {
    let mut frame = Vec::new();
    write_frame(
        &mut frame,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id,
            proof: proof(),
            message,
        },
    )
    .unwrap();
    let envelope: ServerEnvelope = read_frame(&mut frame.as_slice()).unwrap().unwrap();
    (envelope.message, frame.len() as u64)
}

/// A request's frame, encoded and decoded.
fn request_frame(request: ClientRequest) -> u64 {
    let mut frame = Vec::new();
    write_frame(
        &mut frame,
        &ClientEnvelope {
            version: PROTOCOL_VERSION,
            request_id: 1,
            proof: Some(proof()),
            request,
        },
    )
    .unwrap();
    let _: ClientEnvelope = read_frame(&mut frame.as_slice()).unwrap().unwrap();
    frame.len() as u64
}

fn edge(sequence: u64, edit: &ModelEdit, effects: EditEffects) -> ServerMessage {
    ServerMessage::WorkspaceModelEdited {
        sequence,
        edit: edit.clone(),
        effects,
    }
}

/// The window reads the whole model for the edge.
fn full_read(registry: &Registry, sequence: u64, edit: &ModelEdit) -> u64 {
    let (_, edge) = over_the_wire(None, edge(sequence, edit, EditEffects::default()));
    let request = request_frame(ClientRequest::ReadWorkspaceModel);
    let model = registry.model().unwrap();
    let (message, reply) = over_the_wire(
        Some(1),
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel { model },
        },
    );
    assert!(matches!(
        message,
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel { .. }
        }
    ));
    edge + request + reply
}

/// The window applies the edge to its copy, reading a scope it cannot apply.
fn delta(
    registry: &Registry,
    mirror: &mut Mirror,
    sequence: u64,
    edit: &ModelEdit,
    effects: EditEffects,
) -> (u64, usize) {
    let (message, mut bytes) = over_the_wire(None, edge(sequence, edit, effects));
    let ServerMessage::WorkspaceModelEdited { edit, effects, .. } = message else {
        unreachable!()
    };
    let applied = mirror.apply(&edit, &effects);
    let reads = applied.unresolved.len();
    for scope in applied.unresolved {
        bytes += request_frame(ClientRequest::ReadWorkspaceView {
            scope: scope.clone(),
        });
        let (view, sequence) = registry.view_snapshot(&scope).unwrap();
        let (message, reply) = over_the_wire(
            Some(1),
            ServerMessage::Response {
                response: ServerResponse::WorkspaceView { view, sequence },
            },
        );
        bytes += reply;
        let ServerMessage::Response {
            response: ServerResponse::WorkspaceView { view, .. },
        } = message
        else {
            unreachable!()
        };
        mirror.install(scope, view);
    }
    (bytes, reads)
}

#[test]
#[ignore = "needs CAGENTS_STORE_COPY, a copy of a real store"]
fn edge_cost_bench() {
    let source = std::env::var_os("CAGENTS_STORE_COPY").expect("CAGENTS_STORE_COPY");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    rusqlite::Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let mut registry = Registry::at(&path).unwrap();
    let model = registry.model().unwrap();
    let mut mirror = Mirror::new(&model.views);
    let mut occupied: Vec<_> = model
        .views
        .iter()
        .filter(|view| !view.panes.is_empty())
        .collect();
    occupied.sort_by_key(|view| view.panes.len());
    let big = occupied.last().expect("an occupied view").scope.clone();
    let (_, whole) = over_the_wire(
        None,
        ServerMessage::Response {
            response: ServerResponse::WorkspaceModel {
                model: model.clone(),
            },
        },
    );
    println!(
        "bench views {} occupied {} panes {} agents {} channels {}; model frame {whole} bytes",
        model.views.len(),
        occupied.len(),
        model
            .views
            .iter()
            .map(|view| view.panes.len())
            .sum::<usize>(),
        model.agents.len(),
        model.channels.len(),
    );

    let change = |registry: &Registry, change| ModelEdit::ChangeView {
        scope: big.clone(),
        expected_revision: registry.view_revision(&big).unwrap(),
        operation: Uuid::new_v4(),
        change,
    };
    let names = ["insert", "retitle", "move", "remove"];
    let mut costs: Vec<[(Vec<Duration>, Vec<u64>); 2]> =
        names.iter().map(|_| Default::default()).collect();
    let mut reads = 0;
    for repetition in 0..REPETITIONS {
        let pane = browser(&format!("bench {repetition}"));
        let mut renamed = pane.clone();
        renamed.user_title = Some(format!("renamed {repetition}"));
        let first = registry.view_snapshot(&big).unwrap().0.unwrap().panes[0].id;
        for (index, cost) in costs.iter_mut().enumerate() {
            let edit = change(
                &registry,
                match index {
                    0 => Change::Insert {
                        pane: pane.clone(),
                        before: None,
                    },
                    1 => Change::Update {
                        before: pane.clone(),
                        pane: renamed.clone(),
                    },
                    2 => Change::Move {
                        id: pane.id,
                        before: Some(first),
                    },
                    _ => Change::Remove { id: pane.id },
                },
            );
            let applied = registry.apply(&edit).unwrap();
            assert!(applied.changed);
            let started = Instant::now();
            let bytes = full_read(&registry, applied.sequence, &edit);
            cost[0].0.push(started.elapsed());
            cost[0].1.push(bytes);
            let started = Instant::now();
            let (bytes, read) = delta(
                &registry,
                &mut mirror,
                applied.sequence,
                &edit,
                applied.effects,
            );
            cost[1].0.push(started.elapsed());
            cost[1].1.push(bytes);
            reads += read;
        }
    }
    for (name, [full, delta]) in names.iter().zip(costs) {
        println!(
            "bench {name}: full read {:?} {} bytes; delta {:?} {} bytes",
            median(full.0),
            median(full.1),
            median(delta.0),
            median(delta.1),
        );
    }
    println!("bench delta scope reads: {reads}");
    let held = registry.view_snapshot(&big).unwrap().0.unwrap();
    assert_eq!(
        mirror.views().find(|view| view.scope == big),
        Some(&held),
        "the window's copy followed the registry"
    );

    // A window that missed an edge reads the one scope instead of the model.
    let samples = (0..REPETITIONS)
        .map(|_| {
            let started = Instant::now();
            let request = request_frame(ClientRequest::ReadWorkspaceView { scope: big.clone() });
            let (view, sequence) = registry.view_snapshot(&big).unwrap();
            let (_, reply) = over_the_wire(
                Some(1),
                ServerMessage::Response {
                    response: ServerResponse::WorkspaceView { view, sequence },
                },
            );
            (started.elapsed(), request + reply)
        })
        .collect::<Vec<_>>();
    println!(
        "bench scope read: {:?} {} bytes",
        median(samples.iter().map(|sample| sample.0).collect()),
        median(samples.iter().map(|sample| sample.1).collect()),
    );
}
