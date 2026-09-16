use super::*;
use crate::agent::{Activity, Provider};
use crate::chat::client::{ClientHandle, Wake};
use crate::chat::{self, Attachment, Command, ItemContent, PromptMode, Snapshot};
use base64::Engine as _;
use cosmic::widget::{markdown, text_editor};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

mod step;
mod turn;
mod view;

fn action(action: Action) -> Message {
    Message::ChatUi(action)
}

/// One icon control on the chat surface.
///
/// Shared by the transcript and the composer so a control means the same
/// thing, and is the same size, wherever it appears.
fn icon<'a>(
    name: &'static str,
    label: impl Into<Cow<'a, str>>,
    enabled: bool,
    message: Action,
) -> Element<'a, Message> {
    crate::ui_controls::icon_control(
        widget::icon::from_name(name).handle(),
        label,
        false,
        enabled,
        Some(action(message)),
    )
    .into()
}

/// A message's own control, at caption scale.
///
/// An answer's footer sits directly under prose, so its controls are drawn
/// smaller than the composer's: at the primary size a single copy button
/// looked like a toolbar bolted to every answer.
fn small_icon<'a>(
    name: &'static str,
    label: impl Into<Cow<'a, str>>,
    message: Action,
) -> Element<'a, Message> {
    crate::ui_controls::compact_icon_control(
        crate::ui_controls::symbolic_icon(name),
        label,
        Some(action(message)),
    )
    .into()
}

/// Local wall-clock time for a transcript stamp, as `14:32`.
///
/// A transcript is read while the work is happening, so the hour and minute
/// are what a reader needs; a date would repeat itself on every row of a
/// conversation that lasts an afternoon. `localtime_r` is used because the
/// application carries no date library, and a hand-rolled offset would be
/// wrong across every daylight-saving boundary.
fn clock_label(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) as libc::time_t;
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `localtime_r` writes into the caller's `tm` and reads only the
    // time it is given, so it is sound for any `time_t` value.
    if unsafe { libc::localtime_r(&seconds, &mut parts) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}", parts.tm_hour, parts.tm_min)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Target {
    pub card: CardId,
    pub generation: u64,
}

#[derive(Clone)]
pub struct Ready(Arc<Mutex<Option<Result<Arc<ClientHandle>, String>>>>);
impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatReady")
    }
}

#[derive(Debug, Clone)]
pub enum CopyPart {
    Text,
    Input,
    Output,
    Code(usize),
}

#[derive(Clone)]
pub enum Action {
    Switch(CardId),
    BindingInput(CardId, String),
    BindingScanned(Target, Result<Vec<chat::BindingCandidate>, String>),
    BindingChoose(CardId, String),
    BindingPreview(CardId),
    BindingPreviewed(Target, Result<chat::BindingPreview, String>),
    BindingConfirm(CardId),
    /// Leaves the terminal alone and opens a new conversation in its
    /// directory, which is the only thing left when nothing attachable exists.
    BindingNew(CardId),
    BindingCancel(CardId),
    Ready(Target, Ready),
    Wake(Target, Wake),
    UpdatesAcknowledged(Target, Result<(), String>),
    Reconnect(Target),
    Composer(Target, text_editor::Action),
    Submit(Target, PromptMode),
    AppendProviderDraft(Target),
    Command(Target, Command),
    CommandFinished(Target, u64, Result<(), String>),
    Toggle(Target, String),
    /// Opens or closes a whole turn's step strip, addressed by the id of the
    /// turn's first item.
    ToggleTurn(Target, String),
    Copy(Target, String, CopyPart),
    OpenUrl(String),
    Attach(Target),
    Attached(Target, Result<Option<Vec<PathBuf>>, String>),
    RemoveAttachment(Target, usize),
    Model(Target, String),
    Reasoning(Target, String, String),
    PermissionMode(Target, String),
    Field(Target, String, String, String),
    Choice(Target, String, String, String),
    Answer(Target, String, String),
    Slash(Target, String),
    /// Names a provider skill in the draft rather than dispatching a command,
    /// because that is how a skill is invoked.
    Mention(Target, String),
    HandoffFinished(Target, Result<TerminalSession, String>),
    CloseFinished(Target, Result<(), String>),
    UrlFinished(Result<(), String>),
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // App-message diagnostics must never expose secret answers, composer
        // text, attachment paths, or provider authorization URLs.
        f.debug_tuple("ChatAction")
            .field(&std::mem::discriminant(self))
            .finish()
    }
}

impl Action {
    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(
            self,
            Self::Ready(..)
                | Self::Wake(..)
                | Self::UpdatesAcknowledged(..)
                | Self::CommandFinished(..)
                | Self::Attached(..)
                | Self::HandoffFinished(..)
                | Self::CloseFinished(..)
                | Self::UrlFinished(..)
                | Self::BindingPreviewed(..)
                | Self::BindingScanned(..)
        )
    }

    fn mutation_target(&self) -> Option<Target> {
        match self {
            Self::Composer(target, action) if action.is_edit() => Some(*target),
            Self::Reconnect(target)
            | Self::Submit(target, _)
            | Self::AppendProviderDraft(target)
            | Self::Command(target, _)
            | Self::Attach(target)
            | Self::Attached(target, _)
            | Self::RemoveAttachment(target, _)
            | Self::Model(target, _)
            | Self::Reasoning(target, _, _)
            | Self::PermissionMode(target, _)
            | Self::Field(target, _, _, _)
            | Self::Choice(target, _, _, _)
            | Self::Answer(target, _, _)
            | Self::Slash(target, _)
            | Self::Mention(target, _) => Some(*target),
            _ => None,
        }
    }
}

struct Attempt {
    target: Target,
    session: chat::Session,
}

struct Binding {
    target: Target,
    session: chat::Session,
    source: HostSessionProof,
    input: String,
    /// What the account's shared server is holding open, as the daemon last
    /// reported it. Listing is what turns this from a UUID prompt into a
    /// choice a reader can make.
    candidates: Vec<chat::BindingCandidate>,
    scanning: bool,
    /// Set when the listing resolved the conversation on its own, so the
    /// preview that follows is confirmed without asking the reader to approve
    /// a choice they never made.
    resolved: bool,
    preview: Option<chat::BindingPreview>,
    loading: bool,
    error: Option<String>,
}

pub(super) struct State {
    sender: async_channel::Sender<(Target, Wake)>,
    receiver: async_channel::Receiver<(Target, Wake)>,
    generation: u64,
    attempts: HashMap<CardId, Attempt>,
    bindings: HashMap<CardId, Binding>,
    handoffs: HashSet<Target>,
    closing: HashSet<Target>,
    closed: HashSet<CardId>,
    /// Unsent composer text belonging to a card whose chat runtime has been
    /// dropped.
    ///
    /// Returning a chat card to its terminal view and back used to discard
    /// whatever was typed, which is why leaving the surface at all had to be
    /// refused. Holding the draft here makes the round trip lossless, so the
    /// refusal is only needed where the process itself is about to restart.
    drafts: HashMap<CardId, String>,
}
impl Default for State {
    fn default() -> Self {
        let (sender, receiver) = async_channel::unbounded();
        Self {
            sender,
            receiver,
            generation: 0,
            attempts: HashMap::new(),
            handoffs: HashSet::new(),
            bindings: HashMap::new(),
            closing: HashSet::new(),
            closed: HashSet::new(),
            drafts: HashMap::new(),
        }
    }
}

impl State {
    pub(super) fn transition_pending(&self) -> bool {
        !self.attempts.is_empty()
            || !self.handoffs.is_empty()
            || !self.closing.is_empty()
            || !self.bindings.is_empty()
    }
}

/// FNV-1a over the last bytes of `text`, seeded with `seed` and the text's
/// byte length.
///
/// The digest is what makes a change token trustworthy without reading a whole
/// message: the length alone would miss a same-length rewrite of a tool's
/// output, and an append whose earlier bytes were rewritten would otherwise be
/// handed to the incremental parser as if it were a plain suffix.
fn tail_digest(seed: u64, text: &str) -> u64 {
    const WINDOW: usize = 64;
    let bytes = text.as_bytes();
    let mut hash = seed ^ text.len() as u64;
    for byte in &bytes[bytes.len().saturating_sub(WINDOW)..] {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A constant-size stand-in for an item's content.
///
/// The daemon coalesces presentation at 50 ms, so `Runtime::refresh` runs about
/// twenty times a second for the whole of a turn. Deciding whether an item
/// changed must therefore not touch its body: the window holds up to 128 items
/// and a long answer is megabytes. The content's shape, its field lengths, and
/// a bounded tail digest answer that question in constant time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChangeToken {
    /// The content variant plus the flags that sit beside its text.
    shape: u8,
    /// Byte length of the field that streams, so a pure append is recognized
    /// and its suffix located.
    body: usize,
    /// Digest over the tail of every field.
    digest: u64,
}

impl ChangeToken {
    /// Text's shape carries both flags that sit beside its body, because a
    /// message that turns from commentary into an answer must not be mistaken
    /// for an append of the same length.
    fn text_shape(thinking: bool, phase: chat::Phase) -> u8 {
        u8::from(thinking) | (u8::from(phase == chat::Phase::Commentary) << 1)
    }
    fn status_offset(status: chat::ToolStatus) -> u8 {
        match status {
            chat::ToolStatus::Running => 0,
            chat::ToolStatus::Succeeded => 1,
            chat::ToolStatus::Failed => 2,
            chat::ToolStatus::Denied => 3,
        }
    }
    /// Folds a number into a digest so a field that is not a string still
    /// changes the token. An exit code arriving on a command whose output is
    /// already complete is exactly that case.
    fn mix(digest: u64, value: u64) -> u64 {
        (digest ^ value).wrapping_mul(0x0000_0100_0000_01b3)
    }
    fn of(content: &ItemContent) -> Self {
        match content {
            ItemContent::Text {
                text,
                thinking,
                phase,
            } => Self {
                shape: Self::text_shape(*thinking, *phase),
                body: text.len(),
                digest: tail_digest(0, text),
            },
            ItemContent::Command {
                label,
                command,
                working_directory,
                action,
                status,
                exit_code,
                duration_ms,
                output,
            } => Self {
                shape: 4 + Self::status_offset(*status),
                body: output.len(),
                digest: Self::mix(
                    Self::mix(
                        tail_digest(
                            tail_digest(
                                tail_digest(tail_digest(0, label), command),
                                working_directory,
                            ),
                            output,
                        ),
                        exit_code.map_or(u64::MAX, |code| code as i64 as u64),
                    ),
                    duration_ms.unwrap_or(u64::MAX) ^ u64::from(*action as u8),
                ),
            },
            ItemContent::Changes { changes, status } => Self {
                shape: 8 + Self::status_offset(*status),
                body: changes.last().map_or(0, |change| change.diff.len()),
                digest: changes.iter().fold(
                    Self::mix(0, changes.len() as u64),
                    |digest, change| {
                        // The kind is folded in as its discriminant so an
                        // add that becomes a rename is a change even when
                        // the path and the patch are untouched, and a
                        // rename's source path with it, because the detail
                        // prints it and it is the one field of the variant
                        // that nothing else in this digest covers.
                        let digest = Self::mix(
                            tail_digest(tail_digest(digest, &change.path), &change.diff),
                            match &change.kind {
                                chat::ChangeKind::Added => 0,
                                chat::ChangeKind::Deleted => 1,
                                chat::ChangeKind::Updated => 2,
                                chat::ChangeKind::Renamed { .. } => 3,
                            },
                        );
                        match &change.kind {
                            chat::ChangeKind::Renamed { from } => tail_digest(digest, from),
                            _ => digest,
                        }
                    },
                ),
            },
            ItemContent::WebSearch { query, results } => Self {
                shape: 12,
                body: results.len(),
                digest: results
                    .iter()
                    .fold(tail_digest(0, query), |digest, result| {
                        tail_digest(
                            tail_digest(tail_digest(digest, &result.title), &result.url),
                            &result.snippet,
                        )
                    }),
            },
            ItemContent::Tool {
                name,
                input,
                output,
                status,
            } => Self {
                shape: 13 + Self::status_offset(*status),
                body: output.len(),
                digest: tail_digest(tail_digest(tail_digest(0, name), input), output),
            },
            ItemContent::Image { path, description } => Self {
                shape: 17,
                body: path.len(),
                digest: tail_digest(tail_digest(0, path), description),
            },
        }
    }
    /// The item's body when the only change since this token was taken is bytes
    /// appended to it, so the cached parse can be extended instead of redone.
    fn appended<'a>(&self, content: &'a ItemContent) -> Option<&'a str> {
        let ItemContent::Text {
            text,
            thinking,
            phase,
        } = content
        else {
            return None;
        };
        if self.shape != Self::text_shape(*thinking, *phase) || text.len() <= self.body {
            return None;
        }
        // `get` also rejects a rewrite that left the old length mid-character.
        let parsed = text.get(..self.body)?;
        (tail_digest(0, parsed) == self.digest).then_some(text.as_str())
    }
}

/// Fenced code blocks found in an item's body, scanned one line at a time so a
/// streamed body is never re-scanned from the top.
#[derive(Default)]
struct CodeScan {
    /// Bytes of the body already scanned; always just past a line ending.
    scanned: usize,
    /// The open fence's marker character and run length.
    fence: Option<(char, usize)>,
    /// Whether `codes` ends with the block the open fence is still filling.
    open: bool,
    /// Each fenced block's body, in the order the blocks appear.
    codes: Vec<String>,
}

impl CodeScan {
    /// Scans the complete lines of `body` that the cursor has not reached.
    ///
    /// A partial trailing line is left for the refresh that completes it: a
    /// fence is only a fence once its line ends, and a provisional scan would
    /// have to be undone.
    fn feed(&mut self, body: &str) {
        while let Some(index) = body[self.scanned..].find('\n') {
            let line = &body[self.scanned..self.scanned + index];
            self.scanned += index + 1;
            self.line(line.strip_suffix('\r').unwrap_or(line));
        }
    }
    fn line(&mut self, line: &str) {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        if matches!(marker, '`' | '~') {
            let length = trimmed
                .chars()
                .take_while(|character| *character == marker)
                .count();
            if length >= 3 {
                match self.fence {
                    None => {
                        self.fence = Some((marker, length));
                        return;
                    }
                    Some((opening, count)) if opening == marker && length >= count => {
                        if !self.open {
                            // An empty block still earns its copy control.
                            self.codes.push(String::new());
                        }
                        self.fence = None;
                        self.open = false;
                        return;
                    }
                    _ => {}
                }
            }
        }
        if self.fence.is_some() {
            if !self.open {
                self.codes.push(String::new());
                self.open = true;
            }
            if let Some(code) = self.codes.last_mut() {
                code.push_str(line);
                code.push('\n');
            }
        }
    }
}

/// A step's elapsed time at the precision a reader can act on.
///
/// Sub-second work is reported in milliseconds because the difference between
/// 4 ms and 400 ms is the difference between a cache hit and a process launch;
/// past a second the milliseconds stop carrying information.
fn format_duration(millis: u64) -> String {
    match millis {
        0..1_000 => format!("{millis}ms"),
        0..60_000 => format!("{:.1}s", millis as f64 / 1_000.0),
        _ => format!("{}m {}s", millis / 60_000, (millis % 60_000) / 1_000),
    }
}

fn status_word(status: chat::ToolStatus) -> &'static str {
    match status {
        chat::ToolStatus::Running => "Running",
        chat::ToolStatus::Succeeded => "Completed",
        chat::ToolStatus::Failed => "Failed",
        chat::ToolStatus::Denied => "Denied",
    }
}

/// What a copy control hands the reader.
///
/// Every step variant answers, because a control that silently does nothing is
/// worse than no control: a command's output and a tool's result are only
/// reachable through their step's detail, so this is the only way out of the
/// transcript for them. `Text` is the whole step as a reader would quote it;
/// `Input` and `Output` are the individual blocks their own headers offer.
fn copy_text(part: CopyPart, content: &ItemContent) -> Option<String> {
    fn joined(parts: &[&str]) -> Option<String> {
        let text = parts
            .iter()
            .copied()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        (!text.is_empty()).then_some(text)
    }
    match (part, content) {
        (CopyPart::Text, ItemContent::Text { text, .. }) => joined(&[text]),
        (
            CopyPart::Text,
            ItemContent::Command {
                command, output, ..
            },
        ) => joined(&[command, output]),
        (CopyPart::Text, ItemContent::Changes { changes, .. }) => joined(
            &changes
                .iter()
                .map(|change| change.diff.as_str())
                .collect::<Vec<_>>(),
        ),
        (CopyPart::Text, ItemContent::WebSearch { query, results }) => {
            let mut text = query.clone();
            for result in results {
                text.push_str("\n\n");
                text.push_str(&result.title);
                text.push('\n');
                text.push_str(&result.url);
            }
            Some(text)
        }
        (CopyPart::Text, ItemContent::Tool { input, output, .. }) => joined(&[input, output]),
        (CopyPart::Text, ItemContent::Image { path, description }) => joined(&[description, path]),
        // A command has no separate input block, so its header offers the
        // command line under the same name the tool detail uses for one.
        (CopyPart::Input, ItemContent::Command { command, .. }) => joined(&[command]),
        (CopyPart::Input, ItemContent::Tool { input, .. }) => joined(&[input]),
        (CopyPart::Output, ItemContent::Command { output, .. })
        | (CopyPart::Output, ItemContent::Tool { output, .. }) => joined(&[output]),
        // The code blocks of a Markdown answer are the cache's own, resolved
        // by the caller before it reaches this.
        (CopyPart::Code(_), _) | (CopyPart::Input | CopyPart::Output, _) => None,
    }
}

/// One transcript item's presentation, retained across refreshes.
///
/// The body is never copied in: what an item looked like when it was parsed is
/// recorded as a `ChangeToken`, and the authoritative text stays in the
/// snapshot the runtime already holds.
struct CachedItem {
    token: ChangeToken,
    markdown: markdown::Content,
    code: CodeScan,
    /// A step's trailing metadata — exit status and duration — formed once per
    /// change rather than once per frame.
    detail: Option<String>,
    image: Option<iced::widget::image::Handle>,
}
impl CachedItem {
    fn new(content: &ItemContent) -> Self {
        let mut cached = Self {
            token: ChangeToken::of(content),
            markdown: markdown::Content::new(),
            code: CodeScan::default(),
            detail: None,
            image: None,
        };
        match content {
            ItemContent::Text { text, .. } => cached.extend(text, 0),
            ItemContent::Command {
                status,
                exit_code,
                duration_ms,
                ..
            } => {
                // The label already says what ran, so this line carries only
                // what the label cannot: whether it worked and how long it
                // took. A zero exit code is the uninteresting case and is left
                // out so a failure stands out.
                let mut detail = String::new();
                match exit_code {
                    Some(0) | None => {}
                    Some(code) => detail.push_str(&format!("exit {code}")),
                }
                if let Some(elapsed) = duration_ms {
                    if !detail.is_empty() {
                        detail.push_str(" · ");
                    }
                    detail.push_str(&format_duration(*elapsed));
                }
                if detail.is_empty() {
                    detail.push_str(status_word(*status));
                }
                cached.detail = Some(detail);
            }
            ItemContent::Changes { changes, status } => {
                cached.detail = Some(match changes.len() {
                    0 | 1 => status_word(*status).to_owned(),
                    count => format!("{count} files"),
                });
            }
            ItemContent::WebSearch { results, .. } => {
                cached.detail = Some(match results.len() {
                    1 => "1 result".to_owned(),
                    count => format!("{count} results"),
                });
            }
            ItemContent::Tool { status, .. } => {
                cached.detail = Some(status_word(*status).to_owned());
            }
            ItemContent::Image { path, .. } => {
                cached.image = match path {
                    path if Path::new(path).is_absolute() => {
                        Some(iced::widget::image::Handle::from_path(path))
                    }
                    path if path.len() <= 24 * 1024 * 1024 && path.starts_with("data:image/") => {
                        path.split_once(";base64,")
                            .and_then(|(_, encoded)| {
                                base64::engine::general_purpose::STANDARD
                                    .decode(encoded)
                                    .ok()
                            })
                            .map(iced::widget::image::Handle::from_bytes)
                    }
                    _ => None,
                };
            }
        }
        cached
    }
    /// Parses the part of `body` past `parsed`, the bytes already seen.
    ///
    /// `markdown::Content` re-parses only its trailing block, and the fence
    /// scan resumes from its own cursor, so a streamed answer costs its new
    /// bytes per frame instead of its whole length.
    fn extend(&mut self, body: &str, parsed: usize) {
        self.markdown.push_str(&body[parsed..]);
        self.code.feed(body);
    }
}

struct PendingCommand {
    id: u64,
    draft_revision: Option<u64>,
    request: Option<String>,
    permission_mode: Option<String>,
}
pub(super) struct Runtime {
    pub(super) target: Target,
    pub(super) session: chat::Session,
    handle: Arc<ClientHandle>,
    snapshot: Arc<Snapshot>,
    cache: HashMap<String, CachedItem>,
    /// The snapshot's items grouped into turns, rebuilt only when the snapshot
    /// changes rather than once per frame.
    turns: Vec<turn::Turn>,
    /// The reader's explicit choice to open or close a turn's step strip,
    /// keyed by the turn's first item id.
    ///
    /// An override rather than a set, because the useful default is not
    /// constant: a running turn shows its steps, because watching them is the
    /// point, and a finished turn hides them, because the answer is. Only a
    /// deliberate toggle is recorded here.
    opened: HashMap<String, bool>,
    composer: text_editor::Content<cosmic::Renderer>,
    revision: u64,
    attachments: Vec<Attachment>,
    attachment_chooser_pending: bool,
    expanded: HashSet<String>,
    answers: HashMap<(String, String), Vec<String>>,
    pending: Option<PendingCommand>,
    next_command: u64,
    disconnected: bool,
    visible: AtomicBool,
    error: Option<String>,
    permission_mode: Option<String>,
}
impl Runtime {
    fn new(
        target: Target,
        session: chat::Session,
        handle: Arc<ClientHandle>,
        visible: bool,
    ) -> Self {
        let mut runtime = Self {
            target,
            session,
            snapshot: Arc::new(Snapshot::default()),
            handle,
            cache: HashMap::new(),
            turns: Vec::new(),
            opened: HashMap::new(),
            composer: text_editor::Content::new(),
            revision: 0,
            attachments: Vec::new(),
            attachment_chooser_pending: false,
            expanded: HashSet::new(),
            answers: HashMap::new(),
            pending: None,
            next_command: 0,
            disconnected: false,
            visible: AtomicBool::new(visible),
            error: None,
            permission_mode: None,
        };
        runtime.refresh();
        runtime
    }
    fn refresh(&mut self) {
        if !self.visible.load(Ordering::Relaxed) {
            return;
        }
        let snapshot = self.handle.snapshot();
        if self.snapshot.generation == snapshot.generation && Arc::ptr_eq(&self.snapshot, &snapshot)
        {
            return;
        }
        let live_ids: HashSet<&str> = snapshot.items.iter().map(|item| item.id.as_str()).collect();
        self.cache.retain(|id, _| live_ids.contains(id.as_str()));
        // The disclosure sets are keyed by item id and the store drops items
        // past its window, so without this they grow for the life of the card:
        // one entry per step the reader ever opened, held after the step it
        // named stopped existing.
        self.expanded.retain(|id| live_ids.contains(id.as_str()));
        self.opened.retain(|id, _| live_ids.contains(id.as_str()));
        for item in &snapshot.items {
            let token = ChangeToken::of(&item.content);
            if let Some(cached) = self.cache.get_mut(&item.id) {
                if cached.token == token {
                    continue;
                }
                if let Some(body) = cached.token.appended(&item.content) {
                    let parsed = cached.token.body;
                    cached.extend(body, parsed);
                    cached.token = token;
                    continue;
                }
            }
            self.cache
                .insert(item.id.clone(), CachedItem::new(&item.content));
        }
        self.answers
            .retain(|(id, _), _| snapshot.requests.iter().any(|request| &request.id == id));
        for request in &snapshot.requests {
            for field in &request.fields {
                self.answers
                    .entry((request.id.clone(), field.id.clone()))
                    .or_insert_with(|| {
                        if field.initial.is_empty() {
                            Vec::new()
                        } else {
                            vec![field.initial.clone()]
                        }
                    });
            }
        }
        if snapshot.composer != self.snapshot.composer
            && self.composer.text().trim().is_empty()
            && let Some(text) = &snapshot.composer
        {
            self.composer = text_editor::Content::with_text(text);
            self.revision = self.revision.wrapping_add(1);
        }
        if snapshot.session_id.is_some() || self.snapshot.session_id.is_some() {
            self.session.launch.session_id = snapshot.session_id.clone();
        }
        self.turns = turn::group(&snapshot.items);
        self.snapshot = snapshot;
    }
    pub(super) fn set_visible(&self, visible: bool) -> Result<(), String> {
        self.visible.store(visible, Ordering::Relaxed);
        self.handle.set_visible(visible)
    }
    fn show(&mut self, visible: bool) {
        if let Err(error) = self.set_visible(visible) {
            self.error = Some(error);
        }
        if visible {
            self.refresh();
        }
    }
    /// Whether the turn keyed by `key` still has a step in flight, which is
    /// the default disclosure state a toggle inverts.
    fn turn_running(&self, key: &str) -> bool {
        self.turns
            .iter()
            .find(|turn| turn.key == key)
            .is_some_and(|turn| turn.running)
    }

    /// Why this chat cannot be swapped for its terminal view right now.
    ///
    /// An unsent draft is deliberately absent: `Cagents::chat_forget` stashes
    /// it and a returning runtime restores it, so typing a single character no
    /// longer makes the card impossible to leave. Attachments and half-entered
    /// request answers still block, because those are references and decisions
    /// that cannot be reconstructed from text.
    pub(super) fn transition_blocker(&self) -> Option<&'static str> {
        if self.attachment_chooser_pending {
            Some("Finish or cancel the file chooser before leaving the chat.")
        } else if self.pending.is_some() {
            Some("Wait for the pending chat command to finish before leaving the chat.")
        } else if !self.attachments.is_empty() {
            Some("Send or remove the attached files before leaving the chat.")
        } else if self
            .answers
            .values()
            .any(|answers| answers.iter().any(|answer| !answer.is_empty()))
        {
            Some("Submit or clear the answer you started before leaving the chat.")
        } else {
            None
        }
    }
    /// Why this chat cannot survive the process restarting.
    ///
    /// The draft stash lives in memory, so an update is the one transition
    /// that really does discard unsent text.
    pub(super) fn restart_blocker(&self) -> Option<&'static str> {
        self.transition_blocker().or_else(|| {
            (!self.composer.is_empty()).then_some(
                "Send or clear the unsent chat message before updating; restarting discards it.",
            )
        })
    }
}

#[derive(Clone)]
struct Source(async_channel::Receiver<(Target, Wake)>);
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
    }
}
fn stream(source: &Source) -> Pin<Box<dyn iced::futures::Stream<Item = (Target, Wake)> + Send>> {
    Box::pin(iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|wake| (wake, receiver)) },
    ))
}

/// The tab title a native chat starts with.
///
/// It names the provider because three providers now front a native chat and
/// a bare "Native chat" left the reader unable to tell which agent a tab was
/// talking to.
pub(super) fn chat_card_title(provider: Provider) -> &'static str {
    match provider {
        Provider::Omp => "OMP chat",
        Provider::Claude => "Claude chat",
        Provider::Codex => "Codex chat",
    }
}

/// Whether a card title is still the one the application chose for it.
///
/// A channel takes its name from a conversation that has named itself, so
/// every title the application assigns has to be recognized here or a channel
/// would be named after the agent rather than the work.
pub(super) fn placeholder_card_title(title: &str) -> bool {
    let title = title.trim();
    matches!(title, "" | "Native chat" | "Agent" | "Terminal")
        || Provider::ALL
            .iter()
            .any(|provider| title == chat_card_title(*provider))
}

fn observed_launch(session: &TerminalSession) -> Option<LaunchRecipe> {
    session
        .resume_hint
        .as_deref()
        .and_then(crate::model::agent_recovery::observed_launch_from_hint)
        .or_else(|| Provider::from_program(&session.launch.program).map(|_| session.launch.clone()))
}

pub(super) fn conversion(session: &TerminalSession) -> Result<chat::Session, String> {
    let recipe = session
        .agent_resume_recipe()
        .or_else(|| observed_launch(session))
        .ok_or_else(|| {
            "No supported foreground agent is known. Focus the running agent terminal and retry."
                .to_owned()
        })?;
    let provider = Provider::from_program(&recipe.program)
        .ok_or("This terminal is not a supported agent session.")?;
    let identity_flag = provider.resume_flag();
    let session_id = session.agent_resume_recipe().and_then(|recipe| {
        recipe
            .arguments
            .windows(2)
            .find(|pair| pair[0] == identity_flag)
            .map(|pair| pair[1].clone())
    });
    Ok(chat::Session {
        id: session.id,
        launch: chat::Launch {
            provider,
            recipe,
            session_id,
        },
        proof: None,
        terminal: Some(session.clone()),
    })
}

fn return_terminal(session: &chat::Session) -> Result<TerminalSession, String> {
    session.launch.session_id.as_ref().ok_or_else(|| "Waiting for the provider's exact conversation identity. Return is unavailable until it is known.".to_owned())?;
    if session.launch.provider == Provider::Codex
        && let Some(source) = &session.terminal
        && source.continuity.live_proof().is_some()
    {
        return Ok(source.clone());
    }
    let recipe = session
        .launch
        .terminal_recipe()
        .ok_or_else(|| "Cannot form a safe exact-session terminal resume command.".to_owned())?;
    let id = TerminalId::new();
    let mut terminal = session.terminal.clone().unwrap_or_else(|| TerminalSession {
        id,
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: recipe.clone(),
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
    });
    terminal.id = id;
    terminal.launch = recipe.clone();
    terminal.resume_hint = crate::model::agent_recovery::hint_from_launch(&recipe);
    terminal.continuity = TerminalContinuity::RelaunchAvailable {
        recipe,
        last_exit: None,
    };
    Ok(terminal)
}

impl Cagents {
    /// The provider a new channel's conversation should use.
    ///
    /// The menu names the provider explicitly, but a channel is created from
    /// the sidebar with no such choice, so it follows the most recent
    /// conversation in the workspace. That is state the workspace already
    /// persists, so the answer survives a restart without a second setting
    /// for the same decision.
    pub(super) fn preferred_chat_provider(&self) -> Provider {
        self.durable
            .cards
            .iter()
            .rev()
            .find_map(|card| match &card.kind {
                CardKind::Chat(session) => Some(session.launch.provider),
                _ => None,
            })
            .unwrap_or(Provider::Codex)
    }

    pub(super) fn new_native_chat(&mut self, provider: Provider) -> Task<Message> {
        if self.project_ui.deleting() {
            return Task::none();
        }
        let Some(working_directory) = self.terminal_directory() else {
            return cosmic::task::message(Message::OpenRepository);
        };
        let mut recipe = LaunchRecipe {
            program: provider.executable().into(),
            arguments: Vec::new(),
            working_directory: working_directory.clone(),
            environment: crate::model::agent_recovery::fresh_native_environment(provider, true),
        };
        if provider == Provider::Omp {
            let Some(root) = provider.session_root(&recipe) else {
                self.status = Some(TransientNotice::error(
                    "Cannot locate the configured OMP account for a new native chat.",
                ));
                return Task::none();
            };
            // An explicit session directory bypasses OMP's auto-resume setting.
            // Never let a new tab select an established conversation implicitly.
            recipe.arguments.extend([
                "--session-dir".into(),
                root.join("sessions")
                    .join("native")
                    .to_string_lossy()
                    .into_owned(),
            ]);
        }
        let session = chat::Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider,
                recipe,
                session_id: None,
            },
            proof: None,
            terminal: None,
        };
        let card_id = CardId::new();
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: chat_card_title(provider).into(),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hydrating { epoch: 1 },
            kind: CardKind::Chat(session.clone()),
        };
        let scope = if self.active_workspace.as_deref() != Some(working_directory.as_path()) {
            self.select_worktree(working_directory, false)
        } else {
            Task::none()
        };
        let (index, durable_index) = self.terminal_insertion_indices();
        let durable = Arc::make_mut(&mut self.durable);
        durable.focused = Some(card_id);
        durable.cards.insert(durable_index, card.clone());
        self.register_channel_card(card_id);
        let update = self
            .workspace
            .add_card(
                WorkspaceCard::new(
                    card_id,
                    card.title,
                    CardWidth::Fraction(card.width_fraction),
                )
                .expect("new chat card is valid"),
                index,
                true,
            )
            .unwrap_or_else(|error| {
                self.status = Some(TransientNotice::error(error.to_string()));
                ViewportUpdate::unchanged()
            });
        self.cards.insert(card_id, CardRuntime::Loading);
        self.lifecycle.register(
            card_id,
            Exposure::Visible,
            Residency::Hydrating { epoch: 1 },
            Instant::now(),
        );
        self.persist();
        Task::batch([
            scope,
            self.start_chat(card_id, session, true, None, false),
            self.apply_viewport(update, true, false),
        ])
    }

    pub(super) fn chat_subscription(&self) -> Subscription<Message> {
        Subscription::run_with(Source(self.chat_ui.receiver.clone()), stream)
            .map(|(target, wake)| Message::ChatUi(Action::Wake(target, wake)))
    }
    pub(super) fn chat_forget(&mut self, card: CardId) {
        // The runtime is about to be dropped, so its unsent text is captured
        // here rather than lost. `chat_forget` is the single place a chat
        // runtime stops existing while its card lives on.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) {
            let draft = runtime.composer.text();
            if draft.trim().is_empty() {
                self.chat_ui.drafts.remove(&card);
            } else {
                self.chat_ui.drafts.insert(card, draft);
            }
        }
        self.chat_ui.attempts.remove(&card);
        self.chat_ui.bindings.remove(&card);
        self.chat_ui.handoffs.retain(|target| target.card != card);
        self.chat_ui.closing.retain(|target| target.card != card);
        self.observe_agent_activity_for_card(card, None);
    }
    /// Forgets a chat runtime whose card is going away for good.
    ///
    /// The difference from `chat_forget` is the draft: a card that is about to
    /// be tombstoned will never build a runtime under that id again, so
    /// stashing its unsent text would pin the string for the lifetime of the
    /// process with nothing able to read it back.
    pub(super) fn chat_discard(&mut self, card: CardId) {
        self.chat_forget(card);
        self.chat_ui.drafts.remove(&card);
    }
    pub(super) fn chat_start_pending(&self, card: CardId) -> bool {
        self.chat_ui.attempts.contains_key(&card)
            || self
                .chat_ui
                .handoffs
                .iter()
                .any(|target| target.card == card)
            || self
                .chat_ui
                .closing
                .iter()
                .any(|target| target.card == card)
    }
    pub(super) fn chat_switch_available(&self, card: CardId) -> bool {
        match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(_)) => true,
            Some(CardKind::Terminal(session)) => {
                session.agent_resume_recipe().is_some() || observed_launch(session).is_some()
            }
            _ => false,
        }
    }
    pub(super) fn chat_switch_label(&self, card: CardId) -> &'static str {
        if matches!(
            self.card_state(card).map(|state| &state.kind),
            Some(CardKind::Chat(_))
        ) {
            "Return to terminal"
        } else {
            "Switch to native chat"
        }
    }
    pub(super) fn chat_close_approved(&self, card: CardId) -> bool {
        self.chat_ui.closed.contains(&card)
    }
    pub(super) fn begin_chat_close(&mut self, card: CardId) -> Task<Message> {
        if self.chat_start_pending(card) {
            return Task::none();
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        if runtime.attachment_chooser_pending {
            self.status = Some(TransientNotice::error(
                "Finish or cancel the file chooser before closing the chat.",
            ));
            return Task::none();
        }
        let target = runtime.target;
        let handle = runtime.handle.clone();
        self.chat_ui.closing.insert(target);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.close())
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::CloseFinished(target, result))
        })
    }
    pub(super) fn start_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
        spawn: bool,
        source: Option<HostSessionProof>,
        terminate_source: bool,
    ) -> Task<Message> {
        self.chat_ui.generation = self.chat_ui.generation.wrapping_add(1).max(1);
        let selection = self
            .chat_ui
            .bindings
            .remove(&card)
            .and_then(|binding| binding.preview)
            .filter(|preview| session.launch.session_id.as_ref() == Some(&preview.session_id))
            .map(|preview| preview.token);
        let target = Target {
            card,
            generation: self.chat_ui.generation,
        };
        self.chat_ui.attempts.insert(
            card,
            Attempt {
                target,
                session: session.clone(),
            },
        );
        let sender = self.chat_ui.sender.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                ClientHandle::connect(
                    session,
                    spawn,
                    source,
                    terminate_source,
                    selection,
                    move |wake| {
                        let _ = sender.try_send((target, wake));
                    },
                )
                .map(Arc::new)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ChatUi(Action::Ready(
                target,
                Ready(Arc::new(Mutex::new(Some(result)))),
            ))
        })
    }
    pub(super) fn acknowledge_chat_updates(&mut self, card: CardId) -> Task<Message> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        if !self
            .card_state(card)
            .is_some_and(|state| state.existence.is_open())
        {
            return Task::none();
        }
        let target = runtime.target;
        let handle = runtime.handle.clone();
        let epoch = handle.begin_acknowledgement();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.acknowledge_updates(epoch))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::UpdatesAcknowledged(target, result))
        })
    }
    fn chat_runtime_mut(&mut self, target: Target) -> Option<&mut Runtime> {
        match self.cards.get_mut(&target.card) {
            Some(CardRuntime::Chat(runtime)) if runtime.target == target => Some(runtime),
            _ => None,
        }
    }
    fn chat_send(&mut self, target: Target, command: Command, clear_draft: bool) -> Task<Message> {
        if self.chat_start_pending(target.card) {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.pending.is_some() || runtime.disconnected {
            return Task::none();
        }
        let request = if let Command::Answer {
            request_id, action, ..
        } = &command
        {
            if !runtime.snapshot.requests.iter().any(|request| {
                &request.id == request_id
                    && request.actions.iter().any(|choice| &choice.id == action)
            }) {
                return Task::none();
            }
            Some(request_id.clone())
        } else {
            None
        };
        runtime.next_command = runtime.next_command.wrapping_add(1);
        let id = runtime.next_command;
        let permission_mode = if let Command::SetPermissionMode { mode } = &command {
            Some(mode.clone())
        } else {
            None
        };
        runtime.pending = Some(PendingCommand {
            id,
            draft_revision: clear_draft.then_some(runtime.revision),
            request,
            permission_mode,
        });
        runtime.error = None;
        let handle = runtime.handle.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.send(command))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::CommandFinished(target, id, result))
        })
    }
    pub(super) fn handle_chat_ui(&mut self, action: Action) -> Task<Message> {
        if let Some(target) = action.mutation_target()
            && (self.chat_start_pending(target.card) || self.chat_ui.closed.contains(&target.card))
        {
            return Task::none();
        }
        match action {
            Action::BindingInput(card, input) => {
                if let Some(binding) = self.chat_ui.bindings.get_mut(&card) {
                    binding.input = input;
                    binding.preview = None;
                    binding.error = None;
                    binding.loading = false;
                    self.chat_ui.generation = self.chat_ui.generation.wrapping_add(1).max(1);
                    binding.target.generation = self.chat_ui.generation;
                }
            }
            Action::BindingScanned(target, result) => {
                let Some(binding) = self
                    .chat_ui
                    .bindings
                    .get_mut(&target.card)
                    .filter(|binding| binding.target == target)
                else {
                    return Task::none();
                };
                binding.scanning = false;
                let candidates = match result {
                    Ok(candidates) => candidates,
                    Err(error) => {
                        binding.error = Some(error);
                        return Task::none();
                    }
                };
                binding.candidates = candidates;
                // A terminal is looking at a conversation rooted in its own
                // directory. Where exactly one of the account's open
                // conversations is, there is nothing for the reader to
                // disambiguate, so the choice is made for them and still goes
                // through the daemon's own preview and confirmation.
                let directory = binding
                    .session
                    .terminal
                    .as_ref()
                    .map(|terminal| terminal.launch.working_directory.clone());
                let mut here = binding.candidates.iter().filter(|candidate| {
                    directory
                        .as_ref()
                        .is_some_and(|directory| &candidate.working_directory == directory)
                });
                let resolved = match (here.next(), here.next()) {
                    (Some(only), None) => Some(only.session_id.clone()),
                    _ => None,
                };
                if let Some(id) = resolved {
                    binding.input = id;
                    binding.resolved = true;
                    return self.handle_chat_ui(Action::BindingPreview(target.card));
                }
            }
            Action::BindingChoose(card, id) => {
                let Some(binding) = self.chat_ui.bindings.get_mut(&card) else {
                    return Task::none();
                };
                binding.input = id;
                binding.preview = None;
                binding.error = None;
                binding.loading = false;
                binding.resolved = false;
                self.chat_ui.generation = self.chat_ui.generation.wrapping_add(1).max(1);
                if let Some(binding) = self.chat_ui.bindings.get_mut(&card) {
                    binding.target.generation = self.chat_ui.generation;
                }
                return self.handle_chat_ui(Action::BindingPreview(card));
            }
            Action::BindingPreview(card) => {
                let Some(binding) = self.chat_ui.bindings.get_mut(&card) else {
                    return Task::none();
                };
                if binding.loading {
                    return Task::none();
                }
                let id = binding.input.trim();
                if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
                    binding.error = Some("Enter the full conversation UUID shown by /status in the intended Codex CLI.".into());
                    return Task::none();
                }
                binding.session.launch.session_id = Some(id.to_owned());
                binding.preview = None;
                binding.error = None;
                binding.loading = true;
                let target = binding.target;
                let session = binding.session.clone();
                let source = binding.source.clone();
                return cosmic::task::future(async move {
                    let result =
                        tokio::task::spawn_blocking(move || ClientHandle::preview(session, source))
                            .await
                            .map_err(|error| error.to_string())
                            .and_then(|result| result);
                    Message::ChatUi(Action::BindingPreviewed(target, result))
                });
            }
            Action::BindingPreviewed(target, result) => {
                let Some(binding) = self
                    .chat_ui
                    .bindings
                    .get_mut(&target.card)
                    .filter(|binding| binding.target == target)
                else {
                    return Task::none();
                };
                binding.loading = false;
                match result {
                    Ok(preview)
                        if binding.session.launch.session_id.as_ref()
                            == Some(&preview.session_id) =>
                    {
                        binding.preview = Some(preview)
                    }
                    Ok(_) => binding.error = Some(
                        "The preview returned a different identity; no conversation was activated."
                            .into(),
                    ),
                    Err(error) => binding.error = Some(error),
                }
                if binding.resolved && binding.preview.is_some() {
                    // The reader asked for this terminal's conversation and
                    // the account had exactly one here, so there is no choice
                    // left to present. Stopping to ask them to approve the
                    // only answer is the prompt this replaced.
                    return self.handle_chat_ui(Action::BindingConfirm(target.card));
                }
            }
            Action::BindingConfirm(card) => {
                let Some(binding) = self
                    .chat_ui
                    .bindings
                    .get(&card)
                    .filter(|binding| binding.preview.is_some() && !binding.loading)
                else {
                    return Task::none();
                };
                if !self.card_state(card).is_some_and(|state| matches!(&state.kind, CardKind::Terminal(terminal) if terminal.id == binding.session.id && terminal.continuity.live_proof().as_ref() == Some(&binding.source))) {
                    self.chat_ui.bindings.remove(&card);
                    self.status = Some(TransientNotice::error("The source terminal changed; choose its conversation again."));
                    return Task::none();
                }
                let session = binding.session.clone();
                let source = binding.source.clone();
                return self.start_chat(card, session, true, Some(source), false);
            }
            Action::BindingNew(card) => {
                let Some(binding) = self.chat_ui.bindings.remove(&card) else {
                    return Task::none();
                };
                // The source terminal keeps running and keeps its own
                // conversation; this opens a second card beside it rather than
                // taking anything over.
                return self.new_native_chat(binding.session.launch.provider);
            }
            Action::BindingCancel(card) => {
                self.chat_ui.bindings.remove(&card);
            }
            Action::Switch(card) => {
                if self.chat_start_pending(card) {
                    return Task::none();
                }
                match self.card_state(card).map(|state| &state.kind) {
                    Some(CardKind::Terminal(terminal)) => {
                        let source = terminal.continuity.live_proof();
                        let mut session = match conversion(terminal) {
                            Ok(session) => session,
                            Err(error) => {
                                self.status = Some(TransientNotice::error(error));
                                return Task::none();
                            }
                        };
                        if source.is_none() {
                            // Nothing is running, so there is nothing to take
                            // over and no handoff to authorize: the same
                            // configuration opens a new conversation natively.
                            // Refusing here rejected the one case that needs no
                            // live process, which left a stopped agent terminal
                            // with no way into the chat at all. The stopped
                            // thread is deliberately not resumed: joining an
                            // existing thread means joining whichever process
                            // holds it, and no process holds this one.
                            if let Some(recipe) =
                                crate::model::agent_recovery::fresh_recipe(&session.launch.recipe)
                            {
                                session.launch.recipe = recipe;
                            }
                            session.launch.session_id = None;
                            self.status = Some(TransientNotice::warning(
                                "This terminal's agent is not running, so a new conversation was opened here.",
                            ));
                            return self.start_chat(card, session, true, None, false);
                        }
                        if session.launch.provider == Provider::Codex {
                            self.chat_ui.generation =
                                self.chat_ui.generation.wrapping_add(1).max(1);
                            let target = Target {
                                card,
                                generation: self.chat_ui.generation,
                            };
                            let input = session.launch.session_id.clone().unwrap_or_default();
                            let source = source.expect("checked source");
                            self.chat_ui.bindings.insert(
                                card,
                                Binding {
                                    target,
                                    session: session.clone(),
                                    source: source.clone(),
                                    input,
                                    candidates: Vec::new(),
                                    scanning: true,
                                    resolved: false,
                                    preview: None,
                                    loading: false,
                                    error: None,
                                },
                            );
                            // The account's shared server knows which
                            // conversations it is holding open, so the reader
                            // is asked to choose from those rather than to
                            // copy an identifier out of another program.
                            let scan = cosmic::task::future(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    ClientHandle::candidates(session, source)
                                })
                                .await
                                .map_err(|error| error.to_string())
                                .and_then(|result| result);
                                Message::ChatUi(Action::BindingScanned(target, result))
                            });
                            return match self.workspace.focus(card) {
                                Ok(update) => {
                                    Task::batch([self.apply_viewport(update, true, false), scan])
                                }
                                Err(error) => {
                                    self.status = Some(TransientNotice::error(error.to_string()));
                                    Task::none()
                                }
                            };
                        }
                        let terminate = session.launch.provider != Provider::Codex;
                        return self.start_chat(card, session, true, source, terminate);
                    }
                    Some(CardKind::Chat(_)) => {
                        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
                            return Task::none();
                        };
                        if let Some(reason) = runtime.transition_blocker() {
                            self.status = Some(TransientNotice::error(reason));
                            return Task::none();
                        }
                        let mut session = runtime.session.clone();
                        session.launch.session_id = runtime.handle.snapshot().session_id.clone();
                        let terminal = match return_terminal(&session) {
                            Ok(terminal) => terminal,
                            Err(error) => {
                                self.status = Some(TransientNotice::error(error));
                                return Task::none();
                            }
                        };
                        let target = runtime.target;
                        let handle = runtime.handle.clone();
                        let detach = session.launch.provider == Provider::Codex
                            && session
                                .terminal
                                .as_ref()
                                .is_some_and(|source| source.id == terminal.id);
                        self.chat_ui.handoffs.insert(target);
                        return cosmic::task::future(async move {
                            let result = if detach {
                                Ok(terminal)
                            } else {
                                tokio::task::spawn_blocking(move || handle.return_terminal())
                                    .await
                                    .map_err(|error| error.to_string())
                                    .and_then(|result| result)
                            };
                            Message::ChatUi(Action::HandoffFinished(target, result))
                        });
                    }
                    _ => return Task::none(),
                }
            }
            Action::Ready(target, ready) => {
                let result = ready.0.lock().ok().and_then(|mut ready| ready.take());
                let Some(result) = result else {
                    return Task::none();
                };
                if !self
                    .chat_ui
                    .attempts
                    .get(&target.card)
                    .is_some_and(|attempt| attempt.target == target)
                {
                    return Task::none();
                }
                let attempt = self.chat_ui.attempts.remove(&target.card).unwrap();
                if !self.card_state(target.card).is_some_and(|card| {
                    card.existence.is_open()
                        && match &card.kind {
                            CardKind::Chat(session) => session.id == attempt.session.id,
                            CardKind::Terminal(session) => {
                                attempt.session.terminal.as_ref().is_some_and(|source| {
                                    source.id == session.id && source.launch == session.launch
                                })
                            }
                            _ => false,
                        }
                }) {
                    return Task::none();
                }
                match result {
                    Ok(handle) => {
                        let mut session = handle.session();
                        session.proof = handle.proof();
                        let title = handle.snapshot().title.clone();
                        let activity = Activity {
                            provider: session.launch.provider,
                            state: handle.take_activity(),
                        };
                        handle.take_observed();
                        let visible = self.workspace.visibility(target.card)
                            == Some(VisibilityClass::Visible);
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            runtime.target = target;
                            runtime.handle = handle;
                            runtime.session = session.clone();
                            runtime.disconnected = false;
                            runtime.pending = None;
                            runtime.error = None;
                            runtime.show(visible);
                        } else {
                            let mut runtime =
                                Runtime::new(target, session.clone(), handle, visible);
                            // Whatever was typed before this card last left the
                            // chat surface comes back with it.
                            if let Some(draft) = self.chat_ui.drafts.remove(&target.card)
                                && !draft.is_empty()
                            {
                                runtime.composer = text_editor::Content::with_text(&draft);
                                runtime.revision = runtime.revision.wrapping_add(1);
                            }
                            runtime.show(visible);
                            if let Some(CardRuntime::Terminal(terminal)) = self
                                .cards
                                .insert(target.card, CardRuntime::Chat(Box::new(runtime)))
                            {
                                self.begin_terminal_teardown(terminal);
                            }
                        }
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) {
                            session = runtime.session.clone();
                        }
                        self.commit_chat_session(target.card, session);
                        if let Some(title) = title
                            && self.apply_reported_title(target.card, title)
                        {
                            self.persist();
                        }
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        return iced::widget::operation::focus(card_control_id(
                            target.card,
                            "chat-composer",
                        ))
                        .map(cosmic::Action::App);
                    }
                    Err(error) => {
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            runtime.error = Some(error);
                            runtime.disconnected = true;
                            self.observe_agent_activity_for_card(target.card, None);
                        } else if matches!(
                            self.card_state(target.card).map(|state| &state.kind),
                            Some(CardKind::Chat(_))
                        ) {
                            self.fail_runtime(target.card, error.clone());
                            self.status = Some(TransientNotice::error(error));
                        } else {
                            // Failed recognition/attachment must never replace or
                            // tear down the user's still-running terminal.
                            self.status = Some(TransientNotice::error(error));
                        }
                        return self.drain_terminal(target.card, 0);
                    }
                }
            }
            Action::UpdatesAcknowledged(target, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Err(error) = result
                {
                    runtime.error = Some(error);
                }
            }
            Action::Wake(target, wake) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                match wake {
                    Wake::Changed => {
                        runtime.refresh();
                        let activity = Activity {
                            provider: runtime.session.launch.provider,
                            state: runtime.handle.take_activity(),
                        };
                        let session = runtime.session.clone();
                        let title = runtime.handle.snapshot().title.clone();
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        self.commit_chat_session(target.card, session);
                        if let Some(title) = title
                            && self.apply_reported_title(target.card, title)
                        {
                            self.persist();
                        }
                        return Task::none();
                    }
                    Wake::Observed => {
                        runtime.handle.take_observed();
                        return Task::none();
                    }
                    Wake::Activity => {
                        let activity = Activity {
                            provider: runtime.session.launch.provider,
                            state: runtime.handle.take_activity(),
                        };
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        return Task::none();
                    }
                    Wake::Disconnected(error) => {
                        runtime.disconnected = true;
                        runtime.error = Some(error);
                    }
                }
                let session = runtime.session.clone();
                self.commit_chat_session(target.card, session);
                self.observe_agent_activity_for_card(target.card, None);
            }
            Action::Reconnect(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.attachment_chooser_pending {
                    runtime.error = Some(
                        "Finish or cancel the file chooser before reconnecting the chat.".into(),
                    );
                    return Task::none();
                }
                let session = runtime.session.clone();
                return self.start_chat(target.card, session, false, None, false);
            }
            Action::Composer(target, action) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if action.is_edit() {
                        runtime.revision = runtime.revision.wrapping_add(1);
                    }
                    runtime.composer.perform(action);
                }
            }
            Action::AppendProviderDraft(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if let Some(provided) = &runtime.snapshot.composer {
                        let mut text = runtime.composer.text();
                        if !text.trim().is_empty() {
                            text.push('\n');
                        }
                        text.push_str(provided);
                        runtime.composer = text_editor::Content::with_text(&text);
                        runtime.revision = runtime.revision.wrapping_add(1);
                    }
                }
            }
            Action::Submit(target, mode) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.composer.text().trim().is_empty() && runtime.attachments.is_empty() {
                    return Task::none();
                }
                if (mode == PromptMode::Steer && !runtime.snapshot.capabilities.steer)
                    || (mode == PromptMode::Queue && !runtime.snapshot.capabilities.queue)
                    || (mode == PromptMode::Send && runtime.snapshot.busy)
                {
                    return Task::none();
                }
                let text = runtime.composer.text();
                let slash_name = text
                    .trim()
                    .strip_prefix('/')
                    .and_then(|command| command.split_whitespace().next());
                let command = if mode == PromptMode::Send
                    && slash_name.is_some_and(|name| {
                        runtime
                            .snapshot
                            .commands
                            .iter()
                            .any(|command| command.name.trim_start_matches('/') == name)
                    }) {
                    if !runtime.attachments.is_empty() {
                        runtime.error =
                            Some("Send attachments with a message, not a slash command.".into());
                        return Task::none();
                    }
                    Command::Slash {
                        command: text.trim().to_owned(),
                    }
                } else {
                    Command::Prompt {
                        text,
                        attachments: runtime.attachments.clone(),
                        mode,
                    }
                };
                return self.chat_send(target, command, true);
            }
            Action::Command(target, command) => return self.chat_send(target, command, false),
            Action::CommandFinished(target, id, result) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id == id)
                {
                    return Task::none();
                }
                let pending = runtime.pending.take().unwrap();
                match result {
                    Ok(()) => {
                        if let Some(mode) = pending.permission_mode {
                            runtime.permission_mode = Some(mode);
                        }
                        if pending.draft_revision == Some(runtime.revision) {
                            runtime.composer = text_editor::Content::new();
                            runtime.attachments.clear();
                            runtime.revision = runtime.revision.wrapping_add(1);
                        }
                        if let Some(request) = pending.request {
                            runtime.answers.retain(|(id, _), _| id != &request);
                        }
                        runtime.refresh();
                    }
                    Err(error) => runtime.error = Some(error),
                }
            }
            Action::Toggle(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if !runtime.expanded.remove(&id) {
                        runtime.expanded.insert(id);
                    }
                }
            }
            Action::ToggleTurn(target, key) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    // The default depends on whether the turn is still
                    // running, so the toggle is recorded against what is
                    // currently shown rather than against a fixed value.
                    let open = runtime
                        .opened
                        .get(&key)
                        .copied()
                        .unwrap_or_else(|| runtime.turn_running(&key));
                    runtime.opened.insert(key, !open);
                }
            }
            Action::Copy(target, id, part) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                // The body itself lives in the snapshot; only the extracted
                // code blocks are the cache's own.
                let text = match part {
                    CopyPart::Code(index) => runtime
                        .cache
                        .get(&id)
                        .and_then(|cached| cached.code.codes.get(index))
                        .cloned(),
                    part => runtime
                        .snapshot
                        .items
                        .iter()
                        .find(|item| item.id == id)
                        .and_then(|item| copy_text(part, &item.content)),
                };
                if let Some(text) = text {
                    return iced::clipboard::write(text).map(cosmic::Action::App);
                }
            }
            Action::OpenUrl(raw) => {
                return cosmic::task::future(async move {
                    let result = match Url::parse(&raw) {
                        Ok(url) if matches!(url.scheme(), "https" | "http") => {
                            open_system_url(url).await
                        }
                        _ => Err(
                            "Only explicit HTTP and HTTPS links can be opened from chat.".into(),
                        ),
                    };
                    Message::ChatUi(Action::UrlFinished(result))
                });
            }
            Action::UrlFinished(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(error));
                }
            }
            Action::Attach(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime.snapshot.capabilities.attachments || runtime.attachment_chooser_pending
                {
                    return Task::none();
                }
                runtime.attachment_chooser_pending = true;
                let directory = runtime.session.launch.recipe.working_directory.clone();
                return cosmic::task::future(async move {
                    let result = match file_chooser::open::Dialog::new()
                        .title("Attach files to chat")
                        .directory(directory)
                        .open_files()
                        .await
                    {
                        Ok(response) => Ok(Some(
                            response
                                .urls()
                                .iter()
                                .filter_map(|url| url.to_file_path().ok())
                                .collect(),
                        )),
                        Err(file_chooser::Error::Cancelled) => Ok(None),
                        Err(error) => Err(error.to_string()),
                    };
                    Message::ChatUi(Action::Attached(target, result))
                });
            }
            Action::Attached(target, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if !std::mem::take(&mut runtime.attachment_chooser_pending) {
                        return Task::none();
                    }
                    match result {
                        Ok(Some(paths)) => {
                            for path in paths {
                                if !runtime
                                    .attachments
                                    .iter()
                                    .any(|attachment| attachment.path == path)
                                {
                                    runtime.attachments.push(Attachment { path });
                                }
                            }
                            runtime.revision = runtime.revision.wrapping_add(1);
                        }
                        Ok(None) => {}
                        Err(error) => runtime.error = Some(error),
                    }
                }
            }
            Action::RemoveAttachment(target, index) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if index < runtime.attachments.len() {
                        runtime.attachments.remove(index);
                        runtime.revision = runtime.revision.wrapping_add(1);
                    }
                }
            }
            Action::Model(target, id) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(model) = runtime.snapshot.models.iter().find(|model| model.id == id)
                else {
                    return Task::none();
                };
                let command = Command::SetModel {
                    model: model.id.clone(),
                    reasoning: runtime
                        .snapshot
                        .reasoning
                        .clone()
                        .filter(|reasoning| model.reasoning.contains(reasoning)),
                };
                return self.chat_send(target, command, false);
            }
            Action::Reasoning(target, model_id, reasoning) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(model) = runtime.snapshot.models.iter().find(|model| {
                    model.id == model_id
                        && Some(&model.id) == runtime.snapshot.selected_model.as_ref()
                }) else {
                    return Task::none();
                };
                if !model.reasoning.contains(&reasoning) {
                    return Task::none();
                }
                let command = Command::SetModel {
                    model: model.id.clone(),
                    reasoning: Some(reasoning),
                };
                return self.chat_send(target, command, false);
            }
            Action::PermissionMode(target, mode) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime
                    .snapshot
                    .capabilities
                    .permission_modes
                    .contains(&mode)
                {
                    return Task::none();
                }
                return self.chat_send(target, Command::SetPermissionMode { mode }, false);
            }
            Action::Field(target, request, field, value) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if let Some(schema) = runtime
                        .snapshot
                        .requests
                        .iter()
                        .find(|candidate| candidate.id == request)
                        .and_then(|request| {
                            request
                                .fields
                                .iter()
                                .find(|candidate| candidate.id == field)
                        })
                    {
                        let values = runtime.answers.entry((request, field)).or_default();
                        if schema.multiple {
                            values.retain(|selected| {
                                schema.choices.iter().any(|choice| &choice.id == selected)
                            });
                        } else {
                            values.clear();
                        }
                        if !value.is_empty() {
                            values.push(value);
                        }
                    }
                }
            }
            Action::Choice(target, request, field, value) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if let Some(schema) = runtime
                        .snapshot
                        .requests
                        .iter()
                        .find(|candidate| candidate.id == request)
                        .and_then(|request| {
                            request
                                .fields
                                .iter()
                                .find(|candidate| candidate.id == field)
                        })
                    {
                        if schema.choices.iter().any(|choice| choice.id == value) {
                            let values = runtime.answers.entry((request, field)).or_default();
                            if schema.multiple {
                                if let Some(index) =
                                    values.iter().position(|selected| selected == &value)
                                {
                                    values.remove(index);
                                } else {
                                    values.push(value);
                                }
                            } else {
                                *values = vec![value];
                            }
                        }
                    }
                }
            }
            Action::Answer(target, request_id, action) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(request) = runtime
                    .snapshot
                    .requests
                    .iter()
                    .find(|request| request.id == request_id)
                else {
                    return Task::none();
                };
                let answers = request
                    .fields
                    .iter()
                    .map(|field| chat::Answer {
                        field: field.id.clone(),
                        values: runtime
                            .answers
                            .get(&(request_id.clone(), field.id.clone()))
                            .cloned()
                            .unwrap_or_default(),
                    })
                    .collect();
                return self.chat_send(
                    target,
                    Command::Answer {
                        request_id,
                        action,
                        answers,
                    },
                    false,
                );
            }
            Action::Slash(target, name) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if runtime
                        .snapshot
                        .commands
                        .iter()
                        .any(|command| command.name == name)
                    {
                        runtime.composer = text_editor::Content::with_text(&format!(
                            "/{} ",
                            name.trim_start_matches('/')
                        ));
                        runtime.revision = runtime.revision.wrapping_add(1);
                    }
                }
            }
            Action::Mention(target, invocation) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime
                    .snapshot
                    .skills
                    .iter()
                    .any(|skill| skill.invocation == invocation)
                {
                    return Task::none();
                }
                // A skill is named inside a message rather than instead of
                // one, so only the word the reader was typing is replaced and
                // whatever they had already written stays.
                let draft = runtime.composer.text();
                let kept = draft
                    .rfind(|character: char| character.is_whitespace())
                    .map_or(0, |index| index + 1);
                runtime.composer =
                    text_editor::Content::with_text(&format!("{}{invocation} ", &draft[..kept]));
                runtime.revision = runtime.revision.wrapping_add(1);
            }
            Action::HandoffFinished(target, result) => {
                if self.chat_runtime_mut(target).is_none() {
                    return Task::none();
                }
                if !self.chat_ui.handoffs.remove(&target) {
                    return Task::none();
                }
                let terminal = match result {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error));
                        return Task::none();
                    }
                };
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if let Some(reason) = runtime.transition_blocker() {
                    self.status = Some(TransientNotice::error(reason));
                    return Task::none();
                }
                runtime.session.launch.session_id = runtime.handle.snapshot().session_id.clone();
                let session = runtime.session.clone();
                self.commit_chat_session(target.card, session);
                // Enqueue the returned live proof checkpoint. The host retains
                // the transition receipt independently of this UI attachment.
                let task = self.finish_chat_terminal_handoff(target.card, terminal);
                self.chat_forget(target.card);
                return task;
            }
            Action::CloseFinished(target, result) => {
                if self.chat_runtime_mut(target).is_none() {
                    return Task::none();
                }
                if !self.chat_ui.closing.remove(&target) {
                    return Task::none();
                }
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(error));
                    return Task::none();
                }
                self.chat_ui.closed.insert(target.card);
                self.observe_agent_activity_for_card(target.card, None);
                return cosmic::task::message(Message::CommitClose(target.card));
            }
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(body: &str) -> ItemContent {
        ItemContent::Text {
            text: body.to_owned(),
            thinking: false,
            phase: chat::Phase::Final,
        }
    }

    /// A message that arrives byte by byte must present exactly as one that
    /// arrived whole. The incremental fence scan is the only difference, and a
    /// mistake in it would show the wrong code behind a copy control rather
    /// than fail loudly.
    #[test]
    fn streamed_body_finds_the_same_code_blocks_as_a_whole_one() {
        const BODY: &str =
            "Intro\n\n```rust\nfn main() {}\nlet total = 1;\n```\n\nAfter\n\n~~~\nplain\n~~~\n";
        let whole = CachedItem::new(&text(BODY));
        assert_eq!(
            whole.code.codes,
            vec![
                "fn main() {}\nlet total = 1;\n".to_owned(),
                "plain\n".to_owned()
            ]
        );

        let mut streamed = CachedItem::new(&text(""));
        for end in 1..=BODY.len() {
            if !BODY.is_char_boundary(end) {
                continue;
            }
            let content = text(&BODY[..end]);
            let Some(body) = streamed.token.appended(&content) else {
                panic!("a longer prefix of the same body must read as an append");
            };
            let parsed = streamed.token.body;
            streamed.extend(body, parsed);
            streamed.token = ChangeToken::of(&content);
        }
        assert_eq!(streamed.code.codes, whole.code.codes);
        assert_eq!(streamed.token, whole.token);
    }

    /// The token is what licenses reusing a parse, so it must refuse anything
    /// that is not bytes added to the end.
    #[test]
    fn change_token_refuses_a_rewritten_body() {
        let cached = CachedItem::new(&text("hello world"));
        assert_eq!(
            cached.token.appended(&text("hello world and more")),
            Some("hello world and more")
        );
        assert!(
            cached
                .token
                .appended(&text("HELLO WORLD and more"))
                .is_none()
        );
        assert!(cached.token.appended(&text("hello world")).is_none());
        assert!(cached.token.appended(&text("hello")).is_none());
        assert!(
            cached
                .token
                .appended(&ItemContent::Text {
                    text: "hello world and more".to_owned(),
                    thinking: true,
                    phase: chat::Phase::Final,
                })
                .is_none(),
            "thinking text is collapsed differently and is not the same item"
        );
        assert!(
            cached
                .token
                .appended(&ItemContent::Text {
                    text: "hello world and more".to_owned(),
                    thinking: false,
                    phase: chat::Phase::Commentary,
                })
                .is_none(),
            "narration and an answer are typeset differently, so a phase change \
             is not an append"
        );
    }

    /// Same-length edits are what a length-only token would miss.
    #[test]
    fn change_token_separates_same_length_tool_output() {
        let tool = |output: &str| ItemContent::Tool {
            name: "shell".to_owned(),
            input: "ls".to_owned(),
            output: output.to_owned(),
            status: chat::ToolStatus::Running,
        };
        assert_ne!(
            ChangeToken::of(&tool("aaaa")),
            ChangeToken::of(&tool("aaab"))
        );
        assert_eq!(
            ChangeToken::of(&tool("aaaa")),
            ChangeToken::of(&tool("aaaa"))
        );
    }

    /// A copy control that resolves to nothing is indistinguishable from a
    /// broken one, and a command's output has no other way out of the
    /// transcript.
    #[test]
    fn every_step_a_copy_control_is_offered_on_resolves_to_text() {
        let command = ItemContent::Command {
            label: "Ran tests".to_owned(),
            command: "cargo test".to_owned(),
            working_directory: "/tmp".to_owned(),
            action: chat::CommandAction::Run,
            status: chat::ToolStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: Some(12),
            output: "ok. 3 passed".to_owned(),
        };
        assert_eq!(
            copy_text(CopyPart::Text, &command).as_deref(),
            Some("cargo test\n\nok. 3 passed")
        );
        assert_eq!(
            copy_text(CopyPart::Input, &command).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            copy_text(CopyPart::Output, &command).as_deref(),
            Some("ok. 3 passed")
        );
        let changes = ItemContent::Changes {
            changes: vec![chat::FileChange {
                path: "src/app.rs".to_owned(),
                diff: "@@\n-old\n+new\n".to_owned(),
                kind: chat::ChangeKind::Updated,
            }],
            status: chat::ToolStatus::Succeeded,
        };
        assert_eq!(
            copy_text(CopyPart::Text, &changes).as_deref(),
            Some("@@\n-old\n+new\n")
        );
        let tool = ItemContent::Tool {
            name: "write".to_owned(),
            input: "{}".to_owned(),
            output: "done".to_owned(),
            status: chat::ToolStatus::Succeeded,
        };
        assert_eq!(
            copy_text(CopyPart::Text, &tool).as_deref(),
            Some("{}\n\ndone")
        );
        assert_eq!(
            copy_text(
                CopyPart::Text,
                &ItemContent::WebSearch {
                    query: "iced scrollable".to_owned(),
                    results: vec![chat::SearchResult {
                        title: "Scrollable".to_owned(),
                        url: "https://docs.rs/iced".to_owned(),
                        snippet: String::new(),
                    }],
                }
            )
            .as_deref(),
            Some("iced scrollable\n\nScrollable\nhttps://docs.rs/iced")
        );
    }

    /// The detail prints a rename's source path, so a token that ignores it
    /// would leave the stale path on screen after an in-place patch update.
    #[test]
    fn change_token_separates_renames_that_differ_only_in_their_source() {
        let renamed = |from: &str| ItemContent::Changes {
            changes: vec![chat::FileChange {
                path: "src/new.rs".to_owned(),
                diff: String::new(),
                kind: chat::ChangeKind::Renamed {
                    from: from.to_owned(),
                },
            }],
            status: chat::ToolStatus::Succeeded,
        };
        assert_ne!(
            ChangeToken::of(&renamed("src/old.rs")),
            ChangeToken::of(&renamed("src/older.rs"))
        );
        assert_eq!(
            ChangeToken::of(&renamed("src/old.rs")),
            ChangeToken::of(&renamed("src/old.rs"))
        );
    }
}
