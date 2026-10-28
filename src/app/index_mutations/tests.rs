use super::*;
use crate::repository::{RepoPath, test_fixture::isolated_fixture_case};

fn selection(name: &str) -> PathSelection {
    PathSelection::new(RepoPath::parse(name).unwrap())
}

fn fixture() -> (Cagents, ActiveWorktreeKey) {
    let mut app = super::super::tests::context_inspector_test_app(false);
    app.sidebar_model = sidebar::tests::panel_model();
    app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
    let key = app.sidebar_state.active_worktree().unwrap().clone();
    app.active_workspace = Some(key.canonical_path.clone());
    (app, key)
}

fn stage(app: &mut Cagents, key: &ActiveWorktreeKey, selected: PathSelection) -> Task<Message> {
    app.update(Message::Sidebar(SidebarAction::Stage {
        key: key.clone(),
        selection: vec![selected],
    }))
}

#[test]
fn adjacent_batches_deduplicate_complete_rename_selections_and_keep_kind_order() {
    let mut pending = Pending::default();
    let mut rename = selection("new");
    rename.old_path = Some(RepoPath::parse("old").unwrap());
    let mut other_source = rename.clone();
    other_source.old_path = Some(RepoPath::parse("other").unwrap());
    pending.push(Kind::Stage, vec![rename.clone()]);
    pending.push(Kind::Stage, vec![rename.clone(), other_source.clone()]);
    pending.push(Kind::Unstage, vec![selection("new")]);
    pending.push(Kind::Stage, vec![selection("last")]);
    assert_eq!(pending.len(), 4);
    let first = pending.pop().unwrap();
    assert_eq!(first.kind, Kind::Stage);
    assert_eq!(first.selection, [rename, other_source]);
    assert_eq!(pending.pop().unwrap().kind, Kind::Unstage);
    assert_eq!(pending.pop().unwrap().selection, [selection("last")]);
    assert_eq!(pending.len(), 0);
}

#[test]
fn accepted_queue_survives_navigation_and_stale_completions_cannot_release_its_owner() {
    let (mut app, key) = fixture();
    let original = app.begin_mutation(&key, MutationKind::Push, None).unwrap();
    drop(stage(&mut app, &key, selection("queued")));
    let sibling = app
        .sidebar_model
        .worktree_at(Path::new("/repo/fix-sidebar"))
        .unwrap();
    app.active_workspace = Some(sibling.canonical_path.clone());
    drop(app.rebind_source_control());
    let next = app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: original,
        report: None,
        committed: false,
    });
    let current = &app.source_control_mutations[&key.canonical_path];
    assert_eq!(current.key, key);
    assert_eq!(current.kind, MutationKind::Stage);
    assert_ne!(current.id, original);
    let next_id = current.id;
    assert_eq!(
        app.sidebar_model.source_control.as_ref().unwrap().key,
        sibling
    );
    assert!(!app.sidebar_model.source_control.as_ref().unwrap().busy());
    let mut replacement = key.clone();
    replacement.project_id = crate::model::ProjectId::new();
    drop(app.update(Message::SourceControlMutated {
        key: replacement,
        operation: next_id,
        report: Some("foreign".into()),
        committed: false,
    }));
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: original,
        report: Some("late".into()),
        committed: false,
    }));
    assert_eq!(
        app.source_control_mutations[&key.canonical_path].id,
        next_id
    );
    drop(next);
}

#[test]
fn failure_stops_queued_work_with_a_visible_retry_report_and_one_reconciliation() {
    let (mut app, key) = fixture();
    let original = app.begin_mutation(&key, MutationKind::Stage, None).unwrap();
    let before = app.refresh_ui.source_generation;
    let rows = app
        .sidebar_model
        .source_control
        .as_ref()
        .unwrap()
        .groups()
        .to_vec();
    drop(stage(&mut app, &key, selection("next")));
    drop(stage(&mut app, &key, selection("another")));
    assert_eq!(
        app.sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .queued_index_changes,
        2
    );
    drop(app.reload_source_control(&key));
    assert_eq!(app.refresh_ui.source_generation, before);
    drop(app.update(Message::SourceControlMutated {
        key: key.clone(),
        operation: original,
        report: Some("index.lock is held".into()),
        committed: false,
    }));
    assert!(app.source_control_mutations.is_empty());
    let source = app.sidebar_model.source_control.as_ref().unwrap();
    assert!(!source.busy());
    assert_eq!(source.queued_index_changes, 0);
    assert_eq!(source.groups(), rows);
    let error = source.error.as_deref().unwrap();
    assert!(error.contains("index.lock is held"));
    assert!(error.contains("2 queued file actions were not run"));
    assert_eq!(app.refresh_ui.source_generation, before.wrapping_add(1));
    drop(stage(&mut app, &key, selection("retry")));
    assert!(
        app.source_control_mutations
            .contains_key(&key.canonical_path)
    );
    assert!(
        app.sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .error
            .is_none()
    );
}

#[test]
fn removed_or_replaced_worktree_never_receives_the_original_queued_actions() {
    let (mut app, key) = fixture();
    let original = app.begin_mutation(&key, MutationKind::Stage, None).unwrap();
    drop(stage(&mut app, &key, selection("original")));
    app.sidebar_model.projects[0].worktrees[0].id =
        crate::model::WorktreeId::from_uuid(uuid::Uuid::new_v4());
    drop(app.finish_source_control_mutation(key, original, None, false));
    assert!(app.source_control_mutations.is_empty());
    let error = app
        .sidebar_model
        .source_control
        .as_ref()
        .unwrap()
        .error
        .as_deref()
        .unwrap();
    assert!(error.contains("original worktree is no longer available"));
    assert!(error.contains("1 queued file actions were not run"));
}

#[test]
fn same_path_new_worktree_waits_for_old_owner_then_becomes_usable() {
    let (mut app, key) = fixture();
    let original = app.begin_mutation(&key, MutationKind::Stage, None).unwrap();
    drop(stage(&mut app, &key, selection("original")));
    app.sidebar_model.projects[0].worktrees[0].id =
        crate::model::WorktreeId::from_uuid(uuid::Uuid::new_v4());
    drop(app.rebind_source_control());
    let source = app.sidebar_model.source_control.as_mut().unwrap();
    let replacement = source.key.clone();
    source.data_state = sidebar::SidebarDataState::Ready;
    assert_ne!(replacement, key);
    assert!(source.busy());
    assert!(source.index_queue_blocked);
    drop(stage(&mut app, &replacement, selection("replacement")));
    assert_eq!(
        app.source_control_mutations[&key.canonical_path]
            .pending
            .len(),
        1
    );
    let before = app.refresh_ui.source_generation;
    drop(app.update(Message::SourceControlMutated {
        key,
        operation: original,
        report: None,
        committed: false,
    }));
    assert!(app.source_control_mutations.is_empty());
    let source = app.sidebar_model.source_control.as_ref().unwrap();
    assert_eq!(source.key, replacement);
    assert!(!source.busy());
    assert!(!source.index_queue_blocked);
    assert!(
        source.error.is_none(),
        "old worktree failures do not belong to its replacement"
    );
    assert_eq!(app.refresh_ui.source_generation, before.wrapping_add(1));
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = std::process::Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(name);
        }
    }
    let output = command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "init.templateDir=",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn rapid_stage_clicks_use_one_following_batch_and_reconcile_after_real_git_settles() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(rapid_stage_clicks_use_one_following_batch_and_reconcile_after_real_git_settles)
    )) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--initial-branch=main"]);
    for index in 0..10 {
        std::fs::write(root.path().join(format!("file{index}")), b"original\n").unwrap();
    }
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "original"]);
    for index in 0..10 {
        std::fs::write(root.path().join(format!("file{index}")), b"changed\n").unwrap();
    }
    let (mut app, mut key) = fixture();
    key.canonical_path = root.path().to_path_buf();
    app.sidebar_model.projects[0].worktrees[0].path = key.canonical_path.clone();
    app.sidebar_model.source_control.as_mut().unwrap().key = key.clone();
    app.active_workspace = Some(key.canonical_path.clone());
    app.sidebar_state.sync_active_worktree(
        &app.sidebar_model,
        &SidebarFocus::Repository {
            worktree: key.canonical_path.clone(),
        },
    );
    let before = app.refresh_ui.source_generation;
    let first = stage(&mut app, &key, selection("file0"));
    for index in 1..10 {
        drop(stage(&mut app, &key, selection(&format!("file{index}"))));
    }
    drop(stage(&mut app, &key, selection("file1")));
    assert_eq!(
        app.source_control_mutations[&key.canonical_path]
            .pending
            .batches
            .len(),
        1
    );
    assert_eq!(
        app.source_control_mutations[&key.canonical_path]
            .pending
            .len(),
        9
    );
    assert_eq!(app.refresh_ui.source_generation, before);
    let first_done = super::super::tests::task_app_message(first);
    assert!(matches!(
        &first_done,
        Message::SourceControlMutated { report: None, .. }
    ));
    assert_eq!(
        git(root.path(), &["diff", "--cached", "--name-only"]),
        b"file0\n"
    );
    let next = app.update(first_done);
    assert!(app.sidebar_model.source_control.as_ref().unwrap().busy());
    assert_eq!(app.refresh_ui.source_generation, before);
    let all_done = super::super::tests::task_app_message(next);
    assert!(matches!(
        &all_done,
        Message::SourceControlMutated { report: None, .. }
    ));
    drop(app.update(all_done));
    assert!(app.source_control_mutations.is_empty());
    assert_eq!(app.refresh_ui.source_generation, before.wrapping_add(1));
    let names = String::from_utf8(git(root.path(), &["diff", "--cached", "--name-only"])).unwrap();
    assert_eq!(
        names.lines().collect::<Vec<_>>(),
        (0..10)
            .map(|index| format!("file{index}"))
            .collect::<Vec<_>>()
    );
    assert!(git(root.path(), &["diff", "--name-only"]).is_empty());
}

#[test]
fn queued_stage_unstage_stage_changes_only_its_original_worktree_after_navigation() {
    if isolated_fixture_case(concat!(
        module_path!(),
        "::",
        stringify!(queued_stage_unstage_stage_changes_only_its_original_worktree_after_navigation)
    )) {
        return;
    }
    let original = tempfile::tempdir().unwrap();
    let sibling = tempfile::tempdir().unwrap();
    for root in [original.path(), sibling.path()] {
        git(root, &["init", "--initial-branch=main"]);
        std::fs::write(root.join("file"), b"original\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "original"]);
        std::fs::write(root.join("file"), b"local draft\n").unwrap();
    }
    let (mut app, mut key) = fixture();
    key.canonical_path = original.path().to_path_buf();
    app.sidebar_model.projects[0].worktrees[0].path = key.canonical_path.clone();
    app.sidebar_model.projects[0].worktrees[1].path = sibling.path().to_path_buf();
    app.sidebar_model.source_control.as_mut().unwrap().key = key.clone();
    app.active_workspace = Some(key.canonical_path.clone());
    app.sidebar_state.sync_active_worktree(
        &app.sidebar_model,
        &SidebarFocus::Repository {
            worktree: key.canonical_path.clone(),
        },
    );
    let first = stage(&mut app, &key, selection("file"));
    drop(app.update(Message::Sidebar(SidebarAction::Unstage {
        key: key.clone(),
        selection: vec![selection("file")],
    })));
    drop(stage(&mut app, &key, selection("file")));
    app.active_workspace = Some(sibling.path().to_path_buf());
    drop(app.rebind_source_control());
    let generation = app.refresh_ui.source_generation;
    let done = super::super::tests::task_app_message(first);
    assert!(matches!(
        &done,
        Message::SourceControlMutated { report: None, .. }
    ));
    assert_eq!(
        git(original.path(), &["diff", "--cached", "--name-only"]),
        b"file\n"
    );
    let unstage = app.update(done);
    let done = super::super::tests::task_app_message(unstage);
    assert!(matches!(
        &done,
        Message::SourceControlMutated { report: None, .. }
    ));
    assert!(git(original.path(), &["diff", "--cached", "--name-only"]).is_empty());
    let last = app.update(done);
    let done = super::super::tests::task_app_message(last);
    assert!(matches!(
        &done,
        Message::SourceControlMutated { report: None, .. }
    ));
    assert_eq!(
        git(original.path(), &["diff", "--cached", "--name-only"]),
        b"file\n"
    );
    drop(app.update(done));
    assert!(app.source_control_mutations.is_empty());
    assert_eq!(
        app.sidebar_model
            .source_control
            .as_ref()
            .unwrap()
            .key
            .canonical_path,
        sibling.path()
    );
    assert!(!app.sidebar_model.source_control.as_ref().unwrap().busy());
    assert_eq!(app.refresh_ui.source_generation, generation);
    assert!(git(sibling.path(), &["diff", "--cached", "--name-only"]).is_empty());
    assert_eq!(
        std::fs::read(sibling.path().join("file")).unwrap(),
        b"local draft\n"
    );
}
