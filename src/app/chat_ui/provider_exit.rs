//! Provider output is loaded only after an explicit disclosure or copy, and
//! only the visible page is shaped. Hidden panes release their full buffer.
use super::*;

const PAGE_BYTES: usize = 16 * 1024;
const SESSION_BYTES: usize = 1024;
const RECENT_BYTES: usize = 64 * 1024;

use chat::client::{
    DiagnosticAuthority, DiagnosticCompletion, DiagnosticCopyCompletion, DiagnosticCopyInterest,
    DiagnosticCopySummary, DiagnosticOutput, DiagnosticReadRequest,
};
use std::sync::Weak;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    log: chat::ProviderExitLog,
    turn: u64,
    session: Option<Arc<str>>,
}
impl Key {
    fn matches(&self, snapshot: &Snapshot) -> bool {
        self.turn == snapshot.turn_generation
            && self.session.as_deref() == snapshot.session_id.as_deref()
            && snapshot
                .provider_exit
                .as_ref()
                .and_then(|diagnostic| diagnostic.log.as_ref())
                == Some(&self.log)
    }
    fn from_snapshot(snapshot: &Snapshot) -> Option<Self> {
        let log = snapshot.provider_exit.as_ref()?.log.as_ref()?;
        if log.id.is_empty() || log.id.len() > 256 {
            return None;
        }
        let session = snapshot.session_id.as_deref();
        if session.is_some_and(|value| value.len() > SESSION_BYTES) {
            return None;
        }
        Some(Self {
            log: chat::ProviderExitLog {
                id: log.id.as_str().to_owned(),
                bytes: log.bytes,
                omitted: log.omitted,
            },
            turn: snapshot.turn_generation,
            session: session.map(Arc::from),
        })
    }
}
#[derive(Clone)]
struct Scope {
    handle: Weak<ClientHandle>,
    key: Key,
    proof: Option<Arc<crate::model::HostSessionProof>>,
}
impl Scope {
    fn capture(handle: &Arc<ClientHandle>, key: Key) -> Self {
        Self {
            handle: Arc::downgrade(handle),
            key,
            proof: handle.diagnostic_proof().map(Arc::new),
        }
    }
    fn current(&self) -> bool {
        self.proof.as_ref().is_some_and(|proof| {
            self.handle.upgrade().is_some_and(|handle| {
                handle.diagnostic_snapshot_matches(proof, self.key.session.as_deref(), |snapshot| {
                    self.key.matches(snapshot)
                })
            })
        })
    }
    fn matches(&self, runtime: &Runtime) -> bool {
        self.key.matches(&runtime.snapshot)
            && runtime
                .handle()
                .is_some_and(|handle| Weak::ptr_eq(&self.handle, &Arc::downgrade(handle)))
            && self.current()
    }
    fn authority(&self) -> DiagnosticAuthority {
        let scope = self.clone();
        Arc::new(move || scope.current())
    }
}
#[derive(Clone)]
pub(super) struct Request {
    id: u64,
    scope: Scope,
    job: DiagnosticReadRequest,
}
impl Request {
    fn current(&self, runtime: &Runtime) -> bool {
        !self.job.cancelled() && self.scope.matches(runtime)
    }
}
impl clipboard_intent::Pending for DiagnosticCopyInterest {
    fn withdraw(&self) {
        DiagnosticCopyInterest::withdraw(self);
    }
}
struct CopyInterest {
    intent: clipboard_intent::Intent,
    _interest: Arc<DiagnosticCopyInterest>,
}
impl CopyInterest {
    fn new(intent: clipboard_intent::Intent, job: &DiagnosticReadRequest) -> Self {
        let interest = job.copy_interest();
        intent.register(&interest);
        Self {
            intent,
            _interest: interest,
        }
    }
    fn current(&self) -> bool {
        self.intent.current()
    }
}
struct Loading {
    request: Request,
    copy: Option<CopyInterest>,
    formatted_summary: Option<DiagnosticCopySummary>,
}
struct Ready {
    output: DiagnosticOutput,
    scope: Scope,
}
#[derive(Default)]
pub(super) struct State {
    key: Option<Key>,
    actor: Option<Weak<ClientHandle>>,
    request: Option<Loading>,
    ready: Option<Ready>,
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
    pub(super) fn clear(&mut self) {
        if let Some(loading) = self.request.take() {
            loading.request.job.cancel();
        }
        self.ready = None;
        self.key = None;
        self.actor = None;
        self.error = None;
        self.page = 0;
    }
    pub(super) fn reconcile(&mut self, snapshot: &Snapshot, actor: Option<Weak<ClientHandle>>) {
        let key = Key::from_snapshot(snapshot);
        let same_actor = match (&self.actor, &actor) {
            (Some(old), Some(new)) => Weak::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        if self.key != key || !same_actor {
            self.clear();
            self.key = key;
            self.actor = actor;
        }
        self.reap_cancelled();
    }
    fn reap_cancelled(&mut self) {
        if self
            .request
            .as_ref()
            .is_some_and(|loading| loading.request.job.cancelled())
        {
            self.request = None;
        }
    }
    pub(super) fn hide(&mut self) {
        self.ready = None;
        if let Some(loading) = &self.request {
            loading.request.job.set_disclosed(false);
        }
        self.reap_cancelled();
    }
    fn coalesce(&mut self, copy: Option<clipboard_intent::Intent>, disclosed: bool) -> bool {
        self.reap_cancelled();
        let Some(loading) = &mut self.request else {
            return false;
        };
        if let Some(intent) = copy {
            loading.copy = Some(CopyInterest::new(intent, &loading.request.job));
        }
        loading.request.job.set_disclosed(disclosed);
        true
    }
    fn begin(
        &mut self,
        scope: Scope,
        job: DiagnosticReadRequest,
        copy: Option<clipboard_intent::Intent>,
    ) -> Request {
        self.next = self.next.wrapping_add(1);
        let request = Request {
            id: self.next,
            scope,
            job,
        };
        let copy = copy.map(|intent| CopyInterest::new(intent, &request.job));
        self.request = Some(Loading {
            request: request.clone(),
            copy,
            formatted_summary: None,
        });
        self.error = None;
        request
    }
    fn owns(&self, request: &Request) -> bool {
        self.request.as_ref().is_some_and(|loading| {
            loading.request.id == request.id && loading.request.job.same_job(&request.job)
        })
    }
    fn take(&mut self, request: &Request) -> Option<Loading> {
        if !self.owns(request) {
            return None;
        }
        self.request.take()
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.clear();
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
            .map_or(diagnostic.output.len(), |ready| ready.output.as_str().len());
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
        let copying = state.request.as_ref().is_some_and(|loading| {
            !loading.request.job.cancelled()
                && loading.copy.as_ref().is_some_and(CopyInterest::current)
        });
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
            let ready = state
                .ready
                .as_ref()
                .filter(|ready| ready.scope.matches(self));
            let output = ready.map_or(diagnostic.output.as_str(), |ready| ready.output.as_str());
            let omitted = ready.map_or(0, |ready| ready.output.omitted());
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
        let intent = self.chat_ui.copy_intents.next();
        self.copy_recent_provider_exit_for_intent(target, intent)
    }
    fn copy_recent_provider_exit_for_intent(
        &mut self,
        target: Target,
        intent: clipboard_intent::Intent,
    ) -> Task<Message> {
        if !intent.current() {
            return Task::none();
        }
        let text = self.chat_runtime_mut(target).and_then(|runtime| {
            let handle = runtime.handle()?;
            let diagnostic = runtime.snapshot.provider_exit.as_ref()?;
            if diagnostic.summary.len() > 1024
                || diagnostic.output.len() > RECENT_BYTES
                || runtime
                    .snapshot
                    .session_id
                    .as_ref()
                    .is_some_and(|session| session.len() > SESSION_BYTES)
            {
                return None;
            }
            let proof = handle.diagnostic_proof()?;
            handle
                .diagnostic_snapshot_matches(
                    &proof,
                    runtime.snapshot.session_id.as_deref(),
                    |snapshot| {
                        snapshot.turn_generation == runtime.snapshot.turn_generation
                            && snapshot.provider_exit.as_ref() == Some(diagnostic)
                    },
                )
                .then(|| copy_text(diagnostic))
        });
        if let Some(text) = text {
            self.status = Some(TransientNotice::success("Copied recent provider output"));
            iced::clipboard::write(text).map(cosmic::Action::App)
        } else {
            Task::none()
        }
    }
    pub(super) fn reload_provider_exit(&mut self, target: Target) -> Task<Message> {
        let needed = self.chat_runtime_mut(target).is_some_and(|runtime| {
            runtime.provider_exit_output.reap_cancelled();
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
        let intent = copy.then(|| self.chat_ui.copy_intents.next());
        self.load_provider_exit_for_intent(target, intent)
    }
    fn load_provider_exit_for_intent(
        &mut self,
        target: Target,
        copy: Option<clipboard_intent::Intent>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(key) = Key::from_snapshot(&runtime.snapshot) else {
            if runtime
                .snapshot
                .provider_exit
                .as_ref()
                .is_some_and(|diagnostic| diagnostic.log.is_some())
            {
                let error = "The saved output reference is unavailable.";
                runtime.provider_exit_output.error = Some(error.into());
                if copy.as_ref().is_some_and(|intent| intent.current()) {
                    self.status = Some(TransientNotice::error(error));
                }
                return Task::none();
            }
            return copy.map_or_else(Task::none, |intent| {
                self.copy_recent_provider_exit_for_intent(target, intent)
            });
        };
        let disclosed = runtime.provider_exit_expanded && runtime.visible.load(Ordering::Relaxed);
        if copy.is_none() && !disclosed {
            return Task::none();
        }
        let actor = runtime.handle().map(Arc::downgrade);
        runtime
            .provider_exit_output
            .reconcile(&runtime.snapshot, actor);
        if copy.is_some()
            && runtime
                .provider_exit_output
                .request
                .as_ref()
                .is_some_and(|loading| {
                    loading.formatted_summary.as_ref().is_some_and(|summary| {
                        runtime
                            .snapshot
                            .provider_exit
                            .as_ref()
                            .is_none_or(|diagnostic| diagnostic.summary != summary.as_str())
                    })
                })
        {
            if let Some(old) = runtime.provider_exit_output.request.take() {
                old.request.job.cancel();
            }
        }
        if runtime
            .provider_exit_output
            .coalesce(copy.clone(), disclosed)
        {
            return Task::none();
        }
        let Some(handle) = runtime.handle().cloned() else {
            runtime.provider_exit_output.error =
                Some("Reconnect this conversation to load its saved output.".into());
            return Task::none();
        };
        let scope = Scope::capture(&handle, key.clone());
        if !scope.current() {
            runtime.provider_exit_output.clear();
            return Task::none();
        }
        let job = match handle.diagnostic_request(
            target.card,
            target.generation,
            key.turn,
            &key.log,
            disclosed,
            copy.is_some(),
            scope.authority(),
        ) {
            Ok(job) => job,
            Err(error) => {
                runtime.provider_exit_output.error = Some(error.clone());
                if copy.as_ref().is_some_and(|intent| intent.current()) {
                    self.status = Some(TransientNotice::error(error));
                }
                return Task::none();
            }
        };
        // A fresh copy request has its own whole-operation deadline. A ready
        // resident output remains reusable after the original read deadline.
        let output = runtime
            .provider_exit_output
            .ready
            .as_ref()
            .filter(|ready| ready.scope.matches(runtime) && ready.output.current_for(&job))
            .map(|ready| ready.output.clone());
        if output.is_some() && copy.is_none() {
            job.cancel();
            return Task::none();
        }
        let request = runtime.provider_exit_output.begin(scope, job, copy);
        if let Some(output) = output {
            return self.prepare_provider_exit_copy(target, request, output);
        }
        cosmic::task::future(async move {
            let result = handle.read_provider_exit_job(request.job.clone()).await;
            action(Action::ProviderExitLoaded(target, request, result))
        })
    }
    fn prepare_provider_exit_copy(
        &mut self,
        target: Target,
        request: Request,
        output: DiagnosticOutput,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.provider_exit_output.owns(&request) {
            return Task::none();
        }
        if !request.scope.matches(runtime) {
            runtime.provider_exit_output.clear();
            return Task::none();
        }
        if request.job.cancelled() {
            runtime.provider_exit_output.take(&request);
            return Task::none();
        }
        let Some(summary) = runtime
            .snapshot
            .provider_exit
            .as_ref()
            .map(|diagnostic| diagnostic.summary.as_str())
        else {
            return Task::none();
        };
        let summary = match DiagnosticCopySummary::new(summary) {
            Ok(summary) => summary,
            Err(error) => {
                runtime.provider_exit_output.request = None;
                runtime.provider_exit_output.error = Some(error);
                return Task::none();
            }
        };
        let Some(loading) = runtime
            .provider_exit_output
            .request
            .as_mut()
            .filter(|loading| loading.request.job.same_job(&request.job))
        else {
            return Task::none();
        };
        loading.formatted_summary = Some(summary.clone());
        let handle = runtime.handle().unwrap().clone();
        cosmic::task::future(async move {
            let result = handle
                .copy_provider_exit_job(request.job.clone(), output, summary)
                .await;
            action(Action::ProviderExitCopyReady(target, request, result))
        })
    }
    pub(super) fn provider_exit_loaded(
        &mut self,
        target: Target,
        request: Request,
        result: DiagnosticCompletion,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            request.job.cancel();
            return Task::none();
        };
        // Clear the exact slot before cancellation/authority refusal. A stale
        // target may drop its completion, whose buffer reclaims off-thread.
        let Some(loading) = runtime.provider_exit_output.take(&request) else {
            return Task::none();
        };
        if !request.scope.matches(runtime) {
            runtime.provider_exit_output.clear();
            return Task::none();
        }
        if request.job.cancelled() {
            return Task::none();
        }
        let Some(result) = result.take_result() else {
            return Task::none();
        };
        match result {
            Ok(output) => {
                if !output.current_for(&request.job) {
                    runtime.provider_exit_output.clear();
                    return Task::none();
                }
                runtime.provider_exit_output.error = None;
                if runtime.provider_exit_expanded && runtime.visible.load(Ordering::Relaxed) {
                    runtime.provider_exit_output.ready = Some(Ready {
                        output: output.clone(),
                        scope: request.scope.clone(),
                    });
                }
                if loading.copy.as_ref().is_some_and(CopyInterest::current) {
                    runtime.provider_exit_output.request = Some(loading);
                    return self.prepare_provider_exit_copy(target, request, output);
                }
            }
            Err(error) => {
                runtime.provider_exit_output.error = Some(error.clone());
                if loading.copy.as_ref().is_some_and(CopyInterest::current) {
                    self.status = Some(TransientNotice::error(format!(
                        "Could not copy saved output: {error}"
                    )));
                }
            }
        }
        Task::none()
    }
    pub(super) fn provider_exit_copy_ready(
        &mut self,
        target: Target,
        request: Request,
        result: DiagnosticCopyCompletion,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            request.job.cancel();
            return Task::none();
        };
        let Some(loading) = runtime.provider_exit_output.take(&request) else {
            return Task::none();
        };
        if !request.scope.matches(runtime) {
            runtime.provider_exit_output.clear();
            return Task::none();
        }
        if request.job.cancelled() || !loading.copy.as_ref().is_some_and(CopyInterest::current) {
            return self.reload_provider_exit(target);
        }
        let Some(summary) = loading.formatted_summary.as_ref() else {
            return self.reload_provider_exit(target);
        };
        let current_summary = request.scope.proof.as_ref().is_some_and(|proof| {
            request.scope.handle.upgrade().is_some_and(|handle| {
                handle.diagnostic_snapshot_matches(
                    proof,
                    request.scope.key.session.as_deref(),
                    |snapshot| {
                        request.scope.key.matches(snapshot)
                            && snapshot
                                .provider_exit
                                .as_ref()
                                .is_some_and(|diagnostic| diagnostic.summary == summary.as_str())
                    },
                )
            })
        });
        if !current_summary {
            return self.reload_provider_exit(target);
        }
        let Some(result) = result.take_effect() else {
            return self.reload_provider_exit(target);
        };
        match result {
            Ok((text, receipt)) => {
                runtime.provider_exit_output.error = None;
                self.status = Some(TransientNotice::success("Copied saved provider output"));
                // The charged allocation stays owned until iced has emitted
                // its immediate clipboard effect. Dropping an undelivered Task
                // also drops this receipt; the platform clipboard is separate.
                let clipboard = iced::clipboard::write(text).map(cosmic::Action::App).chain(
                    cosmic::task::message(action(Action::ProviderExitClipboardIssued(receipt))),
                );
                // Show may have joined a hidden formatting job after its read
                // output moved into the worker. Restore disclosure explicitly;
                // an idle provider need not publish another Wake.
                Task::batch([clipboard, self.reload_provider_exit(target)])
            }
            Err(error) => {
                runtime.provider_exit_output.error = Some(error.clone());
                self.status = Some(TransientNotice::error(format!(
                    "Could not copy saved output: {error}"
                )));
                Task::none()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn isolated(name: &str) -> bool {
        crate::ui_test_process::isolated(
            &format!("app::chat_ui::provider_exit::tests::{name}"),
            "CAGENTS_TEST_DIAGNOSTIC_HANDLER_CASE",
        )
    }
    // App dispatch batches even a Task::none(), which has a real empty
    // stream. Assert the actual runtime effects, never the stream container.
    fn assert_no_actions(task: Task<Message>) {
        use iced::futures::{FutureExt as _, StreamExt as _};
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            assert!(stream.next().now_or_never().expect("no deferred work is permitted").is_none(),
                "the refused or completed handler must emit no runtime effect");
        }
    }
    fn assert_recent_copy_action(task: Task<Message>, expected: Option<&str>) {
        use iced::futures::{FutureExt as _, StreamExt as _};
        let Some(expected) = expected else {
            assert_no_actions(task);
            return;
        };
        let mut stream = iced::runtime::task::into_stream(task).expect("actual clipboard task");
        match stream.next().now_or_never().expect("recent copy is synchronous") {
            Some(iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write { contents, target })) => {
                assert_eq!(contents, expected);
                assert_eq!(target, iced::advanced::clipboard::Kind::Standard);
            }
            _ => panic!("current recent output must produce its actual clipboard write"),
        }
        assert!(stream.next().now_or_never().expect("recent copy has no deferred work").is_none());
    }
    fn wait_capacity(expected: usize) {
        let until = Instant::now() + Duration::from_secs(5);
        while chat::client::diagnostic_resource_usage().reserved_layout_capacity != expected
            && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            chat::client::diagnostic_resource_usage().reserved_layout_capacity,
            expected
        );
    }
    fn fixture(body: &[u8]) -> (Cagents, Target, Arc<ClientHandle>) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.new_native_chat(
            Provider::Codex,
            Some(PathBuf::from("/workspace/chat-fixture")),
        ));
        let card = app.workspace.focused().unwrap();
        let runtime = match app.cards.get_mut(&card).unwrap() {
            CardRuntime::Chat(runtime) => runtime,
            _ => panic!("draft chat"),
        };
        let target = runtime.target;
        let snapshot = Snapshot {
            ready: true,
            session_id: Some("confirmed-diagnostic-session".into()),
            turn_generation: 7,
            provider_exit: Some(chat::ProviderExitDiagnostic {
                summary: "Provider exited with status 3".into(),
                output: "recent tail".into(),
                log: Some(chat::ProviderExitLog {
                    id: "saved-exit".into(),
                    bytes: body.len() as u64,
                    omitted: 17,
                }),
            }),
            ..Snapshot::default()
        };
        let handle = Arc::new(ClientHandle::fixture(runtime.session.clone(), snapshot));
        runtime.connection = Connection::Live(handle.clone());
        runtime.snapshot = handle.snapshot();
        runtime.disconnected = false;
        runtime.visible.store(true, Ordering::Relaxed);
        runtime.provider_exit_expanded = true;
        runtime
            .provider_exit_output
            .reconcile(&runtime.snapshot, Some(Arc::downgrade(&handle)));
        (app, target, handle)
    }
    fn held_read(
        app: &mut Cagents,
        target: Target,
        handle: &Arc<ClientHandle>,
        body: &[u8],
        copy: bool,
    ) -> (Request, DiagnosticCompletion) {
        let intent = copy.then(|| app.chat_ui.copy_intents.next());
        let runtime = app.chat_runtime_mut(target).unwrap();
        let key = Key::from_snapshot(&runtime.snapshot).unwrap();
        let scope = Scope::capture(handle, key.clone());
        assert!(
            scope.current(),
            "real receiver is current before fixture read"
        );
        let (job, completion) = handle
            .fixture_diagnostic_completion(
                target.card,
                target.generation,
                key.turn,
                key.log,
                body,
                scope.authority(),
            )
            .unwrap();
        assert!(
            completion.contains_text_for_test(std::str::from_utf8(body).unwrap()),
            "positively successful charged read before any cancellation"
        );
        let request = runtime.provider_exit_output.begin(scope, job, intent);
        (request, completion)
    }
    fn install(app: &mut Cagents, target: Target, handle: &Arc<ClientHandle>, body: &[u8]) {
        let (request, completion) = held_read(app, target, handle, body, false);
        let task = app.handle_chat_ui(Action::ProviderExitLoaded(target, request, completion));
        assert_no_actions(task);
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(
            runtime
                .provider_exit_output
                .ready
                .as_ref()
                .unwrap()
                .output
                .as_str(),
            std::str::from_utf8(body).unwrap()
        );
    }
    async fn prepared_copy(task: Task<Message>) -> Action {
        use iced::futures::StreamExt as _;
        let mut stream = iced::runtime::task::into_stream(task).expect("actual worker copy task");
        loop {
            match stream
                .next()
                .await
                .expect("actual diagnostic copy completion")
            {
                iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                    action @ Action::ProviderExitCopyReady(..),
                ))) => return action,
                _ => {}
            }
        }
    }
    #[test]
    fn diagnostic_scope_bounds_and_shares_compact_native_session_metadata() {
        let mut session = String::with_capacity(1024 * 1024);
        session.push_str("short-session");
        let mut snapshot = Snapshot {
            session_id: Some(session),
            provider_exit: Some(chat::ProviderExitDiagnostic {
                summary: "failure".into(),
                output: String::new(),
                log: Some(chat::ProviderExitLog {
                    id: "log".into(),
                    bytes: 1,
                    omitted: 0,
                }),
            }),
            ..Snapshot::default()
        };
        let key = Key::from_snapshot(&snapshot).unwrap();
        let cloned = key.clone();
        assert_eq!(key.session.as_deref(), Some("short-session"));
        assert!(Arc::ptr_eq(
            key.session.as_ref().unwrap(),
            cloned.session.as_ref().unwrap()
        ));
        snapshot.session_id = Some("x".repeat(SESSION_BYTES));
        assert!(Key::from_snapshot(&snapshot).is_some());
        snapshot.session_id = Some("x".repeat(SESSION_BYTES + 1));
        assert!(Key::from_snapshot(&snapshot).is_none());
    }
    #[test]
    fn diagnostic_current_tuple_refuses_wrong_host_proof_and_replacement_attachment() {
        if isolated("diagnostic_current_tuple_refuses_wrong_host_proof_and_replacement_attachment")
        {
            return;
        }
        let (mut app, target, handle) = fixture(b"saved diagnostic");
        let runtime = app.chat_runtime_mut(target).unwrap();
        let scope = Scope::capture(&handle, Key::from_snapshot(&runtime.snapshot).unwrap());
        assert!(scope.current() && scope.matches(runtime));
        let mut wrong = handle.diagnostic_proof().unwrap();
        wrong.host_incarnation = crate::model::HostIncarnation::new("another-host-incarnation");
        let called = std::cell::Cell::new(false);
        assert!(
            !handle.diagnostic_snapshot_matches(&wrong, scope.key.session.as_deref(), |_| {
                called.set(true);
                true
            })
        );
        assert!(
            !called.get(),
            "a mismatched authenticated proof cannot inspect the receiver snapshot"
        );
        let replacement = Arc::new(ClientHandle::fixture(
            runtime.session.clone(),
            (*runtime.snapshot).clone(),
        ));
        assert_eq!(
            replacement.diagnostic_proof(),
            handle.diagnostic_proof(),
            "same metadata alone is insufficient"
        );
        runtime.connection = Connection::Live(replacement);
        assert!(
            !scope.matches(runtime),
            "an exact native/turn/log tuple on a replacement Runtime attachment is refused"
        );
    }

    #[test]
    fn recent_copy_requires_unchanged_current_summary_and_tail_without_wake() {
        if isolated("recent_copy_requires_unchanged_current_summary_and_tail_without_wake") {
            return;
        }
        for saved in [false, true] {
            for mutation in 0..3 {
                let (mut app, target, handle) = fixture(b"saved diagnostic");
                if !saved {
                    let mut snapshot = (*handle.snapshot()).clone();
                    snapshot.provider_exit.as_mut().unwrap().log = None;
                    handle.publish_snapshot_for_test(snapshot);
                    app.chat_runtime_mut(target).unwrap().snapshot = handle.snapshot();
                }
                let old = app.chat_runtime_mut(target).unwrap().snapshot.clone();
                if mutation != 0 {
                    let mut current = (*handle.snapshot()).clone();
                    current.generation += 1;
                    let diagnostic = current.provider_exit.as_mut().unwrap();
                    if mutation == 1 {
                        diagnostic.summary = "new same-turn cause".into();
                    } else {
                        diagnostic.output = "new same-turn recent tail".into();
                    }
                    handle.publish_snapshot_for_test(current);
                }
                assert!(Arc::ptr_eq(
                    &old,
                    &app.chat_runtime_mut(target).unwrap().snapshot
                ));
                app.status = None;
                let task = app.handle_chat_ui(Action::CopyProviderExitRecent(target));
                assert_recent_copy_action(task,
                    (mutation == 0).then_some("Provider exited with status 3\n\nrecent tail"));
                assert_eq!(app.status.is_some(), mutation == 0);
            }
        }
    }
    #[test]
    fn hidden_copy_then_show_schedules_current_full_disclosure_without_wake() {
        if isolated("hidden_copy_then_show_schedules_current_full_disclosure_without_wake") {
            return;
        }
        let _guard = chat::client::diagnostic_test_guard();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for supersede in [false, true] {
                let body = b"complete hidden saved output";
                let (mut app, target, handle) = fixture(body);
                app.chat_runtime_mut(target).unwrap().provider_exit_expanded = false;
                let (request, completion) = held_read(&mut app, target, &handle, body, true);
                let old_id = request.id;
                let task =
                    app.handle_chat_ui(Action::ProviderExitLoaded(target, request, completion));
                let completed = prepared_copy(task).await;
                let Action::ProviderExitCopyReady(_, _, reply) = &completed else {
                    unreachable!()
                };
                assert!(reply.contains_text_for_test("complete hidden saved output"));
                assert!(
                    app.chat_runtime_mut(target)
                        .unwrap()
                        .provider_exit_output
                        .ready
                        .is_none()
                );
                let task = app.handle_chat_ui(Action::ToggleProviderExit(target));
                assert_no_actions(task);
                if supersede {
                    app.chat_ui.copy_intents.next();
                }
                app.status = None;
                let task = app.handle_chat_ui(completed);
                assert!(
                    iced::runtime::task::into_stream(task).is_some(),
                    "completion must explicitly schedule full saved disclosure without Wake"
                );
                let runtime = app.chat_runtime_mut(target).unwrap();
                let reload = runtime
                    .provider_exit_output
                    .request
                    .as_ref()
                    .expect("actual new qualified read scheduled");
                assert_ne!(reload.request.id, old_id);
                assert!(!reload.request.job.cancelled());
                assert!(
                    runtime.provider_exit_expanded && runtime.provider_exit_output.ready.is_none()
                );
                if supersede {
                    assert!(app.status.is_none());
                }
                drop(app);
                drop(handle);
                wait_capacity(0);
            }
        });
    }
    #[test]
    fn actual_saved_copy_native_clipboard_intent_rejects_successful_late_ready() {
        if isolated("actual_saved_copy_native_clipboard_intent_rejects_successful_late_ready") {
            return;
        }
        let _guard = chat::client::diagnostic_test_guard();
        let renderer = crate::app::tests::headless_renderer();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let body = b"complete saved diagnostic";
            let (mut app, target, handle) = fixture(body);
            install(&mut app, target, &handle, body);
            let task = app.handle_chat_ui(Action::CopyProviderExit(target));
            let completed = prepared_copy(task).await;
            let Action::ProviderExitCopyReady(_, request, reply) = &completed else {
                unreachable!()
            };
            assert!(request.current(app.chat_runtime_mut(target).unwrap()));
            assert!(reply.contains_text_for_test("complete saved diagnostic"));
            let selected = "new native clipboard text";
            assert_eq!(
                clipboard_surface::native_copy_for_test(
                    &renderer,
                    app.chat_ui.copy_intents.clone(),
                    selected
                ),
                selected
            );
            app.status = None;
            let task = app.handle_chat_ui(completed);
            assert_no_actions(task);
            assert!(app.status.is_none());
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert!(
                runtime.provider_exit_output.request.is_none()
                    && runtime.provider_exit_output.error.is_none()
            );
            assert!(
                runtime.provider_exit_output.ready.is_some(),
                "superseded copy preserves visible disclosure"
            );
        });
    }
    #[test]
    fn actual_held_read_and_copy_refuse_new_receiver_authority_before_wake() {
        if isolated("actual_held_read_and_copy_refuse_new_receiver_authority_before_wake") {
            return;
        }
        let _guard = chat::client::diagnostic_test_guard();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for copy in [false, true] {
                for mutation in 0..4 {
                    let body = b"faithful old saved output";
                    let (mut app, target, handle) = fixture(body);
                    let completed = if copy {
                        install(&mut app, target, &handle, body);
                        let task = app.handle_chat_ui(Action::CopyProviderExit(target));
                        let completed = prepared_copy(task).await;
                        let Action::ProviderExitCopyReady(_, _, reply) = &completed else {
                            unreachable!()
                        };
                        assert!(reply.contains_text_for_test("faithful old saved output"));
                        completed
                    } else {
                        let (request, reply) = held_read(&mut app, target, &handle, body, true);
                        Action::ProviderExitLoaded(target, request, reply)
                    };
                    let old = app.chat_runtime_mut(target).unwrap().snapshot.clone();
                    let mut current = (*handle.snapshot()).clone();
                    current.generation += 1;
                    match mutation {
                        0 => current.session_id = Some("replacement-native-session".into()),
                        1 => current.provider_exit = None,
                        2 => {
                            current
                                .provider_exit
                                .as_mut()
                                .unwrap()
                                .log
                                .as_mut()
                                .unwrap()
                                .id = "replacement-log".into()
                        }
                        _ => current.turn_generation += 1,
                    }
                    handle.publish_snapshot_for_test(current);
                    assert!(
                        Arc::ptr_eq(&old, &app.chat_runtime_mut(target).unwrap().snapshot),
                        "do not process receiver Wake"
                    );
                    app.status = None;
                    let task = app.handle_chat_ui(completed);
                    assert_no_actions(task);
                    assert!(app.status.is_none());
                    let state = &app.chat_runtime_mut(target).unwrap().provider_exit_output;
                    assert!(
                        state.request.is_none() && state.ready.is_none(),
                        "authority refusal withdraws old presentation and pending slot"
                    );
                    drop(app);
                    drop(handle);
                    wait_capacity(0);
                }
            }
        });
    }
    #[test]
    fn canceled_hidden_copy_slot_is_cleared_and_same_source_can_retry() {
        if isolated("canceled_hidden_copy_slot_is_cleared_and_same_source_can_retry") {
            return;
        }
        let _guard = chat::client::diagnostic_test_guard();
        let body = b"complete diagnostic";
        let (mut app, target, handle) = fixture(body);
        let (request, completion) = held_read(&mut app, target, &handle, body, true);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.visible.store(false, Ordering::Relaxed);
        runtime.provider_exit_output.hide();
        assert!(
            !request.job.cancelled(),
            "explicit current copy survives ordinary hide"
        );
        app.chat_ui.copy_intents.next();
        assert!(
            request.job.cancelled(),
            "new clipboard intent withdraws hidden copy-only read"
        );
        let task = app.handle_chat_ui(Action::ProviderExitLoaded(
            target,
            request.clone(),
            completion,
        ));
        assert_no_actions(task);
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .provider_exit_output
                .request
                .is_none()
        );
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.visible.store(true, Ordering::Relaxed);
        let (new, _completion) = held_read(&mut app, target, &handle, body, false);
        assert_ne!(new.id, request.id);
        assert!(!new.job.cancelled());
    }
    #[test]
    fn old_runtime_completion_preserves_replacement_request_with_same_local_id() {
        if isolated("old_runtime_completion_preserves_replacement_request_with_same_local_id") {
            return;
        }
        let _guard = chat::client::diagnostic_test_guard();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for copy in [false, true] {
                let body = b"complete retained diagnostic";
                let (mut app, target, old_handle) = fixture(body);
                let (old, completion) = held_read(&mut app, target, &old_handle, body, copy);
                let old_action = if copy {
                    let task = app.handle_chat_ui(Action::ProviderExitLoaded(
                        target, old.clone(), completion,
                    ));
                    prepared_copy(task).await
                } else {
                    Action::ProviderExitLoaded(target, old.clone(), completion)
                };
                let runtime = app.chat_runtime_mut(target).unwrap();
                let replacement = Arc::new(ClientHandle::fixture(
                    runtime.session.clone(), (*runtime.snapshot).clone(),
                ));
                runtime.connection = Connection::Live(replacement.clone());
                runtime.snapshot = replacement.snapshot();
                runtime.provider_exit_output = State::default();
                runtime.provider_exit_output.reconcile(
                    &runtime.snapshot, Some(Arc::downgrade(&replacement)),
                );
                let (new, current_completion) = held_read(
                    &mut app, target, &replacement, body, false,
                );
                assert_eq!(old.id, new.id, "replacement restarts the local counter");
                assert!(!old.job.same_job(&new.job));
                let task = app.handle_chat_ui(old_action);
                assert_no_actions(task);
                let runtime = app.chat_runtime_mut(target).unwrap();
                assert!(runtime.provider_exit_output.owns(&new));
                assert!(runtime.provider_exit_expanded);
                assert!(runtime.provider_exit_output.error.is_none());
                assert!(!new.job.cancelled());
                let task = app.handle_chat_ui(Action::ProviderExitLoaded(
                    target, new, current_completion,
                ));
                assert_no_actions(task);
                assert_eq!(app.chat_runtime_mut(target).unwrap()
                    .provider_exit_output.ready.as_ref().unwrap().output.as_str(),
                    std::str::from_utf8(body).unwrap());
                drop(app);
                drop(old);
                wait_capacity(0);
            }
        });
    }
    #[test]
    fn actual_copy_emits_clipboard_before_its_capacity_receipt_acknowledgement() {
        if isolated("actual_copy_emits_clipboard_before_its_capacity_receipt_acknowledgement") {
            return;
        }
        let _guard = chat::client::diagnostic_test_guard();
        use iced::futures::StreamExt as _;
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let body=b"saved full output";let(mut app,target,handle)=fixture(body);install(&mut app,target,&handle,body);
            let resident=chat::client::diagnostic_resource_usage().reserved_layout_capacity;
            let task=app.handle_chat_ui(Action::CopyProviderExit(target));let completed=prepared_copy(task).await;
            let formatted=chat::client::diagnostic_resource_usage().reserved_layout_capacity;
            assert!(formatted>resident,"actual prepared copy holds a separate allocation charge");
            let Action::ProviderExitCopyReady(_,_,reply)=&completed else {unreachable!()};assert!(reply.contains_text_for_test("saved full output"));
            let task=app.handle_chat_ui(completed);let mut effects=iced::runtime::task::into_stream(task).unwrap();
            match effects.next().await.unwrap() {
                iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write {contents,..})=>assert_eq!(contents,"Provider exited with status 3\n\nsaved full output\n\n[17 raw output bytes were not retained]"),
                _=>panic!("clipboard effect must precede receipt acknowledgement")
            }
            assert_eq!(chat::client::diagnostic_resource_usage().reserved_layout_capacity,formatted,"clipboard emission alone keeps queued receipt charged");
            let ack=match effects.next().await.unwrap() {
                iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(action @ Action::ProviderExitClipboardIssued(..))))=>action,
                _=>panic!("charged receipt must stay in queued acknowledgement")
            };
            drop(effects);
            assert_eq!(chat::client::diagnostic_resource_usage().reserved_layout_capacity,formatted,"held actual acknowledgement retains formatted owner");
            let task=app.handle_chat_ui(ack);assert_no_actions(task);
            wait_capacity(resident);
            app.chat_runtime_mut(target).unwrap().provider_exit_output.clear();wait_capacity(0);
        });
    }
}
