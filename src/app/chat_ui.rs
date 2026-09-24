use super::*;
use crate::account::{Account, AccountId};
use crate::agent::{Activity, Provider};
use crate::app::account_ui::ChatAccounts;
use crate::chat::client::{ClientHandle, Wake};
use crate::chat::{self, Attachment, Command, ItemContent, PromptMode, Snapshot, clock_label};
use crate::file_drop;
use base64::Engine as _;
use cosmic::widget::{markdown, text_editor};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

mod completion;
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

/// Why a chat with no daemon handoff did not give its actor up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnRefused {
    /// The daemon still runs the conversation's CLI, which ends with the
    /// chat; only reconnecting and taking the daemon's handoff reaches it.
    SourceLive,
    /// Giving the actor up could not be confirmed.
    Unconfirmed(String),
}

#[derive(Clone)]
pub enum Action {
    ToolText(Target, String, CopyPart, text_editor::Action),
    /// Points a draft at a model of any provider, or at a provider's default
    /// model where none is named.
    DraftModel(Target, Provider, Option<String>),
    /// Points a draft at another account of its provider.
    ChooseAccount(Target, AccountId),
    /// Opens or closes the account choice.
    AccountPicker(Target, bool),
    /// Takes the next step of sending a draft's first message.
    DispatchQueued(Target),
    /// Takes back a draft's first message before its conversation has it,
    /// so the draft can be edited or moved again.
    CancelQueued(Target),
    Switch(CardId),
    Ready(Target, Ready),
    /// The result of giving up a conversation's daemon actor after its card
    /// stopped holding it: a closed card, an abandoned start, or a start that
    /// finished for a card which could no longer hold it.
    Retired(Target, Result<(), String>),
    /// A chat card with no daemon handoff to take has given up its actor, so
    /// a terminal can take its place.
    ReturnedLocally(Target, Result<(), ReturnRefused>),
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
    /// Opens a child agent's transcript, or returns to the main conversation.
    SelectSubagent(Target, Option<String>),
    Copy(Target, String, CopyPart),
    OpenUrl(String),
    Attach(Target),
    Attached(Target, Result<Option<Vec<PathBuf>>, String>),
    /// Files or text dropped on the conversation.
    Dropped(Target, file_drop::Payload),
    RemoveAttachment(Target, usize),
    Model(Target, String),
    Reasoning(Target, String, String),
    PermissionMode(Target, String),
    Field(Target, String, String, String),
    Choice(Target, String, String, String),
    Answer(Target, String, String),
    /// Puts a completion row's text over the word being completed. When the
    /// row is no longer offered, because the word changed under the key that
    /// chose it, the key does what it does without the list: the action
    /// given, if any.
    Complete(Target, String, Option<Box<Action>>),
    /// Moves the completion highlight down, or up when `false`.
    CompletionMove(Target, bool),
    /// Sends the completion list away until the reader types again.
    CompletionDismiss(Target),
    /// Where the completion list has been scrolled to.
    CompletionScrolled(Target, f32),
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
                | Self::ReturnedLocally(..)
                | Self::CloseFinished(..)
                | Self::Retired(..)
                | Self::DispatchQueued(..)
                | Self::AccountPicker(_, false)
        )
    }

    /// The card whose provider session this action starts afresh.
    fn starts_session(&self) -> Option<CardId> {
        match self {
            Self::Reconnect(target) => Some(target.card),
            Self::Switch(card) | Self::StartFresh(card) => Some(*card),
            _ => None,
        }
    }

    fn mutation_target(&self) -> Option<Target> {
        match self {
            Self::Composer(target, action) if action.is_edit() => Some(*target),
            Self::DraftModel(target, _, _)
            | Self::ChooseAccount(target, _)
            | Self::AccountPicker(target, _)
            | Self::Reconnect(target)
            | Self::Submit(target, _)
            | Self::AppendProviderDraft(target)
            | Self::Command(target, _)
            | Self::Attach(target)
            | Self::Dropped(target, _)
            | Self::RemoveAttachment(target, _)
            | Self::Model(target, _)
            | Self::Reasoning(target, _, _)
            | Self::PermissionMode(target, _)
            | Self::Field(target, _, _, _)
            | Self::Choice(target, _, _, _)
            | Self::Answer(target, _, _)
            | Self::Complete(target, _, _)
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

struct LocalReturn {
    /// The conversation being given up.
    session: chat::Session,
    /// What the terminal that takes the chat's place runs.
    launch: LaunchRecipe,
    /// Whether the terminal resumes the conversation, and so must not start
    /// beside an actor that may still hold it.
    resumes: bool,
}

pub(super) struct State {
    sender: async_channel::Sender<(Target, Wake)>,
    receiver: async_channel::Receiver<(Target, Wake)>,
    generation: u64,
    attempts: HashMap<CardId, Attempt>,
    handoffs: HashSet<Target>,
    closing: HashSet<Target>,
    /// Cards whose close has been accepted: their removal is reserved in the
    /// workspace and nothing may act on their conversation any more.
    closed: HashSet<CardId>,
    /// Cards closed while their conversation was returning to its terminal.
    ///
    /// That transition is a daemon request whose result has to land on the
    /// card, so the close runs as soon as it does rather than being dropped.
    close_after_handoff: HashSet<CardId>,
    /// Starts and restart closes still in flight for a card that has since
    /// been closed, with the conversation each one concerns.
    ///
    /// The card is gone as soon as the user asks, but whatever the request
    /// leaves in the daemon is only known when it answers, so the answer is
    /// what retires it.
    abandoned: HashMap<Target, chat::Session>,
    /// Conversations being given up in the background after their card left.
    retiring: HashSet<Target>,
    /// The terminal each card without an exact conversation to hand back
    /// becomes once its chat actor is given up, keyed like `handoffs`.
    local_returns: HashMap<Target, LocalReturn>,
    /// The terminal each returned chat card is replaced with once its removal
    /// commits.
    ///
    /// The chat leaves first so that no saved state, however it is cut short,
    /// holds the chat open beside a terminal resuming the same conversation.
    replacements: HashMap<CardId, LaunchRecipe>,
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
    /// The model last chosen for each provider, and its effort, which a new
    /// conversation of that provider starts on.
    last_models: HashMap<Provider, (String, Option<String>)>,
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
            close_after_handoff: HashSet::new(),
            abandoned: HashMap::new(),
            retiring: HashSet::new(),
            local_returns: HashMap::new(),
            replacements: HashMap::new(),
            drafts: HashMap::new(),
            parked: HashMap::new(),
            fresh_offers: HashSet::new(),
            last_models: HashMap::new(),
            restarting: HashMap::new(),
        }
    }
}

impl State {
    pub(super) fn transition_pending(&self) -> bool {
        !self.attempts.is_empty()
            || !self.handoffs.is_empty()
            || !self.closing.is_empty()
            || !self.abandoned.is_empty()
            || !self.retiring.is_empty()
    }

    /// A target no runtime or attempt holds, for a request of its own.
    fn next_target(&mut self, card: CardId) -> Target {
        self.generation = self.generation.wrapping_add(1).max(1);
        Target {
            card,
            generation: self.generation,
        }
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
    /// The model this command chooses, remembered for new drafts of the
    /// provider once the provider has accepted it.
    model: Option<(String, Option<String>)>,
    /// The message this command sends, kept for the composer's history once
    /// the provider has accepted it. A rejected send is not history: the
    /// draft it came from is still in the composer.
    sent: Option<String>,
    /// Which end of the history this command is extending, where it is a
    /// page request at all. The transcript says so at the edge the rows will
    /// appear at, so it has to know which edge is waiting.
    page: Option<view::Page>,
    /// The step of a queued first message this command takes, if it is one.
    queued: Option<QueuedStep>,
}
/// What a chat card's conversation runs through.
enum Connection {
    /// Nothing runs yet. A new conversation starts its CLI when its first
    /// message is sent, so until then the provider, model and account are
    /// choices that cost nothing to change, and an unused card leaves no
    /// process behind.
    Draft,
    Live(Arc<ClientHandle>),
}

/// A draft's first message, held while its conversation starts.
///
/// A provider does not take commands the moment its connection opens:
/// Claude lists its models on initializing, Codex once its thread is attached
/// and OMP once its history has loaded. The message waits here until the
/// conversation takes it, and returns to the composer if it never does.
struct Queued {
    text: String,
    attachments: Vec<Attachment>,
    /// The model the draft chose, to be selected before the message is sent.
    /// `None` once it has been, or when the provider's default was chosen.
    model: Option<(String, Option<String>)>,
    /// The draft as its start was asked for, which the card goes back to if
    /// the conversation never takes the message.
    draft: chat::Session,
    /// When the provider was first seen ready, from which the wait for it to
    /// list the chosen model is bounded.
    since: Option<Instant>,
    /// Whether a look back once a wait is over is already scheduled.
    rechecking: bool,
}
impl Queued {
    /// Brings the queue back to [`Cagents::dispatch_queued`] after `wait`,
    /// unless a look back is already scheduled.
    fn look_again(&mut self, target: Target, wait: Duration) -> Task<Message> {
        if std::mem::replace(&mut self.rechecking, true) {
            return Task::none();
        }
        cosmic::task::future(async move {
            tokio::time::sleep(wait).await;
            Message::ChatUi(Action::DispatchQueued(target))
        })
    }
}

/// Which step of sending a queued first message a command is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueuedStep {
    Model,
    Prompt,
}

pub(super) struct Runtime {
    pub(super) target: Target,
    pub(super) session: chat::Session,
    connection: Connection,
    /// The first message of a draft, while its conversation starts.
    queued: Option<Queued>,
    snapshot: Arc<Snapshot>,
    cache: HashMap<String, CachedItem>,
    /// The palette the cache's code blocks are highlighted in.
    syntax_theme: crate::settings::EditorSyntaxTheme,
    /// The snapshot's items grouped into stretches of work, rebuilt only when
    /// the snapshot changes rather than once per frame.
    turns: Vec<turn::Turn>,
    selected_subagent: Option<String>,
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
    /// A fingerprint of the models this conversation last listed to the
    /// catalog, which takes each list once rather than on every change.
    listed_models: u64,
    visible: AtomicBool,
    error: Option<String>,
    /// Local send errors anchored to the exchange that produced them.
    failures: Vec<(Option<String>, String)>,
    reasoning_picker: bool,
    /// Whether the account choice is open.
    account_picker: bool,
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
    /// The composer's completion list.
    completion: completion::State,
}
/// The name the composer accepts for compaction where the provider offers it
/// as a capability rather than as one of its listed commands.
pub(super) const COMPACT_COMMAND: &str = "compact";

/// What a draft beginning with `/` asks for, when it is not a message.
enum SlashInvocation {
    /// One of the provider's own commands, sent through its dispatcher.
    Command(String),
    /// Compaction, offered as a command wherever the provider can do it.
    Compact,
}

/// Resolves a draft's leading `/name` against the provider's commands.
///
/// Names match without regard to case and to the `/` the provider prefixes
/// its own form with, because a reader completing from the picker and a
/// reader typing from memory should reach the same command. A skill is not a
/// command: it is named inside a message, so a draft opening with one is sent
/// as a message, with the skills in it spelled by [`completion::invocations`].
fn slash_invocation(draft: &str, snapshot: &Snapshot) -> Option<SlashInvocation> {
    let draft = draft.trim();
    let rest = draft.strip_prefix(completion::COMMAND_SIGIL)?;
    let name = &rest[..rest.find(char::is_whitespace).unwrap_or(rest.len())];
    if name.is_empty() {
        return None;
    }
    if snapshot.commands.iter().any(|command| {
        command
            .name
            .trim_start_matches(completion::COMMAND_SIGIL)
            .eq_ignore_ascii_case(name)
    }) {
        return Some(SlashInvocation::Command(draft.to_owned()));
    }
    if name.eq_ignore_ascii_case(COMPACT_COMMAND) && snapshot.capabilities.compact {
        return Some(SlashInvocation::Compact);
    }
    None
}

/// What the composer sends for `text`, or why it will not.
fn submission(
    text: &str,
    snapshot: &Snapshot,
    attachments: &[Attachment],
    mode: PromptMode,
) -> Result<Command, &'static str> {
    match (mode, slash_invocation(text, snapshot)) {
        (PromptMode::Send, Some(SlashInvocation::Command(_))) if !attachments.is_empty() => {
            Err("Send attachments with a message, not a slash command.")
        }
        (PromptMode::Send, Some(SlashInvocation::Command(command))) => {
            Ok(Command::Slash { command })
        }
        (PromptMode::Send, Some(SlashInvocation::Compact)) if !attachments.is_empty() => {
            Err("Send attachments with a message, not a compaction.")
        }
        (PromptMode::Send, Some(SlashInvocation::Compact)) => Ok(Command::Compact),
        // A skill is asked for the way the provider asks for it, whatever the
        // reader typed: the slash form is the composer's, the invocation is
        // the provider's.
        _ => Ok(Command::Prompt {
            text: completion::invocations(
                completion::from_leading_skill(text, &snapshot.skills),
                &snapshot.skills,
            )
            .into_owned(),
            attachments: attachments.to_vec(),
            mode,
        }),
    }
}

impl Runtime {
    /// The word the caret is naming something with and the rows offered for
    /// it, or `None` while there is nothing to offer.
    fn completion(&self) -> Option<(completion::Token, Vec<completion::Candidate<'_>>)> {
        let caret = self.composer.cursor();
        // A selection is text the reader is about to act on, not a word they
        // are still spelling.
        if caret.selection.is_some() {
            return None;
        }
        let line = self.composer.line(caret.position.line)?;
        let mut token = completion::Token::at(
            &line.text,
            caret.position,
            &completion::sigils(&self.snapshot),
            || {
                (0..caret.position.line).all(|earlier| {
                    self.composer
                        .line(earlier)
                        .is_some_and(|line| line.text.trim().is_empty())
                })
            },
        )?;
        if self.completion.dismissed == Some((token.line, token.start)) {
            return None;
        }
        let rows = completion::candidates(&self.snapshot, &token);
        token.fit(&rows);
        (!rows.is_empty()).then_some((token, rows))
    }

    /// Puts `insertion` over the word being completed when it is one of the
    /// rows offered for that word, leaving the rest of the draft as it was.
    ///
    /// Enter chose the row by what the list showed when it was drawn, and
    /// keys typed since may have spelled the word out: it only completes
    /// while it still would choose that row.
    fn complete(&mut self, insertion: &str, by_enter: bool) -> bool {
        let Some(token) = self.completion().and_then(|(token, rows)| {
            rows.iter()
                .any(|row| {
                    row.insertion == insertion
                        && (!by_enter
                            || self
                                .completion
                                .enter_chooses(&token, row, &self.snapshot.skills))
                })
                .then_some(token)
        }) else {
            return false;
        };
        completion::apply(&mut self.composer, &token, insertion);
        self.completion = completion::State::default();
        self.revision = self.revision.wrapping_add(1);
        self.history_cursor = None;
        true
    }

    /// Adds files to the next message, each at most once.
    fn attach(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let before = self.attachments.len();
        for path in paths {
            if !self
                .attachments
                .iter()
                .any(|attachment| attachment.path == path)
            {
                self.attachments.push(Attachment { path });
            }
        }
        if self.attachments.len() != before {
            self.revision = self.revision.wrapping_add(1);
        }
    }

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
        self.replace_draft(text_editor::Content::with_text(text.trim_end_matches('\n')));
    }

    /// Puts a whole new draft in the composer. Whatever the completion list
    /// was doing belonged to the old one.
    fn replace_draft(&mut self, composer: text_editor::Content<cosmic::Renderer>) {
        self.composer = composer;
        self.completion = completion::State::default();
        self.revision = self.revision.wrapping_add(1);
    }

    fn new(
        target: Target,
        session: chat::Session,
        connection: Connection,
        visible: bool,
        syntax_theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let mut runtime = Self {
            target,
            session,
            snapshot: Arc::new(Snapshot::default()),
            connection,
            queued: None,
            cache: HashMap::new(),
            syntax_theme,
            turns: Vec::new(),
            selected_subagent: None,
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
            listed_models: 0,
            visible: AtomicBool::new(visible),
            error: None,
            failures: Vec::new(),
            reasoning_picker: false,
            account_picker: false,
            permission_mode: None,
            history: Vec::new(),
            history_cursor: None,
            history_stash: String::new(),
            scroll: view::Scroll::default(),
            earlier_asked: None,
            later_asked: None,
            model_picker: None,
            completion: completion::State::default(),
        };
        runtime.refresh();
        runtime
    }
    /// Drops everything drawn from the conversation's transcript, for a card
    /// going back to a draft, which has none.
    fn forget_transcript(&mut self) {
        self.cache.clear();
        self.turns.clear();
        self.selected_subagent = None;
        self.asked_after.clear();
        self.asked.clear();
        self.opened.clear();
        self.expanded.clear();
        self.answers.clear();
        self.failures.clear();
        self.listed_models = 0;
        self.earlier_asked = None;
        self.later_asked = None;
    }
    /// Records a notice in the transcript, after the exchange it concerns.
    fn note(&mut self, notice: String) {
        let anchor = self.turns.last().map(|turn| turn.key.clone());
        if self.failures.len() == 32 {
            self.failures.remove(0);
        }
        self.failures.push((anchor, notice));
    }
    /// Puts a first message that was never sent back in the composer, ahead
    /// of anything written since, with its attachments.
    fn unqueue(&mut self) -> Option<chat::Session> {
        let queued = self.queued.take()?;
        // Nothing waits on the snapshot of a hidden card any more; a
        // connection that has gone has nothing to be told.
        let _ = self.present();
        let written = self.composer.text();
        let text = if written.trim().is_empty() {
            queued.text
        } else {
            format!("{}\n{written}", queued.text.trim_end())
        };
        self.replace_draft(text_editor::Content::with_text(&text));
        let written = std::mem::replace(&mut self.attachments, queued.attachments);
        self.attach(written.into_iter().map(|attachment| attachment.path));
        Some(queued.draft)
    }
    /// A conversation that has not started, showing `snapshot` until it does.
    fn draft(
        target: Target,
        session: chat::Session,
        snapshot: Snapshot,
        visible: bool,
        syntax_theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let mut runtime = Self::new(target, session, Connection::Draft, visible, syntax_theme);
        runtime.snapshot = Arc::new(snapshot);
        runtime
    }
    /// The live connection, once the conversation has started.
    fn handle(&self) -> Option<&Arc<ClientHandle>> {
        match &self.connection {
            Connection::Draft => None,
            Connection::Live(handle) => Some(handle),
        }
    }
    /// Whether nothing has started yet, a start for the first message
    /// included.
    fn is_draft(&self) -> bool {
        matches!(self.connection, Connection::Draft)
    }
    /// Whether this draft's provider, model and account can still change: it
    /// has not started, and its first message is not on its way.
    fn choosing(&self) -> bool {
        self.is_draft() && self.queued.is_none() && !self.attachment_chooser_pending
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
        let Some(handle) = self.handle() else {
            return;
        };
        let snapshot = handle.snapshot();
        if self.snapshot.generation == snapshot.generation && Arc::ptr_eq(&self.snapshot, &snapshot)
        {
            return;
        }
        let live_ids: HashSet<&str> = snapshot
            .items
            .iter()
            .chain(
                snapshot
                    .subagents
                    .iter()
                    .flat_map(|agent| agent.items.iter()),
            )
            .map(|item| item.id.as_str())
            .collect();
        if self
            .selected_subagent
            .as_ref()
            .is_some_and(|id| !snapshot.subagents.iter().any(|agent| &agent.id == id))
        {
            self.selected_subagent = None;
        }
        self.cache.retain(|id, _| live_ids.contains(id.as_str()));
        // Both sets are keyed by item id and the store drops items past its
        // window, so without this they grow for the life of the card: one
        // entry per step the reader ever opened and per question they were
        // ever asked, held after the item it named stopped existing.
        self.expanded.retain(|id| live_ids.contains(id.as_str()));
        self.asked_after.retain(|id| live_ids.contains(id.as_str()));
        for item in snapshot.items.iter().chain(
            snapshot
                .subagents
                .iter()
                .flat_map(|agent| agent.items.iter()),
        ) {
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
            self.replace_draft(text_editor::Content::with_text(text));
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
        self.present()
    }
    /// Tells the connection whether to keep the card's snapshot current.
    ///
    /// A first message waiting to be sent keeps it current while the card is
    /// hidden, since its provider becoming ready, and the models it lists,
    /// reach the card only through that snapshot.
    fn present(&self) -> Result<(), String> {
        let visible = self.visible.load(Ordering::Relaxed) || self.queued.is_some();
        self.handle()
            .map_or(Ok(()), |handle| handle.set_visible(visible))
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
    /// Whether this runtime still has a daemon connection to its own actor.
    fn connected(&self) -> bool {
        self.handle().is_some() && !self.disconnected
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
        } else if self.queued.is_some() {
            Some(QUEUED_BLOCKER)
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
    /// Why this chat cannot give its conversation back to its terminal right
    /// now.
    ///
    /// A Codex thread keeps running across the handoff, with whatever command
    /// it was answering, so only a first message still waiting for its
    /// conversation holds it back; the terminal would never send it.
    fn handoff_blocker(&self) -> Option<&'static str> {
        if self.session.launch.provider == Provider::Codex {
            self.queued.is_some().then_some(QUEUED_BLOCKER)
        } else {
            self.transition_blocker()
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

/// Whether `recipe` lets its agent act without asking. A new conversation
/// that carries this over from a terminal names it rather than apply it
/// quietly, since it is the one launch option that decides that.
fn bypasses_approvals(recipe: &LaunchRecipe) -> bool {
    let arguments = &recipe.arguments;
    arguments.iter().enumerate().any(|(index, argument)| {
        matches!(
            argument.as_str(),
            "--dangerously-bypass-approvals-and-sandbox"
                | "--yolo"
                | "--dangerously-skip-permissions"
                | "--permission-mode=bypassPermissions"
        ) || (argument == "--permission-mode"
            && arguments
                .get(index + 1)
                .is_some_and(|mode| mode == "bypassPermissions"))
    }) || recipe
        .environment
        .get("CLAUDE_CODE_PERMISSION_MODE")
        .is_some_and(|mode| mode == "bypassPermissions")
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

/// Gives up a conversation's daemon actor, whatever state its card last saw.
///
/// A live handle is asked first, because a connected client closes its own
/// actor, and the source terminal it was converted from, in one request.
/// Whatever that leaves in doubt - no handle, a disconnected one, a close that
/// failed - is settled against the daemon's inventory: a conversation the
/// daemon no longer lists is closed, and one it does list is attached and
/// closed by identity. `paired` extends that to the source terminal.
fn retire_conversation(
    handle: Option<Arc<ClientHandle>>,
    session: Option<chat::Session>,
) -> Result<(), String> {
    let refused = match handle.map(|handle| handle.close()) {
        Some(Ok(())) => return Ok(()),
        Some(Err(error)) => Some(error),
        None => None,
    };
    let Some(session) = session else {
        return refused.map_or(Ok(()), Err);
    };
    super::channel_ui::close_sessions(vec![CardKind::Chat(session)]).map_err(
        |error| match refused {
            Some(refused) => format!("{refused}; {error}"),
            None => error,
        },
    )
}

/// The CLI the daemon still runs for a Codex thread, if any.
///
/// A Codex chat and its CLI are two views of one thread, and the daemon ends
/// the CLI together with the chat. Only the daemon's handoff can go back to
/// it: giving the chat up first would kill the very terminal being returned
/// to, along with any turn it is running.
fn live_codex_source(chat: TerminalId) -> Result<Option<TerminalSession>, String> {
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client = match crate::control::ControlClient::connect(&socket) {
        Ok(client) => client,
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("Cannot reach the daemon: {error}")),
    };
    Ok(client
        .list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|terminal| terminal.session)
        .find(|terminal| terminal.id == chat && terminal.continuity.live_proof().is_some()))
}

/// What the terminal replacing a chat runs once its actor is gone.
///
/// A conversation the provider named resumes in its CLI, exactly as the
/// daemon's own return would start it. Anything else becomes a shell in the
/// directory the conversation was working in, which is the terminal the card
/// stood in for.
fn local_return_launch(session: &chat::Session) -> Result<LaunchRecipe, String> {
    if session.launch.session_id.is_none() {
        let mut shell = LaunchRecipe::shell(session.launch.recipe.working_directory.clone())
            .map_err(|error| format!("Could not open a terminal here: {error}"))?;
        if let Some(variable) = account::home_variable(session.launch.provider)
            && let Some(home) = session.launch.recipe.environment.get(variable)
        {
            shell.environment.insert(variable.to_owned(), home.clone());
        }
        return Ok(shell);
    }
    session
        .launch
        .terminal_recipe()
        .ok_or_else(|| "Cannot form a safe exact-session terminal resume command.".to_owned())
}

/// A new conversation for `provider` on `account`.
///
/// An account is chosen by pointing the provider's CLI at its home
/// through the recipe's environment, which the daemon lays over its own and
/// which travels with the conversation: resuming or recovering it later
/// starts the CLI on the same home, never on whatever the default has become
/// since. Even the system home is recorded explicitly, because the daemon
/// may have inherited a different environment from the interface.
fn fresh_chat_session(
    provider: Provider,
    id: TerminalId,
    working_directory: PathBuf,
    account: &Account,
) -> Result<chat::Session, String> {
    let mut recipe = LaunchRecipe {
        program: provider.executable().into(),
        arguments: Vec::new(),
        working_directory,
        environment: crate::model::agent_recovery::fresh_native_environment(provider, true),
    };
    if account.provider == provider
        && let (Some(variable), Some(home)) =
            (account::home_variable(provider), account.resolved_home())
    {
        let home = home
            .into_os_string()
            .into_string()
            .map_err(|_| "The account's home is not a path a conversation can record".to_owned())?;
        recipe.environment.insert(variable.to_owned(), home);
    }
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

/// What a draft for `provider` shows before anything runs: the models the
/// provider listed last, the one chosen among them, and the controls every
/// conversation of that provider offers.
fn draft_snapshot(
    provider: Provider,
    models: Vec<chat::Model>,
    selected_model: Option<String>,
    reasoning: Option<String>,
) -> Snapshot {
    Snapshot {
        capabilities: chat::Capabilities {
            // Codex takes an image only on a model that reads one, which it
            // says once the conversation has started.
            attachments: provider != Provider::Codex,
            model_selection: true,
            reasoning_selection: true,
            ..chat::Capabilities::default()
        },
        models,
        selected_model,
        reasoning,
        ..Snapshot::default()
    }
}

/// The words a chat whose first message is still waiting to be sent refuses
/// to be left with.
const QUEUED_BLOCKER: &str = "Wait for the first message to be sent before leaving the chat.";

/// How long a queued first message waits, once its provider is ready, for
/// the conversation to list the model it was meant to be sent to, before
/// going to the provider's default instead.
const MODEL_PATIENCE: Duration = Duration::from_secs(10);

impl Cagents {
    /// Points a draft at `model` of `provider`, or at that provider's
    /// default where `model` is `None`.
    ///
    /// Nothing runs yet, so nothing is closed or started: a move to another
    /// provider only records, on that provider's default account, what the
    /// first message will start.
    fn choose_draft_model(
        &mut self,
        target: Target,
        provider: Provider,
        model: Option<String>,
    ) -> Task<Message> {
        let models = self
            .account_ui
            .catalog(provider)
            .map(|catalog| catalog.models.clone())
            .unwrap_or_default();
        let Some(directory) = self
            .chat_runtime_mut(target)
            .map(|runtime| runtime.session.launch.recipe.working_directory.clone())
        else {
            return Task::none();
        };
        let account = self.default_account_for_directory(provider, &directory);
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.choosing() {
            return Task::none();
        }
        runtime.model_picker = None;
        let moved = if runtime.session.launch.provider == provider {
            None
        } else {
            match fresh_chat_session(
                provider,
                runtime.session.id,
                runtime.session.launch.recipe.working_directory.clone(),
                &account,
            ) {
                Ok(session) => Some(session),
                Err(error) => {
                    runtime.error = Some(error);
                    return Task::none();
                }
            }
        };
        // An effort carries over to a model that offers it, which is what a
        // reader moving between two models of one family expects.
        let reasoning = runtime.snapshot.reasoning.clone().filter(|reasoning| {
            models.iter().any(|candidate| {
                Some(&candidate.id) == model.as_ref() && candidate.reasoning.contains(reasoning)
            })
        });
        runtime.snapshot = Arc::new(draft_snapshot(
            provider,
            models,
            model.clone(),
            reasoning.clone(),
        ));
        if let Some(session) = &moved {
            runtime.session = session.clone();
            runtime.error = None;
        }
        self.remember_chat_model(provider, model, reasoning);
        if let Some(session) = moved {
            self.replace_chat_conversation(target.card, session);
        }
        iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
            .map(cosmic::Action::App)
    }

    /// Starts the next conversation of `provider` on `model` at `reasoning`,
    /// or on the provider's default where no model is named.
    fn remember_chat_model(
        &mut self,
        provider: Provider,
        model: Option<String>,
        reasoning: Option<String>,
    ) {
        match model {
            Some(model) => {
                self.chat_ui
                    .last_models
                    .insert(provider, (model, reasoning));
            }
            None => {
                self.chat_ui.last_models.remove(&provider);
            }
        }
    }

    /// What a new draft of `provider` shows: the models it listed last, and
    /// the one last chosen among them.
    fn chat_draft_snapshot(&self, provider: Provider) -> Snapshot {
        let models = self
            .account_ui
            .catalog(provider)
            .map(|catalog| catalog.models.clone())
            .unwrap_or_default();
        let (model, reasoning) = self
            .chat_ui
            .last_models
            .get(&provider)
            .cloned()
            .map_or((None, None), |(model, reasoning)| (Some(model), reasoning));
        draft_snapshot(provider, models, model, reasoning)
    }

    /// Stops offering `model` to new drafts of `provider`, as a conversation
    /// of that provider would not take it.
    fn forget_chat_model(&mut self, provider: Provider, model: &str) {
        if self
            .chat_ui
            .last_models
            .get(&provider)
            .is_some_and(|(remembered, _)| remembered == model)
        {
            self.chat_ui.last_models.remove(&provider);
        }
    }

    /// Points a draft at account `id` of its own provider.
    fn choose_chat_account(&mut self, target: Target, id: AccountId) -> Task<Message> {
        let Some(account) = self.known_account(&id) else {
            return Task::none();
        };
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        runtime.account_picker = false;
        if !runtime.choosing() || account.provider != runtime.session.launch.provider {
            return Task::none();
        }
        let session = match fresh_chat_session(
            account.provider,
            runtime.session.id,
            runtime.session.launch.recipe.working_directory.clone(),
            &account,
        ) {
            Ok(session) => session,
            Err(error) => {
                runtime.error = Some(error);
                return Task::none();
            }
        };
        if session == runtime.session {
            return Task::none();
        }
        runtime.session = session.clone();
        self.replace_chat_conversation(target.card, session);
        Task::none()
    }

    /// Brings the models every draft offers up to date with the catalog.
    pub(super) fn refresh_chat_drafts(&mut self) {
        let catalogs = &self.account_ui;
        for card in self.cards.values_mut() {
            let CardRuntime::Chat(runtime) = card else {
                continue;
            };
            if !runtime.is_draft() {
                continue;
            }
            let provider = runtime.session.launch.provider;
            let Some(catalog) = catalogs.catalog(provider) else {
                continue;
            };
            if runtime.snapshot.models != catalog.models {
                Arc::make_mut(&mut runtime.snapshot).models = catalog.models.clone();
            }
        }
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
        let open = runtime.model_picker.take().is_some()
            || runtime.reasoning_picker
            || runtime.account_picker;
        runtime.reasoning_picker = false;
        runtime.account_picker = false;
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
        let Some(working_directory) = directory.or_else(|| self.terminal_directory()) else {
            return cosmic::task::message(Message::OpenRepository);
        };
        if self.refuse_in_deleting_worktree(&working_directory) {
            return Task::none();
        }
        let account = self.default_account_for_directory(provider, &working_directory);
        let session = match fresh_chat_session(
            provider,
            TerminalId::new(),
            working_directory.clone(),
            &account,
        ) {
            Ok(session) => session,
            Err(error) => {
                self.status = Some(TransientNotice::error(error));
                return Task::none();
            }
        };
        let scope = if self.active_workspace.as_deref() != Some(working_directory.as_path()) {
            self.select_worktree(working_directory, false)
        } else {
            Task::none()
        };
        Task::batch([scope, self.insert_chat_card(session, None)])
    }

    /// Opens a card for the new conversation `session`, as a draft whose first
    /// message starts it: beside the card it was begun from, filed where that
    /// card is, or else where a new terminal would go.
    pub(super) fn insert_chat_card(
        &mut self,
        session: chat::Session,
        beside: Option<CardId>,
    ) -> Task<Message> {
        let beside = beside.and_then(|source| {
            let index = self.workspace.order().position(|id| id == source)?;
            let durable = self
                .durable
                .cards
                .iter()
                .position(|state| state.id == source)?;
            Some((source, (index + 1, durable + 1)))
        });
        let card_id = CardId::new();
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: chat_card_title(session.launch.provider).into(),
            width_fraction: beside
                .and_then(|(source, _)| self.card_state(source))
                .map_or(self.settings.default_column_fraction, |source| {
                    source.width_fraction
                }),
            existence: Existence::Open,
            exposure: Exposure::Visible,
            residency: Residency::Hot,
            kind: CardKind::Chat(session.clone()),
        };
        let (index, durable_index) = beside
            .map(|(_, slot)| slot)
            .unwrap_or_else(|| self.terminal_insertion_indices());
        let durable = Arc::make_mut(&mut self.durable);
        if let Some(scope) =
            beside.and_then(|(source, _)| durable.card_scopes.get(&source).cloned())
        {
            durable.card_scopes.insert(card_id, scope);
        }
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
        self.open_chat_draft(card_id, session);
        self.persist();
        Task::batch([
            self.apply_viewport(update, true, false),
            iced::widget::operation::focus(card_control_id(card_id, "chat-composer"))
                .map(cosmic::Action::App),
        ])
    }

    /// Opens the conversation card `card` holds: its draft, when nothing has
    /// ever run it, or else a connection to it.
    ///
    /// A draft restored from an earlier run may have been started without
    /// that start ever being answered, which leaves an actor with nothing on
    /// screen able to reach it. Such an actor never took a message, so it is
    /// given up quietly by identity; the draft's first message starts under
    /// a new identity, never racing that.
    pub(super) fn open_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
        intent: chat::Intent,
    ) -> Task<Message> {
        if !session.is_unstarted() {
            return self.start_chat(card, session, intent, None, false);
        }
        let retire = if intent == chat::Intent::Restore {
            self.retire_chat(card, None, Some(session.clone()))
        } else {
            Task::none()
        };
        self.open_chat_draft(card, session);
        retire
    }

    /// Puts a draft of `session` in card `card`, on the model last chosen for
    /// its provider.
    fn open_chat_draft(&mut self, card: CardId, session: chat::Session) {
        let snapshot = self.chat_draft_snapshot(session.launch.provider);
        let target = self.chat_ui.next_target(card);
        let visible = self.workspace.visibility(card) == Some(VisibilityClass::Visible);
        let mut runtime = Runtime::draft(
            target,
            session,
            snapshot,
            visible,
            self.settings.editor_syntax_theme,
        );
        if let Some(draft) = self.chat_ui.drafts.remove(&card)
            && !draft.is_empty()
        {
            runtime.replace_draft(text_editor::Content::with_text(&draft));
        }
        // Nothing runs, so there is nothing for the lifecycle to hydrate or
        // hibernate: the card is whole as it stands.
        self.lifecycle.remove(card);
        if let Some(state) = self.card_state_mut(card) {
            state.residency = Residency::Hot;
        }
        self.card_chrome.remove(&card);
        self.cards
            .insert(card, CardRuntime::Chat(Box::new(runtime)));
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
        self.chat_ui
            .local_returns
            .retain(|target, _| target.card != card);
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
        // A start still in flight is not dropped with the card: whatever it
        // leaves in the daemon is given up when it answers, as for a close.
        if let Some(attempt) = self.chat_ui.attempts.remove(&card) {
            self.chat_ui
                .abandoned
                .insert(attempt.target, attempt.session);
        }
        self.chat_forget(card);
        self.chat_ui.drafts.remove(&card);
        self.chat_ui.parked.remove(&card);
        self.chat_ui.fresh_offers.remove(&card);
        self.chat_ui.closed.remove(&card);
        self.chat_ui.close_after_handoff.remove(&card);
        self.chat_ui.replacements.remove(&card);
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
        match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(session)) => {
                let named = match self.cards.get(&card) {
                    Some(CardRuntime::Chat(runtime)) => {
                        runtime.session.launch.session_id.is_some()
                            || runtime.snapshot.session_id.is_some()
                    }
                    _ => session.launch.session_id.is_some(),
                };
                // A conversation the provider never named has no terminal to
                // go back to, only a place to open one.
                if named {
                    "Return to terminal"
                } else {
                    "Switch to a shell"
                }
            }
            _ => "Switch to native chat",
        }
    }
    /// Whether the card's switch control can act now rather than only be seen.
    ///
    /// A start, a return, or a close in flight owns the card until it answers,
    /// and a switch pressed meanwhile used to vanish without a word.
    pub(super) fn chat_switch_ready(&self, card: CardId) -> bool {
        !self.chat_start_pending(card) && !self.chat_ui.closed.contains(&card)
    }
    /// The conversation a chat card would return from, as current as the card
    /// knows it: the runtime's session carries the identity the provider has
    /// confirmed since the card was saved.
    fn chat_return_session(&self, card: CardId) -> Option<chat::Session> {
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) {
            let mut session = runtime.session.clone();
            if let Some(id) = runtime
                .handle()
                .and_then(|handle| handle.snapshot().session_id.clone())
            {
                session.launch.session_id = Some(id);
            }
            return Some(session);
        }
        match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(session)) => Some(session.clone()),
            _ => None,
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
    /// Closes a chat card, in whatever state its conversation is.
    ///
    /// The card leaves at once. What the conversation holds in the daemon is
    /// given up in the background: through the live connection when there is
    /// one, otherwise by identity against the daemon's inventory, and for a
    /// start or restart still in flight, when that request answers. Nothing a
    /// conversation can be doing - never started, connecting, failed,
    /// disconnected, or live - keeps its card on screen.
    ///
    /// Two cases wait for a daemon answer instead: a return to the terminal
    /// already under way closes the card as soon as it lands, and a chat parked
    /// behind its source terminal closes before that terminal's own close, so
    /// the terminal's close never races the actor that owns it.
    pub(super) fn begin_chat_close(&mut self, card: CardId) -> Task<Message> {
        if self.chat_ui.closed.contains(&card) {
            // The conversation is already given up; only the removal is
            // left. Its commit is resent, and its reservation retaken if a
            // rebuilt strip lost it, which is what makes a close whose commit
            // was dropped (say, while an update was being prepared)
            // recoverable by closing again.
            return self
                .reserve_chat_removal(card)
                .map_or_else(Task::none, |handoff| self.card_removal(handoff));
        }
        if self
            .chat_ui
            .handoffs
            .iter()
            .any(|target| target.card == card)
        {
            self.chat_ui.close_after_handoff.insert(card);
            return Task::none();
        }
        // Only a live conversation is ever parked behind its terminal.
        if let Some(runtime) = self.chat_ui.parked.get(&card)
            && let Some(handle) = runtime.handle().cloned()
        {
            let target = runtime.target;
            if !self.chat_ui.closing.insert(target) {
                return Task::none();
            }
            return cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || handle.close())
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                Message::ChatUi(Action::CloseFinished(target, result))
            });
        }
        if !self
            .card_state(card)
            .is_some_and(|state| state.existence.is_open())
        {
            return Task::none();
        }
        let Some(session) = self.chat_return_session(card) else {
            return Task::none();
        };
        // The removal is reserved before anything is given up, so a close the
        // workspace cannot accept changes nothing.
        if self.reserve_chat_removal(card).is_none() {
            return Task::none();
        }
        let start = self.chat_ui.attempts.remove(&card);
        let start_in_flight = start.is_some();
        if let Some(attempt) = start {
            self.chat_ui
                .abandoned
                .insert(attempt.target, attempt.session);
        }
        // A restart is closing the previous conversation right now; that close
        // is the retirement, and the replacement it was clearing room for is
        // never started.
        let restarting = self
            .chat_ui
            .closing
            .iter()
            .copied()
            .filter(|target| target.card == card)
            .collect::<Vec<_>>();
        self.chat_ui.restarting.remove(&card);
        for target in &restarting {
            self.chat_ui.closing.remove(target);
            self.chat_ui.abandoned.insert(*target, session.clone());
        }
        // A request still in flight retires the conversation when it answers,
        // and only then is there anything certain to retire.
        let retire = if !start_in_flight && restarting.is_empty() {
            let handle = match self.cards.get(&card) {
                Some(CardRuntime::Chat(runtime)) if runtime.connected() => {
                    runtime.handle().cloned()
                }
                _ => None,
            };
            self.retire_chat(card, handle, Some(session))
        } else {
            Task::none()
        };
        Task::batch([retire, self.remove_chat_card(card)])
    }

    /// Gives up a conversation in the background; see [`retire_conversation`].
    fn retire_chat(
        &mut self,
        card: CardId,
        handle: Option<Arc<ClientHandle>>,
        session: Option<chat::Session>,
    ) -> Task<Message> {
        let target = self.chat_ui.next_target(card);
        self.chat_ui.retiring.insert(target);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || retire_conversation(handle, session))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::Retired(target, result))
        })
    }

    /// Replaces a chat card that has no daemon handoff to take with a
    /// terminal in its place.
    ///
    /// The actor is given up first, so the terminal never shares the
    /// conversation with it. The terminal is a new card beside the chat's
    /// rather than the chat card converted: a conversion is only ever an
    /// exact handoff of one conversation, and a shell is not that.
    fn return_chat_locally(&mut self, card: CardId) -> Task<Message> {
        let Some(session) = self.chat_return_session(card) else {
            return Task::none();
        };
        let launch = match local_return_launch(&session) {
            Ok(launch) => launch,
            Err(error) => {
                self.status = Some(TransientNotice::error(error));
                return Task::none();
            }
        };
        let resumes = session.launch.session_id.is_some();
        let handle = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) if runtime.connected() => runtime.handle().cloned(),
            _ => None,
        };
        // The daemon may run a CLI for a Codex thread that this card never
        // heard about, when the answer to an earlier return was lost; only
        // the daemon can say.
        let shared = retains_source_pty(&session);
        let target = self.chat_ui.next_target(card);
        self.chat_ui.handoffs.insert(target);
        self.chat_ui.local_returns.insert(
            target,
            LocalReturn {
                session: session.clone(),
                launch,
                resumes,
            },
        );
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if shared
                    && live_codex_source(session.id)
                        .map_err(ReturnRefused::Unconfirmed)?
                        .is_some()
                {
                    return Err(ReturnRefused::SourceLive);
                }
                retire_conversation(handle, Some(session)).map_err(ReturnRefused::Unconfirmed)
            })
            .await
            .map_err(|error| ReturnRefused::Unconfirmed(error.to_string()))
            .and_then(|result| result);
            Message::ChatUi(Action::ReturnedLocally(target, result))
        })
    }

    /// Reserves the removal of a chat card whose conversation is already
    /// accounted for, and commits it.
    fn remove_chat_card(&mut self, card: CardId) -> Task<Message> {
        let Some(handoff) = self.reserve_chat_removal(card) else {
            return Task::none();
        };
        self.chat_ui.closed.insert(card);
        self.observe_agent_activity_for_card(card, None);
        self.card_removal(handoff)
    }

    /// The terminal that takes a returned chat's place, with the slot it
    /// takes: where the chat stands now, in the strip and in saved order.
    pub(super) fn take_chat_replacement(
        &mut self,
        card: CardId,
    ) -> Option<(LaunchRecipe, (usize, usize))> {
        let launch = self.chat_ui.replacements.remove(&card)?;
        let index = self.workspace.order().position(|id| id == card)?;
        let durable = self
            .durable
            .cards
            .iter()
            .position(|state| state.id == card)?;
        Some((launch, (index, durable + 1)))
    }

    /// Opens the terminal that replaces a chat whose removal has committed.
    pub(super) fn open_chat_replacement(
        &mut self,
        launch: LaunchRecipe,
        slot: (usize, usize),
    ) -> Task<Message> {
        let mut session = fresh_terminal_session(launch);
        session.resume_hint = crate::model::agent_recovery::hint_from_launch(&session.launch);
        self.insert_terminal_card(session, Some(slot))
    }

    /// The workspace's reservation to remove `card`, taken now unless it is
    /// already held; a refusal is reported.
    fn reserve_chat_removal(&mut self, card: CardId) -> Option<crate::workspace::FocusHandoff> {
        let handoff = match self.workspace.pending_close() {
            Some(handoff) if handoff.unmounting == card => Ok(handoff),
            _ => self.workspace.request_close(card),
        };
        handoff
            .inspect_err(|error| {
                self.status = Some(TransientNotice::error(error.to_string()));
            })
            .ok()
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

    /// Hands the models the conversation in `card` lists to the catalog a
    /// new conversation picks from, once for each list it reports.
    fn list_chat_models(&mut self, card: CardId) -> Task<Message> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return Task::none();
        };
        let Some(snapshot) = runtime.handle().map(|handle| handle.snapshot()) else {
            return Task::none();
        };
        let fingerprint = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            snapshot.models.hash(&mut hasher);
            hasher.finish()
        };
        if snapshot.models.is_empty()
            || std::mem::replace(&mut runtime.listed_models, fingerprint) == fingerprint
        {
            return Task::none();
        }
        let provider = runtime.session.launch.provider;
        self.observe_listed_models(provider, snapshot.models.clone())
    }

    pub(super) fn start_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
        intent: chat::Intent,
        source: Option<HostSessionProof>,
        terminate_source: bool,
    ) -> Task<Message> {
        let selection = None;
        let target = self.chat_ui.next_target(card);
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
        let Some(handle) = runtime.handle().cloned() else {
            return Task::none();
        };
        let epoch = handle.begin_acknowledgement();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.acknowledge_updates(epoch))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::UpdatesAcknowledged(target, result))
        })
    }
    /// Hands what a conversation's provider reported about the account it
    /// runs on to the accounts, which find the account by its launch.
    fn take_chat_account_usage(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(reports) = runtime.handle().map(|handle| handle.take_account_usage()) else {
            return Task::none();
        };
        if reports.is_empty() {
            return Task::none();
        }
        let launch = runtime.session.launch.clone();
        self.account_reported(launch.provider, &launch.recipe, reports)
    }
    /// Reads the activity of a conversation that stays on its connection
    /// after its return to a terminal, or its close, failed. An activity wake
    /// that arrived meanwhile was dropped, and the connection wakes for
    /// activity again only once it has been read, which for a hidden card
    /// nothing else would do.
    fn reread_chat_activity(&mut self, target: Target) {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return;
        };
        let Some(handle) = runtime.handle() else {
            return;
        };
        let activity = Activity {
            provider: runtime.session.launch.provider,
            state: handle.take_activity(),
        };
        self.observe_agent_activity_for_card(target.card, Some(activity));
    }
    /// The chat runtime the card `target` names shows, never one parked
    /// behind the terminal it went back to, whose card is that terminal now.
    fn chat_card_runtime_mut(&mut self, target: Target) -> Option<&mut Runtime> {
        match self.cards.get_mut(&target.card) {
            Some(CardRuntime::Chat(runtime)) if runtime.target == target => Some(runtime),
            _ => None,
        }
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
    /// Starts a draft's conversation for the message in its composer, which
    /// waits in the queue until the conversation can take it.
    ///
    /// The conversation starts under a new identity, so it never meets
    /// anything an earlier start of this card may have left in the daemon.
    fn start_chat_draft(&mut self, target: Target) -> Task<Message> {
        if self.chat_start_pending(target.card)
            || self.chat_ui.closed.contains(&target.card)
            || self.refuse_for_deleting_card(target.card)
        {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.choosing() {
            return Task::none();
        }
        let model = runtime
            .snapshot
            .selected_model
            .clone()
            .map(|model| (model, runtime.snapshot.reasoning.clone()));
        let mut session = runtime.session.clone();
        session.id = TerminalId::new();
        runtime.queued = Some(Queued {
            text: runtime.composer.text(),
            attachments: std::mem::take(&mut runtime.attachments),
            model,
            draft: session.clone(),
            since: None,
            rechecking: false,
        });
        runtime.replace_draft(text_editor::Content::new());
        runtime.history_cursor = None;
        runtime.history_stash.clear();
        runtime.error = None;
        runtime.model_picker = None;
        runtime.account_picker = false;
        runtime.reasoning_picker = false;
        runtime.session = session.clone();
        self.replace_chat_conversation(target.card, session.clone());
        self.start_chat(target.card, session, chat::Intent::Start, None, false)
    }

    /// Takes the next step of sending a started draft's first message: its
    /// model, where one was chosen, and then the message.
    ///
    /// Nothing is sent before the provider says it takes commands, which the
    /// next snapshot to say so brings back here. A provider that cannot get
    /// that far says so by ending the conversation, which sends the card back
    /// to its draft with the reason. A model the conversation does not list is not
    /// waited for past [`MODEL_PATIENCE`]; the message is sent on the default
    /// instead, and the transcript says so.
    fn dispatch_queued(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        let Some(handle) = runtime.handle().cloned() else {
            return Task::none();
        };
        if runtime.pending.is_some() || runtime.disconnected {
            return Task::none();
        }
        let provider = runtime.session.launch.provider;
        let Some(queued) = runtime.queued.as_mut() else {
            return Task::none();
        };
        let snapshot = handle.snapshot();
        if snapshot.exited {
            // The error is the last the conversation reported, which is
            // not always why it stopped.
            let stopped = "The conversation stopped before it took the first message.";
            let error = match &snapshot.error {
                Some(error) => format!("{stopped} Its last report: {error}"),
                None => stopped.to_owned(),
            };
            return self.revert_chat_draft(target, Some(error));
        }
        if !snapshot.ready {
            return Task::none();
        }
        let waited = queued.since.get_or_insert_with(Instant::now).elapsed();
        if let Some((model, reasoning)) = queued.model.clone() {
            if snapshot.selected_model.as_ref() == Some(&model)
                && (reasoning.is_none() || snapshot.reasoning == reasoning)
            {
                queued.model = None;
            } else if let Some(listed) = snapshot.models.iter().find(|listed| listed.id == model) {
                let reasoning = reasoning.filter(|reasoning| listed.reasoning.contains(reasoning));
                return self.chat_send_step(
                    target,
                    Command::SetModel { model, reasoning },
                    false,
                    Some(QueuedStep::Model),
                );
            } else if !snapshot.models.is_empty() || waited >= MODEL_PATIENCE {
                queued.model = None;
                runtime.note(format!(
                    "This conversation does not offer {model}, so the message went to its default model."
                ));
                self.forget_chat_model(provider, &model);
            } else {
                // The models it lists arrive in a later snapshot, which
                // brings this back; this is for the one that never comes.
                return queued.look_again(target, MODEL_PATIENCE - waited);
            }
        }
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        let Some(queued) = runtime.queued.as_ref() else {
            return Task::none();
        };
        match submission(
            &queued.text,
            &snapshot,
            &queued.attachments,
            PromptMode::Send,
        ) {
            Ok(command) => self.chat_send_step(target, command, false, Some(QueuedStep::Prompt)),
            Err(refusal) => self.revert_chat_draft(target, Some(refusal.into())),
        }
    }

    /// Carries on after a step of a queued first message was answered.
    ///
    /// The provider was ready when the step was sent, so a refusal is its
    /// answer, not a sign to try again: a message sent twice is worse than
    /// one returned. A model that could not be chosen is given up and the
    /// message sent anyway. A message the conversation did not take sends the
    /// card back to its draft, with the message where it was written; one
    /// its transcript shows arriving was taken, whatever the answer said.
    fn queued_step_finished(
        &mut self,
        target: Target,
        step: QueuedStep,
        first_prompt: Option<String>,
        result: Result<(), String>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        let provider = runtime.session.launch.provider;
        let Some(queued) = runtime.queued.as_mut() else {
            return Task::none();
        };
        let taken = match (step, result) {
            (QueuedStep::Model, Ok(())) => {
                queued.model = None;
                None
            }
            (QueuedStep::Model, Err(error)) => {
                let model = queued.model.take().map(|(model, _)| model);
                runtime.note(format!(
                    "{} could not be chosen ({error}), so the message went to the default model.",
                    model.as_deref().unwrap_or("The model")
                ));
                if let Some(model) = model {
                    self.forget_chat_model(provider, &model);
                }
                None
            }
            (QueuedStep::Prompt, Ok(())) => Some(None),
            (QueuedStep::Prompt, Err(error)) => {
                // The conversation is new, so a message of the reader's in
                // it can only be this one.
                let arrived = runtime.handle().is_some_and(|handle| {
                    handle
                        .snapshot()
                        .items
                        .iter()
                        .any(|item| item.role == chat::Role::User)
                });
                if !arrived {
                    return self.revert_chat_draft(target, Some(error));
                }
                Some(Some(error))
            }
        };
        let Some(error) = taken else {
            return self.dispatch_queued(target);
        };
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        let text = runtime.queued.take().map(|queued| queued.text);
        let _ = runtime.present();
        if let Some(error) = error {
            runtime.note(format!(
                "The first message arrived, but its sending reported: {error}"
            ));
        }
        if let Some(text) = text {
            runtime.remember(text);
        }
        runtime.refresh();
        if let Some(prompt) = first_prompt {
            self.name_channel_from_prompt(target.card, &prompt);
        }
        Task::none()
    }

    /// Returns a card whose conversation never took its first message to the
    /// draft it was started from, with the message back in the composer and
    /// `error` saying why, and gives up whatever the start left running.
    ///
    /// A conversation with nothing in it is not worth keeping, and a draft
    /// can be moved to another model, account or provider where the started
    /// conversation could not.
    fn revert_chat_draft(&mut self, target: Target, error: Option<String>) -> Task<Message> {
        let Some(provider) = self
            .chat_card_runtime_mut(target)
            .map(|runtime| runtime.session.launch.provider)
        else {
            return Task::none();
        };
        let snapshot = self.chat_draft_snapshot(provider);
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        let Some(draft) = runtime.unqueue() else {
            return Task::none();
        };
        let handle = match std::mem::replace(&mut runtime.connection, Connection::Draft) {
            Connection::Live(handle) => Some(handle),
            Connection::Draft => None,
        };
        runtime.session = draft.clone();
        runtime.snapshot = Arc::new(snapshot);
        runtime.forget_transcript();
        runtime.pending = None;
        runtime.disconnected = false;
        runtime.error = error;
        runtime.refresh();
        self.replace_chat_conversation(target.card, draft.clone());
        self.observe_agent_activity_for_card(target.card, None);
        self.retire_chat(target.card, handle, Some(draft))
    }

    /// Takes back a first message its conversation does not have yet.
    ///
    /// A start still under way is abandoned, which retires whatever it
    /// produces; a conversation that has started is given up, as one that
    /// never took its first message. A message already on its way cannot be
    /// taken back.
    fn cancel_queued(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.queued.is_none()
            || runtime
                .pending
                .as_ref()
                .is_some_and(|pending| pending.queued == Some(QueuedStep::Prompt))
        {
            return Task::none();
        }
        if runtime.is_draft() {
            runtime.unqueue();
            if let Some(attempt) = self.chat_ui.attempts.remove(&target.card) {
                self.chat_ui
                    .abandoned
                    .insert(attempt.target, attempt.session);
            }
            return Task::none();
        }
        self.revert_chat_draft(target, None)
    }

    fn chat_send(&mut self, target: Target, command: Command, clear_draft: bool) -> Task<Message> {
        self.chat_send_step(target, command, clear_draft, None)
    }
    /// Sends `command`, which is `step` of a queued first message where that
    /// is given.
    fn chat_send_step(
        &mut self,
        target: Target,
        command: Command,
        clear_draft: bool,
        step: Option<QueuedStep>,
    ) -> Task<Message> {
        if self.chat_start_pending(target.card) {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        // A queued first message goes out ahead of anything else, and
        // nothing is sent past it while it waits.
        if runtime.pending.is_some()
            || runtime.disconnected
            || (runtime.queued.is_some() != step.is_some())
        {
            return Task::none();
        }
        let Some(handle) = runtime.handle().cloned() else {
            return Task::none();
        };
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
        let model = match &command {
            Command::SetModel { model, reasoning } => Some((model.clone(), reasoning.clone())),
            _ => None,
        };
        runtime.pending = Some(PendingCommand {
            id,
            draft_revision: clear_draft.then_some(runtime.revision),
            request,
            permission_mode,
            first_prompt,
            model,
            page,
            sent,
            queued: step,
        });
        runtime.error = None;
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
        if let Some(card) = action.starts_session()
            && self.refuse_for_deleting_card(card)
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
            Action::DraftModel(target, provider, model) => {
                return self.choose_draft_model(target, provider, model);
            }
            Action::ChooseAccount(target, id) => {
                return self.choose_chat_account(target, id);
            }
            Action::AccountPicker(target, open) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.account_picker = open && runtime.choosing();
                    runtime.model_picker = None;
                    runtime.reasoning_picker = false;
                }
            }
            Action::DispatchQueued(target) => {
                if let Some(queued) = self
                    .chat_runtime_mut(target)
                    .and_then(|runtime| runtime.queued.as_mut())
                {
                    queued.rechecking = false;
                }
                return self.dispatch_queued(target);
            }
            Action::CancelQueued(target) => return self.cancel_queued(target),
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
                        let session = match conversion(terminal) {
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
                            // The new conversation opens as a card of its own
                            // beside the stopped terminal. It is not the
                            // terminal converted in place: a conversion is only
                            // ever the exact handoff of one conversation, and
                            // saved state refuses any other. Nor does it take
                            // the terminal's place, because the terminal still
                            // relaunches the conversation it ran, which would
                            // otherwise be lost in one click. That thread is
                            // deliberately not joined, because joining an
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
                            // A terminal still starting may yet run its agent,
                            // and then switches to chat as that agent.
                            if self.pending_terminal_spawn(card) {
                                self.status = Some(TransientNotice::warning(
                                    "This terminal is still starting. Switch once it has started or stopped.",
                                ));
                                return Task::none();
                            }
                            let unrestricted = bypasses_approvals(&recipe);
                            let fresh = chat::Session {
                                id: crate::model::TerminalId::new(),
                                launch: chat::Launch {
                                    recipe,
                                    session_id: None,
                                    ..session.launch
                                },
                                proof: None,
                                terminal: None,
                            };
                            self.status = Some(TransientNotice::warning(if unrestricted {
                                "This terminal's agent is not running, so a new conversation was opened beside it, with this terminal's approvals and sandbox left off."
                            } else {
                                "This terminal's agent is not running, so a new conversation was opened beside it."
                            }));
                            return self.insert_chat_card(fresh, Some(card));
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
                        if self.chat_ui.closed.contains(&card) {
                            return Task::none();
                        }
                        // Only a connected conversation with a confirmed
                        // identity can ask the daemon for its exact terminal;
                        // every other card still has a terminal to become.
                        let named = |runtime: &Runtime| {
                            runtime
                                .handle()
                                .is_some_and(|handle| handle.snapshot().session_id.is_some())
                        };
                        let runtime = match self.cards.get(&card) {
                            Some(CardRuntime::Chat(runtime))
                                if runtime.connected() && named(runtime) =>
                            {
                                runtime
                            }
                            Some(CardRuntime::Chat(runtime))
                                if runtime.transition_blocker().is_some() =>
                            {
                                self.status =
                                    runtime.transition_blocker().map(TransientNotice::error);
                                return Task::none();
                            }
                            _ => return self.return_chat_locally(card),
                        };
                        if let Some(reason) = runtime.handoff_blocker() {
                            self.status = Some(TransientNotice::error(reason));
                            return Task::none();
                        }
                        let Some(handle) = runtime.handle().cloned() else {
                            return Task::none();
                        };
                        let mut session = runtime.session.clone();
                        session.launch.session_id = handle.snapshot().session_id.clone();
                        let terminal = match return_terminal(&session) {
                            Ok(terminal) => terminal,
                            Err(error) => {
                                self.status = Some(TransientNotice::error(error));
                                return Task::none();
                            }
                        };
                        let target = runtime.target;
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
                if let Some(session) = self.chat_ui.abandoned.remove(&target) {
                    // The card was closed while this start was in flight.
                    // Whatever the start produced is given up now; a failed
                    // start is still checked against the inventory, because a
                    // lost reply is not proof the daemon started nothing.
                    return self.retire_chat(target.card, result.ok(), Some(session));
                }
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
                        return self.retire_chat(target.card, Some(handle), None);
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
                            runtime.target = target;
                            runtime.connection = Connection::Live(handle);
                            runtime.model_picker = None;
                            runtime.account_picker = false;
                            runtime.reasoning_picker = false;
                            runtime.session = session.clone();
                            runtime.disconnected = false;
                            runtime.pending = None;
                            runtime.error = None;
                            runtime.show(visible);
                        } else {
                            let mut runtime = Runtime::new(
                                target,
                                session.clone(),
                                Connection::Live(handle),
                                visible,
                                self.settings.editor_syntax_theme,
                            );
                            // Whatever was typed before this card last left the
                            // chat surface comes back with it.
                            if let Some(draft) = self.chat_ui.drafts.remove(&target.card)
                                && !draft.is_empty()
                            {
                                runtime.replace_draft(text_editor::Content::with_text(&draft));
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
                        // Reports that raced the attachment woke nothing.
                        let usage = self.take_chat_account_usage(target);
                        let control = match self.cards.get(&target.card) {
                            Some(CardRuntime::Chat(runtime)) => runtime.focus_control(),
                            _ => "chat-composer",
                        };
                        return Task::batch([
                            usage,
                            self.list_chat_models(target.card),
                            self.dispatch_queued(target),
                            iced::widget::operation::focus(card_control_id(target.card, control))
                                .map(cosmic::Action::App),
                        ]);
                    }
                    Err(failure)
                        if matches!(
                            self.cards.get(&target.card),
                            Some(CardRuntime::Chat(runtime)) if runtime.is_draft()
                        ) =>
                    {
                        // A draft whose first message could not start it
                        // stays a draft, with the message back where it was
                        // written. Whatever the start left in the daemon is
                        // given up, since a lost answer is not proof it
                        // started nothing; the next send starts afresh under
                        // an identity of its own.
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            runtime.unqueue();
                            runtime.error = Some(failure.message);
                        }
                        return self.retire_chat(target.card, None, Some(attempt.session));
                    }
                    Err(failure)
                        if failure.recovery == chat::Recovery::Fresh
                            && attempt.session.launch.session_id.is_none()
                            && attempt.session.terminal.is_none()
                            && matches!(
                                self.card_state(target.card).map(|state| &state.kind),
                                Some(CardKind::Chat(_))
                            ) =>
                    {
                        // A conversation its provider never named, and that
                        // no CLI terminal of the reader's holds, has nothing
                        // to resume, so there is nothing to ask the reader
                        // about: the card opens as a draft, as it would have
                        // if nothing had started it. Chats opened before a
                        // first message started anything come back this way
                        // after an update.
                        let mut draft = attempt.session.clone();
                        draft.proof = None;
                        self.chat_ui.fresh_offers.remove(&target.card);
                        self.chat_forget(target.card);
                        self.replace_chat_conversation(target.card, draft.clone());
                        self.open_chat_draft(target.card, draft);
                        return self.retire_chat(target.card, None, Some(attempt.session));
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
            Action::Retired(target, result) => {
                // Nothing is on screen for this conversation, so only a
                // failure to end it is worth saying anything about.
                if self.chat_ui.retiring.remove(&target)
                    && let Err(error) = result
                {
                    self.status = Some(TransientNotice::error(format!(
                        "A conversation this app gave up could not be confirmed stopped: {error}"
                    )));
                }
            }
            Action::ReturnedLocally(target, result) => {
                let Some(LocalReturn {
                    session,
                    launch,
                    resumes,
                }) = self.chat_ui.local_returns.remove(&target)
                else {
                    return Task::none();
                };
                if !self.chat_ui.handoffs.remove(&target) {
                    return Task::none();
                }
                let card = target.card;
                let close = self.chat_ui.close_after_handoff.remove(&card);
                // The chat stays whenever its conversation is still running
                // somewhere a new terminal must not compete with: in the CLI
                // the daemon keeps, or, for a resume, in an actor that may
                // still be holding it. A new shell shares nothing with the
                // actor, so it goes ahead while the actor is given up again,
                // which reports if it still cannot be confirmed.
                let (kept, retry) = match result {
                    Ok(()) => (None, Task::none()),
                    Err(ReturnRefused::SourceLive) => (
                        Some(
                            "This conversation's CLI is still running. Reconnect the chat to return to it."
                                .to_owned(),
                        ),
                        Task::none(),
                    ),
                    Err(ReturnRefused::Unconfirmed(error)) if resumes => (Some(error), Task::none()),
                    Err(ReturnRefused::Unconfirmed(_)) => {
                        (None, self.retire_chat(card, None, Some(session)))
                    }
                };
                if let Some(error) = kept {
                    self.status = Some(TransientNotice::error(error));
                    self.reread_chat_activity(target);
                    return if close {
                        self.begin_chat_close(card)
                    } else {
                        Task::none()
                    };
                }
                if !self
                    .card_state(card)
                    .is_some_and(|state| state.existence.is_open())
                {
                    return retry;
                }
                // Closed while returning: no terminal is started only to be
                // stopped again.
                if close {
                    return Task::batch([retry, self.remove_chat_card(card)]);
                }
                // The terminal takes the chat's place in its strip, so it can
                // only be placed while that strip is the one on screen.
                if !self.workspace.order().any(|id| id == card) {
                    self.status = Some(TransientNotice::warning(
                        "The chat has stopped. Return to it and switch again to open its terminal.",
                    ));
                    return retry;
                }
                self.chat_ui.replacements.insert(card, launch);
                let removal = self.remove_chat_card(card);
                if !self.chat_ui.closed.contains(&card) {
                    self.chat_ui.replacements.remove(&card);
                }
                return Task::batch([retry, removal]);
            }
            Action::UpdatesAcknowledged(target, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Err(error) = result
                {
                    runtime.error = Some(error);
                }
            }
            Action::Wake(target, wake) => {
                // Ready refreshes the new handle.
                if self.chat_start_pending(target.card) {
                    // The account reports go unread, as the recipe may already
                    // name the account the conversation is moving to, but
                    // not left queued: the queue wakes the surface only when
                    // it fills from empty, so reports left behind would keep
                    // every later one on this connection from waking it.
                    if matches!(wake, Wake::AccountUsage)
                        && let Some(handle) = self
                            .chat_runtime_mut(target)
                            .and_then(|runtime| runtime.handle())
                    {
                        handle.take_account_usage();
                    }
                    return Task::none();
                }
                // Before the runtime is borrowed, as the account it reports
                // on belongs to the whole window.
                if matches!(wake, Wake::AccountUsage) {
                    return self.take_chat_account_usage(target);
                }
                let needs_name = self.channel_needs_name(target.card);
                if let Some(runtime) = self.chat_ui.parked.get_mut(&target.card)
                    && runtime.target == target
                {
                    let name = match runtime.handle() {
                        Some(handle) if needs_name && matches!(wake, Wake::Changed) => {
                            snapshot_channel_name(&handle.snapshot()).map(str::to_owned)
                        }
                        _ => None,
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
                // Wakes come only from a live connection.
                let Some(handle) = runtime.handle().cloned() else {
                    return Task::none();
                };
                match wake {
                    Wake::Changed => {
                        runtime.refresh();
                        let activity = Activity {
                            provider: runtime.session.launch.provider,
                            state: handle.take_activity(),
                        };
                        let session = runtime.session.clone();
                        let snapshot = handle.snapshot();
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
                        return Task::batch([
                            self.list_chat_models(target.card),
                            self.dispatch_queued(target),
                        ]);
                    }
                    Wake::Observed => {
                        handle.take_observed();
                        return Task::none();
                    }
                    Wake::Activity => {
                        let activity = Activity {
                            provider: runtime.session.launch.provider,
                            state: handle.take_activity(),
                        };
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        return Task::none();
                    }
                    // Taken above, for a parked conversation too.
                    Wake::AccountUsage => return Task::none(),
                    Wake::Disconnected(error) => {
                        // A first message still waiting will not be taken
                        // now, and a conversation without it holds nothing.
                        if runtime.queued.is_some() {
                            return self.revert_chat_draft(target, Some(error));
                        }
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
                // Dropping the saved conversation is what this does, and storage
                // takes it as given, so it acts only while the card still
                // offers it: a late or repeated press after the conversation
                // recovered must not discard it.
                if !self.chat_offers_fresh_conversation(card) || self.chat_ui.closed.contains(&card)
                {
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
                if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card)
                    && let Some(handle) = runtime.handle().cloned()
                {
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
                self.replace_chat_conversation(card, session.clone());
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
                    // The completion rows follow the word at the caret, so
                    // anything that can change that word starts them afresh.
                    let rescroll = !matches!(action, text_editor::Action::Scroll { .. })
                        && runtime.completion.restart(action.is_edit());
                    runtime.composer.perform(action);
                    if rescroll {
                        return view::completion_scroll(target, 0.0);
                    }
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
                        runtime.replace_draft(text_editor::Content::with_text(&text));
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
                if runtime.is_draft() {
                    return self.start_chat_draft(target);
                }
                if (mode == PromptMode::Steer && !runtime.snapshot.capabilities.steer)
                    || (mode == PromptMode::Queue && !runtime.snapshot.capabilities.queue)
                    || (mode == PromptMode::Send && runtime.snapshot.busy)
                {
                    return Task::none();
                }
                let text = runtime.composer.text();
                let command = match submission(&text, &runtime.snapshot, &runtime.attachments, mode)
                {
                    Ok(command) => command,
                    Err(refusal) => {
                        runtime.error = Some(refusal.into());
                        return Task::none();
                    }
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
                let provider = runtime.session.launch.provider;
                if result.is_ok()
                    && let Some((model, reasoning)) = pending.model.clone()
                {
                    self.remember_chat_model(provider, Some(model), reasoning);
                }
                if let Some(step) = pending.queued {
                    return self.queued_step_finished(target, step, pending.first_prompt, result);
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let first_prompt = match result {
                    Ok(()) => {
                        if let Some(mode) = pending.permission_mode {
                            runtime.permission_mode = Some(mode);
                        }
                        if pending.draft_revision == Some(runtime.revision) {
                            runtime.replace_draft(text_editor::Content::new());
                            runtime.attachments.clear();
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
                        runtime.note(error.clone());
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
                    if let Some(item) = runtime
                        .snapshot
                        .items
                        .iter()
                        .chain(
                            runtime
                                .snapshot
                                .subagents
                                .iter()
                                .flat_map(|agent| agent.items.iter()),
                        )
                        .find(|item| item.id == id)
                    {
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
            Action::SelectSubagent(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.selected_subagent = id.filter(|id| {
                        runtime
                            .snapshot
                            .subagents
                            .iter()
                            .any(|agent| &agent.id == id)
                    });
                    return iced::widget::scrollable::snap_to(
                        card_control_id(target.card, "chat-transcript"),
                        iced::widget::scrollable::RelativeOffset {
                            x: None,
                            y: Some(1.0),
                        },
                    );
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
                        .chain(
                            runtime
                                .snapshot
                                .subagents
                                .iter()
                                .flat_map(|agent| agent.items.iter()),
                        )
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
                        Ok(Some(paths)) => runtime.attach(paths),
                        Ok(None) => {}
                        Err(error) => runtime.error = Some(error),
                    }
                }
            }
            Action::Dropped(target, payload) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                // Dropped files become attachments where the provider takes
                // them, as they would from the chooser. A file it would refuse
                // at send - a document for Codex, a ninth file for Claude, a
                // folder for any - is written into the draft as its path
                // instead, with web addresses and dropped text, as they are.
                let dropped = match payload.decode() {
                    file_drop::Dropped::Items(items)
                        if runtime.snapshot.capabilities.attachments =>
                    {
                        let rules =
                            chat::AttachmentRules::for_provider(runtime.session.launch.provider);
                        let mut room = rules.max_count.map_or(usize::MAX, |limit| {
                            limit.saturating_sub(runtime.attachments.len())
                        });
                        let mut files = Vec::new();
                        let mut rest = Vec::new();
                        for item in items {
                            match item {
                                file_drop::Item::File(path)
                                    if files.contains(&path)
                                        || runtime
                                            .attachments
                                            .iter()
                                            .any(|attachment| attachment.path == path) => {}
                                file_drop::Item::File(path) if room > 0 && rules.accepts(&path) => {
                                    room -= 1;
                                    files.push(path);
                                }
                                other => rest.push(other),
                            }
                        }
                        runtime.attach(files);
                        file_drop::Dropped::Items(rest)
                    }
                    dropped => dropped,
                };
                let insertion = dropped.insertion(file_drop::Spelling::Plain);
                if !insertion.text.is_empty() {
                    runtime.revision = runtime.revision.wrapping_add(1);
                    runtime.history_cursor = None;
                    runtime
                        .composer
                        .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                            Arc::new(insertion.text.clone()),
                        )));
                }
                if let Some(refusal) = insertion.refusal() {
                    self.status = Some(TransientNotice::warning(refusal));
                }
                return cosmic::task::message(Message::FocusCard(target.card));
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
                if runtime.is_draft() {
                    let provider = runtime.session.launch.provider;
                    return self.choose_draft_model(target, provider, Some(id));
                }
                // Choosing is what the search was opened for, so the popup
                // closes on the answer rather than waiting to be dismissed.
                runtime.model_picker = None;
                let Some(model) = runtime.snapshot.models.iter().find(|model| model.id == id)
                else {
                    return Task::none();
                };
                let reasoning = runtime
                    .snapshot
                    .reasoning
                    .clone()
                    .filter(|reasoning| model.reasoning.contains(reasoning));
                let command = Command::SetModel {
                    model: id,
                    reasoning,
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
                let model = model.id.clone();
                let provider = runtime.session.launch.provider;
                if runtime.is_draft() {
                    if !runtime.choosing() {
                        return Task::none();
                    }
                    Arc::make_mut(&mut runtime.snapshot).reasoning = Some(reasoning.clone());
                    self.remember_chat_model(provider, Some(model), Some(reasoning));
                    return Task::none();
                }
                let command = Command::SetModel {
                    model,
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
            Action::Complete(target, insertion, otherwise) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime.complete(&insertion, otherwise.is_some()) {
                    // Enter pressed while the list still showed a row the
                    // word has since moved past sends or breaks the line, as
                    // it would have without the list, rather than doing
                    // nothing at all.
                    return otherwise
                        .map_or_else(Task::none, |otherwise| self.handle_chat_action(*otherwise));
                }
                // A row chosen with the pointer took the keyboard from the
                // draft, and the reader is mid-sentence.
                return iced::widget::operation::focus(card_control_id(
                    target.card,
                    "chat-composer",
                ))
                .map(cosmic::Action::App);
            }
            Action::CompletionMove(target, down) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(count) = runtime.completion().map(|(_, rows)| rows.len()) else {
                    return Task::none();
                };
                let (row, viewport) = view::completion_extent(count);
                if let Some(offset) = runtime.completion.step(down, count, row, viewport) {
                    return view::completion_scroll(target, offset);
                }
            }
            Action::CompletionDismiss(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.completion.dismissed = runtime
                        .completion()
                        .map(|(token, _)| (token.line, token.start));
                }
            }
            Action::CompletionScrolled(target, offset) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.completion.offset = offset;
                }
            }
            Action::HandoffFinished(target, result) => {
                if self.chat_runtime_mut(target).is_none() {
                    return Task::none();
                }
                if !self.chat_ui.handoffs.remove(&target) {
                    return Task::none();
                }
                // A close asked for during the return runs on whatever the
                // card turned out to be: the terminal it returned to, or the
                // chat that is still there when the return failed.
                let close = self.chat_ui.close_after_handoff.remove(&target.card);
                let terminal = match result {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error));
                        self.reread_chat_activity(target);
                        return if close {
                            self.begin_chat_close(target.card)
                        } else {
                            Task::none()
                        };
                    }
                };
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if let Some(reason) = runtime.handoff_blocker()
                    && !close
                {
                    self.status = Some(TransientNotice::error(reason));
                    return Task::none();
                }
                let Some(snapshot) = runtime.handle().map(|handle| handle.snapshot()) else {
                    return Task::none();
                };
                runtime.session.launch.session_id = snapshot.session_id.clone();
                let preserve =
                    runtime.session.launch.provider == Provider::Codex && !snapshot.exited;
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
                return if close {
                    // Called rather than sent: a message could be dropped by a
                    // modal opened meanwhile, losing the close the user asked for.
                    // A chat parked behind its CLI closes first, exactly as a
                    // close of that card does, and the CLI's close follows it.
                    let close = if self.chat_has_parked_runtime(target.card) {
                        self.begin_chat_close(target.card)
                    } else {
                        self.begin_card_close(target.card)
                    };
                    Task::batch([task, close])
                } else {
                    task
                };
            }
            Action::CloseFinished(target, result) => {
                if let Some(session) = self.chat_ui.abandoned.remove(&target) {
                    // A restart's close whose card has since been closed. It
                    // was already the retirement; only a failure leaves
                    // something for the inventory to settle.
                    return match result {
                        Ok(()) => Task::none(),
                        Err(_) => self.retire_chat(target.card, None, Some(session)),
                    };
                }
                if self.chat_runtime_mut(target).is_none() {
                    return Task::none();
                }
                if !self.chat_ui.closing.remove(&target) {
                    return Task::none();
                }
                if !self.chat_ui.restarting.contains_key(&target.card) {
                    // A chat parked behind its source terminal: the actor is
                    // closed or unreachable either way, and the card is that
                    // terminal now, so its own close takes over from here.
                    self.chat_ui.parked.remove(&target.card);
                    self.observe_agent_activity_for_card(target.card, None);
                    if let Err(error) = result {
                        self.status = Some(TransientNotice::warning(format!(
                            "The chat could not be confirmed stopped: {error}"
                        )));
                    }
                    return self.begin_card_close(target.card);
                }
                if let Err(error) = result {
                    self.chat_ui.restarting.remove(&target.card);
                    self.status = Some(TransientNotice::error(error));
                    self.reread_chat_activity(target);
                    return Task::none();
                }
                if self.refuse_for_deleting_card(target.card) {
                    // The worktree was confirmed for deletion while this
                    // restart was closing the old conversation; its
                    // replacement would start in a directory being removed.
                    // The card keeps the closed conversation instead.
                    self.chat_ui.restarting.remove(&target.card);
                    if let Some(runtime) = self.chat_runtime_mut(target) {
                        runtime.disconnected = true;
                        runtime.error =
                            Some("Restart cancelled: the worktree was being deleted.".into());
                    }
                    self.observe_agent_activity_for_card(target.card, None);
                    return Task::none();
                }
                if let Some(session) = self.chat_ui.restarting.remove(&target.card) {
                    // Only the conversation is being given up here; the card
                    // stays and immediately starts its replacement, so it is
                    // never left showing a conversation that no longer exists.
                    self.chat_forget(target.card);
                    self.cards.insert(target.card, CardRuntime::Loading);
                    self.replace_chat_conversation(target.card, session.clone());
                    return self.start_chat(target.card, session, chat::Intent::Start, None, false);
                }
            }
            Action::Scrolled(target, scroll) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.selected_subagent.is_some() {
                    return Task::none();
                }
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
                runtime.account_picker = false;
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
                    runtime.account_picker = false;
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
    runtime
        .handle()
        .expect("a chat snapshot is shown on a live conversation")
        .replace_snapshot_for_test(snapshot);
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

    const FIXTURE_DIRECTORY: &str = "/workspace/chat-fixture";
    /// Providers name conversations by UUID; resume refuses anything else.
    const KNOWN_THREAD: &str = "0199a2b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b";

    /// A new conversation, which runs nothing until its first message is
    /// sent. Tasks are dropped throughout: nothing here starts a provider or
    /// reaches a daemon.
    fn draft_chat(app: &mut Cagents) -> CardId {
        let _ = app.new_native_chat(Provider::Codex, Some(PathBuf::from(FIXTURE_DIRECTORY)));
        let card = app.workspace.focused().unwrap();
        assert!(
            matches!(app.cards.get(&card), Some(CardRuntime::Chat(runtime)) if runtime.is_draft()),
            "a new conversation opens as a draft"
        );
        assert!(!app.chat_start_pending(card), "and starts nothing");
        card
    }

    fn chat_runtime(app: &Cagents, card: CardId) -> &Runtime {
        match app.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime,
            _ => panic!("the card holds a chat"),
        }
    }

    /// Types `text` into the composer of `card`.
    fn write(app: &mut Cagents, card: CardId, text: &str) {
        let target = chat_runtime(app, card).target;
        let _ = app.update(Message::ChatUi(Action::Composer(
            target,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(text.to_owned()))),
        )));
    }

    /// Sends what the composer of `card` holds.
    fn submit(app: &mut Cagents, card: CardId) {
        let target = chat_runtime(app, card).target;
        let _ = app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send)));
    }

    /// A new conversation whose start, for its first message, is still in
    /// flight.
    fn starting_chat(app: &mut Cagents) -> (CardId, Target) {
        let card = draft_chat(app);
        write(app, card, "hello");
        submit(app, card);
        let target = app
            .chat_ui
            .attempts
            .get(&card)
            .expect("the first message starts the conversation")
            .target;
        (card, target)
    }

    fn answer_start(
        app: &mut Cagents,
        target: Target,
        result: Result<Arc<ClientHandle>, chat::StartFailure>,
    ) {
        let _ = app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(result)))),
        )));
    }

    fn fixture_handle(app: &Cagents, card: CardId, session_id: Option<&str>) -> Arc<ClientHandle> {
        let mut session = app.chat_ui.attempts.get(&card).unwrap().session.clone();
        // The daemon starts a new Codex thread on a listener of its own and
        // hands the thread back naming it.
        session.claim_owned_codex_listener();
        Arc::new(ClientHandle::fixture(
            session,
            Snapshot {
                session_id: session_id.map(str::to_owned),
                ready: true,
                ..Snapshot::default()
            },
        ))
    }

    /// A conversation that started, optionally with a provider-confirmed id.
    fn live_chat(app: &mut Cagents, session_id: Option<&str>) -> CardId {
        let (card, target) = starting_chat(app);
        let handle = fixture_handle(app, card, session_id);
        answer_start(app, target, Ok(handle));
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Chat(_))));
        settle(app, card, Ok(()));
        assert!(chat_runtime(app, card).queued.is_none());
        card
    }

    /// Answers the command `card` is waiting on with `result`.
    fn settle(app: &mut Cagents, card: CardId, result: Result<(), String>) {
        let runtime = chat_runtime(app, card);
        let id = runtime.pending.as_ref().expect("a command is pending").id;
        let target = runtime.target;
        let _ = app.update(Message::ChatUi(Action::CommandFinished(target, id, result)));
    }

    /// A restored conversation, begun but never named by its provider, whose
    /// start the daemon refused.
    fn failed_chat(app: &mut Cagents) -> CardId {
        let card = live_chat(app, None);
        let session = chat_runtime(app, card).session.clone();
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
        let _ = app.open_chat(card, session, chat::Intent::Restore);
        let target = chat_runtime_attempt(app, card);
        answer_start(
            app,
            target,
            Err(chat::StartFailure::retry("fixture: executable unavailable")),
        );
        assert!(matches!(app.cards.get(&card), Some(CardRuntime::Failed(_))));
        app.notices.entries.clear();
        card
    }

    fn chat_runtime_attempt(app: &Cagents, card: CardId) -> Target {
        app.chat_ui
            .attempts
            .get(&card)
            .expect("a start is in flight")
            .target
    }

    /// Closes `card` the way its close button does, and checks that it is gone
    /// for good: off the strip, tombstoned, and held by nothing.
    fn close_chat(app: &mut Cagents, card: CardId) {
        let _ = app.update(Message::CloseCard(card));
        assert_eq!(
            app.workspace
                .pending_close()
                .map(|handoff| handoff.unmounting),
            Some(card),
            "a chat's close is accepted at once, whatever its conversation is doing"
        );
        let _ = app.update(Message::CommitClose(card));
        assert!(
            !app.workspace.order().any(|id| id == card),
            "the card leaves the strip"
        );
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.cards.contains_key(&card));
        assert!(
            errors(app).is_empty(),
            "a close is not an error: {:?}",
            errors(app)
        );
        assert!(
            !app.chat_ui.closed.contains(&card),
            "nothing outlives the card"
        );
        assert!(!app.chat_start_pending(card));
    }

    /// The errors the shell has recorded. `status` is drained into the notice
    /// log at the end of every update, so this is where a test reads it.
    fn errors(app: &Cagents) -> Vec<String> {
        app.notices
            .entries
            .iter()
            .filter(|entry| matches!(entry.notice.severity, crate::app::NoticeSeverity::Error))
            .map(|entry| entry.notice.message.clone())
            .collect()
    }

    fn retired(app: &mut Cagents, card: CardId) -> Target {
        let targets = app
            .chat_ui
            .retiring
            .iter()
            .copied()
            .filter(|target| target.card == card)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "one retirement per conversation");
        targets[0]
    }

    /// Closing the card was a silent no-op while the conversation connected,
    /// so a slow or hung start held the card hostage.
    #[test]
    fn a_chat_still_connecting_closes_at_once_and_retires_what_its_start_produced() {
        for started in [true, false] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (card, target) = starting_chat(&mut app);
            let handle = fixture_handle(&app, card, None);
            close_chat(&mut app, card);
            assert!(
                app.chat_ui.abandoned.contains_key(&target),
                "the start is still answered for"
            );
            assert!(app.chat_ui.retiring.is_empty(), "nothing is certain yet");
            assert!(app.chat_ui.transition_pending());

            let result = if started {
                Ok(handle)
            } else {
                Err(chat::StartFailure::retry("fixture: executable unavailable"))
            };
            answer_start(&mut app, target, result);
            assert!(app.chat_ui.abandoned.is_empty());
            assert!(
                !app.cards.contains_key(&card),
                "a late answer never brings the card back"
            );
            let retirement = retired(&mut app, card);
            let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
            assert!(!app.chat_ui.transition_pending());
            assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        }
    }

    /// The failed card's own Close button did nothing.
    #[test]
    fn a_chat_whose_start_failed_closes() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        close_chat(&mut app, card);
        // With no connection, the daemon's inventory is what settles it.
        let retirement = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(
            retirement,
            Err("fixture: daemon unreachable".into()),
        )));
        assert!(
            !errors(&app).is_empty(),
            "a conversation that may still run is reported"
        );
        assert!(!app.chat_ui.transition_pending());
    }

    /// A successful close committed a removal the workspace had never been
    /// asked for, which failed with "no card removal is pending" and left a
    /// tombstoned card on the strip, spinning, with no way to close it.
    #[test]
    fn a_live_chat_closes_without_leaving_its_card_behind() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("thread"));
        close_chat(&mut app, card);
        retired(&mut app, card);
    }

    /// What a conversation's provider reports about its account reaches the
    /// account it runs on, whether it raced the attachment or woke the
    /// surface after it.
    #[test]
    fn a_conversations_account_report_reaches_its_account() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, Some("thread"));
        let account = crate::account::Account::system(Provider::Codex).id();
        let report = |used: u32| {
            crate::account::LiveReport::CodexLimits(
                crate::account::codex::parse_update(&serde_json::json!({"rateLimits": {
                    "primary": {"usedPercent": used, "windowDurationMins": 300},
                }}))
                .unwrap(),
            )
        };
        let used = |app: &Cagents| {
            app.account_ui
                .snapshot_for_test(&account)
                .and_then(|snapshot| snapshot.windows.first())
                .map(|window| window.used)
        };
        let now = crate::chat::now_ms().unwrap();
        // Live figures refine a reading the account already has.
        let _ = app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
            id: account.clone(),
            outcome: crate::account::probe::Outcome::Answered {
                catalog: None,
                identity: None,
                snapshot: crate::account::UsageSnapshot::state(
                    crate::account::UsageStatus::Ok,
                    "",
                    now - 1,
                ),
            },
        }));
        assert!(handle.push_account_usage_for_test(report(20), now));
        // The wake it sent lands while the start is still pending, which
        // leaves the report to the attachment.
        let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::AccountUsage)));
        assert_eq!(used(&app), None);
        answer_start(&mut app, target, Ok(handle.clone()));
        assert_eq!(used(&app), Some(0.2));
        assert!(
            handle.push_account_usage_for_test(report(60), now + 1),
            "the queue was drained, so the next report wakes the surface"
        );
        assert!(!handle.push_account_usage_for_test(report(70), now + 2));
        let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::AccountUsage)));
        assert_eq!(used(&app), Some(0.7));
        assert!(handle.take_account_usage().is_empty());

        // A wake that arrives while the conversation is being reconnected is
        // not read, but its reports do not stay queued to silence the next.
        let _ = app.update(Message::ChatUi(Action::Reconnect(target)));
        assert!(app.chat_start_pending(card));
        assert!(handle.push_account_usage_for_test(report(90), now + 3));
        let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::AccountUsage)));
        assert_eq!(used(&app), Some(0.7));
        assert!(
            handle.push_account_usage_for_test(report(95), now + 4),
            "the next report wakes the surface again"
        );
    }

    /// A disconnected conversation's close request failed before it was sent,
    /// and the failure only set a status: the card stayed forever.
    #[test]
    fn a_disconnected_chat_closes() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("thread"));
        let target = match app.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime.target,
            _ => unreachable!(),
        };
        let _ = app.update(Message::ChatUi(Action::Wake(
            target,
            Wake::Disconnected("fixture: daemon went away".into()),
        )));
        app.notices.entries.clear();
        close_chat(&mut app, card);
        retired(&mut app, card);
    }

    #[test]
    fn closing_a_chat_twice_is_one_close() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, None);
        let _ = app.update(Message::CloseCard(card));
        let _ = app.update(Message::CloseCard(card));
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        let _ = app.update(Message::CommitClose(card));
        assert!(!app.workspace.order().any(|id| id == card));
        retired(&mut app, card);
    }

    /// A new conversation starts on its provider's default account, and a
    /// draft moves to another account without starting anything; the
    /// account's home is in the recipe its first message starts, which every
    /// later resume keeps.
    #[test]
    fn a_new_chat_runs_on_the_chosen_account_through_its_recipe() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let managed = app.account_ui.add_managed_fixture(Provider::Codex, 21);
        let home = app
            .known_account(&managed)
            .and_then(|account| account.resolved_home())
            .unwrap()
            .display()
            .to_string();

        let card = draft_chat(&mut app);
        let dropped = Attachment {
            path: PathBuf::from("/tmp/dropped.png"),
        };
        let target = match app.cards.get_mut(&card) {
            Some(CardRuntime::Chat(runtime)) => {
                runtime.attachments.push(dropped.clone());
                runtime.target
            }
            _ => unreachable!(),
        };
        let _ = app.update(Message::ChatUi(Action::ChooseAccount(
            target,
            managed.clone(),
        )));
        assert!(!app.chat_start_pending(card), "a choice starts nothing");
        // The composer outlives the move: what the reader attached is still
        // there on the new account.
        let runtime = chat_runtime(&app, card);
        assert!(runtime.is_draft());
        assert_eq!(runtime.attachments, [dropped]);
        assert_eq!(
            runtime.session.launch.recipe.environment.get("CODEX_HOME"),
            Some(&home)
        );
        write(&mut app, card, "hello");
        submit(&mut app, card);
        let attempt = app.chat_ui.attempts.get(&card).unwrap();
        assert_eq!(
            attempt.session.launch.recipe.environment.get("CODEX_HOME"),
            Some(&home)
        );
        assert_eq!(
            attempt.session.launch.recipe.arguments,
            ["--remote", "unix://"]
        );

        let _ = app.update(Message::AccountUi(
            crate::app::account_ui::Action::SetDefault(managed),
        ));
        let _ = app.new_native_chat(Provider::Codex, Some(PathBuf::from(FIXTURE_DIRECTORY)));
        let fresh = app.workspace.focused().unwrap();
        assert_eq!(
            chat_runtime(&app, fresh)
                .session
                .launch
                .recipe
                .environment
                .get("CODEX_HOME"),
            Some(&home),
            "a new conversation starts on the default account"
        );
    }

    /// Makes the focused fixture card a stopped agent terminal running
    /// `arguments`, and returns it.
    fn stopped_agent_terminal(app: &mut Cagents, arguments: &[&str], exited: bool) -> CardId {
        let card = app.workspace.focused().unwrap();
        let launch = LaunchRecipe {
            program: "claude".into(),
            arguments: arguments.iter().map(|&argument| argument.into()).collect(),
            working_directory: PathBuf::from(FIXTURE_DIRECTORY),
            environment: Default::default(),
        };
        let mut state = app.card_state(card).unwrap().clone();
        let CardKind::Terminal(terminal) = &mut state.kind else {
            unreachable!()
        };
        terminal.launch = launch.clone();
        terminal.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: exited.then(|| crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 1,
            }),
        };
        Arc::make_mut(&mut app.durable).cards[0] = state;
        if exited {
            // Its agent has ended, so nothing will start in it.
            app.cards.insert(card, CardRuntime::Completed);
        }
        card
    }

    /// A stopped agent terminal switched to chat opens a new conversation in
    /// a card of its own beside the terminal. It used to become that
    /// conversation in place while still naming the terminal as its source,
    /// which storage refuses for anything but the exact handoff of one
    /// conversation, so every later save of the window failed and the next
    /// launch brought back the window as it was before the switch. The
    /// terminal stays, because it still relaunches the conversation it ran.
    #[test]
    fn a_stopped_terminal_switched_to_chat_opens_a_new_conversation_beside_it() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot-test"),
        )
        .unwrap();
        app.session = Some(writer);
        let card = stopped_agent_terminal(
            &mut app,
            &["--resume", KNOWN_THREAD, "--dangerously-skip-permissions"],
            true,
        );
        let terminal = app.card_state(card).unwrap().clone();
        let neighbour = live_chat(&mut app, Some("neighbour"));
        // Filed under a worktree its directory is not in, as an adopted
        // terminal is; the filing, not the directory, is what places the
        // conversation with it.
        let filed = PathBuf::from("/workspace/filed");
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(card, filed.clone());
        let _ = app.update(Message::FocusCard(card));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), [card, neighbour]);
        let saved = |app: &Cagents, id: CardId| {
            let writer = app.session.as_ref().unwrap();
            writer.flush(Duration::from_secs(10)).unwrap();
            assert_eq!(writer.last_error(), None, "every save is accepted");
            writer
                .operation_store()
                .load_card(id, None)
                .unwrap()
                .unwrap()
        };

        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        let warnings = app
            .notices
            .entries
            .iter()
            .filter(|entry| matches!(entry.notice.severity, crate::app::NoticeSeverity::Warning))
            .map(|entry| entry.notice.message.clone())
            .collect::<Vec<_>>();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("beside it") && warning.contains("approvals")),
            "a carried bypass is named: {warnings:?}"
        );
        let kept = app.card_state(card).unwrap();
        assert_eq!(kept.kind, terminal.kind, "the terminal is kept as it was");
        assert!(kept.existence.is_open());
        let order = app.workspace.order().collect::<Vec<_>>();
        assert_eq!(order.len(), 3);
        assert_eq!((order[0], order[2]), (card, neighbour));
        let chat = order[1];
        assert_eq!(app.workspace.focused(), Some(chat));
        let state = app.card_state(chat).unwrap();
        assert_eq!(state.width_fraction, terminal.width_fraction);
        let CardKind::Chat(session) = &state.kind else {
            panic!("the new card is a chat");
        };
        assert_eq!(session.launch.provider, Provider::Claude);
        assert_eq!(
            session.launch.session_id, None,
            "the stopped thread is not joined"
        );
        assert!(
            !session
                .launch
                .recipe
                .arguments
                .contains(&KNOWN_THREAD.to_owned()),
            "{:?}",
            session.launch.recipe.arguments
        );
        assert_eq!(session.terminal, None, "no source it did not come from");
        assert!(
            chat_runtime(&app, chat).is_draft(),
            "it waits for its first message"
        );
        assert_eq!(app.durable.card_scopes.get(&chat), Some(&filed));
        let position = |id| app.durable.cards.iter().position(|state| state.id == id);
        assert_eq!(position(chat), position(card).map(|index| index + 1));

        // The window saves both, and keeps saving once the conversation has
        // started.
        assert_eq!(saved(&app, card).kind, terminal.kind);
        assert!(saved(&app, card).existence.is_open());
        assert!(matches!(saved(&app, chat).kind, CardKind::Chat(_)));
        write(&mut app, chat, "hello");
        submit(&mut app, chat);
        let target = app.chat_ui.attempts.get(&chat).unwrap().target;
        let handle = fixture_handle(&app, chat, Some("started"));
        answer_start(&mut app, target, Ok(handle));
        let CardKind::Chat(stored) = saved(&app, chat).kind else {
            panic!("the started conversation is saved");
        };
        assert_eq!(stored.launch.session_id.as_deref(), Some("started"));
        assert!(!app.chat_ui.transition_pending());
    }

    /// A stopped terminal whose relaunch is still starting may yet run its
    /// agent, so no second conversation is opened for it meanwhile.
    #[test]
    fn a_starting_terminal_opens_no_conversation_beside_it() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = stopped_agent_terminal(&mut app, &[], false);
        assert!(app.pending_terminal_spawn(card));
        let before = app.durable.clone();
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        assert_eq!(app.durable.cards, before.cards);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), [card]);
        assert!(app.chat_ui.attempts.is_empty());
        assert!(errors(&app).is_empty(), "a wait, not a failure");
    }

    #[test]
    fn a_bypass_is_recognised_in_every_form_it_is_carried() {
        let recipe = |arguments: &[&str], environment: &[(&str, &str)]| LaunchRecipe {
            program: "agent".into(),
            arguments: arguments.iter().map(|&argument| argument.into()).collect(),
            working_directory: PathBuf::from(FIXTURE_DIRECTORY),
            environment: environment
                .iter()
                .map(|&(key, value)| (key.into(), value.into()))
                .collect(),
        };
        for bypassing in [
            recipe(&["--dangerously-bypass-approvals-and-sandbox"], &[]),
            recipe(&["--yolo"], &[]),
            recipe(&["--dangerously-skip-permissions"], &[]),
            recipe(&["--permission-mode", "bypassPermissions"], &[]),
            recipe(&["--permission-mode=bypassPermissions"], &[]),
            recipe(&[], &[("CLAUDE_CODE_PERMISSION_MODE", "bypassPermissions")]),
        ] {
            assert!(bypasses_approvals(&bypassing), "{bypassing:?}");
        }
        for asking in [
            recipe(&[], &[]),
            recipe(&["--allow-dangerously-skip-permissions"], &[]),
            recipe(&["--permission-mode", "plan"], &[]),
            recipe(&["bypassPermissions"], &[]),
            recipe(&[], &[("CLAUDE_CODE_PERMISSION_MODE", "plan")]),
        ] {
            assert!(!bypasses_approvals(&asking), "{asking:?}");
        }
    }

    /// A draft moved to another provider, then to another account, is saved
    /// as the conversation it now holds, and so is the conversation its first
    /// message starts. Storage used to refuse such moves, which failed every
    /// later save of the window and brought the old conversation back on the
    /// next launch, where the daemon no longer had it.
    #[test]
    fn a_draft_moved_to_another_provider_or_account_is_saved_as_its_new_conversation() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("boot-test"),
        )
        .unwrap();
        app.session = Some(writer);
        let managed = app.account_ui.add_managed_fixture(Provider::Claude, 24);
        let home = app
            .known_account(&managed)
            .and_then(|account| account.resolved_home())
            .unwrap()
            .display()
            .to_string();
        let card = draft_chat(&mut app);
        let flushed = |app: &Cagents| {
            let writer = app.session.as_ref().unwrap();
            writer.flush(Duration::from_secs(10)).unwrap();
            assert_eq!(writer.last_error(), None);
            match writer.operation_store().load_card(card, None).unwrap() {
                Some(CardState {
                    kind: CardKind::Chat(stored),
                    ..
                }) => stored,
                other => panic!("the card is stored as a chat: {other:?}"),
            }
        };
        let target = |app: &Cagents| chat_runtime(app, card).target;
        // The started conversation adds its provider's name for it and the
        // daemon's proof; what a card may not change on its own is the rest.
        let identity = |session: &chat::Session| {
            (
                session.id,
                session.launch.provider,
                session.launch.recipe.clone(),
            )
        };
        assert_eq!(flushed(&app).launch.provider, Provider::Codex);

        let _ = app.update(Message::ChatUi(Action::DraftModel(
            target(&app),
            Provider::Claude,
            None,
        )));
        assert!(!app.chat_start_pending(card), "a choice starts nothing");
        let claude = chat_runtime(&app, card).session.clone();
        assert_eq!(claude.launch.provider, Provider::Claude);
        assert_eq!(identity(&flushed(&app)), identity(&claude));
        assert_eq!(app.card_state(card).unwrap().title, "Claude chat");

        let _ = app.update(Message::ChatUi(Action::ChooseAccount(
            target(&app),
            managed.clone(),
        )));
        assert!(!app.chat_start_pending(card));
        let moved = chat_runtime(&app, card).session.clone();
        assert_eq!(identity(&flushed(&app)), identity(&moved));
        assert_eq!(
            moved.launch.recipe.environment.get("CLAUDE_CONFIG_DIR"),
            Some(&home)
        );

        // The first message starts the conversation under an identity of its
        // own, which is saved before the provider has answered.
        write(&mut app, card, "hello");
        submit(&mut app, card);
        let attempt = app.chat_ui.attempts.get(&card).unwrap();
        let (started, session) = (attempt.target, attempt.session.clone());
        assert_ne!(session.id, moved.id);
        assert_eq!(session.launch.recipe, moved.launch.recipe);
        assert_eq!(identity(&flushed(&app)), identity(&session));
        let handle = fixture_handle(&app, card, Some("replacement"));
        answer_start(&mut app, started, Ok(handle));
        let stored = flushed(&app);
        assert_eq!(identity(&stored), identity(&session));
        assert_eq!(stored.launch.session_id.as_deref(), Some("replacement"));

        // A resume the provider refused is offered a fresh start, which
        // drops the saved conversation on purpose.
        app.cards.remove(&card);
        app.chat_ui.fresh_offers.insert(card);
        let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
        let attempt = app.chat_ui.attempts.get(&card).unwrap();
        let (target, fresh) = (attempt.target, attempt.session.clone());
        assert_eq!(fresh.launch.session_id, None);
        let handle = fixture_handle(&app, card, Some("fresh"));
        answer_start(&mut app, target, Ok(handle));
        let stored = flushed(&app);
        assert_eq!(identity(&stored), identity(&fresh));
        assert_eq!(stored.launch.session_id.as_deref(), Some("fresh"));
        // A late or repeated press, once nothing offers it, keeps it.
        let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
        assert!(!app.chat_ui.attempts.contains_key(&card));
        assert_eq!(flushed(&app), stored);
    }

    /// A conversation that has begun keeps its account.
    #[test]
    fn a_begun_chat_cannot_change_account() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let managed = app.account_ui.add_managed_fixture(Provider::Codex, 22);
        let card = live_chat(&mut app, Some("thread"));
        let target = match app.cards.get_mut(&card) {
            Some(CardRuntime::Chat(runtime)) => {
                Arc::make_mut(&mut runtime.snapshot).items.push(chat::Item {
                    id: "prompt".into(),
                    role: chat::Role::User,
                    content: text("hello"),
                    at: None,
                });
                runtime.target
            }
            _ => unreachable!(),
        };
        let _ = app.update(Message::ChatUi(Action::AccountPicker(target, true)));
        let _ = app.update(Message::ChatUi(Action::ChooseAccount(target, managed)));
        assert!(app.chat_ui.restarting.is_empty());
        assert!(!app.chat_ui.closing.contains(&target));
        match app.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => assert!(!runtime.account_picker),
            _ => unreachable!(),
        }
    }

    /// A live conversation whose provider stopped without ever loading the
    /// conversation it was asked to resume, which offers a fresh start.
    fn unresumable_chat(app: &mut Cagents) -> (CardId, Target) {
        let card = live_chat(app, Some(KNOWN_THREAD));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            unreachable!()
        };
        runtime.session.launch.session_id = Some(KNOWN_THREAD.into());
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.exited = true;
        snapshot.session_id = None;
        let target = runtime.target;
        assert!(app.chat_offers_fresh_conversation(card));
        (card, target)
    }

    /// A fresh start gives up the previous conversation before it starts the
    /// new one; a close meanwhile keeps the first half and cancels the
    /// second.
    #[test]
    fn a_chat_closed_mid_restart_never_starts_its_replacement() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = unresumable_chat(&mut app);
        let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
        assert!(app.chat_ui.closing.contains(&target));
        close_chat(&mut app, card);
        assert!(
            app.chat_ui.retiring.is_empty(),
            "the restart's close is the retirement"
        );
        let _ = app.update(Message::ChatUi(Action::CloseFinished(target, Ok(()))));
        assert!(
            app.chat_ui.attempts.is_empty(),
            "the replacement never starts"
        );
        assert!(!app.chat_ui.transition_pending());
    }

    /// A draft's first message starts its conversation, so it is refused in
    /// a worktree being deleted, and the message stays where it was written.
    #[test]
    fn a_draft_in_a_worktree_being_deleted_does_not_start() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        app.project_ui
            .begin_deleting(PathBuf::from(FIXTURE_DIRECTORY));
        write(&mut app, card, "hello");
        submit(&mut app, card);
        assert!(app.chat_ui.attempts.is_empty());
        let runtime = chat_runtime(&app, card);
        assert!(runtime.queued.is_none());
        assert_eq!(runtime.composer.text().trim(), "hello");
        assert_eq!(
            app.notices
                .entries
                .back()
                .map(|entry| entry.notice.message.as_str()),
            Some("That worktree is being deleted.")
        );
    }

    /// A fresh start already closing the old conversation when its worktree
    /// is confirmed for deletion must not start the replacement there.
    #[test]
    fn a_restart_under_way_never_starts_its_replacement_in_a_worktree_being_deleted() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = unresumable_chat(&mut app);
        let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
        assert!(app.chat_ui.closing.contains(&target));
        app.project_ui
            .begin_deleting(PathBuf::from(FIXTURE_DIRECTORY));
        app.sidebar_state.set_card_activity(
            card,
            crate::agent::Activity {
                provider: Provider::Codex,
                state: crate::agent::ActivityState::Working,
            },
        );

        let _ = app.update(Message::ChatUi(Action::CloseFinished(target, Ok(()))));
        assert!(
            app.chat_ui.attempts.is_empty(),
            "the replacement started in the worktree being deleted"
        );
        assert!(!app.chat_ui.restarting.contains_key(&card));
        assert!(!app.chat_start_pending(card));
        let runtime = chat_runtime(&app, card);
        assert!(!runtime.connected());
        assert_eq!(
            runtime.error.as_deref(),
            Some("Restart cancelled: the worktree was being deleted.")
        );
        assert!(
            app.sidebar_state.card_activity(card).is_none(),
            "the closed conversation still shows as active"
        );
        assert_eq!(
            app.notices
                .entries
                .back()
                .map(|entry| entry.notice.message.as_str()),
            Some("That worktree is being deleted.")
        );
    }

    /// The terminal cards open in the current scope.
    fn terminal_cards(app: &Cagents) -> Vec<CardId> {
        app.workspace
            .order()
            .filter(|id| {
                matches!(
                    app.card_state(*id).map(|state| &state.kind),
                    Some(CardKind::Terminal(_))
                )
            })
            .collect()
    }

    /// Checks that a returned chat leaves, and that a new terminal running
    /// `launch` takes its place only once it has.
    fn assert_replaced_by_terminal(app: &mut Cagents, card: CardId, launch: &LaunchRecipe) {
        let chat = match &app.card_state(card).unwrap().kind {
            CardKind::Chat(chat) => chat.id,
            _ => panic!("the chat card is never converted in place"),
        };
        assert!(
            terminal_cards(app).is_empty(),
            "no state holds the chat beside its replacement"
        );
        assert_eq!(
            app.workspace
                .pending_close()
                .map(|handoff| handoff.unmounting),
            Some(card)
        );
        let before = app.workspace.order().collect::<Vec<_>>();
        let slot = before.iter().position(|&id| id == card).unwrap();
        let _ = app.update(Message::CommitClose(card));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.cards.contains_key(&card));
        assert!(!app.chat_ui.closed.contains(&card));
        assert!(app.chat_ui.replacements.is_empty());
        let terminals = terminal_cards(app);
        assert_eq!(terminals.len(), 1, "one terminal takes the chat's place");
        let terminal = terminals[0];
        let Some(CardKind::Terminal(session)) = app.card_state(terminal).map(|state| &state.kind)
        else {
            unreachable!()
        };
        assert_eq!(&session.launch, launch);
        assert_ne!(
            session.id, chat,
            "a new terminal, not the conversation's identity"
        );
        let order = app.workspace.order().collect::<Vec<_>>();
        assert!(!order.contains(&card));
        assert_eq!(order.len(), before.len());
        assert_eq!(order[slot], terminal, "in the chat's own slot");
        let saved = |id| app.durable.cards.iter().position(|state| state.id == id);
        assert_eq!(saved(terminal), saved(card).map(|index| index + 1));
        assert_eq!(app.workspace.focused(), Some(terminal));
        assert!(matches!(
            app.cards.get(&terminal),
            Some(CardRuntime::Loading)
        ));
        assert!(!app.chat_ui.transition_pending());
    }

    /// "Return to terminal" refused a conversation the provider had not named
    /// yet, and silently did nothing for one that never started, so a chat
    /// that could not go back had no terminal to offer at all.
    #[test]
    fn a_chat_with_no_conversation_to_return_to_switches_to_a_shell_in_its_directory() {
        for failed in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let account = app.account_ui.add_managed_fixture(Provider::Codex, 17);
            let _ = app.update(Message::AccountUi(
                crate::app::account_ui::Action::SetDefault(account),
            ));
            let home = app
                .default_account(Provider::Codex)
                .resolved_home()
                .unwrap();
            let card = if failed {
                failed_chat(&mut app)
            } else {
                live_chat(&mut app, None)
            };
            assert_eq!(app.chat_switch_label(card), "Switch to a shell");
            assert!(app.chat_switch_ready(card));
            let _ = app.update(Message::ChatUi(Action::Switch(card)));
            assert!(errors(&app).is_empty(), "{:?}", errors(&app));
            let (&target, plan) = app.chat_ui.local_returns.iter().next().unwrap();
            assert!(!plan.resumes);
            let mut shell = LaunchRecipe::shell(PathBuf::from(FIXTURE_DIRECTORY)).unwrap();
            shell
                .environment
                .insert("CODEX_HOME".into(), home.display().to_string());
            assert_eq!(plan.launch, shell);
            assert!(!app.chat_switch_ready(card), "one return at a time");

            // The shell shares nothing with the actor, so doubt about the
            // actor does not keep the card a chat; the actor is given up
            // again, and that retirement reports whether it succeeded.
            let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
                target,
                Err(ReturnRefused::Unconfirmed(
                    "fixture: daemon unreachable".into(),
                )),
            )));
            assert!(errors(&app).is_empty(), "{:?}", errors(&app));
            let retry = retired(&mut app, card);
            let _ = app.update(Message::ChatUi(Action::Retired(retry, Ok(()))));
            assert_replaced_by_terminal(&mut app, card, &shell);
        }
    }

    /// A failed card whose conversation the provider did name goes back to
    /// that conversation in its CLI, but never beside an actor that might
    /// still be holding it.
    #[test]
    fn a_failed_chat_with_a_known_conversation_resumes_it_in_its_cli() {
        for retired in [true, false] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = failed_chat(&mut app);
            let state = Arc::make_mut(&mut app.durable)
                .cards
                .iter_mut()
                .find(|state| state.id == card)
                .unwrap();
            let CardKind::Chat(session) = &mut state.kind else {
                unreachable!()
            };
            session.launch.session_id = Some(KNOWN_THREAD.into());
            let resume = session.launch.terminal_recipe().unwrap();
            assert_eq!(app.chat_switch_label(card), "Return to terminal");
            let _ = app.update(Message::ChatUi(Action::Switch(card)));
            assert!(errors(&app).is_empty(), "{:?}", errors(&app));
            let (&target, plan) = app.chat_ui.local_returns.iter().next().unwrap();
            assert!(plan.resumes);
            assert_eq!(plan.launch, resume, "the CLI the daemon would resume");
            assert!(
                resume
                    .arguments
                    .iter()
                    .any(|argument| argument == KNOWN_THREAD),
                "{resume:?}"
            );
            let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
                target,
                if retired {
                    Ok(())
                } else {
                    Err(ReturnRefused::Unconfirmed(
                        "fixture: daemon unreachable".into(),
                    ))
                },
            )));
            if retired {
                assert_replaced_by_terminal(&mut app, card, &resume);
            } else {
                assert!(terminal_cards(&app).is_empty(), "no second writer");
                assert!(matches!(
                    app.card_state(card).unwrap().kind,
                    CardKind::Chat(_)
                ));
                assert_eq!(app.workspace.pending_close(), None);
                assert!(!errors(&app).is_empty());
                assert!(!app.chat_ui.transition_pending());
            }
            assert!(!app.chat_start_pending(card));
        }
    }

    /// A Codex chat and the CLI the daemon still runs for its thread end
    /// together, so a chat that cannot take the daemon's handoff keeps its
    /// card rather than kill that CLI, and a close then closes it as usual.
    #[test]
    fn a_codex_chat_whose_cli_still_runs_is_kept_rather_than_killing_it() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = *app.chat_ui.local_returns.keys().next().unwrap();
        let _ = app.update(Message::CloseCard(card));
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
            target,
            Err(ReturnRefused::SourceLive),
        )));
        assert!(terminal_cards(&app).is_empty());
        assert!(!errors(&app).is_empty(), "the refusal says why");
        app.notices.entries.clear();
        // The close asked for meanwhile still happens, and retires the chat.
        assert_eq!(
            app.workspace
                .pending_close()
                .map(|handoff| handoff.unmounting),
            Some(card)
        );
        let _ = app.update(Message::CommitClose(card));
        assert!(!app.workspace.order().any(|id| id == card));
        retired(&mut app, card);

        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = *app.chat_ui.local_returns.keys().next().unwrap();
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
            target,
            Err(ReturnRefused::SourceLive),
        )));
        assert!(terminal_cards(&app).is_empty());
        assert!(matches!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(_)
        ));
        assert_eq!(app.workspace.pending_close(), None);
        assert!(app.chat_switch_ready(card), "it can be tried again");
        assert!(!app.chat_ui.transition_pending());
    }

    /// Activity that arrives while a chat returns to its terminal is not read
    /// then, and the connection wakes for activity again only once it has
    /// been read. A chat whose return failed stays on that connection, so it
    /// reads it then, or a hidden card would never show activity again.
    #[test]
    fn a_chat_whose_return_failed_reads_the_activity_it_missed() {
        use crate::agent::ActivityState;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = *app.chat_ui.handoffs.iter().next().unwrap();
        let handle = chat_runtime(&app, card).handle().unwrap().clone();
        assert!(handle.observe_activity_for_test(ActivityState::Working));
        let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Activity)));
        let _ = app.update(Message::ChatUi(Action::HandoffFinished(
            target,
            Err("fixture: the terminal could not start".into()),
        )));
        assert!(matches!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(_)
        ));
        assert_eq!(
            app.sidebar_state
                .card_activity(card)
                .map(|activity| activity.state),
            Some(ActivityState::Working)
        );
        assert!(
            handle.observe_activity_for_test(ActivityState::Idle),
            "the next edge wakes the surface again"
        );
    }

    /// A close pressed while the card is on its way back to a terminal waits
    /// for that return to settle, then closes the card instead of starting a
    /// terminal only to stop it.
    #[test]
    fn a_chat_closed_during_its_return_closes_once_the_return_lands() {
        for confirmed in [true, false] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, None);
            let _ = app.update(Message::ChatUi(Action::Switch(card)));
            let target = *app.chat_ui.local_returns.keys().next().unwrap();
            let _ = app.update(Message::CloseCard(card));
            assert!(app.chat_ui.close_after_handoff.contains(&card));
            assert_eq!(app.workspace.pending_close(), None);
            assert!(errors(&app).is_empty(), "{:?}", errors(&app));
            let result = if confirmed {
                Ok(())
            } else {
                Err(ReturnRefused::Unconfirmed(
                    "fixture: daemon unreachable".into(),
                ))
            };
            let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, result)));
            assert!(app.chat_ui.close_after_handoff.is_empty());
            assert!(app.chat_ui.replacements.is_empty());
            assert_eq!(
                app.chat_ui.retiring.is_empty(),
                confirmed,
                "a conversation not confirmed stopped is given up again"
            );
            assert_eq!(
                app.workspace
                    .pending_close()
                    .map(|handoff| handoff.unmounting),
                Some(card),
                "the close the user asked for goes ahead"
            );
            let _ = app.update(Message::CommitClose(card));
            assert!(
                terminal_cards(&app).is_empty(),
                "no terminal only to stop it"
            );
            assert!(!app.workspace.order().any(|id| id == card));
            assert!(!app.card_state(card).unwrap().existence.is_open());
            assert!(!app.cards.contains_key(&card));
            if !confirmed {
                let retry = retired(&mut app, card);
                let _ = app.update(Message::ChatUi(Action::Retired(retry, Ok(()))));
            }
            assert!(!app.chat_ui.transition_pending());
        }
    }

    /// A return that lands once its chat is no longer on screen, or once its
    /// removal cannot be reserved, keeps the chat rather than opening a
    /// terminal in a strip the chat is not in, or beside a chat that stays.
    #[test]
    fn a_return_that_cannot_take_the_chats_place_keeps_the_chat() {
        for away in [true, false] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let other = failed_chat(&mut app);
            let card = failed_chat(&mut app);
            let _ = app.update(Message::ChatUi(Action::Switch(card)));
            let target = *app.chat_ui.local_returns.keys().next().unwrap();
            if away {
                // The user has moved to another strip meanwhile.
                app.workspace =
                    crate::workspace::Workspace::new(crate::workspace::WorkspaceConfig::default())
                        .unwrap();
            } else {
                // Another card's close is waiting for its commit.
                app.workspace.request_close(other).unwrap();
            }
            let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
            assert!(terminal_cards(&app).is_empty());
            assert!(app.chat_ui.replacements.is_empty());
            assert!(!app.chat_ui.closed.contains(&card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(matches!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(_)
            ));
            assert!(app.chat_switch_ready(card), "it can be tried again");
            assert!(!app.chat_ui.transition_pending());
        }
    }

    /// A send pressed after the removal was reserved never starts a
    /// conversation in the card it is removing.
    #[test]
    fn a_closing_draft_never_starts() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        write(&mut app, card, "hello");
        let target = chat_runtime(&app, card).target;
        let _ = app.update(Message::CloseCard(card));
        let _ = app.handle_chat_ui(Action::Submit(target, PromptMode::Send));
        assert!(!app.chat_start_pending(card));
    }

    /// Issue #237: a channel whose panes are all closed closes itself, in
    /// either order, including when its chat never got as far as starting.
    #[test]
    fn a_channel_closes_when_its_last_pane_does() {
        for (chat_first, chat_state) in [(true, 0), (true, 1), (true, 2), (false, 0), (false, 2)] {
            let mut app = crate::app::tests::context_inspector_test_app(true);
            let terminal = app.workspace.focused().unwrap();
            // A shell that already ended: its close needs no daemon.
            app.cards
                .insert(terminal, CardRuntime::Failed("fixture".into()));
            let channel = crate::model::ChannelId::new();
            let durable = Arc::make_mut(&mut app.durable);
            durable.channels.push(crate::model::ProjectChannel {
                id: channel,
                project_id: crate::model::ProjectId::new(),
                title: String::new(),
                ordinal: 0,
                shared: false,
                cards: vec![terminal],
                focused: Some(terminal),
            });
            durable.active_channel = Some(channel);
            // Already in the chat's directory, so starting it changes no scope.
            app.active_workspace = Some(PathBuf::from(FIXTURE_DIRECTORY));
            let chat = match chat_state {
                0 => starting_chat(&mut app).0,
                1 => failed_chat(&mut app),
                _ => live_chat(&mut app, Some("thread")),
            };
            let channel_exists = |app: &Cagents| {
                app.durable
                    .channels
                    .iter()
                    .any(|candidate| candidate.id == channel)
            };
            let (first, second) = if chat_first {
                (chat, terminal)
            } else {
                (terminal, chat)
            };
            for (index, card) in [first, second].into_iter().enumerate() {
                if card == chat {
                    close_chat(&mut app, card);
                } else {
                    let _ = app.update(Message::CloseCard(card));
                    let _ = app.update(Message::CommitClose(card));
                    assert!(!app.workspace.order().any(|id| id == card));
                }
                assert_eq!(
                    channel_exists(&app),
                    index == 0,
                    "chat first: {chat_first}, chat state: {chat_state}"
                );
            }
            assert!(app.channel_ui.was_deleted(channel));
        }
    }

    /// A closed chat whose commit was dropped and whose reservation a rebuilt
    /// strip lost closes again when asked, instead of staying on screen with
    /// its conversation already given up.
    #[test]
    fn a_closed_chat_that_lost_its_reservation_closes_again() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let _ = app.update(Message::CloseCard(card));
        assert!(app.chat_close_approved(card));
        app.workspace.cancel_close(card).unwrap();
        assert_eq!(app.workspace.pending_close(), None);
        close_chat(&mut app, card);
    }

    /// Retry between a chat's close and its commit starts nothing, and a start
    /// still in flight when a card is discarded is given up when it answers
    /// rather than leaving a daemon actor no card can reach.
    #[test]
    fn a_discarded_card_never_strands_a_start() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let _ = app.update(Message::CloseCard(card));
        let _ = app.update(Message::RetryCard(card));
        assert!(
            !app.chat_start_pending(card),
            "retry after close starts nothing"
        );

        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        app.chat_discard(card);
        assert!(
            app.chat_ui.transition_pending(),
            "the start is still owed an answer"
        );
        answer_start(&mut app, target, Ok(handle));
        let retry = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(retry, Ok(()))));
        assert!(!app.chat_ui.transition_pending());
    }

    /// A close pressed while a live Codex chat returns to its CLI lands on a
    /// card that is the CLI with the chat parked behind it. The chat must be
    /// closed before the CLI, as a close of that card always does; closing the
    /// CLI first reserved the card's removal twice and left it stuck.
    #[test]
    fn a_codex_chat_closed_during_its_daemon_return_closes_its_cli_too() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = *app.chat_ui.handoffs.iter().next().unwrap();
        let _ = app.update(Message::CloseCard(card));
        assert!(app.chat_ui.close_after_handoff.contains(&card));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            unreachable!()
        };
        let mut session = runtime.session.clone();
        session.launch.session_id = Some(KNOWN_THREAD.into());
        let proof = runtime.handle().unwrap().proof().unwrap();
        let mut cli = return_terminal(&session).unwrap();
        cli.continuity = TerminalContinuity::Live {
            host_session_id: proof.host_session_id.clone(),
            boot_id: proof.boot_id.clone(),
            host_incarnation: proof.host_incarnation.clone(),
        };
        let _ = app.update(Message::ChatUi(Action::HandoffFinished(target, Ok(cli))));
        // Drive every daemon request to success until nothing is left to do.
        for _ in 0..8 {
            if let Some(intent) = app.terminal_close.clone() {
                assert!(intent.detail.is_none(), "{:?}", intent.detail);
                let _ = app.update(Message::TerminalCloseFinished {
                    card: intent.card,
                    terminal: intent.terminal,
                    proof: intent.proof,
                    generation: intent.generation,
                    outcome: crate::app::terminal_close::CloseOutcome::Closed,
                });
            } else if app
                .workspace
                .pending_close()
                .is_some_and(|handoff| handoff.unmounting == card)
            {
                let _ = app.update(Message::CommitClose(card));
            } else if let Some(&closing) = app.chat_ui.closing.iter().next() {
                let _ = app.update(Message::ChatUi(Action::CloseFinished(closing, Ok(()))));
            } else {
                break;
            }
        }
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert_eq!(app.workspace.pending_close(), None);
        assert!(app.terminal_close.is_none());
        assert!(!app.chat_has_parked_runtime(card));
        assert!(!app.chat_ui.transition_pending());
    }

    /// A chat that switches to a shell leaves its channel only for the shell
    /// that takes its place; the channel is not empty, so it stays.
    #[test]
    fn a_chat_replaced_by_a_shell_keeps_its_channel() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let channel = crate::model::ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            id: channel,
            project_id: crate::model::ProjectId::new(),
            title: String::new(),
            ordinal: 0,
            shared: false,
            cards: Vec::new(),
            focused: None,
        });
        durable.active_channel = Some(channel);
        app.active_workspace = Some(PathBuf::from(FIXTURE_DIRECTORY));
        let card = failed_chat(&mut app);
        let members = |app: &Cagents| {
            app.durable
                .channels
                .iter()
                .find(|candidate| candidate.id == channel)
                .map(|channel| channel.cards.clone())
        };
        assert_eq!(members(&app), Some(vec![card]), "the chat's only pane");
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = *app.chat_ui.local_returns.keys().next().unwrap();
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
        let mut shell = LaunchRecipe::shell(PathBuf::from(FIXTURE_DIRECTORY)).unwrap();
        shell.environment.insert(
            "CODEX_HOME".into(),
            app.default_account(Provider::Codex)
                .resolved_home()
                .unwrap()
                .display()
                .to_string(),
        );
        assert_replaced_by_terminal(&mut app, card, &shell);
        let terminal = terminal_cards(&app)[0];
        assert!(
            !app.channel_ui.is_deleting(channel) && !app.channel_ui.was_deleted(channel),
            "the channel still holds the shell"
        );
        assert_eq!(members(&app), Some(vec![terminal]));
        assert!(app.card_state(terminal).unwrap().existence.is_open());
    }

    /// A draft whose first message could not start it stays a draft, with
    /// the message back in the composer, gives up whatever the start left,
    /// and can move to another provider and try again under a new identity.
    #[test]
    fn a_draft_whose_start_failed_keeps_its_message_and_can_move() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let failed = app.chat_ui.attempts.get(&card).unwrap().session.id;
        answer_start(
            &mut app,
            target,
            Err(chat::StartFailure::retry("fixture: executable unavailable")),
        );
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing(), "the card is a draft again");
        assert_eq!(runtime.composer.text().trim(), "hello");
        assert_eq!(
            runtime.error.as_deref(),
            Some("fixture: executable unavailable")
        );
        let retirement = retired(&mut app, card);
        let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));

        let draft = chat_runtime(&app, card).target;
        let _ = app.update(Message::ChatUi(Action::DraftModel(
            draft,
            Provider::Claude,
            None,
        )));
        assert!(!app.chat_start_pending(card));
        submit(&mut app, card);
        let attempt = app.chat_ui.attempts.get(&card).unwrap();
        assert_ne!(attempt.session.id, failed);
        assert_eq!(attempt.session.launch.provider, Provider::Claude);
        assert_eq!(
            attempt.session.launch.recipe.working_directory,
            PathBuf::from(FIXTURE_DIRECTORY)
        );
        assert_eq!(app.workspace.focused(), Some(card));
    }

    fn fixture_model(id: &str, reasoning: &[&str]) -> chat::Model {
        chat::Model {
            id: id.into(),
            label: id.into(),
            reasoning: reasoning.iter().map(|&effort| effort.into()).collect(),
        }
    }

    /// A draft with "hello" written on `model` of the Codex catalog, which
    /// lists `gpt-a` and `gpt-b`, sent and started on a conversation that
    /// lists `listed`; returns the card once the conversation is ready.
    fn queued_on(app: &mut Cagents, model: &str, listed: Vec<chat::Model>) -> CardId {
        let _ = app.observe_listed_models(
            Provider::Codex,
            vec![
                fixture_model("gpt-a", &[]),
                fixture_model("gpt-b", &["low", "high"]),
            ],
        );
        let card = draft_chat(app);
        let target = chat_runtime(app, card).target;
        let _ = app.update(Message::ChatUi(Action::DraftModel(
            target,
            Provider::Codex,
            Some(model.into()),
        )));
        write(app, card, "hello");
        submit(app, card);
        let runtime = chat_runtime(app, card);
        assert_eq!(
            runtime
                .queued
                .as_ref()
                .and_then(|queued| queued.model.clone()),
            Some((model.to_owned(), None))
        );
        assert!(runtime.composer.text().trim().is_empty());
        let target = chat_runtime_attempt(app, card);
        let handle = fixture_handle(app, card, None);
        handle.replace_snapshot_for_test(Snapshot {
            models: listed,
            ready: true,
            ..Snapshot::default()
        });
        answer_start(app, target, Ok(handle));
        card
    }

    fn pending_step(app: &Cagents, card: CardId) -> Option<QueuedStep> {
        chat_runtime(app, card)
            .pending
            .as_ref()
            .and_then(|pending| pending.queued)
    }

    /// A first message chosen for a model goes out once the conversation has
    /// switched to it, and not before.
    #[test]
    fn a_first_message_goes_out_on_the_model_chosen_for_it() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let listed = vec![fixture_model("gpt-a", &[]), fixture_model("gpt-b", &[])];
        let card = queued_on(&mut app, "gpt-b", listed);
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Model));
        settle(&mut app, card, Ok(()));
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
        settle(&mut app, card, Ok(()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.queued.is_none());
        assert!(runtime.pending.is_none());
        assert!(runtime.failures.is_empty(), "{:?}", runtime.failures);
    }

    /// A model the conversation turns out not to offer is not waited for:
    /// the message goes to its default, and the transcript says so.
    #[test]
    fn a_first_message_on_a_model_the_conversation_lacks_goes_to_its_default() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = queued_on(&mut app, "gpt-b", vec![fixture_model("gpt-a", &[])]);
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
        let runtime = chat_runtime(&app, card);
        assert!(
            runtime
                .failures
                .iter()
                .any(|(_, notice)| notice.contains("does not offer gpt-b")),
            "{:?}",
            runtime.failures
        );
    }

    /// A conversation that has not listed its models yet is given time to,
    /// rather than losing the reader's choice at once.
    #[test]
    fn a_first_message_waits_for_the_conversation_to_list_its_models() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = queued_on(&mut app, "gpt-b", Vec::new());
        assert_eq!(pending_step(&app, card), None);
        let runtime = chat_runtime(&app, card);
        let queued = runtime.queued.as_ref().unwrap();
        assert!(queued.rechecking, "it looks again once the wait is over");
        assert!(queued.model.is_some());

        let handle = runtime.handle().unwrap().clone();
        let target = runtime.target;
        handle.replace_snapshot_for_test(Snapshot {
            models: vec![fixture_model("gpt-b", &[])],
            ready: true,
            ..Snapshot::default()
        });
        let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Model));
    }

    /// Nothing is sent before the provider takes commands, and the snapshot
    /// that says it does sends the message; the hidden card of a message
    /// still waiting keeps that snapshot coming.
    #[test]
    fn a_first_message_waits_for_its_provider_to_be_ready() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        handle.replace_snapshot_for_test(Snapshot::default());
        answer_start(&mut app, target, Ok(handle.clone()));
        assert_eq!(pending_step(&app, card), None);
        let runtime = chat_runtime(&app, card);
        assert!(runtime.queued.is_some());
        let _ = runtime.set_visible(false);
        assert!(
            handle.is_visible_for_test(),
            "a hidden card waiting to send still hears from its provider"
        );

        handle.replace_snapshot_for_test(Snapshot {
            ready: true,
            ..Snapshot::default()
        });
        let target = chat_runtime(&app, card).target;
        let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
        settle(&mut app, card, Ok(()));
        assert!(
            !handle.is_visible_for_test(),
            "once sent, the hidden card goes quiet again"
        );
    }

    /// A ready provider's refusal of the first message is its answer, and
    /// is never met by sending the message again: the card goes back to its
    /// draft, with the message where it was written, and the conversation
    /// that holds nothing is given up.
    #[test]
    fn a_refused_first_message_returns_the_card_to_its_draft() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let started = app.chat_ui.attempts.get(&card).unwrap().session.clone();
        let handle = fixture_handle(&app, card, None);
        answer_start(&mut app, target, Ok(handle));
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
        settle(&mut app, card, Err("fixture: refused".into()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing(), "the card is a draft again");
        assert!(runtime.pending.is_none());
        assert_eq!(runtime.composer.text().trim(), "hello");
        assert_eq!(runtime.error.as_deref(), Some("fixture: refused"));
        assert_eq!(
            runtime.session, started,
            "the draft its start was asked for"
        );
        assert!(runtime.session.is_unstarted());
        retired(&mut app, card);
        let CardKind::Chat(saved) = &app.card_state(card).unwrap().kind else {
            panic!("the card is a chat");
        };
        assert_eq!(saved, &started, "a restore reopens it as a draft");
    }

    /// A message the transcript shows arriving was taken, whatever its
    /// sending reported, so it is neither sent again nor handed back. A
    /// conversation that is merely busy does not show that.
    #[test]
    fn a_first_message_that_arrived_is_not_handed_back() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        answer_start(&mut app, target, Ok(handle.clone()));
        handle.replace_snapshot_for_test(Snapshot {
            ready: true,
            busy: true,
            ..Snapshot::default()
        });
        settle(
            &mut app,
            card,
            Err("fixture: acknowledgement timed out".into()),
        );
        assert!(chat_runtime(&app, card).choosing(), "busy is not arrival");

        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        answer_start(&mut app, target, Ok(handle.clone()));
        handle.replace_snapshot_for_test(Snapshot {
            ready: true,
            items: vec![chat::Item {
                id: "prompt".into(),
                role: chat::Role::User,
                content: text("hello"),
                at: None,
            }],
            ..Snapshot::default()
        });
        settle(
            &mut app,
            card,
            Err("fixture: acknowledgement timed out".into()),
        );
        let runtime = chat_runtime(&app, card);
        assert!(runtime.queued.is_none());
        assert!(!runtime.is_draft());
        assert!(runtime.composer.text().trim().is_empty());
        assert!(
            runtime
                .failures
                .iter()
                .any(|(_, notice)| notice.contains("acknowledgement timed out")),
            "{:?}",
            runtime.failures
        );
    }

    /// A provider that cannot start ends its conversation, and the card goes
    /// back to its draft saying why; a warning it carries on past does not.
    #[test]
    fn a_provider_that_cannot_start_returns_the_card_to_its_draft() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        handle.replace_snapshot_for_test(Snapshot {
            error: Some("fixture: deprecated setting".into()),
            ..Snapshot::default()
        });
        answer_start(&mut app, target, Ok(handle.clone()));
        let target = chat_runtime(&app, card).target;
        let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
        assert!(
            chat_runtime(&app, card).queued.is_some(),
            "a warning is waited past"
        );

        handle.replace_snapshot_for_test(Snapshot {
            error: Some("fixture: no state".into()),
            exited: true,
            ..Snapshot::default()
        });
        let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Changed)));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing());
        assert_eq!(runtime.composer.text().trim(), "hello");
        assert_eq!(
            runtime.error.as_deref(),
            Some(
                "The conversation stopped before it took the first message. Its last report: fixture: no state"
            )
        );
        retired(&mut app, card);
    }

    /// A card that goes back to its draft keeps nothing of the conversation
    /// it gave up, not even the notes its first message's sending left.
    #[test]
    fn a_card_back_to_its_draft_keeps_nothing_of_its_conversation() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = queued_on(&mut app, "gpt-b", vec![fixture_model("gpt-a", &[])]);
        assert!(!chat_runtime(&app, card).failures.is_empty());
        settle(&mut app, card, Err("fixture: refused".into()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing());
        assert!(runtime.failures.is_empty(), "{:?}", runtime.failures);
        assert!(runtime.turns.is_empty());
    }

    /// A Codex conversation can be handed to its terminal mid-command, but
    /// not while its first message waits: the terminal would never send it.
    #[test]
    fn a_first_message_waiting_keeps_its_codex_chat_from_its_terminal() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, Some("thread"));
        handle.replace_snapshot_for_test(Snapshot {
            session_id: Some("thread".into()),
            ..Snapshot::default()
        });
        answer_start(&mut app, target, Ok(handle));
        assert_eq!(
            chat_runtime(&app, card).session.launch.provider,
            Provider::Codex
        );
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        assert!(app.chat_ui.handoffs.is_empty(), "no handoff was asked for");
        assert!(chat_runtime(&app, card).queued.is_some());
        assert_eq!(errors(&app), [QUEUED_BLOCKER]);
    }

    /// A conversation that goes away before it takes the first message
    /// leaves the card a draft, not a dead conversation with nothing in it.
    #[test]
    fn a_conversation_lost_before_its_first_message_leaves_a_draft() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        handle.replace_snapshot_for_test(Snapshot::default());
        answer_start(&mut app, target, Ok(handle));
        let target = chat_runtime(&app, card).target;
        let _ = app.update(Message::ChatUi(Action::Wake(
            target,
            Wake::Disconnected("fixture: gone".into()),
        )));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing());
        assert!(!runtime.disconnected);
        assert_eq!(runtime.composer.text().trim(), "hello");
        assert_eq!(runtime.error.as_deref(), Some("fixture: gone"));
        retired(&mut app, card);
    }

    /// A first message can be taken back until it is on its way: while its
    /// conversation starts, and after, while it waits on the provider.
    #[test]
    fn a_first_message_can_be_taken_back_until_it_is_on_its_way() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, started) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        let target = chat_runtime(&app, card).target;
        let _ = app.update(Message::ChatUi(Action::CancelQueued(target)));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing());
        assert_eq!(runtime.composer.text().trim(), "hello");
        assert!(!app.chat_start_pending(card));
        // What the abandoned start produces is given up when it lands.
        answer_start(&mut app, started, Ok(handle));
        retired(&mut app, card);
        assert!(chat_runtime(&app, card).is_draft());

        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        handle.replace_snapshot_for_test(Snapshot::default());
        answer_start(&mut app, target, Ok(handle));
        let target = chat_runtime(&app, card).target;
        let _ = app.update(Message::ChatUi(Action::CancelQueued(target)));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing());
        assert_eq!(runtime.composer.text().trim(), "hello");
        retired(&mut app, card);

        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = starting_chat(&mut app);
        let handle = fixture_handle(&app, card, None);
        answer_start(&mut app, target, Ok(handle));
        assert_eq!(pending_step(&app, card), Some(QueuedStep::Prompt));
        let target = chat_runtime(&app, card).target;
        let _ = app.update(Message::ChatUi(Action::CancelQueued(target)));
        assert_eq!(
            pending_step(&app, card),
            Some(QueuedStep::Prompt),
            "a message on its way is not taken back"
        );
    }

    /// A model is remembered for new drafts once its provider takes it, not
    /// when it is asked for.
    #[test]
    fn a_model_is_remembered_once_its_conversation_takes_it() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, None);
        show_chat_snapshot_for_test(
            &mut app,
            card,
            Snapshot {
                generation: 1,
                models: vec![fixture_model("gpt-b", &[])],
                ready: true,
                ..Snapshot::default()
            },
        );
        let target = chat_runtime(&app, card).target;
        let _ = app.update(Message::ChatUi(Action::Model(target, "gpt-b".into())));
        assert!(!app.chat_ui.last_models.contains_key(&Provider::Codex));
        settle(&mut app, card, Err("fixture: refused".into()));
        assert!(!app.chat_ui.last_models.contains_key(&Provider::Codex));
        let _ = app.update(Message::ChatUi(Action::Model(target, "gpt-b".into())));
        settle(&mut app, card, Ok(()));
        assert_eq!(
            app.chat_ui.last_models.get(&Provider::Codex),
            Some(&("gpt-b".to_owned(), None))
        );
    }

    /// A conversation an earlier version started for a chat nobody wrote in
    /// was never named by its provider, so once its daemon is gone there is
    /// nothing to resume: it reopens as a draft rather than as a failure.
    #[test]
    fn a_never_named_conversation_whose_daemon_is_gone_reopens_as_a_draft() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, None);
        let session = chat_runtime(&app, card).session.clone();
        assert!(!session.is_unstarted());
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
        let _ = app.open_chat(card, session.clone(), chat::Intent::Restore);
        let target = chat_runtime_attempt(&app, card);
        answer_start(
            &mut app,
            target,
            Err(chat::StartFailure::fresh("fixture: daemon gone")),
        );
        let runtime = chat_runtime(&app, card);
        assert!(runtime.choosing());
        assert_eq!(runtime.error, None, "nothing was lost, so nothing is said");
        assert!(runtime.session.is_unstarted());
        assert!(!app.chat_offers_fresh_conversation(card));
        retired(&mut app, card);
        let CardKind::Chat(saved) = &app.card_state(card).unwrap().kind else {
            panic!("the card is a chat");
        };
        assert!(saved.is_unstarted(), "the next launch opens it as a draft");
        assert_eq!(saved.id, session.id);

        // One converted from a CLI terminal may hold a conversation there,
        // so it asks, as before.
        let card = live_chat(&mut app, None);
        let mut session = chat_runtime(&app, card).session.clone();
        session.terminal = Some(crate::app::fresh_terminal_session(LaunchRecipe {
            program: "codex".into(),
            arguments: Vec::new(),
            working_directory: session.launch.recipe.working_directory.clone(),
            environment: Default::default(),
        }));
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
        let _ = app.open_chat(card, session, chat::Intent::Restore);
        let target = chat_runtime_attempt(&app, card);
        answer_start(
            &mut app,
            target,
            Err(chat::StartFailure::fresh("fixture: daemon gone")),
        );
        assert!(app.chat_offers_fresh_conversation(card));
    }

    /// A saved conversation nothing ever ran reopens as a draft, and any
    /// actor an unanswered start left for it is given up; one that did run
    /// reconnects as before.
    #[test]
    fn only_a_conversation_that_ran_reconnects_on_restore() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let session = chat_runtime(&app, card).session.clone();
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
        let _ = app.open_chat(card, session, chat::Intent::Restore);
        assert!(chat_runtime(&app, card).is_draft());
        assert!(!app.chat_start_pending(card));
        retired(&mut app, card);

        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let session = chat_runtime(&app, card).session.clone();
        assert!(!session.is_unstarted());
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
        let _ = app.open_chat(card, session, chat::Intent::Restore);
        assert!(app.chat_start_pending(card));
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

    /// A message naming several skills goes out as one message, each skill
    /// in the provider's own spelling; only a draft that is a command as a
    /// whole is dispatched as one.
    #[test]
    fn a_message_naming_several_skills_is_sent_as_one_message() {
        let skill = |name: &str, sigil: char| chat::Skill {
            name: name.into(),
            description: String::new(),
            invocation: format!("{sigil}{name}"),
        };
        let prompt = |command: Command| match command {
            Command::Prompt { text, .. } => text,
            other => panic!("sent as {other:?}, not a message"),
        };
        let codex = Snapshot {
            skills: vec![skill("commits", '$'), skill("pr-review", '$')],
            capabilities: chat::Capabilities {
                compact: true,
                ..chat::Capabilities::default()
            },
            ..Snapshot::default()
        };
        assert_eq!(
            prompt(
                submission(
                    "/commits the fix, then /pr-review it",
                    &codex,
                    &[],
                    PromptMode::Send
                )
                .unwrap()
            ),
            "$commits the fix, then $pr-review it"
        );
        assert!(matches!(
            submission("/compact", &codex, &[], PromptMode::Send),
            Ok(Command::Compact)
        ));

        let claude = Snapshot {
            commands: vec![chat::SlashCommand {
                name: "clear".into(),
                description: String::new(),
            }],
            skills: vec![skill("alpha", '/'), skill("beta", '/')],
            ..Snapshot::default()
        };
        let attachment = [Attachment {
            path: "shot.png".into(),
        }];
        assert_eq!(
            prompt(
                submission(
                    "/alpha this, and /beta that",
                    &claude,
                    &attachment,
                    PromptMode::Send
                )
                .unwrap()
            ),
            "/alpha this, and /beta that"
        );
        // A skill alone is a message Claude Code expands, not a command.
        assert_eq!(
            prompt(submission("/alpha", &claude, &[], PromptMode::Send).unwrap()),
            "/alpha"
        );
        // Claude Code expands a skill only from the message's first
        // character, so blank lines ahead of one are not sent; a message
        // that opens with anything else keeps its indentation.
        assert_eq!(
            prompt(submission("\n  /alpha this", &claude, &[], PromptMode::Send).unwrap()),
            "/alpha this"
        );
        assert_eq!(
            prompt(submission("    let x = 1;\n/beta", &claude, &[], PromptMode::Send).unwrap()),
            "    let x = 1;\n/beta"
        );
        assert!(matches!(
            submission("/clear", &claude, &[], PromptMode::Send),
            Ok(Command::Slash { command }) if command == "/clear"
        ));
        assert!(submission("/clear", &claude, &attachment, PromptMode::Send).is_err());
        // Steering carries words to a running turn, never a command.
        assert_eq!(
            prompt(submission("/clear", &claude, &[], PromptMode::Steer).unwrap()),
            "/clear"
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
