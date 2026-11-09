//! What opening the store, reading the model and editing a view cost on a copy
//! of a real store. Uses only interfaces every schema since 22 has, so the same
//! file measures the build before a schema change and the build after it:
//!
//! ```text
//! CAGENTS_STORE_COPY=/path/to/workspace-model.sqlite3 \
//!     cargo test --release --no-default-features --features host --lib \
//!     view_cost_bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The named file is only read; every measurement runs on a scratch copy.
use super::{Registry, views};
use crate::model::{BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind};
use crate::workspace_model::{
    ModelEdit,
    views::{Change, Pane, Scope},
};
use std::path::Path;
use std::time::{Duration, Instant};
use uuid::Uuid;

const REPETITIONS: usize = 25;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn median_bytes(mut samples: Vec<u64>) -> u64 {
    samples.sort();
    samples[samples.len() / 2]
}

fn wal(path: &Path) -> u64 {
    std::fs::metadata(path.with_extension("sqlite3-wal"))
        .map(|metadata| metadata.len())
        .unwrap_or(0)
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

fn change(registry: &Registry, scope: &Scope, change: Change) -> ModelEdit {
    ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: registry.view_revision(scope).unwrap(),
        operation: Uuid::new_v4(),
        change,
    }
}

/// Applies `edit`, returning its duration and the WAL bytes it appended.
fn timed(registry: &mut Registry, path: &Path, edit: &ModelEdit) -> (Duration, u64) {
    let before = wal(path);
    let started = Instant::now();
    registry.apply(edit).unwrap();
    (started.elapsed(), wal(path) - before)
}

#[test]
#[ignore = "needs CAGENTS_STORE_COPY, a copy of a real store"]
fn view_cost_bench() {
    let source = std::env::var_os("CAGENTS_STORE_COPY").expect("CAGENTS_STORE_COPY");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("model.sqlite3");
    rusqlite::Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
        .unwrap();
    let size = |path: &Path| std::fs::metadata(path).unwrap().len();
    println!("bench file bytes before opening: {}", size(&path));

    let started = Instant::now();
    let registry = Registry::at(&path).unwrap();
    println!(
        "bench first open (any conversion included): {:?}",
        started.elapsed()
    );
    drop(registry);
    let opens = (0..REPETITIONS)
        .map(|_| {
            let started = Instant::now();
            drop(Registry::at(&path).unwrap());
            started.elapsed()
        })
        .collect();
    println!("bench open: {:?}", median(opens));

    let mut registry = Registry::at(&path).unwrap();
    // The WAL then only grows, so its size difference is what an edit wrote.
    registry
        .db
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    let reads = (0..REPETITIONS)
        .map(|_| {
            let started = Instant::now();
            registry.model().unwrap();
            started.elapsed()
        })
        .collect();
    println!("bench model read: {:?}", median(reads));

    let model = registry.model().unwrap();
    let mut occupied: Vec<_> = model
        .views
        .iter()
        .filter(|view| !view.panes.is_empty())
        .collect();
    occupied.sort_by_key(|view| view.panes.len());
    let small = occupied.first().expect("an occupied view").scope.clone();
    let big = occupied.last().unwrap().scope.clone();
    println!(
        "bench views {} occupied {} panes in the big view {} and the small view {}",
        model.views.len(),
        occupied.len(),
        occupied.last().unwrap().panes.len(),
        occupied.first().unwrap().panes.len(),
    );

    for (label, scope) in [("big", &big), ("small", &small)] {
        let mut costs: [(&str, Vec<Duration>, Vec<u64>); 5] = [
            ("insert", vec![], vec![]),
            ("retitle", vec![], vec![]),
            ("move", vec![], vec![]),
            ("remove", vec![], vec![]),
            ("seed", vec![], vec![]),
        ];
        for repetition in 0..REPETITIONS {
            let pane = browser(&format!("bench {repetition}"));
            let first = views::read(&registry.db, scope).unwrap().panes[0].id;
            let mut renamed = pane.clone();
            renamed.user_title = Some(format!("renamed {repetition}"));
            for index in 0..costs.len() {
                let edit = match index {
                    0 => change(
                        &registry,
                        scope,
                        Change::Insert {
                            pane: pane.clone(),
                            before: None,
                        },
                    ),
                    1 => change(
                        &registry,
                        scope,
                        Change::Update {
                            before: pane.clone(),
                            pane: renamed.clone(),
                        },
                    ),
                    2 => change(
                        &registry,
                        scope,
                        Change::Move {
                            id: pane.id,
                            before: Some(first),
                        },
                    ),
                    3 => change(&registry, scope, Change::Remove { id: pane.id }),
                    // A window seeding the panes the view already holds.
                    _ => ModelEdit::SeedView {
                        scope: scope.clone(),
                        panes: views::read(&registry.db, scope).unwrap().panes,
                        closed: vec![],
                    },
                };
                let (duration, bytes) = timed(&mut registry, &path, &edit);
                costs[index].1.push(duration);
                costs[index].2.push(bytes);
            }
        }
        for (name, durations, bytes) in costs {
            println!(
                "bench {label} {name}: {:?}, {} WAL bytes",
                median(durations),
                median_bytes(bytes)
            );
        }
    }
    registry
        .db
        .pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    drop(registry);
    println!("bench file bytes after the edits: {}", size(&path));
}
