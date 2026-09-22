use super::*;
use crate::agent::{Activity, Provider};
use crate::chat::client::{ClientHandle, Wake};
use crate::chat::{self, Attachment, Command, ItemContent, PromptMode, Snapshot, clock_label};
use base64::Engine as _;
use cosmic::widget::{markdown, text_editor};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

mod scroll;
mod step;
mod tool_text;
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
        crate::ui_controls::symbolic_icon(name),
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
    // Muted at rest: these sit at the foot of every answer, and drawn in the
    // text color they competed with the answer for attention.
    .icon_tone(crate::design::Tone::Themed(crate::design::muted_icon_color))
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Target {
    pub card: CardId,
    pub generation: u64,
}

#[derive(Clone)]
pub struct Ready(Arc<Mutex<Option<Result<Arc<ClientHandle>, chat::StartFailure>>>>);
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
    Change(usize),
}

#[derive(Clone)]
pub enum Action {
    ToolText(Target, String, CopyPart, text_editor::Action),
    ChooseProvider(Target, Provider),
    ChooseFreshProvider(CardId, Provider),
    Switch(CardId),
    Ready(Target, Ready),
    /// The result of closing a conversation that started for a card which
    /// could no longer hold it, so a stranded writer is not left behind.
    Discarded(Result<(), String>),
    Wake(Target, Wake),
    UpdatesAcknowledged(Target, Result<(), String>),
    Reconnect(Target),
    /// Starts a new conversation in the same session directory and working
    /// directory as a saved one that cannot be resumed.
    ///
    /// Separate from `Reconnect` because the two are not interchangeable: one
    /// asks for the saved conversation again, the other gives it up. A card
    /// that cannot resume offers both and does neither on its own.
    StartFresh(CardId),
    Composer(Target, text_editor::Action),
    Submit(Target, PromptMode),
    /// Recalls the message sent before the one the composer shows, or the
    /// unsent draft again once the recall has walked forward past the newest.
    HistoryOlder(Target),
    HistoryNewer(Target),
    AppendProviderDraft(Target),
    Command(Target, Command),
    CommandFinished(Target, u64, Result<(), String>),
    Toggle(Target, String),
    /// Opens or closes one group's step strip, addressed by the id of the item
    /// that started the group.
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
    /// Where the transcript's viewport has moved to, which is what asks for
    /// another page of history.
    Scrolled(Target, view::Scroll),
    /// Opens or closes the model search.
    ModelPicker(Target, bool),
    ReasoningPicker(Target, bool),
    ModelQuery(Target, String),
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
        )
    }

    fn mutation_target(&self) -> Option<Target> {
        match self {
            Self::Composer(target, action) if action.is_edit() => Some(*target),
            Self::ChooseProvider(target, _)
            | Self::Reconnect(target)
            | Self::Submit(target, _)
            | Self::AppendProviderDraft(target)
            | Self::Command(target, _)
            | Self::Attach(target)
            | Self::RemoveAttachment(target, _)
            | Self::Model(target, _)
            | Self::Reasoning(target, _, _)
            | Self::PermissionMode(target, _)
            | Self::Field(target, _, _, _)
            | Self::Choice(target, _, _, _)
            | Self::Answer(target, _, _)
            | Self::Slash(target, _)
            | Self::Mention(target, _)
            | Self::Scrolled(target, _)
            | Self::ModelPicker(target, _)
            | Self::ModelQuery(target, _) => Some(*target),
            _ => None,
        }
    }
}

struct Attempt {
    target: Target,
    session: chat::Session,
}

pub(super) struct State {
    sender: async_channel::Sender<(Target, Wake)>,
    receiver: async_channel::Receiver<(Target, Wake)>,
    generation: u64,
    attempts: HashMap<CardId, Attempt>,
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
    parked: HashMap<CardId, Box<Runtime>>,
    /// The cards whose saved conversation cannot be resumed by trying again.
    ///
    /// A card in this set has no chat runtime to hold the offer, because its
    /// start never produced one, so the failed card itself has to carry it.
    fresh_offers: HashSet<CardId>,
    fresh_threads: HashSet<CardId>,
    /// The new conversation a card is waiting to start once the one it could
    /// not resume has been closed.
    ///
    /// The daemon keys a conversation by this card's chat id, so the failed
    /// conversation has to be given up before the replacement can claim it.
    restarting: HashMap<CardId, chat::Session>,
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
            closing: HashSet::new(),
            closed: HashSet::new(),
            drafts: HashMap::new(),
            parked: HashMap::new(),
            fresh_offers: HashSet::new(),
            fresh_threads: HashSet::new(),
            restarting: HashMap::new(),
        }
    }
}

impl State {
    pub(super) fn transition_pending(&self) -> bool {
        !self.attempts.is_empty() || !self.handoffs.is_empty() || !self.closing.is_empty()
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

/// An elapsed time at the precision a reader can act on.
///
/// Sub-second work is reported in milliseconds because the difference between
/// 4 ms and 400 ms is the difference between a cache hit and a process launch;
/// past a second the milliseconds stop carrying information.
pub(super) fn format_duration(millis: u64) -> String {
    match millis {
        0..1_000 => format!("{millis}ms"),
        0..60_000 => format!("{:.1}s", millis as f64 / 1_000.0),
        _ => format!("{}m {}s", millis / 60_000, (millis % 60_000) / 1_000),
    }
}

/// Settles which step strips are open for the groups that now exist.
///
/// Two jobs, and they have to happen together. A group is forgotten once the
/// store's window no longer produces it, because the map would otherwise grow
/// for the life of the card. A group that is new gets its automatic state
/// written here, once: open while it is running, because watching the work is
/// the point, and closed otherwise, because a finished group is read for its
/// answer.
///
/// Writing that state instead of resolving it per frame is the whole point.
/// The surface used to fall back to "open if running" whenever the reader had
/// not toggled anything, which meant a strip the reader was reading collapsed
/// under them the instant its last step finished - and a group they had
/// deliberately opened reverted as soon as its key was rederived.
fn reconcile_disclosure(opened: &mut HashMap<String, bool>, turns: &[turn::Turn]) {
    let keys: HashSet<&str> = turns.iter().map(|turn| turn.key.as_str()).collect();
    opened.retain(|key, _| keys.contains(key.as_str()));
    for turn in turns {
        if !opened.contains_key(&turn.key) {
            opened.insert(turn.key.clone(), turn.running);
        }
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
        (CopyPart::Change(index), ItemContent::Changes { changes, .. }) => {
            changes.get(index).map(|change| change.diff.clone())
        }
        (CopyPart::Code(_) | CopyPart::Change(_), _) | (CopyPart::Input | CopyPart::Output, _) => {
            None
        }
    }
}

/// One transcript item's presentation, retained across refreshes.
///
/// The body is never copied in: what an item looked like when it was parsed is
/// recorded as a `ChangeToken`, and the authoritative text stays in the
/// snapshot the runtime already holds.
struct CachedItem {
    preview: tool_text::Preview,
    tool_input: Option<tool_text::ToolText>,
    tool_output: Option<tool_text::ToolText>,
    patches: Vec<tool_text::ToolText>,
    token: ChangeToken,
    markdown: markdown::Content,
    code: CodeScan,
    /// The highlighted spans of each fenced block the Markdown parsed, keyed
    /// by the block's language and text: what a frame looks a block up by.
    /// The palette is the runtime's, applied when the block is highlighted,
    /// so a change of syntax theme rehighlights rather than restyles.
    blocks: HashMap<u64, crate::diff_view::SyntaxLines>,
    /// A step's trailing metadata — exit status and duration — formed once per
    /// change rather than once per frame.
    detail: Option<String>,
    image: Option<iced::widget::image::Handle>,
}
impl CachedItem {
    #[cfg(test)]
    fn new(content: &ItemContent, theme: crate::settings::EditorSyntaxTheme) -> Self {
        Self::updated(content, theme, None, false)
    }

    fn updated(
        content: &ItemContent,
        theme: crate::settings::EditorSyntaxTheme,
        previous: Option<Self>,
        expanded: bool,
    ) -> Self {
        let (input, output, preview, patches) = previous
            .map(|cached| {
                (
                    cached.tool_input,
                    cached.tool_output,
                    Some(cached.preview),
                    cached.patches,
                )
            })
            .unwrap_or_default();
        use tool_text::{Language, ToolText};
        let patches = match content {
            ItemContent::Changes { changes, .. } if expanded => {
                let mut previous = patches.into_iter();
                changes
                    .iter()
                    .map(|change| ToolText::update(previous.next(), &change.diff, Language::Diff))
                    .collect()
            }
            _ => Vec::new(),
        };
        let (tool_input, tool_output) = match content {
            _ if !expanded => (None, None),
            ItemContent::Command {
                command,
                output: text,
                ..
            } => (
                Some(ToolText::update(input, command, Language::Shell)),
                Some(ToolText::update(output, text, Language::structured(text))),
            ),
            ItemContent::Tool {
                input: text,
                output: result,
                ..
            } => (
                Some(ToolText::update(input, text, Language::structured(text))),
                Some(ToolText::update(
                    output,
                    result,
                    Language::structured(result),
                )),
            ),
            _ => (None, None),
        };
        let mut cached = Self {
            preview: tool_text::Preview::update(preview, content, theme),
            tool_input,
            tool_output,
            patches,
            token: ChangeToken::of(content),
            markdown: markdown::Content::new(),
            code: CodeScan::default(),
            blocks: HashMap::new(),
            detail: None,
            image: None,
        };
        match content {
            ItemContent::Text { text, .. } => cached.extend(text, 0, theme),
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
    fn extend(&mut self, body: &str, parsed: usize, theme: crate::settings::EditorSyntaxTheme) {
        self.markdown.push_str(&body[parsed..]);
        self.code.feed(body);
        self.highlight(theme);
    }

    /// A fence larger than this is drawn plain: a syntect pass over it would
    /// stall the frame it is cached on, and a block that size is a file the
    /// agent pasted rather than a snippet it is showing.
    const HIGHLIGHT_LIMIT: usize = 64 * 1024;

    /// Highlights every fenced block the Markdown now holds, keeping the
    /// spans of blocks that are unchanged since the last pass. A streamed
    /// block changes on every refresh until its fence closes, so it is the
    /// one block a refresh usually rehighlights.
    fn highlight(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        let mut previous = std::mem::take(&mut self.blocks);
        let mut blocks = HashMap::new();
        for_each_fence(self.markdown.items(), &mut |language, code| {
            if code.len() > Self::HIGHLIGHT_LIMIT {
                return;
            }
            let key = fence_key(language, code);
            let spans = previous
                .remove(&key)
                .unwrap_or_else(|| crate::diff_view::highlight_snippet(code, language, theme));
            blocks.insert(key, spans);
        });
        self.blocks = blocks;
    }

    /// Discards the highlighting and does it again in `theme`.
    fn rehighlight(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        self.preview.rehighlight(theme);
        self.blocks.clear();
        self.highlight(theme);
    }

    /// The highlighted lines of the fence whose info string is `language`
    /// and whose text is `code`, where the cache holds them.
    fn block(&self, language: Option<&str>, code: &str) -> Option<&crate::diff_view::SyntaxLines> {
        self.blocks.get(&fence_key(language?, code))
    }
}

/// Visits every fenced block among `items` that names a language, in the
/// order the Markdown draws them, descending into lists and quotes the way
/// the drawing does.
fn for_each_fence<'a>(
    items: impl IntoIterator<Item = &'a markdown::Item>,
    visit: &mut impl FnMut(&'a str, &'a str),
) {
    for item in items {
        match item {
            markdown::Item::CodeBlock {
                language: Some(language),
                code,
                ..
            } => visit(language, code),
            markdown::Item::List { bullets, .. } => {
                for bullet in bullets {
                    let (markdown::Bullet::Point { items } | markdown::Bullet::Task { items, .. }) =
                        bullet;
                    for_each_fence(items, visit);
                }
            }
            markdown::Item::Quote(items) => for_each_fence(items, visit),
            _ => {}
        }
    }
}

/// The identity of a fenced block, as its highlighting is looked up by: the
/// language it declares and the text it holds.
fn fence_key(language: &str, code: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    language.hash(&mut hasher);
    code.hash(&mut hasher);
    hasher.finish()
}

/// The transcript's Markdown, drawn with the shell's own code blocks.
///
/// Everything but a fenced block is iced's rendering; a fence is drawn from
/// its cached highlighting on the sunken surface commands and outputs use, in
/// the palette the editor's syntax theme chose for the current appearance.
struct Prose<'a> {
    cached: &'a CachedItem,
}

impl<'a> Prose<'a> {
    fn view(cached: &'a CachedItem, settings: markdown::Settings) -> Element<'a, Message> {
        markdown::view_with(cached.markdown.items(), settings, &Self { cached })
    }
}

impl<'a> markdown::Viewer<'a, Message, cosmic::Theme, cosmic::Renderer> for Prose<'a> {
    fn on_link_click(uri: markdown::Uri) -> Message {
        action(Action::OpenUrl(uri.to_string()))
    }

    fn code_block(
        &self,
        settings: markdown::Settings,
        language: Option<&'a str>,
        code: &'a str,
        _lines: &'a [markdown::Text],
    ) -> Element<'a, Message> {
        let palette = self
            .cached
            .block(language, code)
            .map(|lines| &lines[usize::from(cosmic::theme::is_dark())]);
        let rows: Vec<Element<'a, Message>> = code
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let spans = palette
                    .and_then(|lines| lines.get(index))
                    .map_or(&[][..], Vec::as_slice);
                let line: Element<'a, Message> =
                    iced::widget::rich_text(crate::diff_view::syntax_spans(line, spans))
                        .font(settings.style.code_block_font)
                        .size(settings.code_size)
                        .wrapping(iced::widget::text::Wrapping::None)
                        .into();
                line
            })
            .collect();
        widget::container(
            widget::scrollable(
                widget::container(widget::column::with_children(rows))
                    .padding([step::GAP_S as u16, step::GAP_M as u16]),
            )
            .direction(iced::widget::scrollable::Direction::Horizontal(
                design::scrollbar(),
            )),
        )
        .class(design::surface(
            Some(design::sunken),
            [design::RADIUS_S; 4],
            true,
        ))
        .width(Length::Fill)
        .into()
    }
}

/// Provider snapshots also cover restored chats and prompts sent through the
/// paired CLI. Never infer a conversation from another process in the same cwd.
fn snapshot_channel_name(snapshot: &Snapshot) -> Option<&str> {
    snapshot
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| {
            snapshot
                .items
                .iter()
                .find_map(|item| match (&item.role, &item.content) {
                    (chat::Role::User, ItemContent::Text { text, .. })
                        if !text.trim().is_empty() =>
                    {
                        Some(text.as_str())
                    }
                    _ => None,
                })
        })
}

struct PendingCommand {
    id: u64,
    draft_revision: Option<u64>,
    request: Option<String>,
    permission_mode: Option<String>,
    /// The first user prompt, used to name a new channel once locally queued.
    first_prompt: Option<String>,
    /// The message this command sends, kept for the composer's history once
    /// the provider has accepted it. A rejected send is not history: the
    /// draft it came from is still in the composer.
    sent: Option<String>,
    /// Which end of the history this command is extending, where it is a
    /// page request at all. The transcript says so at the edge the rows will
    /// appear at, so it has to know which edge is waiting.
    page: Option<view::Page>,
}
pub(super) struct Runtime {
    pub(super) target: Target,
    pub(super) session: chat::Session,
    handle: Arc<ClientHandle>,
    snapshot: Arc<Snapshot>,
    cache: HashMap<String, CachedItem>,
    /// The palette the cache's code blocks are highlighted in.
    syntax_theme: crate::settings::EditorSyntaxTheme,
    /// The snapshot's items grouped into stretches of work, rebuilt only when
    /// the snapshot changes rather than once per frame.
    turns: Vec<turn::Turn>,
    /// The ids of items the reader was asked something after.
    ///
    /// A question or an approval request is the agent handing control back, so
    /// it ends the stretch of work that asked and the answer starts a new one.
    /// Requests never enter the transcript and vanish the moment they are
    /// resolved, while the grouping is recomputed from the whole item list on
    /// every snapshot, so the boundary has to be remembered here or answering
    /// a question would merge the work before it into the work after.
    asked_after: HashSet<String>,
    /// The requests whose boundary has already been recorded.
    ///
    /// An optional question stays pending while the agent carries on working,
    /// so without this every refresh would record whatever item was newest
    /// and each item would become a stretch of work of its own. A resolved
    /// request is dropped from here, and a request id is never reused, so the
    /// set stays the size of what is actually pending.
    asked: HashSet<String>,
    /// Whether each group's step strip is open, keyed by the group.
    ///
    /// A group's state is written once, when the group is first seen: open
    /// while it is running, because watching the work is the point, and
    /// closed otherwise, because the answer is what a finished group is read
    /// for. From then on only a toggle changes it, so a group the reader
    /// opened does not shut itself the moment its last step finishes.
    opened: HashMap<String, bool>,
    composer: text_editor::Content<cosmic::Renderer>,
    revision: u64,
    attachments: Vec<Attachment>,
    attachment_chooser_pending: bool,
    expanded: HashSet<String>,
    answers: view::Answers,
    pending: Option<PendingCommand>,
    next_command: u64,
    disconnected: bool,
    /// The empty connection was explicitly closed for a model-family change.
    /// Unlike disconnection, this proves there is no old actor left to close.
    provider_closed: bool,
    visible: AtomicBool,
    error: Option<String>,
    /// Local send errors anchored to the exchange that produced them.
    failures: Vec<(Option<String>, String)>,
    reasoning_picker: bool,
    permission_mode: Option<String>,
    /// Messages the provider accepted, oldest first, for Up and Down in the
    /// composer to walk through the way a shell does.
    history: Vec<String>,
    /// Which entry the composer is showing while the reader walks the
    /// history; `None` while they are writing.
    history_cursor: Option<usize>,
    /// The draft that was in the composer when the walk began, restored when
    /// the walk goes past the newest message.
    history_stash: String,
    /// The most recent transcript viewport, used for history pagination.
    scroll: view::Scroll,
    /// The loaded transcript's height when a page of history was last asked
    /// for from that end, and `None` while the reader is not at that end.
    ///
    /// This is what makes the gesture ask once. A viewport signal arrives
    /// whenever the transcript moves or grows, and a reader parked at the top
    /// of a streaming conversation would otherwise ask for the same page
    /// again on each of them. Recording the height rather than a bare flag is
    /// what lets a pane taller than its own history keep paging: there, the
    /// reader never leaves the boundary, so the arrival of the page is the
    /// only thing that can arm the next one.
    earlier_asked: Option<f32>,
    later_asked: Option<f32>,
    /// The model search's query while its popup is open, `None` when closed.
    model_picker: Option<String>,
}
/// The name the composer accepts for compaction where the provider offers it
/// as a capability rather than as one of its listed commands.
pub(super) const COMPACT_COMMAND: &str = "compact";

/// What a draft beginning with `/` asks for.
enum SlashInvocation {
    /// One of the provider's own commands, sent through its dispatcher.
    Command(String),
    /// Compaction, offered as a command wherever the provider can do it.
    Compact,
    /// A skill, as the message the provider invokes it with: `/review src`
    /// becomes `$review src` for Codex and stays as it is for a provider whose
    /// skills are slash commands.
    Skill(String),
}

/// Resolves a draft's leading `/name` against what the provider offers.
///
/// Names match without regard to case and to the `/` or `$` the provider
/// prefixes its own form with, because a reader completing from the picker
/// and a reader typing from memory should reach the same skill.
fn slash_invocation(draft: &str, snapshot: &Snapshot) -> Option<SlashInvocation> {
    let draft = draft.trim();
    let rest = draft.strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (name, arguments) = (&rest[..end], rest[end..].trim_start());
    if name.is_empty() {
        return None;
    }
    fn bare(candidate: &str) -> &str {
        candidate.trim_start_matches(['/', '$'])
    }
    if snapshot
        .commands
        .iter()
        .any(|command| bare(&command.name).eq_ignore_ascii_case(name))
    {
        return Some(SlashInvocation::Command(draft.to_owned()));
    }
    if let Some(skill) = snapshot.skills.iter().find(|skill| {
        skill.name.eq_ignore_ascii_case(name) || bare(&skill.invocation).eq_ignore_ascii_case(name)
    }) {
        let mut text = skill.invocation.clone();
        if !arguments.is_empty() {
            text.push(' ');
            text.push_str(arguments);
        }
        return Some(SlashInvocation::Skill(text));
    }
    if name.eq_ignore_ascii_case(COMPACT_COMMAND) && snapshot.capabilities.compact {
        return Some(SlashInvocation::Compact);
    }
    None
}

impl Runtime {
    /// Records a message the provider accepted. The same message sent twice
    /// in a row is one entry, as a shell keeps it, and the walk restarts
    /// from the newest.
    fn remember(&mut self, sent: String) {
        if self.history.last() != Some(&sent) {
            self.history.push(sent);
        }
        self.history_cursor = None;
        self.history_stash.clear();
    }

    /// Walks the history one message older or newer, showing it in the
    /// composer. Walking older from the draft keeps the draft; walking newer
    /// past the newest message brings it back.
    fn recall(&mut self, older: bool) {
        let next = match (self.history_cursor, older) {
            (None, true) => {
                let Some(newest) = self.history.len().checked_sub(1) else {
                    return;
                };
                self.history_stash = self.composer.text();
                Some(newest)
            }
            (None, false) => return,
            (Some(0), true) => return,
            (Some(index), true) => Some(index - 1),
            (Some(index), false) if index + 1 < self.history.len() => Some(index + 1),
            (Some(_), false) => None,
        };
        let text = match next {
            Some(index) => self.history[index].clone(),
            None => std::mem::take(&mut self.history_stash),
        };
        self.history_cursor = next;
        self.composer = text_editor::Content::with_text(text.trim_end_matches('\n'));
        self.revision = self.revision.wrapping_add(1);
    }

    fn new(
        target: Target,
        session: chat::Session,
        handle: Arc<ClientHandle>,
        visible: bool,
        syntax_theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let mut runtime = Self {
            target,
            session,
            snapshot: Arc::new(Snapshot::default()),
            handle,
            cache: HashMap::new(),
            syntax_theme,
            turns: Vec::new(),
            asked_after: HashSet::new(),
            asked: HashSet::new(),
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
            provider_closed: false,
            visible: AtomicBool::new(visible),
            error: None,
            failures: Vec::new(),
            reasoning_picker: false,
            permission_mode: None,
            history: Vec::new(),
            history_cursor: None,
            history_stash: String::new(),
            scroll: view::Scroll::default(),
            earlier_asked: None,
            later_asked: None,
            model_picker: None,
        };
        runtime.refresh();
        runtime
    }
    /// Highlights the transcript's code blocks in `theme` from now on.
    pub(super) fn set_syntax_theme(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        if self.syntax_theme == theme {
            return;
        }
        self.syntax_theme = theme;
        for cached in self.cache.values_mut() {
            cached.rehighlight(theme);
        }
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
        // Both sets are keyed by item id and the store drops items past its
        // window, so without this they grow for the life of the card: one
        // entry per step the reader ever opened and per question they were
        // ever asked, held after the item it named stopped existing.
        self.expanded.retain(|id| live_ids.contains(id.as_str()));
        self.asked_after.retain(|id| live_ids.contains(id.as_str()));
        for item in &snapshot.items {
            let token = ChangeToken::of(&item.content);
            if let Some(cached) = self.cache.get_mut(&item.id) {
                if cached.token == token {
                    continue;
                }
                if let Some(body) = cached.token.appended(&item.content) {
                    let parsed = cached.token.body;
                    cached.extend(body, parsed, self.syntax_theme);
                    cached.token = token;
                    continue;
                }
            }
            let previous = self.cache.remove(&item.id);
            self.cache.insert(
                item.id.clone(),
                CachedItem::updated(
                    &item.content,
                    self.syntax_theme,
                    previous,
                    self.expanded.contains(&item.id),
                ),
            );
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
        // Each question's boundary is recorded once, against whatever item was
        // newest when it arrived, so it lands where the reader was actually
        // asked. The daemon coalesces presentation, so a request delivered in
        // the same batch as the work it interrupted can be recorded one item
        // late; that costs a step its group, never the disclosure state of one.
        self.asked
            .retain(|id| snapshot.requests.iter().any(|request| &request.id == id));
        for request in &snapshot.requests {
            if self.asked.insert(request.id.clone())
                && let Some(last) = snapshot.items.last()
            {
                self.asked_after.insert(last.id.clone());
            }
        }
        self.turns = turn::group(&snapshot.items, &self.asked_after);
        reconcile_disclosure(&mut self.opened, &self.turns);
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

    /// The named control that should receive the keyboard for this card.
    pub(super) fn focus_control(&self) -> &'static str {
        if self.model_picker.is_some() {
            "chat-model-search"
        } else {
            "chat-composer"
        }
    }

    fn can_choose_provider(&self) -> bool {
        self.session.terminal.is_none()
            && !self.snapshot.busy
            && !self.snapshot.has_earlier
            && !self.snapshot.has_later
            && self.snapshot.requests.is_empty()
            && self
                .snapshot
                .items
                .iter()
                .all(|item| item.role == chat::Role::System)
            && self.pending.is_none()
            && !self.attachment_chooser_pending
    }
    /// Whether this card should offer to start a new conversation beside the
    /// one it was asked to resume.
    ///
    /// The offer belongs to a card whose launch names a saved conversation and
    /// whose provider has since stopped without ever reporting that
    /// conversation: it is not on screen, and no amount of waiting will put it
    /// there, because the process that would have loaded it is gone. A card
    /// that merely lost its daemon connection is not in that position - its
    /// conversation is still being held for it - so it keeps only the
    /// reconnect.
    fn offers_fresh_conversation(&self) -> bool {
        self.session.launch.session_id.is_some()
            && self.snapshot.exited
            && self.snapshot.session_id != self.session.launch.session_id
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

/// The draft with the word being typed replaced by `invocation`.
///
/// A skill is named inside a message rather than instead of one, so the text
/// already written stays and a trailing space follows the name, ready for the
/// rest of the sentence.
fn mention_replaced(draft: &str, invocation: &str) -> String {
    // The split has to land on a character boundary: a no-break space pasted
    // in with a quotation is whitespace more than one byte wide, and a byte
    // offset taken from it would slice inside the character.
    let kept = draft
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    format!("{}{invocation} ", &draft[..kept])
}

fn observed_launch(session: &TerminalSession) -> Option<LaunchRecipe> {
    session
        .resume_hint
        .as_deref()
        .and_then(crate::model::agent_recovery::observed_launch_from_hint)
        .or_else(|| Provider::from_program(&session.launch.program).map(|_| session.launch.clone()))
}

/// Whether switching this chat to the native view must leave its source PTY running.
///
/// Codex native chat is the same thread as the CLI, including while a turn is
/// in progress. `ShutdownTerminal` kills that child. The session returned by
/// attach copies the daemon's stored terminal, whose continuity stays
/// `RelaunchAvailable` even while the process is live, so the decision cannot
/// consult `live_proof`. Claude and OMP stop the CLI before native resume, and
/// their source PTY is torn down with the surface.
pub(super) fn retains_source_pty(session: &chat::Session) -> bool {
    session.launch.provider == Provider::Codex
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
        recovery_origin: None,
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

fn fresh_chat_session(
    provider: Provider,
    id: TerminalId,
    working_directory: PathBuf,
) -> Result<chat::Session, String> {
    let mut recipe = LaunchRecipe {
        program: provider.executable().into(),
        arguments: Vec::new(),
        working_directory,
        environment: crate::model::agent_recovery::fresh_native_environment(provider, true),
    };
    match provider {
        // A shared listener lets CLI and native views use the same thread.
        Provider::Codex => recipe
            .arguments
            .extend(["--remote".into(), "unix://".into()]),
        Provider::Omp => {
            let root = provider
                .session_root(&recipe)
                .ok_or("Cannot locate the configured account for a new conversation")?;
            recipe.arguments.extend([
                "--session-dir".into(),
                root.join("sessions/native").to_string_lossy().into_owned(),
            ]);
        }
        Provider::Claude => {}
    }
    Ok(chat::Session {
        id,
        launch: chat::Launch {
            provider,
            recipe,
            session_id: None,
        },
        proof: None,
        terminal: None,
    })
}

impl Cagents {
    fn can_choose_failed_chat_provider(&self, card: CardId) -> bool {
        self.chat_ui.fresh_threads.contains(&card)
            && !self.chat_start_pending(card)
            && matches!(self.cards.get(&card), Some(CardRuntime::Failed(_)))
            && matches!(self.card_state(card).map(|state| &state.kind), Some(CardKind::Chat(session)) if session.launch.session_id.is_none() && session.terminal.is_none() && session.proof.is_none())
    }

    pub(super) fn chat_provider_recovery(&self, card: CardId) -> Option<Element<'_, Message>> {
        self.can_choose_failed_chat_provider(card).then(|| {
            widget::row::with_children(
                [
                    (Provider::Codex, "ChatGPT"),
                    (Provider::Claude, "Claude"),
                    (Provider::Omp, "Other models"),
                ]
                .into_iter()
                .map(|(provider, label)| {
                    widget::button::standard(label)
                        .on_press(action(Action::ChooseFreshProvider(card, provider)))
                        .into()
                })
                .collect::<Vec<_>>(),
            )
            .spacing(step::GAP_S)
            .into()
        })
    }
    fn choose_chat_provider(&mut self, target: Target, provider: Provider) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.can_choose_provider()
            || (runtime.session.launch.provider == provider && !runtime.provider_closed)
        {
            return Task::none();
        }
        let session = match fresh_chat_session(
            provider,
            runtime.session.id,
            runtime.session.launch.recipe.working_directory.clone(),
        ) {
            Ok(session) => session,
            Err(error) => {
                runtime.error = Some(error);
                return Task::none();
            }
        };
        let handle = runtime.handle.clone();
        runtime.model_picker = None;
        if runtime.provider_closed {
            runtime.session = session.clone();
            self.commit_chat_session(target.card, session.clone());
            return self.start_chat(target.card, session, chat::Intent::Start, None, false);
        }
        self.chat_ui.closing.insert(target);
        self.chat_ui.restarting.insert(target.card, session);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.close())
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::CloseFinished(target, result))
        })
    }

    pub(super) fn chat_scroll_key(&self, event: &keyboard::Event) -> Option<Task<Message>> {
        let keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modifiers,
            ..
        } = event
        else {
            return None;
        };
        if modifiers.alt() || modifiers.shift() || modifiers.logo() {
            return None;
        }
        let card = self.workspace.focused()?;
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        use keyboard::key::Named;
        let id = card_control_id(card, "chat-transcript");
        let task = match key {
            Named::PageUp | Named::PageDown if !modifiers.control() => {
                iced::widget::scrollable::scroll_by(
                    id,
                    iced::widget::scrollable::AbsoluteOffset {
                        x: 0.0,
                        y: runtime.scroll.page_height()
                            * if *key == Named::PageUp { 1.0 } else { -1.0 },
                    },
                )
            }
            Named::Home | Named::End => iced::widget::scrollable::snap_to(
                id,
                iced::widget::scrollable::RelativeOffset {
                    x: None,
                    y: Some(if *key == Named::Home { 1.0 } else { 0.0 }),
                },
            ),
            _ => return None,
        };
        Some(task.map(cosmic::Action::App))
    }

    pub(super) fn new_native_thread(&mut self) -> Task<Message> {
        let provider = self
            .workspace
            .focused()
            .and_then(|card| self.card_state(card))
            .into_iter()
            .chain(
                self.durable
                    .cards
                    .iter()
                    .rev()
                    .filter(|card| card.existence.is_open()),
            )
            .find_map(|card| match &card.kind {
                CardKind::Chat(session) => Some(session.launch.provider),
                _ => None,
            })
            .unwrap_or(Provider::Codex);
        self.new_native_chat(provider, None)
    }

    pub(super) fn dismiss_chat_picker(&mut self) -> bool {
        let Some(card) = self.workspace.focused() else {
            return false;
        };
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return false;
        };
        let open = runtime.model_picker.take().is_some() || runtime.reasoning_picker;
        runtime.reasoning_picker = false;
        open
    }
    /// Opens a new native conversation in a new card.
    ///
    /// `directory` is the conversation's root. A caller that already knows it,
    /// such as the chooser offering a new conversation beside a terminal,
    /// passes it rather than letting the workspace's own idea of a current
    /// directory answer for a terminal that runs somewhere else.
    pub(super) fn new_native_chat(
        &mut self,
        provider: Provider,
        directory: Option<PathBuf>,
    ) -> Task<Message> {
        if self.project_ui.deleting() {
            return Task::none();
        }
        let Some(working_directory) = directory.or_else(|| self.terminal_directory()) else {
            return cosmic::task::message(Message::OpenRepository);
        };
        let session =
            match fresh_chat_session(provider, TerminalId::new(), working_directory.clone()) {
                Ok(session) => session,
                Err(error) => {
                    self.status = Some(TransientNotice::error(error));
                    return Task::none();
                }
            };
        let card_id = CardId::new();
        self.chat_ui.fresh_threads.insert(card_id);
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
            self.start_chat(card_id, session, chat::Intent::Start, None, false),
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
        self.chat_ui.handoffs.retain(|target| target.card != card);
        self.chat_ui.closing.retain(|target| target.card != card);
        self.chat_ui.restarting.remove(&card);
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
        self.chat_ui.parked.remove(&card);
        self.chat_ui.fresh_offers.remove(&card);
        self.chat_ui.fresh_threads.remove(&card);
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
    /// Whether a card that could not put its saved conversation back on screen
    /// should offer to start a new one in the same place.
    ///
    /// A failed card carries the offer itself because its start never built a
    /// runtime to hold it; a card whose provider exited after starting asks
    /// its runtime instead.
    pub(super) fn chat_offers_fresh_conversation(&self, card: CardId) -> bool {
        if self.chat_start_pending(card) {
            return false;
        }
        match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime.offers_fresh_conversation(),
            _ => self.chat_ui.fresh_offers.contains(&card),
        }
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
    pub(super) fn chat_has_parked_runtime(&self, card: CardId) -> bool {
        self.chat_ui.parked.contains_key(&card)
    }
    pub(super) fn parked_chat_restart_blocker(&self) -> Option<&'static str> {
        self.chat_ui
            .parked
            .values()
            .find_map(|runtime| runtime.restart_blocker())
    }
    pub(super) fn begin_chat_close(&mut self, card: CardId) -> Task<Message> {
        if self.chat_start_pending(card) {
            return Task::none();
        }
        let Some(runtime) = (match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime),
            _ => self.chat_ui.parked.get(&card),
        }) else {
            return Task::none();
        };
        if runtime.attachment_chooser_pending {
            self.status = Some(TransientNotice::error(
                "Finish or cancel the file chooser before closing the chat.",
            ));
            return Task::none();
        }
        let target = runtime.target;
        if runtime.provider_closed {
            self.chat_ui.closing.insert(target);
            return cosmic::task::message(Message::ChatUi(Action::CloseFinished(target, Ok(()))));
        }
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
    /// A panic while connecting is a failed start, not an unwind of the UI thread.
    fn started_chat(
        caught: Result<
            Result<Arc<ClientHandle>, chat::StartFailure>,
            Box<dyn std::any::Any + Send>,
        >,
    ) -> Result<Arc<ClientHandle>, chat::StartFailure> {
        match caught {
            Ok(result) => result,
            Err(payload) => {
                let detail = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("the native chat connection panicked");
                Err(chat::StartFailure::retry(format!(
                    "Native chat failed to start: {detail}"
                )))
            }
        }
    }

    fn retire_switched_terminal(&mut self, terminal: TerminalSurface, session: &chat::Session) {
        self.retire_terminal_surface(terminal, !retains_source_pty(session));
    }

    pub(super) fn start_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
        intent: chat::Intent,
        source: Option<HostSessionProof>,
        terminate_source: bool,
    ) -> Task<Message> {
        self.chat_ui.generation = self.chat_ui.generation.wrapping_add(1).max(1);
        let selection = None;
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
                // A panic in the provider handshake has to come back as a
                // failed start. Unwinding here fails the blocking task; the
                // UI thread then only sees a join error and can still draw.
                Self::started_chat(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || {
                        ClientHandle::connect(
                            session,
                            intent,
                            source,
                            terminate_source,
                            selection,
                            move |wake| {
                                let _ = sender.try_send((target, wake));
                            },
                        )
                        .map(Arc::new)
                    },
                )))
            })
            .await
            .map_err(|error| chat::StartFailure::retry(error.to_string()))
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
            _ => self
                .chat_ui
                .parked
                .get_mut(&target.card)
                .filter(|runtime| runtime.target == target)
                .map(Box::as_mut),
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
        let page = match &command {
            Command::LoadEarlier => Some(view::Page::Earlier),
            Command::Refresh => Some(view::Page::Newer),
            _ => None,
        };
        let first_prompt = match &command {
            Command::Prompt { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        };
        let sent = match &command {
            Command::Prompt { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            Command::Slash { command } => Some(command.clone()),
            _ => None,
        };
        runtime.pending = Some(PendingCommand {
            id,
            draft_revision: clear_draft.then_some(runtime.revision),
            request,
            permission_mode,
            first_prompt,
            page,
            sent,
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
        let focus = if let Action::Switch(card) = &action {
            self.workspace
                .focus(*card)
                .ok()
                .map(|update| self.apply_viewport(update, true, false))
        } else {
            None
        };
        let action = self.handle_chat_action(action);
        Task::batch(focus.into_iter().chain(std::iter::once(action)))
    }
    fn handle_chat_action(&mut self, action: Action) -> Task<Message> {
        if let Some(target) = action.mutation_target()
            && (self.chat_start_pending(target.card) || self.chat_ui.closed.contains(&target.card))
        {
            return Task::none();
        }
        match action {
            Action::ToolText(target, id, part, event) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(cached) = runtime.cache.get_mut(&id)
                    && let Some(block) = match part {
                        CopyPart::Input => cached.tool_input.as_mut(),
                        CopyPart::Output => cached.tool_output.as_mut(),
                        CopyPart::Change(index) => cached.patches.get_mut(index),
                        _ => None,
                    }
                {
                    block.perform(event);
                }
            }
            Action::ChooseProvider(target, provider) => {
                return self.choose_chat_provider(target, provider);
            }
            Action::ChooseFreshProvider(card, provider) => {
                if !self.can_choose_failed_chat_provider(card) {
                    return Task::none();
                }
                let Some(CardKind::Chat(previous)) = self.card_state(card).map(|state| &state.kind)
                else {
                    return Task::none();
                };
                let session = match fresh_chat_session(
                    provider,
                    previous.id,
                    previous.launch.recipe.working_directory.clone(),
                ) {
                    Ok(session) => session,
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error));
                        return Task::none();
                    }
                };
                self.commit_chat_session(card, session.clone());
                self.cards.insert(card, CardRuntime::Loading);
                return self.start_chat(card, session, chat::Intent::Start, None, false);
            }
            Action::Switch(card) => {
                if self.chat_start_pending(card) {
                    return Task::none();
                }
                if let Some(mut runtime) = self.chat_ui.parked.remove(&card) {
                    runtime.show(true);
                    runtime.refresh();
                    let session = runtime.session.clone();
                    if let Some(CardRuntime::Terminal(terminal)) =
                        self.cards.insert(card, CardRuntime::Chat(runtime))
                    {
                        self.retire_switched_terminal(terminal, &session);
                    }
                    self.commit_chat_session(card, session);
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
                        if source.is_none() && !session.owns_codex_listener() {
                            // Nothing is running, so there is nothing to take
                            // over and no handoff to authorize: the same
                            // configuration opens a new conversation natively.
                            // Refusing here rejected the one case that needs no
                            // live process, which left a stopped agent terminal
                            // with no way into the chat at all.
                            //
                            // A new conversation is a new session, so it gets
                            // its own identity: the daemon will not implicitly
                            // replace a terminal record it still holds, and it
                            // holds this one until the card is closed. The
                            // stopped terminal stays named as this card's
                            // source, because that is what a switch back
                            // returns to and what proves which card this
                            // conversation belongs to. The stopped thread is
                            // deliberately not resumed, because joining an
                            // existing thread means joining whichever process
                            // holds it and no process holds this one.
                            let Some(recipe) =
                                crate::model::agent_recovery::fresh_recipe(&session.launch.recipe)
                            else {
                                self.status = Some(TransientNotice::error(
                                    "This terminal's launch options cannot be reused for a native conversation.",
                                ));
                                return Task::none();
                            };
                            // A carried bypass is named rather than applied
                            // quietly: it is the one launch option that decides
                            // the agent may act without asking.
                            let unrestricted = recipe.arguments.iter().any(|argument| {
                                argument == "--dangerously-bypass-approvals-and-sandbox"
                                    || argument == "--yolo"
                            });
                            session.launch.recipe = recipe;
                            session.launch.session_id = None;
                            session.id = crate::model::TerminalId::new();
                            self.status = Some(TransientNotice::warning(if unrestricted {
                                "This terminal's agent is not running, so a new conversation was opened here, with this terminal's approvals and sandbox left off."
                            } else {
                                "This terminal's agent is not running, so a new conversation was opened here."
                            }));
                            return self.start_chat(
                                card,
                                session,
                                chat::Intent::Start,
                                None,
                                false,
                            );
                        }
                        // The daemon reattaches its exact managed thread automatically.
                        // Unmanaged terminals are never matched by directory or recency.
                        let terminate = session.launch.provider != Provider::Codex;
                        return self.start_chat(
                            card,
                            session,
                            chat::Intent::Start,
                            source,
                            terminate,
                        );
                    }
                    Some(CardKind::Chat(_)) => {
                        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
                            return Task::none();
                        };
                        if runtime.session.launch.provider != Provider::Codex
                            && let Some(reason) = runtime.transition_blocker()
                        {
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
                        let _ = terminal;
                        self.chat_ui.handoffs.insert(target);
                        return cosmic::task::future(async move {
                            let result =
                                tokio::task::spawn_blocking(move || handle.return_terminal())
                                    .await
                                    .map_err(|error| error.to_string())
                                    .and_then(|result| result);
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
                    // The conversation started, but this card can no longer
                    // hold it. Leaving it alone left a daemon writer and its
                    // provider process running with nothing on screen able to
                    // reach them, so the start is undone here.
                    if let Ok(handle) = result {
                        return cosmic::task::future(async move {
                            let closed = tokio::task::spawn_blocking(move || handle.close()).await;
                            Message::ChatUi(Action::Discarded(
                                closed
                                    .map_err(|error| error.to_string())
                                    .and_then(|result| result),
                            ))
                        });
                    }
                    return Task::none();
                }
                match result {
                    Ok(handle) => {
                        self.chat_ui.fresh_offers.remove(&target.card);
                        let mut session = handle.session();
                        session.proof = handle.proof();
                        let snapshot = handle.snapshot();
                        let channel_name = snapshot_channel_name(&snapshot).map(str::to_owned);
                        let title = snapshot.title.clone();
                        let activity = Activity {
                            provider: session.launch.provider,
                            state: handle.take_activity(),
                        };
                        handle.take_observed();
                        let visible = self.workspace.visibility(target.card)
                            == Some(VisibilityClass::Visible);
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            if runtime.provider_closed {
                                runtime.model_picker = Some(String::new());
                            }
                            runtime.provider_closed = false;
                            runtime.target = target;
                            runtime.handle = handle;
                            runtime.session = session.clone();
                            runtime.disconnected = false;
                            runtime.pending = None;
                            runtime.error = None;
                            runtime.show(visible);
                        } else {
                            let mut runtime = Runtime::new(
                                target,
                                session.clone(),
                                handle,
                                visible,
                                self.settings.editor_syntax_theme,
                            );
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
                                self.retire_switched_terminal(terminal, &session);
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
                        if let Some(name) = channel_name {
                            self.name_channel_from_prompt(target.card, &name);
                        }
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        let control = match self.cards.get(&target.card) {
                            Some(CardRuntime::Chat(runtime)) => runtime.focus_control(),
                            _ => "chat-composer",
                        };
                        return iced::widget::operation::focus(card_control_id(
                            target.card,
                            control,
                        ))
                        .map(cosmic::Action::App);
                    }
                    Err(failure) => {
                        // A card that cannot resume has to say what refused it
                        // and offer the action that answers that refusal, so
                        // the classification travels with the message rather
                        // than being re-guessed from its wording.
                        if failure.recovery == chat::Recovery::Fresh {
                            self.chat_ui.fresh_offers.insert(target.card);
                        } else {
                            self.chat_ui.fresh_offers.remove(&target.card);
                        }
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            runtime.error = Some(failure.message);
                            runtime.disconnected = true;
                            self.observe_agent_activity_for_card(target.card, None);
                        } else if matches!(
                            self.card_state(target.card).map(|state| &state.kind),
                            Some(CardKind::Chat(_))
                        ) {
                            self.fail_runtime(target.card, failure.message.clone());
                            self.status = Some(TransientNotice::error(failure.message));
                        } else {
                            // Failed recognition/attachment must never replace or
                            // tear down the user's still-running terminal.
                            self.chat_ui.fresh_offers.remove(&target.card);
                            self.status = Some(TransientNotice::error(failure.message));
                        }
                        return self.drain_terminal(target.card, 0);
                    }
                }
            }
            Action::Discarded(result) => {
                // Nothing is on screen for this conversation, so only a
                // failure to end it is worth saying anything about.
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(format!(
                        "A conversation that no longer has a card could not be closed: {error}"
                    )));
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
                // Ready refreshes the new handle. A final wake from the old,
                // explicitly closed connection must not restore its identity.
                if self.chat_start_pending(target.card)
                    || self
                        .chat_runtime_mut(target)
                        .is_some_and(|runtime| runtime.provider_closed)
                {
                    return Task::none();
                }
                let needs_name = self.channel_needs_name(target.card);
                if let Some(runtime) = self.chat_ui.parked.get_mut(&target.card)
                    && runtime.target == target
                {
                    let name = if needs_name && matches!(wake, Wake::Changed) {
                        snapshot_channel_name(&runtime.handle.snapshot()).map(str::to_owned)
                    } else {
                        None
                    };
                    if let Wake::Disconnected(error) = wake {
                        runtime.disconnected = true;
                        runtime.error = Some(error);
                    }
                    if let Some(name) = name {
                        self.name_channel_from_prompt(target.card, &name);
                    }
                    return Task::none();
                }
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
                        let snapshot = runtime.handle.snapshot();
                        let channel_name = needs_name
                            .then(|| snapshot_channel_name(&snapshot).map(str::to_owned))
                            .flatten();
                        let title = snapshot.title.clone();
                        if let Some(name) = channel_name {
                            self.name_channel_from_prompt(target.card, &name);
                        }
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
                return self.start_chat(target.card, session, chat::Intent::Restore, None, false);
            }
            Action::StartFresh(card) => {
                let Some(CardKind::Chat(session)) = self.card_state(card).map(|state| &state.kind)
                else {
                    return Task::none();
                };
                if self.chat_start_pending(card) || self.chat_ui.closed.contains(&card) {
                    return Task::none();
                }
                // The new conversation runs the saved launch verbatim - same
                // program, same options including the session directory, same
                // working directory and environment - with only the
                // conversation it was resuming dropped. That is what makes
                // this a new conversation in the same place rather than a
                // different launch.
                let mut session = session.clone();
                session.launch.session_id = None;
                session.proof = None;
                self.chat_ui.fresh_offers.remove(&card);
                // A chat runtime whose provider has exited still holds the
                // daemon's record for the conversation that failed. Closing it
                // is what frees this chat's id for the fresh start.
                if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) {
                    let handle = runtime.handle.clone();
                    let target = runtime.target;
                    self.chat_ui.closing.insert(target);
                    self.chat_ui.restarting.insert(card, session);
                    return cosmic::task::future(async move {
                        let result = tokio::task::spawn_blocking(move || handle.close())
                            .await
                            .map_err(|error| error.to_string())
                            .and_then(|result| result);
                        Message::ChatUi(Action::CloseFinished(target, result))
                    });
                }
                self.cards.insert(card, CardRuntime::Loading);
                return self.start_chat(card, session, chat::Intent::Start, None, false);
            }
            Action::Composer(target, action) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if action.is_edit() {
                        runtime.revision = runtime.revision.wrapping_add(1);
                        // Typing into a recalled message makes it the draft,
                        // so the next Up starts again from the newest.
                        runtime.history_cursor = None;
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
                let command = match (mode, slash_invocation(&text, &runtime.snapshot)) {
                    (PromptMode::Send, Some(SlashInvocation::Command(command))) => {
                        if !runtime.attachments.is_empty() {
                            runtime.error = Some(
                                "Send attachments with a message, not a slash command.".into(),
                            );
                            return Task::none();
                        }
                        Command::Slash { command }
                    }
                    (PromptMode::Send, Some(SlashInvocation::Compact)) => {
                        if !runtime.attachments.is_empty() {
                            runtime.error =
                                Some("Send attachments with a message, not a compaction.".into());
                            return Task::none();
                        }
                        Command::Compact
                    }
                    // A skill is asked for the way the provider asks for it,
                    // whatever the reader typed: the slash form is the
                    // composer's, the invocation is the provider's.
                    (_, Some(SlashInvocation::Skill(text))) => Command::Prompt {
                        text,
                        attachments: runtime.attachments.clone(),
                        mode,
                    },
                    _ => Command::Prompt {
                        text,
                        attachments: runtime.attachments.clone(),
                        mode,
                    },
                };
                return self.chat_send(target, command, true);
            }
            Action::HistoryOlder(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.recall(true);
                }
            }
            Action::HistoryNewer(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.recall(false);
                }
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
                let first_prompt = match result {
                    Ok(()) => {
                        if let Some(mode) = pending.permission_mode {
                            runtime.permission_mode = Some(mode);
                        }
                        if pending.draft_revision == Some(runtime.revision) {
                            runtime.composer = text_editor::Content::new();
                            runtime.attachments.clear();
                            runtime.revision = runtime.revision.wrapping_add(1);
                        }
                        if let Some(sent) = pending.sent {
                            runtime.remember(sent);
                        }
                        if let Some(request) = pending.request {
                            runtime.answers.retain(|(id, _), _| id != &request);
                        }
                        runtime.refresh();
                        pending.first_prompt
                    }
                    Err(error) => {
                        let anchor = runtime.turns.last().map(|turn| turn.key.clone());
                        if runtime.failures.len() == 32 {
                            runtime.failures.remove(0);
                        }
                        runtime.failures.push((anchor, error.clone()));
                        runtime.error = Some(error);
                        None
                    }
                };
                if let Some(prompt) = first_prompt {
                    self.name_channel_from_prompt(target.card, &prompt);
                }
            }
            Action::Toggle(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if !runtime.expanded.remove(&id) {
                        runtime.expanded.insert(id.clone());
                    }
                    if let Some(item) = runtime.snapshot.items.iter().find(|item| item.id == id) {
                        let previous = runtime.cache.remove(&id);
                        runtime.cache.insert(
                            id.clone(),
                            CachedItem::updated(
                                &item.content,
                                runtime.syntax_theme,
                                previous,
                                runtime.expanded.contains(&id),
                            ),
                        );
                    }
                }
            }
            Action::ToggleTurn(target, key) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    // Every rendered group has a recorded state, written when
                    // the group was first seen, so a toggle inverts exactly
                    // what the reader is looking at. A key with no record
                    // names a group that no longer exists, and inventing a
                    // state for it would resurrect a strip under a group the
                    // transcript has moved past.
                    if let Some(open) = runtime.opened.get_mut(&key) {
                        *open = !*open;
                    }
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
            Action::OpenUrl(raw) => match Url::parse(&raw) {
                Ok(url) if matches!(url.scheme(), "https" | "http") => {
                    return self.open_clicked_link(url);
                }
                _ => {
                    self.status = Some(TransientNotice::error(
                        "Only explicit HTTP and HTTPS links can be opened from chat.",
                    ))
                }
            },
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
                // Choosing is what the search was opened for, so the popup
                // closes on the answer rather than waiting to be dismissed.
                runtime.model_picker = None;
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
                runtime.reasoning_picker = false;
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
                        || (name.trim_start_matches('/') == COMPACT_COMMAND
                            && runtime.snapshot.capabilities.compact)
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
                // The picker names a skill in the composer's slash form, and
                // the provider's own invocation is accepted as well, because
                // a reader who knows it can type it.
                if !runtime.snapshot.skills.iter().any(|skill| {
                    skill.invocation == invocation
                        || invocation
                            .strip_prefix('/')
                            .is_some_and(|name| skill.name.eq_ignore_ascii_case(name))
                }) {
                    return Task::none();
                }
                // A skill is named inside a message rather than instead of
                // one, so only the word the reader was typing is replaced and
                // whatever they had already written stays.
                let draft = mention_replaced(&runtime.composer.text(), &invocation);
                runtime.composer = text_editor::Content::with_text(&draft);
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
                if runtime.session.launch.provider != Provider::Codex
                    && let Some(reason) = runtime.transition_blocker()
                {
                    self.status = Some(TransientNotice::error(reason));
                    return Task::none();
                }
                runtime.session.launch.session_id = runtime.handle.snapshot().session_id.clone();
                let preserve = runtime.session.launch.provider == Provider::Codex
                    && !runtime.handle.snapshot().exited;
                if preserve {
                    runtime.session.terminal = Some(terminal.clone());
                }
                let session = runtime.session.clone();
                self.commit_chat_session(target.card, session);
                if preserve
                    && let Some(CardRuntime::Chat(mut runtime)) = self.cards.remove(&target.card)
                {
                    runtime.show(false);
                    self.chat_ui.parked.insert(target.card, runtime);
                }
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
                    self.chat_ui.restarting.remove(&target.card);
                    self.status = Some(TransientNotice::error(error));
                    return Task::none();
                }
                if let Some(session) = self.chat_ui.restarting.remove(&target.card) {
                    // Only the conversation is being given up here; the card
                    // stays and immediately starts its replacement, so it is
                    // never left showing a conversation that no longer exists.
                    let changing_provider = self.chat_runtime_mut(target).is_some_and(|runtime| {
                        if runtime.session.launch.provider == session.launch.provider {
                            return false;
                        }
                        // Keep the draft, attachments, and composer selection in
                        // this card while its empty connection changes accounts.
                        runtime.session = session.clone();
                        runtime.disconnected = true;
                        runtime.provider_closed = true;
                        let snapshot = Arc::make_mut(&mut runtime.snapshot);
                        snapshot.models.clear();
                        snapshot.selected_model = None;
                        snapshot.reasoning = None;
                        snapshot.session_id = None;
                        runtime.permission_mode = None;
                        true
                    });
                    if !changing_provider {
                        self.chat_forget(target.card);
                        self.cards.insert(target.card, CardRuntime::Loading);
                    }
                    self.commit_chat_session(target.card, session.clone());
                    return self.start_chat(target.card, session, chat::Intent::Start, None, false);
                }
                self.chat_ui.closed.insert(target.card);
                self.observe_agent_activity_for_card(target.card, None);
                return cosmic::task::message(Message::CommitClose(target.card));
            }
            Action::Scrolled(target, scroll) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                runtime.scroll = scroll;
                let (at_top, at_bottom) = (scroll.at_top(), scroll.at_bottom());
                // Leaving an end is what arms the page at it, so a reader who
                // scrolls away and comes back asks again, while one who stays
                // put asks once.
                if !at_top {
                    runtime.earlier_asked = None;
                }
                if !at_bottom {
                    runtime.later_asked = None;
                }
                let height = Some(scroll.content());
                // The earlier end wins where both are in reach at once, which
                // happens on a window shorter than the pane it is drawn in: a
                // reader whose transcript does not fill its own pane is
                // looking for the rest of the conversation, and what came
                // before this window is the part they cannot reach.
                if at_top && runtime.snapshot.has_earlier && runtime.earlier_asked != height {
                    runtime.earlier_asked = height;
                    return self.chat_send(target, Command::LoadEarlier, false);
                }
                if at_bottom && runtime.snapshot.has_later && runtime.later_asked != height {
                    runtime.later_asked = height;
                    return self.chat_send(target, Command::Refresh, false);
                }
            }
            Action::ModelPicker(target, open) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                // A reopened search starts empty: the query a reader typed
                // last time is not the model they are looking for now.
                runtime.model_picker = open.then(String::new);
                runtime.reasoning_picker = false;
                if open {
                    // The field takes focus with the popup, so opening the
                    // picker and naming a model are one gesture.
                    return widget::text_input::focus(card_control_id(
                        target.card,
                        "chat-model-search",
                    ))
                    .map(cosmic::Action::App);
                }
            }
            Action::ModelQuery(target, query) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(current) = runtime.model_picker.as_mut()
                {
                    *current = query;
                }
            }
            Action::ReasoningPicker(target, open) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.reasoning_picker = open;
                    runtime.model_picker = None;
                }
            }
        }
        Task::none()
    }
}

#[cfg(test)]
pub(super) fn refused_switch_message(app: &Cagents, card: CardId, message: &str) -> Message {
    Message::ChatUi(Action::Ready(
        pending_switch_target(app, card),
        Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
            message,
        )))))),
    ))
}

#[cfg(test)]
pub(super) fn pending_switch_target(app: &Cagents, card: CardId) -> Target {
    app.chat_ui
        .attempts
        .get(&card)
        .expect("switch staged an attempt")
        .target
}

#[cfg(test)]
pub(super) fn ready_message_for_pending_switch(
    app: &Cagents,
    card: CardId,
    snapshot: chat::Snapshot,
) -> Message {
    let attempt = app
        .chat_ui
        .attempts
        .get(&card)
        .expect("switch staged an attempt");
    let handle = Arc::new(ClientHandle::fixture(attempt.session.clone(), snapshot));
    Message::ChatUi(Action::Ready(
        attempt.target,
        Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
    ))
}

#[cfg(test)]
pub(super) fn show_chat_snapshot_for_test(
    app: &mut Cagents,
    card: CardId,
    snapshot: chat::Snapshot,
) -> usize {
    let requests = snapshot.requests.len();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("chat snapshot requires a chat runtime");
    };
    runtime.handle.replace_snapshot_for_test(snapshot);
    runtime.show(true);
    assert_eq!(
        runtime.snapshot.requests.len(),
        requests,
        "the drawn transcript has to be the snapshot just installed"
    );
    requests
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumed_and_cli_paired_snapshots_supply_channel_names() {
        let mut snapshot = Snapshot::default();
        assert_eq!(snapshot_channel_name(&snapshot), None);
        snapshot.items.push(chat::Item {
            id: "prompt".into(),
            role: chat::Role::User,
            content: text("Implement the requested feature"),
            at: None,
        });
        assert_eq!(
            snapshot_channel_name(&snapshot),
            Some("Implement the requested feature")
        );
        snapshot.title = Some("Provider conversation name".into());
        assert_eq!(
            snapshot_channel_name(&snapshot),
            Some("Provider conversation name")
        );
    }

    fn text(body: &str) -> ItemContent {
        ItemContent::Text {
            text: body.to_owned(),
            thinking: false,
            phase: chat::Phase::Final,
        }
    }

    /// The reader's view of a group's steps must change when they say so, and
    /// at no other time. A group that opened because it was running stays
    /// open when it finishes, and a group they closed stays closed while the
    /// agent keeps adding to it.
    #[test]
    fn a_groups_disclosure_survives_the_work_finishing_and_continuing() {
        let item = |id: &str, role: chat::Role, content: ItemContent| chat::Item {
            id: id.to_owned(),
            role,
            content,
            at: None,
        };
        let shell = |status: chat::ToolStatus| ItemContent::Command {
            label: String::new(),
            command: "cargo check".to_owned(),
            working_directory: String::new(),
            action: chat::CommandAction::Run,
            status,
            exit_code: None,
            duration_ms: None,
            output: String::new(),
        };
        let transcript = |status: chat::ToolStatus, extra: bool| {
            let mut items = vec![
                item("u1", chat::Role::User, text("look at this")),
                item("c1", chat::Role::Assistant, shell(status)),
            ];
            if extra {
                items.push(item(
                    "c2",
                    chat::Role::Assistant,
                    shell(chat::ToolStatus::Succeeded),
                ));
            }
            items
        };
        let mut opened = HashMap::new();
        let running = turn::group(
            &transcript(chat::ToolStatus::Running, false),
            &HashSet::new(),
        );
        reconcile_disclosure(&mut opened, &running);
        assert_eq!(
            opened.get("u1"),
            Some(&true),
            "a group with work in flight shows it without being asked"
        );
        let finished = turn::group(
            &transcript(chat::ToolStatus::Succeeded, false),
            &HashSet::new(),
        );
        reconcile_disclosure(&mut opened, &finished);
        assert_eq!(
            opened.get("u1"),
            Some(&true),
            "a group the reader never touched must not shut itself when the work ends"
        );
        // What `Action::ToggleTurn` does, which is the only other way this
        // state is allowed to change.
        *opened.get_mut("u1").expect("the group has a record") = false;
        let extended = turn::group(
            &transcript(chat::ToolStatus::Succeeded, true),
            &HashSet::new(),
        );
        reconcile_disclosure(&mut opened, &extended);
        assert_eq!(
            opened.get("u1"),
            Some(&false),
            "a group the reader closed stays closed when another step arrives"
        );
        reconcile_disclosure(&mut opened, &[]);
        assert!(
            opened.is_empty(),
            "a group the store no longer produces is forgotten rather than held for the session"
        );
    }

    #[test]
    fn a_failed_new_thread_can_choose_another_provider_in_the_same_card() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let directory = PathBuf::from("/workspace/chat-fixture");
        // Tasks are intentionally dropped: this exercises the UI lifecycle
        // without starting a provider or invoking a model.
        let _ = app.new_native_chat(Provider::Codex, Some(directory.clone()));
        let card = app.workspace.focused().unwrap();
        let attempt = app.chat_ui.attempts.get(&card).unwrap();
        let target = attempt.target;
        let session_id = attempt.session.id;
        let _ = app.handle_chat_ui(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
                "fixture: executable unavailable",
            )))))),
        ));
        assert!(app.can_choose_failed_chat_provider(card));
        let _ = app.handle_chat_ui(Action::ChooseFreshProvider(card, Provider::Claude));
        let attempt = app.chat_ui.attempts.get(&card).unwrap();
        assert_eq!(attempt.session.id, session_id);
        assert_eq!(attempt.session.launch.provider, Provider::Claude);
        assert_eq!(attempt.session.launch.recipe.working_directory, directory);
        assert_eq!(app.workspace.focused(), Some(card));
        assert!(!app.can_choose_failed_chat_provider(card));
    }

    #[test]
    fn established_failed_conversations_never_offer_provider_replacement() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let _ = app.new_native_chat(
            Provider::Codex,
            Some(PathBuf::from("/workspace/chat-fixture")),
        );
        let card = app.workspace.focused().unwrap();
        app.chat_ui.attempts.remove(&card);
        app.cards
            .insert(card, CardRuntime::Failed("fixture".into()));
        let state = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|state| state.id == card)
            .unwrap();
        let CardKind::Chat(session) = &mut state.kind else {
            unreachable!()
        };
        session.launch.session_id = Some("existing-thread".into());
        assert!(!app.can_choose_failed_chat_provider(card));
        let _ = app.handle_chat_ui(Action::ChooseFreshProvider(card, Provider::Claude));
        assert!(!app.chat_start_pending(card));
        assert!(
            matches!(&app.card_state(card).unwrap().kind, CardKind::Chat(session) if session.launch.provider == Provider::Codex)
        );
    }

    #[test]
    fn a_panic_while_starting_native_chat_is_a_failed_start() {
        let caught =
            std::panic::catch_unwind(|| -> Result<Arc<ClientHandle>, chat::StartFailure> {
                panic!("attach exploded");
            });
        let Err(failure) = Cagents::started_chat(caught) else {
            panic!("a panic is a start failure");
        };
        assert!(
            failure.message.contains("attach exploded"),
            "the panic payload has to reach the card: {}",
            failure.message
        );
        assert_eq!(failure.recovery, chat::Recovery::Retry);
    }

    #[test]
    fn codex_keeps_its_source_pty_even_when_the_daemon_record_is_not_marked_live() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};

        let mut card = crate::app::tests::terminal_card_state();
        let CardKind::Terminal(terminal) = &mut card.kind else {
            unreachable!();
        };
        terminal.continuity = TerminalContinuity::Live {
            host_session_id: HostSessionId::new("host"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        terminal.launch.program = "claude".into();
        let claude = conversion(terminal).expect("claude converts");
        assert!(!retains_source_pty(&claude));

        // Attach copies the daemon's stored terminal. That record keeps the
        // spawn continuity, which is not Live, while the child is still the
        // native writer's process.
        terminal.launch.program = "codex".into();
        terminal.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: terminal.launch.clone(),
            last_exit: None,
        };
        let attached = conversion(terminal).expect("codex converts");
        assert!(
            retains_source_pty(&attached),
            "a Codex attach must not ShutdownTerminal just because the stored continuity is not Live"
        );
    }

    #[test]
    fn switching_a_terminal_never_opens_a_conversation_chooser() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut card = crate::app::tests::terminal_card_state();
        let id = card.id;
        let CardKind::Terminal(terminal) = &mut card.kind else {
            unreachable!()
        };
        terminal.launch.program = "codex".into();
        terminal.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("host"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("incarnation"),
        };
        app.durable = Arc::new(WorkspaceState {
            focused: Some(id),
            cards: vec![card],
            ..WorkspaceState::default()
        });
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        let _ = app.update(Message::ChatUi(Action::Switch(id)));
    }

    /// Picking a skill replaces the word being typed and nothing else, for any
    /// text a reader can put in the composer. A byte offset taken from the last
    /// whitespace character would slice inside a no-break space and panic the
    /// whole window.
    #[test]
    fn choosing_a_skill_replaces_only_the_word_being_typed() {
        assert_eq!(mention_replaced("", "$commits"), "$commits ");
        assert_eq!(mention_replaced("/comm", "$commits"), "$commits ");
        assert_eq!(
            mention_replaced("please run /comm", "$commits"),
            "please run $commits "
        );
        assert_eq!(
            mention_replaced("quoted\u{a0}/comm", "$commits"),
            "quoted\u{a0}$commits "
        );
        assert_eq!(
            mention_replaced("first line\n/comm", "$commits"),
            "first line\n$commits "
        );
    }
    /// A message that arrives byte by byte must present exactly as one that
    /// arrived whole. The incremental fence scan is the only difference, and a
    /// mistake in it would show the wrong code behind a copy control rather
    /// than fail loudly.
    #[test]
    fn streamed_body_finds_the_same_code_blocks_as_a_whole_one() {
        const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
        const BODY: &str =
            "Intro\n\n```rust\nfn main() {}\nlet total = 1;\n```\n\nAfter\n\n~~~\nplain\n~~~\n";
        let whole = CachedItem::new(&text(BODY), THEME);
        assert_eq!(
            whole.code.codes,
            vec![
                "fn main() {}\nlet total = 1;\n".to_owned(),
                "plain\n".to_owned()
            ]
        );

        let mut streamed = CachedItem::new(&text(""), THEME);
        for end in 1..=BODY.len() {
            if !BODY.is_char_boundary(end) {
                continue;
            }
            let content = text(&BODY[..end]);
            let Some(body) = streamed.token.appended(&content) else {
                panic!("a longer prefix of the same body must read as an append");
            };
            let parsed = streamed.token.body;
            streamed.extend(body, parsed, THEME);
            streamed.token = ChangeToken::of(&content);
        }
        assert_eq!(streamed.code.codes, whole.code.codes);
        assert_eq!(streamed.token, whole.token);
        assert_eq!(
            streamed.blocks.keys().collect::<HashSet<_>>(),
            whole.blocks.keys().collect::<HashSet<_>>(),
            "the streamed fences are highlighted under the same identities"
        );
    }

    /// A fence that names its language is highlighted in both of the
    /// editor's palettes the moment it is cached, and looked up by exactly
    /// what the Markdown hands the viewer; a fence without one is left to
    /// draw plain rather than guessed at.
    #[test]
    fn a_named_fence_is_highlighted_in_both_palettes() {
        const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
        let cached = CachedItem::new(
            &text(
                "```rust
fn main() {}
```

```
plain
```
",
            ),
            THEME,
        );
        let lines = cached
            .block(
                Some("rust"),
                "fn main() {}
",
            )
            .expect("the Rust fence is highlighted");
        for palette in lines {
            assert_eq!(palette.len(), 1, "one line of spans per line of code");
            assert!(
                crate::diff_view::syntax_spans("fn main() {}", &palette[0])
                    .iter()
                    .any(|span| span.color.is_some()),
                "a keyword takes a color"
            );
        }
        assert!(
            cached
                .block(
                    None, "plain
"
                )
                .is_none()
        );
        assert!(
            cached
                .block(
                    Some("rust"),
                    "fn other() {}
"
                )
                .is_none()
        );
    }

    /// The token is what licenses reusing a parse, so it must refuse anything
    /// that is not bytes added to the end.
    #[test]
    fn change_token_refuses_a_rewritten_body() {
        const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
        let cached = CachedItem::new(&text("hello world"), THEME);
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
