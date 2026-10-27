//! Remembered display placement only. These keys carry no launch authority.
use super::*;

/// Use only the account context saved with this conversation. Lexical paths
/// deliberately remain lexical: a lookup must not inspect disk or today's
/// selected account to guess which history a conversation belongs to.
pub(super) fn conversation_key(session: &chat::Session) -> Option<String> {
    let launch = &session.launch;
    let id = launch.session_id.as_deref().filter(|id| !id.is_empty())?;
    let recipe = &launch.recipe;
    let root = recorded_account_root(launch.provider, recipe)?;
    serde_json::to_string(&(
        "conversation-channel-v1",
        launch.provider,
        id,
        &recipe.working_directory,
        root,
    ))
    .ok()
}

/// The recorded provider's account root, without consulting today's account
/// or the filesystem. Irrelevant HOME/XDG values cannot split an explicitly
/// selected Claude/Codex account into two live writers.
fn recorded_account_root(provider: Provider, recipe: &LaunchRecipe) -> Option<PathBuf> {
    let present = |name: &str| {
        recipe
            .environment
            .get(name)
            .filter(|_| !recipe.unset_environment.contains(name))
    };
    let recorded = |name: &str| present(name).filter(|value| !value.is_empty());
    let resolve = |path: &str| recipe.working_directory.join(path);
    match provider {
        Provider::Claude | Provider::Codex => {
            // The preserved Codex adapter treats an explicit empty home as
            // the launch cwd; Claude treats empty as its HOME fallback.
            if provider == Provider::Codex
                && let Some(root) = present("CODEX_HOME")
            {
                return Some(resolve(root));
            }
            if let Some(root) = crate::account::home_variable(provider).and_then(recorded) {
                return Some(resolve(root));
            }
            let home = resolve(recorded("HOME")?);
            Some(home.join(if provider == Provider::Claude {
                ".claude"
            } else {
                ".codex"
            }))
        }
        Provider::Omp => {
            // fresh_session records this exact native recipe after resolving
            // Omp's profile/XDG account root. Trust that saved selection only;
            // legacy or complex argv needs the original cwd fallback rather
            // than a new option parser or Omp's filesystem-dependent probe.
            match recipe.arguments.as_slice() {
                [flag, directory] if flag == "--session-dir" && !directory.is_empty() => {
                    Some(resolve(directory))
                }
                _ => None,
            }
        }
    }
}

fn closed_filing(state: &WorkspaceState, card: &CardState) -> Option<(String, ChannelId)> {
    if !card.existence.is_open() {
        return None;
    }
    let CardKind::Chat(session) = &card.kind else {
        return None;
    };
    let key = conversation_key(session)?;
    let mut channels = state
        .channels
        .iter()
        .filter(|channel| channel.cards.contains(&card.id));
    let channel = channels.next()?.id;
    // Ambiguous membership is not permission to pick the first destination.
    if channels.next().is_some() || state.conversation_channels.get(&key) == Some(&channel) {
        return None;
    }
    Some((key, channel))
}

impl Cagents {
    /// Called only after the workspace close commits and before membership is
    /// pruned. The containing channel, never the browsing selection, files it.
    pub(in crate::app) fn remember_closed_conversation_channel(&mut self, card: &CardState) {
        let Some((key, channel)) = closed_filing(&self.durable, card) else {
            return;
        };
        let state = Arc::make_mut(&mut self.durable);
        Arc::make_mut(&mut state.conversation_channels).insert(key, channel);
    }

    /// A remembered ID only selects a currently usable channel. It does not
    /// recreate one, change a launch directory or replace resume preparation.
    pub(super) fn select_recorded_conversation_channel(
        &mut self,
        session: &chat::Session,
    ) -> Option<Task<Message>> {
        let key = conversation_key(session)?;
        let channel = *self.durable.conversation_channels.get(&key)?;
        let path = self.channel_directory(channel)?;
        // Filing changes display placement, never the original launch cwd.
        // Retain that admission as well as a different current channel root
        // before replacing the scope. No navigation changes on refusal.
        let launch_directory = &session.launch.recipe.working_directory;
        let launch_protected = match self.worktree_protections.protect_next(launch_directory) {
            Ok(()) => true,
            // Existing exact Claude history can be read after its original
            // directory is removed; this never authorizes a provider elsewhere.
            Err(_) if session.launch.needs_read_only_history() => false,
            Err(_) => return None,
        };
        if &path != launch_directory || !launch_protected {
            self.worktree_protections.protect_next(&path).ok()?;
        }
        let task = self.select_channel(channel);
        (self.durable.active_channel == Some(channel)
            && self.active_workspace.as_ref() == Some(&path))
        .then_some(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, ProjectChannel, WorktreeId};
    use crate::repository::RepositoryId;

    struct Fixture {
        directory: tempfile::TempDir,
        app: Cagents,
        conversation: CardId,
        a: ChannelId,
        b: ChannelId,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_path_buf();
            let mut app = crate::app::tests::context_inspector_test_app(true);
            let mut conversation = app
                .card_state(app.workspace.focused().unwrap())
                .unwrap()
                .clone();
            let mut session = chat::fresh_session(
                Provider::Claude,
                TerminalId::new(),
                root.clone(),
                &crate::account::Account::system(Provider::Claude),
            )
            .unwrap();
            session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
            session
                .launch
                .recipe
                .environment
                .insert("HOME".into(), "/recorded/account-a".into());
            session.launch.recipe.environment.insert(
                "CLAUDE_CONFIG_DIR".into(),
                "/recorded/account-a/claude".into(),
            );
            session.launch.recipe.unset_environment.remove("HOME");
            session
                .launch
                .recipe
                .unset_environment
                .remove("CLAUDE_CONFIG_DIR");
            conversation.kind = CardKind::Chat(session);
            conversation.title = "Recorded conversation".into();
            // Real content, not a window utility: these siblings keep each
            // channel valid after the conversation closes.
            let sibling_a = crate::app::tests::terminal_card_state();
            let sibling_b = crate::app::tests::terminal_card_state();
            let project = ProjectId::new();
            let a = ChannelId::new();
            let b = ChannelId::new();
            app.sidebar_model.projects = vec![SidebarProject::new(
                project,
                RepositoryId::new(),
                "Project".into(),
                root.clone(),
                vec![SidebarWorktree::new(
                    WorktreeId::new(),
                    root.clone(),
                    None,
                    false,
                    false,
                    None,
                    None,
                    None,
                )],
                Vec::new(),
            )];
            app.durable = Arc::new(WorkspaceState {
                cards: vec![conversation.clone(), sibling_a.clone(), sibling_b.clone()],
                focused: Some(conversation.id),
                active_worktree: Some(root),
                active_channel: Some(a),
                channels: vec![
                    ProjectChannel {
                        id: a,
                        project_id: project,
                        title: "Original".into(),
                        title_source: Default::default(),
                        ordinal: 0,
                        shared: false,
                        cards: vec![conversation.id, sibling_a.id],
                        focused: Some(conversation.id),
                    },
                    ProjectChannel {
                        id: b,
                        project_id: project,
                        title: "Other".into(),
                        title_source: Default::default(),
                        ordinal: 1,
                        shared: false,
                        cards: vec![sibling_b.id],
                        focused: Some(sibling_b.id),
                    },
                ],
                ..WorkspaceState::default()
            });
            app.workspace = Cagents::rebuild_workspace(&app.durable);
            app.workspace.resize_viewport(1280.0);
            drop(app.restore_worktree_scope());
            let (writer, restored) = SessionRuntime::open(
                directory.path().join("state.sqlite3"),
                BootId::new("filing-test"),
            )
            .unwrap();
            assert!(restored.is_none());
            app.session = Some(writer);
            app.persist();
            app.session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(5))
                .unwrap();
            Self {
                directory,
                app,
                conversation: conversation.id,
                a,
                b,
            }
        }

        fn close(&mut self, id: CardId) {
            assert!(
                self.app.channels_emptied_by(&[id]).is_empty(),
                "owned sibling keeps channel usable"
            );
            self.app.workspace.request_close(id).unwrap();
            drop(
                self.app
                    .commit_card_removal(id, None)
                    .expect("committed close"),
            );
            self.app
                .session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(5))
                .unwrap();
            assert!(!self.app.card_state(id).unwrap().existence.is_open());
        }

        fn reopen(&mut self, closed: CardId) -> SavedConversation {
            self.app.persist();
            let mut writer = self.app.session.take().unwrap();
            writer.flush(Duration::from_secs(5)).unwrap();
            writer.shutdown();
            let (writer, restored) = SessionRuntime::open(
                self.directory.path().join("state.sqlite3"),
                BootId::new("filing-test"),
            )
            .unwrap();
            let entry = writer
                .operation_store()
                .saved_conversations()
                .unwrap()
                .into_iter()
                .find(|entry| entry.card == closed)
                .expect("actual closed SQL row");
            assert!(!entry.open);
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.sidebar_model = self.app.sidebar_model.clone();
            app.durable = Arc::new(restored.unwrap());
            app.workspace = Cagents::rebuild_workspace(&app.durable);
            app.workspace.resize_viewport(1280.0);
            app.session = Some(writer);
            drop(app.restore_worktree_scope());
            self.app = app;
            entry
        }

        fn resumed(&mut self, entry: SavedConversation) -> CardId {
            let launch = entry.session.launch.clone();
            drop(self.app.resume_saved_conversation(entry));
            let id = self.app.workspace.focused().unwrap();
            let CardKind::Chat(session) = &self.app.card_state(id).unwrap().kind else {
                panic!("resumed chat")
            };
            assert_eq!(session.launch, launch);
            id
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // The SQLite writer settles before its temporary directory is
            // removed, including assertion failures. No provider task runs.
            if let Some(mut writer) = self.app.session.take() {
                writer.shutdown();
            }
        }
    }

    #[test]
    fn successful_close_flush_reopen_resumes_renamed_original_channel_and_latest_refiling() {
        let mut fixture = Fixture::new();
        // A failed/stale acknowledgement cannot create filing metadata.
        assert!(
            fixture
                .app
                .commit_card_removal(fixture.conversation, None)
                .is_none()
        );
        assert!(fixture.app.durable.conversation_channels.is_empty());
        fixture.close(fixture.conversation);
        let entry = fixture.reopen(fixture.conversation);
        let key = conversation_key(&entry.session).unwrap();
        assert_eq!(
            fixture.app.durable.conversation_channels.get(&key),
            Some(&fixture.a)
        );
        Arc::make_mut(&mut fixture.app.durable)
            .channels
            .iter_mut()
            .find(|c| c.id == fixture.a)
            .unwrap()
            .title = "Renamed".into();
        drop(fixture.app.select_channel(fixture.b));
        let first = fixture.resumed(entry);
        assert_ne!(first, fixture.conversation);
        assert_eq!(fixture.app.durable.active_channel, Some(fixture.a));
        assert!(
            fixture
                .app
                .durable
                .channels
                .iter()
                .any(|c| c.id == fixture.a && c.title == "Renamed" && c.cards.contains(&first))
        );
        // Model the completed move's actual membership, then close through the
        // production close writer again. No filing map is seeded by the test.
        let state = Arc::make_mut(&mut fixture.app.durable);
        for channel in &mut state.channels {
            channel.cards.retain(|id| *id != first);
        }
        state
            .channels
            .iter_mut()
            .find(|c| c.id == fixture.b)
            .unwrap()
            .cards
            .push(first);
        drop(fixture.app.select_channel(fixture.b));
        fixture.close(first);
        let entry = fixture.reopen(first);
        assert_eq!(
            fixture.app.durable.conversation_channels.get(&key),
            Some(&fixture.b)
        );
        drop(fixture.app.select_channel(fixture.a));
        fixture.resumed(entry.clone());
        assert_eq!(fixture.app.durable.active_channel, Some(fixture.b));
        // An already-open instance wins over remembered placement and does not
        // insert another card.
        let count = fixture.app.durable.cards.len();
        let live = fixture.app.workspace.focused().unwrap();
        drop(fixture.app.resume_saved_conversation(entry.clone()));
        assert_eq!(fixture.app.durable.cards.len(), count);
        assert_eq!(fixture.app.workspace.focused(), Some(live));
        // A live instance moved again after preparation began takes priority
        // over its last closed filing, even on the delayed prepared callback.
        let state = Arc::make_mut(&mut fixture.app.durable);
        for channel in &mut state.channels {
            channel.cards.retain(|id| *id != live);
        }
        state
            .channels
            .iter_mut()
            .find(|c| c.id == fixture.a)
            .unwrap()
            .cards
            .push(live);
        drop(fixture.app.select_channel(fixture.a));
        let generation = fixture.app.resume_ui.generation;
        fixture.app.resume_ui.loading = true;
        drop(fixture.app.update_resume(Action::Prepared(
            generation,
            entry.clone(),
            Ok(entry.session.clone()),
        )));
        assert_eq!(fixture.app.durable.cards.len(), count);
        assert_eq!(fixture.app.durable.active_channel, Some(fixture.a));
        assert_eq!(
            fixture.app.durable.conversation_channels.get(&key),
            Some(&fixture.b)
        );
        let before = fixture.app.durable.clone();
        drop(fixture.app.update_resume(Action::Prepared(
            generation.wrapping_add(1),
            entry.clone(),
            Ok(entry.session),
        )));
        assert!(Arc::ptr_eq(&before, &fixture.app.durable));
    }

    #[test]
    fn deleted_recreated_or_unavailable_channel_falls_back_without_recreating_it() {
        for missing_project in [false, true] {
            let mut fixture = Fixture::new();
            fixture.close(fixture.conversation);
            let entry = fixture.reopen(fixture.conversation);
            let replacement = ChannelId::new();
            if missing_project {
                fixture.app.sidebar_model.projects.clear();
            } else {
                let state = Arc::make_mut(&mut fixture.app.durable);
                let channel = state
                    .channels
                    .iter_mut()
                    .find(|c| c.id == fixture.a)
                    .unwrap();
                channel.id = replacement; // same title, genuinely different identity
            }
            let before = fixture.app.durable.channels.len();
            fixture.resumed(entry);
            assert_eq!(fixture.app.durable.active_channel, None);
            assert_eq!(
                fixture.app.active_workspace.as_deref(),
                Some(fixture.directory.path())
            );
            assert_eq!(fixture.app.durable.channels.len(), before);
            assert!(
                !fixture
                    .app
                    .durable
                    .channels
                    .iter()
                    .any(|c| c.id == fixture.a)
                    || missing_project
            );
            assert_ne!(fixture.app.durable.active_channel, Some(replacement));
        }

        // Existing source-owned lease fixture makes the remembered directory
        // genuinely unavailable, without a process, timeout or fake grant.
        let mut fixture = Fixture::new();
        fixture.close(fixture.conversation);
        let entry = fixture.reopen(fixture.conversation);
        drop(fixture.app.select_channel(fixture.b));
        let original = fixture.directory.path().to_path_buf();
        let blocked = original.join("removing");
        let runtime = original.join("lease-runtime");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::create_dir(&runtime).unwrap();
        fixture
            .app
            .worktree_protections
            .use_fixture_runtime(runtime.clone());
        fixture
            .app
            .worktree_protections
            .protect_next(&original)
            .unwrap();
        let busy = crate::worktree_cleanup::fixture_protection(&runtime, &blocked, true).unwrap();
        let project = &mut fixture.app.sidebar_model.projects[0];
        project.repository = blocked.clone();
        project.worktrees[0].path = blocked.clone();
        let before = fixture.app.durable.conversation_channels.clone();
        assert!(
            fixture
                .app
                .select_recorded_conversation_channel(&entry.session)
                .is_none()
        );
        assert_eq!(fixture.app.durable.active_channel, Some(fixture.b));
        assert_eq!(fixture.app.active_workspace.as_ref(), Some(&original));
        assert!(Arc::ptr_eq(
            &before,
            &fixture.app.durable.conversation_channels
        ));
        assert!(
            crate::worktree_cleanup::fixture_protection(&runtime, &original, true).is_err(),
            "original scope lease retained before fallback"
        );
        let fallback = fixture.resumed(entry);
        assert_eq!(fixture.app.durable.active_channel, None);
        assert_eq!(fixture.app.active_workspace.as_ref(), Some(&original));
        assert!(
            crate::worktree_cleanup::fixture_protection(&runtime, &original, true).is_err(),
            "original fallback remains protected"
        );
        assert!(
            crate::worktree_cleanup::fixture_protection(&runtime, &blocked, false).is_err(),
            "busy destination owner unchanged"
        );
        fixture.close(fallback);
        let entry = fixture
            .app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .saved_conversations()
            .unwrap()
            .into_iter()
            .find(|entry| entry.card == fallback)
            .unwrap();
        drop(busy);
        fixture.resumed(entry);
        assert_eq!(fixture.app.durable.active_channel, Some(fixture.a));
        assert_eq!(fixture.app.active_workspace.as_ref(), Some(&blocked));
        assert!(
            crate::worktree_cleanup::fixture_protection(&runtime, &original, true).is_err(),
            "refiled display still retains original launch cwd lease"
        );
        assert!(
            crate::worktree_cleanup::fixture_protection(&runtime, &blocked, true).is_err(),
            "selected channel directory also retained"
        );

        // Both remembered and launch destinations are unavailable. Keep the
        // current, separately protected browsing scope and the retry intent.
        let mut fixture = Fixture::new();
        fixture.close(fixture.conversation);
        let entry = fixture
            .app
            .session
            .as_ref()
            .unwrap()
            .operation_store()
            .saved_conversations()
            .unwrap()
            .into_iter()
            .find(|entry| entry.card == fixture.conversation)
            .unwrap();
        let original = fixture.directory.path().to_path_buf();
        let browsing = original.join("browsing");
        let runtime = original.join("lease-runtime");
        std::fs::create_dir(&browsing).unwrap();
        std::fs::create_dir(&runtime).unwrap();
        let project = ProjectId::new();
        fixture.app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "Browsing".into(),
            browsing.clone(),
            vec![SidebarWorktree::new(
                WorktreeId::new(),
                browsing.clone(),
                None,
                false,
                false,
                None,
                None,
                None,
            )],
            Vec::new(),
        ));
        Arc::make_mut(&mut fixture.app.durable)
            .channels
            .iter_mut()
            .find(|channel| channel.id == fixture.b)
            .unwrap()
            .project_id = project;
        drop(fixture.app.select_channel(fixture.b));
        fixture
            .app
            .worktree_protections
            .use_fixture_runtime(runtime.clone());
        fixture
            .app
            .worktree_protections
            .protect_next(&browsing)
            .unwrap();
        let busy = crate::worktree_cleanup::fixture_protection(&runtime, &original, true).unwrap();
        fixture.app.resume_ui.open = true;
        fixture.app.resume_ui.replace = fixture.app.workspace.focused();
        let replace = fixture.app.resume_ui.replace;
        let before = fixture.app.durable.cards.len();
        let transition_pending = fixture.app.chat_ui.transition_pending();
        assert!(!transition_pending);
        drop(fixture.app.resume_saved_conversation(entry.clone()));
        assert_eq!(fixture.app.durable.cards.len(), before);
        assert_eq!(
            fixture.app.chat_ui.transition_pending(),
            transition_pending,
            "no provider start intent"
        );
        assert_eq!(fixture.app.durable.active_channel, Some(fixture.b));
        assert_eq!(fixture.app.active_workspace.as_ref(), Some(&browsing));
        assert!(fixture.app.resume_ui.open);
        assert_eq!(fixture.app.resume_ui.replace, replace);
        assert!(
            fixture
                .app
                .recently_closed
                .iter()
                .any(|(card, _)| card.id == fixture.conversation)
        );
        assert!(
            !fixture
                .app
                .card_state(fixture.conversation)
                .unwrap()
                .existence
                .is_open()
        );
        assert!(
            crate::worktree_cleanup::fixture_protection(&runtime, &browsing, true).is_err(),
            "browsing scope fence retained"
        );
        assert!(
            crate::worktree_cleanup::fixture_protection(&runtime, &original, false).is_err(),
            "busy original owner unchanged"
        );
        drop(busy);
        fixture.resumed(entry);
        assert_eq!(fixture.app.durable.active_channel, Some(fixture.a));
        assert!(!fixture.app.resume_ui.open);
        assert!(
            !fixture
                .app
                .recently_closed
                .iter()
                .any(|(card, _)| card.id == fixture.conversation)
        );

        // Exact saved Claude history remains readable after its real original
        // directory disappears. A usable remembered Q may display it, without
        // rewriting P or executing any provider task in this headless fixture.
        for remembered_available in [true, false] {
            let mut fixture = Fixture::new();
            // Separate from Q's tree, so old scope-root discovery cannot
            // silently substitute an existing ancestor for the missing P.
            let original_directory = tempfile::tempdir().unwrap();
            let original = original_directory.path().to_path_buf();
            let CardKind::Chat(session) = &mut fixture
                .app
                .card_state_mut(fixture.conversation)
                .unwrap()
                .kind
            else {
                panic!("chat")
            };
            session.launch.recipe.working_directory = original.clone();
            fixture.close(fixture.conversation);
            let entry = fixture.reopen(fixture.conversation);
            original_directory.close().unwrap();
            assert!(entry.session.launch.needs_read_only_history());
            let runtime = fixture.directory.path().join("history-lease-runtime");
            std::fs::create_dir(&runtime).unwrap();
            fixture
                .app
                .worktree_protections
                .use_fixture_runtime(runtime.clone());
            if !remembered_available {
                fixture.app.sidebar_model.projects.clear();
            }
            let id = fixture.resumed(entry);
            let CardKind::Chat(session) = &fixture.app.card_state(id).unwrap().kind else {
                panic!("history")
            };
            assert_eq!(session.launch.recipe.working_directory, original);
            assert!(session.launch.needs_read_only_history());
            if remembered_available {
                assert_eq!(fixture.app.durable.active_channel, Some(fixture.a));
                assert!(
                    crate::worktree_cleanup::fixture_protection(
                        &runtime,
                        fixture.directory.path(),
                        true
                    )
                    .is_err(),
                    "usable Q remains protected for history display"
                );
            }
            assert!(fixture.app.resume_ui.error.is_none());
            assert!(
                !fixture
                    .app
                    .card_state(fixture.conversation)
                    .unwrap()
                    .existence
                    .is_open()
            );
        }
    }

    #[test]
    fn recorded_context_collision_uses_cwd_fallback_and_unchanged_filing_shares_arc() {
        let mut fixture = Fixture::new();
        fixture.close(fixture.conversation);
        let entry = fixture.reopen(fixture.conversation);
        let original_key = conversation_key(&entry.session).unwrap();
        let mut other = entry.clone();
        other.session.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            "/recorded/account-b/claude".into(),
        );
        assert_ne!(conversation_key(&other.session).unwrap(), original_key);
        fixture.resumed(other);
        assert_eq!(fixture.app.durable.active_channel, None);
        // The real close/resume path for the original account still files it.
        let id = fixture.resumed(entry);
        assert_eq!(fixture.app.durable.active_channel, Some(fixture.a));
        let before = fixture.app.durable.conversation_channels.clone();
        fixture.close(id);
        assert!(Arc::ptr_eq(
            &before,
            &fixture.app.durable.conversation_channels
        ));
    }

    #[test]
    fn key_is_exact_recorded_context_and_refuses_unknown_home_or_unidentified_session() {
        let fixture = Fixture::new();
        let CardKind::Chat(mut session) = fixture
            .app
            .card_state(fixture.conversation)
            .unwrap()
            .kind
            .clone()
        else {
            panic!("chat")
        };
        let key = conversation_key(&session).unwrap();
        session
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/recorded/account-b".into());
        session
            .launch
            .recipe
            .environment
            .insert("XDG_CONFIG_HOME".into(), "/unrelated/config".into());
        assert_eq!(conversation_key(&session).unwrap(), key);
        assert!(same_conversation(&session, &session));
        session
            .launch
            .recipe
            .environment
            .remove("CLAUDE_CONFIG_DIR");
        let home_only = conversation_key(&session).unwrap();
        session
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/recorded/account-c".into());
        assert_ne!(conversation_key(&session).unwrap(), home_only);
        session.launch.recipe.environment.remove("HOME");
        assert!(conversation_key(&session).is_none());
        session.launch.session_id = None;
        assert!(conversation_key(&session).is_none());
        session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
        for (provider, variable) in [
            (Provider::Claude, "CLAUDE_CONFIG_DIR"),
            (Provider::Codex, "CODEX_HOME"),
        ] {
            session.launch.provider = provider;
            session
                .launch
                .recipe
                .environment
                .insert(variable.into(), ".".into());
            session.launch.recipe.working_directory = "/recorded/project-a".into();
            let first = conversation_key(&session).unwrap();
            session.launch.recipe.working_directory = "/recorded/project-b".into();
            assert_ne!(conversation_key(&session).unwrap(), first);
            assert_eq!(
                session.launch.recipe.working_directory,
                Path::new("/recorded/project-b")
            );
        }
        session
            .launch
            .recipe
            .environment
            .insert("CODEX_HOME".into(), String::new());
        assert_eq!(
            recorded_account_root(Provider::Codex, &session.launch.recipe),
            Some(session.launch.recipe.working_directory.clone())
        );
        session.launch.provider = Provider::Omp;
        session.launch.recipe.arguments = vec![
            "--session-dir".into(),
            "/recorded/omp-a/sessions/native".into(),
        ];
        let omp = conversation_key(&session).unwrap();
        session
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/irrelevant/home".into());
        session
            .launch
            .recipe
            .environment
            .insert("XDG_DATA_HOME".into(), "/irrelevant/data".into());
        assert_eq!(conversation_key(&session).unwrap(), omp);
        session.launch.recipe.arguments[1] = "/recorded/omp-b/sessions/native".into();
        assert_ne!(conversation_key(&session).unwrap(), omp);
        session
            .launch
            .recipe
            .arguments
            .extend(["--profile".into(), "other".into()]);
        assert!(
            conversation_key(&session).is_none(),
            "complex legacy Omp selectors are not guessed"
        );
    }
}

#[cfg(test)]
mod adoption_tests {
    use super::*;

    #[test]
    fn home_only_live_adoption_requires_the_same_recorded_account_context() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut session = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            directory.path().into(),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
        session
            .launch
            .recipe
            .environment
            .remove("CLAUDE_CONFIG_DIR");
        session
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/recorded/home-a".into());
        session.launch.recipe.unset_environment.remove("HOME");
        // Explicit provider home wins over unrelated HOME/XDG changes, so a
        // descriptor cannot create a second writer for that same account.
        let mut explicit = session.clone();
        explicit.launch.recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            "/recorded/selected-claude".into(),
        );
        let selected = SavedConversation {
            card: CardId::new(),
            title: "Selected account".into(),
            session: explicit.clone(),
            open: false,
            updated_at_unix_ms: 1,
        };
        drop(app.resume_saved_conversation(selected.clone()));
        let selected_id = app.workspace.focused().unwrap();
        let count = app.durable.cards.len();
        explicit
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/unrelated/home".into());
        explicit
            .launch
            .recipe
            .environment
            .insert("XDG_CONFIG_HOME".into(), "/unrelated/xdg".into());
        assert_eq!(app.existing_conversation_card(&explicit), Some(selected_id));
        drop(app.resume_saved_conversation(SavedConversation {
            session: explicit,
            ..selected
        }));
        assert_eq!(app.durable.cards.len(), count);

        let entry = SavedConversation {
            card: CardId::new(),
            title: "Home A".into(),
            session,
            open: false,
            updated_at_unix_ms: 1,
        };
        drop(app.resume_saved_conversation(entry.clone()));
        let a = app.workspace.focused().unwrap();
        let mut other = entry.clone();
        other
            .session
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/recorded/home-b".into());
        assert!(app.existing_conversation_card(&other.session).is_none());
        drop(app.resume_saved_conversation(other.clone()));
        let b = app.workspace.focused().unwrap();
        assert_ne!(a, b);
        let count = app.durable.cards.len();
        assert_eq!(app.existing_conversation_card(&entry.session), Some(a));
        assert_eq!(app.existing_conversation_card(&other.session), Some(b));
        drop(app.resume_saved_conversation(entry));
        assert_eq!(app.durable.cards.len(), count);
        // Unknown context supplies no remembered placement, but retains the
        // original live predicate in either direction. Never create a second
        // writer merely because one descriptor predates saved account data.
        other.session.launch.recipe.environment.remove("HOME");
        assert!(conversation_key(&other.session).is_none());
        assert!(same_conversation(&other.session, &other.session));
        assert_eq!(app.existing_conversation_card(&other.session), Some(a));
        other.open = true;
        drop(app.resume_saved_conversation(other.clone()));
        assert_eq!(app.durable.cards.len(), count);
        let mut legacy = crate::app::tests::context_inspector_test_app(false);
        other.open = false;
        drop(legacy.resume_saved_conversation(other.clone()));
        let legacy_id = legacy.workspace.focused().unwrap();
        let mut known = other.clone();
        known
            .session
            .launch
            .recipe
            .environment
            .insert("HOME".into(), "/recorded/home-b".into());
        assert_eq!(
            legacy.existing_conversation_card(&known.session),
            Some(legacy_id)
        );
        drop(legacy.resume_saved_conversation(known));
        assert_eq!(legacy.durable.cards.len(), 1);
        // A distinct externally open conversation still cannot insert.
        other.open = true;
        other.session.launch.session_id = Some("b6c1ef7a-e3c7-4e65-bb7c-79d511f16817".into());
        drop(app.resume_saved_conversation(other));
        assert_eq!(app.durable.cards.len(), count);
        assert!(
            app.resume_ui
                .error
                .as_deref()
                .unwrap()
                .contains("outside this window")
        );
    }
}
