//! Dictating into a chat's composer.
//!
//! The microphone beside the draft, or its shortcut, starts listening; what
//! is said is written into the draft where the caret was, an utterance at a
//! time, while the words still being heard show above the draft. Pressing it
//! again, Escape, or typing stops listening, and the last words follow. Enter
//! stops and sends once they have. A tap of the shortcut toggles; holding it
//! dictates until it is let go.
//!
//! Speech is recognized on this device by `cagents-dictate`, whose model is
//! downloaded the first time, once the reader agrees to it; see
//! [`crate::dictation`].

use std::time::Duration;

use cagents_dictate::{
    catalog::{self, Model},
    protocol::{ErrorKind, Event, InstallEvent, SessionId},
    store,
};
use cosmic::iced::advanced::{self, Widget, layout, renderer, widget::tree};

use super::*;
use crate::dictation::{Client, Level, Update};

/// How long the helper keeps its model loaded after dictation, so that the
/// next dictation starts at once. A loaded model holds about 900 MB.
const KEEP_WARM: Duration = Duration::from_secs(120);

/// A press of the shortcut held at least this long is push-to-talk: letting
/// it go stops. A shorter one is a tap, which toggles.
const HOLD: Duration = Duration::from_millis(350);

/// Dictation across the window: one microphone, so one session listens at
/// a time, while another may still be finishing.
pub(in crate::app) struct Shared {
    client: Client,
    /// The pane listening now, and its session.
    active: Option<(Target, SessionId)>,
    /// Every session that has not ended, listening or finishing, and its
    /// pane: a session's last words still reach it after another starts.
    sessions: HashMap<SessionId, Target>,
    /// The key that started the session, while it is held.
    held: Option<(keyboard::key::Physical, Instant)>,
    /// Bumped by every session, so a pending idle check knows whether the
    /// helper was used since it was scheduled.
    uses: u64,
    /// The model download under way and how far it has got.
    progress: Option<(&'static str, u64, u64)>,
    /// Why the last download or removal failed, for Settings.
    pub(super) problem: Option<String>,
    /// The models downloaded, read when the app starts and after each
    /// download or removal rather than from disk at each view.
    installed: Vec<&'static str>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            client: Client::default(),
            active: None,
            sessions: HashMap::new(),
            held: None,
            uses: 0,
            progress: None,
            problem: None,
            installed: installed_models(),
        }
    }
}

/// What dictation shows in one pane's composer.
pub(super) enum Pane {
    /// Asking to download `model` before the first dictation.
    Consent(&'static Model),
    /// The model is downloading.
    Downloading(&'static Model),
    /// The download finished; dictation can start.
    Ready(&'static Model),
    Session(Session),
}

impl Pane {
    pub(super) fn is_session(&self) -> bool {
        matches!(self, Self::Session(_))
    }

    /// Where a session's text begins and where its next words go, which
    /// follow every change of the draft, an undo step's included.
    pub(super) fn places(&mut self) -> Vec<&mut attachment_draft::Bookmark> {
        match self {
            Self::Session(session) => vec![&mut session.start, &mut session.end],
            _ => Vec::new(),
        }
    }
}

pub(super) struct Session {
    id: SessionId,
    phase: Phase,
    /// The model has loaded; until then speech is kept to transcribe.
    loaded: bool,
    /// The microphone has heard nothing for a while.
    silent: bool,
    /// The words of the utterance still being spoken.
    partial: String,
    /// Where this session's text begins and where the next text goes. Both
    /// follow other edits of the draft, so the text lands where it was
    /// started, and Discard can take back exactly what was dictated.
    start: attachment_draft::Bookmark,
    end: attachment_draft::Bookmark,
    /// Send the draft this way once the last words are in.
    send: Option<PromptMode>,
    level: Level,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The microphone is opening.
    Starting,
    Listening,
    /// Stopped listening; the last words are on their way.
    Finishing,
}

/// What the reader is told when dictation could not go on.
fn problem(kind: ErrorKind, message: &str) -> String {
    match kind {
        ErrorKind::NoInputDevice => "Dictation needs a microphone, and none is connected.".into(),
        ErrorKind::InputUnavailable => {
            format!("The microphone could not be opened: {message}")
        }
        ErrorKind::InputLost => "The microphone was disconnected, so dictation stopped.".into(),
        ErrorKind::ModelMissing | ErrorKind::ModelInvalid => {
            format!("The speech model could not be loaded: {message}")
        }
        ErrorKind::Protocol => format!("Dictation failed: {message}"),
    }
}

/// The model chosen in Settings, or the default one.
pub(in crate::app) fn chosen_model(id: &str) -> &'static Model {
    catalog::find(id)
        .or_else(|| catalog::find(catalog::DEFAULT))
        .expect("the default model is in the catalog")
}

/// The models downloaded. Looks at a handful of files for each.
fn installed_models() -> Vec<&'static str> {
    let Some(root) = store::root() else {
        return Vec::new();
    };
    catalog::MODELS
        .iter()
        .filter(|model| store::status(&root, model) == store::Status::Installed)
        .map(|model| model.id)
        .collect()
}

/// `bytes` in megabytes, for sizes people read.
pub(in crate::app) fn megabytes(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

impl Runtime {
    /// Whether this pane is listening, as opposed to finishing or idle.
    #[cfg(test)]
    pub(super) fn dictating(&self) -> bool {
        matches!(
            &self.dictation,
            Some(Pane::Session(session)) if session.phase != Phase::Finishing
        )
    }

    fn session_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        match &mut self.dictation {
            Some(Pane::Session(session)) if session.id == id => Some(session),
            _ => None,
        }
    }

    /// Writes an utterance's text where this session's text goes, keeping
    /// the reader's caret where it was if they moved it.
    fn dictation_insert(&mut self, id: SessionId, text: &str) {
        let Some(session) = self.session_mut(id) else {
            return;
        };
        session.partial.clear();
        if text.is_empty() {
            return;
        }
        let mut end = session.end;
        if !end.valid {
            // The place was edited away: the text goes at the end.
            end = attachment_draft::Bookmark::new(text_editor::Cursor {
                position: last_position(&self.composer),
                selection: None,
            });
        }
        let caret = attachment_draft::Bookmark::capture(&self.composer);
        attachment_draft::move_to(&mut self.composer, end.cursor);
        let before = self
            .composer
            .line(end.cursor.position.line)
            .and_then(|line| {
                line.text
                    .get(..end.cursor.position.column)
                    .and_then(|left| left.chars().next_back())
            });
        let after = self
            .composer
            .line(end.cursor.position.line)
            .and_then(|line| {
                line.text
                    .get(end.cursor.position.column..)
                    .and_then(|right| right.chars().next())
            });
        let mut insertion = String::new();
        if before.is_some_and(|before| !before.is_whitespace()) {
            insertion.push(' ');
        }
        insertion.push_str(text);
        // A space keeps the words apart from what follows; the next words go
        // before it, after these.
        let spaced = after.is_some_and(|after| !after.is_whitespace());
        if spaced {
            insertion.push(' ');
        }
        let Some(change) = attachment_draft::edit(
            &mut self.composer,
            text_editor::Edit::Paste(Arc::new(insertion)),
        ) else {
            return;
        };
        for bookmark in self.attachment_bookmarks.values_mut() {
            bookmark.changed(&change);
        }
        self.cancel_invalid_attachment_jobs();
        let undo_change = change.clone();
        if let Some(Pane::Session(session)) = &mut self.dictation {
            session.start.changed(&change);
            let mut position = change.inserted_end();
            if spaced {
                position.column -= 1;
            }
            session.end = attachment_draft::Bookmark::new(text_editor::Cursor {
                position,
                selection: None,
            });
        }
        // A caret elsewhere stays there; one at the insertion point follows
        // the text, as typing would.
        if caret.valid
            && caret.cursor != end.cursor
            && let Some(mapped) = attachment_draft::mapped(caret.cursor, &change)
        {
            attachment_draft::move_to(&mut self.composer, mapped);
        }
        // Each utterance is a step of its own: undo takes it back out and
        // returns the caret to where the reader had it.
        self.composer_undo.record(
            undo_change,
            crate::editor_history::EditKind::Other,
            caret.cursor,
            self.composer.cursor(),
            std::time::Instant::now(),
        );
        self.revision = self.revision.wrapping_add(1);
        self.composer_note = None;
        self.dismiss_suggestion();
        self.completion.restart(true);
        self.end_history_walk();
    }

    /// Follows an edit of the draft other than dictation's own.
    pub(super) fn dictation_edited(&mut self, change: &crate::editor_history::TextChange) {
        if let Some(Pane::Session(session)) = &mut self.dictation {
            session.start.changed(change);
            session.end.changed(change);
        }
    }

    /// An edit that does not say where it moved text: where dictation was
    /// writing is unknown, so what follows goes at the end of the draft and
    /// Discard keeps what is there.
    pub(super) fn dictation_lost_place(&mut self) {
        if let Some(Pane::Session(session)) = &mut self.dictation {
            session.start.valid = false;
            session.end.valid = false;
        }
    }

    /// The whole draft was replaced, as sending it does: what is said next
    /// goes at the end of the new one.
    pub(super) fn dictation_draft_replaced(&mut self) {
        let end = attachment_draft::Bookmark::new(text_editor::Cursor {
            position: last_position(&self.composer),
            selection: None,
        });
        if let Some(Pane::Session(session)) = &mut self.dictation {
            session.start = end;
            session.end = end;
        }
    }

    /// Takes back the text this session wrote, if it is still as written.
    fn dictation_take_back(&mut self) {
        let Some(Pane::Session(session)) = &self.dictation else {
            return;
        };
        let (start, end) = (session.start, session.end);
        if !start.valid || !end.valid || start.cursor.position == end.cursor.position {
            return;
        }
        let before = self.composer.selection_span();
        let inline = self.inline_references();
        attachment_draft::move_to(
            &mut self.composer,
            text_editor::Cursor {
                position: end.cursor.position,
                selection: Some(start.cursor.position),
            },
        );
        if let Some(change) = attachment_draft::edit(&mut self.composer, text_editor::Edit::Delete)
        {
            for bookmark in self.attachment_bookmarks.values_mut() {
                bookmark.changed(&change);
            }
            self.cancel_invalid_attachment_jobs();
            // Discarding is a step too, so undo brings the words back.
            self.composer_undo.record(
                change,
                crate::editor_history::EditKind::Other,
                before,
                self.composer.cursor(),
                std::time::Instant::now(),
            );
        }
        self.reconcile_references(inline);
        self.revision = self.revision.wrapping_add(1);
        self.completion.restart(true);
    }

    /// The microphone control beside the draft, when this copy of cagents
    /// can dictate.
    pub(super) fn dictation_control(
        &self,
        shortcuts: &crate::settings::KeyboardShortcuts,
        enabled: bool,
    ) -> Option<Element<'_, Message>> {
        use crate::workspace_widget::{GlobalShortcut, shortcut_tooltip_named};
        if !crate::dictation::available() {
            return None;
        }
        let target = self.target;
        let (tooltip, selected) = match &self.dictation {
            Some(Pane::Session(session)) if session.phase == Phase::Finishing => {
                ("Finishing dictation".to_owned(), true)
            }
            Some(Pane::Session(_)) => (
                shortcut_tooltip_named(GlobalShortcut::Dictate, shortcuts, "Stop dictating"),
                true,
            ),
            _ => (
                shortcut_tooltip_named(GlobalShortcut::Dictate, shortcuts, "Dictate"),
                false,
            ),
        };
        let control = crate::ui_controls::icon_button(Icon::Microphone, tooltip)
            .id(card_control_id(target.card, "chat-dictate"))
            .selected(selected)
            .tone(crate::design::Tone::Themed(if selected {
                design::accent_color
            } else {
                design::muted_icon_color
            }))
            .on_press_maybe(enabled.then(|| action(Action::Dictate(target))));
        Some(control.into())
    }

    /// What dictation shows above the draft: the words being heard while
    /// listening, or the question and progress of the first download.
    pub(super) fn dictation_view(&self) -> Option<Element<'_, Message>> {
        let target = self.target;
        fn strip(children: Vec<Element<'_, Message>>) -> Element<'_, Message> {
            widget::container(
                widget::row::with_children(children)
                    .spacing(step::GAP_S)
                    .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding(step::GAP_S)
            .class(design::surface(
                Some(design::sunken),
                [design::RADIUS_M, design::RADIUS_M, 0.0, 0.0],
                false,
            ))
            .into()
        }
        let caption = |text: String,
                       class: fn(&cosmic::Theme) -> iced::widget::text::Style|
         -> Element<'_, Message> {
            design::caption(text)
                .class(cosmic::theme::Text::Custom(class))
                .wrapping(iced::widget::text::Wrapping::Word)
                .width(Length::Fill)
                .into()
        };
        Some(match self.dictation.as_ref()? {
            Pane::Consent(model) => strip(vec![
                crate::ui_controls::passive_icon(Icon::Microphone, design::COMPACT_ICON_SIZE),
                caption(
                    format!(
                        "Dictation recognizes speech on this computer with {}, a {} MB \
                         download. Your voice is not sent anywhere; the text is sent \
                         like anything you type.",
                        model.name,
                        megabytes(model.size()),
                    ),
                    design::secondary_text,
                ),
                crate::ui_controls::button::quiet("Not now")
                    .on_press(action(Action::DictationDismiss(target)))
                    .into(),
                crate::ui_controls::button::primary("Download")
                    .id(card_control_id(target.card, "chat-dictation-download"))
                    .on_press(action(Action::DictationDownload(target)))
                    .into(),
            ]),
            Pane::Downloading(model) => {
                let text = match self.dictation_progress {
                    Some((done, total)) if total > 0 => format!(
                        "Downloading {}: {} of {} MB",
                        model.name,
                        megabytes(done),
                        megabytes(total)
                    ),
                    _ => format!("Downloading {}…", model.name),
                };
                strip(vec![
                    crate::ui_controls::activity_spinner(
                        f32::from(design::COMPACT_ICON_SIZE),
                        design::muted_icon_color,
                    ),
                    caption(text, design::secondary_text),
                    crate::ui_controls::button::quiet("Cancel")
                        .on_press(action(Action::DictationCancelDownload))
                        .into(),
                ])
            }
            Pane::Ready(model) => strip(vec![
                crate::ui_controls::passive_icon(Icon::Microphone, design::COMPACT_ICON_SIZE),
                caption(
                    format!("{} is ready. Start dictating when you are.", model.name),
                    design::secondary_text,
                ),
                small_icon(Icon::Close, "Dismiss", Action::DictationDismiss(target)),
                crate::ui_controls::button::primary("Dictate")
                    .on_press(action(Action::Dictate(target)))
                    .into(),
            ]),
            Pane::Session(session) => {
                let heard = if !session.partial.is_empty() {
                    caption(session.partial.clone(), design::muted_text)
                } else {
                    caption(
                        match session.phase {
                            Phase::Starting => "Opening the microphone…",
                            Phase::Finishing => "Finishing…",
                            Phase::Listening if session.silent => {
                                "Listening, but the microphone hears nothing. Check that it \
                                 is not muted."
                            }
                            Phase::Listening if !session.loaded => {
                                "Listening. The speech model is loading; keep talking."
                            }
                            Phase::Listening => "Listening…",
                        }
                        .to_owned(),
                        if session.silent {
                            design::warning_text
                        } else {
                            design::secondary_text
                        },
                    )
                };
                let indicator: Element<'_, Message> = if session.phase == Phase::Finishing {
                    crate::ui_controls::activity_spinner(
                        f32::from(design::COMPACT_ICON_SIZE),
                        design::muted_icon_color,
                    )
                } else {
                    meter(session.level.clone())
                };
                let finishing = session.phase == Phase::Finishing;
                strip(vec![
                    indicator,
                    heard,
                    crate::ui_controls::button::quiet("Discard")
                        .on_press(action(Action::DictationDiscard(target)))
                        .into(),
                    crate::ui_controls::button::secondary("Done")
                        .on_press_maybe((!finishing).then(|| action(Action::DictationStop(target))))
                        .into(),
                ])
            }
        })
    }
}

/// The end of the draft.
fn last_position(content: &text_editor::Content<cosmic::Renderer>) -> text_editor::Position {
    let line = content.line_count().saturating_sub(1);
    let column = content.line(line).map_or(0, |line| line.text.len());
    text_editor::Position { line, column }
}

impl Cagents {
    /// The microphone or its shortcut: starts dictating in `target`, or
    /// stops if it is dictating. `held` is the key that pressed it.
    pub(super) fn dictation_toggle(
        &mut self,
        target: Target,
        held: Option<keyboard::key::Physical>,
    ) -> Task<Message> {
        let sends = self.settings.dictation_sends;
        let model = chosen_model(
            self.settings
                .dictation_model
                .as_deref()
                .unwrap_or(catalog::DEFAULT),
        );
        let installing = self.chat_ui.dictation.client.installing();
        let progress = self
            .chat_ui
            .dictation
            .progress
            .map(|(_, done, total)| (done, total));
        // A model the app has not seen arrive may have been downloaded by
        // another copy of cagents or from the command line; look again
        // before asking to download it.
        if !self.chat_ui.dictation.installed.contains(&model.id) {
            self.chat_ui.dictation.installed = installed_models();
        }
        let installed = self.chat_ui.dictation.installed.contains(&model.id);
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        match &runtime.dictation {
            Some(Pane::Session(session)) => {
                if session.phase != Phase::Finishing {
                    let id = session.id;
                    let send = sends.then(|| runtime.submit_modes()[0]);
                    return self.dictation_stop(target, id, send);
                }
                return Task::none();
            }
            Some(Pane::Consent(_)) => {
                runtime.dictation = None;
                return Task::none();
            }
            Some(Pane::Downloading(_)) => return Task::none(),
            Some(Pane::Ready(_)) | None => {}
        }
        if installing == Some(model.id) {
            runtime.dictation = Some(Pane::Downloading(model));
            runtime.dictation_progress = progress;
            return Task::none();
        }
        if !installed {
            runtime.dictation = Some(Pane::Consent(model));
            return focus(target, "chat-dictation-download");
        }
        // One microphone: dictation elsewhere stops, keeping its words,
        // which still reach it by its session.
        if let Some((other, id)) = self.chat_ui.dictation.active.take() {
            if let Some(session) = self
                .chat_runtime_mut(other)
                .and_then(|runtime| runtime.session_mut(id))
            {
                session.phase = Phase::Finishing;
            }
            self.chat_ui.dictation.client.stop(id);
        }
        let started = self.chat_ui.dictation.client.start(model.id);
        let level = self.chat_ui.dictation.client.level();
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        match started {
            Ok(id) => {
                let here = attachment_draft::Bookmark::capture(&runtime.composer);
                let here = if here.valid {
                    attachment_draft::Bookmark::new(text_editor::Cursor {
                        position: here.cursor.position,
                        selection: None,
                    })
                } else {
                    here
                };
                runtime.composer_note = None;
                runtime.dictation = Some(Pane::Session(Session {
                    id,
                    phase: Phase::Starting,
                    loaded: false,
                    silent: false,
                    partial: String::new(),
                    start: here,
                    end: here,
                    send: None,
                    level,
                }));
                self.chat_ui.dictation.active = Some((target, id));
                self.chat_ui.dictation.sessions.insert(id, target);
                self.chat_ui.dictation.uses += 1;
                self.chat_ui.dictation.held = held.map(|key| (key, Instant::now()));
            }
            Err(message) => {
                runtime.dictation = None;
                runtime.composer_note = Some(message);
            }
        }
        focus(target, "chat-composer")
    }

    /// The shortcut or the palette: dictation in the focused chat. `held` is
    /// the key pressed, so that holding it is push-to-talk.
    pub(in crate::app) fn dictate_focused(
        &mut self,
        held: Option<keyboard::key::Physical>,
    ) -> Task<Message> {
        let Some(card) = self.workspace.focused() else {
            return Task::none();
        };
        if !crate::dictation::available()
            || self.chat_start_pending(card)
            || self.pane_content_inputs_blocked(card)
        {
            return Task::none();
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        if runtime.composer_read_only(false) {
            return Task::none();
        }
        let target = runtime.target;
        self.dictation_toggle(target, held)
    }

    /// Stops listening in `target`; `send` sends the draft that way once
    /// the last words are in.
    pub(super) fn dictation_stop(
        &mut self,
        target: Target,
        id: SessionId,
        send: Option<PromptMode>,
    ) -> Task<Message> {
        let Some(session) = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.session_mut(id))
        else {
            return Task::none();
        };
        session.phase = Phase::Finishing;
        session.send = send.or(session.send);
        if self
            .chat_ui
            .dictation
            .active
            .is_some_and(|(_, active)| active == id)
        {
            self.chat_ui.dictation.active = None;
        }
        self.chat_ui.dictation.held = None;
        self.chat_ui.dictation.client.stop(id);
        focus(target, "chat-composer")
    }

    /// Sending `target`'s draft while it dictates: it stops listening if it
    /// still is, and sends once the last words are in. `None` when it is not
    /// dictating, and the draft goes now.
    pub(super) fn dictation_send(
        &mut self,
        target: Target,
        mode: PromptMode,
    ) -> Option<Task<Message>> {
        let session = match &mut self.chat_runtime_mut(target)?.dictation {
            Some(Pane::Session(session)) => session,
            _ => return None,
        };
        if session.phase == Phase::Finishing {
            session.send = Some(mode);
            return Some(Task::none());
        }
        let id = session.id;
        Some(self.dictation_stop(target, id, Some(mode)))
    }

    /// The session dictating in `target`, if it is still listening.
    pub(super) fn dictation_listening(&mut self, target: Target) -> Option<SessionId> {
        match &self.chat_runtime_mut(target)?.dictation {
            Some(Pane::Session(session)) if session.phase != Phase::Finishing => Some(session.id),
            _ => None,
        }
    }

    /// The chat whose microphone is open, opening or listening, while it is
    /// pane `card`.
    pub(in crate::app) fn dictation_microphone_in(&self, card: CardId) -> Option<Target> {
        self.chat_ui
            .dictation
            .active
            .map(|(target, _)| target)
            .filter(|target| target.card == card)
    }

    /// The chat whose microphone is on while it is not, or will not be once
    /// the strip stops moving, at least half on screen: an edge of it left
    /// in view shows neither its strip nor its tab legibly.
    fn dictation_out_of_sight(&self) -> Option<Target> {
        let workspace = &self.workspace;
        self.chat_ui
            .dictation
            .active
            .map(|(target, _)| target)
            .filter(|target| {
                let card = target.card;
                if workspace.is_hidden(card)
                    || workspace.visibility(card) != Some(VisibilityClass::Visible)
                {
                    return true;
                }
                let Some(geometry) = workspace.card_geometry(card) else {
                    return true;
                };
                // Where the camera is going, so that passing over the chat
                // on the way elsewhere does not flash the strip away.
                let viewport = workspace.viewport_width();
                let start = workspace
                    .camera()
                    .target()
                    .clamp(0.0, workspace.geometry().max_offset(viewport));
                let shown = geometry.end.min(start + viewport) - geometry.start.max(start);
                // A chat wider than the window counts as seen when it fills it.
                shown < geometry.width().min(viewport) / 2.0
            })
    }

    /// Says the microphone is on while the chat it writes into is out of
    /// sight: in a tab scrolled away, another channel or another project.
    /// It cannot be hidden, as the status bar can, and it offers the way
    /// back to that chat and a way to stop. While the chat is in view, its
    /// own strip says this.
    pub(in crate::app) fn dictation_banner(&self) -> Element<'_, Message> {
        let Some(target) = self.dictation_out_of_sight() else {
            return iced::widget::Space::new().height(Length::Shrink).into();
        };
        let title = self
            .card_state(target.card)
            .map_or("a chat", |card| self.card_display_title(card));
        let blocked = self.modal_is_open();
        widget::container(
            widget::row::with_children(vec![
                meter(self.chat_ui.dictation.client.level()),
                crate::ui_controls::fade_text(format!("Microphone on: dictating into {title}"))
                    .width(Length::Fill)
                    .into(),
                crate::ui_controls::button::quiet("Show chat")
                    .on_press_maybe((!blocked).then_some(Message::Palette(
                        crate::app::palette_ui::Action::FocusPane(target.card),
                    )))
                    .into(),
                crate::ui_controls::button::secondary("Stop dictating")
                    .on_press(action(Action::DictationStop(target)))
                    .into(),
            ])
            .spacing(space::M)
            .align_y(Alignment::Center),
        )
        .padding([space::S, space::M])
        .width(Length::Fill)
        .class(design::surface(Some(design::sunken), [0.0; 4], false))
        .into()
    }

    pub(super) fn dictation_discard(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(Pane::Session(session)) = &runtime.dictation else {
            return Task::none();
        };
        let id = session.id;
        runtime.dictation_take_back();
        runtime.dictation = None;
        if self
            .chat_ui
            .dictation
            .active
            .is_some_and(|(_, active)| active == id)
        {
            self.chat_ui.dictation.active = None;
        }
        self.chat_ui.dictation.sessions.remove(&id);
        self.chat_ui.dictation.held = None;
        self.chat_ui.dictation.client.cancel(id);
        Task::batch([self.dictation_idle_later(), focus(target, "chat-composer")])
    }

    /// The reader agreed to download the model.
    pub(super) fn dictation_download(&mut self, target: Target) -> Task<Message> {
        let Some(Pane::Consent(model)) = self
            .chat_runtime_mut(target)
            .and_then(|runtime| runtime.dictation.as_ref())
        else {
            return Task::none();
        };
        let model = *model;
        let result = self.dictation_install(model.id);
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        match result {
            Ok(()) => {
                runtime.dictation = Some(Pane::Downloading(model));
                runtime.dictation_progress = None;
            }
            Err(message) => {
                runtime.dictation = None;
                runtime.composer_note = Some(message);
            }
        }
        Task::none()
    }

    /// Starts downloading `model`, from a pane or from Settings.
    pub(in crate::app) fn dictation_install(&mut self, model: &'static str) -> Result<(), String> {
        let shared = &mut self.chat_ui.dictation;
        if shared.client.installing() == Some(model) {
            return Ok(());
        }
        shared.problem = None;
        shared.client.install(model)?;
        shared.progress = Some((model, 0, 0));
        Ok(())
    }

    pub(in crate::app) fn dictation_cancel_install(&mut self) {
        self.chat_ui.dictation.client.cancel_install();
        self.chat_ui.dictation.progress = None;
        self.each_dictation_pane(|runtime| {
            if matches!(runtime.dictation, Some(Pane::Downloading(_))) {
                runtime.dictation = None;
            }
        });
    }

    pub(in crate::app) fn dictation_remove(&mut self, model: &'static str) {
        // A helper holding this model would keep its files open.
        if self.chat_ui.dictation.sessions.is_empty() {
            self.chat_ui.dictation.client.release();
        }
        self.chat_ui.dictation.problem = self.chat_ui.dictation.client.remove(model).err();
    }

    /// How the speech models stand, for Settings.
    pub(in crate::app) fn dictation_settings(
        &self,
    ) -> crate::app::settings_ui::DictationSettings<'_> {
        let shared = &self.chat_ui.dictation;
        crate::app::settings_ui::DictationSettings {
            available: crate::dictation::available(),
            installed: &shared.installed,
            downloading: shared.progress,
            problem: shared.problem.as_deref(),
        }
    }

    fn each_dictation_pane(&mut self, mut each: impl FnMut(&mut Runtime)) {
        for card in self.cards.values_mut() {
            if let CardRuntime::Chat(runtime) = card {
                each(runtime);
            }
        }
        for runtime in self.chat_ui.parked.values_mut() {
            each(runtime);
        }
    }

    /// Lets the helper go once it has been idle for [`KEEP_WARM`].
    fn dictation_idle_later(&self) -> Task<Message> {
        let uses = self.chat_ui.dictation.uses;
        cosmic::task::future(async move {
            tokio::time::sleep(KEEP_WARM).await;
            Message::ChatUi(Action::DictationIdle(uses))
        })
    }

    pub(super) fn dictation_idle(&mut self, uses: u64) {
        let shared = &mut self.chat_ui.dictation;
        if shared.uses == uses && shared.sessions.is_empty() {
            shared.client.release();
        }
    }

    /// A key let go: the end of push-to-talk if it is the one held.
    pub(in crate::app) fn dictation_key_released(
        &mut self,
        key: keyboard::key::Physical,
    ) -> Option<Task<Message>> {
        let (held, since) = self.chat_ui.dictation.held?;
        if held != key {
            return None;
        }
        self.chat_ui.dictation.held = None;
        if since.elapsed() < HOLD {
            return None;
        }
        let (target, id) = self.chat_ui.dictation.active?;
        let send = if self.settings.dictation_sends {
            self.chat_runtime_mut(target)
                .map(|runtime| runtime.submit_modes()[0])
        } else {
            None
        };
        Some(self.dictation_stop(target, id, send))
    }

    /// The window lost the keyboard while the shortcut was held down, so
    /// its release will not arrive: push-to-talk ends here, keeping what
    /// was said without sending it.
    pub(in crate::app) fn dictation_unfocused(&mut self) -> Task<Message> {
        if self.chat_ui.dictation.held.take().is_none() {
            return Task::none();
        }
        match self.chat_ui.dictation.active {
            Some((target, id)) => self.dictation_stop(target, id, None),
            None => Task::none(),
        }
    }

    /// Chat `card` is closing or giving way to its terminal: it stops
    /// listening at once, so the microphone never stays open for a draft
    /// nobody can see. Each session ends when the helper confirms.
    pub(super) fn dictation_forget(&mut self, card: CardId) {
        let shared = &mut self.chat_ui.dictation;
        for (&id, target) in &shared.sessions {
            if target.card == card {
                shared.client.cancel(id);
            }
        }
        if shared.active.is_some_and(|(target, _)| target.card == card) {
            shared.active = None;
            shared.held = None;
        }
    }

    pub(super) fn dictation_update(&mut self, update: Update) -> Task<Message> {
        match update {
            Update::Helper { generation, event } => {
                if !self.chat_ui.dictation.client.is_current(generation) {
                    return Task::none();
                }
                self.dictation_event(event)
            }
            Update::Exited {
                generation,
                problem,
            } => {
                if !self.chat_ui.dictation.client.is_current(generation) {
                    return Task::none();
                }
                self.chat_ui.dictation.client.release();
                self.chat_ui.dictation.active = None;
                self.chat_ui.dictation.held = None;
                let problem = problem.unwrap_or_else(|| "Dictation stopped unexpectedly.".into());
                for (id, target) in std::mem::take(&mut self.chat_ui.dictation.sessions) {
                    if let Some(runtime) = self.chat_runtime_mut(target)
                        && runtime.session_mut(id).is_some()
                    {
                        runtime.dictation = None;
                        runtime.composer_note = Some(problem.clone());
                    }
                }
                Task::none()
            }
            Update::Install { model, event } => {
                match event {
                    InstallEvent::Progress { done, total } => {
                        // Progress queued before a cancel is not a download.
                        if self.chat_ui.dictation.client.installing() != Some(model) {
                            return Task::none();
                        }
                        self.chat_ui.dictation.progress = Some((model, done, total));
                        self.each_dictation_pane(|runtime| {
                            if matches!(&runtime.dictation, Some(Pane::Downloading(m)) if m.id == model)
                            {
                                runtime.dictation_progress = Some((done, total));
                            }
                        });
                    }
                    InstallEvent::Installed { .. } => {
                        self.chat_ui.dictation.client.installed(model);
                        self.chat_ui.dictation.progress = None;
                        self.chat_ui.dictation.installed = installed_models();
                        self.each_dictation_pane(|runtime| {
                            if let Some(Pane::Downloading(m)) = runtime.dictation
                                && m.id == model
                            {
                                runtime.dictation = Some(Pane::Ready(m));
                            }
                        });
                    }
                    InstallEvent::Failed { message, .. } => {
                        self.chat_ui.dictation.client.installed(model);
                        self.chat_ui.dictation.progress = None;
                        let message = format!("The speech model did not download: {message}");
                        self.chat_ui.dictation.problem = Some(message.clone());
                        self.each_dictation_pane(|runtime| {
                            if matches!(&runtime.dictation, Some(Pane::Downloading(m)) if m.id == model)
                            {
                                runtime.dictation = None;
                                runtime.composer_note = Some(message.clone());
                            }
                        });
                    }
                }
                Task::none()
            }
            Update::Removed { result, .. } => {
                self.chat_ui.dictation.installed = installed_models();
                self.chat_ui.dictation.problem = result
                    .err()
                    .map(|message| format!("The speech model was not removed: {message}"));
                Task::none()
            }
        }
    }

    fn dictation_event(&mut self, event: Event) -> Task<Message> {
        let id = match event.session() {
            Some(id) => id,
            // A start the helper refused before it had a session: it is the
            // one listening now.
            None if matches!(event, Event::Error { .. }) => match self.chat_ui.dictation.active {
                Some((_, id)) => id,
                None => return Task::none(),
            },
            None => return Task::none(),
        };
        let Some(&target) = self.chat_ui.dictation.sessions.get(&id) else {
            return Task::none();
        };
        let Some(runtime) = self.chat_runtime_mut(target) else {
            // Its pane has closed.
            self.dictation_ended(id);
            self.chat_ui.dictation.client.cancel(id);
            return self.dictation_idle_later();
        };
        match event {
            Event::Listening { .. } => {
                if let Some(session) = runtime.session_mut(id)
                    && session.phase == Phase::Starting
                {
                    session.phase = Phase::Listening;
                }
            }
            Event::Ready { .. } => {
                if let Some(session) = runtime.session_mut(id) {
                    session.loaded = true;
                }
            }
            Event::Signal { present, .. } => {
                if let Some(session) = runtime.session_mut(id) {
                    session.silent = !present;
                }
            }
            Event::Partial { text, .. } => {
                if let Some(session) = runtime.session_mut(id) {
                    session.partial = text;
                }
            }
            Event::Final { text, .. } => runtime.dictation_insert(id, &text),
            Event::Stopped { .. } | Event::Cancelled { .. } | Event::Error { .. } => {
                let send = runtime.session_mut(id).and_then(|session| session.send);
                if runtime.session_mut(id).is_some() {
                    runtime.dictation = None;
                }
                if let Event::Error { kind, message, .. } = &event {
                    runtime.composer_note = Some(problem(*kind, message));
                }
                self.dictation_ended(id);
                let idle = self.dictation_idle_later();
                return match send {
                    Some(mode) if matches!(event, Event::Stopped { .. }) => {
                        Task::batch([idle, self.handle_chat_ui(Action::Submit(target, mode))])
                    }
                    _ => idle,
                };
            }
            Event::Hello { .. } | Event::Level { .. } => {}
        }
        Task::none()
    }

    /// Forgets session `id`, which has ended.
    fn dictation_ended(&mut self, id: SessionId) {
        let shared = &mut self.chat_ui.dictation;
        shared.sessions.remove(&id);
        if shared.active.is_some_and(|(_, active)| active == id) {
            shared.active = None;
            shared.held = None;
        }
    }

    pub(super) fn dictation_subscription(&self) -> Subscription<Message> {
        Subscription::run_with(
            UpdateSource(self.chat_ui.dictation.client.events()),
            updates,
        )
        .map(|update| Message::ChatUi(Action::DictationUpdate(update)))
    }
}

fn focus(target: Target, control: &str) -> Task<Message> {
    iced::widget::operation::focus(card_control_id(target.card, control)).map(cosmic::Action::App)
}

#[derive(Clone)]
struct UpdateSource(async_channel::Receiver<Update>);
impl PartialEq for UpdateSource {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}
impl Eq for UpdateSource {}
impl std::hash::Hash for UpdateSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
    }
}
fn updates(source: &UpdateSource) -> Pin<Box<dyn iced::futures::Stream<Item = Update> + Send>> {
    Box::pin(iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|update| (update, receiver)) },
    ))
}

/// The input level as five bars, drawn from [`Level`] at its own pace:
/// levels never pass through the app's update, so the meter redraws itself
/// and nothing else is rebuilt for it.
fn meter<'a>(level: Level) -> Element<'a, Message> {
    Element::new(Meter { level })
}

struct Meter {
    level: Level,
}

/// How often a visible meter redraws: the helper reports levels about this
/// often, and a frame that changes nothing else is cheap (see the activity
/// spinner's `SPINNER_FRAME`).
const METER_FRAME: Duration = Duration::from_millis(50);
/// How often a meter looks again once it has drawn the same bars for
/// `METER_STEADY` frames: a pause or a room's steady hum draws the same
/// frame, so it costs fewer of them, and a word still moves the bars well
/// within a syllable.
const METER_QUIET_FRAME: Duration = Duration::from_millis(150);
const METER_STEADY: u8 = 4;
const METER_BARS: usize = 5;
const METER_SIZE: f32 = 16.0;

#[derive(Default)]
struct MeterState {
    /// The level drawn, eased toward the reported one so the bars fall
    /// rather than flicker between reports.
    shown: f32,
    /// Frames in a row that lit the same bars.
    steady: u8,
}

impl MeterState {
    /// Moves what is shown toward `level` for a frame, and says when to
    /// look again.
    fn advance(&mut self, level: f32) -> Duration {
        let before = bars_lit(self.shown);
        // Rise at once, fall over a few frames.
        self.shown = if level >= self.shown {
            level
        } else {
            (self.shown * 0.6 + level * 0.4).max(level)
        };
        self.steady = if bars_lit(self.shown) == before {
            self.steady.saturating_add(1)
        } else {
            0
        };
        if self.steady < METER_STEADY {
            METER_FRAME
        } else {
            METER_QUIET_FRAME
        }
    }
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Meter {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<MeterState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(MeterState::default())
    }

    fn size(&self) -> iced::Size<Length> {
        iced::Size::new(Length::Fixed(METER_SIZE), Length::Fixed(METER_SIZE))
    }

    fn layout(
        &mut self,
        _tree: &mut tree::Tree,
        _renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, METER_SIZE, METER_SIZE)
    }

    fn update(
        &mut self,
        tree: &mut tree::Tree,
        event: &iced::Event,
        layout: advanced::Layout<'_>,
        _cursor: iced::mouse::Cursor,
        _renderer: &cosmic::Renderer,
        _clipboard: &mut dyn advanced::Clipboard,
        shell: &mut advanced::Shell<'_, Message>,
        viewport: &iced::Rectangle,
    ) {
        if !layout.bounds().intersects(viewport) {
            return;
        }
        if let iced::Event::Window(iced::window::Event::RedrawRequested(now)) = event {
            let next = tree
                .state
                .downcast_mut::<MeterState>()
                .advance(self.level.get());
            shell.request_redraw_at(*now + next);
        }
    }

    fn draw(
        &self,
        tree: &tree::Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        _style: &renderer::Style,
        layout: advanced::Layout<'_>,
        _cursor: iced::mouse::Cursor,
        _viewport: &iced::Rectangle,
    ) {
        use advanced::Renderer as _;
        let bounds = layout.bounds();
        let shown = tree.state.downcast_ref::<MeterState>().shown;
        let bright = design::accent_color(theme);
        let unlit = design::muted_color(theme);
        let gap = 1.5;
        let width = (bounds.width - gap * (METER_BARS - 1) as f32) / METER_BARS as f32;
        for bar in 0..METER_BARS {
            // The middle bar is tallest, as a voice meter is usually drawn.
            let reach = [0.45, 0.75, 1.0, 0.75, 0.45][bar];
            let on = lit(shown, bar);
            let height = bounds.height * reach * if on { 1.0 } else { 0.35 };
            renderer.fill_quad(
                renderer::Quad {
                    bounds: iced::Rectangle {
                        x: bounds.x + bar as f32 * (width + gap),
                        y: bounds.y + (bounds.height - height) / 2.0,
                        width,
                        height,
                    },
                    border: iced::Border {
                        radius: (width / 2.0).into(),
                        ..iced::Border::default()
                    },
                    ..renderer::Quad::default()
                },
                if on { bright } else { unlit },
            );
        }
    }
}

/// Whether `bar` is lit at the level `shown`.
fn lit(shown: f32, bar: usize) -> bool {
    shown >= (bar as f32 + 0.5) / METER_BARS as f32 * 0.8
}

/// How many bars are lit at the level `shown`; they light from the first.
fn bars_lit(shown: f32) -> usize {
    (0..METER_BARS).take_while(|&bar| lit(shown, bar)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_reads_as_a_sentence() {
        for kind in [
            ErrorKind::NoInputDevice,
            ErrorKind::InputUnavailable,
            ErrorKind::InputLost,
            ErrorKind::ModelMissing,
            ErrorKind::ModelInvalid,
            ErrorKind::Protocol,
        ] {
            let text = problem(kind, "detail");
            assert!(text.ends_with('.') || text.ends_with("detail"), "{text}");
            assert!(
                text.chars().next().is_some_and(char::is_uppercase),
                "{text}"
            );
        }
    }

    const SESSION: SessionId = 7;

    /// A chat dictating into `draft`, started with the caret at `column`.
    fn dictating(draft: &str, column: usize) -> (Cagents, CardId, Target) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = listen(&mut app, "dictation", SESSION, draft, column);
        (app, card, target)
    }

    /// Opens another chat in `app` listening as session `id`.
    fn listen(
        app: &mut Cagents,
        name: &str,
        id: SessionId,
        draft: &str,
        column: usize,
    ) -> (CardId, Target) {
        let card = super::super::tests::live_chat(app, Some(name));
        let target = super::super::tests::chat_runtime(app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.replace_draft(text_editor::Content::with_text(draft));
        let here = caret(column);
        attachment_draft::move_to(&mut runtime.composer, here);
        runtime.dictation = Some(Pane::Session(Session {
            id,
            phase: Phase::Listening,
            loaded: true,
            silent: false,
            partial: String::new(),
            start: attachment_draft::Bookmark::new(here),
            end: attachment_draft::Bookmark::new(here),
            send: None,
            level: Level::default(),
        }));
        app.chat_ui.dictation.active = Some((target, id));
        app.chat_ui.dictation.sessions.insert(id, target);
        (card, target)
    }

    fn stopped(id: SessionId) -> Event {
        Event::Stopped {
            session: id,
            stats: Default::default(),
        }
    }

    fn caret(column: usize) -> text_editor::Cursor {
        text_editor::Cursor {
            position: text_editor::Position { line: 0, column },
            selection: None,
        }
    }

    fn hear(app: &mut Cagents, event: Event) {
        drop(app.dictation_event(event));
    }

    fn final_text(text: &str) -> Event {
        Event::Final {
            session: SESSION,
            segment: 0,
            text: text.to_owned(),
        }
    }

    fn draft(app: &Cagents, card: CardId) -> String {
        super::super::tests::chat_runtime(app, card)
            .composer
            .text()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn what_is_said_lands_where_dictation_started_one_utterance_after_another() {
        let (mut app, card, _) = dictating("Hello world", 5);
        hear(&mut app, final_text("there"));
        assert_eq!(draft(&app, card), "Hello there world");
        // The caret was where the text went, so it follows the text.
        let runtime = super::super::tests::chat_runtime(&app, card);
        assert_eq!(runtime.composer.cursor().position.column, 11);
        hear(&mut app, final_text("again."));
        assert_eq!(draft(&app, card), "Hello there again. world");
    }

    #[test]
    fn words_said_before_other_words_are_kept_apart_from_them() {
        let (mut app, card, _) = dictating("world", 0);
        hear(&mut app, final_text("Hello"));
        assert_eq!(draft(&app, card), "Hello world");
        hear(&mut app, final_text("there"));
        assert_eq!(draft(&app, card), "Hello there world");
        drop(app.dictation_event(stopped(SESSION)));
        assert_eq!(draft(&app, card), "Hello there world");
    }

    #[test]
    fn the_last_words_of_one_chat_reach_it_after_another_starts_dictating() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (first, first_target) = listen(&mut app, "first", SESSION, "One", 3);
        drop(app.handle_chat_ui(Action::DictationStop(first_target)));
        let (second, _) = listen(&mut app, "second", SESSION + 1, "Two", 3);
        hear(&mut app, final_text("more"));
        hear(&mut app, stopped(SESSION));
        assert_eq!(draft(&app, first), "One more");
        assert_eq!(draft(&app, second), "Two");
        assert!(
            super::super::tests::chat_runtime(&app, first)
                .dictation
                .is_none()
        );
        assert_eq!(
            app.chat_ui.dictation.active.map(|(_, id)| id),
            Some(SESSION + 1)
        );
        assert!(super::super::tests::chat_runtime(&app, second).dictating());
    }

    #[test]
    fn a_caret_moved_elsewhere_stays_and_edits_before_the_text_move_it() {
        let (mut app, card, target) = dictating("Hello world", 11);
        hear(&mut app, final_text("one"));
        assert_eq!(draft(&app, card), "Hello world one");
        let runtime = app.chat_runtime_mut(target).unwrap();
        attachment_draft::move_to(&mut runtime.composer, caret(0));
        let change = attachment_draft::edit(
            &mut runtime.composer,
            text_editor::Edit::Paste(Arc::new("Oh, ".to_owned())),
        )
        .unwrap();
        runtime.dictation_edited(&change);
        hear(&mut app, final_text("two"));
        assert_eq!(draft(&app, card), "Oh, Hello world one two");
        // The reader's caret, after what they typed, did not jump.
        let runtime = super::super::tests::chat_runtime(&app, card);
        assert_eq!(runtime.composer.cursor().position.column, 4);
    }

    #[test]
    fn discard_takes_back_exactly_what_was_dictated() {
        let (mut app, card, target) = dictating("Hello world", 5);
        hear(&mut app, final_text("there"));
        hear(&mut app, final_text("again"));
        drop(app.handle_chat_ui(Action::DictationDiscard(target)));
        assert_eq!(draft(&app, card), "Hello world");
        assert!(
            super::super::tests::chat_runtime(&app, card)
                .dictation
                .is_none()
        );
        assert!(app.chat_ui.dictation.active.is_none());
    }

    /// Each utterance is a step of the draft's undo history, and so is
    /// discarding them, so neither loses the reader's own edits.
    #[test]
    fn undo_brings_back_a_discard_then_takes_back_one_utterance_at_a_time() {
        let (mut app, card, target) = dictating("Hello world", 5);
        hear(&mut app, final_text("there"));
        hear(&mut app, final_text("again"));
        assert_eq!(draft(&app, card), "Hello there again world");
        drop(app.handle_chat_ui(Action::DictationDiscard(target)));
        assert_eq!(draft(&app, card), "Hello world");
        assert!(app.chat_runtime_mut(target).unwrap().undo_composer(false));
        assert_eq!(draft(&app, card), "Hello there again world");
        assert!(app.chat_runtime_mut(target).unwrap().undo_composer(false));
        assert_eq!(draft(&app, card), "Hello there world");
    }

    /// Undo moves text as any edit does, so where the next words go follows
    /// it: taking back a misheard utterance leaves dictation writing there.
    #[test]
    fn after_undoing_an_utterance_the_next_one_lands_in_its_place() {
        let (mut app, card, target) = dictating("Hello world", 5);
        hear(&mut app, final_text("there"));
        assert!(app.chat_runtime_mut(target).unwrap().undo_composer(false));
        assert_eq!(draft(&app, card), "Hello world");
        hear(&mut app, final_text("again"));
        assert_eq!(draft(&app, card), "Hello again world");
        // Discard takes back what this session still has in the draft.
        drop(app.handle_chat_ui(Action::DictationDiscard(target)));
        assert_eq!(draft(&app, card), "Hello world");
    }

    #[test]
    fn a_place_edited_away_sends_the_rest_to_the_end_and_discard_keeps_the_draft() {
        let (mut app, card, target) = dictating("Hello world", 5);
        hear(&mut app, final_text("there"));
        app.chat_runtime_mut(target).unwrap().dictation_lost_place();
        hear(&mut app, final_text("again"));
        assert_eq!(draft(&app, card), "Hello there world again");
        drop(app.handle_chat_ui(Action::DictationDiscard(target)));
        assert_eq!(draft(&app, card), "Hello there world again");
    }

    #[test]
    fn partials_show_until_their_utterance_is_written_and_other_sessions_are_ignored() {
        let (mut app, card, _) = dictating("", 0);
        hear(
            &mut app,
            Event::Partial {
                session: SESSION,
                segment: 0,
                text: "hel".to_owned(),
            },
        );
        hear(
            &mut app,
            Event::Final {
                session: SESSION + 1,
                segment: 0,
                text: "stale".to_owned(),
            },
        );
        let session = |app: &Cagents| match &super::super::tests::chat_runtime(app, card).dictation
        {
            Some(Pane::Session(session)) => session.partial.clone(),
            _ => panic!("still dictating"),
        };
        assert_eq!(session(&app), "hel");
        assert_eq!(draft(&app, card), "");
        hear(&mut app, final_text("Hello."));
        assert_eq!(session(&app), "");
        assert_eq!(draft(&app, card), "Hello.");
    }

    #[test]
    fn stopping_keeps_the_draft_and_a_failure_says_why() {
        let (mut app, card, target) = dictating("Hello", 5);
        drop(app.handle_chat_ui(Action::DictationStop(target)));
        assert!(!super::super::tests::chat_runtime(&app, card).dictating());
        hear(&mut app, final_text("world"));
        hear(
            &mut app,
            Event::Stopped {
                session: SESSION,
                stats: Default::default(),
            },
        );
        let runtime = super::super::tests::chat_runtime(&app, card);
        assert_eq!(draft(&app, card), "Hello world");
        assert!(runtime.dictation.is_none());
        assert!(app.chat_ui.dictation.active.is_none());

        let (mut app, card, _) = dictating("Hello", 5);
        hear(
            &mut app,
            Event::Error {
                session: Some(SESSION),
                kind: ErrorKind::InputLost,
                message: "gone".to_owned(),
            },
        );
        let runtime = super::super::tests::chat_runtime(&app, card);
        assert!(runtime.dictation.is_none());
        assert_eq!(
            runtime.composer_note.as_deref(),
            Some("The microphone was disconnected, so dictation stopped.")
        );
    }

    #[test]
    fn sending_while_dictating_sends_once_the_last_words_are_in() {
        let (mut app, card, target) = dictating("Hello", 5);
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        // Nothing goes before the last words arrive.
        assert_eq!(draft(&app, card), "Hello");
        assert!(!super::super::tests::chat_runtime(&app, card).dictating());
        hear(&mut app, final_text("world"));
        hear(&mut app, stopped(SESSION));
        assert_eq!(draft(&app, card), "");
    }

    #[test]
    fn sending_while_the_last_words_are_on_their_way_waits_for_them() {
        let (mut app, card, target) = dictating("Hello", 5);
        drop(app.handle_chat_ui(Action::DictationStop(target)));
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
        assert_eq!(draft(&app, card), "Hello");
        hear(&mut app, final_text("world"));
        hear(&mut app, stopped(SESSION));
        assert_eq!(draft(&app, card), "");
    }

    #[test]
    fn an_unknown_choice_falls_back_to_the_default_model() {
        assert_eq!(chosen_model("gone").id, catalog::DEFAULT);
        assert_eq!(
            chosen_model("parakeet-tdt-0.6b-v2").id,
            "parakeet-tdt-0.6b-v2"
        );
    }

    /// A meter showing the same bars, in a pause or over a room's steady
    /// hum, looks again less often, and goes back to every frame as soon
    /// as the bars move.
    #[test]
    fn a_steady_meter_redraws_less_often_until_the_level_moves() {
        let mut meter = MeterState::default();
        for hum in [0.1, 0.0] {
            let frames: Vec<_> = (0..8).map(|_| meter.advance(hum)).collect();
            assert_eq!(frames.last(), Some(&METER_QUIET_FRAME), "{hum}");
            assert!(frames.contains(&METER_FRAME), "{hum}");
        }
        assert_eq!(
            meter.advance(0.9),
            METER_FRAME,
            "a word lights bars at once"
        );
        assert_eq!(bars_lit(meter.shown), METER_BARS);
    }

    /// Escape stops listening; pressed again while the last words arrive,
    /// it does nothing rather than reach past dictation to stop the turn.
    #[test]
    fn escape_while_the_last_words_arrive_leaves_the_turn_running() {
        use keyboard::key::Named;
        let renderer = crate::app::tests::headless_renderer();
        let (mut app, card, target) = dictating("Fix the build", 13);
        let mut snapshot = (*super::super::tests::chat_runtime(&app, card).snapshot).clone();
        snapshot.generation += 1;
        snapshot.busy = true;
        super::super::show_chat_snapshot_for_test(&mut app, card, snapshot);
        let escape = |app: &Cagents| {
            super::super::tests::composer_key_messages(
                app,
                card,
                760.0,
                super::super::tests::permission_key(Named::Escape, keyboard::Modifiers::empty()),
                &renderer,
            )
        };
        let messages = escape(&app);
        assert!(
            matches!(
                messages.as_slice(),
                [Message::ChatUi(Action::DictationStop(_))]
            ),
            "{messages:?}"
        );
        for message in messages {
            drop(app.update(message));
        }
        assert!(!super::super::tests::chat_runtime(&app, card).dictating());
        let messages = escape(&app);
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::ChatUi(Action::Interrupt(_)))),
            "{messages:?}"
        );
        for message in messages {
            drop(app.update(message));
        }
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .session_mut(SESSION)
                .is_some()
        );
        // Once the last words are in, Escape is the turn's again.
        drop(app.dictation_event(stopped(SESSION)));
        let messages = escape(&app);
        assert!(
            matches!(messages.as_slice(), [Message::ChatUi(Action::Interrupt(_))]),
            "{messages:?}"
        );
    }

    /// A chat that closes, or gives way to its terminal, stops listening
    /// at once; the helper's word that the session ended finds no pane.
    #[test]
    fn a_chat_that_closes_stops_listening() {
        let (mut app, card, _) = dictating("Fix the build", 13);
        app.chat_forget(card);
        assert!(app.chat_ui.dictation.active.is_none());
        drop(app.dictation_event(Event::Cancelled { session: SESSION }));
        assert!(app.chat_ui.dictation.sessions.is_empty());
    }

    /// The microphone being on is never out of sight: the chat's tab says
    /// so while it shows, and the window says so, with a way back and a way
    /// to stop, while it does not. Stopping ends both.
    #[test]
    fn an_open_microphone_shows_from_anywhere_until_it_stops() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = listen(&mut app, "dictation", SESSION, "Fix", 3);
        let other = super::super::tests::live_chat(&mut app, Some("reading"));
        app.workspace.resize_viewport(1600.0);
        drop(app.workspace.focus(card));
        assert_eq!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        assert_eq!(app.dictation_microphone_in(card), Some(target));
        assert_eq!(app.dictation_microphone_in(other), None);
        assert_eq!(app.dictation_out_of_sight(), None);

        // Another chat that fills the window is read: the dictating one is
        // scrolled away, or no more than an edge of it shows.
        app.workspace.resize_viewport(500.0);
        for pane in [card, other] {
            drop(app.workspace.set_card_width(pane, CardWidth::Fraction(1.0)));
        }
        drop(app.workspace.focus(other));
        assert_eq!(app.dictation_out_of_sight(), Some(target));
        assert_eq!(app.dictation_microphone_in(card), Some(target));
        drop(app.workspace.focus(card));
        assert_eq!(app.dictation_out_of_sight(), None);
        // Its tab put away.
        drop(app.workspace.focus(other));
        drop(app.workspace.hide_card(card));
        assert_ne!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        assert_eq!(app.dictation_out_of_sight(), Some(target));
        assert_eq!(app.dictation_microphone_in(card), Some(target));

        drop(app.handle_chat_ui(Action::DictationStop(target)));
        assert_eq!(app.dictation_out_of_sight(), None);
        assert_eq!(app.dictation_microphone_in(card), None);
    }

    /// Leaving the window while holding the shortcut ends push-to-talk,
    /// since the release never comes, and keeps the words without sending
    /// them even when stopping sends.
    #[test]
    fn leaving_the_window_mid_push_to_talk_stops_without_sending() {
        let (mut app, _, target) = dictating("Fix the build", 13);
        app.settings.dictation_sends = true;
        app.chat_ui.dictation.held = Some((
            keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            std::time::Instant::now(),
        ));
        drop(app.dictation_unfocused());
        assert!(app.chat_ui.dictation.held.is_none());
        assert!(app.chat_ui.dictation.active.is_none());
        let session = app
            .chat_runtime_mut(target)
            .unwrap()
            .session_mut(SESSION)
            .unwrap();
        assert!(session.phase == Phase::Finishing && session.send.is_none());
        // Without the key held, losing the window leaves dictation going.
        let (mut app, card, _) = dictating("Fix the build", 13);
        drop(app.dictation_unfocused());
        assert!(super::super::tests::chat_runtime(&app, card).dictating());
    }
}
