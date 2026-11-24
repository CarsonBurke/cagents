use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Agent, Origin, Spec, Surface, Target},
    automations::{Definition, DirectoryIdentity, Schedule, SchedulePattern, SessionMode},
    model::AgentId,
    workspace_model::delta::Mirror,
};
use std::process::Command;

fn fixture(kind: ProjectKind) -> (Registry, tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let repository = root.path().canonicalize().unwrap().join("project");
    fs::create_dir(&repository).unwrap();
    let mut registry = Registry::at(&root.path().join("workspace.sqlite3")).unwrap();
    registry
        .apply_delta(&ModelEdit::OpenProject {
            repository: repository.clone(),
            name: "Project".into(),
            kind,
        })
        .unwrap();
    (registry, root, repository)
}

fn worktree(repository: &Path, name: &str) -> Scope {
    Scope::Worktree {
        repository: repository.to_owned(),
        path: repository.join(name),
    }
}

fn seed_edit(scope: &Scope, panes: Vec<Pane>, closed: Vec<CardId>) -> ModelEdit {
    ModelEdit::SeedView {
        scope: scope.clone(),
        panes,
        closed,
    }
}

fn spec(repository: &Path, directory: PathBuf, placement: Option<Scope>) -> Spec {
    Spec {
        provider: Provider::Claude,
        account: None,
        target: Target {
            repository: repository.into(),
            directory,
            placement,
        },
        surface: Surface::Chat,
        model: None,
        permission_mode: None,
        reasoning_effort: None,
    }
}

fn definition(agent: Spec) -> Definition {
    Definition {
        name: "Review daily".into(),
        prompt: "Review the fixture".into(),
        agent,
        schedule: Schedule {
            pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
            timezone: "America/Vancouver".into(),
        },
        session_mode: SessionMode::Fresh,
        precheck: None,
    }
}

#[test]
fn sweep_keeps_every_guard_including_disabled_automation_targets() {
    let (mut registry, root, repository) = fixture(ProjectKind::Folder);
    let channel = ChannelId::new();
    registry
        .apply_delta(&ModelEdit::CreateChannel {
            id: channel,
            repository: repository.clone(),
            title: "Empty channel".into(),
            title_source: Default::default(),
            checkout: None,
        })
        .unwrap();
    let populated = worktree(&repository, "has-pane");
    let retired = worktree(&repository, "retired-pane");
    let operation = worktree(&repository, "recent-operation");
    let existing_path = repository.with_file_name("existing-directory");
    let existing = Scope::Worktree {
        repository: repository.clone(),
        path: existing_path.clone(),
    };
    let agent = worktree(&repository, "agent-placement");
    let placement = Scope::Worktree {
        repository: repository.clone(),
        path: repository.clone(),
    };
    let directory = worktree(&repository, "automation-directory");
    let eligible = worktree(&repository, "eligible");
    fs::create_dir(existing_path).unwrap();
    for edit in [
        seed_edit(&Scope::Channel(channel), vec![], vec![]),
        seed_edit(&populated, vec![pane()], vec![]),
        seed_edit(&retired, vec![], vec![CardId::new()]),
        seed_edit(&operation, vec![], vec![]),
        seed_edit(&existing, vec![], vec![]),
        seed_edit(&agent, vec![], vec![]),
        seed_edit(&placement, vec![], vec![]),
        seed_edit(&directory, vec![], vec![]),
        seed_edit(&eligible, vec![], vec![]),
    ] {
        assert!(registry.apply_delta(&edit).unwrap().changed);
    }

    // A no-op still has an idempotency receipt without adding any pane rows.
    let operation_id = Uuid::new_v4();
    let no_op = ModelEdit::ChangeView {
        scope: operation.clone(),
        expected_revision: registry.view_revision(&operation).unwrap(),
        operation: operation_id,
        change: Change::Move {
            id: CardId::new(),
            before: None,
        },
    };
    assert!(!registry.apply_delta(&no_op).unwrap().changed);
    assert!(registry.view_operation_seen(operation_id).unwrap());

    // New agents start in channels. Model the retained placement an older
    // agent can still have after its worktree and pane are gone.
    let open_agent = Agent {
        id: AgentId::new(),
        name: None,
        origin: Origin::User,
        spec: spec(&repository, repository.clone(), Some(agent.clone())),
        placement: agent.clone(),
        session: None,
        console: None,
        created_ms: 1,
        closed: false,
    };
    registry
        .db
        .execute(
            "INSERT INTO agents(id, origin, record, closed) VALUES (?1, ?2, ?3, 0)",
            params![
                open_agent.id.to_string(),
                serde_json::to_string(&open_agent.origin).unwrap(),
                serde_json::to_string(&open_agent).unwrap(),
            ],
        )
        .unwrap();
    let identity = DirectoryIdentity {
        device: 1,
        inode: 1,
    };
    let by_placement = registry
        .create_automation(
            definition(spec(&repository, root.path().into(), Some(placement))),
            1,
            identity.clone(),
        )
        .unwrap();
    let by_directory = registry
        .create_automation(
            definition(spec(
                &repository,
                repository.join("automation-directory"),
                None,
            )),
            1,
            identity,
        )
        .unwrap();
    assert!(!by_placement.enabled && !by_directory.enabled);
    fs::rename(&repository, root.path().join("removed-project")).unwrap();

    let compactor = registry.execution_connection().unwrap();
    let mut retained = registry.model().unwrap().views;
    retained.retain(|view| view.scope != eligible);
    let (_, effects) = compactor.compact_worktree_views().unwrap().unwrap();
    assert_eq!(effects.views.len(), 1);
    assert_eq!(effects.views[0].scope, eligible);
    assert_eq!(registry.model().unwrap().views, retained);
    assert!(registry.view_operation_seen(operation_id).unwrap());

    registry
        .db
        .execute(
            "UPDATE view_operations SET applied_at_unix_ms = 0 WHERE operation = ?1",
            [operation_id.to_string()],
        )
        .unwrap();
    registry.close_agent(&Origin::User, open_agent.id).unwrap();
    registry
        .remove_automation(by_directory.id, by_directory.revision, 2)
        .unwrap();
    retained.retain(|view| view.scope != agent && view.scope != directory);
    let (_, effects) = compactor.compact_worktree_views().unwrap().unwrap();
    assert_eq!(effects.views.len(), 2);
    assert_eq!(registry.model().unwrap().views, retained);

    assert_eq!(compactor.collect_view_operations().unwrap(), 1);
    let (_, effects) = compactor.compact_worktree_views().unwrap().unwrap();
    assert_eq!(effects.views.len(), 1);
    assert_eq!(effects.views[0].scope, operation);
    assert!(registry.view_snapshot(&operation).unwrap().0.is_none());
}

fn git(repository: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "maintenance.auto=false",
        ])
        .args(args)
        .current_dir(repository)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn targeted_removal_preserves_registered_worktrees_and_publishes_the_later_deletion() {
    let (mut registry, root, repository) = fixture(ProjectKind::Git);
    git(&repository, &["init", "-q"]);
    git(
        &repository,
        &[
            "commit",
            "-q",
            "--no-gpg-sign",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    git(
        &repository,
        &["worktree", "add", "-q", "--detach", "linked"],
    );
    let scope = worktree(&repository, "linked");
    let other = worktree(&repository, "other-dead-scope");
    let path = repository.join("linked");
    registry
        .apply_delta(&seed_edit(&scope, vec![], vec![]))
        .unwrap();
    registry
        .apply_delta(&seed_edit(&other, vec![], vec![]))
        .unwrap();

    fs::rename(&path, root.path().join("parked")).unwrap();
    let before = registry.model().unwrap();
    let old_revision = registry.view_revision(&scope).unwrap();
    assert_ne!(old_revision, 0);
    let mut mirror = Mirror::for_model(&before);
    let compactor = registry.execution_connection().unwrap();
    assert!(
        compactor
            .compact_worktree_view(&repository, &path)
            .unwrap()
            .is_none()
    );
    assert_eq!(registry.model().unwrap(), before);

    git(&repository, &["worktree", "prune", "--expire", "now"]);
    let (sequence, removal) = compactor
        .compact_worktree_view(&repository, &path)
        .unwrap()
        .unwrap();
    assert_eq!(registry.view_revision(&scope).unwrap(), 0);
    assert!(registry.view_snapshot(&other).unwrap().0.is_some());
    assert_eq!(sequence, before.sequence + 1);
    assert_eq!(registry.view_snapshot(&scope).unwrap(), (None, sequence));
    assert_eq!(
        removal.views,
        vec![ViewRevision {
            scope: scope.clone(),
            before: Some(old_revision),
            after: None,
        }],
    );
    assert!(removal.closed.is_empty());
    let applied = mirror.apply(&ModelEdit::AgentsChanged, &removal);
    assert!(applied.unresolved.is_empty());
    assert_eq!(applied.changes.removed, vec![(scope.clone(), old_revision)]);
    assert_eq!(mirror.revision(&scope), None);

    let operation = Uuid::new_v4();
    let stale_edit = ModelEdit::ChangeView {
        scope: scope.clone(),
        expected_revision: old_revision,
        operation,
        change: Change::Insert {
            pane: pane(),
            before: None,
        },
    };
    assert_eq!(
        registry.apply_delta(&stale_edit).unwrap_err(),
        Registry::VIEW_REVISION_CONFLICT
    );
    assert_eq!(registry.view_snapshot(&scope).unwrap(), (None, sequence));

    let seed = seed_edit(&scope, vec![], vec![]);
    let recreated = registry.apply_delta(&seed).unwrap().receipt;
    let revision = registry.view_revision(&scope).unwrap();
    assert!(revision > old_revision);
    assert_eq!(
        recreated.effects.views,
        vec![ViewRevision {
            scope: scope.clone(),
            before: None,
            after: Some(revision),
        }]
    );
    assert_eq!(
        mirror.apply(&seed, &recreated.effects).unresolved,
        vec![scope.clone()]
    );
    let snapshot = registry.view_snapshot(&scope).unwrap();
    mirror.install(scope.clone(), snapshot.0.clone());
    mirror.apply(&ModelEdit::AgentsChanged, &removal);
    assert_eq!(mirror.revision(&scope), Some(revision));
    assert_eq!(
        registry.apply_delta(&stale_edit).unwrap_err(),
        Registry::VIEW_REVISION_CONFLICT
    );
    assert!(!registry.view_operation_seen(operation).unwrap());
    assert_eq!(registry.view_snapshot(&scope).unwrap(), snapshot);
}
