//! Registered conversation controls share the native toolbar's live guards.
use super::*;
use crate::workspace_widget::GlobalShortcut as RegisteredAction;

/// A palette result retains both the conversation generation and any
/// parameter it displayed; a newer queue entry never replaces that intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct RegisteredChatCommand {
    pub command: RegisteredAction,
    pub target: Target,
    queued: Option<QueueEntry>,
    first_queued: Option<TerminalId>,
    interrupt_mode: Option<String>,
    fast_mode: Option<bool>,
}

impl RegisteredChatCommand {
    pub(in crate::app) fn action(&self) -> Option<Action> {
        Some(match self.command {
            RegisteredAction::ChatSend => Action::Submit(self.target, PromptMode::Send),
            RegisteredAction::ChatSteer => Action::Submit(self.target, PromptMode::Steer),
            RegisteredAction::ChatQueue => Action::Submit(self.target, PromptMode::Queue),
            RegisteredAction::ChatInterrupt => Action::Interrupt(self.target),
            RegisteredAction::ChatCompact => Action::Command(self.target, Command::Compact),
            RegisteredAction::ChatAttach => Action::Attach(self.target),
            RegisteredAction::ChatReasoning => Action::ReasoningPicker(self.target, true),
            RegisteredAction::ChatLatest => Action::Latest(self.target),
            RegisteredAction::ChatTakeBackQueued => Action::TakeBackQueued(self.target, Some(self.queued.clone()?)),
            RegisteredAction::ChatCancelQueued => Action::CancelQueued(self.target),
            RegisteredAction::ChatToggleSteering => Action::Command(self.target, Command::SetInterruptMode { mode: self.interrupt_mode.clone()? }),
            RegisteredAction::ChatToggleFastMode => Action::Command(self.target, Command::SetFastMode { enabled: self.fast_mode? }),
            _ => return None,
        })
    }
}

impl Runtime {
    pub(super) fn command_controls_available(&self, transitioning: bool) -> bool {
        !transitioning && !self.disconnected && !self.snapshot.exited && self.pending.is_none() && self.queued.is_none()
    }

    pub(super) fn composer_has_text(&self) -> bool {
        self.composer.lines().any(|line| !line.text.trim().is_empty())
    }

    pub(super) fn composer_has_draft(&self) -> bool {
        !self.attachments.is_empty() || self.composer_has_text()
    }

    pub(super) fn registered_chat_request(&self, command: RegisteredAction, transitioning: bool, has_draft: bool) -> Option<RegisteredChatCommand> {
        let available = self.command_controls_available(transitioning);
        let snapshot = &self.snapshot;
        let mut request = RegisteredChatCommand { command, target: self.target, queued: None, first_queued: None, interrupt_mode: None, fast_mode: None };
        let enabled = match command {
            RegisteredAction::ChatSend => available && has_draft && !snapshot.busy,
            RegisteredAction::ChatSteer => available && has_draft && snapshot.busy && snapshot.capabilities.steer,
            // Providers without their own queue use the existing local follow-up queue.
            RegisteredAction::ChatQueue => available && has_draft && snapshot.busy,
            RegisteredAction::ChatInterrupt => available && snapshot.busy,
            RegisteredAction::ChatCompact => available && snapshot.ready && !snapshot.busy && snapshot.read_only_reason.is_none() && snapshot.capabilities.compact,
            RegisteredAction::ChatAttach => available && snapshot.capabilities.attachments && !self.attachment_chooser_pending,
            RegisteredAction::ChatReasoning => !transitioning && snapshot.capabilities.reasoning_selection && snapshot.models.iter().any(|model| Some(&model.id) == snapshot.selected_model.as_ref().or_else(|| (!self.is_draft()).then_some(self.recorded_model.as_ref()).flatten()) && !model.reasoning.is_empty()),
            RegisteredAction::ChatLatest => !transitioning && if snapshot.has_later || snapshot.history_loading || self.page_request.is_some_and(|(page, _)| page == view::Page::Earlier) {
                !self.disconnected && !snapshot.exited && self.handle().is_some()
            } else { !self.scroll.at_bottom() || self.unseen_activity },
            RegisteredAction::ChatTakeBackQueued => {
                if available { request.queued = self.newest_take_back(); }
                request.queued.is_some()
            }
            RegisteredAction::ChatCancelQueued => {
                request.first_queued = self.queued.as_ref().map(|queued| queued.draft.id);
                request.first_queued.is_some() && !self.pending.as_ref().is_some_and(|pending| pending.queued == Some(QueuedStep::Prompt))
            },
            RegisteredAction::ChatToggleSteering => {
                let next = if snapshot.interrupt_mode.as_deref() == Some("wait") { "immediate" } else { "wait" };
                if available && snapshot.capabilities.interrupt_modes.iter().any(|mode| mode == next) { request.interrupt_mode = Some(next.into()); }
                request.interrupt_mode.is_some()
            }
            RegisteredAction::ChatToggleFastMode => {
                if available && snapshot.ready && snapshot.capabilities.fast_mode && snapshot.read_only_reason.is_none() {
                    request.fast_mode = snapshot.fast_mode.as_ref().filter(|mode| mode.changeable()).map(|mode| !mode.enabled());
                }
                request.fast_mode.is_some()
            }
            _ => false,
        };
        enabled.then_some(request)
    }
}

impl Cagents {
    pub(in crate::app) fn registered_chat_request(&self, card: CardId, command: RegisteredAction) -> Option<RegisteredChatCommand> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else { return None; };
        if !self.card_state(card).is_some_and(|state| state.existence.is_open() && matches!(state.kind, CardKind::Chat(_))) { return None; }
        let transitioning = self.chat_start_pending(card);
        if command == RegisteredAction::ChatCancelQueued && self.chat_ui.phases.get(&card).is_some_and(|phase| !matches!(phase, Phase::Starting(_))) { return None; }
        runtime.registered_chat_request(command, transitioning, runtime.composer_has_draft())
    }

    pub(in crate::app) fn registered_chat_command_capabilities(&self, card: CardId) -> u16 {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else { return 0; };
        if !self.card_state(card).is_some_and(|state| state.existence.is_open() && matches!(state.kind, CardKind::Chat(_))) { return 0; }
        let transitioning = self.chat_start_pending(card);
        let has_draft = runtime.composer_has_draft();
        crate::workspace_widget::CHAT_USER_COMMANDS.into_iter().filter_map(|command| {
            if command == RegisteredAction::ChatCancelQueued && self.chat_ui.phases.get(&card).is_some_and(|phase| !matches!(phase, Phase::Starting(_))) { return None; }
            runtime.registered_chat_request(command, transitioning, has_draft).and_then(|_| crate::workspace_widget::chat_user_command_bit(command))
        }).fold(0, |mask, bit| mask | bit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (Cagents, CardId, Target) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.new_native_chat(Provider::Codex, Some(PathBuf::from("/tmp"))));
        let card = app.workspace.focused().unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else { panic!("draft chat"); };
        let target = runtime.target;
        (app, card, target)
    }

    #[test]
    fn registered_chat_controls_follow_draft_busy_and_connection_boundaries() {
        let (mut app, card, target) = app();
        assert!(app.registered_chat_request(card, RegisteredAction::ChatSend).is_none());
        let runtime = app.chat_card_runtime_mut(target).unwrap();
        runtime.replace_draft(text_editor::Content::with_text("hello"));
        assert!(app.registered_chat_request(card, RegisteredAction::ChatSend).is_some());
        assert!(app.registered_chat_request(card, RegisteredAction::ChatQueue).is_none());
        let runtime = app.chat_card_runtime_mut(target).unwrap();
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.busy = true;
        snapshot.capabilities.steer = false;
        snapshot.capabilities.queue = false;
        assert!(app.registered_chat_request(card, RegisteredAction::ChatSend).is_none());
        assert!(app.registered_chat_request(card, RegisteredAction::ChatSteer).is_none());
        assert!(app.registered_chat_request(card, RegisteredAction::ChatQueue).is_some(), "local follow-up queue remains supported");
        assert!(app.registered_chat_request(card, RegisteredAction::ChatInterrupt).is_some());
        Arc::make_mut(&mut app.chat_card_runtime_mut(target).unwrap().snapshot).capabilities.steer = true;
        assert!(app.registered_chat_request(card, RegisteredAction::ChatSteer).is_some());
        app.chat_card_runtime_mut(target).unwrap().disconnected = true;
        for command in [RegisteredAction::ChatSend, RegisteredAction::ChatSteer, RegisteredAction::ChatQueue, RegisteredAction::ChatInterrupt, RegisteredAction::ChatCompact, RegisteredAction::ChatAttach] {
            assert!(app.registered_chat_request(card, command).is_none());
        }
    }

    #[test]
    fn old_first_message_cancel_cannot_cancel_a_replacement_start_attempt() {
        let (mut app, card, target) = app();
        app.chat_card_runtime_mut(target).unwrap().replace_draft(text_editor::Content::with_text("first request"));
        drop(app.start_chat_draft(target));
        let request = app.registered_chat_request(card, RegisteredAction::ChatCancelQueued).unwrap();
        let first = request.first_queued.unwrap();
        assert!(matches!(app.chat_ui.phases.get(&card), Some(Phase::Starting(_))));
        drop(app.cancel_queued(target));
        app.chat_card_runtime_mut(target).unwrap().replace_draft(text_editor::Content::with_text("replacement request"));
        drop(app.start_chat_draft(target));
        let current = app.registered_chat_request(card, RegisteredAction::ChatCancelQueued).unwrap();
        assert_eq!(current.target, request.target, "the runtime target alone does not fence starting attempts");
        assert_ne!(current.first_queued, Some(first));
        drop(app.update_palette(crate::app::palette_ui::Action::RunChatUser(request)));
        assert!(matches!(app.chat_ui.phases.get(&card), Some(Phase::Starting(_))));
        let queued = app.chat_card_runtime_mut(target).unwrap().queued.as_ref().unwrap();
        assert_eq!(Some(queued.draft.id), current.first_queued);
        assert_eq!(queued.text, "replacement request");
    }

    #[test]
    fn captured_chat_queue_command_refuses_a_newer_entry_or_replaced_generation() {
        let (mut app, card, target) = app();
        app.chat_card_runtime_mut(target).unwrap().followups.push(Followup { id: 1, text: "first".into(), attachments: vec![] });
        let request = app.registered_chat_request(card, RegisteredAction::ChatTakeBackQueued).unwrap();
        assert_eq!(request.queued, Some(QueueEntry::Local(1)));
        app.chat_card_runtime_mut(target).unwrap().followups.push(Followup { id: 2, text: "newer".into(), attachments: vec![] });
        assert_ne!(app.registered_chat_request(card, request.command).as_ref(), Some(&request));
        drop(app.update_palette(crate::app::palette_ui::Action::RunChatUser(request.clone())));
        assert_eq!(app.chat_card_runtime_mut(target).unwrap().followups.len(), 2);
        app.chat_card_runtime_mut(target).unwrap().followups.pop();
        app.chat_card_runtime_mut(target).unwrap().target.generation += 1;
        drop(app.update_palette(crate::app::palette_ui::Action::RunChatUser(request)));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else { unreachable!() };
        assert_eq!(runtime.followups.len(), 1);
        assert!(runtime.composer.text().is_empty(), "stale selection must not take any entry into the composer");
    }
}
