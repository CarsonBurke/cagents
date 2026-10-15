//! Hiding a view and ending the conversation are separate operations.
use super::*;

#[derive(Debug, Clone, Copy)]
pub enum Action {
    End(CardId),
    Cancel(DialogTarget),
    Confirm(DialogTarget),
    Hide(DialogTarget),
    CommitHide(terminal_close::RemovalTarget),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogTarget {
    card: CardId,
    generation: u64,
}
pub(in crate::app) struct Confirmation {
    pub card: CardId,
    pub tab: bool,
    generation: u64,
    source: CloseSource,
    batch: Option<uuid::Uuid>,
    sidebar_target: Option<Arc<sidebar::ConversationEndTarget>>,
}

/// The issued descriptor and live logical identity observed at consent time.
/// Process roster changes do not change either identity. A replacement issued
/// descriptor requires a correlated recovery receipt before consent can follow.
#[derive(Clone)]
struct CloseSource {
    registry: uuid::Uuid,
    descriptor: chat::Session,
    session: chat::Session,
}
/// One End request follows only its captured logical source and selection.
/// Cleanup results still settle their phase after this consent is revoked.
#[derive(Clone)]
pub(super) struct EndRequest {
    registry: uuid::Uuid,
    pub(super) batch: Option<uuid::Uuid>,
    kind: CardKind,
    pub(super) session: Option<chat::Session>,
    shared_source: Option<shared_views::RemovalSource>,
    pub(super) paired_terminal: Option<terminal_close::QueuedCardClose>,
}
impl EndRequest {
    pub(super) fn capture(app: &mut Cagents, card: CardId, batch: Option<uuid::Uuid>) -> Option<Self> {
        let kind = app.card_state(card).filter(|state| state.existence.is_open())?.kind.clone();
        let session = app.chat_return_session(card);
        let shared_source = match app.capture_shared_removal_source(card) {
            Ok(source) => source,
            Err(error) => {
                app.stop_pane_close_batch(batch);
                app.status = Some(TransientNotice::warning(error));
                return None;
            }
        };
        let paired_terminal = if matches!(&kind, CardKind::Terminal(_)) {
            Some(app.capture_card_close_for(card, batch)?)
        } else { None };
        Some(Self { registry: app.workspace_model.identity, batch, kind, session, shared_source, paired_terminal })
    }
    pub(super) fn archived_metadata_only(&self) -> bool {
        self.shared_source.as_ref().is_some_and(shared_views::RemovalSource::is_archived_automation)
    }
    pub(super) fn source_current(&self, app: &Cagents, card: CardId) -> bool {
        if self.registry != app.workspace_model.identity { return false; }
        let Some(state) = app.card_state(card).filter(|state| state.existence.is_open()) else { return false; };
        let same_kind = match (&self.kind, &state.kind) {
            (CardKind::Chat(expected), CardKind::Chat(current)) => terminal_close::same_chat_removal_source(expected, current),
            (CardKind::Terminal(expected), CardKind::Terminal(current)) => {
                let proof_matches = current.continuity.live_proof().is_none_or(|proof| expected.continuity.live_proof() == Some(proof));
                let mut current = current.clone();
                current.continuity = expected.continuity.clone();
                current == *expected && proof_matches
            }
            _ => false,
        };
        same_kind && match (&self.session, app.chat_return_session(card)) {
            (Some(expected), Some(current)) => terminal_close::same_chat_removal_source(expected, &current),
            (None, None) => true,
            _ => false,
        }
    }
    /// Only the initial End intake captures publication authority. Installing
    /// it after cleanup verifies that same basis and never adopts a successor.
    pub(super) fn install_removal_source(&self, app: &mut Cagents, card: CardId) -> bool {
        if !self.current(app, card) || !app.install_shared_removal_source(card, self.shared_source.clone()) {
            app.stop_pane_close_batch(self.batch);
            return false;
        }
        true
    }
    pub(super) fn current(&self, app: &Cagents, card: CardId) -> bool {
        let original_membership_current = match &self.shared_source {
            Some(source) => source.current(&app.workspace_model),
            None => !app.workspace_model.views.iter().any(|view| view.panes.iter().any(|pane| pane.id == card))
                && !app.shared_views.attached.contains(&card)
                && !app.shared_views.baseline.values().any(|panes| panes.iter().any(|pane| pane.id == card)),
        };
        self.source_current(app, card) && original_membership_current
            && app.pane_close_decision_current(card, self.batch)
    }
}

impl Confirmation {
    pub(in crate::app) fn generation(&self) -> u64 {
        self.generation
    }
    pub(in crate::app) fn target(&self) -> DialogTarget {
        DialogTarget {
            card: self.card,
            generation: self.generation,
        }
    }
}

pub(in crate::app) fn draft_key(session: &chat::Session) -> Option<String> {
    let id = session.launch.session_id.as_deref()?.trim();
    if id.is_empty() {
        return None;
    }
    Some(format!("{}:{id}", session.launch.provider.label()))
}

impl Cagents {
    fn conversation_end_placement(&self, card: CardId) -> Option<(Option<crate::model::ChannelId>, Option<ProjectId>, Option<PathBuf>, Option<crate::workspace_model::views::Scope>, Option<PathBuf>, Option<PathBuf>)> {
        let mut channels = self.durable.channels.iter().filter(|channel| channel.cards.contains(&card));
        let (channel, channel_project) = channels.next().map_or((None, None), |channel| (Some(channel.id), Some(channel.project_id)));
        if channels.next().is_some() { return None; }
        let mut views = self.workspace_model.views.iter().filter(|view| view.panes.iter().any(|pane| pane.id == card));
        let shared_view = views.next();
        let shared_scope = shared_view.map(|view| view.scope.clone());
        let shared_worktree = shared_view.and_then(|view| view.panes.iter().find(|pane| pane.id == card)).and_then(|pane| pane.scope.clone());
        if views.next().is_some() { return None; }
        let shared_repository = match shared_scope.as_ref() {
            Some(crate::workspace_model::views::Scope::Channel(id)) => Some(self.workspace_model.channel(*id)?.repository.clone()),
            Some(crate::workspace_model::views::Scope::Worktree { repository, .. }) => Some(repository.clone()),
            None => None,
        };
        Some((channel, channel_project, self.durable.card_scopes.get(&card).cloned(), shared_scope, shared_repository, shared_worktree))
    }
    /// Capture when constructing a lazy, scoped sidebar menu, not in response
    /// to its later click. The original issued and observed identity are kept
    /// separately, including the legitimate unlearned/learned thread pair.
    pub(in crate::app) fn capture_conversation_end_target(&self, card: CardId) -> Option<Arc<sidebar::ConversationEndTarget>> {
        let source = self.chat_close_source(card)?;
        let (channel, channel_project, worktree, shared_scope, shared_repository, shared_worktree) = self.conversation_end_placement(card)?;
        if !self.conversation_end_shared_source_current(card, &source.descriptor, shared_scope.as_ref()) { return None; }
        Some(Arc::new(sidebar::ConversationEndTarget {
            card, registry: source.registry, issued: Arc::new(source.descriptor), observed: Arc::new(source.session),
            channel, channel_project, worktree, shared_scope, shared_repository, shared_worktree,
        }))
    }
    /// The registry can replace a pane before the local descriptor adopts it.
    /// Consent must still refer to the original authoritative issued source.
    pub(in crate::app) fn conversation_end_shared_source_current(&self, card: CardId, issued: &chat::Session, expected_scope: Option<&crate::workspace_model::views::Scope>) -> bool {
        let mut matches = self.workspace_model.views.iter().flat_map(|view| {
            view.panes.iter().filter(move |pane| pane.id == card).map(move |pane| (&view.scope, pane))
        });
        let first = matches.next();
        if matches.next().is_some() { return false; }
        match (expected_scope, first) {
            (None, None) => true,
            (Some(expected), Some((scope, pane))) => scope == expected && !pane.pending_terminal_start
                && match &pane.kind {
                    CardKind::Chat(current) => terminal_close::same_chat_removal_source(issued, current),
                    // A saved automation console has an explicitly persisted,
                    // proof-checked read-only projection. This grants no live
                    // execution authority and never accepts an arbitrary kind
                    // conversion or a freshly looked-up terminal source.
                    CardKind::Terminal(_) => self.canonical_automation_archive_pane(card).as_ref() == Some(pane),
                    _ => false,
                },
            _ => false,
        }
    }
    pub(in crate::app) fn conversation_end_target_current(&self, target: &sidebar::ConversationEndTarget) -> bool {
        target.registry == self.workspace_model.identity
            && self.conversation_end_shared_source_current(target.card, &target.issued, target.shared_scope.as_ref())
            && self.card_state(target.card).is_some_and(|card| card.existence.is_open()
                && matches!(&card.kind, CardKind::Chat(issued) if terminal_close::same_chat_removal_source(&target.issued, issued)))
            && self.chat_return_session(target.card).as_ref().is_some_and(|observed| terminal_close::same_chat_removal_source(&target.observed, observed))
            && self.conversation_end_placement(target.card).is_some_and(|(channel, project, worktree, scope, repository, shared_worktree)| channel == target.channel && project == target.channel_project && worktree == target.worktree && scope == target.shared_scope && repository == target.shared_repository && shared_worktree == target.shared_worktree)
    }
    /// Existing command-template dispatch supplies its captured parameters;
    /// never reconstruct authority from the current row/CardId on delivery.
    pub(in crate::app) fn request_conversation_end(&mut self, target: Arc<sidebar::ConversationEndTarget>) -> Task<Message> {
        if self.update_prepared || self.modal_is_open() { return Task::none(); }
        if !self.conversation_end_target_current(&target) {
            self.status = Some(TransientNotice::warning("The original conversation or its sidebar scope changed. Its current source was kept."));
            return Task::none();
        }
        let task = self.update_chat_close(Action::End(target.card));
        if let Some(dialog) = self.chat_ui.close_dialog.as_mut().filter(|dialog| dialog.card == target.card) {
            dialog.sidebar_target = Some(target);
        }
        task
    }

    fn chat_close_source(&self, card: CardId) -> Option<CloseSource> {
        let state = self.card_state(card)?;
        let CardKind::Chat(descriptor) = &state.kind else {
            return None;
        };
        state.existence.is_open().then(|| CloseSource {
            registry: self.workspace_model.identity,
            descriptor: descriptor.clone(),
            session: self
                .chat_return_session(card)
                .unwrap_or_else(|| descriptor.clone()),
        })
    }
    fn chat_close_source_matches(&self, dialog: &Confirmation) -> bool {
        dialog.sidebar_target.as_ref().is_none_or(|target| self.conversation_end_target_current(target))
            && dialog.source.registry == self.workspace_model.identity && self.card_state(dialog.card).is_some_and(|state| {
            state.existence.is_open()
                && matches!(&state.kind, CardKind::Chat(current) if current == &dialog.source.descriptor)
        }) && self.chat_return_session(dialog.card).as_ref() == Some(&dialog.source.session)
            && self.pane_close_decision_current(dialog.card, dialog.batch)
    }
    pub(in crate::app) fn chat_close_backing_current(&self) -> bool {
        self.chat_ui
            .close_dialog
            .as_ref()
            .is_some_and(|dialog| self.chat_close_source_matches(dialog))
    }
    pub(in crate::app) fn reconcile_chat_close_backing_state(&mut self) {
        if self
            .chat_ui
            .close_dialog
            .as_ref()
            .is_some_and(|dialog| !self.chat_close_source_matches(dialog))
        {
            if let Some(dialog) = self.chat_ui.close_dialog.take() {
                self.stop_pane_close_batch(dialog.batch);
            }
        }
    }
    pub(in crate::app) fn remember_conversation_draft(&mut self, card: CardId) {
        let Some(session) = self.chat_return_session(card) else {
            return;
        };
        let Some(key) = draft_key(&session) else {
            return;
        };
        let draft = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.unsent_text()),
            _ => self
                .chat_ui
                .parked
                .get(&card)
                .map(|runtime| runtime.unsent_text())
                .or_else(|| self.chat_ui.drafts.get(&card).cloned()),
        };
        let Some(draft) = draft else {
            return;
        };
        let model = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(&**runtime),
            _ => self.chat_ui.parked.get(&card).map(|runtime| &**runtime),
        }
        .and_then(|runtime| {
            runtime
                .snapshot
                .selected_model
                .clone()
                .or_else(|| runtime.recorded_model.clone())
        });
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&card))
            .map(|channel| channel.id);
        let durable = Arc::make_mut(&mut self.durable);
        if let Some(channel) = channel {
            durable.conversation_channels.insert(key.clone(), channel);
        }
        if let Some(model) = model {
            durable.conversation_models.insert(key.clone(), model);
        }
        let saved = &mut durable.conversation_drafts;
        if draft.is_empty() {
            saved.remove(&key);
        } else {
            saved.insert(key, draft);
        }
    }
    fn chat_close_needs_confirmation(&self, card: CardId) -> bool {
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(&**runtime),
            _ => self.chat_ui.parked.get(&card).map(|runtime| &**runtime),
        };
        self.chat_ui.phases.contains_key(&card)
            || runtime.is_some_and(|runtime| {
                runtime.snapshot.busy
                    || runtime.handle().is_some_and(|handle| {
                        !runtime.visible.load(Ordering::Relaxed) || handle.snapshot().busy
                    })
                    || !runtime.unsent_text().is_empty()
                    || !runtime.attachments.is_empty()
                    || runtime.queued.is_some()
                    || runtime.pending.is_some()
                    || !runtime.followups.is_empty()
            })
            || self
                .chat_ui
                .drafts
                .get(&card)
                .is_some_and(|draft| !draft.is_empty())
    }
    pub(in crate::app) fn request_chat_tab_close(&mut self, card: CardId) -> Task<Message> {
        self.request_chat_tab_close_for(card, None)
    }
    pub(in crate::app) fn request_chat_tab_close_for(&mut self, card: CardId, batch: Option<uuid::Uuid>) -> Task<Message> {
        if !self.pane_close_decision_current(card, batch) { return Task::none(); }
        if self.chat_is_unused(card) {
            return self.begin_chat_close_for(card, batch);
        }
        if self.chat_close_needs_confirmation(card) {
            return self.open_chat_close_confirmation(card, true, batch);
        }
        self.hide_chat_tab_for(card, batch)
    }
    fn open_chat_close_confirmation(&mut self, card: CardId, tab: bool, batch: Option<uuid::Uuid>) -> Task<Message> {
        if !self.pane_close_decision_current(card, batch) { return Task::none(); }
        if self.chat_ui.close_dialog.is_some() {
            self.stop_pane_close_batch(batch);
            return Task::none();
        }
        let Some(source) = self.chat_close_source(card) else {
            return Task::none();
        };
        self.chat_ui.close_generation = self.chat_ui.close_generation.wrapping_add(1);
        self.chat_ui.close_dialog = Some(Confirmation {
            card,
            tab,
            generation: self.chat_ui.close_generation,
            source,
            batch,
            sidebar_target: None,
        });
        self.modal_focus(widget::Id::new("chat-close.cancel"))
    }
    pub(in crate::app) fn cancel_chat_close(&mut self) -> Task<Message> {
        let Some(target) = self.chat_ui.close_dialog.as_ref().map(Confirmation::target) else {
            return Task::none();
        };
        self.update_chat_close(Action::Cancel(target))
    }
    pub(in crate::app) fn update_chat_close(&mut self, action: Action) -> Task<Message> {
        if self.update_prepared && !matches!(action, Action::CommitHide(_)) {
            return Task::none();
        }
        if matches!(
            action,
            Action::Cancel(_) | Action::Confirm(_) | Action::Hide(_)
        ) && self.top_modal() != Some(modal_ui::Modal::ChatClose)
        {
            return Task::none();
        }
        let target = match action {
            Action::Cancel(target) | Action::Confirm(target) | Action::Hide(target) => Some(target),
            _ => None,
        };
        if target.is_some_and(|target| {
            self.chat_ui.close_dialog.as_ref().is_none_or(|dialog| {
                dialog.target() != target || !self.chat_close_source_matches(dialog)
            })
        }) {
            return Task::none();
        }
        match action {
            Action::End(card) => {
                if self.chat_close_source(card).is_none() {
                    return Task::none();
                }
                if self.chat_close_needs_confirmation(card) {
                    self.open_chat_close_confirmation(card, false, None)
                } else {
                    self.begin_chat_close(card)
                }
            }
            Action::Cancel(_) => {
                let Some(dialog) = self.chat_ui.close_dialog.take() else {
                    return Task::none();
                };
                self.stop_pane_close_batch(dialog.batch);
                let restore =
                    resolve_restore_card(Some(dialog.card), self.workspace.focused(), |card| {
                        self.workspace.order().any(|id| id == card)
                    });
                let target = restore
                    .map(|card| self.card_keyboard_focus(card))
                    .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID));
                self.modal_restore_focus(target)
            }
            Action::Confirm(_) => {
                let Some(dialog) = self.chat_ui.close_dialog.take() else {
                    return Task::none();
                };
                self.begin_chat_close_for(dialog.card, dialog.batch)
            }
            Action::Hide(_) => {
                let Some(dialog) = self.chat_ui.close_dialog.take() else {
                    return Task::none();
                };
                self.hide_chat_tab_for(dialog.card, dialog.batch)
            }
            Action::CommitHide(target) => {
                let card = target.card();
                if self.pending_card_removal_target(card) != Some(target) { return Task::none(); }
                if !self.card_removal_current(target) {
                    if let Some(batch) = self.retire_card_removal(target) {
                        let _ = self.workspace.cancel_close(card);
                        self.stop_pane_close_batch(batch);
                    }
                    return Task::none();
                }
                let update = match self.workspace.commit_close(card) {
                    Ok(update) => update,
                    Err(error) => {
                        if let Some(batch) = self.retire_card_removal(target) {
                            let _ = self.workspace.cancel_close(card);
                            self.stop_pane_close_batch(batch);
                        }
                        self.status = Some(TransientNotice::warning(error.to_string()));
                        return Task::none();
                    }
                };
                let Some(batch) = self.retire_card_removal(target) else { return Task::none(); };
                self.remember_conversation_draft(card);
                if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
                    let _ = runtime.set_visible(false);
                }
                // Hiding retires one presentation step, preserving the open
                // conversation, its sidebar membership and provider ownership.
                let viewport = self.apply_viewport(update, true, true);
                let next = self.finish_selected_pane_close(card, batch);
                Task::batch([viewport, next])
            }
        }
    }
    fn hide_chat_tab_for(&mut self, card: CardId, batch: Option<uuid::Uuid>) -> Task<Message> {
        let Some(handoff) = self.reserve_card_removal_for(card, batch) else { return Task::none(); };
        let Some(target) = self.pending_card_removal_target(card) else { return Task::none(); };
        workspace_widget::focus_handoff_then(
            handoff,
            widget::Id::new(TOOLBAR_FOCUS_ID),
            |next| self.card_keyboard_focus(next),
            Message::ChatClose(Action::CommitHide(target)),
        )
        .map(cosmic::Action::App)
    }
    /// End of an actually hidden chat has no focus handoff to reserve. The
    /// caller validates its captured logical source after actor cleanup, and
    /// this synchronous commit retains the usual expected-revision view edit.
    pub(super) fn remove_hidden_chat_card_for(&mut self, card: CardId, batch: Option<uuid::Uuid>) -> Option<Task<Message>> {
        if self.workspace.order().any(|id| id == card)
            || self.pending_card_removal_target(card).is_some()
            || !self.pane_close_decision_current(card, batch)
        { return None; }
        let saved = self.card_state(card).filter(|state| state.existence.is_open() && matches!(&state.kind, CardKind::Chat(_)))?.clone();
        let destination = self.durable.channels.iter().find(|channel| channel.cards.contains(&card))
            .map(|channel| WorkspaceDestination::Channel(channel.id))
            .or_else(|| {
                let path = self.durable.card_scopes.get(&card)?;
                self.sidebar_model.projects.iter().find_map(|project| {
                    project.worktrees.iter().find(|worktree| &worktree.path == path)
                        .map(|worktree| WorkspaceDestination::Worktree(worktree.id))
                        .or_else(|| (project.kind == ProjectKind::Folder && &project.repository == path).then_some(WorkspaceDestination::Folder(project.id)))
                })
            });
        self.remember_conversation_draft(card);
        // Retirement validates the installed original shared basis while the
        // observed runtime is still available. Do not discard that identity
        // before the source-owned publication check has accepted the removal.
        let retired = self.retire_offstrip_card(card);
        if self.card_state(card).is_some_and(|state| state.existence.is_open()) {
            self.stop_pane_close_batch(batch);
            return Some(retired);
        }
        self.recently_closed.push((saved, destination));
        if self.recently_closed.len() > 32 { self.recently_closed.remove(0); }
        self.chat_discard(card);
        let next = self.finish_selected_pane_close(card, batch);
        Some(Task::batch([retired, next]))
    }
    pub(in crate::app) fn chat_close_dialog(&self) -> Option<Element<'_, Message>> {
        let dialog = self.chat_ui.close_dialog.as_ref()?;
        let target = dialog.target();
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title("Close conversation?").into(),
            notice_body(
                if self
                    .chat_return_session(dialog.card)
                    .as_ref()
                    .and_then(draft_key)
                    .is_some()
                {
                    "Closing ends the agent's running turn. Unsent text is saved for this conversation and restored on resume. Attached files must be added again."
                } else if dialog.tab {
                    "This conversation has not received a provider session ID. Closing discards its unsent draft; cancel or hide the tab to keep it."
                } else {
                    "This conversation has not received a provider session ID. Closing discards its unsent draft; cancel to keep it."
                },
            ),
        ];
        if dialog.tab {
            rows.push(
                crate::ui_controls::button::secondary("Hide tab and keep conversation")
                    .id(widget::Id::new("chat-close.hide"))
                    .on_press(Message::ChatClose(Action::Hide(target)))
                    .into(),
            );
        }
        rows.push(dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("chat-close.cancel"))
                .on_press(Message::ChatClose(Action::Cancel(target))),
            crate::ui_controls::button::destructive("Close conversation")
                .id(widget::Id::new("chat-close.confirm"))
                .on_press(Message::ChatClose(Action::Confirm(target))),
        ));
        Some(dialog_ui::overlay(
            dialog_ui::surface(widget::column::with_children(rows).spacing(space::M), 480.0),
            Some(Message::ChatClose(Action::Cancel(target))),
            1.0,
        ))
    }
}

pub(in crate::app) fn recipe_model(recipe: &LaunchRecipe) -> Option<String> {
    recipe
        .arguments
        .iter()
        .enumerate()
        .find_map(|(index, arg)| {
            arg.strip_prefix("--model=").map(str::to_owned).or_else(|| {
                (arg == "--model" || arg == "-m")
                    .then(|| recipe.arguments.get(index + 1).cloned())
                    .flatten()
            })
        })
}
pub(in crate::app) fn clear_model(recipe: &mut LaunchRecipe) {
    let mut index = 0;
    while index < recipe.arguments.len() {
        if recipe.arguments[index] == "--model" || recipe.arguments[index] == "-m" {
            let end = (index + 2).min(recipe.arguments.len());
            recipe.arguments.drain(index..end);
        } else if recipe.arguments[index].starts_with("--model=") {
            recipe.arguments.remove(index);
        } else {
            index += 1;
        }
    }
}
pub(in crate::app) fn set_model(recipe: &mut LaunchRecipe, model: &str) {
    clear_model(recipe);
    recipe
        .arguments
        .extend(["--model".into(), model.to_owned()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Cagents, CardId, tempfile::TempDir) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        drop(app.new_native_chat(Provider::Codex, Some(directory.path().into())));
        let card = app.workspace.focused().unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("draft chat");
        };
        runtime.session.launch.session_id = Some("saved-conversation".into());
        if let Some(state) = app.card_state_mut(card) {
            if let CardKind::Chat(session) = &mut state.kind {
                session.launch.session_id = Some("saved-conversation".into());
            }
        }
        (app, card, directory)
    }
    fn respond(app: &mut Cagents, action: fn(DialogTarget) -> Action) {
        let target = app.chat_ui.close_dialog.as_ref().unwrap().target();
        drop(app.update_chat_close(action(target)));
    }
    fn write(app: &mut Cagents, card: CardId, text: &str, busy: bool) {
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("chat");
        };
        runtime.replace_draft(text_editor::Content::with_text(text));
        Arc::make_mut(&mut runtime.snapshot).busy = busy;
    }
    #[test]
    fn prepared_update_blocks_new_end_confirmations_but_admits_hide_completion() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "retained prompt", true);
        app.update_prepared = true;
        drop(app.update(Message::ChatClose(Action::End(card))));
        assert!(app.chat_ui.close_dialog.is_none());
        app.update_prepared = false;
        drop(app.update(Message::ChatClose(Action::End(card))));
        assert!(app.chat_ui.close_dialog.is_some());
        app.update_prepared = true;
        respond(&mut app, Action::Confirm);
        assert!(app.chat_ui.close_dialog.is_some());
        assert!(app.card_state(card).unwrap().existence.is_open());
        app.update_prepared = false;
        respond(&mut app, Action::Hide);
        let target = app.pending_card_removal_target(card).expect("issued Hide reservation");
        app.update_prepared = true;
        assert!(!super::super::super::user_message_blocked_during_update(
            &Message::ChatClose(Action::CommitHide(target))
        ));
        drop(app.update(Message::ChatClose(Action::CommitHide(target))));
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(app.card_state(card).unwrap().existence.is_open());
    }

    fn emitted_confirm(app: &Cagents) -> Message {
        emitted_control(app, "chat-close.confirm", |message| matches!(message, Message::ChatClose(Action::Confirm(_))))
    }
    fn emitted_control(app: &Cagents, id: &'static str, expected: fn(&Message) -> bool) -> Message {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Tree, operation},
        };
        let renderer = crate::app::tests::headless_renderer();
        let mut dialog = app.dialog().expect("visible conversation confirmation");
        let mut tree = Tree::new(&dialog);
        let bounds = iced::Rectangle::with_size(iced::Size::new(800.0, 600.0));
        let node = dialog.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()),
        );
        dialog.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(widget::Id::new(id)),
        );
        let mut messages = Vec::new();
        dialog.as_widget_mut().update(
            &mut tree,
            &iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            }),
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        messages.into_iter().find(|message| matches!(message,
            Message::ModalSurface { message, .. } if expected(message)))
            .expect("actual dialog control publishes its captured decision")
    }

    fn output(task: Task<Message>, expected: fn(&Message) -> bool) -> Message {
        use iced::futures::StreamExt;
        let mut stream = iced::runtime::task::into_stream(task).expect("task has an output");
        let executor = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        executor.block_on(async move {
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                    assert!(expected(&message), "unexpected application output: {message:?}");
                    return message;
                }
                // These ownership tests do not execute native focus operations.
            }
            panic!("expected application output");
        })
    }
    fn hide_decision(app: &Cagents) -> Message {
        emitted_control(app, "chat-close.hide", |message| matches!(message, Message::ChatClose(Action::Hide(_))))
    }
    fn cancel_decision(app: &Cagents) -> Message {
        emitted_control(app, "chat-close.cancel", |message| matches!(message, Message::ChatClose(Action::Cancel(_))))
    }
    fn hide_receipt(task: Task<Message>) -> Message {
        output(task, |message| matches!(message, Message::ChatClose(Action::CommitHide(_))))
    }
    fn queued(task: Task<Message>) -> Message {
        output(task, |message| matches!(message, Message::CloseQueuedCard(_)))
    }
    fn second_chat(app: &mut Cagents, directory: &Path, name: &str) -> CardId {
        drop(app.new_native_chat(Provider::Codex, Some(directory.into())));
        let card = app.workspace.focused().unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else { panic!("chat"); };
        runtime.session.launch.session_id = Some(name.into());
        let CardKind::Chat(session) = &mut app.card_state_mut(card).unwrap().kind else { panic!("descriptor"); };
        session.launch.session_id = Some(name.into());
        write(app, card, name, false);
        card
    }
    fn retain_in_channel(app: &mut Cagents, directory: &Path, cards: Vec<CardId>) -> crate::model::ChannelId {
        let project = ProjectId::new();
        let channel = crate::model::ChannelId::new();
        app.sidebar_model.projects.push(crate::sidebar::SidebarProject::new(project, crate::repository::RepositoryId::new(),
            "Recorded project".into(), directory.into(), Vec::new(), Vec::new()));
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel { id: channel, project_id: project, title: "Retained conversation".into(),
            title_source: Default::default(), ordinal: 0, shared: false, cards, focused: None });
        durable.active_channel = Some(channel);
        channel
    }
    fn attach_durable_owner(app: &mut Cagents, card: CardId) -> chat::Session {
        let mut session = app.chat_return_session(card).unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("modal-close-owner"),
            boot_id: crate::model::BootId::new("modal-close-boot"),
            host_incarnation: crate::model::HostIncarnation::new("modal-close-incarnation"),
        };
        // Allocate the same owned endpoint shape as production before the
        // fixture learns its thread. No socket or daemon process is started.
        let thread = session.launch.session_id.take();
        assert!(session.claim_owned_codex_listener(Path::new("/modal-fixture-host"), &proof));
        session.launch.session_id = thread;
        session.proof = Some(proof);
        session.native_continuity = chat::NativeContinuity::Durable { provider_version: "1.0".into() };
        assert!(session.has_valid_native_continuity());
        app.card_state_mut(card).unwrap().kind = CardKind::Chat(session.clone());
        let handle = Arc::new(ClientHandle::fixture(session.clone(), Snapshot {session_id: session.launch.session_id.clone(), ready: true, ..Snapshot::default()}));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else { panic!("runtime"); };
        runtime.session = session.clone();
        runtime.connection = Connection::Live(handle);
        session
    }
    fn pending_end(app: &Cagents, card: CardId) -> Target {
        match app.chat_ui.phases.get(&card) {
            Some(Phase::Closing {target, restart: None}) => *target,
            _ => panic!("exact End cleanup phase"),
        }
    }

    #[test]
    fn selected_chat_hide_retires_only_its_step_and_cancel_keeps_the_remaining_conversations() {
        let (mut app, first, directory) = fixture();
        write(&mut app, first, "first draft", false);
        let second = second_chat(&mut app, directory.path(), "second conversation");
        retain_in_channel(&mut app, directory.path(), vec![first, second]);
        let first_delivery = queued(app.close_cards_in_order(vec![first, second]));
        let batch = app.pane_close_batch_for(first);
        assert!(batch.is_some());
        drop(app.update(first_delivery));
        assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().card, first);
        let decision = hide_decision(&app);
        let receipt = hide_receipt(app.update(decision));
        let next = queued(app.update(receipt));
        assert!(!app.workspace.order().any(|id| id == first));
        assert!(app.card_state(first).unwrap().existence.is_open());
        assert!(app.cards.contains_key(&first));
        assert!(app.durable.channels[0].cards.contains(&first));
        let first_key = draft_key(&app.chat_return_session(first).unwrap()).unwrap();
        assert_eq!(app.durable.conversation_drafts[&first_key], "first draft");
        assert!(!app.pane_close_decision_current(first, batch));
        assert!(app.pane_close_decision_current(second, batch));
        drop(app.update(next));
        assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().card, second);
        let cancel = cancel_decision(&app);
        drop(app.update(cancel));
        assert!(!app.project_recovery_ui.closing_panes);
        assert!(app.queued_card_closes.is_empty());
        assert!(app.card_state(first).unwrap().existence.is_open());
        assert!(app.card_state(second).unwrap().existence.is_open());
    }

    #[test]
    fn stale_hide_completion_cannot_consume_a_new_batch_or_reservation_of_the_same_chat() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "saved draft", false);
        let first = queued(app.close_cards_in_order(vec![card]));
        drop(app.update(first));
        let decision = hide_decision(&app);
        let old_receipt = hide_receipt(app.update(decision));
        let old = app.pane_close_batch_for(card);
        let newer = queued(app.close_cards_in_order(vec![card]));
        let current = app.pane_close_batch_for(card);
        assert_ne!(old, current);
        drop(app.update(newer));
        let generation = app.chat_ui.close_dialog.as_ref().unwrap().generation();
        drop(app.update(old_receipt.clone()));
        assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().generation(), generation);
        assert!(app.pane_close_decision_current(card, current));
        assert!(app.workspace.order().any(|id| id == card));
        assert!(app.pending_card_removal_target(card).is_none());
        let decision = hide_decision(&app);
        let receipt = hide_receipt(app.update(decision));
        let replacement = app.pending_card_removal_target(card).unwrap();
        drop(app.update(old_receipt));
        assert_eq!(app.pending_card_removal_target(card), Some(replacement));
        assert!(app.card_removal_current(replacement));
        drop(app.update(receipt));
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!app.project_recovery_ui.closing_panes);
    }

    #[test]
    fn held_chat_decisions_from_a_revoked_selection_cannot_act_on_its_reopened_dialog() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "do not discard", false);
        let old = queued(app.close_cards_in_order(vec![card]));
        drop(app.update(old));
        let held = [cancel_decision(&app), emitted_confirm(&app), hide_decision(&app)];
        let newer = queued(app.close_cards_in_order(vec![card]));
        drop(app.update(newer));
        let generation = app.chat_ui.close_dialog.as_ref().unwrap().generation();
        let current = app.pane_close_batch_for(card);
        for message in held {
            let result = app.update(message);
            assert!(iced::runtime::task::into_stream(result).is_none());
            assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().generation(), generation);
            assert!(app.pane_close_decision_current(card, current));
            assert!(app.workspace.order().any(|id| id == card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.pending_card_removal_target(card).is_none());
            assert!(app.chat_ui.phases.get(&card).is_none());
        }
        let current_cancel = cancel_decision(&app);
        drop(app.update(current_cancel));
        assert!(!app.project_recovery_ui.closing_panes);
    }

    #[test]
    fn selected_end_of_a_hidden_durable_chat_advances_only_after_the_exact_cleanup_receipt() {
        let (mut app, card, directory) = fixture();
        let source = attach_durable_owner(&mut app, card);
        write(&mut app, card, "hidden selected draft", false);
        let other = second_chat(&mut app, directory.path(), "next selected conversation");
        retain_in_channel(&mut app, directory.path(), vec![card, other]);
        drop(app.update(Message::CloseCard(card)));
        let decision = hide_decision(&app);
        let receipt = hide_receipt(app.update(decision));
        drop(app.update(receipt));
        assert!(!app.workspace.order().any(|id| id == card));
        let delivery = queued(app.close_cards_in_order(vec![card, other]));
        drop(app.update(delivery));
        let decision = emitted_confirm(&app);
        drop(app.update(decision));
        let target = pending_end(&app, card);
        let batch = app.pane_close_batch_for(card);
        assert_eq!(app.queued_card_closes.len(), 1);
        assert!(app.card_state(card).unwrap().existence.is_open());
        let next = queued(app.update(Message::ChatUi(super::super::Action::RetainedCloseFinished(target, source, Ok(())))));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(!app.pane_close_decision_current(card, batch));
        assert!(app.pane_close_decision_current(other, batch));
        drop(app.update(next));
        assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().card, other);
    }

    #[test]
    fn direct_chat_hide_cannot_dequeue_an_unrelated_selected_delivery() {
        let (mut app, direct, directory) = fixture();
        write(&mut app, direct, "independent draft", false);
        let first = second_chat(&mut app, directory.path(), "first selected");
        let second = second_chat(&mut app, directory.path(), "second selected");
        let held = queued(app.close_cards_in_order(vec![first, second]));
        let batch = app.pane_close_batch_for(first);
        drop(app.update(Message::CloseCard(direct)));
        let decision = hide_decision(&app);
        let receipt = hide_receipt(app.update(decision));
        drop(app.update(receipt));
        assert_eq!(app.queued_card_closes.len(), 1);
        assert!(app.pane_close_decision_current(first, batch));
        assert!(app.pane_close_decision_current(second, batch));
        assert!(!app.workspace.order().any(|id| id == direct));
        drop(app.update(held));
        assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().card, first);
    }

    #[test]
    fn ending_a_genuinely_hidden_durable_chat_waits_for_cleanup_and_retires_its_saved_conversation() {
        let (mut app, card, directory) = fixture();
        let source = attach_durable_owner(&mut app, card);
        let channel = retain_in_channel(&mut app, directory.path(), vec![card]);
        write(&mut app, card, "provider-keyed hidden draft", false);
        drop(app.update(Message::CloseCard(card)));
        let decision = hide_decision(&app);
        let receipt = hide_receipt(app.update(decision));
        drop(app.update(receipt));
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        drop(app.update(Message::ChatClose(Action::End(card))));
        let confirm = emitted_confirm(&app);
        drop(app.update(confirm));
        let target = pending_end(&app, card);
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(app.workspace.pending_close().is_none());
        assert!(app.card_state(card).unwrap().existence.is_open(), "cleanup is still owed");
        // A fixture result settles the actual phase; no live provider IO is polled.
        drop(app.update(Message::ChatUi(super::super::Action::RetainedCloseFinished(target, source.clone(), Ok(())))));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(!app.cards.contains_key(&card));
        assert!(app.workspace.pending_close().is_none());
        assert_eq!(app.durable.conversation_drafts[&draft_key(&source).unwrap()], "provider-keyed hidden draft");
        assert!(app.recently_closed.iter().any(|(saved, destination)| saved.id == card && *destination == Some(WorkspaceDestination::Channel(channel))));
        assert!(matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(saved) if saved == &source));
    }

    #[test]
    fn revoked_hidden_end_cleanup_cannot_close_a_reopened_chat_or_take_a_new_selection() {
        let (mut app, card, directory) = fixture();
        let source = attach_durable_owner(&mut app, card);
        let channel = retain_in_channel(&mut app, directory.path(), vec![card]);
        write(&mut app, card, "keep after reopened selection", false);
        drop(app.update(Message::CloseCard(card)));
        let decision = hide_decision(&app);
        let receipt = hide_receipt(app.update(decision));
        drop(app.update(receipt));
        assert!(!app.workspace.order().any(|id| id == card));
        let first = queued(app.close_cards_in_order(vec![card]));
        drop(app.update(first));
        let decision = emitted_confirm(&app);
        drop(app.update(decision));
        let target = pending_end(&app, card);
        let old = app.pane_close_batch_for(card);
        let _held = queued(app.close_cards_in_order(vec![card]));
        let current = app.pane_close_batch_for(card);
        assert_ne!(old, current);
        drop(app.select_channel(channel));
        assert!(app.workspace.order().any(|id| id == card));
        drop(app.update(Message::ChatUi(super::super::Action::RetainedCloseFinished(target, source, Ok(())))));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.workspace.order().any(|id| id == card));
        assert!(app.pane_close_decision_current(card, current));
        assert!(app.chat_ui.end_requests.is_empty());
        assert!(app.workspace.pending_close().is_none());
        assert!(app.recently_closed.is_empty());
    }

    fn shared_scope(app: &mut Cagents, card: CardId, directory: &Path) {
        use crate::workspace_model::{ModelChannel, ModelProject, WorkspaceModel, views::{Pane, Scope, View}};
        let channel = app.durable.active_channel.expect("retained channel");
        let scope = Scope::Channel(channel);
        let pane = Pane::from_card(app.card_state(card).unwrap(), Some(directory.into())).unwrap();
        let mut view = View::empty(scope);
        view.panes = vec![pane];
        let mut model = WorkspaceModel::default();
        model.identity = uuid::Uuid::new_v4();
        model.projects = vec![ModelProject { repository: directory.into(), checkout: None, primary_checkout: None, name: "Recorded project".into(), kind: Default::default(), ordinal: 1 }];
        model.channels = vec![ModelChannel { id: channel, repository: directory.into(), title: "Retained conversation".into(), title_source: Default::default(), ordinal: 1 }];
        model.views = vec![view];
        app.workspace_model = model;
    }

    #[test]
    fn sidebar_end_parameters_cannot_follow_replaced_identity_or_moved_scope() {
        for change in 0..12 {
            let (mut app, card, directory) = fixture();
            write(&mut app, card, "keep sidebar conversation", true);
            retain_in_channel(&mut app, directory.path(), vec![card]);
            shared_scope(&mut app, card, directory.path());
            let target = app.capture_conversation_end_target(card).unwrap();
            match change {
                0 => app.workspace_model.identity = uuid::Uuid::new_v4(),
                1 => {
                    let CardKind::Chat(session) = &mut app.card_state_mut(card).unwrap().kind else { panic!("issued chat"); };
                    session.launch.session_id = Some("replacement-issued-thread".into());
                }
                2 => {
                    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else { panic!("observed chat"); };
                    runtime.session.launch.session_id = Some("replacement-observed-thread".into());
                }
                3 => Arc::make_mut(&mut app.durable).channels[0].cards.clear(),
                4 => { Arc::make_mut(&mut app.durable).card_scopes.insert(card, directory.path().join("another-scope")); },
                5 => Arc::make_mut(&mut app.durable).channels[0].project_id = ProjectId::new(),
                6 => app.workspace_model.channels[0].repository = directory.path().join("another-owner"),
                7 => {
                    let CardKind::Chat(session) = &mut app.workspace_model.views[0].panes[0].kind else { panic!("shared chat"); };
                    session.launch.session_id = Some("replaced-before-local-adoption".into());
                }
                8 => app.workspace_model.views[0].panes[0].pending_terminal_start = true,
                9 => { let duplicate = app.workspace_model.views[0].panes[0].clone(); app.workspace_model.views[0].panes.push(duplicate); }
                10 => app.workspace_model.views[0].panes[0].scope = Some(directory.path().join("different-filing")),
                _ => {}
            }
            drop(app.request_conversation_end(target));
            assert_eq!(app.chat_ui.close_dialog.is_some(), change == 11);
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.chat_ui.phases.is_empty());
        }
    }

    #[test]
    fn a_sidebar_confirmation_keeps_its_original_filing_until_the_actual_decision() {
        for move_scope in [false, true] {
            let (mut app, card, directory) = fixture();
            write(&mut app, card, "keep original scoped draft", true);
            retain_in_channel(&mut app, directory.path(), vec![card]);
            shared_scope(&mut app, card, directory.path());
            let target = app.capture_conversation_end_target(card).unwrap();
            drop(app.request_conversation_end(target));
            drop(app.finish_modal_update(Task::none()));
            let held = emitted_confirm(&app);
            if move_scope {
                app.workspace_model.views[0].scope = crate::workspace_model::views::Scope::Worktree {
                    repository: directory.path().into(), path: directory.path().join("moved-worktree"),
                };
            } else {
                app.workspace_model.views[0].panes[0].scope = Some(directory.path().join("moved-filing"));
            }
            drop(app.update(held));
            assert!(app.chat_ui.end_requests.is_empty());
            assert!(app.chat_ui.phases.is_empty());
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.recently_closed.is_empty());
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else { panic!("original runtime retained"); };
            assert_eq!(runtime.unsent_text(), "keep original scoped draft");
            drop(app.finish_modal_update(Task::none()));
            assert!(app.chat_ui.close_dialog.is_none());
            let current = app.capture_conversation_end_target(card).unwrap();
            drop(app.request_conversation_end(current));
            assert!(app.chat_ui.close_dialog.is_some(), "fresh consent may use the new scope");
        }
    }

    #[test]
    fn a_failed_return_cannot_capture_a_new_shared_removal_basis_for_the_held_end() {
        for local_error in [None, Some(false), Some(true)] {
        for retained_owner in [false, true] {
        let (mut app, card, directory) = fixture();
        if retained_owner { attach_durable_owner(&mut app, card); }
        write(&mut app, card, "keep original failed-return draft", false);
        retain_in_channel(&mut app, directory.path(), vec![card]);
        shared_scope(&mut app, card, directory.path());
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else { panic!("original chat"); };
        let target = runtime.target;
        let source = app.chat_return_session(card).unwrap();
        let local = local_error.map(|_| LocalReturn { session: source.clone(), launch: source.launch.recipe.clone(), resumes: true });
        app.chat_ui.phases.insert(card, Phase::Returning { target, local, close_requested: false });
        let _original_delivery = queued(app.close_cards_in_order(vec![card]));
        let batch = app.pane_close_batch_for(card);
        assert!(batch.is_some());
        drop(app.begin_chat_close_for(card, batch));
        assert!(app.chat_ui.end_requests.contains_key(&target));
        app.workspace_model.views[0].scope = crate::workspace_model::views::Scope::Worktree {
            repository: directory.path().into(), path: directory.path().join("new-owner"),
        };
        let completion = match local_error {
            None => super::super::Action::HandoffFinished(target, Err("original return failed".into())),
            Some(false) => super::super::Action::ReturnedLocally(target, Err(ReturnRefused::SourceLive)),
            Some(true) => super::super::Action::ReturnedLocally(target, Err(ReturnRefused::Unconfirmed("original return failed".into()))),
        };
        drop(app.update(Message::ChatUi(completion)));
        assert!(app.chat_ui.end_requests.is_empty());
        assert!(!app.pane_close_batch_current(batch), "failed old intent must not leave a dormant selection");
        assert!(app.chat_ui.phases.is_empty());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.workspace.order().any(|id| id == card));
        assert!(app.recently_closed.is_empty());
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else { panic!("original runtime kept"); };
        assert_eq!(runtime.unsent_text(), "keep original failed-return draft");
        }
        }
    }

    #[test]
    fn a_qualified_saved_report_end_removes_only_its_original_membership() {
        let (mut app, card, owner, report) = super::super::qualified_closed_console_archive_for_test();
        let before = app.canonical_automation_archive_pane(card).expect("original canonical console");
        let scope = app.workspace_model.views[0].scope.clone();
        let original_report = serde_json::to_value(&*report).unwrap();
        let original_owner = serde_json::to_value(&app.workspace_model.archived_automation_agents).unwrap();
        let request = EndRequest::capture(&mut app, card, None).expect("qualified original archive source");
        assert!(request.archived_metadata_only());
        assert!(app.pending_shared_view_changes_for_test().is_empty());

        drop(app.begin_chat_close_request(card, request));

        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.workspace.order().any(|id| id == card));
        assert_eq!(app.durable.automation_archives.get(&card), Some(&owner));
        assert_eq!(serde_json::to_value(&*report).unwrap(), original_report);
        assert_eq!(serde_json::to_value(&app.workspace_model.archived_automation_agents).unwrap(), original_owner);
        assert!(app.chat_ui.phases.is_empty());
        assert!(app.chat_ui.retiring.is_empty());
        assert!(app.chat_ui.end_requests.is_empty());
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        let changes = app.pending_shared_view_changes_for_test();
        assert!(changes.iter().any(|(current_scope, change)| current_scope == &scope && matches!(change,
            crate::workspace_model::views::Change::RemoveArchivedAutomationSource { registry, before: captured, .. }
                if *registry == app.workspace_model.identity && captured == &before)));
        assert!(!changes.iter().any(|(_, change)| matches!(change,
            crate::workspace_model::views::Change::Remove { .. }
                | crate::workspace_model::views::Change::RemoveSource { .. })));
    }

    #[test]
    fn a_saved_report_end_keeps_its_pane_when_original_scope_or_readiness_expires() {
        for revoke_readiness in [false, true] {
            let (mut app, card, owner, report) = super::super::qualified_closed_console_archive_for_test();
            let request = EndRequest::capture(&mut app, card, None).unwrap();
            assert!(request.archived_metadata_only());
            let original_report = serde_json::to_value(&*report).unwrap();
            if revoke_readiness {
                let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else { panic!("ready report runtime"); };
                runtime.archive_readiness = None;
                assert!(!matches!(app.archived_pane_readiness(card), Ok(Some(true))));
            } else {
                let crate::workspace_model::views::Scope::Worktree { path, .. } = &mut app.workspace_model.views[0].scope else { panic!("original worktree"); };
                *path = path.join("another-scope");
            }
            drop(app.begin_chat_close_request(card, request));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.workspace.order().any(|id| id == card));
            assert_eq!(app.durable.automation_archives.get(&card), Some(&owner));
            assert_eq!(serde_json::to_value(&*report).unwrap(), original_report);
            assert!(app.pending_shared_view_changes_for_test().is_empty());
            assert!(app.chat_ui.phases.is_empty());
            assert!(app.chat_ui.retiring.is_empty());
            assert!(app.chat_ui.end_requests.is_empty());
            assert!(app.terminal_starts.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn an_unqualified_unpublished_archive_never_falls_through_to_provider_retirement() {
        let (mut app, card, _directory) = fixture();
        let source = attach_durable_owner(&mut app, card);
        Arc::make_mut(&mut app.durable).automation_archives.insert(card, crate::model::AgentId::from_uuid(card.as_uuid()));
        assert!(!matches!(app.archived_pane_readiness(card), Ok(None)));
        drop(app.begin_chat_close(card));
        assert!(app.chat_ui.phases.is_empty());
        assert!(app.chat_ui.retiring.is_empty());
        assert!(app.chat_ui.end_requests.is_empty());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert_eq!(app.chat_return_session(card), Some(source));
        assert!(app.recently_closed.is_empty());
    }

    #[test]
    fn an_unpublished_hidden_chat_end_cannot_cross_a_registry_join() {
        let (mut app, card, directory) = fixture();
        let source = attach_durable_owner(&mut app, card);
        write(&mut app, card, "local retained prompt", false);
        retain_in_channel(&mut app, directory.path(), vec![card]);
        drop(app.update(Message::CloseCard(card)));
        let decision = hide_decision(&app);
        let receipt = hide_receipt(app.update(decision));
        drop(app.update(receipt));
        drop(app.update(Message::ChatClose(Action::End(card))));
        let decision = emitted_confirm(&app);
        drop(app.update(decision));
        let target = pending_end(&app, card);
        // No shared membership existed at intake; even an identical local
        // descriptor cannot transfer this lifecycle decision to a new registry.
        app.workspace_model.identity = uuid::Uuid::new_v4();
        drop(app.update(Message::ChatUi(super::super::Action::RetainedCloseFinished(target, source, Ok(())))));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.cards.contains_key(&card));
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(app.recently_closed.is_empty());
        assert!(app.chat_ui.end_requests.is_empty());
    }

    #[test]
    fn hidden_end_keeps_original_shared_source_through_cleanup_without_adopting_a_replacement() {
        for replace in [false, true] {
            let (mut app, card, directory) = fixture();
            let source = attach_durable_owner(&mut app, card);
            write(&mut app, card, "shared retained draft", false);
            retain_in_channel(&mut app, directory.path(), vec![card]);
            shared_scope(&mut app, card, directory.path());
            drop(app.update(Message::CloseCard(card)));
            let decision = hide_decision(&app);
            let receipt = hide_receipt(app.update(decision));
            drop(app.update(receipt));
            assert!(!app.workspace.order().any(|id| id == card));
            drop(app.update(Message::ChatClose(Action::End(card))));
            let decision = emitted_confirm(&app);
            drop(app.update(decision));
            let target = pending_end(&app, card);
            let before = app.workspace_model.clone();
            if replace {
                let CardKind::Chat(session) = &mut app.workspace_model.views[0].panes[0].kind else { panic!("shared chat"); };
                session.launch.session_id = Some("replacement-in-authoritative-view".into());
            }
            let expected = app.workspace_model.clone();
            drop(app.update(Message::ChatUi(super::super::Action::RetainedCloseFinished(target, source, Ok(())))));
            if replace {
                assert!(app.card_state(card).unwrap().existence.is_open());
                assert!(app.cards.contains_key(&card));
                assert!(app.recently_closed.is_empty());
                assert_eq!(app.workspace_model, expected);
                assert!(app.status.as_ref().is_some_and(|notice| notice.message.contains("shared pane changed")) || app.notices.entries.iter().any(|entry| entry.notice.message.contains("shared pane changed")));
            } else {
                assert!(!app.card_state(card).unwrap().existence.is_open());
                assert!(!app.cards.contains_key(&card));
                assert_eq!(before.identity, app.workspace_model.identity);
            }
        }
    }

    #[test]
    fn parked_actor_cleanup_resumes_only_the_terminal_worker_captured_before_cleanup() {
        for replacement in [false, true] {
            for initially_bound in [false, true] {
                let (mut app, card, _directory) = fixture();
                let session = attach_durable_owner(&mut app, card);
                let proof = session.proof.clone().unwrap();
                let mut terminal = return_terminal(&session).unwrap();
                terminal.continuity = TerminalContinuity::Live {host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(), host_incarnation: proof.host_incarnation.clone()};
                app.card_state_mut(card).unwrap().kind = CardKind::Terminal(terminal.clone());
                let Some(CardRuntime::Chat(mut runtime)) = app.cards.remove(&card) else { panic!("live chat"); };
                runtime.session.terminal = Some(terminal.clone());
                app.chat_ui.parked.insert(card, runtime);
                let original_worker = uuid::Uuid::new_v4();
                if initially_bound {
                    app.record_terminal_close_binding_for_test(card, crate::terminal::HostedTerminalBinding {source: terminal.clone(), execution_instance: original_worker}, proof.clone(), false);
                }
                drop(app.update(Message::CloseCard(card)));
                let target = pending_end(&app, card);
                assert!(app.chat_ui.end_requests[&target].paired_terminal.is_some());
                if replacement {
                    app.record_terminal_close_binding_for_test(card, crate::terminal::HostedTerminalBinding {source: terminal.clone(), execution_instance: uuid::Uuid::new_v4()}, proof.clone(), true);
                }
                let delivery = queued(app.update(Message::ChatUi(super::super::Action::CloseFinished(target, Ok(())))));
                assert!(!app.chat_ui.parked.contains_key(&card));
                drop(app.update(delivery));
                assert!(app.card_state(card).unwrap().existence.is_open());
                assert_eq!(app.card_state(card).unwrap().kind, CardKind::Terminal(terminal));
                assert!(app.workspace.pending_close().is_none());
                if replacement {
                    assert!(app.terminal_close.is_none(), "old actor cleanup cannot acquire consent to the new CLI worker");
                } else {
                    assert_eq!(app.terminal_close.as_ref().map(|intent| intent.card), Some(card), "unchanged captured source reaches the normal terminal close handshake");
                }
            }
        }
    }

    #[test]
    fn unconfirmed_parked_actor_cleanup_keeps_its_runtime_and_cannot_resume_cli_close() {
        let (mut app, card, _directory) = fixture();
        let session = attach_durable_owner(&mut app, card);
        let proof = session.proof.clone().unwrap();
        let mut terminal = return_terminal(&session).unwrap();
        terminal.continuity = TerminalContinuity::Live { host_session_id: proof.host_session_id.clone(), boot_id: proof.boot_id.clone(), host_incarnation: proof.host_incarnation.clone() };
        app.card_state_mut(card).unwrap().kind = CardKind::Terminal(terminal.clone());
        let Some(CardRuntime::Chat(mut runtime)) = app.cards.remove(&card) else { panic!("chat"); };
        runtime.session.terminal = Some(terminal.clone());
        app.chat_ui.parked.insert(card, runtime);
        app.record_terminal_close_binding_for_test(card, crate::terminal::HostedTerminalBinding { source: terminal.clone(), execution_instance: uuid::Uuid::new_v4() }, proof, false);
        drop(app.update(Message::CloseCard(card)));
        let target = pending_end(&app, card);
        let completion = app.update(Message::ChatUi(super::super::Action::CloseFinished(target, Err("fixture cleanup unconfirmed".into()))));
        assert!(app.chat_ui.parked.contains_key(&card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert_eq!(app.card_state(card).unwrap().kind, CardKind::Terminal(terminal));
        assert!(app.terminal_close.is_none());
        assert!(app.workspace.pending_close().is_none());
        assert!(app.chat_ui.end_requests.is_empty());
        assert!(app.status.as_ref().is_some_and(|notice| notice.message.contains("cleanup unconfirmed")) || app.notices.entries.iter().any(|entry| entry.notice.message.contains("cleanup unconfirmed")));
        drop(completion); // no fresh CLI source may be captured from this result
    }

    #[test]
    fn a_changed_paired_cli_cannot_inherit_an_earlier_end_cleanup() {
        for (descriptor, observed) in [(true, true), (true, false), (false, true)] {
            let (mut app, card, _directory) = fixture();
            let source = attach_durable_owner(&mut app, card);
            write(&mut app, card, "preserve paired conversation", false);
            drop(app.update(Message::ChatClose(Action::End(card))));
            let decision = emitted_confirm(&app);
            drop(app.update(decision));
            let target = pending_end(&app, card);
            let replacement = fresh_terminal_session(source.launch.recipe.clone());
            if descriptor {
                let CardKind::Chat(session) = &mut app.card_state_mut(card).unwrap().kind else { panic!("descriptor"); };
                session.terminal = Some(replacement.clone());
            }
            if observed {
                let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else { panic!("runtime"); };
                runtime.session.terminal = Some(replacement);
            }
            let before = app.card_state(card).unwrap().clone();
            let runtime_source = app.chat_return_session(card);
            drop(app.update(Message::ChatUi(super::super::Action::RetainedCloseFinished(target, source, Ok(())))));
            assert_eq!(app.card_state(card), Some(&before));
            assert_eq!(app.chat_return_session(card), runtime_source);
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(app.workspace.order().any(|id| id == card));
            assert!(app.workspace.pending_close().is_none());
            assert!(app.recently_closed.is_empty());
        }
    }

    #[test]
    fn held_widget_consent_cannot_end_another_conversation_or_configuration_on_the_same_card() {
        for changed in 0..7 {
            let (mut app, card, _directory) = fixture();
            write(&mut app, card, "original prompt", true);
            drop(app.update(Message::CloseCard(card)));
            let held = emitted_confirm(&app);
            let mut replacement = app.chat_return_session(card).unwrap();
            match changed {
                0 => replacement.launch.session_id = Some("different-thread".into()),
                1 => replacement.id = crate::model::TerminalId::new(),
                2 => replacement
                    .launch
                    .recipe
                    .arguments
                    .push("--different-options".into()),
                3 => {
                    replacement
                        .launch
                        .recipe
                        .environment
                        .insert("ACCOUNT".into(), "another".into());
                }
                4 => replacement.launch.recipe.working_directory = "/different-directory".into(),
                _ => replacement.launch.session_id = Some("runtime-or-descriptor-only".into()),
            }
            if changed != 5 {
                app.card_state_mut(card).unwrap().kind = CardKind::Chat(replacement.clone());
            }
            if changed != 6 {
                let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                    panic!("chat");
                };
                runtime.session = replacement;
            }
            drop(app.update(held));
            assert!(
                app.card_state(card).unwrap().existence.is_open(),
                "changed identity {changed}"
            );
            assert!(app.cards.contains_key(&card));
            assert!(app.chat_ui.close_dialog.is_some(), "rejected input has no prelude effects");
            drop(app.update(Message::DismissStatus));
            assert!(app.chat_ui.close_dialog.is_none(), "the next lifecycle update retires stale backing");
        }
    }

    #[test]
    fn invalid_upper_owned_escape_cannot_cancel_a_surviving_lower_dialog() {
        for invalidation in 0..3 {
            let (mut app, card, _directory) = fixture();
            write(&mut app, card, "keep original prompt", true);
            app.settings_reset_confirm = true;
            drop(app.finish_modal_update(Task::none()));
            let batch = if invalidation == 1 {
                let _delivery = queued(app.close_cards_in_order(vec![card]));
                app.pane_close_batch_for(card)
            } else {
                None
            };
            drop(app.request_chat_tab_close_for(card, batch));
            drop(app.finish_modal_update(Task::none()));
            assert_eq!(app.top_modal(), Some(modal_ui::Modal::ChatClose));
            // Obtain the owner from the real mounted dialog output. Keyboard
            // transport uses the same envelope; native Popover routing is
            // additionally covered by the final keyboard integration fixture.
            let Message::ModalSurface { target, .. } = emitted_confirm(&app) else {
                panic!("mounted dialog must stamp its owner");
            };
            let escape = || Message::KeyEvent {
                event: keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(), text: None, repeat: false,
                },
                captured: false,
            };
            if invalidation == 1 {
                app.stop_pane_close_batch(batch);
            } else if invalidation == 2 {
                app.workspace_model.identity = uuid::Uuid::new_v4();
            } else {
                let CardKind::Chat(session) = &mut app.card_state_mut(card).unwrap().kind else { panic!("chat"); };
                session.launch.session_id = Some("replacement-thread".into());
            }
            let revision = app.modal_surface_target(modal_ui::Modal::ChatClose);
            assert_eq!(target, revision, "source invalidation does not change the surface generation");
            drop(app.update(Message::ModalSurface { target, message: Box::new(escape()) }));
            assert!(app.settings_reset_confirm, "old upper Escape must not be reinterpreted for the lower dialog");
            assert!(app.chat_ui.close_dialog.is_some(), "stale input is rejected before cleanup");
            drop(app.finish_modal_update(Task::none()));
            assert!(app.chat_ui.close_dialog.is_none());
            assert_eq!(app.top_modal(), Some(modal_ui::Modal::SettingsReset));
            let target = app.modal_surface_target(modal_ui::Modal::SettingsReset);
            drop(app.update(Message::ModalSurface { target, message: Box::new(escape()) }));
            assert!(!app.settings_reset_confirm, "fresh lower-owned Escape still closes its own dialog");
            assert!(app.card_state(card).unwrap().existence.is_open());
        }
    }

    #[test]
    fn held_widget_consent_survives_same_logical_actor_refresh_without_a_process_roster_fence() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "captured prompt", true);
        drop(app.update(Message::CloseCard(card)));
        let held = emitted_confirm(&app);
        let source = app.chat_return_session(card).unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("chat");
        };
        // A backend replacement can update its presentation without changing
        // the issued logical session. Consent never binds a PID or roster row.
        Arc::make_mut(&mut runtime.snapshot).busy = false;
        runtime.session = source.clone();
        drop(app.update(held));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert_eq!(
            app.durable.conversation_drafts[&draft_key(&source).unwrap()],
            "captured prompt"
        );
    }

    #[test]
    fn obsolete_confirmation_cannot_end_a_reopened_dialog_even_for_the_same_card() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "keep this prompt", true);
        drop(app.update_chat_close(Action::End(card)));
        let previous = app.chat_ui.close_dialog.as_ref().unwrap().target();
        drop(app.cancel_chat_close());
        drop(app.update_chat_close(Action::End(card)));
        let current = app.chat_ui.close_dialog.as_ref().unwrap().target();
        assert_ne!(previous, current);
        for stale in [
            Action::Confirm(previous),
            Action::Cancel(previous),
            Action::Hide(previous),
        ] {
            drop(app.update_chat_close(stale));
            assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().target(), current);
            assert!(app.card_state(card).unwrap().existence.is_open());
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("chat retained");
            };
            assert_eq!(runtime.unsent_text().trim(), "keep this prompt");
        }
    }

    #[test]
    fn a_higher_modal_blocks_chat_decisions_and_escape_restores_its_cancel() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "keep this draft", true);
        let opening = app.update(Message::CloseCard(card));
        let cancel = widget::Id::new("chat-close.cancel");
        assert_eq!(
            crate::app::tests::focused_element(app.dialog().unwrap(), opening),
            Some(cancel.clone())
        );
        let target = app.chat_ui.close_dialog.as_ref().unwrap().target();
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        assert_eq!(app.top_modal(), Some(modal_ui::Modal::SettingsReset));
        for stale in [
            Action::Confirm(target),
            Action::Cancel(target),
            Action::Hide(target),
        ] {
            drop(app.update_chat_close(stale));
            assert!(app.chat_ui.close_dialog.is_some());
            assert!(app.card_state(card).unwrap().existence.is_open());
        }
        let escape = || Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured: false,
        };
        let close_upper = app.update(escape());
        assert!(!app.settings_reset_confirm);
        assert_eq!(app.top_modal(), Some(modal_ui::Modal::ChatClose));
        assert_eq!(
            crate::app::tests::focused_element(app.dialog().unwrap(), close_upper),
            Some(cancel)
        );
        drop(app.update(escape()));
        assert!(!app.modal_is_open());
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("chat retained");
        };
        assert_eq!(runtime.unsent_text().trim(), "keep this draft");
        assert!(runtime.snapshot.busy);
    }

    #[test]
    fn busy_or_drafted_tab_close_confirms_and_cancel_changes_nothing() {
        for (draft, busy) in [
            ("unsent prompt", false),
            ("", true),
            ("draft while working", true),
        ] {
            let (mut app, card, _directory) = fixture();
            write(&mut app, card, draft, busy);
            let task = app.update(Message::CloseCard(card));
            assert!(app.chat_ui.close_dialog.as_ref().unwrap().tab);
            assert_eq!(
                crate::app::tests::focused_element(app.chat_close_dialog().unwrap(), task),
                Some(widget::Id::new("chat-close.cancel"))
            );
            drop(app.cancel_chat_close());
            assert!(app.chat_ui.close_dialog.is_none());
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("cancel retained runtime");
            };
            assert_eq!(runtime.unsent_text().trim(), draft);
            assert_eq!(runtime.snapshot.busy, busy);
            assert!(app.workspace.order().any(|id| id == card));
            assert!(app.card_state(card).unwrap().existence.is_open());
        }
    }
    #[test]
    fn a_close_confirmation_disappears_when_its_conversation_was_closed_elsewhere() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "retained", true);
        drop(app.update(Message::CloseCard(card)));
        assert!(app.chat_ui.close_dialog.is_some());
        drop(app.begin_chat_close(card));
        drop(app.update(Message::DismissStatus));
        assert!(app.chat_ui.close_dialog.is_none());
        assert!(!app.modal_is_open());
    }

    #[test]
    fn confirmed_end_saves_draft_for_session_and_recreated_card_restores_it() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "keep across cards", true);
        let session = app.chat_return_session(card).unwrap();
        drop(app.update(Message::CloseCard(card)));
        respond(&mut app, Action::Confirm);
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.cards.contains_key(&card));
        let key = draft_key(&session).unwrap();
        assert_eq!(
            app.persistent_workspace().conversation_drafts[&key],
            "keep across cards"
        );
        drop(app.insert_chat_card(session, None, None));
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        assert_eq!(app.chat_ui.drafts[&reopened], "keep across cards");
    }
    #[test]
    fn ending_and_resuming_a_new_card_retains_the_original_channel() {
        let (mut app, card, _directory) = fixture();
        let channel = crate::model::ChannelId::new();
        let project = crate::model::ProjectId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "Original project".into(),
                _directory.path().into(),
                Vec::new(),
                Vec::new(),
            ));
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            id: channel,
            project_id: project,
            title: "Original channel".into(),
            title_source: Default::default(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
        durable.active_channel = Some(channel);
        write(&mut app, card, "original draft", true);
        let session = app.chat_return_session(card).unwrap();
        drop(app.begin_chat_close(card));
        assert_eq!(
            app.durable
                .conversation_channels
                .get(&draft_key(&session).unwrap()),
            Some(&channel)
        );
        drop(
            app.resume_saved_conversation(crate::persistence::SavedConversation {
                card,
                title: "Resumed conversation".into(),
                session,
                open: false,
                updated_at_unix_ms: 0,
            }),
        );
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        assert_eq!(app.durable.active_channel, Some(channel));
        assert!(
            app.durable
                .channels
                .iter()
                .find(|row| row.id == channel)
                .unwrap()
                .cards
                .contains(&reopened)
        );
        assert_eq!(app.chat_ui.drafts[&reopened], "original draft");
    }

    #[test]
    fn hiding_preserves_conversation_membership_runtime_and_draft() {
        let (mut app, card, _directory) = fixture();
        let project = crate::model::ProjectId::new();
        let channel = crate::model::ChannelId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "Project".into(),
                _directory.path().into(),
                Vec::new(),
                Vec::new(),
            ));
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: channel,
                project_id: project,
                title: "Conversation".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        write(&mut app, card, "draft survives hide", false);
        drop(app.request_chat_tab_close(card));
        respond(&mut app, Action::Hide);
        let target = app.pending_card_removal_target(card).expect("issued Hide reservation");
        drop(app.update_chat_close(Action::CommitHide(target)));
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.cards.contains_key(&card));
        assert!(app.durable.channels[0].cards.contains(&card));
        drop(app.select_channel(channel));
        assert!(app.workspace.order().any(|id| id == card));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("same runtime");
        };
        assert_eq!(runtime.unsent_text().trim(), "draft survives hide");
    }
    #[test]
    fn discovered_model_never_changes_an_issued_session_launch_recipe() {
        let (mut app, card, _directory) = fixture();
        let issued = app.chat_return_session(card).unwrap();
        let handle = Arc::new(ClientHandle::fixture(
            issued.clone(),
            Snapshot {
                session_id: issued.launch.session_id.clone(),
                selected_model: Some("discovered-model".into()),
                ..Snapshot::default()
            },
        ));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("chat");
        };
        runtime.connection = Connection::Live(Arc::clone(&handle));
        runtime.snapshot = handle.snapshot();
        runtime.recorded_model = Some("discovered-model".into());
        assert_eq!(
            app.chat_return_session(card).unwrap().launch.recipe,
            issued.launch.recipe
        );
        app.remember_conversation_draft(card);
        let saved = app.persistent_workspace();
        assert_eq!(
            saved.conversation_models[&draft_key(&issued).unwrap()],
            "discovered-model"
        );
        assert_eq!(handle.session().launch.recipe, issued.launch.recipe);
        assert_eq!(
            app.chat_return_session(card).unwrap().launch.recipe,
            issued.launch.recipe
        );
        drop(app.begin_chat_close(card));
        let CardKind::Chat(saved) = &app.card_state(card).unwrap().kind else {
            panic!("chat descriptor");
        };
        assert_eq!(saved.launch.recipe, issued.launch.recipe);
        assert_eq!(handle.session().launch.recipe, issued.launch.recipe);
    }

    #[test]
    fn live_conversation_confirms_without_hydrating_its_stale_idle_snapshot() {
        for visible in [false, true] {
            let (mut app, card, _directory) = fixture();
            let session = app.chat_return_session(card).unwrap();
            let handle = Arc::new(ClientHandle::fixture(
                session.clone(),
                Snapshot {
                    session_id: session.launch.session_id.clone(),
                    busy: true,
                    ..Snapshot::default()
                },
            ));
            let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                panic!("chat");
            };
            runtime.connection = Connection::Live(Arc::clone(&handle));
            runtime.visible.store(visible, Ordering::Relaxed);
            assert!(!runtime.snapshot.busy);
            let presentation = Arc::clone(&runtime.snapshot);
            drop(app.update_chat_close(Action::End(card)));
            assert!(app.chat_ui.close_dialog.is_some());
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("chat");
            };
            assert!(Arc::ptr_eq(&presentation, &runtime.snapshot));
            assert!(handle.snapshot().busy);
            drop(app.cancel_chat_close());
            assert!(app.cards.contains_key(&card));
            assert!(handle.snapshot().busy);
        }
    }

    #[test]
    fn model_flags_are_replaced_without_retaining_a_seed_models_argument() {
        let (app, card, _directory) = fixture();
        let mut recipe = app.chat_return_session(card).unwrap().launch.recipe;
        recipe
            .arguments
            .extend(["--model=seed".into(), "-m".into(), "obsolete".into()]);
        set_model(&mut recipe, "recorded-model");
        assert_eq!(recipe_model(&recipe).as_deref(), Some("recorded-model"));
        assert_eq!(
            recipe
                .arguments
                .iter()
                .filter(|arg| *arg == "--model")
                .count(),
            1
        );
        assert!(
            !recipe
                .arguments
                .iter()
                .any(|arg| arg == "obsolete" || arg.contains("seed"))
        );
    }
}
