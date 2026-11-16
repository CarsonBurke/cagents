use super::*;
use crate::{
    app::workspace_model_sync::ModelUpdate,
    model::{ChannelId, ProjectChannel, WorktreeId},
    workspace_model::{
        ModelChannel, ModelProject, WorkspaceModel,
        views::{Pane, Scope, View},
    },
};

fn terminal(directory: &Path) -> CardState {
    let launch = LaunchRecipe {
        program: "/fixture-terminal-never-executed".into(),
        arguments: Vec::new(),
        working_directory: directory.into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    CardState {
        id: CardId::new(),
        generation: 0,
        title: "Retained terminal".into(),
        width_fraction: 0.5,
        existence: Existence::Open,
        kind: CardKind::Terminal(TerminalSession {
            id: TerminalId::new(),
            launch,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::Live {
                host_session_id: "fixture-host".into(),
                boot_id: "fixture-boot".into(),
                host_incarnation: "fixture-incarnation".into(),
            },
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
        }),
    }
}

#[test]
fn home_admits_retained_worktree_and_channel_views_without_navigation() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = context_inspector_test_app(false);
    let identity = uuid::Uuid::new_v4();
    let mut model = WorkspaceModel {
        identity,
        sequence: 1,
        ..WorkspaceModel::default()
    };
    let durable = Arc::make_mut(&mut app.durable);
    durable.workspace_model_identity = Some(identity);
    let mut expected = HashMap::<Scope, Vec<CardId>>::new();
    for name in ["alpha", "beta"] {
        let path = directory.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        let repository = path.join(".git");
        let project = ProjectId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            name.into(),
            repository.clone(),
            vec![SidebarWorktree::new(
                WorktreeId::new(),
                path.clone(),
                Some("main".into()),
                false,
                false,
                None,
                None,
                None,
            )],
            Vec::new(),
        ));
        model.projects.push(ModelProject {
            repository: repository.clone(),
            name: name.into(),
            ordinal: model.projects.len() as u32,
            kind: Default::default(),
        });
        let card = terminal(&path);
        let scope = Scope::Worktree {
            repository: repository.clone(),
            path: path.clone(),
        };
        let mut view = View::empty(scope.clone());
        view.revision = 1;
        view.panes = vec![Pane::from_card(&card, Some(path.clone())).unwrap()];
        model.views.push(view);
        expected.insert(scope, vec![card.id]);
        durable.card_scopes.insert(card.id, path.clone());
        durable.cards.push(card);
        if name == "alpha" {
            let channel = ChannelId::new();
            let card = terminal(&path);
            durable.channels.push(ProjectChannel {
                id: channel,
                project_id: project,
                title: "Offscreen conversation".into(),
                title_source: Default::default(),
                ordinal: 1,
                shared: true,
                cards: vec![card.id],
                focused: Some(card.id),
            });
            model.channels.push(ModelChannel {
                id: channel,
                repository,
                title: "Offscreen conversation".into(),
                title_source: Default::default(),
                ordinal: 1,
            });
            let scope = Scope::Channel(channel);
            let mut view = View::empty(scope.clone());
            view.revision = 1;
            view.panes = vec![Pane::from_card(&card, Some(path.clone())).unwrap()];
            model.views.push(view);
            expected.insert(scope, vec![card.id]);
            durable.card_scopes.insert(card.id, path);
            durable.cards.push(card);
        }
    }
    app.shared_views = shared_views::State::for_workspace(&app.durable);
    drop(app.restore_window_view(None, None));
    assert_eq!(app.active_workspace, None);
    assert_eq!(app.durable.active_channel, None);
    assert_eq!(app.workspace.focused(), None);
    assert!(app.workspace.cards().is_empty());
    let (publisher, requests) = async_channel::unbounded();
    app.workspace_model_tx = publisher;
    drop(app.apply_workspace_model_update(ModelUpdate::Connected(model)));

    // Home still discovers every retained view, including scopes this window
    // never opened. The registry already holds the same canonical panes, so
    // each is admitted from the connection without a seed or a pane edit.
    assert_eq!(
        app.local_view_scopes().into_iter().collect::<HashSet<_>>(),
        expected.keys().cloned().collect()
    );
    assert_eq!(app.shared_views_startup_hold(), None);
    assert!(
        requests.try_recv().is_err(),
        "admission creates no model edits"
    );
    assert_eq!(app.active_workspace, None);
    assert_eq!(app.durable.active_channel, None);
    assert_eq!(app.workspace.focused(), None);
    assert!(app.workspace.cards().is_empty());
    assert_eq!(app.durable.cards.len(), 3);
}
