//! Inspect a genuinely issued report source without replacing its preview.
//! One inspector per pane bounds retained text; clipboard reads keep their
//! existing shared intent and source-authority fences in text_copy.
use super::*;

const PAGE_BYTES: usize = 16 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    id: String,
    source: chat::TextCopySource,
    conversation_revision: u64,
}

#[derive(Clone)]
pub(super) struct Request {
    key: Key,
    handle: Arc<ClientHandle>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
pub(super) struct State {
    key: Option<Key>,
    expanded: bool,
    request: Option<Request>,
    ready: Option<Arc<String>>,
    error: Option<String>,
    page: usize,
}

impl State {
    fn cancel(&mut self) {
        if let Some(request) = self.request.take() {
            request.cancelled.store(true, Ordering::Release);
        }
    }

    pub(super) fn hide(&mut self, reloadable: bool) {
        self.cancel();
        if reloadable {
            self.ready = None;
        }
    }

    pub(super) fn collapse(&mut self, id: &str, child: Option<&str>) {
        if self
            .key
            .as_ref()
            .is_some_and(|key| key.id == id && scope_matches(&key.source.scope, child))
        {
            self.expanded = false;
            self.hide(true);
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn scope_matches(scope: &chat::TextCopyScope, child: Option<&str>) -> bool {
    match (scope, child) {
        (chat::TextCopyScope::Conversation, None) => true,
        (chat::TextCopyScope::Subagent { id }, Some(child)) => id == child,
        _ => false,
    }
}

fn compatible(captured: &chat::TextCopySource, current: &chat::TextCopySource) -> bool {
    captured.cache == current.cache
        && captured.scope == current.scope
        && captured.token == current.token
        && captured.revision == current.revision
        && captured.bytes <= current.bytes
}

fn snapshot_keeps(snapshot: &Snapshot, key: &Key) -> bool {
    let sources = match &key.source.scope {
        chat::TextCopyScope::Conversation => {
            if snapshot.text_copy_sources.conversation_revision != key.conversation_revision {
                return false;
            }
            Some(&snapshot.text_copy_sources.conversation)
        }
        chat::TextCopyScope::Subagent { id } => snapshot.text_copy_sources.children.get(id),
    };
    sources.is_some_and(|sources| {
        sources
            .iter()
            .flatten()
            .any(|source| compatible(&key.source, source))
    })
}

fn page_bounds(text: &str, page: usize) -> (std::ops::Range<usize>, usize, usize) {
    let pages = text.len().div_ceil(PAGE_BYTES).max(1);
    let page = page.min(pages - 1);
    let mut start = (page * PAGE_BYTES).min(text.len());
    let mut end = start.saturating_add(PAGE_BYTES).min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (start..end, page, pages)
}

impl Runtime {
    fn report_output_key(&self, id: &str, source: &chat::TextCopySource) -> Option<Key> {
        let item = self.active_item(id)?;
        if !matches!(
            item.content,
            ItemContent::Delivery {
                kind: chat::DeliveryKind::Report,
                ..
            }
        ) {
            return None;
        }
        let current = self.message_copy_source(item)?;
        if !compatible(source, &current)
            || !scope_matches(&source.scope, self.selected_subagent.as_deref())
        {
            return None;
        }
        Some(Key {
            id: id.to_owned(),
            source: source.clone(),
            conversation_revision: self.snapshot.text_copy_sources.conversation_revision,
        })
    }

    fn keeps_report_output(&self, key: &Key) -> bool {
        scope_matches(&key.source.scope, self.selected_subagent.as_deref())
            && snapshot_keeps(&self.snapshot, key)
            && self
                .handle()
                .is_some_and(|handle| snapshot_keeps(&handle.snapshot(), key))
    }

    pub(super) fn report_output_view(
        &self,
        item: &chat::Item,
    ) -> Option<Element<'static, Message>> {
        let source = self.message_copy_source(item)?;
        let key = self.report_output_key(&item.id, &source)?;
        let state = &self.provider_exit;
        let selected = state.key.as_ref() == Some(&key) && self.keeps_report_output(&key);
        let expanded = selected && state.expanded;
        let target = self.target;
        let mut rows = vec![
            crate::ui_controls::button::quiet(if expanded {
                "Hide saved output"
            } else {
                "Show saved output"
            })
            .on_press(action(Action::ToggleReportOutput(
                target,
                item.id.clone(),
                source.clone(),
            )))
            .into(),
        ];
        if !expanded {
            return Some(
                widget::column::with_children(rows)
                    .spacing(step::GAP_S)
                    .into(),
            );
        }
        if let Some(error) = &state.error {
            rows.push(design::caption(format!("Could not load saved output: {error}")).into());
            rows.push(
                widget::row::with_children(vec![
                    crate::ui_controls::button::quiet("Retry loading saved output")
                        .on_press(action(Action::LoadReportOutput(
                            target,
                            item.id.clone(),
                            source.clone(),
                        )))
                        .into(),
                    crate::ui_controls::button::quiet("Copy report preview")
                        .on_press(action(Action::Copy(
                            target,
                            item.id.clone(),
                            CopyPart::Text,
                        )))
                        .into(),
                ])
                .spacing(step::GAP_S)
                .wrap()
                .into(),
            );
        } else if state.request.is_some() {
            rows.push(
                design::caption("Loading saved output… The report preview remains above.").into(),
            );
        }
        if let Some(text) = &state.ready {
            let (range, page, pages) = page_bounds(text, state.page);
            if pages > 1 {
                rows.push(
                    widget::row::with_children(vec![
                        crate::ui_controls::button::quiet("Previous output page")
                            .on_press_maybe(
                                (page > 0).then(|| action(Action::ReportOutputPage(target, false))),
                            )
                            .into(),
                        design::caption(format!("{} / {pages}", page + 1)).into(),
                        crate::ui_controls::button::quiet("Next output page")
                            .on_press_maybe(
                                (page + 1 < pages)
                                    .then(|| action(Action::ReportOutputPage(target, true))),
                            )
                            .into(),
                    ])
                    .spacing(step::GAP_S)
                    .wrap()
                    .into(),
                );
            }
            // Materialize only this bounded visible page, never the full body
            // on each frame. The original report/encoding labels stay intact.
            rows.push(
                widget::scrollable(
                    widget::selectable_text(text[range].to_owned())
                        .size(design::TEXT_CAPTION)
                        .font(iced::Font::MONOSPACE)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .width(Length::Fill),
                )
                .id(widget::Id::new(format!(
                    "saved-report-{:?}-{}-{}-{page}",
                    target, source.cache, source.token
                )))
                .height(240)
                .into(),
            );
        }
        Some(
            widget::column::with_children(rows)
                .spacing(step::GAP_S)
                .into(),
        )
    }
}

impl Cagents {
    pub(super) fn toggle_report_output(
        &mut self,
        target: Target,
        id: String,
        source: chat::TextCopySource,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(key) = runtime.report_output_key(&id, &source) else {
            return Task::none();
        };
        if runtime.provider_exit.key.as_ref() == Some(&key) && runtime.provider_exit.expanded {
            runtime
                .provider_exit
                .collapse(&id, runtime.selected_subagent.as_deref());
            return Task::none();
        }
        let mut state = State::default();
        state.key = Some(key);
        state.expanded = true;
        runtime.provider_exit = state;
        self.load_report_output(target, id, source)
    }

    pub(super) fn reload_report_output(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime
            .provider_exit
            .key
            .as_ref()
            .is_some_and(|key| !runtime.keeps_report_output(key))
        {
            runtime.provider_exit = State::default();
            return Task::none();
        }
        let state = &runtime.provider_exit;
        if runtime.visible.load(Ordering::Relaxed)
            && state.expanded
            && state.ready.is_none()
            && state.request.is_none()
            && state.error.is_none()
            && let Some(key) = state.key.clone()
        {
            return self.load_report_output(target, key.id, key.source);
        }
        Task::none()
    }

    pub(super) fn load_report_output(
        &mut self,
        target: Target,
        id: String,
        source: chat::TextCopySource,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(key) = runtime.report_output_key(&id, &source) else {
            return Task::none();
        };
        if runtime.provider_exit.key.as_ref() != Some(&key)
            || !runtime.provider_exit.expanded
            || !runtime.visible.load(Ordering::Relaxed)
            || runtime.provider_exit.request.is_some()
        {
            return Task::none();
        }
        let Some(handle) = runtime.handle().cloned() else {
            runtime.provider_exit.error =
                Some("Reconnect this conversation to load saved output.".into());
            return Task::none();
        };
        if !handle.is_connected() {
            runtime.provider_exit.error = Some(
                "Reconnect this conversation to load saved output. The last loaded output is kept."
                    .into(),
            );
            return Task::none();
        }
        let request = Request {
            key,
            handle: handle.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        runtime.provider_exit.request = Some(request.clone());
        runtime.provider_exit.error = None;
        cosmic::task::future(async move {
            let read = request.clone();
            let result = tokio::task::spawn_blocking(move || {
                handle.copy_text_cancellable(read.key.source, &read.cancelled)
            })
            .await
            .map_err(|_| "Saved output loading could not finish".to_owned())
            .and_then(|result| result)
            .map(Arc::new);
            action(Action::ReportOutputLoaded(target, request, result))
        })
    }

    pub(super) fn report_output_loaded(
        &mut self,
        target: Target,
        request: Request,
        result: Result<Arc<String>, String>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime
            .provider_exit
            .request
            .as_ref()
            .is_none_or(|current| {
                current.key != request.key || !Arc::ptr_eq(&current.cancelled, &request.cancelled)
            })
        {
            return Task::none();
        }
        runtime.provider_exit.request = None;
        if request.cancelled.load(Ordering::Acquire)
            || !runtime.keeps_report_output(&request.key)
            || runtime
                .handle()
                .is_none_or(|handle| !Arc::ptr_eq(handle, &request.handle))
        {
            return Task::none();
        }
        match result {
            Ok(text)
                if runtime.visible.load(Ordering::Relaxed) && runtime.provider_exit.expanded =>
            {
                runtime.provider_exit.ready = Some(text);
                runtime.provider_exit.error = None;
            }
            Ok(_) => {}
            Err(error) => runtime.provider_exit.error = Some(error),
        }
        Task::none()
    }

    pub(super) fn report_output_page(&mut self, target: Target, forward: bool) {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return;
        };
        if runtime
            .provider_exit
            .key
            .as_ref()
            .is_none_or(|key| !runtime.keeps_report_output(key))
        {
            return;
        }
        let state = &mut runtime.provider_exit;
        if !state.expanded {
            return;
        }
        let Some(text) = &state.ready else {
            return;
        };
        let last = text.len().div_ceil(PAGE_BYTES).max(1) - 1;
        state.page = if forward {
            state.page.saturating_add(1).min(last)
        } else {
            state.page.min(last).saturating_sub(1)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAVED: &str = "Saved first line\nSaved final line";

    fn fixture() -> (Cagents, Target, Arc<ClientHandle>, chat::TextCopySource) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.new_native_chat(
            Provider::Codex,
            Some(PathBuf::from("/workspace/report-fixture")),
        ));
        let card = app.workspace.focused().unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("chat");
        };
        let target = runtime.target;
        runtime.session.launch.session_id = Some("saved-report-conversation".into());
        let source = chat::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: chat::TextCopyScope::Conversation,
            token: 17,
            revision: 3,
            bytes: SAVED.len() as u64,
        };
        let mut snapshot = (*runtime.snapshot).clone();
        snapshot.generation = 7;
        snapshot.ready = true;
        snapshot.exited = true;
        snapshot.session_id = runtime.session.launch.session_id.clone();
        snapshot.error = Some("Original provider failure with its bounded tail".into());
        snapshot.items = vec![Arc::new(chat::Item {
            id: "owned-report".into(),
            role: chat::Role::System,
            at: None,
            started_at: None,
            content: ItemContent::Delivery {
                kind: chat::DeliveryKind::Report,
                from: "Provider stderr".into(),
                sender: String::new(),
                summary: "Captured provider diagnostics".into(),
                body: "Preview and encoding/omission labels stay readable".into(),
            },
        })];
        snapshot.text_copy_sources.conversation_revision = 4;
        snapshot.text_copy_sources.conversation = vec![Some(source.clone())];
        let handle = Arc::new(ClientHandle::fixture(runtime.session.clone(), snapshot));
        runtime.connection = Connection::Live(handle.clone());
        runtime.snapshot = handle.snapshot();
        runtime.expanded.insert("owned-report".into());
        (app, target, handle, source)
    }

    fn begin(app: &mut Cagents, target: Target, source: &chat::TextCopySource) -> Request {
        let _unpolled = app.handle_chat_ui(Action::ToggleReportOutput(
            target,
            "owned-report".into(),
            source.clone(),
        ));
        app.chat_runtime_mut(target)
            .unwrap()
            .provider_exit
            .request
            .as_ref()
            .unwrap()
            .clone()
    }

    #[test]
    fn report_inspection_requires_published_authority_and_failed_copy_keeps_loaded_output() {
        let _context = text_copy::isolated_context();
        let (mut app, target, handle, source) = fixture();
        let request = begin(&mut app, target, &source);
        let loaded = Arc::new(SAVED.to_owned());
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            request,
            Ok(loaded.clone()),
        )));
        let copy = text_copy::Request::for_test(handle.clone(), source.clone());
        drop(app.handle_chat_ui(Action::MessageCopied(
            target,
            "owned-report".into(),
            copy,
            Err("fixture copy failure".into()),
        )));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(Arc::ptr_eq(
            runtime.provider_exit.ready.as_ref().unwrap(),
            &loaded
        ));
        assert_eq!(runtime.error.as_deref(), Some("fixture copy failure"));
        assert_eq!(
            runtime.snapshot.error.as_deref(),
            Some("Original provider failure with its bounded tail")
        );
        assert!(
            matches!(&runtime.snapshot.items[0].content, ItemContent::Delivery { body, .. } if body == "Preview and encoding/omission labels stay readable")
        );
        assert!(runtime.provider_exit.request.is_none());
        assert!(runtime.provider_exit.error.is_none());

        let mut other = source.clone();
        other.token += 1;
        drop(app.handle_chat_ui(Action::ToggleReportOutput(
            target,
            "owned-report".into(),
            other,
        )));
        assert!(Arc::ptr_eq(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .ready
                .as_ref()
                .unwrap(),
            &loaded
        ));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let mut unpublished = (*handle.snapshot()).clone();
        unpublished.text_copy_sources.conversation = vec![None];
        handle.publish_snapshot_for_test(unpublished);
        assert!(!runtime.keeps_report_output(runtime.provider_exit.key.as_ref().unwrap()));
        assert!(
            runtime.report_output_key("owned-report", &source).is_some(),
            "UI may still hold the older publication"
        );
        drop(app.reload_report_output(target));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .key
                .is_none()
        );
    }

    #[test]
    fn report_load_failure_retry_and_reconnect_keep_or_retire_the_exact_inspector() {
        let (mut app, target, _handle, source) = fixture();
        let old = begin(&mut app, target, &source);
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            old.clone(),
            Err("fixture read failure".into()),
        )));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(runtime.provider_exit.request.is_none());
        assert_eq!(
            runtime.provider_exit.error.as_deref(),
            Some("fixture read failure")
        );
        let _retry = app.handle_chat_ui(Action::LoadReportOutput(
            target,
            "owned-report".into(),
            source.clone(),
        ));
        let fresh = app
            .chat_runtime_mut(target)
            .unwrap()
            .provider_exit
            .request
            .as_ref()
            .unwrap()
            .clone();
        let stale = Arc::new("obsolete output".into());
        let dropped = Arc::downgrade(&stale);
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(target, old, Ok(stale))));
        assert!(dropped.upgrade().is_none());
        assert!(Arc::ptr_eq(
            &app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .request
                .as_ref()
                .unwrap()
                .cancelled,
            &fresh.cancelled
        ));
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            fresh,
            Ok(Arc::new(SAVED.into())),
        )));
        let kept = app
            .chat_runtime_mut(target)
            .unwrap()
            .provider_exit
            .ready
            .as_ref()
            .unwrap()
            .clone();
        drop(app.handle_chat_ui(Action::Reconnect(target)));
        assert!(Arc::ptr_eq(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .ready
                .as_ref()
                .unwrap(),
            &kept
        ));
        let refused =
            super::super::refused_switch_message(&app, target.card, "fixture reconnect failure");
        drop(app.update(refused));
        assert!(Arc::ptr_eq(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .ready
                .as_ref()
                .unwrap(),
            &kept
        ));
        drop(app.handle_chat_ui(Action::Reconnect(target)));
        let snapshot = (*app.chat_runtime_mut(target).unwrap().snapshot).clone();
        let next = super::super::pending_switch_target(&app, target.card);
        let ready = super::super::ready_message_for_pending_switch(&app, target.card, snapshot);
        drop(app.update(ready));
        assert_ne!(next, target);
        assert!(app.chat_runtime_mut(target).is_none());
        let runtime = app.chat_runtime_mut(next).unwrap();
        assert!(runtime.provider_exit.key.is_none());
        assert!(runtime.provider_exit.request.is_none());
        assert!(runtime.provider_exit.ready.is_none());
        let _fresh = app.handle_chat_ui(Action::ToggleReportOutput(
            next,
            "owned-report".into(),
            source,
        ));
        assert!(
            app.chat_runtime_mut(next)
                .unwrap()
                .provider_exit
                .request
                .is_some()
        );
    }

    #[test]
    fn report_disclosure_hide_cancels_read_and_releases_only_reloadable_output() {
        for held in [false, true] {
            let (mut app, target, handle, source) = fixture();
            let original = (*handle.snapshot()).clone();
            if held {
                app.chat_runtime_mut(target)
                    .unwrap()
                    .followups
                    .push(Followup {
                        id: 77,
                        text: "Keep this later draft".into(),
                        preview: "Keep this later draft".into(),
                        attachments: vec![],
                    });
            }
            let request = begin(&mut app, target, &source);
            app.chat_runtime_mut(target)
                .unwrap()
                .set_visible(false)
                .unwrap();
            assert!(request.cancelled.load(Ordering::Acquire));
            assert!(
                app.chat_runtime_mut(target)
                    .unwrap()
                    .provider_exit
                    .request
                    .is_none()
            );
            assert!(handle.snapshot().items.is_empty());
            let returned = Arc::new(SAVED.into());
            let dropped = Arc::downgrade(&returned);
            drop(app.handle_chat_ui(Action::ReportOutputLoaded(target, request, Ok(returned))));
            assert!(dropped.upgrade().is_none());
            app.chat_runtime_mut(target)
                .unwrap()
                .set_visible(true)
                .unwrap();
            drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
            assert!(
                app.chat_runtime_mut(target)
                    .unwrap()
                    .provider_exit
                    .request
                    .is_none(),
                "show waits for a renderable publication"
            );
            // The existing body-policy wake hydrates even with the same
            // generation, with or without a follow-up keeping interest held.
            handle.publish_snapshot_for_test(original);
            drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
            let runtime = app.chat_runtime_mut(target).unwrap();
            let fresh = runtime.provider_exit.request.as_ref().unwrap().clone();
            assert_eq!(runtime.snapshot.generation, 7);
            assert_eq!(runtime.followups.len(), usize::from(held));
            let text = Arc::new(SAVED.into());
            drop(app.handle_chat_ui(Action::ReportOutputLoaded(target, fresh, Ok(text.clone()))));
            handle.disconnect_for_test();
            app.chat_runtime_mut(target)
                .unwrap()
                .set_visible(false)
                .unwrap();
            app.chat_runtime_mut(target)
                .unwrap()
                .set_visible(true)
                .unwrap();
            assert!(
                Arc::ptr_eq(
                    app.chat_runtime_mut(target)
                        .unwrap()
                        .provider_exit
                        .ready
                        .as_ref()
                        .unwrap(),
                    &text
                ),
                "unreachable source keeps its last loaded output across hide/show"
            );
            drop(app.handle_chat_ui(Action::Toggle(target, "owned-report".into())));
            assert!(
                app.chat_runtime_mut(target)
                    .unwrap()
                    .provider_exit
                    .ready
                    .is_none(),
                "explicit report collapse releases its inspector"
            );
        }
    }

    #[test]
    fn switching_report_scope_cancels_only_on_actual_selection_change() {
        let (mut app, target, handle, source) = fixture();
        let mut snapshot = (*handle.snapshot()).clone();
        let mut child_source = source.clone();
        child_source.scope = chat::TextCopyScope::Subagent { id: "child".into() };
        snapshot.subagents.push(chat::Subagent {
            id: "child".into(),
            kind: "review".into(),
            description: String::new(),
            model: None,
            status: chat::ToolStatus::Succeeded,
            background: false,
            waiting: false,
            detail: String::new(),
            items: snapshot.items.clone(),
        });
        snapshot
            .text_copy_sources
            .children
            .insert("child".into(), vec![Some(child_source.clone())]);
        handle.publish_snapshot_for_test(snapshot);
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
        let parent = begin(&mut app, target, &source);
        drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
        assert!(parent.cancelled.load(Ordering::Acquire));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .key
                .is_none()
        );
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            parent,
            Ok(Arc::new("obsolete parent output".into())),
        )));
        drop(app.handle_chat_ui(Action::ToggleReportOutput(
            target,
            "owned-report".into(),
            source,
        )));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .request
                .is_none()
        );
        let child = begin(&mut app, target, &child_source);
        drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
        assert!(!child.cancelled.load(Ordering::Acquire));
        drop(app.handle_chat_ui(Action::SelectSubagent(target, None)));
        assert!(child.cancelled.load(Ordering::Acquire));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .key
                .is_none()
        );
    }

    #[test]
    fn saved_report_pages_are_bounded_and_lossless_at_utf8_boundaries() {
        let text = "€".repeat(PAGE_BYTES);
        let mut restored = String::new();
        for page in 0..text.len().div_ceil(PAGE_BYTES) {
            let (range, actual, _) = page_bounds(&text, page);
            assert_eq!(actual, page);
            assert!(range.len() <= PAGE_BYTES + 3);
            restored.push_str(&text[range]);
        }
        assert_eq!(restored, text);
    }
}
