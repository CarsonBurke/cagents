//! Dictation owns a measured insertion bookmark in one runtime generation.
//! Async work may produce editable text; this module has no submit/agent command
//! dispatch or native focus operation. Existing draft edits stay authoritative.
use super::{Target, attachment_draft};
use crate::{editor_history::TextChange, process::CancellationToken, speech};
use cosmic::widget::text_editor::{self, Content, Cursor};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

type Ticket = speech::Ticket<Target>;
const MAX_TEXT_BYTES: usize = 32 * 1024;

/// The chat Action only needs one nested variant carrying this type. Text is
/// held in a take-once delivery, so cloning application messages cannot apply a
/// completed dictation twice or print its text in diagnostic Debug output.
#[derive(Clone)]
pub(super) enum Action {
    Start,
    StopRecording,
    Cancel,
    InsertRetained,
    DiscardRetained,
    Install,
    CancelSetup,
    DismissSetup,
    SetupProgress(SetupTicket, speech::setup::Progress),
    SetupFinished(SetupDelivery),
    Progress(Ticket, speech::Phase, Instant),
    Finished(Delivery),
}
impl Action {
    pub fn asynchronous(&self) -> bool {
        matches!(
            self,
            Self::Progress(..)
                | Self::Finished(..)
                | Self::SetupProgress(..)
                | Self::SetupFinished(..)
        )
    }
}
#[derive(Clone)]
pub(super) struct Delivery {
    ticket: Ticket,
    result: Arc<Mutex<Option<Result<speech::Transcript<Target>, speech::Error>>>>,
}
impl Delivery {
    pub fn new(ticket: Ticket, result: Result<speech::Transcript<Target>, speech::Error>) -> Self {
        Self {
            ticket,
            result: Arc::new(Mutex::new(Some(result))),
        }
    }
    pub fn target(&self) -> Target {
        self.ticket.target
    }
}
impl std::fmt::Debug for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DictationDelivery")
            .field("operation", &self.ticket.operation())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HoldReason {
    OriginalSelectionChanged,
    CurrentSelectionChanged,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartError {
    Active,
    RetainedText,
    Speech(speech::Error),
}

struct Tracking {
    ticket: Ticket,
    bookmark: attachment_draft::Bookmark,
}
impl Tracking {
    fn new(target: Target, revision: u64, content: &Content) -> Self {
        Self {
            ticket: Ticket::new(target, revision, Uuid::new_v4()),
            bookmark: attachment_draft::Bookmark::capture(content),
        }
    }
    fn accepts(&self, current: Target, ticket: &Ticket) -> bool {
        current == ticket.target && self.ticket.belongs_to(ticket)
    }
}
struct Active {
    tracking: Tracking,
    // Always Some in production. Pure bookmark regressions construct the same
    // state without starting a microphone or manufacturing verified ASR assets.
    control: Option<speech::Control>,
    phase: speech::Phase,
    stopping: bool,
    recording_since: Option<Instant>,
}
struct Retained {
    ticket: Ticket,
    text: String,
    reason: HoldReason,
}

#[derive(Default)]
pub(super) struct State {
    active: Option<Active>,
    retained: Option<Retained>,
    error: Option<speech::Error>,
}
pub(super) struct Started {
    pub ticket: Ticket,
    pub session: speech::Session<Target>,
}
/// The caller must fan the exact measured edit to existing image bookmarks,
/// promote history to a draft and restart completion, as for an ordinary paste.
pub(super) enum Applied {
    Measured(TextChange),
    /// Native paste replaced identical text. No image/history edit is required.
    Unchanged,
    Unmeasured,
}
pub(super) enum Finished {
    Ignored,
    Settled,
    Retained,
    Applied(Applied),
}
impl State {
    pub fn begin(
        &mut self,
        target: Target,
        revision: u64,
        content: &Content,
        assets: &speech::Prepared,
    ) -> Result<Started, StartError> {
        if self.active.is_some() {
            return Err(StartError::Active);
        }
        if self.retained.is_some() {
            return Err(StartError::RetainedText);
        }
        let tracking = Tracking::new(target, revision, content);
        let (session, control) = assets
            .start(tracking.ticket.clone())
            .map_err(StartError::Speech)?;
        let ticket = tracking.ticket.clone();
        self.error = None;
        self.active = Some(Active {
            tracking,
            control: Some(control),
            phase: speech::Phase::Preparing,
            stopping: false,
            recording_since: None,
        });
        Ok(Started { ticket, session })
    }
    #[cfg(test)]
    pub(super) fn capture_for_test(
        &mut self,
        target: Target,
        revision: u64,
        content: &Content,
    ) -> Ticket {
        let tracking = Tracking::new(target, revision, content);
        let ticket = tracking.ticket.clone();
        self.active = Some(Active {
            tracking,
            control: None,
            phase: speech::Phase::Preparing,
            stopping: false,
            recording_since: None,
        });
        ticket
    }
    pub fn has_bookmark(&self) -> bool {
        self.active.is_some()
    }
    pub fn changed(&mut self, change: &TextChange) {
        if let Some(active) = &mut self.active {
            active.tracking.bookmark.changed(change);
        }
    }
    pub fn unmeasured_edit(&mut self) {
        if let Some(active) = &mut self.active {
            active.tracking.bookmark.valid = false;
        }
    }
    pub fn phase(&self) -> Option<speech::Phase> {
        self.active.as_ref().map(|active| active.phase)
    }
    pub fn stopping(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.stopping)
    }
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        self.active
            .as_ref()?
            .recording_since
            .map(|start| now.saturating_duration_since(start))
    }
    pub fn error(&self) -> Option<speech::Error> {
        self.error
    }
    pub fn retained_reason(&self) -> Option<HoldReason> {
        self.retained.as_ref().map(|value| value.reason)
    }
    pub fn progress(
        &mut self,
        current: Target,
        ticket: &Ticket,
        phase: speech::Phase,
        now: Instant,
    ) -> bool {
        let Some(active) = &mut self.active else {
            return false;
        };
        if !active.tracking.accepts(current, ticket) {
            return false;
        }
        let rank = |phase| match phase {
            speech::Phase::Preparing => 0,
            speech::Phase::Recording => 1,
            speech::Phase::Transcribing => 2,
        };
        if rank(phase) < rank(active.phase) {
            return false;
        }
        if phase == speech::Phase::Recording && active.recording_since.is_none() {
            active.recording_since = Some(now);
        }
        active.phase = phase;
        true
    }
    pub fn stop(&mut self, current: Target) -> bool {
        let Some(active) = &mut self.active else {
            return false;
        };
        if active.tracking.ticket.target != current
            || active.phase != speech::Phase::Recording
            || active.stopping
        {
            return false;
        }
        if let Some(control) = &active.control {
            control.stop_recording();
        }
        active.stopping = true;
        true
    }
    pub fn cancel(&mut self, current: Target) -> bool {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.tracking.ticket.target == current)
        {
            self.active.take(); // Dropping the unique control pushes cancellation.
            self.error = None;
            return true;
        }
        false
    }
    /// Draft replacement/send or target retirement cancels the original owner.
    /// It cannot silently carry a fallback transcript into the replacement draft.
    pub fn retire(&mut self) {
        self.active.take();
        self.retained.take();
        self.error = None;
    }

    pub fn complete(
        &mut self,
        current: Target,
        delivery: &Delivery,
        content: &mut Content,
    ) -> Finished {
        if !self
            .active
            .as_ref()
            .is_some_and(|active| active.tracking.accepts(current, &delivery.ticket))
        {
            return Finished::Ignored;
        }
        let Some(result) = delivery
            .result
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        else {
            return Finished::Ignored;
        };
        // A misaddressed success must not retire the current live capture.
        if let Ok(transcript) = &result
            && !transcript.ticket.belongs_to(&delivery.ticket)
        {
            return Finished::Ignored;
        }
        let active = self.active.take().expect("matching dictation owner");
        let text = match result {
            Err(speech::Error::Cancelled) => {
                self.error = None;
                return Finished::Settled;
            }
            Err(error) => {
                self.error = Some(error);
                return Finished::Settled;
            }
            Ok(transcript) => transcript.text,
        };
        if text.len() > MAX_TEXT_BYTES || text.contains('\0') || text.trim().is_empty() {
            self.error = Some(if text.trim().is_empty() {
                speech::Error::NoSpeech
            } else {
                speech::Error::InvalidResult
            });
            return Finished::Settled;
        }
        self.error = None;
        match insert(content, active.tracking.bookmark, &text) {
            Ok(applied) => Finished::Applied(applied),
            Err(reason) => {
                self.retained = Some(Retained {
                    ticket: delivery.ticket.clone(),
                    text,
                    reason,
                });
                Finished::Retained
            }
        }
    }
    /// Explicit user action only; async completion never chooses a new caret.
    pub fn insert_retained(&mut self, current: Target, content: &mut Content) -> Finished {
        let Some(retained) = &self.retained else {
            return Finished::Ignored;
        };
        if retained.ticket.target != current {
            return Finished::Ignored;
        }
        let bookmark = attachment_draft::Bookmark::capture(content);
        if !bookmark.valid {
            return Finished::Retained;
        }
        match insert(content, bookmark, &retained.text) {
            Ok(applied) => {
                self.retained.take();
                self.error = None;
                Finished::Applied(applied)
            }
            Err(_) => Finished::Retained,
        }
    }
    pub fn discard_retained(&mut self, current: Target) -> bool {
        if self
            .retained
            .as_ref()
            .is_some_and(|retained| retained.ticket.target == current)
        {
            self.retained.take();
            true
        } else {
            false
        }
    }
}

fn point(position: text_editor::Position) -> (usize, usize) {
    (position.line, position.column)
}
fn span(cursor: Cursor) -> ((usize, usize), (usize, usize)) {
    let caret = point(cursor.position);
    let anchor = point(cursor.selection.unwrap_or(cursor.position));
    (caret.min(anchor), caret.max(anchor))
}
fn conflicts_with_current(original: Cursor, current: Cursor) -> bool {
    if original == current {
        return false;
    }
    let (start, end) = span(original);
    let (a, b) = span(current);
    // Do not replace text underneath a newly moved selection/caret when its
    // native position could not be preserved through that replacement.
    (a < b && ((start < b && end > a) || (start >= a && start < b)))
        || (start < point(current.position) && point(current.position) < end)
}
fn insert(
    content: &mut Content,
    bookmark: attachment_draft::Bookmark,
    text: &str,
) -> Result<Applied, HoldReason> {
    if !bookmark.valid {
        return Err(HoldReason::OriginalSelectionChanged);
    }
    let current = attachment_draft::Bookmark::capture(content);
    if !current.valid || conflicts_with_current(bookmark.cursor, current.cursor) {
        return Err(HoldReason::CurrentSelectionChanged);
    }
    attachment_draft::move_to(content, bookmark.cursor);
    let effect =
        attachment_draft::edit(content, text_editor::Edit::Paste(Arc::new(text.to_owned())));
    match effect {
        Ok(Some(change)) => {
            if current.cursor != bookmark.cursor
                && let Some(mapped) = attachment_draft::mapped(current.cursor, &change)
            {
                attachment_draft::move_to(content, mapped);
            }
            Ok(Applied::Measured(change))
        }
        Ok(None) => {
            // Replacing a selection with identical text still moves the native
            // caret. Its text delta is empty, so preserve a separately moved
            // selection verbatim and leave unrelated image anchors untouched.
            if current.cursor != bookmark.cursor {
                attachment_draft::move_to(content, current.cursor);
            }
            Ok(Applied::Unchanged)
        }
        // The native edit already ran. Never show a second insertion button
        // for text which is now in the draft; invalidate other pending anchors.
        Err(()) => Ok(Applied::Unmeasured),
    }
}

/// App-wide prepared assets are separate from per-draft recording ownership.
/// No boolean or editable install receipt can manufacture a Ready microphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SetupTicket(Uuid);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SetupPhase {
    CheckingSaved,
    Downloading { bytes: u64 },
    Verifying,
    Extracting,
}
struct SetupJob {
    ticket: SetupTicket,
    phase: SetupPhase,
    cancel: CancellationToken,
}
#[derive(Default)]
pub(super) struct Setup {
    owner: Option<Target>,
    active: Option<SetupJob>,
    prepared: Option<Arc<speech::Prepared>>,
    error: Option<speech::Error>,
}
impl Setup {
    pub fn show(&mut self, target: Target) {
        self.owner = Some(target);
    }
    pub fn shown(&self, target: Target) -> bool {
        self.owner == Some(target)
    }
    pub fn dismiss(&mut self, target: Target) {
        if self.shown(target) && self.active.is_none() {
            self.owner = None;
        }
    }
    pub fn begin(&mut self) -> Result<(SetupTicket, CancellationToken), speech::Error> {
        if self.active.is_some() {
            return Err(speech::Error::Busy);
        }
        let ticket = SetupTicket(Uuid::new_v4());
        let cancel = CancellationToken::new();
        self.active = Some(SetupJob {
            ticket,
            phase: SetupPhase::CheckingSaved,
            cancel: cancel.clone(),
        });
        self.error = None;
        Ok((ticket, cancel))
    }
    pub fn progress(&mut self, ticket: SetupTicket, phase: SetupPhase) -> bool {
        let Some(job) = &mut self.active else {
            return false;
        };
        if job.ticket != ticket || job.cancel.is_cancelled() {
            return false;
        }
        let rank = |phase| match phase {
            SetupPhase::CheckingSaved => 0,
            SetupPhase::Downloading { .. } => 1,
            SetupPhase::Verifying => 2,
            SetupPhase::Extracting => 3,
        };
        if rank(phase) < rank(job.phase) {
            return false;
        }
        if let SetupPhase::Downloading { bytes } = phase {
            let previous = match job.phase {
                SetupPhase::Downloading { bytes } => bytes,
                _ => 0,
            };
            if bytes < previous || bytes > speech::setup::DOWNLOAD_BYTES {
                return false;
            }
        }
        job.phase = phase;
        true
    }
    pub fn complete(
        &mut self,
        ticket: SetupTicket,
        result: Result<Arc<speech::Prepared>, speech::Error>,
    ) -> bool {
        if !self
            .active
            .as_ref()
            .is_some_and(|job| job.ticket == ticket && !job.cancel.is_cancelled())
        {
            return false;
        }
        self.active.take();
        match result {
            Ok(prepared) => {
                self.prepared = Some(prepared);
                self.error = None;
            }
            Err(speech::Error::Cancelled) => self.error = None,
            Err(error) => self.error = Some(error),
        }
        true
    }
    pub fn cancel(&mut self) -> bool {
        if let Some(job) = self.active.take() {
            job.cancel.cancel();
            self.error = None;
            true
        } else {
            false
        }
    }
    pub fn prepared(&self) -> Option<&speech::Prepared> {
        self.prepared.as_deref()
    }
    pub fn phase(&self) -> Option<SetupPhase> {
        self.active.as_ref().map(|job| job.phase)
    }
    pub fn error(&self) -> Option<speech::Error> {
        self.error
    }
}
impl Drop for Setup {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone)]
pub(super) struct SetupDelivery {
    ticket: SetupTicket,
    result: Arc<Mutex<Option<Result<Arc<speech::Prepared>, speech::Error>>>>,
}
impl SetupDelivery {
    fn new(ticket: SetupTicket, result: Result<speech::Prepared, speech::Error>) -> Self {
        Self {
            ticket,
            result: Arc::new(Mutex::new(Some(result.map(Arc::new)))),
        }
    }
}
impl Setup {
    fn delivered(&mut self, delivery: SetupDelivery) {
        if !self
            .active
            .as_ref()
            .is_some_and(|job| job.ticket == delivery.ticket && !job.cancel.is_cancelled())
        {
            return;
        }
        if let Some(result) = delivery
            .result
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            self.complete(delivery.ticket, result);
        }
    }
}
impl super::Runtime {
    fn dictation_applied(&mut self, finished: Finished) {
        let Finished::Applied(applied) = finished else {
            return;
        };
        match applied {
            Applied::Unchanged => return,
            Applied::Measured(change) => {
                for bookmark in self.attachment_bookmarks.values_mut() {
                    bookmark.changed(&change);
                }
            }
            Applied::Unmeasured => {
                for bookmark in self.attachment_bookmarks.values_mut() {
                    bookmark.valid = false;
                }
            }
        }
        self.cancel_invalid_attachment_jobs();
        self.revision = self.revision.wrapping_add(1);
        self.completion.restart(true);
        self.history_cursor = None;
        self.history_stash = None;
        self.history_attachment_stash = None;
        self.dismiss_suggestion();
    }
}
impl super::Runtime {
    pub(super) fn dictation_controls<'a>(
        &'a self,
        available: bool,
        setup: Option<&'a Setup>,
    ) -> Vec<super::Element<'a, super::Message>> {
        use super::{Action as ChatAction, Icon, action};
        let target = self.target;
        let button = |icon: Icon,
                      label: &'static str,
                      action: Action,
                      enabled: bool|
         -> super::Element<'a, super::Message> {
            crate::ui_controls::icon_button(icon, label)
                .compact()
                .on_press_maybe(
                    enabled.then_some(super::action(ChatAction::Dictation(target, action))),
                )
                .into()
        };
        match self.dictation.phase() {
            Some(speech::Phase::Recording) => vec![
                button(
                    Icon::Stop,
                    "Finish dictation recording and transcribe",
                    Action::StopRecording,
                    !self.dictation.stopping(),
                ),
                button(
                    Icon::Cancel,
                    "Cancel dictation and discard recording (Esc)",
                    Action::Cancel,
                    true,
                ),
            ],
            Some(speech::Phase::Preparing | speech::Phase::Transcribing) => vec![
                crate::ui_controls::activity_spinner(
                    super::design::COMPACT_ICON_SIZE as f32,
                    super::design::muted_icon_color,
                )
                .into(),
                button(Icon::Cancel, "Cancel dictation (Esc)", Action::Cancel, true),
            ],
            None => vec![
                crate::ui_controls::icon_button(
                    Icon::Microphone,
                    if setup.is_some_and(|setup| setup.prepared().is_some()) {
                        "Start local dictation"
                    } else {
                        "Set up local dictation"
                    },
                )
                .compact()
                .id(super::card_control_id(
                    target.card,
                    "chat-dictation-microphone",
                ))
                .on_press_maybe(
                    (available && self.dictation.retained_reason().is_none())
                        .then(|| action(ChatAction::Dictation(target, Action::Start))),
                )
                .into(),
            ],
        }
    }
    pub(super) fn dictation_notice<'a>(
        &'a self,
        available: bool,
        setup: Option<&'a Setup>,
    ) -> super::Element<'a, super::Message> {
        use super::{Action as ChatAction, Length, action, design, widget};
        let target = self.target;
        let button = |label: &'static str,
                      operation: Action,
                      enabled: bool|
         -> super::Element<'a, super::Message> {
            crate::ui_controls::button::secondary(label)
                .on_press_maybe(enabled.then_some(action(ChatAction::Dictation(target, operation))))
                .into()
        };
        let mut rows: Vec<super::Element<'a, super::Message>> = Vec::new();
        if let Some(phase) = self.dictation.phase() {
            let label = match phase {
                speech::Phase::Preparing => "Starting microphone…".to_owned(),
                speech::Phase::Recording if self.dictation.stopping() => {
                    "Finishing recording…".to_owned()
                }
                speech::Phase::Recording => {
                    let seconds = self
                        .dictation
                        .elapsed(Instant::now())
                        .unwrap_or_default()
                        .as_secs();
                    format!(
                        "Recording {}:{:02} · 60 second limit",
                        seconds / 60,
                        seconds % 60
                    )
                }
                speech::Phase::Transcribing => "Transcribing locally…".to_owned(),
            };
            rows.push(design::caption(label).into());
        }
        if let Some(error) = self.dictation.error() {
            rows.push(
                design::caption(error.to_string())
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .into(),
            );
        }
        if self.dictation.retained_reason().is_some() {
            rows.push(
                design::caption(
                    "The draft changed. Insert the dictation at the current caret when ready.",
                )
                .into(),
            );
            rows.push(
                widget::flex_row(vec![
                    button("Insert here", Action::InsertRetained, available),
                    button("Discard", Action::DiscardRetained, true),
                ])
                .spacing(super::step::GAP_XS as u16)
                .into(),
            );
        }
        if let Some(setup) = setup.filter(|setup| setup.shown(target)) {
            let mut controls = Vec::new();
            let label = if let Some(phase) = setup.phase() {
                controls.push(button("Cancel setup", Action::CancelSetup, true));
                match phase {
                    SetupPhase::CheckingSaved => "Checking saved speech files…".to_owned(),
                    SetupPhase::Downloading { bytes } => format!(
                        "Downloading speech files · {:.0}%",
                        bytes as f64 / speech::setup::DOWNLOAD_BYTES as f64 * 100.0
                    ),
                    SetupPhase::Verifying => "Verifying speech files…".to_owned(),
                    SetupPhase::Extracting => "Preparing local speech runtime…".to_owned(),
                }
            } else if setup.prepared().is_some() {
                controls.push(button("Done", Action::DismissSetup, true));
                "Local dictation is ready. Press the microphone to record.".to_owned()
            } else {
                controls.push(button(
                    if setup.error().is_some() {
                        "Retry setup"
                    } else {
                        "Download and set up"
                    },
                    Action::Install,
                    available,
                ));
                controls.push(button("Close", Action::DismissSetup, true));
                format!(
                    "Local dictation runs on this computer. Optional speech download: {} MiB.",
                    speech::setup::DOWNLOAD_BYTES.div_ceil(1024 * 1024)
                )
            };
            rows.push(design::caption(label).into());
            if let Some(error) = setup.error() {
                rows.push(
                    design::caption(error.to_string())
                        .class(cosmic::theme::Text::Custom(design::warning_text))
                        .into(),
                );
            }
            rows.push(
                widget::flex_row(controls)
                    .spacing(super::step::GAP_XS as u16)
                    .into(),
            );
        }
        if rows.is_empty() {
            return widget::Space::new().height(0).into();
        }
        widget::container(widget::column::with_children(rows).spacing(super::step::GAP_XS))
            .padding([0, super::step::GAP_M as u16])
            .width(Length::Fill)
            .into()
    }
}
impl super::Cagents {
    pub(super) fn handle_dictation(
        &mut self,
        target: Target,
        action: Action,
    ) -> super::Task<super::Message> {
        use super::{Action as ChatAction, Message, Task};
        use cosmic::iced::futures::StreamExt;
        match action {
            Action::Start => {
                // Validate the live editor before exposing setup or acquiring a
                // microphone. A selected remote workspace never selects its mic.
                let assets = self.chat_ui.dictation_setup.prepared.clone();
                if assets.is_some() {
                    self.chat_ui.dictation_setup.dismiss(target);
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.closed_elsewhere
                    || runtime.disconnected
                    || runtime.snapshot.exited
                    || runtime.pending.is_some()
                    || runtime.queued.is_some()
                {
                    return Task::none();
                }
                let Some(assets) = assets else {
                    self.chat_ui.dictation_setup.show(target);
                    return Task::none();
                };
                let started = match runtime.dictation.begin(
                    target,
                    runtime.revision,
                    &runtime.composer,
                    &assets,
                ) {
                    Ok(started) => started,
                    Err(StartError::Speech(error)) => {
                        runtime.dictation.error = Some(error);
                        return Task::none();
                    }
                    Err(StartError::Active | StartError::RetainedText) => return Task::none(),
                };
                let (sender, receiver) = async_channel::bounded(4);
                let phases = cosmic::task::stream(
                    receiver
                        .map(move |action| Message::ChatUi(ChatAction::Dictation(target, action))),
                );
                let result = cosmic::task::future(async move {
                    let ticket = started.ticket;
                    let progress_ticket = ticket.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        started.session.run(|phase| {
                            let _ = sender.try_send(Action::Progress(
                                progress_ticket.clone(),
                                phase,
                                Instant::now(),
                            ));
                        })
                    })
                    .await
                    .unwrap_or(Err(speech::Error::Inference));
                    Message::ChatUi(ChatAction::Dictation(
                        target,
                        Action::Finished(Delivery::new(ticket, result)),
                    ))
                });
                return Task::batch([phases, result]);
            }
            Action::StopRecording => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.dictation.stop(target);
                }
            }
            Action::Cancel => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.dictation.cancel(target);
                }
            }
            Action::Progress(ticket, phase, now) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.dictation.progress(target, &ticket, phase, now);
                }
            }
            Action::Finished(delivery) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    let finished =
                        runtime
                            .dictation
                            .complete(target, &delivery, &mut runtime.composer);
                    runtime.dictation_applied(finished);
                }
            }
            Action::InsertRetained => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    let finished = runtime
                        .dictation
                        .insert_retained(target, &mut runtime.composer);
                    runtime.dictation_applied(finished);
                }
            }
            Action::DiscardRetained => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.dictation.discard_retained(target);
                }
            }
            Action::Install => {
                let eligible = self.chat_runtime_mut(target).is_some_and(|runtime| {
                    !runtime.closed_elsewhere
                        && !runtime.disconnected
                        && !runtime.snapshot.exited
                        && runtime.pending.is_none()
                        && runtime.queued.is_none()
                });
                if !eligible || !self.chat_ui.dictation_setup.shown(target) {
                    return Task::none();
                }
                let Some(root) =
                    crate::instance::project_dirs().map(|dirs| dirs.cache_dir().join("speech"))
                else {
                    self.chat_ui.dictation_setup.error = Some(speech::Error::SetupStorage);
                    return Task::none();
                };
                let (ticket, cancel) = match self.chat_ui.dictation_setup.begin() {
                    Ok(started) => started,
                    Err(_) => return Task::none(),
                };
                let (sender, receiver) = async_channel::bounded(4);
                let phases = cosmic::task::stream(
                    receiver
                        .map(move |action| Message::ChatUi(ChatAction::Dictation(target, action))),
                );
                let result = cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        speech::setup::prepare(&root, &cancel, |phase| {
                            let _ = sender.try_send(Action::SetupProgress(ticket, phase));
                        })
                    })
                    .await
                    .unwrap_or(Err(speech::Error::SetupStorage));
                    Message::ChatUi(ChatAction::Dictation(
                        target,
                        Action::SetupFinished(SetupDelivery::new(ticket, result)),
                    ))
                });
                return Task::batch([phases, result]);
            }
            Action::SetupProgress(ticket, progress) => {
                let phase = match progress {
                    speech::setup::Progress::CheckingSaved => SetupPhase::CheckingSaved,
                    speech::setup::Progress::Downloading { bytes, .. } => {
                        SetupPhase::Downloading { bytes }
                    }
                    speech::setup::Progress::Verifying => SetupPhase::Verifying,
                    speech::setup::Progress::Extracting => SetupPhase::Extracting,
                };
                self.chat_ui.dictation_setup.progress(ticket, phase);
            }
            Action::SetupFinished(delivery) => self.chat_ui.dictation_setup.delivered(delivery),
            Action::CancelSetup => {
                if self.chat_ui.dictation_setup.shown(target) {
                    self.chat_ui.dictation_setup.cancel();
                }
            }
            Action::DismissSetup => self.chat_ui.dictation_setup.dismiss(target),
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::widget::text_editor::{Edit, Position};

    fn target() -> Target {
        Target {
            card: crate::model::CardId::new(),
            generation: 7,
        }
    }
    fn cursor(line: usize, column: usize) -> Cursor {
        Cursor {
            position: Position { line, column },
            selection: None,
        }
    }
    fn fixture(target: Target, content: &Content) -> (State, Ticket) {
        let tracking = Tracking::new(target, 31, content);
        let ticket = tracking.ticket.clone();
        (
            State {
                active: Some(Active {
                    tracking,
                    control: None,
                    phase: speech::Phase::Preparing,
                    stopping: false,
                    recording_since: None,
                }),
                ..State::default()
            },
            ticket,
        )
    }
    fn delivered(ticket: &Ticket, text: &str) -> Delivery {
        Delivery::new(
            ticket.clone(),
            Ok(speech::Transcript {
                ticket: ticket.clone(),
                text: text.to_owned(),
            }),
        )
    }
    fn changed(state: &mut State, content: &mut Content, text: &str) {
        let change = attachment_draft::edit(content, Edit::Paste(Arc::new(text.into())))
            .unwrap()
            .unwrap();
        state.changed(&change);
    }

    #[test]
    fn concurrent_unicode_edit_maps_original_insert_and_preserves_moved_caret_and_image() {
        let target = target();
        let mut content = Content::with_text("before π\nselected\nafter");
        attachment_draft::move_to(&mut content, cursor(1, 3));
        let (mut state, ticket) = fixture(target, &content);
        attachment_draft::move_to(&mut content, cursor(0, 0));
        changed(&mut state, &mut content, "🚀\n");
        // A separate pending image follows the exact dictation replacement;
        // its identity and jobs remain owned by the existing attachment layer.
        let mut image = attachment_draft::Bookmark::new(cursor(2, 8));
        let current = content.cursor();
        let Finished::Applied(Applied::Measured(change)) =
            state.complete(target, &delivered(&ticket, "voice"), &mut content)
        else {
            panic!("mapped insertion must apply");
        };
        image.changed(&change);
        assert_eq!(content.text(), "🚀\nbefore π\nselvoiceected\nafter");
        assert_eq!(content.cursor(), current);
        assert!(image.valid);
        assert_eq!(image.cursor, cursor(2, 13));
        assert!(!state.has_bookmark());
    }

    #[test]
    fn concurrent_selection_edit_retains_text_until_explicit_insert_at_current_caret() {
        let target = target();
        let mut content = Content::with_text("alpha beta gamma");
        attachment_draft::move_to(
            &mut content,
            Cursor {
                position: Position {
                    line: 0,
                    column: 10,
                },
                selection: Some(Position { line: 0, column: 6 }),
            },
        );
        let (mut state, ticket) = fixture(target, &content);
        attachment_draft::move_to(&mut content, cursor(0, 8));
        changed(&mut state, &mut content, "!");
        assert!(matches!(
            state.complete(target, &delivered(&ticket, "spoken"), &mut content),
            Finished::Retained
        ));
        assert_eq!(
            state.retained_reason(),
            Some(HoldReason::OriginalSelectionChanged)
        );
        assert_eq!(content.text(), "alpha be!ta gamma");
        let late_target = Target {
            generation: 8,
            ..target
        };
        assert!(matches!(
            state.insert_retained(late_target, &mut content),
            Finished::Ignored
        ));
        attachment_draft::move_to(&mut content, cursor(0, 17));
        assert!(matches!(
            state.insert_retained(target, &mut content),
            Finished::Applied(_)
        ));
        assert_eq!(content.text(), "alpha be!ta gammaspoken");
        assert_eq!(state.retained_reason(), None);
    }

    #[test]
    fn newly_selected_text_is_never_erased_by_async_result() {
        let target = target();
        let mut content = Content::with_text("alpha beta gamma");
        attachment_draft::move_to(&mut content, cursor(0, 8));
        let (mut state, ticket) = fixture(target, &content);
        attachment_draft::move_to(
            &mut content,
            Cursor {
                position: Position {
                    line: 0,
                    column: 10,
                },
                selection: Some(Position { line: 0, column: 6 }),
            },
        );
        let selection = content.cursor();
        assert!(matches!(
            state.complete(target, &delivered(&ticket, "voice"), &mut content),
            Finished::Retained
        ));
        assert_eq!(
            state.retained_reason(),
            Some(HoldReason::CurrentSelectionChanged)
        );
        assert_eq!(content.text(), "alpha beta gamma");
        assert_eq!(content.cursor(), selection);
    }

    #[test]
    fn target_generation_operation_and_retirement_fence_late_events() {
        let target = target();
        let mut content = Content::with_text("draft");
        let (mut state, ticket) = fixture(target, &content);
        let now = Instant::now();
        let newer = Target {
            generation: target.generation + 1,
            ..target
        };
        assert!(!state.progress(newer, &ticket, speech::Phase::Recording, now));
        assert!(matches!(
            state.complete(newer, &delivered(&ticket, "wrong"), &mut content),
            Finished::Ignored
        ));
        let other = Ticket::new(target, ticket.draft_revision, ticket.bookmark);
        assert!(!state.progress(target, &other, speech::Phase::Recording, now));
        assert!(matches!(
            state.complete(target, &delivered(&other, "wrong"), &mut content),
            Finished::Ignored
        ));
        assert!(state.progress(target, &ticket, speech::Phase::Recording, now));
        assert!(state.stop(target));
        assert!(!state.stop(target));
        assert!(state.progress(target, &ticket, speech::Phase::Transcribing, now));
        assert!(!state.progress(target, &ticket, speech::Phase::Recording, now));
        state.retire();
        assert!(matches!(
            state.complete(target, &delivered(&ticket, "wrong"), &mut content),
            Finished::Ignored
        ));
        assert_eq!(content.text(), "draft");
    }

    #[test]
    fn cloned_delivery_applies_once_and_wrong_inner_ticket_does_not_retire_capture() {
        let target = target();
        let mut content = Content::with_text("");
        let (mut state, ticket) = fixture(target, &content);
        let wrong = Ticket::new(target, ticket.draft_revision, ticket.bookmark);
        let wrong_delivery = Delivery::new(
            ticket.clone(),
            Ok(speech::Transcript {
                ticket: wrong,
                text: "wrong".into(),
            }),
        );
        assert!(matches!(
            state.complete(target, &wrong_delivery, &mut content),
            Finished::Ignored
        ));
        assert!(state.has_bookmark());
        let delivery = delivered(&ticket, "once");
        let cloned = delivery.clone();
        assert!(matches!(
            state.complete(target, &delivery, &mut content),
            Finished::Applied(_)
        ));
        assert!(matches!(
            state.complete(target, &cloned, &mut content),
            Finished::Ignored
        ));
        assert_eq!(content.text(), "once");
    }

    #[test]
    fn unmeasured_edit_requires_explicit_insert_and_cancelled_error_never_changes_draft() {
        let target = target();
        let mut content = Content::with_text("safe");
        let (mut state, ticket) = fixture(target, &content);
        state.unmeasured_edit();
        assert!(matches!(
            state.complete(target, &delivered(&ticket, "retained"), &mut content),
            Finished::Retained
        ));
        assert!(state.discard_retained(target));
        let (mut state, ticket) = fixture(target, &content);
        let cancelled = Delivery::new(ticket, Err(speech::Error::Cancelled));
        assert!(matches!(
            state.complete(target, &cancelled, &mut content),
            Finished::Settled
        ));
        assert_eq!(state.error(), None);
        assert_eq!(content.text(), "safe");
    }

    #[test]
    fn identical_replacement_preserves_moved_caret_or_disjoint_selection_without_image_edit() {
        for current in [
            cursor(0, 16),
            Cursor {
                position: Position {
                    line: 0,
                    column: 16,
                },
                selection: Some(Position {
                    line: 0,
                    column: 11,
                }),
            },
        ] {
            let target = target();
            let mut content = Content::with_text("alpha beta gamma");
            attachment_draft::move_to(
                &mut content,
                Cursor {
                    position: Position {
                        line: 0,
                        column: 10,
                    },
                    selection: Some(Position { line: 0, column: 6 }),
                },
            );
            let (mut state, ticket) = fixture(target, &content);
            attachment_draft::move_to(&mut content, current);
            assert!(matches!(
                state.complete(target, &delivered(&ticket, "beta"), &mut content),
                Finished::Applied(Applied::Unchanged)
            ));
            assert_eq!(content.text(), "alpha beta gamma");
            assert_eq!(content.cursor(), current);
        }
    }

    #[test]
    fn cancel_then_retry_rejects_old_result_without_consuming_current_capture() {
        let target = target();
        let mut content = Content::with_text("draft");
        attachment_draft::move_to(&mut content, cursor(0, 0));
        let (mut state, first) = fixture(target, &content);
        assert!(!state.cancel(Target {
            generation: 99,
            ..target
        }));
        assert!(state.cancel(target));
        let (next, second) = fixture(target, &content);
        state = next;
        assert!(matches!(
            state.complete(target, &delivered(&first, "stale"), &mut content),
            Finished::Ignored
        ));
        assert!(state.has_bookmark());
        assert!(matches!(
            state.complete(target, &delivered(&second, "fresh"), &mut content),
            Finished::Applied(_)
        ));
        assert_eq!(content.text(), "freshdraft");
    }

    #[test]
    fn cancelled_and_stale_setup_callbacks_cannot_publish_ready_or_poison_retry() {
        let mut setup = Setup::default();
        let (first, cancel) = setup.begin().unwrap();
        assert_eq!(setup.begin().err(), Some(speech::Error::Busy));
        assert!(setup.progress(first, SetupPhase::Downloading { bytes: 10 }));
        assert!(!setup.progress(first, SetupPhase::Downloading { bytes: 9 }));
        assert!(!setup.progress(first, SetupPhase::Downloading { bytes: u64::MAX }));
        assert!(setup.progress(first, SetupPhase::Verifying));
        assert!(!setup.progress(first, SetupPhase::Downloading { bytes: 20 }));
        assert!(setup.cancel());
        assert!(cancel.is_cancelled());
        let (second, _) = setup.begin().unwrap();
        assert!(!setup.complete(first, Err(speech::Error::Inference)));
        assert_eq!(setup.error(), None);
        assert!(setup.prepared().is_none());
        assert!(setup.complete(second, Err(speech::Error::Unavailable)));
        assert_eq!(setup.error(), Some(speech::Error::Unavailable));
        assert!(setup.prepared().is_none());
    }
}
