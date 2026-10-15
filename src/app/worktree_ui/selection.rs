//! Navigation commits only after its target's filesystem lease is held.
use super::*;
use std::sync::Mutex;

#[derive(Default)]
pub(in crate::app) struct State {
    generation: u64,
    pub(in crate::app) pending: bool,
    pub(in crate::app) initialize_runtimes: bool,
    /// Superseded workers can still own a not-yet-delivered directory lease.
    pub(super) inflight: HashSet<u64>,
}
impl State {
    pub(in crate::app) fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = false;
    }
}

/// Work which depends on the destination strip must travel with navigation.
/// It must never run in a batch beside the asynchronous preparation task.
pub(in crate::app) enum Intent {
    Navigate,
    Terminal(TerminalSession),
    TerminalWithLifecycle { session: TerminalSession, committed: Box<Message>, rejected: Box<Message> },
    NewTerminal(PathBuf),
    NewNativeThread,
    Chat { session: chat::Session, beside: Option<CardId>, ephemeral: bool },
    Resume { entry: crate::persistence::SavedConversation, replace: Option<CardId> },
    Focus { card: CardId, activate: bool },
    Agent(crate::agent_execution::Agent),
    Delete(project_ui::Target),
}

struct Prepared {
    path: PathBuf,
    canonical: Vec<(PathBuf, PathBuf)>,
    guard: Option<crate::worktree_cleanup::Protection>,
}

#[derive(Clone)]
pub(in crate::app) struct Receipt(Arc<Mutex<Option<(PathBuf, u64, HashSet<PathBuf>, Intent, Result<Prepared, String>)>>>);
impl std::fmt::Debug for Receipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedWorktreeSelection")
    }
}

/// Production path with injectable filesystem operations for race regressions.
fn prepare_target(
    path: PathBuf, roots: Vec<(PathBuf,Option<PathBuf>,bool)>, cached: Option<PathBuf>,
    watch: &owner_watch::Monitor,
    mut canonicalize: impl FnMut(&Path) -> std::io::Result<PathBuf>,
    mut protect: impl FnMut(&Path) -> std::io::Result<Option<crate::worktree_cleanup::Protection>>,
) -> Result<(PathBuf,Vec<(PathBuf,PathBuf)>,Option<crate::worktree_cleanup::Protection>),String> {
    let lexical = std::iter::once((path.clone(),path.clone()))
        .chain(roots.iter().map(|(path,_,_)| (path.clone(),path.clone()))).collect::<Vec<_>>();
    watch.register(&lexical);
    let resolve = |canonicalize: &mut dyn FnMut(&Path)->std::io::Result<PathBuf>, use_cached: bool| {
        let canonical = match cached.as_ref().filter(|_|use_cached) {Some(value)=>value.clone(),None=>canonicalize(&path)?};
        let mut observed = HashMap::from([(path.clone(),canonical.clone())]);
        let mut resolved = vec![(path.clone(),canonical.clone())];
        let mut selected = (canonical.clone(),0usize,false);
        for (lexical,cached,visible) in &roots {
            let physical = observed.get(lexical).cloned().unwrap_or_else(|| {
                let value = cached.as_ref().filter(|_|use_cached).cloned()
                    .unwrap_or_else(|| canonicalize(lexical).unwrap_or_else(|_|lexical.clone()));
                observed.insert(lexical.clone(),value.clone());
                value
            });
            let depth = physical.components().count();
            if canonical.starts_with(&physical) && (depth,*visible) > (selected.1,selected.2) {
                selected = (lexical.clone(),depth,*visible);
            }
            resolved.push((lexical.clone(),physical));
        }
        let physical = resolved.iter().find(|(lexical,_)|lexical==&selected.0)
            .map_or_else(||selected.0.clone(),|(_,physical)|physical.clone());
        if !resolved.iter().any(|(lexical,_)|lexical==&selected.0) {
            resolved.push((selected.0.clone(),physical.clone()));
        }
        Ok::<_,std::io::Error>((selected.0,physical,resolved))
    };
    let first = resolve(&mut canonicalize,true).map_err(|e|e.to_string())?;
    watch.register(&first.2);
    // Cached ownership chooses a candidate; it cannot authorize a new lease.
    // Real reads fence the first-watch race and retargets whose notify callback
    // has not run yet. This work runs only for navigation, on the blocking pool.
    let validated = resolve(&mut canonicalize,false).map_err(|e|e.to_string())?;
    watch.register(&validated.2);
    let guard = protect(&validated.1).map_err(|e|e.to_string())?;
    let final_paths = resolve(&mut canonicalize,false).map_err(|e|e.to_string())?;
    if final_paths != validated { return Err("worktree alias changed while acquiring its protection; select it again".into()); }
    Ok((validated.0,validated.2,guard))
}

impl Cagents {
    pub(in crate::app) fn select_worktree_then(&mut self, path: PathBuf, create: bool, intent: Intent) -> Task<Message> {
        self.worktree_selection.cancel();
        // Headless fixture windows own no filesystem leases. Keep their
        // existing synchronous navigation semantics; production takes the
        // exact same commit path after off-thread preparation.
        if !crate::instance::window_is_initialized() || crate::device::is_remote() {
            let scope = self.select_worktree_inner(path, create);
            let after = self.finish_worktree_selection(intent);
            return Task::batch([scope, after, self.resolve_worktree_owner_paths()]);
        }
        let epoch = self.worktree_owner_paths.invalidate_watched();
        self.refresh_worktree_owners(Some(&path));
        let generation = self.worktree_selection.generation;
        self.worktree_selection.pending = true;
        let roots = self.known_worktree_roots.iter().map(|root| (
            root.clone(), self.worktree_owner_paths.cached.get(root).cloned(),
            self.sidebar_model.worktree_at(root).is_some(),
        )).collect::<Vec<_>>();
        let cached = self.worktree_owner_paths.cached.get(&path).cloned();
        let watch = self.worktree_owner_paths.watch.clone();
        self.prepare_worktree_selection(generation, path, create, intent, epoch, move |path| {
            prepare_target(path, roots, cached, &watch, |path| path.canonicalize(), |path| {
                crate::worktree_cleanup::protect(path)?.ok_or_else(|| std::io::Error::other("local worktree protection became unavailable")).map(Some)
            })
        })
    }

    fn prepare_worktree_selection(
        &mut self, generation: u64, path: PathBuf, create: bool, intent: Intent, epoch: u64,
        prepare: impl FnOnce(PathBuf) -> Result<(PathBuf, Vec<(PathBuf,PathBuf)>, Option<crate::worktree_cleanup::Protection>),String> + Send + 'static,
    ) -> Task<Message> {
        self.worktree_selection.inflight.insert(generation);
        let roots = self.known_worktree_roots.clone();
        cosmic::task::future(async move {
            let requested = path.clone();
            let result = tokio::task::spawn_blocking(move || prepare(path)).await
                .map_err(|error| error.to_string()).and_then(|value| value)
                .map(|(path,canonical,guard)| Prepared {path,canonical,guard});
            Message::WorktreeSelectionPrepared { generation, create,
                receipt: Receipt(Arc::new(Mutex::new(Some((requested,epoch,roots,intent,result))))) }
        })
    }

    pub(in crate::app) fn worktree_selection_prepared(&mut self, generation: u64, create: bool, receipt: Receipt) -> Task<Message> {
        let task = self.apply_worktree_selection_prepared(generation, create, receipt);
        // The consumed receipt and every uninstalled guard have dropped before
        // a deletion continuation can attempt its exclusive directory lease.
        self.worktree_selection.inflight.remove(&generation);
        let releases = self.finish_worktree_protection_releases(None);
        Task::batch([task, releases])
    }

    fn apply_worktree_selection_prepared(&mut self, generation: u64, create: bool, receipt: Receipt) -> Task<Message> {
        let Some((requested,epoch,roots,intent,result)) = receipt.0.lock().unwrap().take() else { return Task::none(); };
        if generation != self.worktree_selection.generation || !self.worktree_selection.pending {
            // Dropping an obsolete receipt releases its unexposed lease.
            return self.abandon_worktree_selection(intent);
        }
        self.worktree_selection.pending = false;
        self.refresh_worktree_owners(Some(&requested));
        if roots != self.known_worktree_roots || epoch != self.worktree_owner_paths.watch.epoch() {
            return self.select_worktree_then(requested,create,intent);
        }
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                let rejected = self.abandon_worktree_selection(intent);
                self.status = Some(TransientNotice::warning(format!("Cannot open this worktree yet: {error}")));
                return rejected;
            }
        };
        if self.refuse_in_deleting_worktree(&prepared.path) {
            return self.abandon_worktree_selection(intent);
        }
        self.worktree_protections.install_prepared(prepared.guard);
        self.worktree_protection_sync.invalidate();
        self.worktree_owner_paths.install_prepared(prepared.canonical);
        let selected = self.select_worktree_inner(prepared.path, create);
        let initialization = self.initialize_prepared_scope_runtimes();
        let after = self.finish_worktree_selection(intent);
        Task::batch([selected,initialization,after,self.resolve_worktree_owner_paths()])
    }

    pub(in crate::app) fn initialize_prepared_scope_runtimes(&mut self) -> Task<Message> {
        if !self.worktree_selection.initialize_runtimes || self.worktree_selection.pending { return Task::none(); }
        self.worktree_selection.initialize_runtimes = false;
        let initialize = self.initialize_runtimes();
        Task::batch([initialize,self.ensure_default_pane()])
    }

    fn abandon_worktree_selection(&mut self, intent: Intent) -> Task<Message> {
        match intent {
            Intent::Delete(target) => {self.cancel_prepared_worktree_deletion(target); Task::none()},
            Intent::TerminalWithLifecycle {rejected,..} => self.update(*rejected),
            _ => Task::none(),
        }
    }

    fn finish_worktree_selection(&mut self, intent: Intent) -> Task<Message> {
        match intent {
            Intent::Navigate => Task::none(),
            Intent::Terminal(session) => self.insert_terminal_card(session,None),
            Intent::TerminalWithLifecycle {session,committed,rejected} => {
                if self.refuse_in_deleting_worktree(&session.launch.working_directory) {
                    return self.update(*rejected);
                }
                let insert = self.insert_terminal_card(session,None);
                // The scope lease and inserted card are installed before the
                // caller can release its preparation path/guard or row.
                let settled = self.update(*committed);
                Task::batch([insert,settled])
            }
            Intent::NewTerminal(path) => self.add_terminal(path),
            Intent::NewNativeThread => self.active_workspace.clone().map_or_else(Task::none,
                |directory|self.new_native_thread_at(directory)),
            Intent::Chat {session,beside,ephemeral} => self.insert_chat_card_with_lifetime(session,beside,ephemeral,None),
            Intent::Resume {entry,replace} => self.commit_resumed_conversation(entry,replace),
            Intent::Focus {card,activate} => {
                self.set_terminal_review_owner(None);
                let focus = self.focus_workspace_card(card);
                if activate {Task::batch([focus,self.activate_main_window(None)])} else {focus}
            }
            Intent::Agent(agent) => {
                let adopted = self.adopt_execution_agent(&agent).unwrap_or_else(Task::none);
                self.set_terminal_review_owner(None);
                let focus = self.focus_workspace_card(CardId::from_uuid(agent.id.as_uuid()));
                Task::batch([adopted,focus])
            }
            Intent::Delete(target) => self.run_worktree_deletion(target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::Application as _;

    #[test]
    fn delayed_preparation_keeps_old_scope_and_input_responsive_then_discards_stale_intent() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.select_worktree("/old".into(), false));
        app.worktree_selection.cancel();
        let generation = app.worktree_selection.generation;
        app.worktree_selection.pending = true;
        let (entered, entered_rx) = crossbeam_channel::bounded(1);
        let (release, release_rx) = crossbeam_channel::bounded(1);
        let card = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(session) = card.kind else { unreachable!() };
        let before = app.durable.cards.len();
        let task = app.prepare_worktree_selection(generation, "/slow".into(), false, Intent::Terminal(session), app.worktree_owner_paths.watch.epoch(), move |path| {
            entered.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok((path, Vec::new(), None))
        });
        let worker = std::thread::spawn(move || crate::app::tests::task_app_message(task));
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let input_started = Instant::now();
        drop(app.update(Message::SettingsSearchChanged("responsive while acquiring lease".into())));
        eprintln!("blocked_selection_input_elapsed_us={}", input_started.elapsed().as_micros());
        assert_eq!(app.settings_search,"responsive while acquiring lease");
        assert_eq!(app.active_workspace.as_deref(), Some(Path::new("/old")));
        assert_eq!(app.durable.cards.len(), before);
        drop(app.select_worktree("/newer".into(),false));
        release.send(()).unwrap();
        let Message::WorktreeSelectionPrepared {generation,create,receipt} = worker.join().unwrap() else {panic!("unexpected selection reply")};
        drop(app.worktree_selection_prepared(generation,create,receipt.clone()));
        drop(app.worktree_selection_prepared(generation,create,receipt));
        assert_eq!(app.active_workspace.as_deref(),Some(Path::new("/newer")));
        assert_eq!(app.durable.cards.len(),before);
    }

    #[test]
    fn blocked_production_preparation_keeps_real_old_lease_then_installs_new_lease_before_exposure() {
        use crate::worktree_cleanup::fixture_protection;
        let runtime = tempfile::tempdir().unwrap();
        let old = tempfile::tempdir().unwrap();
        let next = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.known_worktree_roots.extend([old.path().to_owned(),next.path().to_owned()]);
        drop(app.select_worktree_inner(old.path().to_owned(),false));
        app.worktree_protections.install_prepared(Some(fixture_protection(runtime.path(),old.path(),false).unwrap()));
        app.worktree_selection.cancel();
        app.worktree_selection.pending = true;
        let generation = app.worktree_selection.generation;
        let epoch = app.worktree_owner_paths.invalidate_watched();
        let monitor = app.worktree_owner_paths.watch.clone();
        let roots = vec![(old.path().to_owned(),None,true),(next.path().to_owned(),None,true)];
        let worker_runtime = runtime.path().to_owned();
        let (entered,entered_rx) = crossbeam_channel::bounded(1);
        let (release,release_rx) = crossbeam_channel::bounded(1);
        let task = app.prepare_worktree_selection(generation,next.path().to_owned(),false,Intent::Navigate,epoch,move |path| {
            prepare_target(path,roots,None,&monitor,|path|path.canonicalize(),|physical| {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                fixture_protection(&worker_runtime,physical,false).map(Some)
            })
        });
        let worker = std::thread::spawn(move || crate::app::tests::task_app_message(task));
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let input_started = Instant::now();
        drop(app.update(Message::SettingsSearchChanged("typing before lease acquisition".into())));
        eprintln!("blocked_real_selection_lease_input_elapsed_us={}",input_started.elapsed().as_micros());
        assert_eq!(app.settings_search,"typing before lease acquisition");
        assert_eq!(app.active_workspace.as_deref(),Some(old.path()));
        assert!(matches!(fixture_protection(runtime.path(),old.path(),true),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
        drop(fixture_protection(runtime.path(),next.path(),true).unwrap());
        release.send(()).unwrap();
        let Message::WorktreeSelectionPrepared {generation,create,receipt} = worker.join().unwrap() else {panic!("unexpected selection reply")};
        // A completed receipt owns the new guard before it is committed.
        assert_eq!(app.active_workspace.as_deref(),Some(old.path()));
        assert!(matches!(fixture_protection(runtime.path(),next.path(),true),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
        drop(app.worktree_selection_prepared(generation,create,receipt));
        assert_eq!(app.active_workspace.as_deref(),Some(next.path()));
        assert!(matches!(fixture_protection(runtime.path(),next.path(),true),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
        drop(app);
        fixture_protection(runtime.path(),next.path(),true).unwrap();
    }

    #[test]
    fn failed_watch_does_not_invalidate_every_prepared_selection_receipt() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.worktree_owner_paths.watch.force_unavailable_for_test();
        let fixture = tempfile::tempdir().unwrap();
        for index in 0..2 {
            let path = fixture.path().join(format!("selection-{index}"));
            std::fs::create_dir(&path).unwrap();
            app.refresh_worktree_owners(Some(&path));
            app.worktree_selection.cancel();
            app.worktree_selection.pending = true;
            let generation = app.worktree_selection.generation;
            let epoch = app.worktree_owner_paths.invalidate_watched();
            let monitor = app.worktree_owner_paths.watch.clone();
            let task = app.prepare_worktree_selection(generation,path.clone(),false,Intent::Navigate,epoch,move |path| {
                prepare_target(path,Vec::new(),None,&monitor,|path|path.canonicalize(), |_|Ok(None))
            });
            let Message::WorktreeSelectionPrepared {generation,create,receipt} = crate::app::tests::task_app_message(task) else {panic!("unexpected selection result")};
            drop(app.worktree_selection_prepared(generation,create,receipt));
            assert_eq!(app.worktree_selection.generation,generation,"failed watch restarted a completed preparation");
            assert!(!app.worktree_selection.pending);
            assert_eq!(app.active_workspace.as_deref(),Some(path.as_path()));
        }
    }

    #[cfg(unix)]
    #[test]
    fn production_preparation_revalidates_alias_before_protecting_its_physical_owner() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&first,&alias).unwrap();
        let monitor = owner_watch::Monitor::default();
        let mut first_read = true;
        let mut protected = None;
        let (selected,_,_) = prepare_target(alias.clone(),vec![(first.clone(),None,true),(second.clone(),None,true)],None,&monitor,
            |path| {
                let value = path.canonicalize();
                if path == alias && first_read {
                    first_read = false;
                    std::fs::remove_file(&alias).unwrap();
                    std::os::unix::fs::symlink(&second,&alias).unwrap();
                }
                value
            }, |path| { protected = Some(path.to_owned()); Ok(None) }).unwrap();
        assert_eq!(selected,second);
        assert_eq!(protected,Some(second));
    }

    #[test]
    fn cached_candidates_are_physically_revalidated_at_selection_authorization() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let monitor = owner_watch::Monitor::default();
        let mut reads = Vec::new();
        let (selected,_,_) = prepare_target(path.clone(),vec![(path.clone(),Some(path.clone()),true)],Some(path.clone()),&monitor,
            |path| { reads.push(path.to_owned()); path.canonicalize() }, |_|Ok(None)).unwrap();
        assert_eq!(selected,path);
        // The cached candidate requires no read, then each authorization pass
        // reads a shared requested/root path only once.
        assert_eq!(reads,vec![path.clone(),path]);
    }

    #[cfg(unix)]
    #[test]
    fn cached_alias_retarget_during_protection_rejects_and_releases_the_obsolete_guard() {
        let runtime = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let alias = directory.path().join("alias");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&first,&alias).unwrap();
        let monitor = owner_watch::Monitor::default();
        let roots = [&first,&second].into_iter().map(|root|(root.clone(),Some(root.clone()),true)).collect();
        let result = prepare_target(alias.clone(),roots,Some(first.clone()),&monitor,|path|path.canonicalize(),|physical| {
            let guard = crate::worktree_cleanup::fixture_protection(runtime.path(),physical,false)?;
            std::fs::remove_file(&alias)?;
            std::os::unix::fs::symlink(&second,&alias)?;
            Ok(Some(guard))
        });
        assert!(matches!(result,Err(error) if error.contains("alias changed")));
        crate::worktree_cleanup::fixture_protection(runtime.path(),&first,true).unwrap();
        crate::worktree_cleanup::fixture_protection(runtime.path(),&second,true).unwrap();
    }

    #[test]
    fn committed_native_thread_uses_worktree_scope_instead_of_focused_terminal_cwd() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let primary = PathBuf::from("/project");
        let linked = PathBuf::from("/project/worktrees/linked");
        app.sidebar_model.projects = vec![SidebarProject::new(ProjectId::new(),RepositoryId::new(),"project".into(),primary.clone(),
            [&primary,&linked].into_iter().map(|path|SidebarWorktree::new(crate::model::WorktreeId::new(),path.clone(),None,false,false,None,None,None)).collect(),Vec::new())];
        let mut card = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(session) = &mut card.kind else {unreachable!()};
        session.launch.working_directory = linked.clone();
        let terminal = card.id;
        Arc::make_mut(&mut app.durable).cards.push(card);
        drop(app.select_worktree(linked.clone(),false));
        drop(app.workspace.focus(terminal));
        app.terminal_working_directories.insert(terminal,primary.clone());
        assert_eq!(app.terminal_directory(),Some(primary));
        drop(app.finish_worktree_selection(Intent::NewNativeThread));
        let chat = app.durable.cards.iter().find_map(|card|match &card.kind {CardKind::Chat(session)=>Some(session),_=>None}).unwrap();
        assert_eq!(chat.launch.recipe.working_directory,linked);
        assert_eq!(app.active_workspace.as_ref(),Some(&linked));
    }

    #[test]
    fn terminal_commit_callback_runs_after_card_insertion() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.worktree_selection.cancel();
        app.worktree_selection.pending = true;
        let generation = app.worktree_selection.generation;
        let card = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(session) = card.kind else {unreachable!()};
        let intent = Intent::TerminalWithLifecycle {session,
            committed:Box::new(Message::Sidebar(SidebarAction::OpenSettings)),
            rejected:Box::new(Message::SettingsSearchChanged("rejected".into()))};
        let task = app.prepare_worktree_selection(generation,"/repo".into(),false,intent,
            app.worktree_owner_paths.watch.epoch(), |path|Ok((path,Vec::new(),None)));
        let Message::WorktreeSelectionPrepared {generation,create,receipt} = crate::app::tests::task_app_message(task) else {panic!("unexpected selection reply")};
        drop(app.worktree_selection_prepared(generation,create,receipt));
        assert!(app.durable.cards.iter().any(|card|matches!(card.kind,CardKind::Terminal(_))));
        assert!(matches!(app.workspace.focused().and_then(|id|app.card_state(id)).map(|card|&card.kind),Some(CardKind::Settings)),
            "terminal insertion after the callback would steal its settings focus");
        assert_ne!(app.settings_search,"rejected");
    }

    #[test]
    fn failed_preparation_performs_no_dependent_mutation() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.select_worktree("/old".into(),false));
        let before = app.durable.cards.len();
        app.worktree_selection.cancel();
        let generation = app.worktree_selection.generation;
        app.worktree_selection.pending = true;
        let card = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(session) = card.kind else {unreachable!()};
        let intent = Intent::TerminalWithLifecycle {session,
            committed:Box::new(Message::SettingsSearchChanged("committed".into())),
            rejected:Box::new(Message::SettingsSearchChanged("rejected".into()))};
        let task = app.prepare_worktree_selection(generation,"/unavailable".into(),false,intent,app.worktree_owner_paths.watch.epoch(), |_| Err("lease unavailable".into()));
        let Message::WorktreeSelectionPrepared {generation,create,receipt} = crate::app::tests::task_app_message(task) else {panic!("unexpected selection reply")};
        drop(app.worktree_selection_prepared(generation,create,receipt.clone()));
        assert_eq!(app.settings_search,"rejected");
        app.settings_search = "already settled".into();
        drop(app.worktree_selection_prepared(generation,create,receipt));
        assert_eq!(app.settings_search,"already settled","receipt replay repeated lifecycle callback");
        assert_eq!(app.active_workspace.as_deref(),Some(Path::new("/old")));
        assert_eq!(app.durable.cards.len(),before);
        assert!(app.status.as_ref().unwrap().message.contains("lease unavailable"));
    }
}
