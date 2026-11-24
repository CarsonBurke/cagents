//! Opening a channel in a workspace the size of a long-lived installation.
//!
//! The scale follows a real workspace that showed the lag: about 76 channels,
//! 45 open chats, 1,750 cards ever opened, about 3,000 worktrees and 1,850
//! shared views, most of them historical and empty. A real session writer
//! holds each saved snapshot the way it does in production.
//!
//! The timing run is ignored by default. Build it optimized, as people run the
//! app:
//!
//! ```sh
//! scripts/cargo-gate test --locked --profile local -p cagents-ui \
//!     --lib -- --ignored --nocapture app::channel_open_workload
//! ```
//!
//! It reports, for each click, the `update` that handles it and the first
//! frame built after it (view, layout and draw on the headless renderer), and
//! the steady frame cost once nothing has changed.
use super::*;
use crate::model::{ChannelId, ProjectChannel};
use crate::workspace_model::views::{Pane, Scope, View};
use std::collections::BTreeMap;

pub(super) struct Scale {
    pub worktrees: usize,
    pub channels: usize,
    pub chats: usize,
    pub loose_terminals: usize,
    pub closed_cards: usize,
    /// Empty shared views of worktrees visited once, beyond the channels'.
    pub historical_views: usize,
}

impl Scale {
    pub(super) const LARGE: Self = Self {
        worktrees: 3000,
        channels: 76,
        chats: 45,
        loose_terminals: 20,
        closed_cards: 1680,
        historical_views: 1850,
    };
}

/// A native chat launch saves the allowlisted runtime variables of the
/// environment it started in (`fresh_native_environment`): locale, XDG and
/// display variables, the SSH agent and the like, about this many on a
/// desktop session. Terminals start from a login shell with none.
const CHAT_ENVIRONMENT: [&str; 16] = [
    "HOME",
    "PATH",
    "LANG",
    "USER",
    "LOGNAME",
    "SHELL",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "CLAUDE_CONFIG_DIR",
];

fn recipe(directory: &Path, environment: BTreeMap<String, String>) -> LaunchRecipe {
    LaunchRecipe {
        unset_environment: Default::default(),
        program: "claude".into(),
        arguments: vec!["--model".into(), "haiku".into()],
        working_directory: directory.to_path_buf(),
        environment,
    }
}

fn chat_environment(index: usize) -> BTreeMap<String, String> {
    CHAT_ENVIRONMENT
        .iter()
        .map(|name| {
            (
                (*name).to_owned(),
                format!("/home/user/.local/share/{name}-{index}"),
            )
        })
        .collect()
}

fn chat(directory: &Path, index: usize) -> CardKind {
    CardKind::Chat(crate::chat::Session {
        native_continuity: Default::default(),
        owned_codex_listener: None,
        id: TerminalId::new(),
        launch: crate::chat::Launch {
            provider: crate::agent::Provider::Claude,
            recipe: recipe(directory, chat_environment(index)),
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            draft_model: None,
        },
        proof: None,
        terminal: None,
    })
}

pub(super) fn terminal(directory: &Path) -> CardKind {
    CardKind::Terminal(fresh_terminal_session(LaunchRecipe {
        program: "bash".into(),
        arguments: Vec::new(),
        ..recipe(directory, BTreeMap::new())
    }))
}

pub(super) fn card(title: String, kind: CardKind, existence: Existence) -> CardState {
    CardState {
        id: CardId::new(),
        generation: 0,
        title,
        width_fraction: 0.5,
        existence,
        kind,
    }
}

fn pane(card: &CardState) -> Pane {
    Pane {
        id: card.id,
        title: card.title.clone(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: card.width_fraction,
        kind: card.kind.clone(),
        scope: None,
    }
}

/// The workload app and the channels a person would click, in sidebar order.
pub(super) fn large_workspace(scale: &Scale) -> (Cagents, Vec<ChannelId>, tempfile::TempDir) {
    let mut app = super::tests::context_inspector_test_app(false);
    app.workspace.resize_viewport(1200.0);

    let projects = ["app", "api", "web", "infra"];
    let mut repositories = Vec::new();
    let mut project_ids = Vec::new();
    let mut worktree_paths = Vec::new();
    for (position, name) in projects.iter().enumerate() {
        let root = PathBuf::from(format!("/w/{name}"));
        let repository = root.join(".git");
        let mut paths = vec![root.clone()];
        let count = if position == 0 { scale.worktrees } else { 4 };
        paths.extend((1..count).map(|index| PathBuf::from(format!("/w/{name}-wt/wt-{index}"))));
        let worktrees = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                crate::sidebar::SidebarWorktree::new(
                    crate::model::WorktreeId::new(),
                    path.clone(),
                    Some(if index == 0 {
                        "main".into()
                    } else {
                        format!("feature/wt-{index}")
                    }),
                    false,
                    false,
                    None,
                    None,
                    None,
                )
            })
            .collect();
        let id = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            id,
            RepositoryId::new(),
            (*name).into(),
            repository.clone(),
            worktrees,
            vec![],
        ));
        app.known_worktree_roots.extend(paths.iter().cloned());
        repositories.push(repository);
        project_ids.push(id);
        worktree_paths.push(paths);
    }

    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.clear();
    let mut channels = Vec::new();
    let mut views = Vec::new();
    for index in 0..scale.channels {
        let project = index % projects.len();
        let id = ChannelId::new();
        let mut members = Vec::new();
        if index < scale.chats {
            let directory = &worktree_paths[project][0];
            let conversation = card(
                format!("Investigate scheduler failure {index}"),
                chat(directory, index),
                Existence::Open,
            );
            members.push(conversation);
            if index % 3 == 0 {
                members.push(card(
                    format!("cargo test {index}"),
                    terminal(directory),
                    Existence::Open,
                ));
            }
        }
        let mut view = View::empty(id);
        view.revision = 1;
        view.panes = members.iter().map(pane).collect();
        views.push(view);
        durable.channels.push(ProjectChannel {
            id,
            project_id: project_ids[project],
            title: format!("Channel {index}"),
            title_source: Default::default(),
            ordinal: index as u32 + 1,
            shared: true,
            cards: members.iter().map(|card| card.id).collect(),
            focused: members.first().map(|card| card.id),
            checkout: None,
        });
        app.workspace_model
            .channels
            .push(crate::workspace_model::ModelChannel {
                id,
                repository: repositories[project].clone(),
                title: format!("Channel {index}"),
                title_source: Default::default(),
                ordinal: index as u32 + 1,
                checkout: None,
            });
        durable.cards.extend(members);
        channels.push(id);
    }
    for index in 0..scale.loose_terminals {
        let directory = &worktree_paths[0][1 + index * 7 % (worktree_paths[0].len() - 1)];
        let state = card(
            format!("shell {index}"),
            terminal(directory),
            Existence::Open,
        );
        durable.cards.push(state);
    }
    for index in 0..scale.closed_cards {
        let paths = &worktree_paths[index % projects.len()];
        let directory = &paths[index % paths.len()];
        let kind = if index % 2 == 0 {
            chat(directory, index)
        } else {
            terminal(directory)
        };
        durable.cards.push(card(
            format!("historical {index}"),
            kind,
            Existence::Closed {
                tombstone: crate::model::Tombstone {
                    operation_id: None,
                    closed_at_unix_ms: 1,
                    reason: Some("historical pane".into()),
                },
            },
        ));
    }
    // Every worktree ever visited keeps a shared view, almost all empty.
    for (project, paths) in worktree_paths.iter().enumerate() {
        for path in paths {
            views.push(View {
                scope: Scope::Worktree {
                    repository: repositories[project].clone(),
                    path: path.clone(),
                },
                revision: 1,
                panes: Vec::new(),
                retired: Vec::new(),
            });
        }
    }
    views.truncate(scale.channels + scale.historical_views);
    app.workspace_model.views = views;
    app.workspace_model.identity = uuid::Uuid::new_v4();
    for view in &app.workspace_model.views {
        app.shared_views
            .initialize(view.scope.clone(), view.panes.clone());
    }

    let directory = tempfile::tempdir().unwrap();
    let (writer, _) = SessionRuntime::open(
        directory.path().join("window.sqlite3"),
        crate::model::BootId::new("channel-open-workload"),
    )
    .unwrap();
    app.session = Some(writer);
    drop(app.restore_worktree_scope());
    drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
        channel_id: channels[0],
    })));
    (app, channels, directory)
}

/// Builds, lays out and draws one frame, as the shell does after an update.
fn frame(
    app: &Cagents,
    cache: iced::runtime::user_interface::Cache,
    renderer: &mut cosmic::Renderer,
) -> iced::runtime::user_interface::Cache {
    timed_frame(app, cache, renderer).0
}

/// One frame and how long its view, layout and draw each took.
fn timed_frame(
    app: &Cagents,
    cache: iced::runtime::user_interface::Cache,
    renderer: &mut cosmic::Renderer,
) -> (iced::runtime::user_interface::Cache, [Duration; 3]) {
    let started = Instant::now();
    let element = app.view();
    let viewed = Instant::now();
    let mut ui = iced::runtime::UserInterface::build(
        element,
        iced::Size::new(1600.0, 1000.0),
        cache,
        renderer,
    );
    let built = Instant::now();
    ui.draw(
        renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        iced::mouse::Cursor::Unavailable,
    );
    let drawn = Instant::now();
    (
        ui.into_cache(),
        [viewed - started, built - viewed, drawn - built],
    )
}

/// The user-space instructions this thread retires, from a Linux hardware
/// counter. Unlike wall time, the count does not depend on what else the host
/// is running.
struct RetiredInstructions(std::fs::File);

impl RetiredInstructions {
    fn open() -> Option<Self> {
        // `perf_event_attr` in its first, 64-byte layout: the type
        // (PERF_TYPE_HARDWARE, 0) and size share the first word, the config
        // (PERF_COUNT_HW_INSTRUCTIONS, 1) is the second, and the flag word at
        // byte 40 pins the counter to the PMU (bit 2), so a read fails rather
        // than undercounting while it is multiplexed, and excludes the kernel
        // (bit 5) and the hypervisor (bit 6).
        let mut attr = [0u64; 8];
        attr[0] = 64 << 32;
        attr[1] = 1;
        attr[5] = 1 << 2 | 1 << 5 | 1 << 6;
        // SAFETY: `attr` is a valid, fully initialized perf_event_attr of the
        // size it declares, and outlives the call. The counter follows this
        // thread (pid 0) on any CPU (-1), in no group (-1), with no flags.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                attr.as_ptr(),
                0 as libc::pid_t,
                -1 as libc::c_int,
                -1 as libc::c_int,
                0 as libc::c_ulong,
            )
        };
        let fd = libc::c_int::try_from(fd).ok().filter(|fd| *fd >= 0)?;
        // SAFETY: the kernel just returned `fd`, and nothing else owns it.
        Some(Self(std::fs::File::from(unsafe {
            <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
        })))
    }

    fn read(&self) -> u64 {
        use std::io::Read;
        let mut count = [0; 8];
        (&self.0)
            .read_exact(&mut count)
            .expect("read the instruction counter");
        u64::from_ne_bytes(count)
    }
}

fn summary(label: &str, durations: &mut [Duration]) {
    let summary = crate::performance::LatencySummary::from_durations(durations).unwrap();
    eprintln!(
        "{label:<28} n={:<4} median {:>8.2?}  p95 {:>8.2?}  worst {:>8.2?}",
        summary.samples, summary.median, summary.p95, summary.worst
    );
}

#[test]
fn the_workload_opens_each_channel_with_its_own_panes() {
    let scale = Scale {
        worktrees: 40,
        channels: 8,
        chats: 5,
        loose_terminals: 3,
        closed_cards: 30,
        historical_views: 1850,
    };
    let (mut app, channels, _directory) = large_workspace(&scale);
    for (index, channel) in channels.iter().enumerate() {
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: *channel,
        })));
        assert_eq!(app.durable.active_channel, Some(*channel));
        let expected = app
            .durable
            .channels
            .iter()
            .find(|candidate| candidate.id == *channel)
            .unwrap()
            .cards
            .clone();
        let shown = app.workspace.order().collect::<Vec<_>>();
        if index < scale.chats {
            assert_eq!(shown, expected);
        } else {
            // An empty channel gets its default pane.
            assert!(shown.len() <= 1);
        }
    }
}

/// The session writer holds the last snapshot from each save until its flush
/// settles, so a channel opened meanwhile copies the workspace. The copy
/// shares every card the switch does not change, rather than copying all of
/// them on the UI thread. (Opening an empty channel adds its default chat,
/// and leaving it removes that chat again, which moves the others along.)
#[test]
fn opening_a_channel_while_a_save_is_in_flight_copies_no_card() {
    let scale = Scale {
        worktrees: 40,
        channels: 8,
        chats: 5,
        loose_terminals: 3,
        closed_cards: 30,
        historical_views: 1850,
    };
    let (mut app, channels, _directory) = large_workspace(&scale);
    drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
        channel_id: channels[0],
    })));
    for channel in &channels[1..] {
        let held = Arc::clone(&app.durable);
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: *channel,
        })));
        assert!(!Arc::ptr_eq(&held, &app.durable), "the switch is saved");
        let copied = app.durable.cards.copied_from(&held.cards);
        assert!(copied.is_empty(), "a channel switch copied {copied:?}");
    }
}

#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn channel_open_timing() {
    super::projection_oracle::disable();
    let (mut app, channels, _directory) = large_workspace(&Scale::LARGE);
    let mut renderer = super::tests::headless_renderer();
    let mut cache = frame(&app, Default::default(), &mut renderer);
    // Only channels with conversations: an empty channel adds a default pane
    // the first time, which is a different interaction.
    let targets = &channels[..Scale::LARGE.chats];
    for channel in targets.iter().take(5) {
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: *channel,
        })));
        cache = frame(&app, cache, &mut renderer);
    }
    let mut update = Vec::new();
    let mut first_frame = Vec::new();
    let mut steady_frame = Vec::new();
    let mut click_to_frame = Vec::new();
    let mut parts = [Vec::new(), Vec::new(), Vec::new()];
    let mut steady_parts = [Vec::new(), Vec::new(), Vec::new()];
    for round in 0..3 {
        for index in 0..targets.len() {
            // Far-apart channels in turn, as a person scanning the sidebar
            // clicks them.
            let channel_id = targets[(index * 7 + round) % targets.len()];
            let started = Instant::now();
            drop(app.update(Message::Sidebar(SidebarAction::OpenChannel { channel_id })));
            let updated = Instant::now();
            let (next, first) = timed_frame(&app, cache, &mut renderer);
            let drawn = Instant::now();
            let (next, steady) = timed_frame(&app, next, &mut renderer);
            cache = next;
            steady_frame.push(drawn.elapsed());
            for (part, (first, steady)) in first.into_iter().zip(steady).enumerate() {
                parts[part].push(first);
                steady_parts[part].push(steady);
            }
            update.push(updated - started);
            first_frame.push(drawn - updated);
            click_to_frame.push(drawn - started);
        }
    }
    eprintln!(
        "channel open workload: {} cards, {} channels, {} views, {} worktrees",
        app.durable.cards.len(),
        app.durable.channels.len(),
        app.workspace_model.views.len(),
        app.known_worktree_roots.len(),
    );
    summary("update(OpenChannel)", &mut update);
    summary("first frame after click", &mut first_frame);
    summary("click to first frame", &mut click_to_frame);
    summary("steady frame", &mut steady_frame);
    for (label, (first, steady)) in ["view", "layout", "draw"]
        .into_iter()
        .zip(parts.iter_mut().zip(steady_parts.iter_mut()))
    {
        summary(&format!("  first frame {label}"), first);
        summary(&format!("  steady frame {label}"), steady);
    }
}

/// Opening worktrees from the sidebar, each holding a terminal so that none
/// opens a default pane: the `update` that handles the click, whose share of
/// the work grows with the worktrees the window has known when ownership is
/// resolved against every one of them.
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn worktree_open_timing() {
    super::projection_oracle::disable();
    let (mut app, _channels, _directory) = large_workspace(&Scale::LARGE);
    let project = &app.sidebar_model.projects[0];
    let rows = &project.worktrees;
    let row_count = rows.len();
    // The worktrees `large_workspace` gives a loose terminal.
    let targets = (0..Scale::LARGE.loose_terminals)
        .map(|index| {
            let row = &rows[1 + index * 7 % (rows.len() - 1)];
            SidebarAction::OpenWorktree {
                project_id: project.id,
                worktree_id: row.id,
                path: row.path.clone(),
            }
        })
        .collect::<Vec<_>>();
    let mut updates = Vec::new();
    for round in 0..4 {
        for (index, action) in targets.iter().enumerate() {
            let started = Instant::now();
            drop(app.update(Message::Sidebar(action.clone())));
            let elapsed = started.elapsed();
            assert!(app.navigation_save.is_some(), "the click defers its save");
            drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
            if round > 0 || index >= 5 {
                updates.push(elapsed);
            }
        }
    }
    eprintln!(
        "worktree open workload: {} known worktree roots, {} rows",
        app.known_worktree_roots.len(),
        row_count,
    );
    summary("update(OpenWorktree)", &mut updates);
}

/// What the save a channel click defers costs the UI thread when it comes
/// due, clicking channels in `channel_open_timing`'s order. The whole
/// `NavigationSaveDue` update is timed untraced: the save, and the projection
/// it needs, which is none when the save changed nothing the projection
/// reads. A second pass traces only those updates and
/// reports the workspace pipeline's own timing of each phase of the save.
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn navigation_save_timing() {
    super::projection_oracle::disable();
    let (mut app, channels, directory) = large_workspace(&Scale::LARGE);
    app.shared_views.skip_rediscovery_check();
    settle_seeds(&mut app);
    let targets = &channels[..Scale::LARGE.chats];
    let click = |app: &mut Cagents, round: usize, index: usize| {
        let channel_id = targets[(index * 7 + round) % targets.len()];
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel { channel_id })));
        assert!(app.navigation_save.is_some(), "the click defers its save");
    };
    let save_due = |app: &mut Cagents| {
        let started = Instant::now();
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        let elapsed = started.elapsed();
        assert!(app.navigation_save.is_none(), "the save ran");
        elapsed
    };
    // From the second target: the first is already open.
    for index in 1..6 {
        click(&mut app, 0, index);
        save_due(&mut app);
    }
    let mut updates = Vec::new();
    for round in 0..2 {
        for index in 0..targets.len() {
            click(&mut app, round, index);
            updates.push(save_due(&mut app));
        }
    }
    summary("update(NavigationSaveDue)", &mut updates);
    let phases = save_phases(&directory, || {
        for index in 0..targets.len() {
            click(&mut app, 2, index);
            save_due(&mut app);
        }
    });
    report_phases(phases);
}

/// Saves, then acknowledges every seed that save published and lists its
/// view, as the daemon does once the window's first save reaches it. A
/// running window publishes into seeded scopes, so a save that still meets
/// over a thousand unseeded ones measures a state the window is never in.
fn settle_seeds(app: &mut Cagents) {
    // The save that seeds every scope the window shows.
    app.persist();
    // With no daemon to deliver them, the seeds fail as they are published.
    let seeds = app
        .shared_views
        .seeding_scopes()
        .into_iter()
        .chain(app.shared_views.seed_failures.keys().cloned())
        .collect::<Vec<_>>();
    assert!(!seeds.is_empty(), "the window seeds its unseeded scopes");
    for scope in seeds {
        app.acknowledge_view_edit(scope.clone(), None);
        if !app
            .workspace_model
            .views
            .iter()
            .any(|view| view.scope == scope)
        {
            let mut view = View::empty(ChannelId::new());
            view.scope = scope;
            view.revision = 1;
            app.workspace_model.views.push(view);
        }
    }
}

/// Answers every view edit the window has published as the registry would,
/// applying each to the model's view and acknowledging it, until a
/// reconciliation publishes no more. Without a daemon they would stay pending,
/// and every later pass would project its own unacknowledged inserts again.
fn answer_view_edits(app: &mut Cagents) {
    for _ in 0..8 {
        let pending = std::mem::take(&mut app.shared_views.pending);
        if pending.is_empty() {
            return;
        }
        for (operation, scope, change) in pending {
            let views = &mut app.workspace_model.views;
            if !views.iter().any(|view| view.scope == scope) {
                let mut view = View::empty(ChannelId::new());
                view.scope = scope.clone();
                views.push(view);
            }
            if let crate::workspace_model::views::Change::Transfer {
                id,
                destination,
                before,
            } = &change
            {
                if !views.iter().any(|view| &view.scope == destination) {
                    let mut view = View::empty(ChannelId::new());
                    view.scope = destination.clone();
                    views.push(view);
                }
                crate::workspace_model::views::transfer(views, &scope, *id, destination, *before)
                    .unwrap();
            } else {
                let view = views.iter_mut().find(|view| view.scope == scope).unwrap();
                drop(view.apply(&change));
            }
            for view in views.iter_mut().filter(|view| {
                view.scope == scope
                    || matches!(&change, crate::workspace_model::views::Change::Transfer { destination, .. } if *destination == view.scope)
            }) {
                view.revision += 1;
            }
            app.shared_views
                .pending
                .push((operation, scope.clone(), change));
            app.acknowledge_view_edit(scope, Some(operation));
        }
        drop(app.reconcile_workspace_model());
    }
    panic!("the window kept publishing view edits");
}

/// The save a terminal's start makes once its surface attaches: the start
/// changes the terminal's saved session, and `persist()` publishes that
/// change before the handoff is committed. Each round opens a channel with a
/// terminal, lets its navigation save run, then records a new session for
/// the terminal and saves, as `terminal_started` does.
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn terminal_started_save_timing() {
    super::projection_oracle::disable();
    let (mut app, channels, directory) = large_workspace(&Scale::LARGE);
    app.shared_views.skip_rediscovery_check();
    settle_seeds(&mut app);
    // The channels whose conversation came with a terminal.
    let targets = channels[..Scale::LARGE.chats]
        .iter()
        .step_by(3)
        .copied()
        .collect::<Vec<_>>();
    let mut starts = 0usize;
    let mut start = |app: &mut Cagents, index: usize| {
        let channel_id = targets[index % targets.len()];
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel { channel_id })));
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        let terminal = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .and_then(|channel| {
                channel.cards.iter().copied().find(|id| {
                    matches!(
                        app.card_state(*id).map(|card| &card.kind),
                        Some(CardKind::Terminal(_))
                    )
                })
            })
            .expect("the channel holds a terminal");
        starts += 1;
        let CardKind::Terminal(session) = &mut app.card_state_mut(terminal).unwrap().kind else {
            unreachable!();
        };
        session.resume_hint = Some(format!("started {starts}"));
        let started = Instant::now();
        app.persist();
        started.elapsed()
    };
    for index in 0..5 {
        start(&mut app, index);
    }
    let mut saves = (0..targets.len() * 2)
        .map(|index| start(&mut app, index))
        .collect::<Vec<_>>();
    summary("persist() as a terminal starts", &mut saves);
    let phases = save_phases(&directory, || {
        for index in 0..targets.len() {
            start(&mut app, index);
        }
    });
    report_phases(phases);
}

/// The save phases `run` traces, by stage. Phases report microseconds since
/// the previous mark; the snapshot capture after them reports its own
/// nanoseconds.
fn save_phases(
    directory: &tempfile::TempDir,
    run: impl FnOnce(),
) -> BTreeMap<String, Vec<Duration>> {
    let log_path = directory.path().join("save-phases.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "cagents::workspace_pipeline=debug",
        ))
        .with_writer(move || log.try_clone().unwrap())
        .finish();
    crate::test_tracing::with_thread_subscriber(subscriber, run);
    let logged = std::fs::read_to_string(log_path).unwrap();
    let field = |line: &str, prefix: &str| {
        line.split_whitespace()
            .find_map(|token| token.strip_prefix(prefix))
            .map(|value| value.trim_matches('"').to_owned())
    };
    let mut phases = BTreeMap::<String, Vec<Duration>>::new();
    let mut counts = BTreeMap::<String, usize>::new();
    for line in logged.lines() {
        let Some(stage) = field(line, "stage=") else {
            continue;
        };
        if matches!(
            stage.as_str(),
            "local_scopes" | "local_settled" | "view_index"
        ) {
            // Counts, not durations: the shape of each pass.
            *counts
                .entry(
                    line.split_once(stage.as_str())
                        .map_or(line, |(_, rest)| rest)
                        .trim_start_matches('"')
                        .trim()
                        .to_owned(),
                )
                .or_insert(0usize) += 1;
            continue;
        }
        let duration = if let Some(micros) = field(line, "phase_us=") {
            micros.parse().ok().map(Duration::from_micros)
        } else {
            field(line, "elapsed_ns=")
                .and_then(|nanos| nanos.parse().ok())
                .map(Duration::from_nanos)
        };
        if let Some(duration) = duration {
            phases.entry(stage).or_default().push(duration);
        }
    }
    for (shape, passes) in counts {
        eprintln!("  {passes:>3} passes: {shape}");
    }
    phases
}

fn report_phases(mut phases: BTreeMap<String, Vec<Duration>>) {
    for (stage, durations) in &mut phases {
        summary(&format!("  {stage}"), durations);
    }
}

/// Opening worktrees that hold no card yet, after the daemon has settled
/// every seed: the deferred save that publishes each one's new pane, then the
/// complete save its terminal's start makes.
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn new_worktree_save_timing() {
    super::projection_oracle::disable();
    let (mut app, _channels, _directory) = large_workspace(&Scale::LARGE);
    app.shared_views.skip_rediscovery_check();
    settle_seeds(&mut app);
    let project = &app.sidebar_model.projects[0];
    // Past the worktrees where `large_workspace` puts a loose terminal or a
    // closed card.
    let targets = project.worktrees[1701..]
        .iter()
        .step_by(50)
        .map(|row| SidebarAction::OpenWorktree {
            project_id: project.id,
            worktree_id: row.id,
            path: row.path.clone(),
        })
        .collect::<Vec<_>>();
    let (mut navigation, mut started) = (Vec::new(), Vec::new());
    for (index, action) in targets.iter().enumerate() {
        let SidebarAction::OpenWorktree { path, .. } = action else {
            unreachable!();
        };
        assert!(
            !app.durable
                .cards
                .iter()
                .any(|card| super::worktree_ui::card_directory(card) == Some(path)),
            "no card is in the worktree yet"
        );
        drop(app.update(Message::Sidebar(action.clone())));
        let begun = Instant::now();
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        let save = begun.elapsed();
        let begun = Instant::now();
        app.persist();
        let start = begun.elapsed();
        if index >= 5 {
            navigation.push(save);
            started.push(start);
        }
    }
    eprintln!("new worktree workload: {} worktrees opened", targets.len());
    summary("update(NavigationSaveDue)", &mut navigation);
    summary("persist() as a terminal starts", &mut started);
}

/// Where a steady frame's build time goes, phase by phase, as
/// `UserInterface::build` runs them: the named-state extraction, the
/// reconciliation, the restoration of states that cached subtrees kept, and
/// layout itself. The headless frame above reports these four together as
/// "layout".
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn steady_frame_phases() {
    super::projection_oracle::disable();
    use iced::advanced::widget::{Tree, tree::NAMED};

    fn nodes(tree: &Tree) -> usize {
        1 + tree.children.iter().map(nodes).sum::<usize>()
    }

    let (mut app, channels, _directory) = large_workspace(&Scale::LARGE);
    drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
        channel_id: channels[1],
    })));
    let renderer = super::tests::headless_renderer();
    let limits =
        iced::advanced::layout::Limits::new(iced::Size::ZERO, iced::Size::new(1600.0, 1000.0));
    let mut tree = Tree::empty();
    let mut phases: [Vec<Duration>; 5] = Default::default();
    for round in 0..64 {
        let started = Instant::now();
        let mut element = app.view();
        let viewed = Instant::now();
        NAMED.with(|named| *named.borrow_mut() = tree.take_all_named());
        let taken = Instant::now();
        tree.diff(element.as_widget_mut());
        let diffed = Instant::now();
        iced::runtime::user_interface::restore_cached_named_states(&mut tree);
        let restored = Instant::now();
        drop(
            element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits),
        );
        let laid_out = Instant::now();
        NAMED.with(|named| named.borrow_mut().clear());
        if round < 8 {
            continue;
        }
        for (phase, duration) in phases.iter_mut().zip([
            viewed - started,
            taken - viewed,
            diffed - taken,
            restored - diffed,
            laid_out - restored,
        ]) {
            phase.push(duration);
        }
    }
    eprintln!(
        "steady frame tree: {} nodes, {} named",
        nodes(&tree),
        tree.take_all_named().len()
    );
    for (label, phase) in ["view", "take named", "diff", "restore named", "layout"]
        .into_iter()
        .zip(phases.iter_mut())
    {
        summary(&format!("  steady {label}"), phase);
    }
}

/// What keeping chats' pull request labels current costs each update in the
/// large workspace: each project lists as many pull requests as its poll
/// reads (its user's hundred most recent), and each open chat is a long
/// transcript linking to 48 of the first project's, four of them shown. It
/// measures an update after which nothing changed, and the update after a
/// poll that saw one of the linked pull requests merge, which relabels every
/// chat.
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn pull_request_label_upkeep_timing() {
    super::projection_oracle::disable();
    const LISTED: u64 = 100;
    const TURNS: usize = 16;
    const SHOWN: usize = 4;
    let (mut app, _channels, _directory) = large_workspace(&Scale::LARGE);
    let rows = |index: usize, project: &crate::sidebar::SidebarProject, merged: Option<u64>| {
        // The first project is the repository the transcripts link into.
        let name = if index == 0 {
            "widgets"
        } else {
            project.name.as_str()
        };
        (1_000..1_000 + LISTED)
            .map(|number| {
                let mut row = crate::sidebar::SidebarPullRequest::new(
                    project.id,
                    number,
                    format!("Change {number} of {name}"),
                    format!("https://github.com/acme/{name}/pull/{number}")
                        .parse()
                        .unwrap(),
                    format!("change-{number}"),
                    None,
                    false,
                    crate::sidebar::CheckStatus::Passing,
                );
                row.state = Some(if Some(number) == merged {
                    crate::sidebar::WorktreePullRequestState::Merged
                } else {
                    crate::sidebar::WorktreePullRequestState::Open
                });
                row
            })
            .collect::<Vec<_>>()
    };
    for index in 0..app.sidebar_model.projects.len() {
        let project = &app.sidebar_model.projects[index];
        let name = if index == 0 {
            "widgets"
        } else {
            project.name.as_str()
        };
        let key = crate::github_identity::GitHubRepoKey::new("github.com", None, "acme", name)
            .expect("a repository");
        app.github_availability.insert(
            project.id,
            crate::github::GitHubAvailability::Loading {
                confirmed: Some(key),
            },
        );
        let listed = rows(index, project, None);
        app.sidebar_model.projects[index].replace_pull_requests(listed);
    }
    let chats = app
        .durable
        .cards
        .iter()
        .filter(|card| matches!(card.kind, CardKind::Chat(_)) && card.existence.is_open())
        .map(|card| card.id)
        .take(Scale::LARGE.chats)
        .collect::<Vec<_>>();
    // A window shows a few chats side by side; the rest are open elsewhere.
    for (index, card) in chats.iter().enumerate() {
        chat_ui::open_linked_chat_for_test(&mut app, *card, TURNS, index < SHOWN);
    }
    let tick = || Message::ChatUi(chat_ui::Action::Tick);
    drop(app.update(tick()));
    chat_ui::drain_preparation_for_test(&mut app);
    let mut steady = Vec::new();
    for _ in 0..256 {
        let started = Instant::now();
        drop(app.update(tick()));
        steady.push(started.elapsed());
    }
    let mut polled = Vec::new();
    for round in 0..64 {
        let merged = Some(1_000 + round % (TURNS as u64 * 3));
        let listed = rows(0, &app.sidebar_model.projects[0], merged);
        app.sidebar_model.projects[0].replace_pull_requests(listed);
        let started = Instant::now();
        drop(app.update(tick()));
        polled.push(started.elapsed());
        chat_ui::drain_preparation_for_test(&mut app);
    }
    eprintln!(
        "pull request label upkeep: {} projects listing {LISTED} each, {} chats of {TURNS} linked turns ({SHOWN} shown), {} worktrees",
        app.sidebar_model.projects.len(),
        chats.len(),
        app.known_worktree_roots.len(),
    );
    summary("update, nothing changed", &mut steady);
    summary("update after a poll", &mut polled);
}

/// What the projection that ends an update costs in the large workspace, in
/// full and stage by stage, and the whole update for messages that change
/// nothing the projection reads: a modifier key's event, which every keystroke
/// delivers beside the text it types, an animation frame, a transcript probe's
/// retry, a superseded visibility dwell and the save a channel click defers.
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn projection_tail_timing() {
    super::projection_oracle::disable();
    const ROUNDS: usize = 64;
    let (mut app, channels, _directory) = large_workspace(&Scale::LARGE);
    // A save publishes views; production skips the test build's check that
    // rediscovery agrees with the incremental result.
    app.shared_views.skip_rediscovery_check();
    drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
        channel_id: channels[1],
    })));
    let modifiers = || Message::KeyEvent {
        event: keyboard::Event::ModifiersChanged(keyboard::Modifiers::empty()),
        captured: true,
    };
    let timed = |label: &str, app: &mut Cagents, stage: &mut dyn FnMut(&mut Cagents)| {
        let mut durations = Vec::with_capacity(ROUNDS);
        for round in 0..ROUNDS + 8 {
            let started = Instant::now();
            stage(app);
            if round >= 8 {
                durations.push(started.elapsed());
            }
        }
        summary(label, &mut durations);
    };
    eprintln!(
        "projection tail workload: {} cards, {} channels, {} views, {} worktrees",
        app.durable.cards.len(),
        app.durable.channels.len(),
        app.workspace_model.views.len(),
        app.known_worktree_roots.len(),
    );
    timed("update(KeyEvent modifiers)", &mut app, &mut |app| {
        drop(app.update(modifiers()))
    });
    timed("update(Frame)", &mut app, &mut |app| {
        drop(app.update(Message::Frame(Instant::now())))
    });
    timed("update(ChatUi Tick)", &mut app, &mut |app| {
        drop(app.update(Message::ChatUi(chat_ui::Action::Tick)))
    });
    timed(
        "update(TerminalFailure ProbeRetryDue)",
        &mut app,
        &mut |app| {
            drop(app.update(Message::TerminalFailure(
                terminal_failure::Action::ProbeRetryDue,
            )))
        },
    );
    timed(
        "update(AcknowledgeVisiblePane superseded)",
        &mut app,
        &mut |app| {
            let card = app.workspace.focused().expect("the channel focuses a pane");
            drop(app.update(Message::AcknowledgeVisiblePane {
                card,
                epoch: app.attention_focus_epoch.wrapping_sub(1),
            }))
        },
    );
    // The save a channel click defers, which falls due with nothing left to
    // project: the click projected its navigation.
    let mut saves = Vec::with_capacity(ROUNDS);
    let mut projected = 0;
    for round in 0..ROUNDS + 8 {
        drop(app.update(Message::Sidebar(SidebarAction::OpenChannel {
            channel_id: channels[1 + round % 2],
        })));
        assert!(app.navigation_save.is_some(), "the click defers its save");
        let before = super::terminal_output_tests::projections();
        let started = Instant::now();
        drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
        if round >= 8 {
            saves.push(started.elapsed());
            projected += super::terminal_output_tests::projections() - before;
        }
        assert!(app.navigation_save.is_none(), "the save ran");
    }
    summary("update(NavigationSaveDue after a click)", &mut saves);
    eprintln!("  of which {projected} of {ROUNDS} projected the whole workspace");
    timed("project_workspace (Full)", &mut app, &mut |app| {
        drop(app.project_workspace(false))
    });
    // Most full projections follow an edit, which gives the workspace a new
    // address, so every cache keyed on it is rebuilt: the card index, the
    // terminal numbers' pruning and the sidebar's attention fold among them.
    // With only weak references left, the edit moves the workspace rather
    // than cloning it; that move is timed alone so it can be subtracted. A
    // real edit while a save holds the workspace also clones it.
    timed("workspace move of an edit", &mut app, &mut |app| {
        Arc::make_mut(&mut app.durable);
        app.card_indices_source = Arc::downgrade(&app.durable);
    });
    timed(
        "project_workspace (Full) after an edit",
        &mut app,
        &mut |app| {
            Arc::make_mut(&mut app.durable);
            drop(app.project_workspace(false))
        },
    );
    timed("  worktree_protections", &mut app, &mut |app| {
        app.sync_worktree_protections()
    });
    timed("  notice_popup", &mut app, &mut |app| {
        drop(app.reconcile_notice_popup())
    });
    timed("  sidebar_selection", &mut app, &mut |app| {
        app.synchronize_sidebar_selection()
    });
    timed("  sidebar_channels", &mut app, &mut |app| {
        app.refresh_sidebar_channels()
    });
    timed("  retained_attention", &mut app, &mut |app| {
        app.sidebar_state
            .prune_retained_attention(&app.sidebar_model)
    });
    timed("  row_selection", &mut app, &mut |app| {
        app.prune_sidebar_row_selection()
    });
    timed("  worktree_panes", &mut app, &mut |app| {
        app.refresh_sidebar_worktree_panes()
    });
    timed("  pane_inspectors", &mut app, &mut |app| {
        app.sync_pane_inspectors()
    });
    timed("  pull_request_inspector", &mut app, &mut |app| {
        app.sync_pull_request_inspector()
    });
    timed("  source_control", &mut app, &mut |app| {
        drop(app.rebind_source_control())
    });
    timed("  chrome", &mut app, &mut |app| drop(app.sync_chrome()));
    timed("  transcript_probes", &mut app, &mut |app| {
        drop(app.terminal_transcript_probes())
    });
    timed("  automation_requests", &mut app, &mut |app| {
        drop(app.read_owed_automation_requests())
    });
    timed("  postlude chat_failures", &mut app, &mut |app| {
        app.reconcile_chat_failure_presentations()
    });
    timed("  postlude pull_request_labels", &mut app, &mut |app| {
        app.sync_pull_request_labels()
    });
    timed("  postlude visible_attention", &mut app, &mut |app| {
        drop(app.sync_visible_attention())
    });
    timed("  postlude notification_presence", &mut app, &mut |app| {
        app.sync_notification_presence()
    });
    timed("  postlude chat_limit_accounts", &mut app, &mut |app| {
        drop(app.refresh_chat_limit_accounts())
    });
}

/// The edges an agent spawn elsewhere brings, as the observer reports them:
/// agents changed, with nothing else for this window to install.
fn agent_edges(app: &Cagents, count: usize) -> Vec<workspace_model_sync::ModelUpdate> {
    let identity = app.workspace_model.identity;
    let sequence = app
        .workspace_model
        .sequence
        .max(app.shared_views.snapshot_sequence);
    (1..=count as u64)
        .map(|offset| workspace_model_sync::ModelUpdate::Changed {
            sequence: sequence + offset,
            edit: crate::workspace_model::ModelEdit::AgentsChanged,
            changes: workspace_model_sync::EdgeChanges {
                identity: Some(identity),
                ..Default::default()
            },
        })
        .collect()
}

/// A burst of shared model edges, such as a spawn's or a startup's, met by a
/// window the size of a long-lived installation: each edge as its own
/// update, as the observer delivered them before, and the same edges as one
/// run.
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn observed_edge_burst_timing() {
    super::projection_oracle::disable();
    let (mut app, _channels, directory) = large_workspace(&Scale::LARGE);
    app.shared_views.skip_rediscovery_check();
    settle_seeds(&mut app);
    const BURST: usize = 8;
    let mut apart = Vec::new();
    let mut runs = Vec::new();
    for _ in 0..3 {
        for edge in agent_edges(&app, BURST) {
            let started = Instant::now();
            drop(app.update(Message::WorkspaceModelObserved(edge)));
            apart.push(started.elapsed());
        }
        let edges = agent_edges(&app, BURST);
        let started = Instant::now();
        drop(app.apply_workspace_model_updates(edges));
        runs.push(started.elapsed());
    }
    summary("update per edge, one edge per update", &mut apart);
    summary(&format!("update per run of {BURST} edges"), &mut runs);
    let phases = save_phases(&directory, || {
        for edge in agent_edges(&app, BURST) {
            drop(app.update(Message::WorkspaceModelObserved(edge)));
        }
    });
    report_phases(phases);
}

/// Edges that change something, as an agent start brings them: an agent
/// record, renamed each time, for an agent placed in one channel. Each runs
/// the complete reconciliation, which the phases split.
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn changing_edge_timing() {
    super::projection_oracle::disable();
    // `CAGENTS_WORKLOAD_HISTORY=N` multiplies the workspace's history - closed
    // cards, worktrees and their empty views - leaving what is open alone, so
    // runs at two sizes show which work grows with history.
    // `CAGENTS_WORKLOAD_CLOSED=N` multiplies closed cards alone, overriding it.
    let multiplier = |name| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
    };
    let history = multiplier("CAGENTS_WORKLOAD_HISTORY").unwrap_or(1);
    let closed = multiplier("CAGENTS_WORKLOAD_CLOSED").unwrap_or(history);
    let scale = Scale {
        worktrees: Scale::LARGE.worktrees * history,
        closed_cards: Scale::LARGE.closed_cards * closed,
        historical_views: Scale::LARGE.historical_views * history,
        ..Scale::LARGE
    };
    let (mut app, channels, directory) = large_workspace(&scale);
    eprintln!(
        "history x{history}, closed cards x{closed}: {} cards, {} views",
        app.durable.cards.len(),
        app.workspace_model.views.len()
    );
    app.shared_views.skip_rediscovery_check();
    settle_seeds(&mut app);
    answer_view_edits(&mut app);
    let mut agent = crate::agent_execution::Agent::fixture_placed_in(channels[0]);
    agent.spec.target.repository = app.workspace_model.channels[0].repository.clone();
    let mut renamed = 0;
    let mut edges = |app: &Cagents, count: usize| {
        let identity = app.workspace_model.identity;
        let sequence = app
            .workspace_model
            .sequence
            .max(app.shared_views.snapshot_sequence);
        (1..=count as u64)
            .map(|offset| {
                renamed += 1;
                agent.name = Some(format!("worker {renamed}"));
                workspace_model_sync::ModelUpdate::Changed {
                    sequence: sequence + offset,
                    edit: crate::workspace_model::ModelEdit::AgentsChanged,
                    changes: workspace_model_sync::EdgeChanges {
                        identity: Some(identity),
                        effects: crate::workspace_model::delta::EditEffects {
                            agents: vec![agent.clone()],
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                }
            })
            .collect::<Vec<_>>()
    };
    // `CAGENTS_WORKLOAD_EDGES=N` measures N edges rather than 12, and
    // `CAGENTS_WORKLOAD_PERF_CTL=FIFO` names the control FIFO of a
    // `perf record --control fifo:FIFO -D -1`, which then samples only them:
    // building the workspace otherwise dominates a profile.
    let count = multiplier("CAGENTS_WORKLOAD_EDGES").unwrap_or(12);
    let perf = std::env::var_os("CAGENTS_WORKLOAD_PERF_CTL").map(|fifo| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(fifo)
            .expect("perf control FIFO")
    });
    let perf_control = |command: &str| {
        if let Some(mut fifo) = perf.as_ref() {
            std::io::Write::write_all(&mut fifo, format!("{command}\n").as_bytes())
                .expect("perf control");
        }
    };
    let mut apart = Vec::new();
    let counter = RetiredInstructions::open();
    let mut instructions = Vec::new();
    let measured = edges(&app, count);
    perf_control("enable");
    for edge in measured {
        let before = counter.as_ref().map(RetiredInstructions::read);
        let started = Instant::now();
        drop(app.update(Message::WorkspaceModelObserved(edge)));
        apart.push(started.elapsed());
        if let (Some(counter), Some(before)) = (&counter, before) {
            instructions.push(counter.read() - before);
        }
    }
    perf_control("disable");
    summary("update per changing edge", &mut apart);
    if instructions.is_empty() {
        eprintln!("instructions per changing edge: no hardware counter here");
    } else {
        instructions.sort_unstable();
        eprintln!(
            "instructions per changing edge n={:<4} median {:>12}  min {:>12}  max {:>12}",
            instructions.len(),
            instructions[instructions.len() / 2],
            instructions[0],
            instructions[instructions.len() - 1]
        );
    }
    let batch = edges(&app, 12);
    let phases = save_phases(&directory, || {
        for edge in batch {
            drop(app.update(Message::WorkspaceModelObserved(edge)));
        }
    });
    report_phases(phases);
}
