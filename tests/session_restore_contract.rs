use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use cagents::{
    model::{
        BootId, BrowserId, BrowserRestoreFidelity, BrowserSession, CardId, CardKind, CardState,
        Existence, Exposure, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe,
        ProjectId, ProjectState, RepositoryView, Residency, TerminalContinuity, TerminalId,
        TerminalSession, WorkspaceId, WorkspaceState, WorktreeId, WorktreeState,
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
