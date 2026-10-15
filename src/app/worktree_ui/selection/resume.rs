//! Recorded-cwd classification and its selected scope share one lease receipt.
use super::*;

fn prepare_resume(
    recorded: &Path,
    allow_missing: bool,
    snapshot: &placement::Snapshot,
    watch: &owner_watch::Monitor,
    mut canonicalize: impl FnMut(&Path) -> std::io::Result<PathBuf>,
    mut directory_available: impl FnMut(&Path) -> bool,
    mut protect: impl FnMut(&Path) -> std::io::Result<Option<crate::worktree_cleanup::Protection>>,
) -> Result<Prepared, String> {
    let paths = snapshot.required_paths(recorded);
    // Install lexical ancestor watches before the first physical observation.
    watch.register(&paths.iter().map(|path| (path.clone(), path.clone())).collect::<Vec<_>>());
    let mut read = || {
        let canonical = paths.iter().filter_map(|path| canonicalize(path).ok().map(|physical| (path.clone(), physical))).collect::<Vec<_>>();
        let resolved = placement::resolve(recorded, allow_missing, snapshot, &canonical, directory_available(recorded))?;
        Ok::<_, String>((resolved, canonical))
    };
    let (_, first) = read()?;
    watch.register(&first);
    // The first pass only discovers physical watch parents. A second fresh
    // observation authorizes the guard; its evidence is checked again after it.
    let (resolved, canonical) = read()?;
    watch.register(&canonical);
    let guard = protect(&resolved.physical_root).map_err(|error| error.to_string())?;
    // Filing in B never replaces the immutable executable cwd A. Protect both
    // before exposure; acquire() also holds containing Git-worktree ancestors.
    let mut additional_guards = Vec::new();
    if let Some((_, launch)) = canonical.iter().find(|(path, _)| path == recorded)
        && launch != &resolved.physical_root
        && let Some(guard) = protect(launch).map_err(|error| error.to_string())?
    {
        additional_guards.push(guard);
    }
    let final_paths = read()?;
    if final_paths != (resolved.clone(), canonical.clone()) {
        return Err("The conversation's project changed while acquiring its protection. Resume it again.".into());
    }
    Ok(Prepared { path: resolved.root.clone(), canonical, guard, resume: Some(resolved), additional_guards })
}

/// Remote paths belong to the selected SSH host. Lexical metadata is the
/// authority here; local filesystem state cannot qualify a remote path.
fn prepare_remote_resume(recorded: &Path, snapshot: &placement::Snapshot) -> Result<Prepared, String> {
    let canonical = snapshot.required_paths(recorded).into_iter().map(|path| (path.clone(), path)).collect::<Vec<_>>();
    let resolved = placement::resolve(recorded, false, snapshot, &canonical, true)?;
    Ok(Prepared { path: resolved.root.clone(), canonical, guard: None, resume: Some(resolved), additional_guards: Vec::new() })
}

impl Cagents {
    /// No directory is inspected, channel created, or scope exposed on this
    /// thread. Every retry takes a fresh immutable metadata snapshot.
    pub(in crate::app) fn select_resume_then(
        &mut self,
        entry: crate::persistence::SavedConversation,
        replace: Option<CardId>,
    ) -> Task<Message> {
        let cancelled = self.cancel_worktree_selection();
        let remote = crate::device::is_remote();
        if !remote {
            self.worktree_owner_paths.invalidate_watched();
        }
        // Incorporate discovered/restored root metadata before snapshotting.
        self.refresh_worktree_owners(None);
        let snapshot = placement::Snapshot::capture(self, entry.card, &entry.session);
        let recorded = entry.session.launch.recipe.working_directory.clone();
        self.worktree_selection.additional_paths = snapshot.required_paths(&recorded).into_iter().collect();
        self.refresh_worktree_owners(Some(&recorded));
        let epoch = self.worktree_owner_paths.watch.epoch();
        let generation = self.worktree_selection.generation;
        self.worktree_selection.pending = true;
        let watch = self.worktree_owner_paths.watch.clone();
        let launch = entry.session.launch.clone();
        let worker_snapshot = snapshot.clone();
        let local_window = crate::instance::window_is_initialized() && !crate::device::is_remote();
        #[cfg(test)]
        let local_window = self.worktree_selection.local_window_override.unwrap_or(local_window);
        #[cfg(test)]
        let guard_runtime = self.worktree_selection.guard_runtime_override.clone();
        #[cfg(test)]
        let canonicalize_calls = self.worktree_selection.canonicalize_calls.clone();
        let prepare = self.prepare_worktree_selection_receipt(
            generation, recorded, false, Intent::ResumePlacement { entry, replace, snapshot }, epoch,
            move |recorded| {
                if remote {
                    return prepare_remote_resume(&recorded, &worker_snapshot);
                }
                prepare_resume(
                &recorded, launch.needs_read_only_history(), &worker_snapshot, &watch,
                |path| {
                    #[cfg(test)]
                    if let Some(calls) = &canonicalize_calls {
                        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    path.canonicalize()
                },
                Path::is_dir,
                |path| {
                    if !local_window { return Ok(None); }
                    #[cfg(test)]
                    if let Some(runtime) = &guard_runtime {
                        return crate::worktree_cleanup::fixture_protection(runtime, path, false).map(Some);
                    }
                    crate::worktree_cleanup::protect(path)?.ok_or_else(|| std::io::Error::other("local conversation protection became unavailable")).map(Some)
                },
            )},
        );
        Task::batch([cancelled, prepare])
    }

    pub(super) fn commit_resumed_selection_scope(&mut self, resolved: &placement::Resolved, title: &str) -> Task<Message> {
        match &resolved.destination {
            placement::Destination::Channel(id) => self.select_channel_inner(*id),
            placement::Destination::Worktree(root) => self.select_worktree_inner(root.clone(), false),
            placement::Destination::NewChannel { project, .. } => {
                let id = ChannelId::new();
                let ordinal = self.next_provisional_ordinal();
                Arc::make_mut(&mut self.durable).channels.push(crate::model::ProjectChannel {
                    id, project_id: *project, title: title.to_owned(),
                    title_source: crate::model::ChannelTitleSource::Prompt, ordinal,
                    shared: false, cards: Vec::new(), focused: None,
                });
                self.publish_channel_creation(id, *project);
                self.select_channel_inner(id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::Application as _;

    fn entry(path: &Path) -> crate::persistence::SavedConversation {
        let mut session = crate::chat::fresh_session_deferred_policy(
            crate::agent::Provider::Claude, TerminalId::new(), path.to_owned(),
            &crate::account::Account::system(crate::agent::Provider::Claude),
        ).unwrap();
        session.launch.session_id = Some("protected-resume-fixture".into());
        crate::persistence::SavedConversation {
            card: CardId::new(), title: "Protected history".into(), session,
            open: false, updated_at_unix_ms: 1,
        }
    }

    fn snapshot(path: &Path) -> placement::Snapshot {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.known_worktree_roots.insert(path.to_owned());
        let entry = entry(path);
        placement::Snapshot::capture(&app, entry.card, &entry.session)
    }

    #[cfg(unix)]
    #[test]
    fn resume_revalidates_alias_after_watch_registration_before_guarding() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let scope = snapshot(&alias);
        let watch = owner_watch::Monitor::default();
        let mut swapped = false;
        let prepared = prepare_resume(
            &alias, false, &scope, &watch,
            |path| {
                let physical = path.canonicalize();
                if path == alias && !swapped {
                    std::fs::remove_file(&alias).unwrap();
                    std::os::unix::fs::symlink(&second, &alias).unwrap();
                    swapped = true;
                }
                physical
            }, Path::is_dir,
            |path| crate::worktree_cleanup::fixture_protection(runtime.path(), path, false).map(Some),
        ).unwrap();
        assert_eq!(prepared.resume.as_ref().unwrap().physical_root, second);
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &first, true).is_ok());
        assert!(matches!(crate::worktree_cleanup::fixture_protection(runtime.path(), &second, true),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
        drop(prepared);
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &second, true).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn resume_retarget_during_guard_acquisition_rejects_and_releases_guard() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let scope = snapshot(&alias);
        let watch = owner_watch::Monitor::default();
        let result = prepare_resume(&alias, false, &scope, &watch, Path::canonicalize, Path::is_dir,
            |path| {
                let guard = crate::worktree_cleanup::fixture_protection(runtime.path(), path, false)?;
                std::fs::remove_file(&alias).unwrap();
                std::os::unix::fs::symlink(&second, &alias).unwrap();
                Ok(Some(guard))
            });
        assert!(result.is_err());
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &first, true).is_ok());
    }

    #[test]
    fn resumed_scope_classification_is_scheduled_without_ui_filesystem_reads() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.known_worktree_roots.insert(directory.path().to_owned());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        app.worktree_selection.canonicalize_calls = Some(calls.clone());
        let entry = entry(directory.path());
        let original = entry.session.launch.clone();
        let prepare = app.select_resume_then(entry, None);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(app.worktree_selection.pending);
        drop(app.update(Message::SettingsSearchChanged("input remains responsive".into())));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        let message = crate::app::tests::task_app_message(prepare);
        drop(app.update(message));
        assert!(calls.load(std::sync::atomic::Ordering::Relaxed) > 0);
        assert_eq!(app.active_workspace.as_deref(), Some(directory.path()));
        assert!(app.durable.cards.iter().any(|card| matches!(&card.kind,
            CardKind::Chat(session) if session.launch == original)));
    }
    #[test]
    fn resumed_fallback_filing_protects_independent_launch_worktree_before_exposure() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let launch_root = directory.path().join("issued-root");
        let recorded = launch_root.join("nested");
        let filing = directory.path().join("filing");
        std::fs::create_dir_all(&recorded).unwrap();
        std::fs::create_dir_all(launch_root.join(".git")).unwrap();
        std::fs::create_dir_all(&filing).unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.known_worktree_roots.clear();
        app.active_workspace = Some(filing.clone());
        Arc::make_mut(&mut app.durable).active_channel = None;
        let entry = entry(&recorded);
        let scope = placement::Snapshot::capture(&app, entry.card, &entry.session);
        let watch = owner_watch::Monitor::default();
        let prepared = prepare_resume(&recorded, false, &scope, &watch, Path::canonicalize, Path::is_dir,
            |path| crate::worktree_cleanup::fixture_protection(runtime.path(), path, false).map(Some)).unwrap();
        assert_eq!(prepared.resume.as_ref().unwrap().destination, placement::Destination::Worktree(filing.clone()));
        for path in [&launch_root, &filing] {
            assert!(matches!(crate::worktree_cleanup::fixture_protection(runtime.path(), path, true),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
        }
        drop(prepared);
        for path in [&launch_root, &filing] {
            assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), path, true).is_ok());
        }
    }

    #[test]
    fn remote_resume_uses_host_lexical_ownership_without_local_filesystem_authority() {
        let recorded = PathBuf::from("/remote-host-only-cagents-ownership/linked/nested");
        let owner = PathBuf::from("/remote-host-only-cagents-ownership/linked");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.known_worktree_roots.clear();
        app.known_worktree_roots.insert(owner.clone());
        let entry = entry(&recorded);
        let scope = placement::Snapshot::capture(&app, entry.card, &entry.session);
        let prepared = prepare_remote_resume(&recorded, &scope).unwrap();
        assert_eq!(prepared.path, owner);
        assert_eq!(prepared.resume.as_ref().unwrap().physical_root, owner);
        assert!(prepared.canonical.iter().all(|(lexical, physical)| lexical == physical));
        assert!(prepared.guard.is_none());
        assert!(prepared.additional_guards.is_empty());
    }

    #[test]
    fn unavailable_launch_lease_releases_filing_guard_without_a_resume_receipt() {
        let launch = tempfile::tempdir().unwrap();
        let filing = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.known_worktree_roots.clear();
        app.active_workspace = Some(filing.path().to_owned());
        Arc::make_mut(&mut app.durable).active_channel = None;
        let entry = entry(launch.path());
        let scope = placement::Snapshot::capture(&app, entry.card, &entry.session);
        let _exclusive = crate::worktree_cleanup::fixture_protection(runtime.path(), launch.path(), true).unwrap();
        let result = prepare_resume(launch.path(), false, &scope, &owner_watch::Monitor::default(), Path::canonicalize, Path::is_dir,
            |path| crate::worktree_cleanup::fixture_protection(runtime.path(), path, false).map(Some));
        assert!(result.is_err());
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), filing.path(), true).is_ok());
    }

    #[test]
    fn adoption_while_resume_prepares_reuses_existing_logical_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.known_worktree_roots.insert(directory.path().to_owned());
        let entry = entry(directory.path());
        let logical = entry.session.clone();
        let task = app.select_resume_then(entry, None);
        let receipt = crate::app::tests::task_app_message(task);
        drop(app.insert_chat_card(logical.clone(), None, None));
        let existing = app.existing_conversation_card(&logical).unwrap();
        let count = app.durable.cards.len();
        drop(app.update(receipt));
        assert_eq!(app.durable.cards.len(), count);
        assert_eq!(app.existing_conversation_card(&logical), Some(existing));
    }

}
