//! A remote close is proved by canonical view retirement, not by losing a socket.
use super::*;

pub(in crate::app) fn closed_draft_source_matches(
    source: &chat::Session,
    current: &chat::Session,
) -> bool {
    source
        .proof
        .as_ref()
        .is_some_and(HostSessionProof::is_complete)
        && crate::agent_execution::same_chat_process(source, current)
        && source.launch.session_id == current.launch.session_id
}

impl Cagents {
    pub(super) fn chat_closed_elsewhere(&self, card: CardId) -> bool {
        self.shared_views.closed_chats.get(&card).is_some_and(|source| {
            matches!(self.card_state(card).map(|card| &card.kind), Some(CardKind::Chat(session)) if closed_draft_source_matches(source, session))
                && !self.workspace_model.views.iter().any(|view| view.panes.iter().any(|pane| pane.id == card))
                && match self.cards.get(&card) {
                    Some(CardRuntime::Chat(runtime)) => closed_draft_source_matches(source, &runtime.session),
                    _ => true,
                }
        })
    }

    pub(in crate::app) fn sync_closed_chat_draft(&mut self, card: CardId) {
        let closed = self.chat_closed_elsewhere(card);
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
            runtime.closed_elsewhere = closed;
        }
        if let Some(runtime) = self.chat_ui.parked.get_mut(&card) {
            runtime.closed_elsewhere = closed;
        }
    }

    pub(in crate::app) fn clear_closed_chat_drafts(&mut self) {
        for card in self.cards.values_mut() {
            if let CardRuntime::Chat(runtime) = card {
                runtime.closed_elsewhere = false;
            }
        }
        for runtime in self.chat_ui.parked.values_mut() {
            runtime.closed_elsewhere = false;
        }
    }

    pub(super) fn copy_closed_chat_draft(
        &mut self,
        target: Target,
        revision: u64,
    ) -> Task<Message> {
        if !self.chat_closed_elsewhere(target.card) {
            return Task::none();
        }
        let Some(runtime) = self
            .chat_runtime_mut(target)
            .filter(|runtime| runtime.closed_elsewhere && runtime.revision == revision)
        else {
            return Task::none();
        };
        let text = runtime.unsent_text();
        if text.is_empty() {
            return Task::none();
        }
        let task = self.handle_chat_ui(Action::CopyLiteral(text));
        self.status = Some(TransientNotice::success("Copied draft"));
        task
    }

    pub(super) fn discard_closed_chat_draft(
        &mut self,
        target: Target,
        revision: u64,
    ) -> Task<Message> {
        if !self.chat_closed_elsewhere(target.card) || self.chat_start_pending(target.card) {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target).filter(|runtime| {
            runtime.closed_elsewhere
                && runtime.revision == revision
                && runtime.closed_draft_can_discard()
        }) else {
            return Task::none();
        };
        runtime.replace_draft(text_editor::Content::new());
        runtime.history_cursor = None;
        runtime.history_stash = None;
        self.chat_ui.drafts.remove(&target.card);
        self.persist();
        Task::none()
    }
}

impl Runtime {
    pub(super) fn has_unsent_text(&self) -> bool {
        let content = self
            .history_stash
            .as_ref()
            .filter(|_| self.history_cursor.is_some())
            .unwrap_or(&self.composer);
        !content.is_empty()
    }

    pub(super) fn expected_remote_close_error(&self, error: &str) -> bool {
        self.closed_elsewhere
            && error == "Exact native chat is not hosted; explicit recovery is required"
    }

    fn closed_draft_can_discard(&self) -> bool {
        self.pending.is_none()
            && self.queued.is_none()
            && self.followups.is_empty()
            && self.recalling.is_empty()
            && self.attachment_chooser_pending.is_none()
            && self.pastes_pending == 0
            && self.image_pastes_saving == 0
    }

    pub(super) fn closed_draft_controls(&self, transitioning: bool) -> Element<'_, Message> {
        let target = self.target;
        let revision = self.revision;
        let has_text = self.has_unsent_text();
        widget::column::with_children(vec![
            design::body("Closed in another window. Your conversation and unsent draft are kept here. Copy the draft into another conversation, or discard its text. Attachments remain until you remove them.").into(),
            widget::flex_row(vec![
                widget::button::custom(design::caption("Copy draft"))
                    .class(crate::ui_controls::control_style(false, None))
                    .on_press_maybe(has_text.then(|| Message::ChatUi(Action::CopyClosedDraft(target, revision))))
                    .into(),
                widget::button::custom(design::caption("Discard draft text"))
                    .class(crate::ui_controls::control_style(false, None))
                    .on_press_maybe((has_text && !transitioning && self.closed_draft_can_discard()).then(|| Message::ChatUi(Action::DiscardClosedDraft(target, revision))))
                    .into(),
            ]).spacing(space::S as u16).into(),
        ]).spacing(space::S).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_model::views::{Pane, Scope, View};
    use cosmic::iced::futures::{FutureExt, StreamExt};

    fn fixture(draft: &str) -> (Cagents, Target, crate::model::ChannelId, Arc<ClientHandle>) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.new_native_chat(Provider::Claude, Some(PathBuf::from("/project"))));
        let card = app.workspace.focused().unwrap();
        let runtime = match app.cards.get_mut(&card).unwrap() {
            CardRuntime::Chat(runtime) => runtime,
            _ => panic!("chat runtime"),
        };
        runtime.session.launch.session_id = Some("closed-draft-thread".into());
        runtime.session.proof = Some(HostSessionProof {
            host_session_id: "fixture-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-incarnation".into(),
        });
        let session = runtime.session.clone();
        let handle = Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                finished_at: Some(42),
                session_id: session.launch.session_id.clone(),
                items: vec![Arc::new(chat::Item {
                    id: "completed-report".into(),
                    role: chat::Role::Assistant,
                    at: Some(42),
                    started_at: None,
                    content: ItemContent::Text {
                        text: "The completed report stays readable.".into(),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                })],
                ..Snapshot::default()
            },
        ));
        runtime.connection = Connection::Live(handle.clone());
        // The test renders a visible completed report. Hidden runtimes defer
        // snapshot hydration, just as they do in the actual UI.
        runtime.show(true);
        assert_eq!(runtime.snapshot.finished_at, Some(42));
        assert_eq!(runtime.snapshot.items.len(), 1);
        assert_eq!(runtime.snapshot.items[0].id, "completed-report");
        runtime.replace_draft(text_editor::Content::with_text(draft));
        let target = runtime.target;
        let state = app.card_state_mut(card).unwrap();
        state.kind = CardKind::Chat(session);
        let pane = Pane::from_card(state, None).unwrap();
        let channel = crate::model::ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            id: channel,
            project_id: crate::model::ProjectId::new(),
            title: "closed draft".into(),
            title_source: Default::default(),
            ordinal: 1,
            shared: true,
            cards: vec![card],
            focused: Some(card),
        });
        // Keep this scope inactive so the projection does not add placeholders.
        let mut view = View::empty(channel);
        view.panes.push(pane);
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.views = vec![view];
        app.acknowledge_view_edit(Scope::Channel(channel), None);
        drop(app.reconcile_shared_views());
        (app, target, channel, handle)
    }

    fn retire(app: &mut Cagents, channel: crate::model::ChannelId, card: CardId) {
        let view = app
            .workspace_model
            .views
            .iter_mut()
            .find(|view| view.scope == Scope::Channel(channel))
            .unwrap();
        view.revision += 1;
        view.panes.clear();
        view.retired.push(card);
        drop(app.reconcile_shared_views());
    }

    fn runtime(app: &Cagents, target: Target) -> &Runtime {
        match app.cards.get(&target.card).unwrap() {
            CardRuntime::Chat(runtime) => runtime,
            _ => panic!("chat retained"),
        }
    }

    fn actions(task: Task<Message>) -> Vec<iced::runtime::Action<cosmic::Action<Message>>> {
        let Some(mut stream) = iced::runtime::task::into_stream(task) else {
            return Vec::new();
        };
        let mut result = Vec::new();
        while let Some(action) = stream
            .next()
            .now_or_never()
            .expect("local draft actions must be immediate, never a provider task")
        {
            result.push(action);
        }
        result
    }

    fn visible_text(app: &mut Cagents, card: CardId) -> Vec<String> {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        #[derive(Default)]
        struct Texts(Vec<String>);
        impl Operation for Texts {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, text: &str) {
                self.0.push(text.to_owned());
            }
        }
        // These tests set the local error directly instead of dispatching a
        // provider callback. Run the same postlude as handle_chat_ui, then
        // settle the real asynchronous presentation before inspecting widgets.
        app.reconcile_chat_failure_presentations();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("retained chat fixture");
        };
        preparation::drain_fixture(runtime);
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app.chat_view(card);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(900.0, 980.0)),
        );
        let mut text = Texts::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut text);
        text.0
    }

    #[test]
    fn remote_closed_draft_retains_report_and_draft_without_reporting_execution_failure() {
        let (mut app, target, channel, _) = fixture("unsent\nkeep exact spaces  ");
        let report = runtime(&app, target).snapshot.items[0].clone();
        retire(&mut app, channel, target.card);
        assert!(app.durable.local_only_cards.contains(&target.card));
        let runtime = runtime(&app, target);
        assert!(runtime.closed_elsewhere);
        assert_eq!(runtime.unsent_text(), "unsent\nkeep exact spaces  ");
        assert!(Arc::ptr_eq(&runtime.snapshot.items[0], &report));
        assert_eq!(
            runtime.conversation_status(false).1,
            "Closed · draft retained"
        );
        assert!(runtime.expected_remote_close_error(
            "Exact native chat is not hosted; explicit recovery is required"
        ));
        assert!(!runtime.expected_remote_close_error("Provider failed while writing the report"));
        assert!(!app.chat_start_pending(target.card));
        assert!(app.card_state(target.card).unwrap().existence.is_open());
        app.chat_runtime_mut(target).unwrap().error =
            Some("Exact native chat is not hosted; explicit recovery is required".into());
        let text = visible_text(&mut app, target.card).join("\n");
        assert!(
            text.contains("The completed report stays readable."),
            "{text}"
        );
        assert!(
            text.contains(
                "Closed in another window. Your conversation and unsent draft are kept here."
            ),
            "{text}"
        );
        assert!(
            text.contains("Copy draft") && text.contains("Discard draft text"),
            "{text}"
        );
        assert!(
            !text.contains("Finished with error")
                && !text.contains("explicit recovery is required"),
            "{text}"
        );
    }

    #[test]
    fn remote_closed_draft_requires_retirement_and_no_current_owner() {
        let (mut app, target, channel, _) = fixture("saved text");
        let error = "Exact native chat is not hosted; explicit recovery is required";
        app.chat_runtime_mut(target).unwrap().error = Some(error.into());
        drop(app.reconcile_shared_views());
        assert!(!runtime(&app, target).closed_elsewhere);
        assert_eq!(
            runtime(&app, target).conversation_status(false).1,
            "Finished with error"
        );
        assert!(!runtime(&app, target).expected_remote_close_error(error));
        let text = visible_text(&mut app, target.card).join("\n");
        assert!(text.contains(error), "{text}");
        assert!(text.contains("Copy details"), "{text}");
        assert!(
            text.contains("The completed report stays readable."),
            "{text}"
        );
        assert!(!text.contains("Closed in another window."), "{text}");
        let pane = app.workspace_model.views[0].panes[0].clone();
        // A source tombstone can also be a transfer. Destination ownership wins.
        let mut destination = View::empty(crate::model::ChannelId::new());
        destination.panes.push(pane);
        app.workspace_model.views.push(destination);
        retire(&mut app, channel, target.card);
        assert!(!app.chat_closed_elsewhere(target.card));
        assert!(!runtime(&app, target).closed_elsewhere);

        // A replacement registry has no authority to repeat an old registry's
        // closure state; resetting shared metadata must also reset its UI flag.
        let (mut app, target, channel, _) = fixture("survives registry replacement");
        retire(&mut app, channel, target.card);
        assert!(runtime(&app, target).closed_elsewhere);
        let mut replacement = crate::workspace_model::WorkspaceModel::default();
        replacement.identity = uuid::Uuid::new_v4();
        drop(app.apply_workspace_model_update(
            crate::app::workspace_model_sync::ModelUpdate::Connected(replacement),
        ));
        assert!(!runtime(&app, target).closed_elsewhere);
        assert!(!app.chat_closed_elsewhere(target.card));
        assert_eq!(
            runtime(&app, target).unsent_text(),
            "survives registry replacement"
        );
    }

    #[test]
    fn remote_closed_draft_copy_and_discard_are_exact_and_cannot_restart_the_actor() {
        let (mut app, target, channel, handle) = fixture("Recover this exact draft");
        retire(&mut app, channel, target.card);
        let revision = runtime(&app, target).revision;
        let result = actions(app.handle_chat_ui(Action::CopyClosedDraft(target, revision)));
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0], iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write { contents, .. }) if contents == "Recover this exact draft")
        );
        for action in [
            Action::Reconnect(target),
            Action::StartFresh(target.card),
            Action::Submit(target, PromptMode::Send),
        ] {
            assert!(actions(app.handle_chat_ui(action)).is_empty());
            assert!(!app.chat_start_pending(target.card));
        }
        let report = runtime(&app, target).snapshot.items[0].clone();
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("Newer draft"));
        assert!(
            actions(app.handle_chat_ui(Action::DiscardClosedDraft(target, revision))).is_empty()
        );
        assert_eq!(runtime(&app, target).unsent_text(), "Newer draft");
        let revision = runtime(&app, target).revision;
        assert!(
            actions(app.handle_chat_ui(Action::DiscardClosedDraft(target, revision))).is_empty()
        );
        assert!(runtime(&app, target).unsent_text().is_empty());
        assert!(Arc::ptr_eq(
            &runtime(&app, target).snapshot.items[0],
            &report
        ));
        assert!(Arc::ptr_eq(
            runtime(&app, target).handle().unwrap(),
            &handle
        ));
        drop(app.reconcile_shared_views());
        assert!(app.card_state(target.card).unwrap().existence.is_open());
    }

    #[test]
    fn remote_closed_draft_discard_refuses_active_local_work_and_replaced_targets() {
        let (mut app, target, channel, _) = fixture("still mine");
        retire(&mut app, channel, target.card);
        let revision = runtime(&app, target).revision;
        app.chat_runtime_mut(target).unwrap().pastes_pending = 1;
        assert!(
            actions(app.handle_chat_ui(Action::DiscardClosedDraft(target, revision))).is_empty()
        );
        assert_eq!(runtime(&app, target).unsent_text(), "still mine");
        app.chat_runtime_mut(target).unwrap().pastes_pending = 0;
        // Exercise the original ordering through actual start/Ready handlers:
        // the replacement is installed before the old canonical tombstone,
        // while its descriptor update has not been acknowledged.
        let (mut app, old_target, channel, _) = fixture("replacement's draft");
        let mut replacement = runtime(&app, old_target).session.clone();
        replacement.launch.session_id = Some("new-native-thread-before-ack".into());
        drop(app.start_chat(
            old_target.card,
            replacement.clone(),
            chat::Intent::Restore,
            None,
            false,
        ));
        let new_target = app.chat_ui.attempt(old_target.card).unwrap().target;
        let handle = Arc::new(ClientHandle::fixture(
            replacement.clone(),
            Snapshot {
                ready: true,
                session_id: replacement.launch.session_id.clone(),
                ..Snapshot::default()
            },
        ));
        drop(app.handle_chat_ui(Action::Ready(
            new_target,
            Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
        )));
        assert!(!app.chat_start_pending(new_target.card));
        assert_eq!(
            runtime(&app, new_target).session.launch.session_id,
            replacement.launch.session_id
        );
        assert!(
            matches!(&app.workspace_model.views[0].panes[0].kind, CardKind::Chat(old) if old.launch.session_id.as_deref() == Some("closed-draft-thread"))
        );
        let revision = runtime(&app, new_target).revision;
        retire(&mut app, channel, new_target.card);
        assert!(!app.chat_closed_elsewhere(new_target.card));
        assert!(!runtime(&app, new_target).closed_elsewhere);
        assert!(
            actions(app.handle_chat_ui(Action::DiscardClosedDraft(new_target, revision)))
                .is_empty()
        );
        assert!(
            actions(app.handle_chat_ui(Action::DiscardClosedDraft(old_target, revision)))
                .is_empty()
        );
        assert_eq!(
            runtime(&app, new_target).unsent_text(),
            "replacement's draft"
        );
    }
}
