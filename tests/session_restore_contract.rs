#![cfg(feature = "ui")]

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};

use cagents::{
    agent::Provider,
    chat::{Attachment, ImageReference, Launch, Session},
    model::{
        BootId, BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, CardState,
        ChannelId, Existence, Exposure, HostIncarnation, HostSessionId, HostSessionProof,
        LaunchRecipe, ProjectChannel, ProjectId, ProjectState, RepositoryView, Residency,
        TerminalContinuity, TerminalId, TerminalSession, WorkspaceId, WorkspaceState, WorktreeId,
        WorktreeState,
    },
    persistence::StateStore,
    session::SessionRuntime,
};
use tempfile::tempdir;
use url::Url;

struct RestoreFixture {
    boot_id: BootId,
    proof: HostSessionProof,
    launch: LaunchRecipe,
    workspace: WorkspaceState,
    project: ProjectState,
    worktree: WorktreeState,
    terminal_card_id: CardId,
    browser_card_id: CardId,
    repository_card_id: CardId,
    terminal_id: TerminalId,
    browser_id: BrowserId,
}

impl RestoreFixture {
    fn new() -> Self {
        let boot_id = BootId::new("boot-session-restore-a");
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("host-session-a"),
            boot_id: boot_id.clone(),
            host_incarnation: HostIncarnation::new("host-incarnation-a"),
        };
        let project_id = ProjectId::new();
        let worktree_id = WorktreeId::new();
        let terminal_card_id = CardId::new();
        let browser_card_id = CardId::new();
        let repository_card_id = CardId::new();
        let terminal_id = TerminalId::new();
        let browser_id = BrowserId::new();
        let repository_path = PathBuf::from("/srv/cagents");
        let worktree_path = repository_path.join(".worktrees/session-restore");
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/usr/bin/env".into(),
            arguments: vec!["bash".into(), "--noprofile".into()],
            working_directory: worktree_path.clone(),
            environment: BTreeMap::from([
                ("LANG".into(), "C.UTF-8".into()),
                ("TERM".into(), "xterm-256color".into()),
            ]),
        };
        let browser_history = vec![
            Url::parse("https://example.test/projects").unwrap(),
            Url::parse("https://example.test/projects/cagents").unwrap(),
            Url::parse("https://example.test/projects/cagents/activity").unwrap(),
        ];
        let project = ProjectState {
            kind: Default::default(),
            id: project_id,
            generation: 0,
            name: "cagents".into(),
            repository: repository_path.clone(),
            existence: Existence::Open,
        };
        let worktree = WorktreeState {
            id: worktree_id,
            project_id,
            generation: 0,
            path: worktree_path.clone(),
            branch: Some("feature/session-restore".into()),
            existence: Existence::Open,
        };
        let workspace = WorkspaceState {
            schema_version: 1,
            id: WorkspaceId::new(),
            generation: 0,
            existence: Existence::Open,
            active_worktree: Some(worktree_path.clone()),
            worktree_focus: HashMap::from([(worktree_path.clone(), terminal_card_id)]),
            workspace_history: Vec::new(),
            project_order: Vec::new(),
            channels: Vec::new(),
            active_channel: None,
            card_scopes: HashMap::new(),
            chat_drafts: HashMap::new(),
            chat_draft_attachments: HashMap::new(),
            conversation_channels: Default::default(),
            agent_updates: BTreeMap::new(),
            repository_navigation: HashMap::new(),
            local_only_cards: Default::default(),
            pending_chat_admissions: Default::default(),
            pending_terminal_creations: Default::default(),
            workspace_model_identity: None,
            cards: vec![
                CardState {
                    id: terminal_card_id,
                    generation: 0,
                    title: "Build shell".into(),
                    width_fraction: 0.38,
                    existence: Existence::Open,
                    exposure: Exposure::Visible,
                    residency: Residency::Hot,
                    kind: CardKind::Terminal(TerminalSession {
                        owned_codex_listener: None,
                        recovery_origin: None,
                        predecessor: None,
                        id: terminal_id,
                        launch: launch.clone(),
                        transcript_path: Some(PathBuf::from("/var/tmp/cagents/transcript.cast")),
                        resume_hint: Some("tmux:session-restore".into()),
                        continuity: TerminalContinuity::Live {
                            host_session_id: proof.host_session_id.clone(),
                            boot_id: proof.boot_id.clone(),
                            host_incarnation: proof.host_incarnation.clone(),
                        },
                    }),
                },
                CardState {
                    id: browser_card_id,
                    generation: 0,
                    title: "Project activity".into(),
                    width_fraction: 0.34,
                    existence: Existence::Open,
                    exposure: Exposure::Prefetch,
                    residency: Residency::Hibernated,
                    kind: CardKind::Browser(BrowserSession {
                        id: browser_id,
                        current_url: browser_history[1].clone(),
                        history: browser_history,
                        history_index: 1,
                        scroll_x: 17.25,
                        scroll_y: 912.5,
                        fidelity: BrowserRestoreFidelity::ExactLive,
                    }),
                },
                CardState {
                    id: repository_card_id,
                    generation: 0,
                    title: "Working tree".into(),
                    width_fraction: 0.28,
                    existence: Existence::Open,
                    exposure: Exposure::Offscreen,
                    residency: Residency::Cold,
                    kind: CardKind::Repository(RepositoryView {
                        selected_file: None,
                        project: project_id,
                        repository: repository_path,
                        worktree: worktree_path,
                    }),
                },
            ],
            focused: Some(browser_card_id),
        };

        Self {
            boot_id,
            proof,
            launch,
            workspace,
            project,
            worktree,
            terminal_card_id,
            browser_card_id,
            repository_card_id,
            terminal_id,
            browser_id,
        }
    }

    /// Persists through the lane production actually uses: `SessionRuntime`
    /// owns the only debounce in the codebase, coalesces the snapshots it is
    /// handed, and flushes them on shutdown.
    fn persist(&self, database_path: &std::path::Path) {
        let (mut runtime, restored) =
            SessionRuntime::open(database_path, self.boot_id.clone()).unwrap();
        assert!(
            restored.is_none(),
            "a fresh database must not restore a workspace"
        );
        runtime.save_project(self.project.clone());
        runtime.save_worktrees([self.worktree.clone()]);

        // Two snapshots inside the debounce window must collapse into a single
        // commit, which is why the restored workspace generation below is 1.
        let snapshot = Arc::new(self.workspace.clone());
        runtime.snapshot(Arc::clone(&snapshot));
        runtime.snapshot(snapshot);
        runtime.shutdown();
        assert_eq!(runtime.last_error(), None);
    }
}

#[test]
fn clean_shutdown_reopen_restores_workspace_catalog_and_live_host_proof() {
    let directory = tempdir().unwrap();
    let database_path = directory.path().join("session.sqlite3");
    let fixture = RestoreFixture::new();
    fixture.persist(&database_path);

    let reopened = StateStore::open(&database_path, fixture.boot_id.clone()).unwrap();
    assert!(!reopened.recovery_report().dirty_shutdown);
    assert!(!reopened.recovery_report().boot_changed);
    assert_eq!(reopened.recovery_report().terminals_downgraded, 0);

    let projects = reopened.list_projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, fixture.project.id);
    assert_eq!(projects[0].generation, 1);
    assert_eq!(projects[0].name, fixture.project.name);
    assert_eq!(projects[0].repository, fixture.project.repository);
    assert_eq!(projects[0].existence, Existence::Open);

    let worktrees = reopened.list_worktrees().unwrap();
    assert_eq!(worktrees.len(), 1);
    assert_eq!(worktrees[0].id, fixture.worktree.id);
    assert_eq!(worktrees[0].project_id, fixture.project.id);
    assert_eq!(worktrees[0].generation, 1);
    assert_eq!(worktrees[0].path, fixture.worktree.path);
    assert_eq!(worktrees[0].branch, fixture.worktree.branch);
    assert_eq!(worktrees[0].existence, Existence::Open);

    let restored = reopened
        .load_workspace(fixture.workspace.id, Some(&fixture.proof))
        .unwrap()
        .expect("queued workspace must be flushed by clean shutdown");
    assert_eq!(restored.id, fixture.workspace.id);
    assert_eq!(restored.generation, 1);
    assert_eq!(restored.focused, Some(fixture.browser_card_id));
    assert_eq!(restored.active_worktree, fixture.workspace.active_worktree);
    assert_eq!(restored.worktree_focus, fixture.workspace.worktree_focus);
    assert_eq!(
        restored
            .cards
            .iter()
            .map(|card| card.id)
            .collect::<Vec<_>>(),
        vec![
            fixture.terminal_card_id,
            fixture.browser_card_id,
            fixture.repository_card_id,
        ]
    );
    assert_eq!(
        restored
            .cards
            .iter()
            .map(|card| card.title.as_str())
            .collect::<Vec<_>>(),
        vec!["Build shell", "Project activity", "Working tree"]
    );

    let CardKind::Terminal(terminal) = &restored.cards[0].kind else {
        panic!("first restored card must remain a terminal");
    };
    assert_eq!(terminal.id, fixture.terminal_id);
    assert_eq!(terminal.launch, fixture.launch);
    assert_eq!(
        terminal.transcript_path,
        Some(PathBuf::from("/var/tmp/cagents/transcript.cast"))
    );
    assert_eq!(
        terminal.resume_hint.as_deref(),
        Some("tmux:session-restore")
    );
    assert_eq!(
        terminal.continuity,
        TerminalContinuity::Live {
            host_session_id: fixture.proof.host_session_id.clone(),
            boot_id: fixture.proof.boot_id.clone(),
            host_incarnation: fixture.proof.host_incarnation.clone(),
        }
    );

    let CardKind::Browser(browser) = &restored.cards[1].kind else {
        panic!("second restored card must remain a browser");
    };
    assert_eq!(browser.id, fixture.browser_id);
    assert_eq!(
        browser.current_url,
        Url::parse("https://example.test/projects/cagents").unwrap()
    );
    assert_eq!(
        browser.history,
        vec![
            Url::parse("https://example.test/projects").unwrap(),
            Url::parse("https://example.test/projects/cagents").unwrap(),
            Url::parse("https://example.test/projects/cagents/activity").unwrap(),
        ]
    );
    assert_eq!(browser.history_index, 1);
    assert_eq!(browser.scroll_x, 17.25);
    assert_eq!(browser.scroll_y, 912.5);
    assert_eq!(browser.fidelity, BrowserRestoreFidelity::HistoryAndScroll);

    let CardKind::Repository(repository) = &restored.cards[2].kind else {
        panic!("third restored card must remain a repository view");
    };
    assert_eq!(repository.project, fixture.project.id);
    assert_eq!(repository.repository, fixture.project.repository);
    assert_eq!(repository.worktree, fixture.worktree.path);

    reopened.shutdown().unwrap();
}

/// Exercise the production debounced writer, not only direct StateStore JSON:
/// image-only drafts and metadata-only reorder must survive navigation saves.
#[test]
fn managed_image_drafts_survive_debounced_shutdown_navigation_and_metadata_only_edits() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("image-draft-session.sqlite3");
    let mut fixture = RestoreFixture::new();
    let text_card = CardId::new();
    let image_only_card = CardId::new();
    for card in [text_card, image_only_card] {
        fixture.workspace.cards.push(CardState {
            id: card,
            generation: 0,
            title: "Unsent image draft".into(),
            width_fraction: 0.5,
            existence: Existence::Open,
            exposure: Exposure::Offscreen,
            residency: Residency::Cold,
            kind: CardKind::Chat(Session {
                id: TerminalId::new(),
                launch: Launch {
                    provider: Provider::Claude,
                    recipe: LaunchRecipe {
                        program: "claude".into(),
                        arguments: Vec::new(),
                        ..fixture.launch.clone()
                    },
                    session_id: None,
                },
                proof: None,
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            }),
        });
    }
    let device = uuid::Uuid::new_v4();
    // The fixture tests exact receipt metadata durability. Blob validation
    // and provider admission are separate contracts; neither runs here.
    let image = |number, digest: &str| {
        let id = uuid::Uuid::new_v4();
        Attachment {
            path: format!("/backend-owned/images/{id}.png").into(),
            reference: Some(ImageReference {
                id,
                number,
                device,
                name: "literal {draft} *.png".into(),
                sha256: digest.repeat(64),
                extension: "png".into(),
            }),
        }
    };
    let first = image(41, "a");
    let second = image(7, "b");
    fixture.workspace.chat_drafts.insert(
        text_card,
        format!(
            "Before {} between {} after",
            second.reference.as_ref().unwrap().token(),
            first.reference.as_ref().unwrap().token()
        ),
    );
    fixture.workspace.chat_draft_attachments = HashMap::from([
        (text_card, vec![first.clone(), second.clone()]),
        (image_only_card, vec![first.clone()]),
    ]);
    fixture.persist(&database);

    let (mut runtime, restored) = SessionRuntime::open(&database, fixture.boot_id.clone()).unwrap();
    let mut state = restored.expect("debounced shutdown must retain image associations");
    assert_eq!(state.chat_drafts, fixture.workspace.chat_drafts);
    assert_eq!(
        state.chat_draft_attachments,
        fixture.workspace.chat_draft_attachments
    );
    assert!(!state.chat_drafts.contains_key(&image_only_card));
    for id in [text_card, image_only_card] {
        assert!(
            matches!(&state.cards.iter().find(|card| card.id == id).unwrap().kind, CardKind::Chat(session) if session.is_unstarted())
        );
    }
    state.active_worktree = Some("/srv/navigation-after-images".into());
    runtime.snapshot(Arc::new(state.clone()));
    // Cards and text do not change: a metadata-only update must replace the
    // earlier queued navigation snapshot rather than disappear in debounce.
    state
        .chat_draft_attachments
        .get_mut(&text_card)
        .unwrap()
        .reverse();
    runtime.snapshot(Arc::new(state.clone()));
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);

    let (mut runtime, restored) = SessionRuntime::open(&database, fixture.boot_id).unwrap();
    let restored = restored.expect("image associations must survive a second restart");
    assert_eq!(restored.active_worktree, state.active_worktree);
    assert_eq!(restored.chat_drafts, state.chat_drafts);
    assert_eq!(
        restored.chat_draft_attachments[&text_card],
        [second, first.clone()]
    );
    assert_eq!(restored.chat_draft_attachments[&image_only_card], [first]);
    assert!(!restored.chat_drafts.contains_key(&image_only_card));
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);
}

#[test]
fn changed_boot_reopens_terminal_as_relaunchable_not_live() {
    let directory = tempdir().unwrap();
    let database_path = directory.path().join("session.sqlite3");
    let fixture = RestoreFixture::new();
    fixture.persist(&database_path);

    let reopened = StateStore::open(&database_path, BootId::new("boot-session-restore-b")).unwrap();
    assert!(!reopened.recovery_report().dirty_shutdown);
    assert!(reopened.recovery_report().boot_changed);
    assert_eq!(reopened.recovery_report().terminals_downgraded, 1);

    let restored = reopened
        .load_workspace(fixture.workspace.id, Some(&fixture.proof))
        .unwrap()
        .expect("workspace must survive a host reboot");
    assert_eq!(restored.id, fixture.workspace.id);
    assert_eq!(restored.focused, Some(fixture.browser_card_id));
    assert_eq!(
        restored
            .cards
            .iter()
            .map(|card| card.id)
            .collect::<Vec<_>>(),
        vec![
            fixture.terminal_card_id,
            fixture.browser_card_id,
            fixture.repository_card_id,
        ]
    );

    let CardKind::Terminal(terminal) = &restored.cards[0].kind else {
        panic!("first restored card must remain a terminal");
    };
    assert_eq!(terminal.id, fixture.terminal_id);
    assert_eq!(
        terminal.continuity,
        TerminalContinuity::RelaunchAvailable {
            recipe: fixture.launch.clone(),
            last_exit: None,
        }
    );
    assert!(!terminal.continuity.can_resume_live(&fixture.proof));

    reopened.shutdown().unwrap();
}

#[test]
fn dedicated_file_descriptor_survives_sqlite_session_restore() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("session.sqlite3");
    let mut fixture = RestoreFixture::new();
    let CardKind::Repository(view) = &mut fixture.workspace.cards[2].kind else {
        panic!()
    };
    view.selected_file = Some(PathBuf::from("results/report one.html"));
    fixture.persist(&database);
    let reopened = StateStore::open(&database, fixture.boot_id.clone()).unwrap();
    let restored = reopened
        .load_workspace(fixture.workspace.id, Some(&fixture.proof))
        .unwrap()
        .unwrap();
    assert_eq!(restored.cards[2].kind, fixture.workspace.cards[2].kind);
    reopened.shutdown().unwrap();
}

#[test]
fn browser_focus_and_navigation_only_snapshots_survive_restart_without_cards() {
    let directory = tempdir().unwrap();
    let database_path = directory.path().join("navigation.sqlite3");
    let fixture = RestoreFixture::new();
    fixture.persist(&database_path);

    let (mut runtime, restored) =
        SessionRuntime::open(&database_path, fixture.boot_id.clone()).unwrap();
    let mut state = restored.unwrap();
    assert_eq!(state.focused, Some(fixture.browser_card_id));
    assert_eq!(state.active_worktree, Some(fixture.worktree.path.clone()));
    assert_eq!(state.worktree_focus, fixture.workspace.worktree_focus);
    state.active_worktree = Some(PathBuf::from("/srv/empty-worktree"));
    state
        .worktree_focus
        .insert(fixture.worktree.path.clone(), fixture.repository_card_id);
    runtime.snapshot(Arc::new(state.clone()));
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);

    let (mut runtime, restored) =
        SessionRuntime::open(&database_path, fixture.boot_id.clone()).unwrap();
    let mut restored = restored.unwrap();
    assert_eq!(restored.focused, state.focused);
    assert_eq!(restored.active_worktree, state.active_worktree);
    assert_eq!(restored.worktree_focus, state.worktree_focus);
    restored.cards.clear();
    restored.focused = None;
    restored.worktree_focus.clear();
    runtime.snapshot(Arc::new(restored));
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);

    let (mut runtime, restored) = SessionRuntime::open(&database_path, fixture.boot_id).unwrap();
    let restored = restored.unwrap();
    assert_eq!(restored.focused, None);
    assert_eq!(restored.active_worktree, state.active_worktree);
    assert!(restored.worktree_focus.is_empty());
    assert!(restored.cards.iter().all(|card| !card.existence.is_open()));
    runtime.shutdown();
}

/// A channel is durable state in its own right: its identity, the chats it
/// owns, the chat it had focused, its title, and the fact that it was selected
/// all have to come back.
///
/// Storage spells this state exactly the way the build before the channel
/// rename did - `cagents::persistence` pins those field names against the
/// on-disk format - so this is equally the restore path for a workspace an
/// older installation saved.
#[test]
fn a_selected_channel_restores_its_chats_focus_and_title() {
    let directory = tempdir().unwrap();
    let database_path = directory.path().join("channels.sqlite3");
    let mut fixture = RestoreFixture::new();
    let channel_id = ChannelId::new();
    fixture.workspace.channels = vec![ProjectChannel {
        title_source: Default::default(),
        id: channel_id,
        project_id: fixture.project.id,
        title: "Investigate startup".into(),
        // A channel this window created and has already shared, which is the
        // ordinary state of a channel that has been alive long enough to be
        // restored: the rank the registry gave it has to survive a restart, or
        // a window would resort its sidebar on every launch.
        ordinal: 7,
        shared: true,
        cards: vec![fixture.terminal_card_id, fixture.browser_card_id],
        focused: Some(fixture.browser_card_id),
    }];
    fixture.workspace.active_channel = Some(channel_id);
    fixture.persist(&database_path);

    let (mut runtime, restored) =
        SessionRuntime::open(&database_path, fixture.boot_id.clone()).unwrap();
    let restored = restored.expect("a saved workspace must restore");
    assert_eq!(restored.active_channel, Some(channel_id));
    assert_eq!(restored.channels.len(), 1);
    assert_eq!(restored.channels[0].id, channel_id);
    assert_eq!(restored.channels[0].project_id, fixture.project.id);
    assert_eq!(restored.channels[0].title, "Investigate startup");
    assert_eq!(
        restored.channels[0].cards,
        vec![fixture.terminal_card_id, fixture.browser_card_id]
    );
    assert_eq!(restored.channels[0].focused, Some(fixture.browser_card_id));
    // The rank and the share flag are what let a restarted window render its
    // sidebar in the shared order before the registry has answered, and tell a
    // channel the registry has never seen from one it deleted while this window
    // was down. Losing either across a restore turns the second case into the
    // first, which republishes a deleted channel.
    assert_eq!(restored.channels[0].ordinal, 7);
    assert!(restored.channels[0].shared);
    runtime.shutdown();
    assert_eq!(runtime.last_error(), None);
}
