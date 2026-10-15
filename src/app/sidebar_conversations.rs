//! Cached sidebar targets for retained conversations, including hidden tabs.
//!
//! This is the menu adapter for the source-bound End API from modals
//! 0cd1028c2/e10816e7b/21a916a98. The final application must also compose
//! 12da0cb31's captured End pipeline and generic removal source65cce, plus
//! 66e77b52b/bdcfc5987 preconfirmation shared-source and filing fences.
use super::*;
use crate::sidebar::conversation::{ConversationEndTarget, Entry};
use crate::workspace_model::views::Scope;

#[cfg(test)]
thread_local! { static PREPARATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
fn preparation_count() -> u64 { PREPARATIONS.with(std::cell::Cell::get) }


/// Repeated identity is ambiguous even if its two values happen to compare
/// equal. An old menu must not borrow authority from a duplicated owner.
fn unique<K: std::hash::Hash + Eq, V>(map: &mut HashMap<K, Option<V>>, key: K, value: V) {
    match map.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => { entry.insert(Some(value)); }
        std::collections::hash_map::Entry::Occupied(mut entry) => { entry.insert(None); }
    }
}

/// The indexed equivalent of automation12856's canonical archive-console
/// presentation predicate. This only admits a cached menu candidate: the click
/// still uses the authoritative canonical helper and original source token.
fn archive_console_matches_binding(
    agent: crate::model::AgentId,
    card: CardId,
    saved: &crate::chat::Session,
    current: &crate::model::TerminalSession,
) -> bool {
    if agent.as_uuid() != card.as_uuid() { return false; }
    let Some(proof) = saved.proof.as_ref().filter(|proof| proof.is_complete()) else { return false; };
    let Some(original) = saved.terminal.as_ref() else { return false; };
    if original.id != saved.id
        || original.launch.arguments != ["agent".to_owned(), "console".to_owned(), agent.to_string()]
        || original.continuity.live_proof().as_ref() != Some(proof) { return false; }
    let mut original = original.clone();
    if matches!(current.continuity,
        crate::model::TerminalContinuity::Exited { .. }
        | crate::model::TerminalContinuity::RelaunchAvailable { last_exit: Some(_), .. }) {
        original.continuity = current.continuity.clone();
    }
    original == *current
}
fn resource(kind: &CardKind) -> Option<(u8, crate::model::TerminalId)> {
    match kind {
        CardKind::Chat(session) => Some((0, session.id)),
        CardKind::Terminal(session) => Some((1, session.id)),
        _ => None,
    }
}

impl Cagents {
    /// Index filing once, then capture each actual open durable Chat exactly
    /// once. workspace.order intentionally does not participate: Hide removes
    /// a view from the strip while the conversation and its draft survive.
    pub(super) fn capture_conversation_end_targets(&self) -> Vec<Entry> {
        #[cfg(test)]
        PREPARATIONS.with(|count| count.set(count.get().wrapping_add(1)));
        let mut channels = HashMap::new();
        for channel in &self.durable.channels {
            for card in &channel.cards { unique(&mut channels, *card, (channel.id, channel.project_id)); }
        }
        let mut repositories = HashMap::new();
        for channel in &self.workspace_model.channels {
            unique(&mut repositories, channel.id, channel.repository.as_path());
        }
        let mut views = HashMap::new();
        let mut resources: HashMap<_, usize> = HashMap::new();
        for view in &self.workspace_model.views {
            let repository = match &view.scope {
                Scope::Channel(id) => repositories.get(id).copied().flatten(),
                Scope::Worktree { repository, .. } => Some(repository.as_path()),
            };
            for pane in &view.panes {
                if let Some(key) = resource(&pane.kind) { *resources.entry(key).or_default() += 1; }
                unique(&mut views, pane.id, (view.scope.clone(), repository, pane));
            }
        }
        let mut cards = HashMap::new();
        for card in &self.durable.cards { unique(&mut cards, card.id, card); }
        self.durable.cards.iter().filter_map(|card| {
            let CardKind::Chat(issued) = &card.kind else { return None; };
            if !card.existence.is_open() || cards.get(&card.id).is_none_or(Option::is_none) { return None; }
            let (channel, channel_project) = match channels.get(&card.id) {
                Some(Some((id, project))) => (Some(*id), Some(*project)),
                Some(None) => return None,
                None => (None, None),
            };
            let (shared_scope, shared_repository, shared_worktree) = match views.get(&card.id) {
                Some(Some((scope, Some(repository), pane))) if !pane.pending_terminal_start
                    && match &pane.kind {
                        CardKind::Chat(current) => terminal_close::same_chat_removal_source(issued, current),
                        CardKind::Terminal(current) => resource(&pane.kind).is_some_and(|key| resources.get(&key) == Some(&1))
                            && self.durable.automation_archives.get(&card.id).copied().is_some_and(|agent|
                                archive_console_matches_binding(agent, card.id, issued, current)),
                        _ => false,
                    } => (Some(scope.clone()), Some((*repository).to_path_buf()), pane.scope.clone()),
                Some(_) => return None,
                None => (None, None, None),
            };
            // Incoming authoritative membership must not be offered under an
            // old durable row while its local filing is still being adopted.
            let consistent = match shared_scope.as_ref() {
                Some(Scope::Channel(id)) => channel == Some(*id),
                Some(Scope::Worktree { path, .. }) => channel.is_none()
                    && self.durable.card_scopes.get(&card.id).is_none_or(|scope| scope == path),
                None => true,
            };
            if !consistent { return None; }
            // The shared accessor's descriptor fallback scans durable cards.
            // We already hold that exact descriptor; reserve the accessor for
            // its constant-time active/parked runtime path.
            let observed = if matches!(self.cards.get(&card.id), Some(CardRuntime::Chat(_))) || self.chat_has_parked_runtime(card.id) {
                self.chat_return_session(card.id)?
            } else { issued.clone() };
            Some(Entry { title: card.title.clone(), target: Arc::new(ConversationEndTarget {
                card: card.id, registry: self.workspace_model.identity,
                issued: Arc::new(issued.clone()), observed: Arc::new(observed),
                channel, channel_project, worktree: self.durable.card_scopes.get(&card.id).cloned(),
                shared_scope, shared_repository, shared_worktree,
            }) })
        }).collect()
    }

    /// Called on meaningful cached-sidebar refresh, never during view. New
    /// snapshots preserve their Arc when parameters and labels are unchanged,
    /// so native menus do not rebind on unrelated terminal wakes.
    pub(super) fn refresh_sidebar_conversations(&mut self) {
        let rows = self.capture_conversation_end_targets();
        let mut channels: HashMap<_, Vec<Entry>> = HashMap::new();
        let mut worktrees: HashMap<_, Vec<Entry>> = HashMap::new();
        // Loaded, exact scopes are navigable. Historical nested boundaries
        // prevent an unloaded worktree's conversations falling into its parent.
        let mut roots = HashSet::new();
        for worktree in &self.durable.worktrees { roots.insert(worktree.path.as_path()); }
        let mut loaded = HashSet::new();
        for project in &self.sidebar_model.projects {
            if project.kind == crate::model::ProjectKind::Folder {
                roots.insert(project.repository.as_path()); loaded.insert(project.repository.as_path());
            }
            for worktree in &project.worktrees { roots.insert(worktree.path.as_path()); loaded.insert(worktree.path.as_path()); }
        }
        for row in rows {
            if let Some(channel) = row.target.channel {
                channels.entry(channel).or_default().push(row);
                continue;
            }
            let scope = if let Some(scope) = row.target.worktree.as_deref() {
                loaded.get(scope).copied()
            } else if let Some(Scope::Worktree { path, .. }) = &row.target.shared_scope {
                loaded.get(path.as_path()).copied()
            } else if matches!(row.target.shared_scope, Some(Scope::Channel(_))) {
                // Shared filing has not been adopted locally yet. Do not
                // reinterpret a channel conversation as a worktree thread.
                None
            } else {
                row.target.issued.launch.recipe.working_directory.ancestors()
                    .find(|root| roots.contains(*root)).and_then(|root| loaded.get(root).copied())
            };
            if let Some(scope) = scope { worktrees.entry(scope.to_path_buf()).or_default().push(row); }
        }
        for project in &mut self.sidebar_model.projects {
            project.conversation_ends.update(worktrees.get(&project.repository).cloned().unwrap_or_default());
            for channel in &mut project.channels {
                channel.conversation_ends.update(channels.remove(&channel.id).unwrap_or_default());
            }
            for worktree in &mut project.worktrees {
                worktree.conversation_ends.update(worktrees.remove(&worktree.path).unwrap_or_default());
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::chat_ui::closing;
    use crate::workspace_widget::GlobalShortcut as Command;

    fn fixture() -> (Cagents, CardId, crate::model::ChannelId, tempfile::TempDir) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        drop(app.new_native_chat(crate::agent::Provider::Codex, Some(directory.path().into())));
        let card = app.workspace.focused().unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else { panic!("native chat fixture"); };
        runtime.session.launch.session_id = Some("retained-sidebar-thread".into());
        let target = runtime.target;
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        drop(app.update(Message::ChatUi(chat_ui::Action::Composer(target,
            widget::text_editor::Action::Edit(widget::text_editor::Edit::Paste(Arc::new("retain this draft".into())))))));
        let CardKind::Chat(issued) = &mut app.card_state_mut(card).unwrap().kind else { panic!("issued descriptor"); };
        issued.launch.session_id = Some("retained-sidebar-thread".into());
        let project = ProjectId::new();
        let channel = crate::model::ChannelId::new();
        app.sidebar_model.projects.push(sidebar::SidebarProject::new(project, crate::repository::RepositoryId::new(),
            "Sidebar fixture".into(), directory.path().into(), Vec::new(), Vec::new()));
        Arc::make_mut(&mut app.durable).channels.push(crate::model::ProjectChannel {
            id: channel, project_id: project, title: "Retained conversations".into(), title_source: Default::default(),
            ordinal: 1, shared: false, cards: vec![card], focused: Some(card),
        });
        app.refresh_sidebar_channels();
        (app, card, channel, directory)
    }
    fn hide(app: &mut Cagents, card: CardId) {
        drop(app.update(Message::CloseCard(card)));
        let target = app.chat_ui.close_dialog.as_ref().expect("draft close confirmation").target();
        let task = app.update(Message::ChatClose(closing::Action::Hide(target)));
        use iced::futures::StreamExt;
        let mut stream = iced::runtime::task::into_stream(task).expect("captured Hide receipt");
        let executor = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let receipt = executor.block_on(async {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    if matches!(message, Message::ChatClose(closing::Action::CommitHide(_))) { return message; }
                }
            }
            panic!("Hide must emit its source-bound completion");
        });
        drop(app.update(receipt));
        assert!(!app.workspace.order().any(|id| id == card));
    }
    fn captured(app: &Cagents, channel: crate::model::ChannelId) -> Arc<ConversationEndTarget> {
        Arc::clone(&app.sidebar_model.projects.iter().flat_map(|project| &project.channels)
            .find(|row| row.id == channel).unwrap().conversation_ends.entries()[0].target)
    }
    fn activate(app: &mut Cagents, target: Arc<ConversationEndTarget>) {
        drop(app.update(Message::Sidebar(sidebar::commands::registered(SidebarAction::EndConversation(target)))));
    }

    #[test]
    fn sidebar_end_hidden_conversation_dispatches_registered_target_without_reopening_and_cancel_keeps_draft() {
        let (mut app, card, channel, _directory) = fixture();
        let source = app.chat_return_session(card).unwrap();
        hide(&mut app, card);
        let target = captured(&app, channel);
        assert_eq!(target.card(), card);
        assert!(app.pane_command_available(card, Command::EndConversation));
        activate(&mut app, target);
        assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().card, card);
        assert!(!app.workspace.order().any(|id| id == card));
        let decision = app.chat_ui.close_dialog.as_ref().unwrap().target();
        drop(app.update(Message::ChatClose(closing::Action::Cancel(decision))));
        assert!(app.chat_ui.close_dialog.is_none());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert_eq!(app.chat_return_session(card), Some(source));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else { panic!("retained original runtime"); };
        assert_eq!(runtime.unsent_text(), "retain this draft");
        assert!(runtime.snapshot.busy);
        assert!(!app.workspace.order().any(|id| id == card));
    }

    #[test]
    fn sidebar_end_old_menu_never_recaptures_replaced_registry_or_channel_owner() {
        for registry in [true, false] {
            let (mut app, card, channel, _directory) = fixture();
            hide(&mut app, card);
            let target = captured(&app, channel);
            if registry { app.workspace_model.identity = uuid::Uuid::new_v4(); }
            else { Arc::make_mut(&mut app.durable).channels[0].project_id = ProjectId::new(); }
            activate(&mut app, target);
            assert!(app.chat_ui.close_dialog.is_none());
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(!app.workspace.order().any(|id| id == card));
        }
    }

    #[test]
    fn sidebar_end_unrelated_typing_and_noop_wakes_do_not_prepare_conversation_catalog() {
        for keep_channel in [true, false] {
            let (mut app, card, _channel, _directory) = fixture();
            if !keep_channel { Arc::make_mut(&mut app.durable).channels.clear(); }
            drop(app.update(Message::Sidebar(SidebarAction::SearchChanged(String::new()))));
            let before = preparation_count();
            let runs = app.update_tail.runs;
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else { panic!("composer"); };
            let target = runtime.target;
            for _ in 0..10 {
                drop(app.update(Message::KeyEvent { event: keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character("t".into()), modified_key: keyboard::Key::Character("t".into()),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyT), location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(), text: Some("t".into()), repeat: false,
                }, captured: true }));
                drop(app.update(Message::ChatUi(chat_ui::Action::Composer(target,
                    widget::text_editor::Action::Edit(widget::text_editor::Edit::Insert('t'))))));
                drop(app.update(Message::KeyEvent { event: keyboard::Event::KeyReleased {
                    key: keyboard::Key::Character("t".into()), modified_key: keyboard::Key::Character("t".into()),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyT), location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                }, captured: false }));
                drop(app.update(Message::ChatUi(chat_ui::Action::ComposerCursor(target,
                    widget::text_editor::Cursor { position: widget::text_editor::Position { line: 0, column: 0 }, selection: None }))));
                drop(app.update(Message::TerminalWake { card: CardId::new(), generation: 0 }));
            }
            assert_eq!(preparation_count(), before, "zero catalog preparation includes the no-channel case");
            assert_eq!(app.update_tail.runs, runs);
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else { panic!("same composer"); };
            let text = runtime.unsent_text();
            assert!(text.contains("retain this draft"));
            assert_eq!(text.len(), "retain this draft".len() + 10);
            // Actual incoming registry identity remains a semantic invalidation.
            let mut model = app.workspace_model.clone(); model.identity = uuid::Uuid::new_v4(); model.sequence += 1;
            drop(app.update(Message::WorkspaceModelObserved(workspace_model_sync::ModelUpdate::Read(model))));
            assert!(preparation_count() > before);
        }
    }

    #[test]
    fn sidebar_end_bulk_waits_for_authoritative_channel_filing_instead_of_using_a_stale_row() {
        let (mut app, card, _channel, directory) = fixture();
        let current = crate::model::ChannelId::new();
        app.workspace_model.channels.push(crate::workspace_model::ModelChannel {
            id: current, repository: directory.path().into(), title: "Current shared filing".into(), title_source: Default::default(), ordinal: 2,
        });
        let pane = crate::workspace_model::views::Pane::from_card(app.card_state(card).unwrap(), None).unwrap();
        app.workspace_model.views.push(crate::workspace_model::views::View { scope: Scope::Channel(current), revision: 1, panes: vec![pane], retired: Vec::new() });
        assert!(app.capture_conversation_end_targets().is_empty(), "never put the current shared conversation under the historical channel");
        Arc::make_mut(&mut app.durable).channels[0].id = current;
        let rows = app.capture_conversation_end_targets();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target.channel, Some(current));
        app.workspace_model.views[0].scope = Scope::Worktree { repository: directory.path().into(), path: directory.path().into() };
        assert!(app.capture_conversation_end_targets().is_empty(), "a shared worktree pane is not a channel conversation");
    }

    #[test]
    fn sidebar_end_bulk_capture_refuses_ambiguous_card_or_filing_without_affecting_other_rows() {
        let (mut app, card, _channel, _directory) = fixture();
        assert_eq!(app.capture_conversation_end_targets().len(), 1);
        Arc::make_mut(&mut app.durable).channels[0].cards.push(card);
        assert!(app.capture_conversation_end_targets().is_empty());
        Arc::make_mut(&mut app.durable).channels[0].cards.pop();
        let duplicate = app.card_state(card).unwrap().clone();
        Arc::make_mut(&mut app.durable).cards.push(duplicate);
        assert!(app.capture_conversation_end_targets().is_empty());
    }
}


#[cfg(test)]
mod shared_capture_tests {
    use super::*;
    #[test]
    fn sidebar_end_bulk_source_check_refuses_same_scope_replacement_before_local_adoption() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        drop(app.new_native_chat(crate::agent::Provider::Codex, Some(directory.path().into())));
        let card = app.workspace.focused().unwrap();
        let issued = app.card_state(card).unwrap().clone();
        let pane = crate::workspace_model::views::Pane::from_card(&issued, None).unwrap();
        app.workspace_model.views.push(crate::workspace_model::views::View {
            scope: Scope::Worktree { repository: directory.path().into(), path: directory.path().into() },
            revision: 1, panes: vec![pane], retired: Vec::new(),
        });
        assert_eq!(app.capture_conversation_end_targets().len(), 1);
        let CardKind::Chat(shared) = &mut app.workspace_model.views[0].panes[0].kind else { panic!("shared source"); };
        shared.launch.session_id = Some("replacement-before-local-adoption".into());
        assert!(app.capture_conversation_end_targets().is_empty());
        app.workspace_model.views[0].panes[0].kind = issued.kind;
        app.workspace_model.views[0].panes[0].pending_terminal_start = true;
        assert!(app.capture_conversation_end_targets().is_empty());
    }
}


#[cfg(test)]
mod archive_candidate_tests {
    use super::*;
    #[test]
    fn sidebar_end_archive_candidate_requires_original_canonical_console_recipe_and_complete_proof() {
        let agent = crate::model::AgentId::new();
        let card = CardId::from_uuid(agent.as_uuid());
        let proof = crate::model::HostSessionProof { host_session_id: crate::model::HostSessionId::new("archive-owner"),
            boot_id: crate::model::BootId::new("archive-boot"), host_incarnation: crate::model::HostIncarnation::new("archive-host") };
        let id = crate::model::TerminalId::new();
        let console = crate::model::TerminalSession { id, owned_codex_listener: None, recovery_origin: None, predecessor: None,
            launch: crate::model::LaunchRecipe { program: "cagents".into(), arguments: vec!["agent".into(), "console".into(), agent.to_string()],
                working_directory: "/original".into(), environment: Default::default(), unset_environment: Default::default() },
            transcript_path: None, resume_hint: None, continuity: crate::model::TerminalContinuity::Live {
                host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(), host_incarnation: proof.host_incarnation.clone() } };
        let mut saved = crate::chat::Session { id, launch: crate::chat::Launch { provider: crate::agent::Provider::Codex,
            recipe: console.launch.clone(), session_id: Some("exact-archived-thread".into()) }, proof: Some(proof), terminal: Some(console.clone()),
            owned_codex_listener: None, native_continuity: Default::default() };
        assert!(archive_console_matches_binding(agent, card, &saved, &console));
        let mut ended = console.clone(); ended.continuity = crate::model::TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit { code: Some(0), signal: None, occurred_at_unix_ms: 1234 } };
        assert!(archive_console_matches_binding(agent, card, &saved, &ended));
        assert!(!archive_console_matches_binding(crate::model::AgentId::new(), card, &saved, &ended));
        ended.launch.working_directory = "/replaced".into();
        assert!(!archive_console_matches_binding(agent, card, &saved, &ended));
        saved.proof = None;
        assert!(!archive_console_matches_binding(agent, card, &saved, &console));
    }
}
