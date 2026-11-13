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
//! scripts/cargo-gate test --locked --profile local --no-default-features --features ui \
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
}

impl Scale {
    pub(super) const LARGE: Self = Self {
        worktrees: 3000,
        channels: 76,
        chats: 45,
        loose_terminals: 20,
        closed_cards: 1680,
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
        },
        proof: None,
        terminal: None,
    })
}

fn terminal(directory: &Path) -> CardKind {
    CardKind::Terminal(fresh_terminal_session(LaunchRecipe {
        program: "bash".into(),
        arguments: Vec::new(),
        ..recipe(directory, BTreeMap::new())
    }))
}

fn card(title: String, kind: CardKind, existence: Existence) -> CardState {
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
        });
        app.workspace_model
            .channels
            .push(crate::workspace_model::ModelChannel {
                id,
                repository: repositories[project].clone(),
                title: format!("Channel {index}"),
                title_source: Default::default(),
                ordinal: index as u32 + 1,
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
    views.truncate(scale.channels + 1850);
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

/// Where a steady frame's build time goes, phase by phase, as
/// `UserInterface::build` runs them: the named-state extraction, the
/// reconciliation, the restoration of states that cached subtrees kept, and
/// layout itself. The headless frame above reports these four together as
/// "layout".
#[test]
#[ignore = "timing workload; run optimized with --ignored --nocapture"]
fn steady_frame_phases() {
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
