//! Inspect a genuinely issued report source without replacing its preview.
//! One inspector per pane bounds retained text; clipboard reads keep their
//! existing shared intent and source-authority fences in text_copy.
use super::*;
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, renderer};
use iced::{Event, Rectangle, Size};
use std::{cell::RefCell, rc::Rc};

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

/// Header Copy is available before the inspector is opened. Its separate
/// ticket joins the inspector's hide/collapse/drop revocation without opening
/// or replacing the currently inspected report.
#[derive(Clone)]
pub(super) struct CopyTicket(Arc<AtomicBool>);

impl CopyTicket {
    pub(super) fn is_current(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn revoke(&self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Default)]
pub(super) struct State {
    key: Option<Key>,
    expanded: bool,
    request: Option<Request>,
    ready: Option<Arc<String>>,
    error: Option<String>,
    page: usize,
    selection: Rc<RefCell<PageSelection>>,
    // Owned by this inspector, never by a callback. Revoked tickets cannot
    // become live again when the same runtime is shown later.
    visibility: Arc<AtomicBool>,
    full_copy: Option<(Key, CopyTicket)>,
}

#[derive(Default)]
struct PageSelection {
    selected: Option<Arc<str>>,
    select_all: bool,
}

// Use the same selectable core as COSMIC's text widget, but keep its menu
// actions in the app's shared clipboard-intent path. COSMIC's automatic native
// text popup writes directly from another surface on Wayland.
struct PageText {
    text: widget::text::Text<'static, cosmic::Theme, cosmic::Renderer>,
    selection: Rc<RefCell<PageSelection>>,
    key: Key,
    handle: Arc<ClientHandle>,
    visible: Arc<AtomicBool>,
}

struct SelectionClipboard<'a> {
    clipboard: &'a mut dyn Clipboard,
    key: &'a Key,
    handle: &'a ClientHandle,
    visible: &'a AtomicBool,
}

impl Clipboard for SelectionClipboard<'_> {
    fn read(&self, kind: iced::advanced::clipboard::Kind) -> Option<String> {
        self.clipboard.read(kind)
    }

    fn write(&mut self, kind: iced::advanced::clipboard::Kind, contents: String) {
        if self.visible.load(Ordering::Relaxed) && snapshot_keeps(&self.handle.snapshot(), self.key)
        {
            text_copy::invalidate();
            self.clipboard.write(kind, contents);
        }
    }
}

impl PageText {
    fn widget(&self) -> &dyn Widget<Message, cosmic::Theme, cosmic::Renderer> {
        &self.text
    }

    fn widget_mut(&mut self) -> &mut dyn Widget<Message, cosmic::Theme, cosmic::Renderer> {
        &mut self.text
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for PageText {
    fn tag(&self) -> tree::Tag {
        self.widget().tag()
    }
    fn state(&self) -> tree::State {
        self.widget().state()
    }
    fn children(&self) -> Vec<Tree> {
        self.widget().children()
    }
    fn diff(&mut self, tree: &mut Tree) {
        self.widget_mut().diff(tree);
    }
    fn id(&self) -> Option<widget::Id> {
        self.widget().id()
    }
    fn set_id(&mut self, id: widget::Id) {
        self.widget_mut().set_id(id);
    }
    fn size(&self) -> Size<Length> {
        self.widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.widget_mut().layout(tree, renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.widget_mut().operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        use widget::HasSelectableText;
        if std::mem::take(&mut self.selection.borrow_mut().select_all) {
            self.text.select_all(tree);
            shell.request_redraw();
        }
        Widget::<Message, cosmic::Theme, cosmic::Renderer>::update(
            &mut self.text,
            tree,
            event,
            layout,
            cursor,
            renderer,
            &mut SelectionClipboard {
                clipboard,
                key: &self.key,
                handle: &self.handle,
                visible: &self.visible,
            },
            shell,
            viewport,
        );
        self.text.set_context_menu_position(tree, None);
        self.selection.borrow_mut().selected = self.text.selected_text(tree).map(Arc::from);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
        // Match COSMIC SelectableText's native keyboard-focus treatment even
        // when there is no selected range to provide another visible cue.
        use iced::advanced::Renderer as _;
        type TextState = iced::advanced::widget::text::State<
            <cosmic::Renderer as iced::advanced::text::Renderer>::Paragraph,
        >;
        if tree.state.downcast_ref::<TextState>().is_keyboard_focused() {
            let cosmic = theme.cosmic();
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: iced::Border {
                        color: cosmic.accent.base.into(),
                        width: 1.0,
                        radius: cosmic.corner_radii.radius_s.into(),
                    },
                    ..renderer::Quad::default()
                },
                iced::Color::TRANSPARENT,
            );
        }
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.widget().a11y_nodes(layout, tree, cursor)
    }
}

fn page_menu(
    target: Target,
    key: Key,
    page: usize,
    selection: &PageSelection,
) -> Vec<widget::menu::Tree<Message>> {
    let copy: Element<'static, Message> =
        widget::menu::menu_button(vec![widget::text("Copy").into()])
            .name("Copy selected saved output")
            .on_press_maybe(selection.selected.as_ref().map(|text| {
                action(Action::CopyReportSelection(
                    target,
                    key.clone(),
                    page,
                    text.clone(),
                ))
            }))
            .into();
    let select: Element<'static, Message> =
        widget::menu::menu_button(vec![widget::text("Select all").into()])
            .name("Select all on this saved output page")
            .on_press(action(Action::SelectReportPage(target, key, page)))
            .into();
    vec![copy.into(), select.into()]
}

impl State {
    fn revoke_full_copy(&mut self) {
        if let Some((_, ticket)) = self.full_copy.take() {
            ticket.revoke();
        }
    }

    fn admit(&mut self) {
        if !self.visibility.load(Ordering::Relaxed) {
            self.visibility = Arc::new(AtomicBool::new(true));
        }
    }

    fn cancel(&mut self) {
        if let Some(request) = self.request.take() {
            request.cancelled.store(true, Ordering::Release);
        }
    }

    pub(super) fn hide(&mut self, reloadable: bool) {
        self.revoke_full_copy();
        self.hide_inspector(reloadable);
    }

    fn hide_inspector(&mut self, reloadable: bool) {
        self.visibility.store(false, Ordering::Relaxed);
        self.cancel();
        if reloadable {
            self.ready = None;
            self.selection = Rc::default();
        }
    }

    pub(super) fn collapse(&mut self, id: &str, child: Option<&str>) {
        // A header copy need not have opened this inspector at all.
        if self
            .full_copy
            .as_ref()
            .is_some_and(|(key, _)| key.id == id && scope_matches(&key.source.scope, child))
        {
            self.revoke_full_copy();
        }
        if self
            .key
            .as_ref()
            .is_some_and(|key| key.id == id && scope_matches(&key.source.scope, child))
        {
            self.expanded = false;
            self.hide_inspector(true);
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.revoke_full_copy();
        self.visibility.store(false, Ordering::Relaxed);
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
    pub(super) fn requires_report_copy_ticket(
        &self,
        id: &str,
        source: &chat::TextCopySource,
    ) -> bool {
        let report = |item: &chat::Item| {
            matches!(
                item.content,
                ItemContent::Delivery {
                    kind: chat::DeliveryKind::Report,
                    ..
                }
            )
        };
        if self.active_item(id).is_some_and(|item| report(item)) {
            return true;
        }
        let items = match self.selected_subagent.as_deref() {
            Some(child) => self
                .snapshot
                .subagents
                .iter()
                .find(|agent| agent.id == child)
                .map(|agent| agent.items.as_slice()),
            None => Some(self.snapshot.items.as_slice()),
        };
        items.is_some_and(|items| {
            items.iter().any(|item| {
                report(item)
                    && self
                        .message_copy_source(item)
                        .is_some_and(|current| compatible(source, &current))
            })
        })
    }

    pub(super) fn report_copy_ticket(
        &mut self,
        id: &str,
        source: &chat::TextCopySource,
    ) -> Result<Option<CopyTicket>, &'static str> {
        if !self.requires_report_copy_ticket(id, source) {
            return Ok(None);
        }
        let key = self
            .report_output_key(id, source)
            .filter(|key| self.visible.load(Ordering::Relaxed) && self.keeps_report_output(key))
            .ok_or("This report is no longer visible with its saved output source. Copy its current report again.")?;
        self.provider_exit.revoke_full_copy();
        let ticket = CopyTicket(Arc::new(AtomicBool::new(true)));
        self.provider_exit.full_copy = Some((key, ticket.clone()));
        Ok(Some(ticket))
    }

    // Keep presentation ownership local to the inspector. The Runtime's own
    // visibility is still the authority; callbacks only retain a revocable
    // admission granted after both current source publications agree.
    pub(super) fn admit_report_output(&mut self) {
        if self.visible.load(Ordering::Relaxed)
            && self.provider_exit.expanded
            && self
                .provider_exit
                .key
                .as_ref()
                .is_some_and(|key| self.keeps_report_output(key))
        {
            self.provider_exit.admit();
        }
    }

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
            conversation_revision: match &source.scope {
                chat::TextCopyScope::Conversation => {
                    self.snapshot.text_copy_sources.conversation_revision
                }
                chat::TextCopyScope::Subagent { .. } => 0,
            },
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
            let selection = state.selection.clone();
            let menu_key = key.clone();
            let page_text = PageText {
                text: widget::text(text[range].to_owned())
                    .selectable()
                    .size(design::TEXT_CAPTION)
                    .font(iced::Font::MONOSPACE)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .width(Length::Fill),
                selection: selection.clone(),
                key: key.clone(),
                handle: self.handle()?.clone(),
                visible: state.visibility.clone(),
            };
            rows.push(
                widget::scrollable(crate::ui_controls::owned(
                    (key.clone(), page),
                    crate::interaction::context_menu_lazy(Element::new(page_text), move || {
                        page_menu(target, menu_key.clone(), page, &selection.borrow())
                    }),
                ))
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
        // Showing this same source is not a revocation. Transfer the original
        // live permit before replacing State, including a captured prefix of
        // an appended publication. Never allocate or revive a ticket here.
        if runtime.visible.load(Ordering::Relaxed)
            && runtime.keeps_report_output(&key)
            && runtime
                .provider_exit
                .full_copy
                .as_ref()
                .is_some_and(|(captured, ticket)| {
                    ticket.is_current()
                        && captured.id == key.id
                        && captured.conversation_revision == key.conversation_revision
                        && compatible(&captured.source, &key.source)
                        && runtime.keeps_report_output(captured)
                })
        {
            state.full_copy = runtime.provider_exit.full_copy.take();
        }
        state.key = Some(key);
        state.expanded = true;
        runtime.provider_exit = state;
        runtime.admit_report_output();
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
                runtime.provider_exit.selection = Rc::default();
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
        let page = if forward {
            state.page.saturating_add(1).min(last)
        } else {
            state.page.min(last).saturating_sub(1)
        };
        if state.page != page {
            state.page = page;
            state.selection = Rc::default();
        }
    }

    pub(super) fn report_page_selection(
        &mut self,
        target: Target,
        key: Key,
        page: usize,
        copied: Option<Arc<str>>,
    ) -> Task<Message> {
        if self.workspace.focused() != Some(target.card) {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let state = &runtime.provider_exit;
        if !runtime.visible.load(Ordering::Relaxed)
            || !state.expanded
            || state.key.as_ref() != Some(&key)
            || !runtime.keeps_report_output(&key)
            || state.page != page
            || state.ready.is_none()
        {
            return Task::none();
        }
        if let Some(text) = copied {
            if state.selection.borrow().selected.as_ref() != Some(&text) {
                return Task::none();
            }
            let handle = runtime.handle().unwrap().clone();
            let visible = runtime.provider_exit.visibility.clone();
            let mut write = Some(text_copy::write_shared_literal(text));
            return iced::Task::done(()).then(move |_| {
                let effect = write.take();
                if visible.load(Ordering::Relaxed) && snapshot_keeps(&handle.snapshot(), &key) {
                    effect.unwrap_or_else(Task::none)
                } else {
                    Task::none()
                }
            });
        }
        state.selection.borrow_mut().select_all = true;
        Task::none()
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
        // Admit the pane through the same viewport/update path as the native
        // workspace, before installing any report body. A zero-width fixture
        // is genuinely hidden and must not load output.
        drop(app.update(Message::Viewport {
            generation: app.workspace_view_generation,
            event: WorkspaceViewportEvent::Resized(1_600.0),
        }));
        assert_eq!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("chat");
        };
        assert!(runtime.visible.load(Ordering::Relaxed));
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
        let copy = full_copy_request(&mut app, target, &source);
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

    #[test]
    fn child_report_inspection_survives_parent_revision_but_parent_inspection_does_not() {
        let (mut app, target, handle, source) = fixture();
        let mut child_source = source.clone();
        child_source.scope = chat::TextCopyScope::Subagent { id: "child".into() };
        let mut snapshot = (*handle.snapshot()).clone();
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
        handle.publish_snapshot_for_test(snapshot.clone());
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
        drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("child".into()))));
        let child = begin(&mut app, target, &child_source);
        snapshot.generation += 1;
        snapshot.items.clear();
        snapshot.text_copy_sources.conversation.clear();
        snapshot.text_copy_sources.conversation_revision += 1;
        handle.publish_snapshot_for_test(snapshot);
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(
            runtime.provider_exit.key == runtime.report_output_key("owned-report", &child_source)
        );
        assert!(runtime.provider_exit.expanded);
        assert!(!child.cancelled.load(Ordering::Acquire));
        let loaded = Arc::new(SAVED.into());
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            child,
            Ok(loaded.clone()),
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

        let (mut app, target, handle, source) = fixture();
        let parent = begin(&mut app, target, &source);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.text_copy_sources.conversation_revision += 1;
        handle.publish_snapshot_for_test(snapshot);
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
        assert!(parent.cancelled.load(Ordering::Acquire));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .key
                .is_none()
        );
    }

    fn clipboard_writes(task: Task<Message>) -> Vec<String> {
        use iced::futures::StreamExt;
        iced::futures::executor::block_on(async move {
            let Some(stream) = iced::runtime::task::into_stream(task) else {
                return vec![];
            };
            stream
                .filter_map(|effect| async move {
                    match effect {
                        iced::runtime::Action::Clipboard(
                            iced::runtime::clipboard::Action::Write { contents, .. },
                        ) => Some(contents),
                        _ => None,
                    }
                })
                .collect()
                .await
        })
    }

    fn full_copy_request(
        app: &mut Cagents,
        target: Target,
        source: &chat::TextCopySource,
    ) -> text_copy::Request {
        app.message_copy_request(target, "owned-report", Some(source.clone()))
            .expect("the production header-copy admission grants visible report authority")
    }

    fn full_copy_writes(task: Task<Message>) -> Vec<String> {
        use iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async move {
                tokio::time::timeout(Duration::from_secs(3), async move {
                    let Some(stream) = iced::runtime::task::into_stream(task) else {
                        return vec![];
                    };
                    stream
                        .filter_map(|effect| async move {
                            match effect {
                                iced::runtime::Action::Clipboard(
                                    iced::runtime::clipboard::Action::Write {
                                        target: iced::advanced::clipboard::Kind::Standard,
                                        contents,
                                        ..
                                    },
                                ) => Some(contents),
                                _ => None,
                            }
                        })
                        .collect()
                        .await
                })
                .await
                .expect("the actual deferred clipboard effect and acknowledgement timer settle")
            })
    }

    #[test]
    fn full_report_copy_revokes_at_completion_and_deferred_effect_after_hide_or_collapse() {
        let _context = text_copy::isolated_context();
        for hide_saved_output in [false, true] {
            for completed_before_revoke in [false, true] {
                let (mut app, target, handle, source) = fixture();
                if hide_saved_output {
                    let read = begin(&mut app, target, &source);
                    drop(app.handle_chat_ui(Action::ReportOutputLoaded(
                        target,
                        read,
                        Ok(Arc::new(SAVED.into())),
                    )));
                } else {
                    assert!(
                        app.chat_runtime_mut(target)
                            .unwrap()
                            .provider_exit
                            .key
                            .is_none()
                    );
                }
                let request = full_copy_request(&mut app, target, &source);
                let ticket = app
                    .chat_runtime_mut(target)
                    .unwrap()
                    .provider_exit
                    .full_copy
                    .as_ref()
                    .unwrap()
                    .1
                    .clone();
                let snapshot = handle.snapshot();
                let pending = completed_before_revoke.then(|| {
                    app.handle_chat_ui(Action::MessageCopied(
                        target,
                        "owned-report".into(),
                        request.clone(),
                        Ok(SAVED.into()),
                    ))
                });
                drop(app.handle_chat_ui(if hide_saved_output {
                    Action::ToggleReportOutput(target, "owned-report".into(), source.clone())
                } else {
                    Action::Toggle(target, "owned-report".into())
                }));
                assert!(!ticket.is_current());
                assert!(
                    Arc::ptr_eq(&handle.snapshot(), &snapshot),
                    "only presentation authority changed"
                );
                assert_eq!(app.workspace.focused(), Some(target.card));
                assert!(Arc::ptr_eq(
                    app.chat_runtime_mut(target).unwrap().handle().unwrap(),
                    &handle
                ));
                let task = pending.unwrap_or_else(|| {
                    app.handle_chat_ui(Action::MessageCopied(
                        target,
                        "owned-report".into(),
                        request.clone(),
                        Ok(SAVED.into()),
                    ))
                });
                assert!(
                    full_copy_writes(task).is_empty(),
                    "a revoked full Copy cannot emit Standard clipboard bytes"
                );
                if !completed_before_revoke {
                    assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
                }
                app.chat_runtime_mut(target).unwrap().error = Some("current error".into());
                let obsolete_error = app.handle_chat_ui(Action::MessageCopied(
                    target,
                    "owned-report".into(),
                    request,
                    Err("obsolete read error".into()),
                ));
                assert!(full_copy_writes(obsolete_error).is_empty());
                assert_eq!(
                    app.chat_runtime_mut(target).unwrap().error.as_deref(),
                    Some("current error")
                );

                // Header Copy remains available after Hide/outer collapse,
                // without reopening or loading the saved-output inspector.
                let fresh = full_copy_request(&mut app, target, &source);
                assert!(
                    !ticket.is_current(),
                    "fresh admission never revives an old ticket"
                );
                let copied = app.handle_chat_ui(Action::MessageCopied(
                    target,
                    "owned-report".into(),
                    fresh,
                    Ok(SAVED.into()),
                ));
                assert_eq!(full_copy_writes(copied), [SAVED]);
            }
        }
    }

    #[test]
    fn full_report_copy_without_opening_inspector_requires_a_real_report_permit() {
        let _context = text_copy::isolated_context();
        let (mut app, target, handle, source) = fixture();
        let request = full_copy_request(&mut app, target, &source);
        let state = &app.chat_runtime_mut(target).unwrap().provider_exit;
        assert!(state.key.is_none() && !state.expanded && state.ready.is_none());
        let copied = app.handle_chat_ui(Action::MessageCopied(
            target,
            "owned-report".into(),
            request,
            Ok(SAVED.into()),
        ));
        assert_eq!(full_copy_writes(copied), [SAVED]);

        app.chat_runtime_mut(target).unwrap().copied = None;
        let unguarded = text_copy::Request::for_test(handle, source.clone());
        let missing = app.handle_chat_ui(Action::MessageCopied(
            target,
            "owned-report".into(),
            unguarded,
            Ok(SAVED.into()),
        ));
        assert!(full_copy_writes(missing).is_empty());
        assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
        assert!(
            app.message_copy_request(target, "missing-report", Some(source.clone()))
                .is_none(),
            "a report descriptor paired with an absent report must not fall through generic Copy"
        );
        assert!(
            app.message_copy_request(target, "owned-report", None)
                .is_none()
        );
    }

    #[test]
    fn showing_same_report_preserves_admitted_copy_and_deferred_effect_with_compatible_append() {
        let _context = text_copy::isolated_context();
        for completed_before_show in [false, true] {
            for appended in [false, true] {
                let (mut app, target, handle, source) = fixture();
                let request = full_copy_request(&mut app, target, &source);
                let (captured, ticket) = app
                    .chat_runtime_mut(target)
                    .unwrap()
                    .provider_exit
                    .full_copy
                    .as_ref()
                    .unwrap()
                    .clone();
                assert!(
                    app.chat_runtime_mut(target)
                        .unwrap()
                        .provider_exit
                        .key
                        .is_none()
                );
                let pending = completed_before_show.then(|| {
                    app.handle_chat_ui(Action::MessageCopied(
                        target,
                        "owned-report".into(),
                        request.clone(),
                        Ok(SAVED.into()),
                    ))
                });
                let mut opening = source.clone();
                if appended {
                    opening.bytes = format!("{SAVED}\nAppended later 🦀").len() as u64;
                    let mut snapshot = (*handle.snapshot()).clone();
                    snapshot.generation += 1;
                    snapshot.text_copy_sources.conversation[0] = Some(opening.clone());
                    handle.publish_snapshot_for_test(snapshot);
                    drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
                }
                let before = handle.snapshot();
                drop(app.handle_chat_ui(Action::ToggleReportOutput(
                    target,
                    "owned-report".into(),
                    opening.clone(),
                )));
                let state = &app.chat_runtime_mut(target).unwrap().provider_exit;
                assert!(state.expanded);
                assert_eq!(state.key.as_ref().unwrap().source, opening);
                let (retained, current) = state
                    .full_copy
                    .as_ref()
                    .expect("Show retains the valid same-report permit");
                assert_eq!(retained.source, captured.source);
                assert_eq!(retained.source.bytes, SAVED.len() as u64);
                assert!(
                    Arc::ptr_eq(&ticket.0, &current.0),
                    "transfer the original ticket, never mint a replacement"
                );
                assert!(ticket.is_current());
                assert!(Arc::ptr_eq(&handle.snapshot(), &before));
                assert_eq!(app.workspace.focused(), Some(target.card));
                assert!(Arc::ptr_eq(
                    app.chat_runtime_mut(target).unwrap().handle().unwrap(),
                    &handle
                ));
                let copied = pending.unwrap_or_else(|| {
                    app.handle_chat_ui(Action::MessageCopied(
                        target,
                        "owned-report".into(),
                        request,
                        Ok(SAVED.into()),
                    ))
                });
                assert_eq!(
                    full_copy_writes(copied),
                    [SAVED],
                    "Show keeps exactly the originally captured bytes, including after an append"
                );
            }
        }
    }

    #[test]
    fn showing_report_never_revives_copy_revoked_by_hide_collapse_or_different_report() {
        let _context = text_copy::isolated_context();
        for revoke in ["hide", "collapse", "different-report"] {
            for completed_before_revoke in [false, true] {
                let (mut app, target, handle, source) = fixture();
                let mut snapshot = (*handle.snapshot()).clone();
                let mut other = (*snapshot.items[0]).clone();
                other.id = "other-report".into();
                snapshot.items.push(Arc::new(other));
                let mut other_source = source.clone();
                other_source.token += 1;
                snapshot
                    .text_copy_sources
                    .conversation
                    .push(Some(other_source.clone()));
                handle.publish_snapshot_for_test(snapshot);
                drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
                if revoke == "hide" {
                    let _unpolled = begin(&mut app, target, &source);
                }
                let request = full_copy_request(&mut app, target, &source);
                let ticket = app
                    .chat_runtime_mut(target)
                    .unwrap()
                    .provider_exit
                    .full_copy
                    .as_ref()
                    .unwrap()
                    .1
                    .clone();
                let before = handle.snapshot();
                let pending = completed_before_revoke.then(|| {
                    app.handle_chat_ui(Action::MessageCopied(
                        target,
                        "owned-report".into(),
                        request.clone(),
                        Ok(SAVED.into()),
                    ))
                });
                drop(app.handle_chat_ui(match revoke {
                    "hide" => {
                        Action::ToggleReportOutput(target, "owned-report".into(), source.clone())
                    }
                    "collapse" => Action::Toggle(target, "owned-report".into()),
                    _ => Action::ToggleReportOutput(target, "other-report".into(), other_source),
                }));
                assert!(!ticket.is_current());
                if revoke == "collapse" {
                    drop(app.handle_chat_ui(Action::Toggle(target, "owned-report".into())));
                }
                drop(app.handle_chat_ui(Action::ToggleReportOutput(
                    target,
                    "owned-report".into(),
                    source.clone(),
                )));
                let state = &app.chat_runtime_mut(target).unwrap().provider_exit;
                assert!(state.expanded);
                assert_eq!(state.key.as_ref().unwrap().id, "owned-report");
                assert!(
                    state.full_copy.is_none(),
                    "Show alone never restores revoked Copy authority"
                );
                assert!(!ticket.is_current());
                assert!(Arc::ptr_eq(&handle.snapshot(), &before));
                assert_eq!(app.workspace.focused(), Some(target.card));
                let obsolete = pending.unwrap_or_else(|| {
                    app.handle_chat_ui(Action::MessageCopied(
                        target,
                        "owned-report".into(),
                        request,
                        Ok(SAVED.into()),
                    ))
                });
                assert!(full_copy_writes(obsolete).is_empty());
                let fresh = full_copy_request(&mut app, target, &source);
                let fresh_ticket = &app
                    .chat_runtime_mut(target)
                    .unwrap()
                    .provider_exit
                    .full_copy
                    .as_ref()
                    .unwrap()
                    .1;
                assert!(!Arc::ptr_eq(&ticket.0, &fresh_ticket.0));
                assert!(!ticket.is_current());
                let copied = app.handle_chat_ui(Action::MessageCopied(
                    target,
                    "owned-report".into(),
                    fresh,
                    Ok(SAVED.into()),
                ));
                assert_eq!(full_copy_writes(copied), [SAVED]);
            }
        }
    }

    #[test]
    fn full_report_copy_old_ticket_stays_revoked_after_pane_hide_show_or_report_selection() {
        let _context = text_copy::isolated_context();
        for pane_hide in [false, true] {
            let (mut app, target, handle, source) = fixture();
            let mut snapshot = (*handle.snapshot()).clone();
            let mut other = (*snapshot.items[0]).clone();
            other.id = "other-report".into();
            snapshot.items.push(Arc::new(other));
            let mut other_source = source.clone();
            other_source.token += 1;
            snapshot
                .text_copy_sources
                .conversation
                .push(Some(other_source.clone()));
            handle.publish_snapshot_for_test(snapshot.clone());
            drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
            let request = full_copy_request(&mut app, target, &source);
            let ticket = app
                .chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .full_copy
                .as_ref()
                .unwrap()
                .1
                .clone();
            if pane_hide {
                app.chat_runtime_mut(target)
                    .unwrap()
                    .set_visible(false)
                    .unwrap();
                app.chat_runtime_mut(target)
                    .unwrap()
                    .set_visible(true)
                    .unwrap();
                // Restore every source/publication check; the old ticket alone
                // still prevents the deferred request from becoming live again.
                handle.publish_snapshot_for_test(snapshot);
                drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
            } else {
                let before = handle.snapshot();
                drop(app.handle_chat_ui(Action::ToggleReportOutput(
                    target,
                    "other-report".into(),
                    other_source.clone(),
                )));
                assert!(Arc::ptr_eq(&handle.snapshot(), &before));
            }
            assert!(!ticket.is_current());
            let obsolete = app.handle_chat_ui(Action::MessageCopied(
                target,
                "owned-report".into(),
                request,
                Ok(SAVED.into()),
            ));
            assert!(full_copy_writes(obsolete).is_empty());
            let fresh = full_copy_request(&mut app, target, &source);
            if !pane_hide {
                assert_eq!(
                    app.chat_runtime_mut(target)
                        .unwrap()
                        .provider_exit
                        .key
                        .as_ref()
                        .unwrap()
                        .id,
                    "other-report",
                    "header Copy must not replace the current inspector"
                );
                let fresh_ticket = app
                    .chat_runtime_mut(target)
                    .unwrap()
                    .provider_exit
                    .full_copy
                    .as_ref()
                    .unwrap()
                    .1
                    .clone();
                drop(app.handle_chat_ui(Action::ToggleReportOutput(
                    target,
                    "other-report".into(),
                    other_source,
                )));
                assert!(
                    fresh_ticket.is_current(),
                    "hiding another report must not revoke this report's Copy"
                );
            }
            let copied = app.handle_chat_ui(Action::MessageCopied(
                target,
                "owned-report".into(),
                fresh,
                Ok(SAVED.into()),
            ));
            assert_eq!(full_copy_writes(copied), [SAVED]);
        }
    }

    #[test]
    fn hiding_saved_output_keeps_an_unrelated_normal_transcript_copy_working() {
        let _context = text_copy::isolated_context();
        let (mut app, target, handle, source) = fixture();
        let body = "ordinary transcript body 🦀";
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.items.push(Arc::new(chat::Item {
            id: "ordinary".into(),
            role: chat::Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        }));
        let mut ordinary = source.clone();
        ordinary.token += 10;
        ordinary.bytes = body.len() as u64;
        snapshot
            .text_copy_sources
            .conversation
            .push(Some(ordinary.clone()));
        handle.publish_snapshot_for_test(snapshot);
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
        let read = begin(&mut app, target, &source);
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            read,
            Ok(Arc::new(SAVED.into())),
        )));
        let request = app
            .message_copy_request(target, "ordinary", Some(ordinary))
            .unwrap();
        drop(app.handle_chat_ui(Action::ToggleReportOutput(
            target,
            "owned-report".into(),
            source,
        )));
        let copied = app.handle_chat_ui(Action::MessageCopied(
            target,
            "ordinary".into(),
            request,
            Ok(body.into()),
        ));
        assert_eq!(full_copy_writes(copied), [body]);
    }

    #[test]
    fn full_report_copy_runtime_drop_revokes_request_and_effect_with_a_still_live_handle() {
        let _context = text_copy::isolated_context();
        for completed_before_remove in [false, true] {
            let (mut app, target, handle, source) = fixture();
            let request = full_copy_request(&mut app, target, &source);
            let (key, ticket) = app
                .chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .full_copy
                .as_ref()
                .unwrap()
                .clone();
            let pending = completed_before_remove.then(|| {
                app.handle_chat_ui(Action::MessageCopied(
                    target,
                    "owned-report".into(),
                    request.clone(),
                    Ok(SAVED.into()),
                ))
            });
            drop(
                app.remove_chat_card(target.card, None)
                    .expect("remove the report's actual owning runtime"),
            );
            assert!(!app.cards.contains_key(&target.card));
            assert!(handle.is_connected());
            assert!(snapshot_keeps(&handle.snapshot(), &key));
            assert!(!ticket.is_current());
            let obsolete = pending.unwrap_or_else(|| {
                app.handle_chat_ui(Action::MessageCopied(
                    target,
                    "owned-report".into(),
                    request,
                    Ok(SAVED.into()),
                ))
            });
            assert!(full_copy_writes(obsolete).is_empty());
        }
    }

    #[test]
    fn report_page_visibility_ticket_survives_live_updates_but_not_hide_show_or_runtime_drop() {
        let _context = text_copy::isolated_context();
        let (mut app, target, handle, source) = fixture();
        let original = (*handle.snapshot()).clone();
        let request = begin(&mut app, target, &source);
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            request,
            Ok(Arc::new(SAVED.into())),
        )));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let key = runtime.provider_exit.key.as_ref().unwrap().clone();
        let first = runtime.provider_exit.visibility.clone();
        assert!(first.load(Ordering::Relaxed));
        for _ in 0..3 {
            runtime.set_visible(true).unwrap();
            assert!(Arc::ptr_eq(&first, &runtime.provider_exit.visibility));
        }
        let selected: Arc<str> = Arc::from("Saved first");
        runtime.provider_exit.selection.borrow_mut().selected = Some(selected.clone());
        let pending = app.handle_chat_ui(Action::CopyReportSelection(
            target,
            key.clone(),
            0,
            selected.clone(),
        ));
        // Hide through the actual Runtime transition. Showing again must not
        // revive an Arc already retained by a queued clipboard effect/widget.
        app.chat_runtime_mut(target)
            .unwrap()
            .set_visible(false)
            .unwrap();
        assert!(!first.load(Ordering::Relaxed));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .request
                .is_none()
        );
        app.chat_runtime_mut(target)
            .unwrap()
            .set_visible(true)
            .unwrap();
        assert!(!first.load(Ordering::Relaxed));
        assert!(clipboard_writes(pending).is_empty());

        handle.publish_snapshot_for_test(original);
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
        let request = app
            .chat_runtime_mut(target)
            .unwrap()
            .provider_exit
            .request
            .as_ref()
            .unwrap()
            .clone();
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            request,
            Ok(Arc::new(SAVED.into())),
        )));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let fresh = runtime.provider_exit.visibility.clone();
        assert!(fresh.load(Ordering::Relaxed));
        assert!(!Arc::ptr_eq(&first, &fresh));
        runtime.provider_exit.selection.borrow_mut().selected = Some(selected.clone());
        let admitted = app.handle_chat_ui(Action::CopyReportSelection(
            target,
            key.clone(),
            0,
            selected.clone(),
        ));
        assert_eq!(clipboard_writes(admitted), [selected.to_string()]);
        let pending = app.handle_chat_ui(Action::CopyReportSelection(
            target,
            key.clone(),
            0,
            selected,
        ));
        drop(
            app.remove_chat_card(target.card, None)
                .expect("commit removal of the owning chat surface"),
        );
        assert!(!app.cards.contains_key(&target.card));
        assert!(!fresh.load(Ordering::Relaxed));
        assert!(handle.is_connected());
        assert!(snapshot_keeps(&handle.snapshot(), &key));
        assert!(
            clipboard_writes(pending).is_empty(),
            "a retained live handle is insufficient after its inspector owner drops"
        );
    }

    #[test]
    fn report_page_visibility_ticket_revokes_on_collapse_and_source_replacement() {
        let _context = text_copy::isolated_context();
        for withdraw in [false, true] {
            let (mut app, target, handle, source) = fixture();
            let request = begin(&mut app, target, &source);
            drop(app.handle_chat_ui(Action::ReportOutputLoaded(
                target,
                request,
                Ok(Arc::new(SAVED.into())),
            )));
            let runtime = app.chat_runtime_mut(target).unwrap();
            let key = runtime.provider_exit.key.as_ref().unwrap().clone();
            let ticket = runtime.provider_exit.visibility.clone();
            let selected: Arc<str> = Arc::from("Saved first");
            runtime.provider_exit.selection.borrow_mut().selected = Some(selected.clone());
            let pending = app.handle_chat_ui(Action::CopyReportSelection(
                target,
                key.clone(),
                0,
                selected,
            ));
            if withdraw {
                let mut withdrawn = (*handle.snapshot()).clone();
                withdrawn.text_copy_sources.conversation = vec![None];
                handle.publish_snapshot_for_test(withdrawn);
                drop(app.reload_report_output(target));
            } else {
                drop(app.handle_chat_ui(Action::ToggleReportOutput(
                    target,
                    "owned-report".into(),
                    source,
                )));
                assert!(snapshot_keeps(&handle.snapshot(), &key));
            }
            assert!(!ticket.load(Ordering::Relaxed));
            assert!(clipboard_writes(pending).is_empty());
            app.chat_runtime_mut(target)
                .unwrap()
                .set_visible(true)
                .unwrap();
            assert!(
                !ticket.load(Ordering::Relaxed),
                "an old ticket is never reused"
            );
        }
    }

    #[test]
    fn page_selection_copy_supersedes_delayed_full_copy_and_refuses_stale_actions() {
        let _context = text_copy::isolated_context();
        let (mut app, target, handle, source) = fixture();
        let request = begin(&mut app, target, &source);
        let loaded = Arc::new(SAVED.to_owned());
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            request,
            Ok(loaded.clone()),
        )));
        let key = app
            .chat_runtime_mut(target)
            .unwrap()
            .provider_exit
            .key
            .as_ref()
            .unwrap()
            .clone();
        let selected: Arc<str> = Arc::from("Saved first");
        app.chat_runtime_mut(target)
            .unwrap()
            .provider_exit
            .selection
            .borrow_mut()
            .selected = Some(selected.clone());
        let delayed = full_copy_request(&mut app, target, &source);
        let pointer_copy = app.handle_chat_ui(Action::CopyReportSelection(
            target,
            key.clone(),
            0,
            selected.clone(),
        ));
        assert_eq!(clipboard_writes(pointer_copy), [selected.to_string()]);
        let obsolete = app.handle_chat_ui(Action::MessageCopied(
            target,
            "owned-report".into(),
            delayed,
            Ok(SAVED.into()),
        ));
        assert!(
            clipboard_writes(obsolete).is_empty(),
            "an earlier full read cannot overwrite the newer pointer copy"
        );
        assert!(Arc::ptr_eq(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .ready
                .as_ref()
                .unwrap(),
            &loaded
        ));

        let stale_page = app.handle_chat_ui(Action::CopyReportSelection(
            target,
            key.clone(),
            1,
            selected.clone(),
        ));
        assert!(clipboard_writes(stale_page).is_empty());
        drop(app.handle_chat_ui(Action::SelectReportPage(target, key.clone(), 1)));
        assert!(
            !app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .selection
                .borrow()
                .select_all
        );
        drop(app.handle_chat_ui(Action::SelectReportPage(target, key.clone(), 0)));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .selection
                .borrow()
                .select_all
        );

        // Source withdrawal between the click and polling the actual effect
        // must also refuse the captured menu write.
        let pending = app.handle_chat_ui(Action::CopyReportSelection(
            target,
            key.clone(),
            0,
            selected.clone(),
        ));
        let mut withdrawn = (*handle.snapshot()).clone();
        withdrawn.text_copy_sources.conversation = vec![None];
        handle.publish_snapshot_for_test(withdrawn);
        assert!(clipboard_writes(pending).is_empty());
        let stale_source =
            app.handle_chat_ui(Action::CopyReportSelection(target, key, 0, selected));
        assert!(clipboard_writes(stale_source).is_empty());
    }

    #[test]
    fn page_keyboard_clipboard_effect_invalidates_older_full_copy_before_writing() {
        let _context = text_copy::isolated_context();
        let (mut app, target, handle, source) = fixture();
        let request = begin(&mut app, target, &source);
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            request,
            Ok(Arc::new(SAVED.into())),
        )));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let key = runtime.provider_exit.key.as_ref().unwrap().clone();
        let visible = runtime.provider_exit.visibility.clone();
        let delayed = full_copy_request(&mut app, target, &source);
        #[derive(Default)]
        struct MemoryClipboard(Vec<String>);
        impl Clipboard for MemoryClipboard {
            fn read(&self, _kind: iced::advanced::clipboard::Kind) -> Option<String> {
                self.0.last().cloned()
            }
            fn write(&mut self, _kind: iced::advanced::clipboard::Kind, text: String) {
                self.0.push(text);
            }
        }
        let mut clipboard = MemoryClipboard::default();
        SelectionClipboard {
            clipboard: &mut clipboard,
            key: &key,
            handle: &handle,
            visible: &visible,
        }
        .write(
            iced::advanced::clipboard::Kind::Standard,
            "Saved final line".into(),
        );
        assert_eq!(clipboard.0, ["Saved final line"]);
        let obsolete = app.handle_chat_ui(Action::MessageCopied(
            target,
            "owned-report".into(),
            delayed,
            Ok(SAVED.into()),
        ));
        assert!(clipboard_writes(obsolete).is_empty());
        visible.store(false, Ordering::Relaxed);
        SelectionClipboard {
            clipboard: &mut clipboard,
            key: &key,
            handle: &handle,
            visible: &visible,
        }
        .write(
            iced::advanced::clipboard::Kind::Standard,
            "hidden page".into(),
        );
        assert_eq!(clipboard.0, ["Saved final line"]);
    }

    fn page_text_fixture() -> PageText {
        let (_app, _target, handle, source) = fixture();
        PageText {
            text: widget::text(SAVED.to_owned())
                .selectable()
                .font(iced::Font::MONOSPACE)
                .size(design::TEXT_CAPTION)
                .width(Length::Fill),
            selection: Rc::default(),
            key: Key {
                id: "owned-report".into(),
                source,
                conversation_revision: 4,
            },
            handle,
            visible: Arc::new(AtomicBool::new(true)),
        }
    }

    #[test]
    fn saved_output_page_retains_native_accessibility_identity_across_view_rebuilds() {
        let mut first = page_text_fixture();
        let id = first.id().expect("the native text ID must be forwarded");
        let mut tree = Tree::new(&first as &dyn Widget<Message, cosmic::Theme, cosmic::Renderer>);
        assert_eq!(tree.id, Some(id.clone()));
        let node = layout::Node::new(Size::new(240.0, 80.0));
        for _ in 0..3 {
            let mut rebuilt = PageText {
                text: widget::text(SAVED.to_owned()).selectable(),
                selection: first.selection.clone(),
                key: first.key.clone(),
                handle: first.handle.clone(),
                visible: first.visible.clone(),
            };
            assert_ne!(
                rebuilt.id(),
                Some(id.clone()),
                "the rebuilt native text begins with a fresh ID"
            );
            tree.diff(&mut rebuilt as &mut dyn Widget<Message, cosmic::Theme, cosmic::Renderer>);
            assert_eq!(rebuilt.id(), Some(id.clone()));
            assert_eq!(tree.id, Some(id.clone()));
            assert!(
                rebuilt
                    .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable)
                    .contains(&id.clone().into())
            );
            first = rebuilt;
        }
    }

    #[test]
    fn saved_output_page_keyboard_focus_paints_without_selection_and_clears_on_blur() {
        use iced::advanced::{Renderer as _, renderer::Headless as _};
        use widget::HasSelectableText;
        struct Focus(bool);
        impl Operation for Focus {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                _id: Option<&widget::Id>,
                _bounds: Rectangle,
                state: &mut dyn iced::advanced::widget::operation::Focusable,
            ) {
                if self.0 {
                    state.focus();
                } else {
                    state.unfocus();
                }
            }
        }
        let mut renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                iced::Font::default(),
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut page = page_text_fixture();
        let mut tree = Tree::new(&page as &dyn Widget<Message, cosmic::Theme, cosmic::Renderer>);
        let node = page
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(200.0, 56.0)),
            )
            .move_to(iced::Point::new(8.0, 4.0));
        let viewport = Rectangle::with_size(Size::new(220.0, 64.0));
        let theme = cosmic::Theme::default();
        let paint = |renderer: &mut cosmic::Renderer, page: &PageText, tree: &Tree| {
            renderer.reset(viewport);
            page.draw(
                tree,
                renderer,
                &theme,
                &renderer::Style::default(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &viewport,
            );
            renderer.screenshot(Size::new(220, 64), 1.0, iced::Color::WHITE)
        };
        let unfocused = paint(&mut renderer, &page, &tree);
        page.operate(&mut tree, Layout::new(&node), &renderer, &mut Focus(true));
        assert!(
            page.text.selected_text(&tree).is_none(),
            "focus alone must not select text"
        );
        let focused = paint(&mut renderer, &page, &tree);
        assert_ne!(
            focused, unfocused,
            "actual page draw must reveal keyboard focus even with an empty selection"
        );
        page.operate(&mut tree, Layout::new(&node), &renderer, &mut Focus(false));
        assert_eq!(paint(&mut renderer, &page, &tree), unfocused);
    }

    #[test]
    fn report_view_shapes_one_page_and_full_copy_preserves_publisher_readable_labels() {
        use iced::futures::StreamExt;
        #[derive(Default)]
        struct TextProbe(Vec<String>);
        impl Operation for TextProbe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _id: Option<&widget::Id>, _bounds: Rectangle, text: &str) {
                self.0.push(text.to_owned());
            }
        }
        fn rendered(runtime: &Runtime, renderer: &cosmic::Renderer) -> (Vec<String>, f32) {
            let item = runtime.active_item("owned-report").unwrap();
            let mut element = runtime.report_output_view(item).unwrap();
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(600.0, 900.0)),
            );
            let mut probe = TextProbe::default();
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), renderer, &mut probe);
            (probe.0, node.size().height)
        }
        let _context = text_copy::isolated_context();
        let (mut app, target, handle, mut source) = fixture();
        // This is already-readable, publisher-labeled synthetic data. It does
        // not exercise raw capture, UTF-8 replacement, ANSI cleanup or omission
        // accounting, which remain separate producer qualification gates.
        let full = Arc::new(format!(
            "Provider exited with status 7\n[Unreadable bytes are shown as �]\n{}\nLast retained line\n[7 raw output bytes were not retained]",
            "猫 retained body\n".repeat(3_000),
        ));
        source.bytes = full.len() as u64;
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.error = Some("Provider exited with status 7".into());
        snapshot.text_copy_sources.conversation = vec![Some(source.clone())];
        handle.publish_snapshot_for_test(snapshot);
        drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
        let request = begin(&mut app, target, &source);
        drop(app.handle_chat_ui(Action::ReportOutputLoaded(
            target,
            request,
            Ok(full.clone()),
        )));
        let renderer = crate::app::tests::headless_renderer();
        let first = full[page_bounds(&full, 0).0].to_owned();
        let (texts, height) = rendered(app.chat_runtime_mut(target).unwrap(), &renderer);
        assert!(texts.contains(&first));
        assert!(texts.iter().all(|text| text.len() <= PAGE_BYTES + 3));
        assert!(
            height < 400.0,
            "the shaped page stays in a bounded viewport"
        );
        drop(app.handle_chat_ui(Action::ReportOutputPage(target, true)));
        let second = full[page_bounds(&full, 1).0].to_owned();
        let (texts, height) = rendered(app.chat_runtime_mut(target).unwrap(), &renderer);
        assert!(texts.contains(&second));
        assert!(!texts.contains(&first));
        assert!(texts.iter().all(|text| text.len() <= PAGE_BYTES + 3));
        assert!(height < 400.0);

        let copy = full_copy_request(&mut app, target, &source);
        let task = app.handle_chat_ui(Action::MessageCopied(
            target,
            "owned-report".into(),
            copy,
            Ok((*full).clone()),
        ));
        let effects = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(3), async {
                    match iced::runtime::task::into_stream(task) {
                        Some(stream) => stream.collect::<Vec<_>>().await,
                        None => vec![],
                    }
                })
                .await
                .expect("copy effect and acknowledgement timer must settle")
            });
        let copied = effects
            .into_iter()
            .filter_map(|effect| match effect {
                iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write {
                    target: iced::advanced::clipboard::Kind::Standard,
                    contents,
                }) => Some(contents),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            copied,
            [(*full).clone()],
            "copy must preserve the complete publisher-readable source, including its labels"
        );
        assert!(Arc::ptr_eq(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .ready
                .as_ref()
                .unwrap(),
            &full
        ));
        drop(app.handle_chat_ui(Action::ToggleReportOutput(
            target,
            "owned-report".into(),
            source,
        )));
        let (texts, _) = rendered(app.chat_runtime_mut(target).unwrap(), &renderer);
        assert!(!texts.contains(&first) && !texts.contains(&second));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit
                .ready
                .is_none()
        );
    }
}
