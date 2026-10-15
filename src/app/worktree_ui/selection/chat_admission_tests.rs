//! Actual selection workers and real filesystem leases; no initialized global window.
use super::*;
use crate::worktree_cleanup::fixture_protection;
use cosmic::Application as _;

fn app(filing: &Path, launch: &Path, runtime: &Path, channel: bool) -> (Cagents, Option<ChannelId>) {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let project = ProjectId::new();
    app.sidebar_model.projects = vec![SidebarProject::new(
        project, RepositoryId::new(), "chat admission".into(), filing.to_owned(),
        [filing, launch].into_iter().map(|path| SidebarWorktree::new(
            crate::model::WorktreeId::new(), path.to_owned(), None, false, false,
            None, None, None,
        )).collect(), Vec::new(),
    )];
    app.known_worktree_roots.extend([filing.to_owned(), launch.to_owned()]);
    let selected = channel.then(ChannelId::new);
    if let Some(id) = selected {
        Arc::make_mut(&mut app.durable).channels.push(crate::model::ProjectChannel {
            id, project_id: project, title: "filing channel".into(), ordinal: 0,
            shared: false, title_source: Default::default(), cards: Vec::new(), focused: None,
        });
        drop(app.select_channel_inner(id));
    } else {
        drop(app.select_worktree_inner(filing.to_owned(), false));
    }
    app.force_local_selection_with_counter_for_test(Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    app.force_selection_guard_runtime_for_test(runtime.to_owned());
    (app, selected)
}

fn prepared(task: Task<Message>) -> (u64, bool, Receipt) {
    let Message::WorktreeSelectionPrepared { generation, create, receipt } =
        crate::app::tests::task_app_message(task) else { panic!("actual preparation receipt required") };
    (generation, create, receipt)
}

fn exclusive_refused(runtime: &Path, path: &Path) {
    assert!(matches!(fixture_protection(runtime, path, true), Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn new_chat_same_scope_or_channel_launch_refusal_never_exposes_a_draft() {
    for channel in [false, true] {
        let runtime = tempfile::tempdir().unwrap();
        let filing = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let (mut app, selected) = app(filing.path(), launch.path(), runtime.path(), channel);
        let target = if channel { launch.path() } else { filing.path() };
        let refusal = fixture_protection(runtime.path(), target, true).unwrap();
        let cards = app.durable.cards.len();
        let order = app.workspace.order().collect::<Vec<_>>();
        let task = app.new_native_chat(crate::agent::Provider::Claude, Some(target.to_owned()));
        assert!(app.worktree_selection.pending);
        assert_eq!(app.durable.cards.len(), cards);
        let (generation, create, receipt) = prepared(task);
        assert!(receipt.0.lock().unwrap().as_ref().unwrap().4.is_err());
        drop(app.update(Message::WorktreeSelectionPrepared { generation, create, receipt }));
        assert!(!app.worktree_selection.pending);
        assert_eq!(app.durable.cards.len(), cards);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
        assert_eq!(app.active_workspace.as_deref(), Some(filing.path()));
        assert_eq!(app.durable.active_channel, selected);
        assert!(app.status.is_some());
        if channel { drop(fixture_protection(runtime.path(), filing.path(), true).unwrap()); }
        drop(refusal);
    }
}

#[test]
fn channel_filing_and_distinct_launch_are_both_guarded_before_the_first_draft() {
    let runtime = tempfile::tempdir().unwrap();
    let filing = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let (mut app, channel) = app(filing.path(), launch.path(), runtime.path(), true);
    let cards = app.durable.cards.len();
    let task = app.new_native_chat(crate::agent::Provider::Claude, Some(launch.path().to_owned()));
    assert_eq!(app.durable.cards.len(), cards);
    let (generation, create, receipt) = prepared(task);
    assert_eq!(app.durable.cards.len(), cards);
    exclusive_refused(runtime.path(), filing.path());
    exclusive_refused(runtime.path(), launch.path());
    drop(app.update(Message::WorktreeSelectionPrepared { generation, create, receipt }));
    assert_eq!(app.durable.cards.len(), cards + 1);
    assert_eq!(app.active_workspace.as_deref(), Some(filing.path()));
    assert_eq!(app.durable.active_channel, channel);
    let inserted = app.durable.cards.iter().find(|card| matches!(&card.kind, CardKind::Chat(session) if session.launch.recipe.working_directory==launch.path())).unwrap();
    assert!(app.durable.channels.iter().find(|row| Some(row.id)==channel).unwrap().cards.contains(&inserted.id));
    exclusive_refused(runtime.path(), filing.path());
    exclusive_refused(runtime.path(), launch.path());
    drop(app);
    drop(fixture_protection(runtime.path(), filing.path(), true).unwrap());
    drop(fixture_protection(runtime.path(), launch.path(), true).unwrap());
}

#[test]
fn native_thread_intake_commits_once_without_a_second_selection_or_wrong_channel() {
    let runtime = tempfile::tempdir().unwrap();
    let filing = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let (mut app, selected_channel) = app(filing.path(), launch.path(), runtime.path(), true);
    let mut source = crate::app::tests::terminal_card_state();
    source.kind = CardKind::Chat(crate::chat::fresh_session_deferred_policy(
        crate::agent::Provider::Claude, TerminalId::new(), filing.path().to_owned(),
        &crate::account::Account::system(crate::agent::Provider::Claude),
    ).unwrap());
    source.residency = Residency::Hot;
    let source_id = source.id;
    let mut destination = crate::app::tests::terminal_card_state();
    destination.kind = CardKind::Chat(crate::chat::fresh_session_deferred_policy(
        crate::agent::Provider::Codex, TerminalId::new(), launch.path().to_owned(),
        &crate::account::Account::system(crate::agent::Provider::Codex),
    ).unwrap());
    destination.residency = Residency::Hot;
    let destination_id = destination.id;
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards.extend([source, destination]);
    durable.card_scopes.insert(destination_id, launch.path().to_owned());
    let channel = durable.channels.iter_mut().find(|row| Some(row.id)==selected_channel).unwrap();
    channel.cards.push(source_id);
    channel.focused = Some(source_id);
    drop(app.select_channel_inner(selected_channel.unwrap()));
    assert_eq!(app.workspace.focused(), Some(source_id));
    let cards = app.durable.cards.len();
    let task = app.select_worktree_then(launch.path().to_owned(), false, Intent::NewNativeThread);
    let (generation, create, receipt) = prepared(task);
    assert!(matches!(&receipt.0.lock().unwrap().as_ref().unwrap().3, Intent::NewNativeThread));
    assert_eq!(app.durable.cards.len(), cards);
    drop(app.update(Message::WorktreeSelectionPrepared { generation, create, receipt }));
    assert_eq!(app.worktree_selection.generation, generation);
    assert!(!app.worktree_selection.pending);
    assert_eq!(app.durable.cards.len(), cards + 1);
    assert_eq!(app.durable.active_channel, None);
    assert_eq!(app.active_workspace.as_deref(), Some(launch.path()));
    let inserted = app.durable.cards.iter().find(|card| card.id!=destination_id && matches!(&card.kind, CardKind::Chat(session) if session.launch.recipe.working_directory==launch.path())).unwrap();
    let CardKind::Chat(session) = &inserted.kind else { unreachable!() };
    assert_eq!(session.launch.provider, crate::agent::Provider::Codex, "use the destination focus preference after committing scope");
    exclusive_refused(runtime.path(), launch.path());
}

#[cfg(unix)]
#[test]
fn filing_alias_retarget_during_launch_protection_rejects_both_unexposed_guards() {
    let runtime = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let launch = directory.path().join("launch");
    for path in [&first, &second, &launch] { std::fs::create_dir(path).unwrap(); }
    let alias = directory.path().join("filing");
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    let result = prepare_chat_target(alias.clone(), launch.clone(), Vec::new(), None,
        &owner_watch::Monitor::default(), Path::canonicalize, |path| {
            let guard = fixture_protection(runtime.path(), path, false)?;
            if path==launch {
                std::fs::remove_file(&alias)?;
                std::os::unix::fs::symlink(&second, &alias)?;
            }
            Ok(Some(guard))
        });
    assert!(result.is_err());
    for path in [&first, &second, &launch] { drop(fixture_protection(runtime.path(), path, true).unwrap()); }
}

#[test]
fn blocked_launch_guard_keeps_old_strip_and_no_draft_until_both_leases_land() {
    let runtime = tempfile::tempdir().unwrap();
    let filing = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let (mut app, channel) = app(filing.path(), launch.path(), runtime.path(), true);
    let before = app.durable.cards.len();
    let after = app.native_thread_selection_intent(launch.path().to_owned()).unwrap();
    let intent = Intent::Channel { id: channel.unwrap(), path: filing.path().to_owned(), after: Box::new(after) };
    drop(app.cancel_worktree_selection());
    app.worktree_selection.pending = true;
    app.worktree_selection.additional_paths.insert(launch.path().to_owned());
    let generation = app.worktree_selection.generation;
    let epoch = app.worktree_owner_paths.invalidate_watched();
    let watch = app.worktree_owner_paths.watch.clone();
    let worker_runtime = runtime.path().to_owned();
    let worker_launch = launch.path().to_owned();
    let (entered, waiting) = crossbeam_channel::bounded(1);
    let (release, released) = crossbeam_channel::bounded(1);
    let task = app.prepare_worktree_selection_receipt(
        generation, filing.path().to_owned(), false, intent, epoch,
        move |filing| prepare_chat_target(filing, worker_launch.clone(), Vec::new(), None,
            &watch, Path::canonicalize, |path| {
                if path==worker_launch { entered.send(()).unwrap(); released.recv().unwrap(); }
                fixture_protection(&worker_runtime, path, false).map(Some)
            }),
    );
    let worker = std::thread::spawn(move || prepared(task));
    waiting.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(app.durable.cards.len(), before);
    assert_eq!(app.durable.active_channel, channel);
    assert_eq!(app.active_workspace.as_deref(), Some(filing.path()));
    exclusive_refused(runtime.path(), filing.path());
    drop(fixture_protection(runtime.path(), launch.path(), true).unwrap());
    drop(app.update(Message::SettingsSearchChanged("input while launch guard waits".into())));
    assert_eq!(app.settings_search, "input while launch guard waits");
    assert_eq!(app.durable.cards.len(), before);
    release.send(()).unwrap();
    let (generation, create, receipt) = worker.join().unwrap();
    exclusive_refused(runtime.path(), filing.path());
    exclusive_refused(runtime.path(), launch.path());
    drop(app.update(Message::WorktreeSelectionPrepared { generation, create, receipt }));
    assert_eq!(app.durable.cards.len(), before+1);
    assert_eq!(app.durable.active_channel, channel);
    assert_eq!(app.active_workspace.as_deref(), Some(filing.path()));
}
