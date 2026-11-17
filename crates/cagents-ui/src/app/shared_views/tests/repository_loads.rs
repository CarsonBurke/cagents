//! Background repository loads share one scoped reconciliation; see
//! [`Cagents::defer_repository_reconciliation`].

use super::*;
use crate::model::WorktreeId;
use crate::workspace_model::{ModelChannel, ModelProject};

/// One restored project: its registry rows and the panes the registry holds
/// for it that this window has not adopted yet.
struct RestoredProject {
    id: ProjectId,
    repository_id: RepositoryId,
    worktree: WorktreeId,
    channel: ChannelId,
    channel_pane: (CardId, BrowserId),
    worktree_pane: (CardId, BrowserId),
}

/// A window whose registry already holds three projects, each with a channel
/// and a worktree view this window has never adopted, plus a channel of its
/// own the registry has never heard of. An unrelated project is already
/// settled. Identifiers are fixed, so every [`Self::app`] is the same window.
struct RestoredWorkspace {
    id: crate::model::WorkspaceId,
    identity: uuid::Uuid,
    unrelated: (ProjectId, RepositoryId, ChannelId, CardId, BrowserId),
    unshared: ChannelId,
    projects: Vec<RestoredProject>,
}

fn browser_pane((id, browser): (CardId, BrowserId), title: &str) -> Pane {
    Pane {
        id,
        title: title.into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 0.5,
        scope: None,
        kind: CardKind::Browser(BrowserSession {
            id: browser,
            current_url: "https://example.com".parse().unwrap(),
            history: vec![],
            history_index: 0,
            scroll_x: 0.,
            scroll_y: 0.,
            fidelity: BrowserRestoreFidelity::UrlOnly,
        }),
    }
}

fn repository(index: usize) -> PathBuf {
    PathBuf::from(format!("/repositories/project-{index}/.git"))
}

fn root(index: usize) -> PathBuf {
    PathBuf::from(format!("/repositories/project-{index}"))
}

impl RestoredWorkspace {
    fn new() -> Self {
        Self {
            id: crate::model::WorkspaceId::new(),
            identity: uuid::Uuid::new_v4(),
            unrelated: (
                ProjectId::new(),
                RepositoryId::new(),
                ChannelId::new(),
                CardId::new(),
                BrowserId::new(),
            ),
            unshared: ChannelId::new(),
            projects: (0..3)
                .map(|_| RestoredProject {
                    id: ProjectId::new(),
                    repository_id: RepositoryId::new(),
                    worktree: WorktreeId::new(),
                    channel: ChannelId::new(),
                    channel_pane: (CardId::new(), BrowserId::new()),
                    worktree_pane: (CardId::new(), BrowserId::new()),
                })
                .collect(),
        }
    }

    /// The window just before the loads land, and the loads, in order.
    fn app(&self) -> (Cagents, Vec<Message>) {
        let mut app = super::super::super::tests::context_inspector_test_app(false);
        let (unrelated, unrelated_id, unrelated_channel, card, browser) = self.unrelated;
        let unrelated_repository = PathBuf::from("/unrelated/.git");
        app.sidebar_model.projects.push(SidebarProject::new(
            unrelated,
            unrelated_id,
            "unrelated".into(),
            unrelated_repository.clone(),
            vec![],
            vec![],
        ));
        let durable = Arc::make_mut(&mut app.durable);
        durable.id = self.id;
        durable.workspace_model_identity = Some(self.identity);
        app.workspace_model.identity = self.identity;
        app.workspace_model.sequence = 1;
        app.workspace_model.projects.push(ModelProject {
            repository: unrelated_repository.clone(),
            name: "unrelated".into(),
            kind: Default::default(),
            ordinal: 0,
        });
        app.workspace_model.channels.push(ModelChannel {
            id: unrelated_channel,
            repository: unrelated_repository,
            title: "Unrelated".into(),
            title_source: Default::default(),
            ordinal: 1,
        });
        let mut view = View::empty(unrelated_channel);
        view.revision = 1;
        view.panes.push(browser_pane((card, browser), "Unrelated"));
        app.workspace_model.views.push(view);
        app.shared_views
            .initialized
            .insert(Scope::Channel(unrelated_channel));
        drop(app.reconcile_workspace_model());
        assert!(
            app.durable.cards.iter().any(|state| state.id == card),
            "the unrelated project settles before the loads"
        );

        for (index, project) in self.projects.iter().enumerate() {
            let repository = repository(index);
            app.sidebar_model.projects.push(SidebarProject::new(
                project.id,
                project.repository_id,
                format!("project-{index}"),
                repository.clone(),
                vec![crate::sidebar::SidebarWorktree::from_durable(
                    project.worktree,
                    root(index),
                    Some("main".into()),
                )],
                vec![],
            ));
            app.repository_generations.insert(project.id, 1);
            app.workspace_model.projects.push(ModelProject {
                repository: repository.clone(),
                name: format!("project-{index}"),
                kind: Default::default(),
                ordinal: index as u32 + 1,
            });
            app.workspace_model.channels.push(ModelChannel {
                id: project.channel,
                repository: repository.clone(),
                title: format!("Channel {index}"),
                title_source: Default::default(),
                ordinal: index as u32 + 2,
            });
            let mut channel = View::empty(project.channel);
            channel.revision = 3;
            channel
                .panes
                .push(browser_pane(project.channel_pane, "Channel pane"));
            let worktree_scope = Scope::Worktree {
                repository,
                path: root(index),
            };
            // Scopes this window seeded on an earlier run: the registry's views
            // of them are what it adopts.
            app.shared_views
                .initialized
                .extend([Scope::Channel(project.channel), worktree_scope.clone()]);
            let mut worktree = View::empty(worktree_scope);
            worktree.revision = 2;
            worktree
                .panes
                .push(browser_pane(project.worktree_pane, "Worktree pane"));
            app.workspace_model.views.extend([channel, worktree]);
        }
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(ProjectChannel {
            id: self.unshared,
            project_id: self.projects[0].id,
            title: "Never shared".into(),
            title_source: Default::default(),
            ordinal: 9,
            shared: false,
            cards: vec![],
            focused: None,
        });
        app.workspace_model.sequence = 2;

        let loads = self
            .projects
            .iter()
            .enumerate()
            .map(|(index, project)| Message::RepositoryLoaded {
                open_cards: false,
                project_id: project.id,
                generation: 1,
                result: Ok(Box::new(LoadedRepository {
                    project: ProjectState {
                        kind: Default::default(),
                        id: project.id,
                        generation: 0,
                        name: format!("project-{index}"),
                        repository: repository(index),
                        existence: Existence::Open,
                    },
                    repository: None,
                    worktrees: vec![],
                    github: GitHubAvailability::NotAssociated,
                    review_repository: None,
                    head_repositories: HashMap::new(),
                })),
            })
            .collect();
        (app, loads)
    }
}

/// What a reconciliation leaves that later behaviour reads: the saved
/// workspace (whose save counter depends on how many passes saved it), the
/// registry model, what the window last projected for each scope, and what it
/// told the registry.
#[derive(Debug)]
struct Reconciled {
    durable: WorkspaceState,
    model: crate::workspace_model::WorkspaceModel,
    baseline: Vec<(Scope, Vec<Pane>)>,
    published: Vec<ModelEdit>,
}

fn reconciled(app: &Cagents, published: &TestModelRequests) -> Reconciled {
    assert!(app.pending_repository_reconciliation.is_empty());
    let mut durable = (*app.durable).clone();
    durable.generation = 0;
    let mut baseline = app
        .shared_views
        .baseline
        .iter()
        .map(|(scope, panes)| (scope.clone(), panes.clone()))
        .collect::<Vec<_>>();
    baseline.sort_by_key(|(scope, _)| format!("{scope:?}"));
    Reconciled {
        durable,
        model: app.workspace_model.clone(),
        baseline,
        published: std::iter::from_fn(|| published.try_recv().ok()).collect(),
    }
}

/// Startup's background loads now share one pass over their repositories'
/// scopes. That pass must leave exactly what a complete reconciliation of the
/// same loads leaves, and what one pass per load left before the loads were
/// coalesced: the same adopted channels and panes, the same saved workspace,
/// the same edits sent to the registry.
#[test]
fn coalesced_repository_loads_leave_what_a_complete_reconciliation_leaves() {
    let workspace = RestoredWorkspace::new();
    let run = |deliver: &dyn Fn(&mut Cagents, Vec<Message>)| {
        let (mut app, loads) = workspace.app();
        let (sender, published) = test_model_channel();
        app.workspace_model_tx = sender;
        deliver(&mut app, loads);
        reconciled(&app, &published)
    };

    let coalesced = run(&|app, loads| {
        for load in loads {
            drop(app.update(load));
        }
        assert_eq!(app.pending_repository_reconciliation.len(), 3);
        drop(app.update(Message::RepositoryReconciliationDue));
    });
    let per_load = run(&|app, loads| {
        for load in loads {
            drop(app.update(load));
            drop(app.update(Message::RepositoryReconciliationDue));
        }
    });
    let complete = run(&|app, loads| {
        for load in loads {
            drop(app.update(load));
        }
        drop(app.reconcile_workspace_model());
    });

    // The comparison means something only if the loads had work to do.
    for (index, project) in workspace.projects.iter().enumerate() {
        let channel = coalesced
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == project.channel)
            .unwrap_or_else(|| panic!("project {index}'s registry channel is adopted"));
        assert_eq!(channel.cards, [project.channel_pane.0]);
        assert!(
            coalesced
                .durable
                .cards
                .iter()
                .any(|card| card.id == project.worktree_pane.0),
            "project {index}'s worktree pane is adopted"
        );
    }
    assert!(
        coalesced.published.iter().any(
            |edit| matches!(edit, ModelEdit::CreateChannel { id, .. } if *id == workspace.unshared)
        ),
        "the channel the registry never heard of is introduced"
    );

    for (name, other) in [("complete", &complete), ("per load", &per_load)] {
        assert_eq!(
            coalesced.durable, other.durable,
            "saved workspace vs {name}"
        );
        assert_eq!(coalesced.model, other.model, "registry model vs {name}");
        assert_eq!(
            coalesced.baseline, other.baseline,
            "projected views vs {name}"
        );
    }
    assert_eq!(coalesced.published, complete.published);
    // One pass per load re-sent the channel each pass until the registry
    // acknowledged it; one shared pass sends each edit once.
    let mut first_sent = Vec::new();
    for edit in per_load.published {
        if !first_sent.contains(&edit) {
            first_sent.push(edit);
        }
    }
    assert_eq!(coalesced.published, first_sent);
}

/// Opening a project's cards is the user waiting on that project, so its load
/// reconciles at once rather than joining a run of background loads.
#[test]
fn a_load_that_opens_cards_reconciles_at_once() {
    let workspace = RestoredWorkspace::new();
    let (mut app, mut loads) = workspace.app();
    let Message::RepositoryLoaded {
        open_cards,
        project_id,
        ..
    } = &mut loads[0]
    else {
        unreachable!()
    };
    *open_cards = true;
    let project = *project_id;

    drop(app.update(loads.remove(0)));

    assert!(app.pending_repository_reconciliation.is_empty());
    let channel = workspace.projects[0].channel;
    assert!(
        app.durable.channels.iter().any(|local| local.id == channel),
        "the opened project's registry channel is adopted by the load itself"
    );
    assert_eq!(app.sidebar_model.project(project).unwrap().id, project);
}
