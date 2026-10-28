//! Resident data for the interactive offline native UI diagnostic.
//!
//! These are synthetic conversations and repository rows, not imported history
//! or evidence of a connected account. The isolation owner mounts this module
//! only for the interactive fixture; the automatic latency workload stays apart.
use super::*;
use crate::model::WorkspaceId;
use crate::repository::{
    ChangeArea, ChangeKind, ChangedPath, DirtyState, RepoPath, RepositorySnapshot, StatusSnapshot,
    WorkingFiles,
};
use crate::sidebar::{ActiveWorktreeKey, SourceControlModel};

const ROOT: &str = "/offline-native-fixture/atlas";
const ID_BASE: u128 = 0x1881_0000_0000_4000_8000_0000_0000_0000;
const OFFLINE: &str = "Offline UI fixture — no daemon or provider is connected. Text, file rows and attachment labels are synthetic. Backend actions are unavailable.";
const TITLES: [&str; 3] = [
    "Fixture · Plan a readable workspace",
    "Fixture · Review the layout checklist",
    "Fixture · Empty conversation",
];

pub(in crate::app) fn bootstrap() -> Bootstrap {
    let mut workspace = WorkspaceState {
        id: WorkspaceId::from_uuid(uuid::Uuid::from_u128(ID_BASE)),
        active_worktree: Some(ROOT.into()),
        ..WorkspaceState::default()
    };
    for (index, title) in TITLES.into_iter().enumerate() {
        let id = CardId::from_uuid(uuid::Uuid::from_u128(ID_BASE + 1 + index as u128));
        workspace.cards.push(CardState {
            id,
            generation: 0,
            title: title.into(),
            width_fraction: 0.5,
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Hot,
            kind: CardKind::Chat(session(index)),
        });
        workspace.card_scopes.insert(id, ROOT.into());
    }
    workspace.focused = workspace.cards.first().map(|card| card.id);
    Bootstrap {
        workspace,
        session: None,
        // Install sidebar presentation after normal startup's missing-directory
        // pruning. No fixture path is read, watched, or admitted as a project.
        projects: Vec::new(),
        worktrees: Vec::new(),
        settings: AppSettings::default(),
        settings_path: None,
        settings_write_blocked: true,
        status: Some(OFFLINE.into()),
        save_health: None,
        restore_warnings: Vec::new(),
        settings_warnings: Vec::new(),
    }
}

pub(in crate::app) fn install(app: &mut Cagents) {
    app.cards.clear();
    for (index, card) in app.durable.cards.iter().enumerate() {
        let CardKind::Chat(session) = &card.kind else {
            continue;
        };
        let target = Target {
            card: card.id,
            generation: card.generation,
        };
        let mut runtime = Runtime::draft(
            target,
            session.clone(),
            Snapshot::default(),
            true,
            app.settings.editor_syntax_theme,
        );
        runtime.install_snapshot(Arc::new(snapshot(index)), None);
        if index == 0 {
            runtime.replace_draft(text_editor::Content::with_text(
                "Offline draft: select this text, move the caret, or type locally. Send is unavailable.",
            ));
            // Non-image metadata only: no thumbnail loader, file chooser, file
            // read, managed attachment reference, or upload is involved.
            runtime.replace_attachments(vec![
                Attachment::file("/offline-native-fixture/static-fixture-notes.md"),
                Attachment::file("/offline-native-fixture/static-fixture-layout.csv"),
            ]);
        }
        app.cards
            .insert(card.id, CardRuntime::Chat(Box::new(runtime)));
    }

    let repository = repository_snapshot();
    let key = ActiveWorktreeKey {
        project_id: project_id(0),
        worktree_id: worktree_id(0),
        canonical_path: ROOT.into(),
    };
    app.sidebar_model = SidebarModel::ready(
        SidebarMode::Expanded,
        sidebar_projects(repository.status.dirty.clone()),
    )
    .with_source_control(Some(SourceControlModel::ready(key.clone(), &repository)));
    app.sidebar_state = SidebarState::default();
    let _ = app.sidebar_state.sync_active_worktree(
        &app.sidebar_model,
        &SidebarFocus::Repository {
            worktree: ROOT.into(),
        },
    );
    for directory in ["docs", "docs/chapters", "src", "src/panels", "tests"] {
        app.sidebar_state.apply(&SidebarAction::ToggleDirectory {
            key: key.clone(),
            path: repo_path(directory),
        });
    }
    app.active_workspace = Some(ROOT.into());
    app.context_inspector_open = true;
    app.context_tab = ContextTab::Files;
    app.chrome_ui.projects_collapsed = false;
    app.chrome_ui.tabs_focus = app.workspace.focused();
    // A public unavailable state, never a synthetic PR target or refresh task.
    drop(app.pull_request_inspector.synchronize(
        None,
        Some("Pull requests are unavailable in the offline UI fixture.".into()),
    ));
}

fn session(index: usize) -> chat::Session {
    chat::Session {
        id: TerminalId::from_uuid(uuid::Uuid::from_u128(ID_BASE + 101 + index as u128)),
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: LaunchRecipe {
                program: "/bin/false".into(),
                // The recipe is never executed. An explicit default keeps
                // draft preview from consulting real Claude account settings.
                arguments: vec!["--permission-mode=default".into()],
                working_directory: ROOT.into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    }
}

fn snapshot(index: usize) -> Snapshot {
    Snapshot {
        generation: 1,
        items: if index < 2 {
            conversation(index)
        } else {
            Vec::new()
        },
        title: Some(TITLES[index].into()),
        detail: "Resident offline UI fixture".into(),
        read_only_reason: Some(OFFLINE.into()),
        exited: true,
        ..Snapshot::default()
    }
}

fn conversation(conversation: usize) -> Vec<Arc<chat::Item>> {
    const TOPICS: [(&str, &str); 12] = [
        (
            "Reading column",
            "Keep long answers comfortable to read while the scrollbar remains at the pane boundary.",
        ),
        (
            "Split panes",
            "Compare two conversations without losing the selected text or the active pane's keyboard focus.",
        ),
        (
            "Narrow windows",
            "Let paragraphs wrap naturally and keep the composer aligned with the transcript.",
        ),
        (
            "Project navigation",
            "Scan a long list of projects, expand one group, and keep the current workspace easy to identify.",
        ),
        (
            "File tree",
            "Expand documentation and source folders, filter by a filename, then clear the filter.",
        ),
        (
            "Changes list",
            "Read staged, unstaged, and untracked groups without offering a real repository mutation.",
        ),
        (
            "Inspector details",
            "Inspect the selected pane's title and synthetic working directory, then return to Files.",
        ),
        (
            "Selection and copy",
            "Select a sentence across wrapped lines and use the resident message copy control.",
        ),
        (
            "Find navigation",
            "Search for the phrase rail marker, visit another match, and close Find without losing the draft.",
        ),
        (
            "Attachment labels",
            "Keep static fixture attachment chips readable without opening, reading, or uploading a file.",
        ),
        (
            "Scroll and resize",
            "Drag the conversation thumb, then resize the neighboring pane and continue reading.",
        ),
        (
            "Return to the latest",
            "Scroll away from the end, inspect earlier paragraphs, and use the latest-position control.",
        ),
    ];
    let mut items = Vec::new();
    for (turn, (topic, purpose)) in TOPICS.into_iter().enumerate() {
        let topic = if conversation == 0 {
            topic.to_owned()
        } else {
            format!("Checklist: {topic}")
        };
        items.push(text_item(
            conversation,
            turn,
            "question",
            chat::Role::User,
            format!(
                "Offline fixture question {}: {topic}\n\n{purpose}",
                turn + 1
            ),
        ));
        let mut answer = format!(
            "## {topic}\n\n{purpose}\n\nThis is synthetic, resident conversation text for a native UI diagnostic. No agent produced this answer, no account is connected, and no project is being changed. The **rail marker {:02}** is a stable Find target in each turn.\n\nA person reading this pane should be able to keep their place, select a useful passage, and move between the transcript and composer. Resize the pane slowly, then try a quick drag. Check the first and last visible lines as well as the center of the paragraph; a clipped edge can be easy to miss in a static image.\n\nThe neighboring pane contains a different resident conversation. Its selection and scroll position should remain understandable when focus moves. The sidebar and inspector use synthetic names, so screenshots contain no private repositories or conversations.\n\n",
            turn + 1,
        );
        if turn % 3 == 0 {
            answer.push_str(
                "| Local task | What to observe |\n| --- | --- |\n| Drag the thumb | The conversation moves within its pane |\n| Drag the divider | The neighboring pane changes width |\n| Copy a passage | Only resident fixture text is copied |\n\n",
            );
        } else if turn % 3 == 1 {
            answer.push_str(
                "```rust\n// Static example; this code is never executed.\nfn reading_position(previous: usize, visible: usize) -> usize {\n    previous.min(visible.saturating_sub(1))\n}\n```\n\n",
            );
        } else {
            answer.push_str(
                "> Keep a short phrase selected while changing the pane width. The selection should stay attached to the words the reader chose.\n\n- Read the complete paragraph.\n- Move focus to the other conversation.\n- Return and verify the draft is still present.\n\n",
            );
        }
        answer.push_str(
            "Link-shaped text: [public example documentation](https://example.com/offline-ui-fixture). Selecting or copying the text is local. Opening external links is unavailable in this offline diagnostic.\n\nBackend reconnect, send, refresh, stage, discard, provider history, and account actions are unavailable. This fixture can support native layout and local interaction evidence; it cannot establish live provider behavior or repository correctness.",
        );
        items.push(text_item(
            conversation,
            turn,
            "answer",
            chat::Role::Assistant,
            answer,
        ));
    }
    items
}

fn text_item(
    conversation: usize,
    turn: usize,
    part: &str,
    role: chat::Role,
    text: String,
) -> Arc<chat::Item> {
    Arc::new(chat::Item {
        id: format!("offline-fixture:{conversation}:{turn}:{part}"),
        role,
        content: ItemContent::Text {
            text,
            thinking: false,
            phase: chat::Phase::Final,
        },
        started_at: None,
        at: None,
    })
}

fn project_id(index: usize) -> ProjectId {
    ProjectId::from_uuid(uuid::Uuid::from_u128(ID_BASE + 201 + index as u128))
}

fn worktree_id(index: usize) -> WorktreeId {
    WorktreeId::from_uuid(uuid::Uuid::from_u128(ID_BASE + 301 + index as u128))
}

fn sidebar_projects(dirty: DirtyState) -> Vec<SidebarProject> {
    (0..24)
        .map(|index| {
            let path = if index == 0 {
                PathBuf::from(ROOT)
            } else {
                PathBuf::from(format!("/offline-native-fixture/project-{:02}", index + 1))
            };
            let mut project = SidebarProject::new(
                project_id(index),
                RepositoryId(uuid::Uuid::from_u128(ID_BASE + 401 + index as u128)),
                if index == 0 {
                    "Offline fixture · Atlas".into()
                } else {
                    format!("Fixture project {:02} · Reading workspace", index + 1)
                },
                path.clone(),
                vec![SidebarWorktree::new(
                    worktree_id(index),
                    path,
                    (index != 0).then(|| "fixture/layout-check".into()),
                    false,
                    false,
                    (index == 0).then(|| dirty.clone()),
                    None,
                    None,
                )],
                Vec::new(),
            );
            project.github_status = "Unavailable in the offline UI fixture".into();
            project
        })
        .collect()
}

fn repository_snapshot() -> RepositorySnapshot {
    let mut paths = vec![repo_path("FIXTURE-README.md")];
    let mut entries = Vec::new();
    for index in 0..96 {
        let path = repo_path(&match index % 3 {
            0 => format!("docs/chapters/fixture-note-{index:02}.md"),
            1 => format!("src/panels/fixture-panel-{index:02}.rs"),
            _ => format!("tests/fixture-scroll-{index:02}.rs"),
        });
        paths.push(path.clone());
        let (area, kind) = match index % 3 {
            0 => (ChangeArea::Staged, ChangeKind::Modified),
            1 => (ChangeArea::Unstaged, ChangeKind::Modified),
            _ => (ChangeArea::Untracked, ChangeKind::Untracked),
        };
        entries.push(ChangedPath {
            path,
            old_path: None,
            area,
            kind,
            conflict: None,
            submodule: false,
            added_lines: Some(4 + index as u64),
            removed_lines: Some(if index % 3 == 2 { 0 } else { 2 }),
        });
    }
    paths.sort();
    RepositorySnapshot {
        status: StatusSnapshot {
            // No invented commit, tracking result, remote, or branch authority.
            dirty: DirtyState {
                staged: 32,
                unstaged: 32,
                untracked: 32,
                ..DirtyState::default()
            },
            entries,
            ..StatusSnapshot::default()
        },
        files: WorkingFiles {
            paths,
            tracked_count: 65,
            ..WorkingFiles::default()
        },
        remotes: Vec::new(),
        local_refs: Vec::new(),
        remote_refs: Vec::new(),
        remote_heads: Vec::new(),
    }
}

fn repo_path(path: &str) -> RepoPath {
    RepoPath::parse(path).expect("fixed relative offline fixture path")
}
