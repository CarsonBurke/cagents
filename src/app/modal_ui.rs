//! One ordered registry controls modality, drawing, keyboard routing and focus.
//! Dialog payloads remain in their owning feature modules; entries without a
//! live payload are removed before they can block the workspace.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Modal {
    PullRequest,
    Resume,
    Worktree,
    Channel,
    Project(project_ui::ModalRole),
    Selection,
    DirtyClose,
    Discard,
    SettingsReset,
    TerminalClose,
    ChatClose,
    Update,
    Health,
    Recovery,
    #[cfg(feature = "integrated-browser")]
    BrowserImport,
    Paste,
    Image,
    Device,
    AutomationDraft,
    TransientClose,
    Approval,
}

impl Modal {
    pub(super) fn owns_tab(self) -> bool {
        matches!(
            self,
            Self::Approval
                | Self::Recovery
                | Self::Health
                | Self::SettingsReset
                | Self::TerminalClose
                | Self::DirtyClose
        )
    }
}

/// Feature requests keep their original identity without lossy conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instance {
    Counter(u64),
    Operation(crate::model::OperationId),
}

/// The visible dialog which produced a widget message. Lifecycle task outputs
/// deliberately do not carry this UI-only admission token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceTarget {
    modal: Modal,
    revision: u64,
}

#[derive(Default)]
pub(super) struct Stack {
    frames: Vec<Frame>,
    generation: u64,
    focus_revision: Arc<std::sync::atomic::AtomicU64>,
    workspace_keyboard_nav: Option<bool>,
    pub(super) workspace_focus: crate::interaction::FocusSnapshot,
}
struct Frame {
    kind: Modal,
    generation: u64,
    instance: Option<Instance>,
    restore: widget::Id,
    focus: crate::interaction::FocusSnapshot,
}
impl Stack {
    pub(super) fn with_keyboard_nav(enabled: bool) -> Self {
        Self {
            workspace_keyboard_nav: Some(enabled),
            ..Self::default()
        }
    }
    pub(super) fn adopt_keyboard_nav(&mut self, enabled: bool) {
        self.workspace_keyboard_nav = Some(enabled);
    }
    fn top(&self, active: &[Modal]) -> Option<Modal> {
        // Feature handlers are also called directly by tests and startup.
        // Newly present payloads still participate before the next update.
        active
            .iter()
            .rev()
            .find(|kind| !self.frames.iter().any(|f| f.kind == **kind))
            .copied()
            .or_else(|| {
                self.frames
                    .iter()
                    .rev()
                    .find(|f| active.contains(&f.kind))
                    .map(|f| f.kind)
            })
    }
    #[cfg(test)]
    pub(super) fn reconcile(
        &mut self,
        active: &[Modal],
        fallback: widget::Id,
    ) -> Option<(Modal, u64, bool)> {
        self.reconcile_instances(active, fallback, |_| None)
    }
    fn reconcile_instances(
        &mut self,
        active: &[Modal],
        fallback: widget::Id,
        instance: impl Fn(Modal) -> Option<Instance>,
    ) -> Option<(Modal, u64, bool)> {
        let previous = self.frames.last().map(|f| f.kind);
        // A removed lower dialog cannot remain the restore target of the
        // next surviving frame. Carry its parent link through any gap.
        let mut inherited_restore = None;
        for frame in &mut self.frames {
            if active.contains(&frame.kind) && frame.instance == instance(frame.kind) {
                if let Some(restore) = inherited_restore.take() {
                    frame.restore = restore;
                }
            } else if inherited_restore.is_none() {
                inherited_restore = Some(frame.restore.clone());
            }
        }
        self.frames
            .retain(|f| active.contains(&f.kind) && f.instance == instance(f.kind));
        let mut pushed = false;
        for &kind in active {
            if !self.frames.iter().any(|f| f.kind == kind) {
                self.generation = self.generation.wrapping_add(1);
                let restore = self
                    .frames
                    .last()
                    .and_then(|frame| frame.focus.get())
                    .or_else(|| self.workspace_focus.get())
                    .unwrap_or_else(|| fallback.clone());
                self.frames.push(Frame {
                    kind,
                    instance: instance(kind),
                    generation: self.generation,
                    restore,
                    focus: Default::default(),
                });
                pushed = true;
            }
        }
        if pushed || previous != self.frames.last().map(|frame| frame.kind) {
            self.focus_revision
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        self.frames
            .last()
            .filter(|f| pushed || previous != Some(f.kind))
            .map(|f| (f.kind, f.generation, pushed))
    }
    pub(super) fn focus_snapshot(&self, kind: Modal) -> crate::interaction::FocusSnapshot {
        self.frames
            .iter()
            .find(|frame| frame.kind == kind)
            .map(|frame| frame.focus.clone())
            .unwrap_or_default()
    }

    fn focus(&self, target: widget::Id, fallback: widget::Id) -> Task<Message> {
        self.guarded_focus(target, Some(fallback))
    }
    fn focus_if_present(&self, target: widget::Id) -> Task<Message> {
        self.guarded_focus(target, None)
    }
    fn guarded_focus(&self, target: widget::Id, fallback: Option<widget::Id>) -> Task<Message> {
        use cosmic::iced::advanced::widget::{Operation, operation};
        struct Matched<'a>(&'a mut bool);
        impl operation::Focusable for Matched<'_> {
            fn is_focused(&self) -> bool {
                false
            }
            fn focus(&mut self) {
                *self.0 = true;
            }
            fn unfocus(&mut self) {}
        }
        struct GuardedFocus<T> {
            revision: Arc<std::sync::atomic::AtomicU64>,
            expected: u64,
            target: widget::Id,
            fallback: Option<widget::Id>,
            probe: bool,
            found: bool,
            inner: Box<dyn Operation<T>>,
        }
        impl<T> GuardedFocus<T> {
            fn current(&self) -> bool {
                self.revision.load(std::sync::atomic::Ordering::Acquire) == self.expected
            }
        }
        impl<T: 'static> Operation<T> for GuardedFocus<T> {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<T>)) {
                if self.current() {
                    visit(self);
                }
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if self.current() {
                    if self.probe {
                        // Let native focus perform its own IdEq/alias matching
                        // without mutating the tree during this first pass.
                        self.inner
                            .focusable(id, bounds, &mut Matched(&mut self.found));
                    } else {
                        self.inner.focusable(id, bounds, state);
                    }
                }
            }
            fn finish(&self) -> operation::Outcome<T> {
                if !self.current() || !self.probe {
                    return operation::Outcome::None;
                }
                let Some(target) = (if self.found {
                    Some(self.target.clone())
                } else {
                    self.fallback.clone()
                }) else {
                    return operation::Outcome::None;
                };
                operation::Outcome::Chain(Box::new(Self {
                    revision: Arc::clone(&self.revision),
                    expected: self.expected,
                    target: target.clone(),
                    fallback: Some(target.clone()),
                    probe: false,
                    found: false,
                    inner: Box::new(operation::focusable::focus(target)),
                }))
            }
        }
        cosmic::iced::advanced::widget::operate(GuardedFocus {
            revision: Arc::clone(&self.focus_revision),
            expected: self
                .focus_revision
                .load(std::sync::atomic::Ordering::Acquire),
            probe: fallback.as_ref() != Some(&target),
            found: false,
            inner: Box::new(operation::focusable::focus(target.clone())),
            target,
            fallback,
        })
        .map(cosmic::Action::App)
    }
}

impl Cagents {
    pub(super) fn modal_surface_target(&self, modal: Modal) -> SurfaceTarget {
        SurfaceTarget {
            modal,
            revision: self.modal_stack.focus_revision.load(Ordering::Relaxed),
        }
    }

    pub(super) fn admit_modal_surface_message(&self, message: Message) -> Option<Message> {
        match message {
            Message::ModalSurface { target, message } => {
                (self.top_modal() == Some(target.modal)
                    && self.modal_stack.focus_revision.load(Ordering::Relaxed) == target.revision
                    && !matches!(&*message, Message::ModalSurface { .. }))
                .then_some(*message)
            }
            message => Some(message),
        }
    }

    /// Feature focus tasks share the modal revision, and cannot clear focus
    /// when their lower dialog is hidden by the current modal tree.
    pub(super) fn modal_focus(&self, target: widget::Id) -> Task<Message> {
        self.modal_stack.focus_if_present(target)
    }
    /// Closing a feature must not restore into the workspace or reset a
    /// surviving dialog's focused field. The stack owns those transitions.
    pub(super) fn modal_restore_focus(&self, target: widget::Id) -> Task<Message> {
        if self.modal_is_open() {
            Task::none()
        } else {
            self.modal_focus(target)
        }
    }
    pub(super) fn active_modals(&self) -> Vec<Modal> {
        let mut result = Vec::new();
        macro_rules! active {
            ($kind:ident, $condition:expr) => {
                if $condition {
                    result.push(Modal::$kind);
                }
            };
        }
        active!(PullRequest, self.pull_request_dialog.is_some());
        active!(Resume, self.resume_ui.open);
        active!(Worktree, self.worktree_dialog.is_some());
        active!(Channel, self.channel_ui.modal_open());
        result.extend(self.project_ui.modal_roles().map(Modal::Project));
        active!(Selection, self.sidebar_selection_ui.dialog.is_some());
        active!(
            DirtyClose,
            self.dirty_repository_close
                .as_ref()
                .is_some_and(|close| matches!(
                    self.cards.get(&close.card),
                    Some(CardRuntime::Repository(_))
                ))
        );
        active!(Discard, self.sidebar_state.pending_discard().is_some());
        active!(SettingsReset, self.settings_reset_confirm);
        active!(TerminalClose, self.terminal_close_dialog_visible());
        active!(ChatClose, self.chat_ui.close_dialog.is_some());
        active!(Update, self.update_ui.confirmation.is_some());
        active!(Health, self.daemon_health_ui.confirmation);
        active!(Recovery, self.session_recovery.open());
        #[cfg(feature = "integrated-browser")]
        active!(BrowserImport, self.browser_import.open());
        active!(Paste, self.terminal_paste.is_some());
        active!(Image, self.chat_ui.image_viewer.is_some());
        active!(Device, self.device_ui.open);
        active!(AutomationDraft, self.automation_ui.draft_dialog_open());
        active!(TransientClose, self.transient_work_close.is_some());
        active!(Approval, self.automation_approval_open());
        result
    }
    pub(super) fn top_modal(&self) -> Option<Modal> {
        self.modal_stack.top(&self.active_modals())
    }
    pub(super) fn modal_restore_fallback(&self) -> widget::Id {
        if self.settings_reset_confirm && self.settings_card_focused() {
            return widget::Id::new("settings.reset");
        }
        self.workspace
            .focused()
            .map(|card| self.card_keyboard_focus(card))
            .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID))
    }
    pub(super) fn reconcile_modal_backing_state(&mut self) {
        self.reconcile_chat_close_backing_state();
        if self.dirty_repository_close.as_ref().is_some_and(|close| {
            !matches!(
                self.cards.get(&close.card),
                Some(CardRuntime::Repository(_))
            )
        }) {
            self.dirty_repository_close = None;
            self.chrome_ui.closing_group.clear();
        }
    }
    pub(super) fn finish_modal_update(&mut self, task: Task<Message>) -> Task<Message> {
        let active = self.active_modals();
        // Learn the reader's choice before an opening handler suppresses it.
        // A startup/direct-handler payload without a prior frame must not
        // teach us an already-suppressed feature-local setting.
        if self.modal_stack.frames.is_empty() && active.is_empty() {
            self.modal_stack
                .adopt_keyboard_nav(self.core.keyboard_nav());
        }
        let restore = self
            .modal_stack
            .frames
            .first()
            .map(|frame| frame.restore.clone());
        let fallback = self.modal_restore_fallback();
        let project_ui = &self.project_ui;
        let chat_close = self.chat_ui.close_dialog.as_ref();
        let terminal_close = self.terminal_close.as_ref();
        let changed = self
            .modal_stack
            .reconcile_instances(&active, fallback.clone(), |kind| match kind {
                Modal::Project(role) => project_ui.modal_target(role).map(|target| target.request),
                Modal::ChatClose => chat_close.map(|dialog| Instance::Counter(dialog.generation())),
                Modal::TerminalClose => {
                    terminal_close.map(|intent| Instance::Counter(intent.generation))
                }
                _ => None,
            });
        if let Some((kind, _, pushed)) = changed {
            let initial = self.modal_initial_focus(kind);
            let target = if pushed {
                initial.clone()
            } else {
                self.modal_stack
                    .frames
                    .last()
                    .and_then(|frame| frame.focus.get())
                    .unwrap_or_else(|| initial.clone())
            };
            return Task::batch([task, self.modal_stack.focus(target, initial)]);
        }

        if self.modal_stack.frames.is_empty()
            && let Some(restore) = restore
        {
            if let Some(enabled) = self.modal_stack.workspace_keyboard_nav {
                self.restore_modal_keyboard_nav(enabled);
            }
            return Task::batch([task, self.modal_stack.focus(restore, fallback)]);
        }
        task
    }
    pub(super) fn modal_initial_focus(&self, modal: Modal) -> widget::Id {
        match modal {
            Modal::PullRequest => pull_request_dialog_focus_id(),
            Modal::Resume => resume_ui::focus_id(),
            Modal::Worktree => worktree_dialog_focus_id(),
            Modal::Channel => channel_ui::State::focus_target(),
            Modal::Project(role) => self.project_ui.focus_target_for(role),
            Modal::Selection => self
                .sidebar_selection_ui
                .dialog
                .as_ref()
                .unwrap()
                .focus_target(),
            Modal::DirtyClose => dirty_close_control_id(
                self.dirty_repository_close.as_ref().unwrap().card,
                DirtyCloseControl::Cancel,
            ),
            Modal::Discard => sidebar::discard_cancel_id(),
            Modal::SettingsReset => widget::Id::new("dialog.reset-settings.cancel"),
            Modal::TerminalClose => widget::Id::new("dialog.terminal-close.cancel"),
            Modal::ChatClose => widget::Id::new("chat-close.cancel"),
            Modal::Update => widget::Id::new("update.cancel"),
            Modal::Health => widget::Id::new("daemon-health.cancel"),
            Modal::Recovery => widget::Id::new("session-recovery.cancel"),
            #[cfg(feature = "integrated-browser")]
            Modal::BrowserImport => widget::Id::new("browser-import.cancel"),
            Modal::Paste => widget::Id::new("terminal-paste-cancel"),
            Modal::Image => widget::Id::new("image-viewer.close"),
            Modal::Device => widget::Id::new("device.close"),
            Modal::AutomationDraft => crate::automation_ui::draft_dialog_focus_id(),
            Modal::TransientClose => widget::Id::new("dialog.transient-close.cancel"),
            Modal::Approval => automation_approval::default_focus_id(),
        }
    }
    pub(super) fn cancel_top_modal(&mut self) -> Task<Message> {
        match self.top_modal() {
            None => return Task::none(),
            Some(top) => match top {
                Modal::Channel => self.handle_channel_ui(channel_ui::Action::Dismiss),
                Modal::Project(role) => self.dismiss_project_modal(role),
                Modal::Selection => {
                    self.update_sidebar_selection(sidebar_selection_ui::Action::Dismiss)
                }
                Modal::DirtyClose => self.cancel_dirty_close(),
                Modal::Discard => self.update(Message::Sidebar(SidebarAction::CancelDiscard)),
                Modal::SettingsReset => self.close_settings_reset(),
                Modal::TerminalClose => self.cancel_terminal_close(),
                Modal::ChatClose => self.cancel_chat_close(),
                Modal::Update => self.update_update_ui(update_ui::Action::CancelApply),
                #[cfg(feature = "integrated-browser")]
                Modal::BrowserImport => self.cancel_browser_import(),
                Modal::Paste => self.handle_terminal_find(terminal_ui::Action::ConfirmPaste(
                    self.terminal_paste.as_ref().unwrap().0,
                    false,
                )),
                Modal::Image => {
                    self.chat_ui.image_viewer = None;
                    Task::none()
                }
                Modal::Device => self.update_device(device_ui::Action::Close),
                Modal::AutomationDraft => self
                    .automation_ui
                    .keep_draft_action()
                    .map_or_else(Task::none, |action| self.update_automation_ui(action)),
                Modal::TransientClose => self.update(Message::CancelTransientWorkClose),
                Modal::Resume => self.update_resume(resume_ui::Action::Close),
                Modal::Worktree => {
                    if let Some(dialog) = self.worktree_dialog.take() {
                        dialog.cancel();
                    }
                    Task::none()
                }
                Modal::PullRequest => {
                    self.pull_request_dialog = None;
                    self.pull_request_dialog_generation =
                        self.pull_request_dialog_generation.wrapping_add(1);
                    Task::none()
                }
                Modal::Approval => self.cancel_automation_approval(),
                Modal::Recovery => self.update_session_recovery(session_recovery::Action::Cancel),
                Modal::Health => self.update_daemon_health_ui(daemon_health_ui::Action::Cancel),
            },
        }
    }
    pub(super) fn modal_key_event(
        &mut self,
        event: &keyboard::Event,
        captured: bool,
    ) -> Option<Task<Message>> {
        let top = self.top_modal()?;
        if let keyboard::Event::ModifiersChanged(modifiers) = event {
            self.keyboard_modifiers = *modifiers;
        }
        if top == Modal::Approval {
            return self
                .automation_approval_key(event, captured)
                .or_else(|| Some(Task::none()));
        }
        if top == Modal::Recovery {
            return self
                .session_recovery_key_event(event, captured)
                .or_else(|| Some(Task::none()));
        }
        if top == Modal::Health {
            return self
                .daemon_health_key_event(event, captured)
                .or_else(|| Some(Task::none()));
        }
        if is_escape_press(event) {
            return Some(self.cancel_top_modal());
        }
        if let Some(backwards) = is_tab_press(event) {
            if top == Modal::SettingsReset {
                self.settings_reset_focus_confirm = !self.settings_reset_focus_confirm;
                return Some(self.modal_focus(widget::Id::new(
                    if self.settings_reset_focus_confirm {
                        "dialog.reset-settings.confirm"
                    } else {
                        "dialog.reset-settings.cancel"
                    },
                )));
            }
            if top == Modal::TerminalClose {
                return Some(self.cycle_terminal_close_focus(backwards));
            }
            if top == Modal::DirtyClose {
                return Some(self.cycle_dirty_close_focus(backwards));
            }
            if !captured && !self.core.keyboard_nav() {
                return Some(
                    if backwards {
                        iced::widget::operation::focus_previous()
                    } else {
                        iced::widget::operation::focus_next()
                    }
                    .map(cosmic::Action::App),
                );
            }
        }
        if top == Modal::Resume {
            return self.resume_key_event(event).or_else(|| Some(Task::none()));
        }
        if top == Modal::Image
            && let keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                ..
            } = event
        {
            match key {
                keyboard::key::Named::ArrowLeft => {
                    self.chat_ui.image_viewer.as_mut().unwrap().next(false)
                }
                keyboard::key::Named::ArrowRight => {
                    self.chat_ui.image_viewer.as_mut().unwrap().next(true)
                }
                _ => {}
            }
        }
        Some(Task::none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> Cagents {
        super::super::tests::context_inspector_test_app(true)
    }
    fn escape(app: &mut Cagents) {
        drop(app.update(Message::KeyEvent {
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
        }));
    }

    fn emitted_reset_cancel(app: &Cagents) -> Message {
        use iced::advanced::{Layout, Shell, clipboard, layout, widget::{Tree, operation}};
        let renderer = super::super::tests::headless_renderer();
        let mut dialog = app.dialog().expect("visible dialog");
        let mut tree = Tree::new(&dialog);
        let bounds = iced::Rectangle::with_size(iced::Size::new(800.0, 600.0));
        let node = dialog.as_widget_mut().layout(&mut tree, &renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()));
        dialog.as_widget_mut().operate(&mut tree, Layout::new(&node), &renderer,
            &mut operation::focusable::focus::<()>(widget::Id::new("dialog.reset-settings.cancel")));
        let mut messages = Vec::new();
        dialog.as_widget_mut().update(&mut tree, &iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(), text: None, repeat: false,
        }), Layout::new(&node), iced::mouse::Cursor::Unavailable, &renderer,
            &mut clipboard::Null, &mut Shell::new(&mut messages), &bounds);
        messages.into_iter().find(|message| matches!(message,
            Message::ModalSurface { message, .. } if matches!(&**message, Message::CancelResetSettings)))
            .expect("actual Cancel widget publishes its surface-bound decision")
    }

    #[test]
    fn held_widget_decision_cannot_close_a_hidden_or_reopened_modal() {
        let mut app = app();
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        let old = emitted_reset_cancel(&app);
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        let revision = app.modal_stack.focus_revision.load(Ordering::Relaxed);
        drop(app.update(old.clone()));
        assert!(app.settings_reset_confirm && app.resume_ui.open);
        assert_eq!(app.modal_stack.focus_revision.load(Ordering::Relaxed), revision,
            "rejected UI messages must not run reconciliation or focus side effects");
        escape(&mut app);
        drop(app.update(emitted_reset_cancel(&app)));
        assert!(!app.settings_reset_confirm);
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        drop(app.update(old));
        assert!(app.settings_reset_confirm, "reopening the same kind creates a new UI owner");
        drop(app.update(emitted_reset_cancel(&app)));
        assert!(!app.modal_is_open());
    }

    #[test]
    fn lower_only_removal_keeps_the_visible_widget_decision_valid() {
        let mut app = app();
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        let current = emitted_reset_cancel(&app);
        app.resume_ui.open = false;
        drop(app.finish_modal_update(Task::none()));
        drop(app.update(current));
        assert!(!app.modal_is_open());
    }
    #[test]
    fn escape_closes_the_visible_top_modal_only() {
        let mut app = app();
        app.settings_reset_confirm = true;
        let active = app.active_modals();
        app.modal_stack
            .reconcile(&active, widget::Id::new("editor.caret"));
        app.resume_ui.open = true;
        assert_eq!(app.top_modal(), Some(Modal::Resume));
        assert!(app.dialog().is_some());
        escape(&mut app);
        assert!(!app.resume_ui.open);
        assert!(app.settings_reset_confirm);
        assert_eq!(app.top_modal(), Some(Modal::SettingsReset));
        escape(&mut app);
        assert!(!app.modal_is_open());
    }
    #[test]
    fn vanished_backing_state_cannot_block_or_hide_a_lower_dialog() {
        let mut app = app();
        app.settings_reset_confirm = true;
        // A repository runtime can disappear when another close finishes.
        app.dirty_repository_close = Some(DirtyRepositoryClose {
            card: CardId::new(),
            restore_card: None,
            focused: DirtyCloseControl::Cancel,
            intent: DirtyCloseIntent::Card(CardId::new()),
            remaining: VecDeque::new(),
        });
        assert_eq!(app.top_modal(), Some(Modal::SettingsReset));
        assert!(app.dialog().is_some());
        app.reconcile_modal_backing_state();
        assert!(app.dirty_repository_close.is_none());
        escape(&mut app);
        assert!(!app.modal_is_open());
        assert!(app.dialog().is_none());
    }
    fn observed_input(
        snapshot: crate::interaction::FocusSnapshot,
        id: widget::Id,
    ) -> Element<'static, Message> {
        crate::interaction::observe_focus(
            crate::ui_controls::field::text("Original input", "draft")
                .id(id)
                .on_input(|_| Message::DismissStatus),
            snapshot,
        )
    }
    #[test]
    fn closing_restores_the_focus_observed_before_the_modal_opened() {
        let mut app = app();
        let id = widget::Id::new("fixture.original-input");
        let focus = iced::widget::operation::focus(id.clone()).map(cosmic::Action::App);
        assert_eq!(
            super::super::tests::focused_element(
                observed_input(app.modal_stack.workspace_focus.clone(), id.clone()),
                focus
            ),
            Some(id.clone())
        );
        app.settings_reset_confirm = true;
        let active = app.active_modals();
        app.modal_stack
            .reconcile(&active, app.modal_restore_fallback());
        app.settings_reset_confirm = false;
        let task = app.finish_modal_update(Task::none());
        assert_eq!(
            super::super::tests::focused_element(
                observed_input(app.modal_stack.workspace_focus.clone(), id.clone()),
                task
            ),
            Some(id)
        );
    }
    #[test]
    fn stacked_modal_restores_the_lower_modals_observed_field() {
        let mut app = app();
        app.resume_ui.open = true;
        app.modal_stack
            .reconcile(&app.active_modals(), app.modal_restore_fallback());
        let id = widget::Id::new("fixture.lower-modal-field");
        let snapshot = app.modal_stack.focus_snapshot(Modal::Resume);
        assert_eq!(
            super::super::tests::focused_element(
                observed_input(snapshot.clone(), id.clone()),
                iced::widget::operation::focus(id.clone()).map(cosmic::Action::App)
            ),
            Some(id.clone())
        );
        app.settings_reset_confirm = true;
        app.modal_stack
            .reconcile(&app.active_modals(), app.modal_restore_fallback());
        app.settings_reset_confirm = false;
        let restore = app.finish_modal_update(Task::none());
        assert_eq!(
            super::super::tests::focused_element(observed_input(snapshot, id.clone()), restore),
            Some(id)
        );
    }
    #[test]
    fn focus_is_immediate_and_a_queued_old_operation_cannot_steal_new_modal_focus() {
        use cosmic::iced::futures::{FutureExt, StreamExt};
        let mut app = app();
        app.settings_reset_confirm = true;
        let never_finishes = cosmic::task::future(std::future::pending::<Message>());
        let task = app.finish_modal_update(never_finishes);
        let mut stream = iced::runtime::task::into_stream(task).unwrap();
        // Opening focus cannot wait for a picker indexer or save stream to end.
        let action = stream
            .next()
            .now_or_never()
            .flatten()
            .expect("focus is ready while the feature stream remains pending");
        let iced::runtime::Action::Widget(operation) = action else {
            panic!("focus widget operation");
        };
        app.settings_reset_confirm = false;
        drop(app.finish_modal_update(Task::none()));
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        let current = widget::Id::new("fixture.new-modal-input");
        let content: Element<'_, Message> = widget::column::with_children(vec![
            observed_input(
                Default::default(),
                widget::Id::new("dialog.reset-settings.cancel"),
            ),
            observed_input(Default::default(), current.clone()),
        ])
        .into();
        let stale = iced::advanced::widget::operate(operation)
            .map(|_| cosmic::Action::App(Message::DismissStatus));
        let task = iced::widget::operation::focus(current.clone())
            .map(cosmic::Action::App)
            .chain(stale);
        assert_eq!(
            super::super::tests::focused_element(content, task),
            Some(current)
        );
    }

    #[test]
    fn held_feature_open_resume_focus_cannot_steal_a_new_dialogs_focus() {
        use cosmic::iced::futures::{FutureExt, StreamExt};
        let mut app = app();
        let feature = app.update_resume(resume_ui::Action::Open);
        assert!(app.resume_ui.open);
        let mut stream = iced::runtime::task::into_stream(feature).unwrap();
        let action = stream
            .next()
            .now_or_never()
            .flatten()
            .expect("opening focus ready");
        let iced::runtime::Action::Widget(operation) = action else {
            panic!("feature widget focus");
        };
        drop(app.finish_modal_update(Task::none()));
        drop(app.update_resume(resume_ui::Action::Close));
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        let current = widget::Id::new("dialog.reset-settings.confirm");
        let content: Element<'_, Message> = widget::column::with_children(vec![
            observed_input(Default::default(), resume_ui::focus_id()),
            observed_input(Default::default(), current.clone()),
        ])
        .into();
        let held = iced::advanced::widget::operate(operation)
            .map(|_| cosmic::Action::App(Message::DismissStatus));
        let task = iced::widget::operation::focus(current.clone())
            .map(cosmic::Action::App)
            .chain(held);
        assert_eq!(
            super::super::tests::focused_element(content, task),
            Some(current)
        );
    }

    #[test]
    fn current_revision_lower_feature_focus_leaves_upper_focus_untouched() {
        let mut app = app();
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        let current = widget::Id::new("dialog.reset-settings.confirm");
        let content = observed_input(Default::default(), current.clone());
        let task = iced::widget::operation::focus(current.clone())
            .map(cosmic::Action::App)
            .chain(app.modal_focus(resume_ui::focus_id()));
        assert_eq!(
            super::super::tests::focused_element(content, task),
            Some(current)
        );
    }

    #[test]
    fn upper_form_tabs_when_a_lower_confirmation_disabled_cosmic_navigation() {
        let mut app = app();
        app.resume_ui.open = true;
        app.core.set_keyboard_nav(false);
        let first = widget::Id::new("fixture.first-field");
        let second = widget::Id::new("fixture.second-field");
        let task = app
            .modal_key_event(
                &keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Tab),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Tab),
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                },
                false,
            )
            .unwrap();
        let content: Element<'_, Message> = widget::column::with_children(vec![
            observed_input(Default::default(), first.clone()),
            observed_input(Default::default(), second.clone()),
        ])
        .into();
        let task = iced::widget::operation::focus(first)
            .map(cosmic::Action::App)
            .chain(task);
        assert_eq!(
            super::super::tests::focused_element(content, task),
            Some(second)
        );
    }

    fn focus_input(snapshot: crate::interaction::FocusSnapshot, id: widget::Id) {
        assert_eq!(
            super::super::tests::focused_element(
                observed_input(snapshot, id.clone()),
                iced::widget::operation::focus(id.clone()).map(cosmic::Action::App)
            ),
            Some(id)
        );
    }

    #[test]
    fn removing_a_middle_modal_restores_the_surviving_lower_field() {
        let mut app = app();
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        let id = widget::Id::new("fixture.surviving-lower-field");
        let snapshot = app.modal_stack.focus_snapshot(Modal::Resume);
        focus_input(snapshot.clone(), id.clone());
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        app.device_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = false;
        drop(app.finish_modal_update(Task::none()));
        app.device_ui.open = false;
        let restore = app.finish_modal_update(Task::none());
        assert_eq!(
            super::super::tests::focused_element(observed_input(snapshot, id.clone()), restore),
            Some(id)
        );
    }

    #[test]
    fn removing_the_bottom_modal_preserves_the_original_workspace_restore_link() {
        let mut app = app();
        let id = widget::Id::new("fixture.root-before-bottom-removal");
        let snapshot = app.modal_stack.workspace_focus.clone();
        focus_input(snapshot.clone(), id.clone());
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        app.device_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        app.resume_ui.open = false;
        drop(app.finish_modal_update(Task::none()));
        app.device_ui.open = false;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = false;
        let restore = app.finish_modal_update(Task::none());
        assert_eq!(
            super::super::tests::focused_element(observed_input(snapshot, id.clone()), restore),
            Some(id)
        );
    }

    #[test]
    fn removing_a_lower_modal_keeps_the_queued_visible_modal_focus_valid() {
        let mut app = app();
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = true;
        let focus = app.finish_modal_update(Task::none());
        app.resume_ui.open = false;
        drop(app.finish_modal_update(Task::none()));
        let id = widget::Id::new("dialog.reset-settings.cancel");
        assert_eq!(
            super::super::tests::focused_element(
                observed_input(Default::default(), id.clone()),
                focus
            ),
            Some(id)
        );
    }

    #[test]
    fn an_unregistered_dialog_cannot_overwrite_the_workspace_focus_snapshot() {
        let mut app = app();
        let id = widget::Id::new("fixture.original-before-unregistered-dialog");
        focus_input(app.modal_stack.workspace_focus.clone(), id.clone());
        app.settings_reset_confirm = true;
        let cancel = widget::Id::new("dialog.reset-settings.cancel");
        assert_eq!(
            super::super::tests::focused_element(
                app.dialog().unwrap(),
                iced::widget::operation::focus(cancel.clone()).map(cosmic::Action::App)
            ),
            Some(cancel)
        );
        assert_eq!(app.modal_stack.workspace_focus.get(), Some(id));
    }

    #[test]
    fn cancelling_through_update_restores_the_observed_workspace_input() {
        let mut app = app();
        let id = widget::Id::new("fixture.update-cancel-origin");
        let snapshot = app.modal_stack.workspace_focus.clone();
        focus_input(snapshot.clone(), id.clone());
        drop(app.update(Message::ResetSettings));
        let cancel = app.update(Message::CancelResetSettings);
        assert_eq!(
            super::super::tests::focused_element(observed_input(snapshot, id.clone()), cancel),
            Some(id)
        );
    }

    #[test]
    fn vanished_top_backing_state_restores_focus_during_update_entry() {
        let mut app = app();
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        let id = widget::Id::new("fixture.lower-after-vanished-runtime");
        let snapshot = app.modal_stack.focus_snapshot(Modal::Resume);
        focus_input(snapshot.clone(), id.clone());
        let card = CardId::new();
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(
                repository_widget::RepositoryRuntime::dirty_fixture(card),
            )),
        );
        app.dirty_repository_close = Some(DirtyRepositoryClose {
            card,
            restore_card: None,
            focused: DirtyCloseControl::Cancel,
            intent: DirtyCloseIntent::Card(card),
            remaining: VecDeque::new(),
        });
        drop(app.finish_modal_update(Task::none()));
        app.cards.remove(&card);
        let restore = app.update(Message::DismissStatus);
        assert!(app.dirty_repository_close.is_none());
        assert_eq!(app.top_modal(), Some(Modal::Resume));
        assert_eq!(
            super::super::tests::focused_element(observed_input(snapshot, id.clone()), restore),
            Some(id)
        );
    }

    #[test]
    fn a_removed_workspace_origin_falls_back_to_the_current_pane_control() {
        let mut app = app();
        let missing = widget::Id::new("fixture.origin-that-unmounted");
        focus_input(app.modal_stack.workspace_focus.clone(), missing);
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = false;
        let fallback = app.modal_restore_fallback();
        let restore = app.finish_modal_update(Task::none());
        assert_eq!(
            super::super::tests::focused_element(
                observed_input(Default::default(), fallback.clone()),
                restore
            ),
            Some(fallback)
        );
    }

    #[test]
    fn a_removed_lower_field_falls_back_to_that_modals_initial_control() {
        let mut app = app();
        app.resume_ui.open = true;
        drop(app.finish_modal_update(Task::none()));
        focus_input(
            app.modal_stack.focus_snapshot(Modal::Resume),
            widget::Id::new("fixture.lower-field-that-unmounted"),
        );
        app.settings_reset_confirm = true;
        drop(app.finish_modal_update(Task::none()));
        app.settings_reset_confirm = false;
        let fallback = app.modal_initial_focus(Modal::Resume);
        let restore = app.finish_modal_update(Task::none());
        assert_eq!(
            super::super::tests::focused_element(
                observed_input(Default::default(), fallback.clone()),
                restore
            ),
            Some(fallback)
        );
    }

    #[test]
    fn full_unwind_preserves_both_workspace_keyboard_navigation_choices() {
        for enabled in [false, true] {
            let mut app = app();
            app.core.set_keyboard_nav(enabled);
            drop(app.finish_modal_update(Task::none()));
            app.settings_reset_confirm = true;
            app.settings_reset_keyboard_nav = Some(enabled);
            app.core.set_keyboard_nav(false);
            drop(app.finish_modal_update(Task::none()));
            app.resume_ui.open = true;
            drop(app.finish_modal_update(Task::none()));
            drop(app.close_settings_reset());
            drop(app.finish_modal_update(Task::none()));
            assert_eq!(app.top_modal(), Some(Modal::Resume));
            app.resume_ui.open = false;
            drop(app.finish_modal_update(Task::none()));
            assert_eq!(app.core.keyboard_nav(), enabled);
        }
    }

    #[test]
    fn direct_handler_without_a_workspace_snapshot_keeps_its_original_choice() {
        for enabled in [false, true] {
            let mut app = app();
            // This fixture bypassed the update entry and already suppressed
            // native traversal before its first registered frame existed.
            app.settings_reset_confirm = true;
            app.settings_reset_keyboard_nav = Some(enabled);
            app.core.set_keyboard_nav(false);
            drop(app.finish_modal_update(Task::none()));
            drop(app.update(Message::CancelResetSettings));
            assert_eq!(app.core.keyboard_nav(), enabled);
        }
    }

    #[test]
    fn dirty_close_open_and_retry_keep_the_focused_control_on_cancel() {
        let mut app = app();
        let card = app.workspace.focused().unwrap();
        let (runtime, _) = RepositoryRuntime::post_persist_failure_fixture(card);
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        let opening = app.begin_dirty_close(DirtyCloseIntent::Card(card), VecDeque::from([card]));
        assert_eq!(
            app.dirty_repository_close.as_ref().unwrap().focused,
            DirtyCloseControl::Cancel
        );
        let cancel = dirty_close_control_id(card, DirtyCloseControl::Cancel);
        assert_eq!(
            super::super::tests::focused_element(app.dialog().unwrap(), opening),
            Some(cancel.clone())
        );
        // An unsuccessful save/close leaves the same confirmation present;
        // resetting it must not silently move onto Save or Discard.
        let retry = app.advance_dirty_close();
        assert_eq!(
            app.dirty_repository_close.as_ref().unwrap().focused,
            DirtyCloseControl::Cancel
        );
        assert_eq!(
            super::super::tests::focused_element(app.dialog().unwrap(), retry),
            Some(cancel)
        );
    }

    #[test]
    fn safe_confirmation_initial_focus_targets_are_nonempty() {
        let app = app();
        for (modal, expected) in [
            (Modal::Channel, "dialog.channel.close"),
            (
                Modal::Project(project_ui::ModalRole::WorktreeDelete),
                "dialog.project.close",
            ),
            (Modal::Discard, "sidebar.discard.cancel"),
            (Modal::SettingsReset, "dialog.reset-settings.cancel"),
            (Modal::TerminalClose, "dialog.terminal-close.cancel"),
            (Modal::ChatClose, "chat-close.cancel"),
            (Modal::Update, "update.cancel"),
            (Modal::Health, "daemon-health.cancel"),
            (Modal::Recovery, "session-recovery.cancel"),
            (Modal::Paste, "terminal-paste-cancel"),
        ] {
            assert_eq!(app.modal_initial_focus(modal), widget::Id::new(expected));
        }
    }
}
