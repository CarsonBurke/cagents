//! A window store at the size a long-lived installation reaches: about 1770
//! cards, nineteen in twenty of them closed, with the navigation metadata such
//! a window carries. These tests pin how much a flush that changes nothing
//! writes, and time the store's hot paths at that size when run explicitly:
//!
//! ```sh
//! scripts/cargo-gate test --locked --no-default-features --features ui --lib \
//!     persistence::history_scale -- --ignored --nocapture
//! ```
use super::*;
use crate::model::{
    BrowserId, BrowserRestoreFidelity, BrowserSession, CardState, LaunchRecipe, PullRequestView,
    RepositoryView, TerminalSession,
};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

/// Card counts by kind, open and closed, from a long-lived installation.
const SHAPE: &[(&str, usize, usize)] = &[
    ("chat", 45, 850),
    ("terminal", 27, 580),
    ("repository", 4, 145),
    ("browser", 2, 38),
    ("pull_requests", 1, 5),
    ("settings", 1, 23),
    ("accounts", 0, 29),
    ("automations", 0, 13),
    ("work", 0, 6),
];

fn recipe(index: usize, program: &str) -> LaunchRecipe {
    LaunchRecipe {
        program: program.into(),
        arguments: vec!["--permission-mode".into(), "bypassPermissions".into()],
        working_directory: PathBuf::from(format!("/home/user/src/project-{}", index % 18)),
        environment: (0..9)
            .map(|variable| {
                (
                    format!("CAGENTS_FIXTURE_VARIABLE_{variable}"),
                    format!("/home/user/.local/share/cagents/accounts/fixture/{variable:040}"),
                )
            })
            .collect(),
        unset_environment: ["ANTHROPIC_API_KEY".into()].into(),
    }
}

fn kind(name: &str, index: usize) -> CardKind {
    match name {
        "chat" => CardKind::Chat(crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: recipe(index, "claude"),
                session_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
        }),
        "terminal" => CardKind::Terminal(TerminalSession {
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            id: TerminalId::new(),
            launch: recipe(index, "/usr/bin/fish"),
            transcript_path: Some(PathBuf::from(format!(
                "/home/user/.local/share/cagents/transcripts/{index}.log"
            ))),
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: recipe(index, "/usr/bin/fish"),
                last_exit: None,
            },
        }),
        "repository" => CardKind::Repository(RepositoryView {
            project: crate::model::ProjectId::new(),
            repository: PathBuf::from(format!("/home/user/src/project-{}", index % 18)),
            worktree: PathBuf::from(format!("/home/user/src/project-{}", index % 18)),
            selected_file: None,
        }),
        "browser" => CardKind::Browser(BrowserSession {
            id: BrowserId::new(),
            current_url: url::Url::parse(&format!("https://example.com/page/{index}")).unwrap(),
            history: (0..6)
                .map(|page| url::Url::parse(&format!("https://example.com/page/{page}")).unwrap())
                .collect(),
            history_index: 5,
            scroll_x: 0.0,
            scroll_y: 120.0,
            fidelity: BrowserRestoreFidelity::HistoryAndScroll,
        }),
        "pull_requests" => CardKind::PullRequests(PullRequestView {
            repository: PathBuf::from(format!("/home/user/src/project-{}", index % 18)),
            selected_number: Some(42),
            file_panel_width: None,
            file_panel_collapsed: None,
        }),
        "settings" => CardKind::Settings,
        "accounts" => CardKind::Accounts,
        "automations" => CardKind::Automations,
        "work" => CardKind::Work,
        other => unreachable!("{other}"),
    }
}

pub(super) fn long_lived_workspace() -> WorkspaceState {
    let mut cards = Vec::new();
    let mut index = 0;
    for (name, open, closed) in SHAPE {
        for position in 0..open + closed {
            index += 1;
            cards.push(CardState {
                id: CardId::new(),
                generation: 0,
                title: format!("{name} {index}"),
                width_fraction: 0.5,
                existence: if position < *open {
                    Existence::Open
                } else {
                    Existence::Closed {
                        tombstone: Tombstone {
                            operation_id: None,
                            closed_at_unix_ms: 1_780_000_000_000 + index as i64,
                            reason: Some("closed by user".into()),
                        },
                    }
                },
                kind: kind(name, index),
            });
        }
    }
    let card_scopes = cards
        .iter()
        .step_by(6)
        .enumerate()
        .map(|(index, card)| {
            (
                card.id,
                PathBuf::from(format!("/home/user/src/project-{}", index % 18)),
            )
        })
        .collect();
    let pending_chat_admissions = cards
        .iter()
        .filter(|card| card.existence.is_open())
        .filter_map(|card| match &card.kind {
            CardKind::Chat(session) => Some((card.id, session.clone())),
            _ => None,
        })
        .take(10)
        .collect();
    let chat_drafts = cards
        .iter()
        .filter(|card| card.existence.is_open() && matches!(card.kind, CardKind::Chat(_)))
        .take(4)
        .map(|card| (card.id, "an unsent thought".to_owned()))
        .collect();
    let worktree_focus = cards
        .iter()
        .filter(|card| card.existence.is_open())
        .take(18)
        .enumerate()
        .map(|(index, card)| {
            (
                PathBuf::from(format!("/home/user/src/project-{index}")),
                card.id,
            )
        })
        .collect();
    WorkspaceState {
        schema_version: 1,
        id: WorkspaceId::new(),
        generation: 0,
        existence: Existence::Open,
        focused: cards
            .iter()
            .find(|card| card.existence.is_open())
            .map(|card| card.id),
        cards: cards.into(),
        active_worktree: None,
        worktree_focus,
        project_order: Vec::new(),
        channels: Vec::new(),
        active_channel: None,
        card_scopes,
        chat_drafts,
        chat_draft_attachments: HashMap::new(),
        chat_waiting_messages: HashMap::new(),
        agent_updates: BTreeMap::new(),
        workspace_history: Vec::new(),
        repository_navigation: HashMap::new(),
        workspace_model_identity: None,
        local_only_cards: HashSet::new(),
        pending_chat_admissions,
        pending_terminal_creations: HashSet::new(),
    }
}

#[derive(Default)]
struct Lane {
    workspace: u64,
    cards: BTreeMap<CardId, u64>,
}

impl Lane {
    fn save(&mut self, store: &StateStore, state: &WorkspaceState) -> WorkspaceCommit {
        let commit = store
            .save_workspace(
                state,
                SnapshotBase {
                    workspace: self.workspace,
                    cards: &self.cards,
                },
            )
            .unwrap();
        self.workspace = commit.generation;
        self.cards.clone_from(&commit.card_generations);
        commit
    }
}

fn changes(store: &StateStore) -> u64 {
    store.lock_inner().unwrap().connection.total_changes()
}

/// A flush that changes nothing in the window writes only the workspace row
/// whose generation records the flush, however much history the window holds.
#[test]
fn an_unchanged_flush_writes_only_the_workspace_generation() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(
        directory.path().join("history.sqlite3"),
        BootId::new("scale"),
    )
    .unwrap();
    let mut state = long_lived_workspace();
    let mut lane = Lane::default();
    lane.save(&store, &state);
    let before = changes(&store);
    lane.save(&store, &state);
    assert_eq!(changes(&store) - before, 1);

    // The same navigation rebuilt in maps and sets that iterate in another
    // order, as a restored window holds it, is still the stored navigation.
    state.card_scopes = state.card_scopes.into_iter().collect();
    state.chat_drafts = state.chat_drafts.into_iter().collect();
    state.pending_chat_admissions = state.pending_chat_admissions.into_iter().collect();
    state.worktree_focus = state.worktree_focus.into_iter().collect();
    state.local_only_cards = state.cards.iter().take(40).map(|card| card.id).collect();
    lane.save(&store, &state);
    state.local_only_cards = state.local_only_cards.into_iter().collect();
    let before = changes(&store);
    lane.save(&store, &state);
    assert_eq!(changes(&store) - before, 1);
}

/// The SQL of every statement the store's connection starts while `run` runs,
/// as SQLite's own statement trace reports it.
fn statements_during(store: &StateStore, run: impl FnOnce()) -> Vec<String> {
    use rusqlite::ffi;
    use std::{
        ffi::{CStr, c_int, c_uint, c_void},
        sync::Mutex,
    };
    unsafe extern "C" fn record(
        _event: c_uint,
        context: *mut c_void,
        statement: *mut c_void,
        _sql: *mut c_void,
    ) -> c_int {
        // SAFETY: `context` is the `Mutex` below, which outlives the trace,
        // and a statement event's subject is the statement being started.
        unsafe {
            let recorded = &*(context as *const Mutex<Vec<String>>);
            let sql = ffi::sqlite3_sql(statement.cast());
            if !sql.is_null() {
                recorded
                    .lock()
                    .unwrap()
                    .push(CStr::from_ptr(sql).to_string_lossy().into_owned());
            }
        }
        0
    }
    /// Removes the trace when dropped, so a `run` that panics cannot leave
    /// SQLite calling back into a `recorded` its unwinding has freed.
    struct Traced(*mut ffi::sqlite3);
    impl Drop for Traced {
        fn drop(&mut self) {
            // SAFETY: the handle is still the store's open connection.
            unsafe { ffi::sqlite3_trace_v2(self.0, 0, None, std::ptr::null_mut()) };
        }
    }
    let recorded = Mutex::new(Vec::new());
    {
        // SAFETY: the handle is the store's open connection, which outlives
        // this call; `traced` removes the trace before `recorded` is dropped.
        let handle = unsafe { store.lock_inner().unwrap().connection.handle() };
        unsafe {
            ffi::sqlite3_trace_v2(
                handle,
                ffi::SQLITE_TRACE_STMT as c_uint,
                Some(record),
                (&raw const recorded).cast_mut().cast(),
            );
        }
        let _traced = Traced(handle);
        run();
    }
    recorded.into_inner().unwrap()
}

/// Statements that read one card's row of `cards` by its id.
fn card_row_reads(statements: &[String]) -> Vec<&String> {
    statements
        .iter()
        .filter(|sql| {
            let sql = sql.split_whitespace().collect::<String>();
            sql.starts_with("SELECT") && sql.contains("FROMcardsWHEREcard_id=?")
        })
        .collect()
}

/// An unchanged flush learns everything it needs about each stored card from
/// the one query over the window's cards. Its 895 chats, open and closed, skip
/// their identity comparison and its 580 closed terminals their
/// archived-descriptor check; before, each cost a query of its own. A chat that
/// did change is still compared with its stored row.
#[test]
fn an_unchanged_flush_reads_no_card_row_on_its_own() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(
        directory.path().join("history.sqlite3"),
        BootId::new("scale"),
    )
    .unwrap();
    let mut state = long_lived_workspace();
    let mut lane = Lane::default();
    lane.save(&store, &state);

    let unchanged = statements_during(&store, || {
        lane.save(&store, &state);
    });
    assert!(
        unchanged
            .iter()
            .any(|sql| sql.contains("FROM cards WHERE workspace_id")),
        "the trace sees the flush's statements"
    );
    let reads = card_row_reads(&unchanged);
    let identity = reads
        .iter()
        .filter(|sql| sql.contains("SELECT content_json FROM cards"))
        .count();
    let archived = reads
        .iter()
        .filter(|sql| sql.contains("content_json IS NOT NULL"))
        .count();
    assert_eq!(identity, 0, "unchanged chats compare no stored identity");
    assert_eq!(archived, 0, "closed terminals read no row of their own");
    assert_eq!(reads, Vec::<&String>::new(), "nor does any other card");

    let chat = state
        .cards
        .iter()
        .position(|card| card.existence.is_open() && matches!(card.kind, CardKind::Chat(_)))
        .unwrap();
    state.cards[chat].title = "a renamed conversation".into();
    let changed = statements_during(&store, || {
        lane.save(&store, &state);
    });
    let reads = card_row_reads(&changed);
    assert_eq!(reads.len(), 1, "{reads:?}");
    assert!(reads[0].contains("SELECT content_json FROM cards"));
}

#[test]
#[ignore = "timing at realistic size; run explicitly"]
fn time_store_paths_at_long_lived_size() {
    fn median(mut samples: Vec<f64>) -> f64 {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let store = StateStore::open(&path, BootId::new("scale")).unwrap();
    let mut state = long_lived_workspace();
    let mut lane = Lane::default();
    lane.save(&store, &state);

    let unchanged = (0..25)
        .map(|_| {
            let started = Instant::now();
            lane.save(&store, &state);
            started.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    let open = state
        .cards
        .iter()
        .position(|card| card.existence.is_open())
        .unwrap();
    let retitled = (0..25)
        .map(|round| {
            state.cards[open].title = format!("retitled {round}");
            let started = Instant::now();
            lane.save(&store, &state);
            started.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    let wal_before = std::fs::metadata(sqlite_sidecar_path(&path, "-wal"))
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    store
        .lock_inner()
        .unwrap()
        .connection
        .pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    let flushes = 25;
    for _ in 0..flushes {
        lane.save(&store, &state);
    }
    let wal_per_flush = std::fs::metadata(sqlite_sidecar_path(&path, "-wal"))
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        / flushes;
    let loads = (0..7)
        .map(|_| {
            let started = Instant::now();
            store
                .load_workspace_restored(state.id, None)
                .unwrap()
                .unwrap();
            started.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    let conversations = (0..7)
        .map(|_| {
            let started = Instant::now();
            store.saved_conversations().unwrap();
            started.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    let navigable = (0..7)
        .map(|_| {
            let started = Instant::now();
            read_navigable_session_ids(&path).unwrap();
            started.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    eprintln!(
        "cards={} unchanged_flush_ms={:.2} one_title_flush_ms={:.2} wal_bytes_per_unchanged_flush={} (wal before checkpoint {}) load_ms={:.2} saved_conversations_ms={:.2} navigable_sessions_ms={:.2}",
        state.cards.len(),
        median(unchanged),
        median(retitled),
        wal_per_flush,
        wal_before,
        median(loads),
        median(conversations),
        median(navigable),
    );
}

/// Times the flushes a running window makes over a copy of a real window
/// store, named by `CAGENTS_HISTORY_SCALE_STORE`. The named file is copied
/// again before it is opened, because opening runs startup reconciliation,
/// which writes; the file itself is only read.
///
/// ```sh
/// CAGENTS_HISTORY_SCALE_STORE=/path/to/copy/sessions.sqlite3 \
///     <ui lib test binary> persistence::history_scale::time_flushes_on_a_store_copy \
///     --ignored --nocapture
/// ```
#[test]
#[ignore = "timing over a real store copy; run explicitly"]
fn time_flushes_on_a_store_copy() {
    fn median(mut samples: Vec<f64>) -> f64 {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    }
    fn wal_len(path: &std::path::Path) -> u64 {
        std::fs::metadata(sqlite_sidecar_path(path, "-wal"))
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    }
    let Some(source) = std::env::var_os("CAGENTS_HISTORY_SCALE_STORE") else {
        eprintln!("CAGENTS_HISTORY_SCALE_STORE is not set; nothing to time");
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    StateStore::copy_snapshot(std::path::Path::new(&source), &path).unwrap();
    let store = StateStore::open(&path, BootId::new("scale")).unwrap();
    let workspace = store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .next()
        .expect("the store holds a workspace");
    let state = store
        .load_workspace_restored(workspace.id, None)
        .unwrap()
        .unwrap()
        .state;
    let mut lane = Lane {
        workspace: state.generation,
        cards: state
            .cards
            .iter()
            .map(|card| (card.id, card.generation))
            .collect(),
    };
    let closed = state
        .cards
        .iter()
        .filter(|card| !card.existence.is_open())
        .count();

    // The first flush after a restart has nothing to compare against.
    let started = Instant::now();
    lane.save(&store, &state);
    let first_ms = started.elapsed().as_secs_f64() * 1e3;

    let flushes = 25;
    store
        .lock_inner()
        .unwrap()
        .connection
        .pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();
    let before = changes(&store);
    let unchanged = (0..flushes)
        .map(|_| {
            let started = Instant::now();
            lane.save(&store, &state);
            started.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    let rows = (changes(&store) - before) / flushes;
    let wal = wal_len(&path) / flushes;
    eprintln!(
        "cards={} closed={closed} first_flush_ms={first_ms:.2} unchanged_flush_ms={:.2} rows_per_unchanged_flush={rows} wal_bytes_per_unchanged_flush={wal}",
        state.cards.len(),
        median(unchanged),
    );
}
