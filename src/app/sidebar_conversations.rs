//! Cached sidebar targets for retained conversations, including hidden tabs.
//!
//! This is the menu adapter for the source-bound End API from modals
//! 0cd1028c2/e10816e7b/21a916a98. The final application must also compose
//! 12da0cb31's captured End pipeline and generic removal source65cce.
use super::*;
use crate::sidebar::conversation::{ConversationEndTarget, Entry};
use crate::workspace_model::views::Scope;

/// Repeated identity is ambiguous even if its two values happen to compare
/// equal. An old menu must not borrow authority from a duplicated owner.
fn unique<K: std::hash::Hash + Eq, V>(map: &mut HashMap<K, Option<V>>, key: K, value: V) {
    match map.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => { entry.insert(Some(value)); }
        std::collections::hash_map::Entry::Occupied(mut entry) => { entry.insert(None); }
    }
}

impl Cagents {
    /// Index filing once, then capture each actual open durable Chat exactly
    /// once. workspace.order intentionally does not participate: Hide removes
    /// a view from the strip while the conversation and its draft survive.
    pub(super) fn capture_conversation_end_targets(&self) -> Vec<Entry> {
        let mut channels = HashMap::new();
        for channel in &self.durable.channels {
            for card in &channel.cards { unique(&mut channels, *card, (channel.id, channel.project_id)); }
        }
        let mut repositories = HashMap::new();
        for channel in &self.workspace_model.channels {
            unique(&mut repositories, channel.id, channel.repository.as_path());
        }
        let mut views = HashMap::new();
        for view in &self.workspace_model.views {
            let repository = match &view.scope {
                Scope::Channel(id) => repositories.get(id).copied().flatten(),
                Scope::Worktree { repository, .. } => Some(repository.as_path()),
            };
            for pane in &view.panes { unique(&mut views, pane.id, (view.scope.clone(), repository)); }
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
            let (shared_scope, shared_repository) = match views.get(&card.id) {
                Some(Some((scope, Some(repository)))) => (Some(scope.clone()), Some((*repository).to_path_buf())),
                Some(_) => return None,
                None => (None, None),
            };
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
                shared_scope, shared_repository,
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
        runtime.replace_draft(widget::text_editor::Content::with_text("retain this draft"));
        Arc::make_mut(&mut runtime.snapshot).busy = true;
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
