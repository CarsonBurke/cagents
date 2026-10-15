//! Provider output is loaded only after an explicit disclosure or copy, and
//! only the visible page is shaped. Hidden panes release their full buffer.
use super::*;

const PAGE_BYTES: usize = 16 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    log: chat::ProviderExitLog,
    turn: u64,
}
impl Key {
    fn matches(&self, snapshot: &Snapshot) -> bool {
        self.turn == snapshot.turn_generation
            && snapshot
                .provider_exit
                .as_ref()
                .and_then(|diagnostic| diagnostic.log.as_ref())
                == Some(&self.log)
    }
    fn from_snapshot(snapshot: &Snapshot) -> Option<Self> {
        Some(Self {
            log: snapshot.provider_exit.as_ref()?.log.clone()?,
            turn: snapshot.turn_generation,
        })
    }
}

#[derive(Clone)]
pub(super) struct Request {
    id: u64,
    key: Key,
    cancelled: Arc<AtomicBool>,
}
struct Loading {
    request: Request,
    copy: bool,
}
#[derive(Default)]
pub(super) struct State {
    key: Option<Key>,
    request: Option<Loading>,
    ready: Option<chat::client::ProviderExitOutput>,
    error: Option<String>,
    next: u64,
    page: usize,
}
impl State {
    fn advance_page(&mut self, forward: bool, bytes: usize) {
        let last = bytes.div_ceil(PAGE_BYTES).max(1) - 1;
        self.page = if forward {
            self.page.saturating_add(1).min(last)
        } else {
            self.page.min(last).saturating_sub(1)
        };
    }
    pub(super) fn reconcile(&mut self, snapshot: &Snapshot) {
        let key = Key::from_snapshot(snapshot);
        if self.key != key {
            if let Some(loading) = self.request.take() {
                loading.request.cancelled.store(true, Ordering::Release);
            }
            self.ready = None;
            self.error = None;
            self.page = 0;
            self.key = key;
        }
    }
    pub(super) fn hide(&mut self) {
        self.ready = None;
        if self.request.as_ref().is_some_and(|loading| !loading.copy)
            && let Some(loading) = self.request.take()
        {
            loading.request.cancelled.store(true, Ordering::Release);
        }
    }
    fn begin(&mut self, key: Key, copy: bool) -> Option<Request> {
        if let Some(loading) = &mut self.request {
            loading.copy |= copy;
            return None;
        }
        self.next = self.next.wrapping_add(1);
        let request = Request {
            id: self.next,
            key,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.request = Some(Loading {
            request: request.clone(),
            copy,
        });
        self.error = None;
        Some(request)
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(loading) = &self.request {
            loading.request.cancelled.store(true, Ordering::Release);
        }
    }
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
    pub(super) fn provider_exit_page(&mut self, forward: bool) {
        let Some(diagnostic) = &self.snapshot.provider_exit else {
            return;
        };
        if !self.provider_exit_expanded {
            return;
        }
        let bytes = self
            .provider_exit_output
            .ready
            .as_ref()
            .map_or(diagnostic.output.len(), |output| output.text.len());
        self.provider_exit_output.advance_page(forward, bytes);
    }
    /// Replace only the latest matching failure row; earlier identical exits
    /// remain part of the conversation's history.
    pub(super) fn provider_exit_failure_index(&self) -> Option<usize> {
        let diagnostic = self.snapshot.provider_exit.as_ref()?;
        self.failures
            .iter()
            .rposition(|(_, message)| message == &diagnostic.summary)
    }
    pub(super) fn provider_exit_notice(&self, message: &str) -> bool {
        self.snapshot
            .provider_exit
            .as_ref()
            .is_some_and(|diagnostic| diagnostic.summary == message)
    }
    pub(super) fn provider_exit_view(&self) -> Option<Element<'_, Message>> {
        let diagnostic = self.snapshot.provider_exit.as_ref()?;
        let target = self.target;
        let state = &self.provider_exit_output;
        let mut content = vec![super::view::notice_entry(&diagnostic.summary, target)];
        let mut controls = Vec::new();
        if !diagnostic.output.is_empty() || diagnostic.log.is_some() {
            controls.push(
                widget::button::custom(design::caption(if self.provider_exit_expanded {
                    "Hide output"
                } else if diagnostic.log.is_some() {
                    "Show saved output"
                } else {
                    "Show recent output"
                }))
                .class(crate::ui_controls::control_style(false, None))
                .on_press(action(Action::ToggleProviderExit(target)))
                .into(),
            );
        }
        let copying = state.request.as_ref().is_some_and(|loading| loading.copy);
        controls.push(
            widget::button::custom(design::caption(if copying {
                "Copying…"
            } else if diagnostic.log.is_some() {
                "Copy saved output"
            } else {
                "Copy recent output"
            }))
            .class(crate::ui_controls::control_style(false, None))
            .on_press_maybe((!copying).then(|| action(Action::CopyProviderExit(target))))
            .into(),
        );
        content.push(
            widget::row::with_children(controls)
                .spacing(step::GAP_S)
                .wrap()
                .into(),
        );
        if self.provider_exit_expanded {
            if let Some(error) = &state.error {
                content
                    .push(design::caption(format!("Could not load saved output: {error}")).into());
                content.push(
                    widget::row::with_children(vec![
                        crate::ui_controls::button::quiet("Retry loading")
                            .on_press(action(Action::LoadProviderExit(target)))
                            .into(),
                        crate::ui_controls::button::quiet("Copy recent output")
                            .on_press(action(Action::CopyProviderExitRecent(target)))
                            .into(),
                    ])
                    .spacing(step::GAP_S)
                    .wrap()
                    .into(),
                );
            } else if state.request.is_some() {
                content.push(
                    design::caption("Loading saved output… Recent output is shown below.").into(),
                );
            } else if diagnostic.log.is_none() {
                content.push(
                    design::caption("Only recent provider output is available for this run.")
                        .into(),
                );
            }
            let ready = state.ready.as_ref().filter(|_| {
                state
                    .key
                    .as_ref()
                    .is_some_and(|key| key.matches(&self.snapshot))
            });
            let output = ready.map_or(diagnostic.output.as_str(), |output| output.text.as_str());
            let omitted = ready.map_or(0, |output| output.omitted);
            if omitted > 0 {
                content.push(
                    design::caption(format!("{omitted} raw output bytes were not retained."))
                        .into(),
                );
            }
            let (range, page, pages) = page_bounds(output, state.page);
            if pages > 1 {
                content.push(
                    widget::row::with_children(vec![
                        crate::ui_controls::button::quiet("Previous")
                            .on_press_maybe(
                                (page > 0).then(|| action(Action::ProviderExitPage(target, false))),
                            )
                            .into(),
                        design::caption(format!("{} / {pages}", page + 1)).into(),
                        crate::ui_controls::button::quiet("Next")
                            .on_press_maybe(
                                (page + 1 < pages)
                                    .then(|| action(Action::ProviderExitPage(target, true))),
                            )
                            .into(),
                    ])
                    .spacing(step::GAP_S)
                    .align_y(Alignment::Center)
                    .into(),
                );
            }
            content.push(
                widget::container(
                    widget::scrollable(
                        widget::selectable_text(&output[range])
                            .size(design::TEXT_CAPTION)
                            .font(iced::Font::MONOSPACE)
                            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                            .width(Length::Fill),
                    )
                    .id(widget::Id::new(format!(
                        "provider-output-{target:?}-{}-{page}",
                        diagnostic
                            .log
                            .as_ref()
                            .map_or("recent", |log| log.id.as_str())
                    )))
                    .height(240),
                )
                .width(Length::Fill)
                .padding(step::GAP_S as u16)
                .class(design::surface(
                    Some(design::sunken),
                    [design::RADIUS_M; 4],
                    false,
                ))
                .into(),
            );
        }
        Some(
            widget::column::with_children(content)
                .spacing(step::GAP_S)
                .width(Length::Fill)
                .into(),
        )
    }
}

pub(super) fn copy_text(diagnostic: &chat::ProviderExitDiagnostic) -> String {
    if diagnostic.output.is_empty() {
        diagnostic.summary.clone()
    } else {
        format!("{}\n\n{}", diagnostic.summary, diagnostic.output)
    }
}
fn copy_saved(
    diagnostic: &chat::ProviderExitDiagnostic,
    output: &chat::client::ProviderExitOutput,
) -> String {
    let mut text = format!("{}\n\n{}", diagnostic.summary, output.text);
    if output.omitted > 0 {
        text.push_str(&format!(
            "\n\n[{} raw output bytes were not retained]",
            output.omitted
        ));
    }
    text
}

impl Cagents {
    pub(super) fn toggle_provider_exit(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.snapshot.provider_exit.is_none() {
            return Task::none();
        }
        runtime.provider_exit_expanded = !runtime.provider_exit_expanded;
        if !runtime.provider_exit_expanded {
            runtime.provider_exit_output.hide();
            return Task::none();
        }
        self.load_provider_exit(target, false)
    }
    pub(super) fn copy_recent_provider_exit(&mut self, target: Target) -> Task<Message> {
        let text = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.snapshot.provider_exit.as_ref())
            .map(copy_text);
        if let Some(text) = text {
            self.status = Some(TransientNotice::success("Copied recent provider output"));
            iced::clipboard::write(text).map(cosmic::Action::App)
        } else {
            Task::none()
        }
    }
    pub(super) fn reload_provider_exit(&mut self, target: Target) -> Task<Message> {
        let needed = self.chat_runtime_mut(target).is_some_and(|runtime| {
            runtime.provider_exit_expanded
                && runtime.visible.load(Ordering::Relaxed)
                && runtime.provider_exit_output.ready.is_none()
                && runtime.provider_exit_output.request.is_none()
                && runtime.provider_exit_output.error.is_none()
        });
        if needed {
            self.load_provider_exit(target, false)
        } else {
            Task::none()
        }
    }
    pub(super) fn load_provider_exit(&mut self, target: Target, copy: bool) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(diagnostic) = runtime.snapshot.provider_exit.as_ref() else {
            return Task::none();
        };
        let Some(key) = Key::from_snapshot(&runtime.snapshot) else {
            return if copy {
                self.copy_recent_provider_exit(target)
            } else {
                Task::none()
            };
        };
        if !copy && (!runtime.provider_exit_expanded || !runtime.visible.load(Ordering::Relaxed)) {
            return Task::none();
        }
        runtime.provider_exit_output.reconcile(&runtime.snapshot);
        if let Some(output) = &runtime.provider_exit_output.ready {
            if copy {
                let text = copy_saved(diagnostic, output);
                self.status = Some(TransientNotice::success("Copied saved provider output"));
                return iced::clipboard::write(text).map(cosmic::Action::App);
            }
            return Task::none();
        }
        let Some(handle) = runtime.handle().cloned() else {
            let message = "Reconnect this conversation to load its saved output.";
            runtime.provider_exit_output.error = Some(message.into());
            if copy {
                self.status = Some(TransientNotice::error(message));
            }
            return Task::none();
        };
        let Some(request) = runtime.provider_exit_output.begin(key, copy) else {
            return Task::none();
        };
        cosmic::task::future(async move {
            let load = request.clone();
            let result = tokio::task::spawn_blocking(move || {
                handle.provider_exit_output(&load.key.log, &load.cancelled)
            })
            .await
            .map_err(|_| "Provider output loading could not finish".to_owned())
            .and_then(|result| result);
            action(Action::ProviderExitLoaded(target, request, result))
        })
    }
    pub(super) fn provider_exit_loaded(
        &mut self,
        target: Target,
        request: Request,
        result: Result<chat::client::ProviderExitOutput, String>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !request.key.matches(&runtime.snapshot) || request.cancelled.load(Ordering::Acquire) {
            return Task::none();
        }
        let Some(loading) = runtime
            .provider_exit_output
            .request
            .as_ref()
            .filter(|loading| loading.request.id == request.id)
        else {
            return Task::none();
        };
        let copy = loading.copy;
        runtime.provider_exit_output.request = None;
        match result {
            Ok(output) => {
                let copied = copy
                    .then(|| copy_saved(runtime.snapshot.provider_exit.as_ref().unwrap(), &output));
                runtime.provider_exit_output.error = None;
                if runtime.provider_exit_expanded && runtime.visible.load(Ordering::Relaxed) {
                    runtime.provider_exit_output.ready = Some(output);
                }
                if let Some(text) = copied {
                    self.status = Some(TransientNotice::success("Copied saved provider output"));
                    return iced::clipboard::write(text).map(cosmic::Action::App);
                }
            }
            Err(error) => {
                runtime.provider_exit_output.error = Some(error.clone());
                if copy {
                    self.status = Some(TransientNotice::error(format!(
                        "Could not copy saved output: {error}"
                    )));
                }
            }
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_output_late_results_ignore_old_turns_and_hidden_copy_retains_no_buffer() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.new_native_chat(
            Provider::Codex,
            Some(PathBuf::from("/workspace/chat-fixture")),
        ));
        let card = app.workspace.focused().unwrap();
        let target = match app.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime.target,
            _ => panic!("draft chat"),
        };
        let log = chat::ProviderExitLog {
            id: "exit-one".into(),
            bytes: 5,
            omitted: 2,
        };
        let runtime = app.chat_runtime_mut(target).unwrap();
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.provider_exit = Some(chat::ProviderExitDiagnostic {
            summary: "Exited".into(),
            output: "tail".into(),
            log: Some(log),
        });
        runtime.provider_exit_output.reconcile(&runtime.snapshot);
        let request = runtime
            .provider_exit_output
            .begin(Key::from_snapshot(&runtime.snapshot).unwrap(), false)
            .unwrap();
        Arc::make_mut(&mut runtime.snapshot).turn_generation += 1;
        runtime.provider_exit_output.reconcile(&runtime.snapshot);
        assert!(request.cancelled.load(Ordering::Acquire));
        let text = Arc::new("stale".to_owned());
        let weak = Arc::downgrade(&text);
        drop(app.provider_exit_loaded(
            target,
            request,
            Ok(chat::client::ProviderExitOutput { text, omitted: 2 }),
        ));
        assert!(weak.upgrade().is_none());
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(runtime.provider_exit_output.ready.is_none());
        let request = runtime
            .provider_exit_output
            .begin(Key::from_snapshot(&runtime.snapshot).unwrap(), true)
            .unwrap();
        runtime.provider_exit_expanded = true;
        runtime.set_visible(false).unwrap();
        assert!(!request.cancelled.load(Ordering::Acquire));
        let text = Arc::new("saved".to_owned());
        let weak = Arc::downgrade(&text);
        drop(app.provider_exit_loaded(
            target,
            request,
            Ok(chat::client::ProviderExitOutput { text, omitted: 2 }),
        ));
        assert!(
            weak.upgrade().is_none(),
            "a hidden copy must not retain the loaded presentation"
        );
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(runtime.provider_exit_output.ready.is_none());
        assert!(runtime.provider_exit_output.request.is_none());
    }

    #[test]
    fn provider_output_copy_contains_summary_and_complete_retained_output() {
        let output = format!("first line\n{}\nlast line", "diagnostic ".repeat(5_000));
        let diagnostic = chat::ProviderExitDiagnostic {
            summary: "Provider exited with status 3".into(),
            output: output.clone(),
            log: None,
        };
        assert_eq!(
            copy_text(&diagnostic),
            format!("{}\n\n{output}", diagnostic.summary)
        );
        assert_eq!(
            copy_text(&chat::ProviderExitDiagnostic {
                summary: "Provider was terminated".into(),
                output: String::new(),
                log: None
            }),
            "Provider was terminated"
        );
    }
    #[test]
    fn provider_output_visual_pages_are_bounded_and_lossless_at_utf8_boundaries() {
        let text = "€".repeat(PAGE_BYTES);
        let mut restored = String::new();
        let pages = text.len().div_ceil(PAGE_BYTES);
        for index in 0..pages {
            let (range, page, actual) = page_bounds(&text, index);
            assert_eq!((page, actual), (index, pages));
            assert!(range.len() <= PAGE_BYTES + 3);
            restored.push_str(&text[range]);
        }
        assert_eq!(restored, text);
    }
    #[test]
    fn provider_output_hide_releases_buffer_and_cancels_only_disclosure_loading() {
        let key = Key {
            log: chat::ProviderExitLog {
                id: "run".into(),
                bytes: 5,
                omitted: 0,
            },
            turn: 1,
        };
        let mut state = State::default();
        state.key = Some(key.clone());
        let request = state.begin(key.clone(), false).unwrap();
        state.hide();
        assert!(request.cancelled.load(Ordering::Acquire));
        assert!(state.request.is_none());
        let request = state.begin(key, true).unwrap();
        let text = Arc::new("saved".to_owned());
        let weak = Arc::downgrade(&text);
        state.ready = Some(chat::client::ProviderExitOutput { text, omitted: 0 });
        state.hide();
        assert!(weak.upgrade().is_none());
        assert!(!request.cancelled.load(Ordering::Acquire));
        assert!(state.request.as_ref().unwrap().copy);
    }
}
