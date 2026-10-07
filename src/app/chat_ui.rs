use super::*;
use crate::account::{Account, AccountId};
use crate::agent::{Activity, Provider};
use crate::app::account_ui::ChatAccounts;
use crate::chat::client::{ClientHandle, Wake};
use crate::chat::{self, Attachment, Command, ItemContent, PromptMode, Snapshot};
use crate::file_drop;
use base64::Engine as _;
use cosmic::widget::{markdown, text_editor};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

mod clipboard_image;
mod completion;
mod fold;
mod images;
mod layout;
mod markdown_source;
mod permissions;
mod prose;
mod retained;
mod scroll;
mod selectable_prose;
mod selection;
mod step;
mod tool_text;
mod turn;
mod view;

pub(super) fn prune_pasted_images() {
    clipboard_image::prune_cache();
}

fn action(action: Action) -> Message {
    Message::ChatUi(action)
}

/// One icon control on the chat surface.
///
/// Shared by the transcript and the composer so a control means the same
/// thing, and is the same size, wherever it appears.
fn icon<'a>(
    name: Icon,
    label: impl Into<Cow<'a, str>>,
    enabled: bool,
    message: Action,
) -> Element<'a, Message> {
    crate::ui_controls::icon_button(name, label)
        .on_press_maybe(enabled.then_some(action(message)))
        .into()
}

/// A message's own control, at caption scale.
///
/// An answer's footer sits directly under prose, so its controls are drawn
/// smaller than the composer's: at the primary size a single copy button
/// looked like a toolbar bolted to every answer.
fn small_icon<'a>(
    name: Icon,
    label: impl Into<Cow<'a, str>>,
    message: Action,
) -> Element<'a, Message> {
    crate::ui_controls::icon_button(name, label)
        .compact()
        .on_press(action(message))
        // Muted at rest: these sit at the foot of every answer, and drawn in the
        // text color they competed with the answer for attention.
        .tone(crate::design::Tone::Themed(crate::design::muted_icon_color))
        .into()
}

/// One message waiting above the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueEntry {
    /// Held by the provider, under the id it queued the message with.
    Provider(String),
    /// Held here for a provider that keeps no queue of its own.
    Local(u64),
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
    /// Points a draft at a model of any provider, or at a provider's default
    /// model where none is named.
    DraftModel(Target, Provider, Option<String>),
    /// Points a draft at another account of its provider.
    ChooseAccount(Target, AccountId),
    RecoverAccount(CardId, AccountId),
    /// Installed skills read without starting the draft's provider.
    DraftSkills(Target, chat::Launch, Vec<chat::Skill>),
    /// Opens or closes the account choice.
    AccountPicker(Target, bool),
    /// Takes the next step of sending a draft's first message.
    DispatchQueued(Target),
    /// Takes back a draft's first message before its conversation has it,
    /// so the draft can be edited or moved again.
    CancelQueued(Target),
    Switch(CardId),
    Ready(Target, Ready),
    AccountCommitted(Target, Ready, Result<(), String>),
    UpgradeCommitted(Target, Ready, Result<(), String>),
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
    Paste(Target),
    PastedImage(Target, Option<clipboard_image::Image>),
    PastedText(Target, Option<String>),
    /// A clipboard image the chat cannot take, offered without any text to
    /// paste in its place.
    PasteRefused(Target, &'static str),
    ImageSaved(Target, Result<PathBuf, String>),
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
    OpenDocument(Target, String),
    OpenImage(Target, String),
    CloseImage,
    ImageNext(bool),
    ImageFit,
    ImageExternal,
    CopyLiteral(String),
    Latest(Target),
    /// Takes a waiting message back into the composer to be revised: the one
    /// named, or the newest that can be taken back.
    TakeBackQueued(Target, Option<QueueEntry>),
    RemoveQueued(Target, QueueEntry),
    BranchPicker(Target, bool),
    Files(Target, PathBuf, Arc<Vec<String>>),
    Find(Target, String),
    FindNext(Target, bool),
    CloseFind(Target),
    FindFocused(Target, bool),
    Tick,
    ClearCopied(Target, Instant),
    DropHover(Target, bool),
    Attach(Target),
    Attached(Target, Result<Option<Vec<PathBuf>>, String>),
    /// Files or text dropped on the conversation.
    Dropped(Target, file_drop::Payload),
    RemoveAttachment(Target, usize),
    Model(Target, String),
    Reasoning(Target, String, String),
    PermissionMode(Target, String),
    PermissionRetry(Target, String),
    PermissionDismiss(Target, String),
    PermissionPage(Target, permissions::Page),
    PermissionRuleText(Target, String),
    PermissionBehavior(Target, chat::PermissionBehavior),
    PermissionEnvironmentText(Target, String),
    PermissionSetting(Target, chat::PermissionSettingsUpdate),
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
    RetainedCloseFinished(Target, chat::Session, Result<(), String>),
    /// Where the transcript's viewport has moved to, which is what asks for
    /// another page of history.
    Scrolled(Target, Option<String>, view::Scroll, bool),
    /// Opens or closes the model search.
    ModelPicker(Target, bool),
    ReasoningPicker(Target, bool),
    PermissionPicker(Target, bool),
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
                | Self::AccountCommitted(..)
                | Self::UpgradeCommitted(..)
                | Self::Wake(..)
                | Self::UpdatesAcknowledged(..)
                | Self::CommandFinished(..)
                | Self::Attached(..)
                | Self::PastedImage(..)
                | Self::PastedText(..)
                | Self::PasteRefused(..)
                | Self::ImageSaved(..)
                | Self::HandoffFinished(..)
                | Self::ReturnedLocally(..)
                | Self::CloseFinished(..)
                | Self::RetainedCloseFinished(..)
                | Self::Retired(..)
                | Self::DispatchQueued(..)
                | Self::ClearCopied(..)
                | Self::Files(..)
                | Self::Tick
                | Self::AccountPicker(_, false)
                | Self::CloseImage
                | Self::ImageNext(_)
                | Self::ImageFit
                | Self::ImageExternal
        )
    }

    /// The card whose provider session this action starts afresh.
    fn starts_session(&self) -> Option<CardId> {
        match self {
            Self::Reconnect(target) => Some(target.card),
            Self::Switch(card) | Self::StartFresh(card) | Self::RecoverAccount(card, _) => {
                Some(*card)
            }
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
            | Self::Paste(target)
            | Self::Dropped(target, _)
            | Self::RemoveAttachment(target, _)
            | Self::TakeBackQueued(target, _)
            | Self::RemoveQueued(target, _)
            | Self::Model(target, _)
            | Self::Reasoning(target, _, _)
            | Self::PermissionMode(target, _)
            | Self::PermissionRetry(target, _)
            | Self::PermissionDismiss(target, _)
            | Self::PermissionPage(target, _)
            | Self::PermissionRuleText(target, _)
            | Self::PermissionBehavior(target, _)
            | Self::PermissionEnvironmentText(target, _)
            | Self::PermissionSetting(target, _)
            | Self::Field(target, _, _, _)
            | Self::Choice(target, _, _, _)
            | Self::Answer(target, _, _)
            | Self::Complete(target, _, _)
            | Self::Scrolled(target, _, _, _)
            | Self::ModelPicker(target, _)
            | Self::ModelQuery(target, _) => Some(*target),
            _ => None,
        }
    }
}

struct AccountRecovery {
    source: chat::Session,
    committed: bool,
    /// Only a composer submission that has never reached a provider can be
    /// sent after handoff. An exhausted/failed provider turn is never replayed.
    submit: Option<(u64, PromptMode)>,
}

struct Attempt {
    target: Target,
    session: chat::Session,
    recovery: Option<AccountRecovery>,
    upgrade_committed: bool,
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

/// One operation owns an existing card at a time. A card absent from this map
/// is ready for input; its runtime and durable descriptor describe its content.
/// Detached actors have their own target-keyed retirement ledger, since an old
/// actor can still be shutting down while this card starts another one.
enum Phase {
    Starting(Attempt),
    Returning {
        target: Target,
        local: Option<LocalReturn>,
        close_requested: bool,
    },
    Closing {
        target: Target,
        restart: Option<chat::Session>,
    },
}

impl Phase {
    fn target(&self) -> Target {
        match self {
            Self::Starting(attempt) => attempt.target,
            Self::Returning { target, .. } | Self::Closing { target, .. } => *target,
        }
    }
}

pub(super) struct State {
    pub(super) image_viewer: Option<images::Gallery>,
    files: HashMap<PathBuf, Arc<Vec<String>>>,
    sender: async_channel::Sender<(Target, Wake)>,
    receiver: async_channel::Receiver<(Target, Wake)>,
    generation: u64,
    phases: HashMap<CardId, Phase>,
    /// Starts and restart closes still in flight for a card that has since
    /// been closed, with the conversation each one concerns.
    ///
    /// The card is gone as soon as the user asks, but whatever the request
    /// leaves in the daemon is only known when it answers, so the answer is
    /// what retires it.
    abandoned: HashMap<Target, chat::Session>,
    /// Conversations being given up in the background after their card left.
    retiring: HashSet<Target>,
    /// Unsent composer text belonging to a card whose chat runtime has been
    /// dropped.
    ///
    /// Returning a chat card to its terminal view and back used to discard
    /// whatever was typed, which is why leaving the surface at all had to be
    /// refused. Holding the draft here makes the round trip lossless, and the
    /// workspace snapshot saves this stash across a restart.
    drafts: HashMap<CardId, String>,
    /// Automatically opened placeholders, discarded when their scope is left
    /// until the reader puts work into them or starts their conversation.
    default_chats: HashSet<CardId>,
    parked: HashMap<CardId, Box<Runtime>>,
    /// The cards whose saved conversation cannot be resumed by trying again.
    ///
    /// A card in this set has no chat runtime to hold the offer, because its
    /// start never produced one, so the failed card itself has to carry it.
    fresh_offers: HashSet<CardId>,
    removed_accounts: HashSet<CardId>,
    /// The model last chosen for each provider, and its effort, which a new
    /// conversation of that provider starts on.
    last_models: HashMap<Provider, (String, Option<String>)>,
}
impl Default for State {
    fn default() -> Self {
        let (sender, receiver) = async_channel::unbounded();
        Self {
            image_viewer: None,
            files: HashMap::new(),
            sender,
            receiver,
            generation: 0,
            phases: HashMap::new(),
            abandoned: HashMap::new(),
            retiring: HashSet::new(),
            drafts: HashMap::new(),
            default_chats: HashSet::new(),
            parked: HashMap::new(),
            fresh_offers: HashSet::new(),
            removed_accounts: HashSet::new(),
            last_models: HashMap::new(),
        }
    }
}
impl State {
    pub(super) fn with_drafts(drafts: HashMap<CardId, String>) -> Self {
        Self {
            drafts,
            ..Self::default()
        }
    }
}

impl State {
    pub(super) fn transition_pending(&self) -> bool {
        !self.phases.is_empty() || !self.abandoned.is_empty() || !self.retiring.is_empty()
    }

    #[cfg(test)]
    fn attempt(&self, card: CardId) -> Option<&Attempt> {
        match self.phases.get(&card) {
            Some(Phase::Starting(attempt)) => Some(attempt),
            _ => None,
        }
    }

    /// Completion messages may consume only the operation they were issued
    /// for, never a newer operation now occupying the same card.
    fn finish(&mut self, target: Target, expected: impl FnOnce(&Phase) -> bool) -> Option<Phase> {
        let phase = self.phases.get(&target.card)?;
        if phase.target() != target || !expected(phase) {
            return None;
        }
        self.phases.remove(&target.card)
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
            ItemContent::Delivery {
                kind,
                from,
                sender,
                summary,
                body,
            } => Self {
                shape: match kind {
                    chat::DeliveryKind::Report => 18,
                    chat::DeliveryKind::Message => 19,
                    chat::DeliveryKind::Notice => 20,
                },
                body: body.len(),
                digest: [from, sender, summary, body]
                    .into_iter()
                    .fold(0, |digest, field| tail_digest(digest, field)),
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

/// Remember reader choices across bounded history pages without retaining transcript bodies.
fn reconcile_disclosure(opened: &mut HashMap<String, bool>, turns: &[turn::Turn]) {
    if opened.len() > 4096 {
        let keys: HashSet<&str> = turns.iter().map(|turn| turn.key.as_str()).collect();
        opened.retain(|key, _| keys.contains(key.as_str()));
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
        (
            CopyPart::Text,
            ItemContent::Delivery {
                kind,
                from,
                sender,
                summary,
                body,
            },
        ) => joined(&[&chat::delivery_heading(*kind, from, sender, summary), body]),
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
        (CopyPart::Change(_), _) | (CopyPart::Input | CopyPart::Output, _) => None,
    }
}

fn contains_text(content: &ItemContent, query: &str) -> bool {
    match content {
        ItemContent::Text { text, .. } => text.contains(query),
        ItemContent::Command {
            command, output, ..
        } => command.contains(query) || output.contains(query),
        ItemContent::Changes { changes, .. } => {
            changes.iter().any(|change| change.diff.contains(query))
        }
        ItemContent::Tool { input, output, .. } => input.contains(query) || output.contains(query),
        ItemContent::WebSearch {
            query: search,
            results,
        } => {
            search.contains(query)
                || results
                    .iter()
                    .any(|result| result.title.contains(query) || result.url.contains(query))
        }
        ItemContent::Image { path, description } => {
            path.contains(query) || description.contains(query)
        }
        ItemContent::Delivery {
            from,
            sender,
            summary,
            body,
            ..
        } => {
            from.contains(query)
                || sender.contains(query)
                || summary.contains(query)
                || body.contains(query)
        }
    }
}

/// One transcript item's presentation, retained across refreshes.
///
/// The body is never copied in: what an item looked like when it was parsed is
/// recorded as a `ChangeToken`, and the authoritative text stays in the
/// snapshot the runtime already holds.
struct CachedItem {
    stamp: Option<(u64, String, String)>,
    preview: tool_text::Preview,
    tool_input: Option<tool_text::ToolText>,
    tool_output: Option<tool_text::ToolText>,
    patches: Vec<tool_text::ToolText>,
    token: ChangeToken,
    /// The body as the Markdown parser is given it, resumed on each append.
    source: markdown_source::Source,
    markdown: markdown::Content,
    /// Each fenced block the Markdown parsed, ready to draw, keyed by the
    /// block's language and text: what a frame looks a block up by. The
    /// palette is the runtime's, applied when the block is highlighted, so a
    /// change of syntax theme rehighlights rather than restyles.
    blocks: HashMap<u64, prose::CodeBlock>,
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
            source: markdown_source::Source::default(),
            markdown: markdown::Content::new(),
            blocks: HashMap::new(),
            detail: None,
            stamp: None,
            image: None,
        };
        match content {
            ItemContent::Text { text, .. } => cached.extend(text, theme),
            // A notice's body is a log line or a raw result, drawn verbatim.
            ItemContent::Delivery { kind, body, .. } if *kind != chat::DeliveryKind::Notice => {
                cached.extend(body, theme)
            }
            ItemContent::Delivery { .. } => {}
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
                    path if Path::new(path).is_absolute() => images::file_handle(path),
                    path if path.len() <= chat::MAX_INLINE_IMAGE_URL_BYTES
                        && path.starts_with("data:image/") =>
                    {
                        path.split_once(";base64,")
                            .and_then(|(_, encoded)| {
                                base64::engine::general_purpose::STANDARD
                                    .decode(encoded)
                                    .ok()
                            })
                            .and_then(images::bytes_handle)
                    }
                    _ => None,
                };
            }
        }
        cached
    }
    /// Parses the part of `body` not yet seen, `body` being what was parsed
    /// before with more appended.
    ///
    /// `markdown::Content` re-parses only its trailing block, and the rewrite
    /// the parser is given resumes from the last whole line, so a streamed
    /// answer costs its new bytes per frame instead of its whole length. The
    /// exception is an unfinished line that turned out to be something else
    /// than it was first handed over as, which is parsed again from the top.
    fn extend(&mut self, body: &str, theme: crate::settings::EditorSyntaxTheme) {
        match self.source.advance(body) {
            markdown_source::Step::Append(text) => self.markdown.push_str(&text),
            markdown_source::Step::Restart(text) => {
                self.markdown = markdown::Content::parse(&text);
            }
        }
        self.highlight(theme);
    }

    /// A fence larger than this is drawn plain: a syntect pass over it would
    /// stall the frame it is cached on, and a block that size is a file the
    /// agent pasted rather than a snippet it is showing.
    const HIGHLIGHT_LIMIT: usize = 64 * 1024;

    /// Prepares every fenced block the Markdown now holds, keeping the blocks
    /// that are unchanged since the last pass. A streamed block changes on
    /// every refresh until its fence closes, so it is the one block a
    /// refresh usually prepares again.
    fn highlight(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        let mut previous = std::mem::take(&mut self.blocks);
        let mut blocks = HashMap::new();
        for_each_fence(self.markdown.items(), &mut |language, code| {
            let key = fence_key(language.unwrap_or_default(), code);
            let block = previous.remove(&key).unwrap_or_else(|| {
                prose::CodeBlock::new(language.and_then(prose::fence_language), code, theme)
            });
            blocks.insert(key, block);
        });
        self.blocks = blocks;
    }

    /// Discards the highlighting and does it again in `theme`.
    fn rehighlight(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        self.preview.rehighlight(theme);
        self.blocks.clear();
        self.highlight(theme);
    }

    /// The fence whose info string is `language` and whose text is `code`,
    /// where the cache holds it.
    fn block(&self, language: Option<&str>, code: &str) -> Option<&prose::CodeBlock> {
        self.blocks
            .get(&fence_key(language.unwrap_or_default(), code))
    }
}

/// Visits every fenced block among `items`, in the order the Markdown draws
/// them, descending into lists and quotes the way the drawing does.
fn for_each_fence<'a>(
    items: impl IntoIterator<Item = &'a markdown::Item>,
    visit: &mut impl FnMut(Option<&'a str>, &'a str),
) {
    for item in items {
        match item {
            markdown::Item::CodeBlock { language, code, .. } => visit(language.as_deref(), code),
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

/// Provider snapshots also cover restored chats and prompts sent through the
/// paired CLI. Never infer a conversation from another process in the same cwd.
fn snapshot_channel_name(
    snapshot: &Snapshot,
) -> Option<(String, crate::model::ChannelTitleSource)> {
    snapshot
        .title
        .as_deref()
        .and_then(crate::workspace_model::channel_title)
        .map(|title| (title, crate::model::ChannelTitleSource::Provider))
        .or_else(|| {
            snapshot
                .items
                .iter()
                .find_map(|item| match (&item.role, &item.content) {
                    (chat::Role::User, ItemContent::Text { text, .. }) => {
                        crate::workspace_model::channel_title_from_prompt(text)
                            .map(|title| (title, crate::model::ChannelTitleSource::Prompt))
                    }
                    _ => None,
                })
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryNavigation {
    EarlierEdge,
    EarlierExplicit,
    LatestEdge,
    Latest,
}

struct PendingCommand {
    followup: bool,
    /// The provider's queued message this command asks back, which stops
    /// being waited for if the request never reaches the provider.
    take_back: Option<String>,
    id: u64,
    draft_revision: Option<u64>,
    request: Option<String>,
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
    /// The reader's wording, restored if startup cannot take the message.
    text: String,
    /// Resolve draft skill choices before the provider replaces the draft
    /// catalog; two provider skills may share a name after startup.
    resolved_text: String,
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
    fn submission_text<'a>(&'a self, snapshot: &Snapshot) -> &'a str {
        // A provider command learned during startup takes precedence over a
        // draft skill with the same slash name.
        if slash_invocation(&self.text, snapshot).is_some() {
            &self.text
        } else {
            &self.resolved_text
        }
    }

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

/// A message held for the turn after the running one, by a chat whose
/// provider keeps no queue of its own.
///
/// Each stays its own entry so one can be revised or dropped without the
/// others; they are sent together, as one prompt, when the turn ends.
struct Followup {
    id: u64,
    text: String,
    attachments: Vec<Attachment>,
}

/// A provider's queued message on its way back to the composer.
///
/// Removing a message races the agent starting on it, and the provider alone
/// knows which happened, so the text returns only on its answer.
struct Recall {
    message: chat::QueuedMessage,
    /// The answers about this message the provider had already given.
    answered: usize,
}

/// Which step of sending a queued first message a command is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueuedStep {
    Model,
    Prompt,
}

pub(super) struct Runtime {
    find: Option<String>,
    find_focused: bool,
    find_matches: Vec<String>,
    find_index: usize,
    copied: Option<(String, Instant)>,
    drop_hover: bool,
    files: Option<Arc<Vec<String>>>,
    pub(super) target: Target,
    pub(super) session: chat::Session,
    connection: Connection,
    /// The first message of a draft, while its conversation starts.
    queued: Option<Queued>,
    /// Messages held here for the next turn, oldest first.
    followups: Vec<Followup>,
    next_followup: u64,
    /// Provider queue entries asked back, until the provider says whether it
    /// let go of each.
    recalling: Vec<Recall>,
    /// Provider queue entries already back in the composer, hidden until the
    /// provider's own listing drops them.
    taken_back: Vec<String>,
    /// How the draft was queued before it was taken back, which is how
    /// sending it again queues it unless the reader chooses otherwise.
    resend: Option<PromptMode>,
    branch_picker: bool,
    fork_requested: bool,
    fork_seen: Option<String>,
    snapshot: Arc<Snapshot>,
    source_clock: Option<chat::client::SourceClock>,
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
    /// Explicit reader choices override the configured default and survive
    /// history-page eviction within a bounded disclosure budget.
    opened: HashMap<String, bool>,
    pub(super) tool_groups_expanded: bool,
    composer: text_editor::Content<cosmic::Renderer>,
    revision: u64,
    attachments: Vec<Attachment>,
    /// Clipboard reads and image writes still destined for this draft.
    pastes_pending: usize,
    image_pastes_saving: usize,
    submit_after_paste: Option<PromptMode>,
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
    permission_picker: bool,
    permissions_form: permissions::Form,
    /// Whether the account choice is open.
    account_picker: bool,
    picker_focus: usize,
    picker_keyboard: bool,
    /// Messages the provider accepted, oldest first, for Up and Down in the
    /// composer to walk through the way a shell does.
    history: Vec<String>,
    /// Which entry the composer is showing while the reader walks the
    /// history; `None` while they are writing.
    history_cursor: Option<usize>,
    /// The draft that was in the composer when the walk began, restored when
    /// the walk goes past the newest message.
    history_stash: Option<text_editor::Content<cosmic::Renderer>>,
    /// The most recent transcript viewport, used for history pagination.
    scroll: view::Scroll,
    subagent_scroll: HashMap<String, view::Scroll>,
    page_request: Option<(view::Page, u64)>,
    deferred_page: Option<HistoryNavigation>,
    unseen_activity: bool,
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
pub(super) const PERMISSIONS_COMMAND: &str = "permissions";

fn permissions_available(snapshot: &Snapshot) -> bool {
    !snapshot.capabilities.permission_modes.is_empty()
        || !snapshot.permission_denials.is_empty()
        || snapshot
            .requests
            .iter()
            .any(|request| request.fields.is_empty())
}

/// The name the composer answers itself, by opening the conversation picker,
/// whatever command of that name the provider lists.
pub(super) const RESUME_COMMAND: &str = "resume";

/// What a draft reading `/resume [query]` searches for, or `None` for any
/// other draft.
fn resume_invocation(draft: &str) -> Option<String> {
    let rest = draft.trim().strip_prefix(completion::COMMAND_SIGIL)?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    rest[..end]
        .eq_ignore_ascii_case(RESUME_COMMAND)
        .then(|| rest[end..].trim().to_owned())
}

/// What a draft beginning with `/` asks for, when it is not a message.
enum SlashInvocation {
    Permissions,
    PermissionsArguments,
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
    if name.eq_ignore_ascii_case(PERMISSIONS_COMMAND) && permissions_available(snapshot) {
        return Some(if rest.eq_ignore_ascii_case(PERMISSIONS_COMMAND) {
            SlashInvocation::Permissions
        } else {
            SlashInvocation::PermissionsArguments
        });
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
        (_, Some(SlashInvocation::Permissions)) => {
            Err("Permissions are managed in the native permissions panel.")
        }
        (_, Some(SlashInvocation::PermissionsArguments)) => {
            Err("Use /permissions without arguments to open the permissions panel.")
        }
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
        let rows = if token.sigil == '@' {
            completion::files(
                self.files.as_deref().map_or(&[][..], Vec::as_slice),
                &token.query,
            )
        } else {
            completion::candidates(&self.snapshot, &token)
        };
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
        self.history_stash = None;
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

    /// Puts a message that was waiting back in the composer, ahead of
    /// whatever was typed after it, with the files it was queued with.
    fn take_back(&mut self, text: &str, attachments: Vec<Attachment>, queued: PromptMode) {
        let text = text.trim_end();
        let draft = self.unsent_text();
        let combined = match (text.is_empty(), draft.trim().is_empty()) {
            (_, true) => text.to_owned(),
            (true, false) => draft,
            (false, false) => format!("{text}\n{draft}"),
        };
        let mut composer = text_editor::Content::with_text(&combined);
        // The caret lands where revising the message continues from.
        let last = text.lines().next_back().unwrap_or_default();
        composer.move_to(text_editor::Cursor {
            position: text_editor::Position {
                line: text.lines().count().saturating_sub(1),
                // A column counts bytes of its line.
                column: last.len(),
            },
            selection: None,
        });
        self.replace_draft(composer);
        self.resend = Some(queued);
        // The draft a walk through sent messages set aside is in `combined`.
        self.history_cursor = None;
        self.history_stash = None;
        let later = std::mem::replace(&mut self.attachments, attachments);
        self.attach(later.into_iter().map(|attachment| attachment.path));
    }

    /// Returns every held follow-up to the composer, in the order written.
    fn restore_followups(&mut self) {
        if self.followups.is_empty() {
            return;
        }
        let (text, attachments) = self.combined_followups();
        self.followups.clear();
        self.take_back(&text, attachments, PromptMode::Queue);
        let _ = self.present();
    }

    /// The held follow-ups as the one prompt they are sent as: their text in
    /// order, and each file once.
    fn combined_followups(&self) -> (String, Vec<Attachment>) {
        let mut text = String::new();
        let mut attachments: Vec<Attachment> = Vec::new();
        for followup in &self.followups {
            if !followup.text.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(followup.text.trim_end());
            }
            for attachment in &followup.attachments {
                if !attachments.iter().any(|old| old.path == attachment.path) {
                    attachments.push(attachment.clone());
                }
            }
        }
        (text, attachments)
    }

    /// Whether a command is carrying the held follow-ups to the provider.
    fn followups_sending(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.followup)
    }

    /// Whether `message`, held by the provider, can return to the composer.
    fn provider_take_back(&self, message: &chat::QueuedMessage) -> bool {
        self.snapshot.capabilities.queue_remove
            && message.editable
            && !self.taken_back.contains(&message.id)
            && !self.is_recalling(&message.id)
    }

    fn is_recalling(&self, id: &str) -> bool {
        self.recalling.iter().any(|recall| recall.message.id == id)
    }

    /// Settles each message asked back against what the provider now says.
    fn settle_recalls(&mut self, snapshot: &Snapshot) {
        for recall in std::mem::take(&mut self.recalling) {
            let id = &recall.message.id;
            let answer = snapshot
                .queue_removals
                .iter()
                .filter(|removal| &removal.id == id)
                .skip(recall.answered)
                .last();
            match answer {
                Some(removal) if removal.removed => {
                    let queued = match recall.message.delivery {
                        chat::QueueDelivery::NextStep => PromptMode::Steer,
                        chat::QueueDelivery::NextTurn => PromptMode::Queue,
                    };
                    self.take_back(&recall.message.text, recall.message.attachments, queued);
                    self.taken_back.push(recall.message.id);
                }
                // The provider's own refusal is reported as its error.
                Some(_) => {}
                None if snapshot
                    .queued_messages
                    .iter()
                    .any(|queued| &queued.id == id) =>
                {
                    self.recalling.push(recall);
                }
                None => {
                    self.error = Some(
                        "The agent had already started on that message, so it was not taken back."
                            .into(),
                    );
                }
            }
        }
    }

    /// The waiting message the keyboard takes back: the newest one that can
    /// be. Held follow-ups are sent after everything the provider holds.
    fn newest_take_back(&self) -> Option<QueueEntry> {
        if self.followups_sending() {
            return None;
        }
        self.followups
            .last()
            .map(|followup| QueueEntry::Local(followup.id))
            .or_else(|| {
                self.snapshot
                    .queued_messages
                    .iter()
                    .rev()
                    .find(|message| self.provider_take_back(message))
                    .map(|message| QueueEntry::Provider(message.id.clone()))
            })
    }

    /// Records a message the provider accepted. The same message sent twice
    /// in a row is one entry, as a shell keeps it, and the walk restarts
    /// from the newest.
    fn remember(&mut self, sent: String) {
        if self.history.last() != Some(&sent) {
            self.history.push(sent);
        }
        self.history_cursor = None;
        self.history_stash = None;
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
                self.history_stash = Some(std::mem::take(&mut self.composer));
                Some(newest)
            }
            (None, false) => return,
            (Some(0), true) => return,
            (Some(index), true) => Some(index - 1),
            (Some(index), false) if index + 1 < self.history.len() => Some(index + 1),
            (Some(_), false) => None,
        };
        let composer = match next {
            Some(index) => {
                text_editor::Content::with_text(self.history[index].trim_end_matches('\n'))
            }
            None => self.history_stash.take().unwrap_or_default(),
        };
        self.history_cursor = next;
        self.replace_draft(composer);
    }

    /// Recalled prompts are complete history entries, including multiline
    /// ones. Both arrows keep walking that history until the draft returns.
    fn history_navigation(&self) -> (bool, bool) {
        let browsing = self.history_cursor.is_some();
        let older =
            !self.history.is_empty() && (browsing || self.composer.cursor().position.line == 0);
        (older, browsing)
    }

    /// Browsing sent prompts does not replace the unsent draft on disk.
    pub(super) fn unsent_text(&self) -> String {
        self.history_stash
            .as_ref()
            .filter(|_| self.history_cursor.is_some())
            .unwrap_or(&self.composer)
            .text()
    }

    /// Puts a whole new draft in the composer. Whatever the completion list
    /// was doing belonged to the old one.
    fn replace_draft(&mut self, composer: text_editor::Content<cosmic::Renderer>) {
        self.composer = composer;
        self.completion = completion::State::default();
        self.revision = self.revision.wrapping_add(1);
        self.resend = None;
    }

    fn new(
        target: Target,
        session: chat::Session,
        connection: Connection,
        visible: bool,
        syntax_theme: crate::settings::EditorSyntaxTheme,
    ) -> Self {
        let permission_mode =
            draft_permission_mode(&session.launch.recipe, session.launch.provider);
        let mut runtime = Self {
            target,
            session,
            snapshot: Arc::new(Snapshot {
                permission_mode,
                ..Snapshot::default()
            }),
            source_clock: None,
            connection,
            queued: None,
            followups: Vec::new(),
            next_followup: 0,
            recalling: Vec::new(),
            taken_back: Vec::new(),
            resend: None,
            branch_picker: false,
            fork_requested: false,
            fork_seen: None,
            files: None,
            find: None,
            find_focused: false,
            find_matches: Vec::new(),
            find_index: 0,
            copied: None,
            drop_hover: false,
            cache: HashMap::new(),
            syntax_theme,
            turns: Vec::new(),
            selected_subagent: None,
            asked_after: HashSet::new(),
            asked: HashSet::new(),
            opened: HashMap::new(),
            tool_groups_expanded: false,
            composer: text_editor::Content::new(),
            revision: 0,
            attachments: Vec::new(),
            pastes_pending: 0,
            image_pastes_saving: 0,
            submit_after_paste: None,
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
            permission_picker: false,
            permissions_form: permissions::Form::default(),
            account_picker: false,
            picker_focus: 0,
            picker_keyboard: false,
            history: Vec::new(),
            history_cursor: None,
            history_stash: None,
            scroll: view::Scroll::default(),
            subagent_scroll: HashMap::new(),
            page_request: None,
            deferred_page: None,
            unseen_activity: false,
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
        self.subagent_scroll.clear();
        self.page_request = None;
        self.deferred_page = None;
        self.scroll = view::Scroll::default();
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
    fn source_now_ms(&self) -> Option<u64> {
        self.source_clock
            .map(chat::client::SourceClock::now)
            .or_else(chat::now_ms)
    }

    fn active_scroll(&self) -> view::Scroll {
        match self.selected_subagent.as_ref() {
            Some(id) => self.subagent_scroll.get(id).copied().unwrap_or_default(),
            None => self.scroll,
        }
    }
    fn refresh(&mut self) {
        self.refresh_session();
        if !self.visible.load(Ordering::Relaxed) {
            return;
        }
        let Some(handle) = self.handle() else {
            return;
        };
        let (mut snapshot, source_clock) = handle.snapshot_with_clock();
        if self.page_request.is_some_and(|(_, generation)| {
            snapshot.generation > generation && !snapshot.history_loading
        }) && self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.page.is_none())
        {
            self.page_request = None;
            self.earlier_asked = None;
            self.later_asked = None;
        }
        if self.snapshot.generation == snapshot.generation && Arc::ptr_eq(&self.snapshot, &snapshot)
        {
            return;
        }
        self.subagent_scroll
            .retain(|id, _| snapshot.subagents.iter().any(|agent| &agent.id == id));
        self.settle_recalls(&snapshot);
        self.taken_back.retain(|id| {
            snapshot
                .queued_messages
                .iter()
                .any(|message| &message.id == id)
        });
        if snapshot.permission_mode.is_none() && snapshot.session_id.is_none() {
            Arc::make_mut(&mut snapshot).permission_mode =
                self.snapshot.permission_mode.clone().or_else(|| {
                    draft_permission_mode(&self.session.launch.recipe, self.session.launch.provider)
                });
        }
        if !self.scroll.at_bottom()
            && !snapshot.has_later
            && self.page_request.is_none()
            && !snapshot.history_loading
            && snapshot.items.last().is_some_and(|item| {
                self.cache
                    .get(&item.id)
                    .is_none_or(|cached| cached.token != ChangeToken::of(&item.content))
            })
        {
            self.unseen_activity = true;
        }
        // Restored and paged conversations carry their own prompt history.
        // Preserve a recalled index while the reader is walking it.
        if self.history_cursor.is_none() {
            self.history = snapshot
                .items
                .iter()
                .filter_map(|item| match (&item.role, &item.content) {
                    (chat::Role::User, ItemContent::Text { text, .. })
                        if !text.trim().is_empty() =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .collect();
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
        // Keep disclosure choices across page eviction, with a bounded budget.
        // Question anchors only describe the currently loaded window.
        if self.expanded.len() > 4096 {
            self.expanded.retain(|id| live_ids.contains(id.as_str()));
        }
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
                    cached.extend(body, self.syntax_theme);
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
        for item in snapshot.items.iter() {
            if let Some(cached) = self.cache.get_mut(&item.id)
                && let Some(at) = item.at
                && cached
                    .stamp
                    .as_ref()
                    .is_none_or(|(cached_at, _, _)| *cached_at != at)
            {
                if let Some(time) = chat::local_time(i64::try_from(at / 1000).unwrap_or(i64::MAX)) {
                    let full = format!(
                        "{:04}-{:02}-{:02} {}",
                        time.year + 1900,
                        time.month + 1,
                        time.day,
                        time.clock()
                    );
                    let today =
                        chat::now_ms().and_then(|now| chat::local_time((now / 1000) as i64));
                    let label = if today.is_some_and(|today| {
                        today.year == time.year && today.day_of_year == time.day_of_year
                    }) {
                        time.clock()
                    } else {
                        full.clone()
                    };
                    cached.stamp = Some((at, label, full));
                }
            }
        }
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
        self.source_clock = source_clock;
        self.turns = turn::group(&snapshot.items, &self.asked_after);
        reconcile_disclosure(&mut self.opened, &self.turns);
        self.snapshot = snapshot;
        self.refresh_find();
    }

    fn refresh_find(&mut self) {
        let previous = self.find_matches.get(self.find_index).cloned();
        let query = self.find.as_deref().unwrap_or_default();
        self.find_matches = self
            .turns
            .iter()
            .filter(|turn| {
                !query.is_empty()
                    && turn
                        .prompt
                        .iter()
                        .copied()
                        .chain(
                            turn.steps
                                .iter()
                                .chain(&turn.prose)
                                .map(|placed| placed.index),
                        )
                        .filter_map(|index| self.snapshot.items.get(index))
                        .any(|item| contains_text(&item.content, query))
            })
            .map(|turn| turn.key.clone())
            .collect();
        self.find_index = previous
            .and_then(|key| {
                self.find_matches
                    .iter()
                    .position(|candidate| candidate == &key)
            })
            .unwrap_or_else(|| {
                self.find_index
                    .min(self.find_matches.len().saturating_sub(1))
            });
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
        let visible = self.visible.load(Ordering::Relaxed)
            || self.queued.is_some()
            || !self.followups.is_empty();
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
    /// Ownership can become durable after the initial attachment, including
    /// while the pane is hidden. Preserve that confirmed capability before
    /// any reconnect or saved-descriptor update.
    fn refresh_session(&mut self) {
        let Some(handle) = self.handle() else { return };
        let observed = handle.session();
        if observed.launch.session_id.is_some() {
            self.session.launch.session_id = observed.launch.session_id;
        }
        if matches!(
            observed.native_continuity,
            chat::NativeContinuity::Durable { .. }
        ) {
            self.session.native_continuity = observed.native_continuity;
        }
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
        } else if !self.followups.is_empty() {
            Some("Send or take back the queued follow-up before leaving the chat.")
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
            if self.queued.is_some() {
                Some(QUEUED_BLOCKER)
            } else if !self.followups.is_empty() {
                Some("Send or take back the queued follow-up before leaving the chat.")
            } else {
                None
            }
        } else {
            self.transition_blocker()
        }
    }
    /// Why this chat cannot survive the process restarting.
    ///
    /// Composer text is included in the workspace snapshot flushed before an
    /// update. Attachments, answers and in-flight work still need a live UI.
    pub(super) fn restart_blocker(&self) -> Option<&'static str> {
        self.transition_blocker()
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
        native_continuity: Default::default(),
        owned_codex_listener: session.owned_codex_listener.clone(),
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

/// The root a terminal replacing this chat names as its recovery origin: the
/// one the chat's own terminal descended from, or the chat itself.
fn succession_root(session: &chat::Session) -> TerminalId {
    session
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.recovery_origin)
        .unwrap_or(session.id)
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
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
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
/// daemon's own return would start it. Before a conversation has an identity,
/// open the selected provider's interactive CLI with its account and policy.
fn local_return_launch(session: &chat::Session) -> Result<LaunchRecipe, String> {
    if session.launch.session_id.is_none() {
        let mut fresh = session.clone();
        fresh.reset_owned_codex_listener();
        let mut recipe = fresh.launch.recipe;
        if session.launch.provider == Provider::Codex {
            // The native launch reserves a local server before one exists.
            // An interactive fresh CLI creates its own managed server.
            let mut arguments = recipe.arguments.iter();
            let mut interactive = Vec::with_capacity(recipe.arguments.len());
            while let Some(argument) = arguments.next() {
                if argument == "--remote"
                    && arguments
                        .clone()
                        .next()
                        .is_some_and(|value| value == "unix://")
                {
                    arguments.next();
                } else if argument != "--remote=unix://" {
                    interactive.push(argument.clone());
                }
            }
            recipe.arguments = interactive;
        }
        return crate::model::agent_recovery::fresh_recipe(&recipe)
            .ok_or_else(|| "Cannot form a safe provider CLI launch command.".to_owned());
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
    automatic: bool,
) -> Result<chat::Session, String> {
    let mut session = chat::fresh_session(provider, id, working_directory, account)?;
    if automatic {
        session
            .launch
            .recipe
            .environment
            .insert(account::routing::AUTOMATIC_MARKER.into(), "1".into());
    }
    Ok(session)
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
            // Codex reports model image support only after its conversation
            // starts. Let a draft stage an image; the first send checks the
            // provider's actual capability and restores the draft on refusal.
            attachments: true,
            model_selection: true,
            reasoning_selection: true,
            permission_modes: match provider {
                Provider::Claude => vec![
                    "manual".into(),
                    "acceptEdits".into(),
                    "auto".into(),
                    "plan".into(),
                    "dontAsk".into(),
                    "bypassPermissions".into(),
                ],
                Provider::Codex => vec!["readOnly".into(), "workspaceWrite".into(), "yolo".into()],
                Provider::Omp => Vec::new(),
            },
            skill_reach: match provider {
                Provider::Codex => chat::SkillReach::Anywhere,
                Provider::Claude => chat::SkillReach::Leading,
                Provider::Omp => chat::SkillReach::First,
            },
            ..chat::Capabilities::default()
        },
        models,
        selected_model,
        reasoning,
        ..Snapshot::default()
    }
}

fn draft_permission_mode(recipe: &LaunchRecipe, provider: Provider) -> Option<String> {
    let arguments = &recipe.arguments;
    match provider {
        Provider::Claude => {
            if arguments
                .iter()
                .any(|argument| argument == "--dangerously-skip-permissions")
            {
                return Some("bypassPermissions".into());
            }
            arguments
                .iter()
                .enumerate()
                .find_map(|(index, argument)| {
                    argument
                        .strip_prefix("--permission-mode=")
                        .or_else(|| {
                            (argument == "--permission-mode")
                                .then(|| arguments.get(index + 1).map(String::as_str))
                                .flatten()
                        })
                        .map(|mode| if mode == "default" { "manual" } else { mode }.to_owned())
                })
                .or_else(|| {
                    (!recipe
                        .unset_environment
                        .contains("CLAUDE_CODE_PERMISSION_MODE"))
                    .then(|| {
                        recipe
                            .environment
                            .get("CLAUDE_CODE_PERMISSION_MODE")
                            .map(|mode| if mode == "default" { "manual" } else { mode }.to_owned())
                    })
                    .flatten()
                })
                .or_else(|| chat::claude_configured_default_mode(recipe))
                .or_else(|| Some("manual".into()))
        }
        Provider::Codex => codex_draft_permission_mode(recipe),
        Provider::Omp => None,
    }
}

/// Preview the launch policy without changing the CLI's configuration. Once
/// connected, the thread's own reported policy replaces this preview.
fn codex_draft_permission_mode(recipe: &LaunchRecipe) -> Option<String> {
    if crate::device::is_remote() {
        return None;
    }
    let arguments = &recipe.arguments;
    if arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            "--yolo" | "--dangerously-bypass-approvals-and-sandbox"
        )
    }) {
        return Some("yolo".into());
    }
    let environment = |name: &str| {
        (!recipe.unset_environment.iter().any(|unset| unset == name))
            .then(|| {
                recipe
                    .environment
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
            })
            .flatten()
    };
    let root = environment("CODEX_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| environment("HOME").map(|home| PathBuf::from(home).join(".codex")));
    let root = root.map(|root| {
        if root.is_absolute() {
            root
        } else {
            recipe.working_directory.join(root)
        }
    });
    let config = root
        .as_ref()
        .and_then(|root| std::fs::read_to_string(root.join("config.toml")).ok())
        .and_then(|contents| contents.parse::<toml::Table>().ok())
        .unwrap_or_default();

    // These recipes are not supported for new native threads. Do not guess
    // which profile or config override a resumed provider thread uses.
    if arguments.iter().any(|argument| {
        matches!(argument.as_str(), "-p" | "--profile") || argument.starts_with("--profile=")
    }) {
        return None;
    }
    if arguments.iter().enumerate().any(|(index, argument)| {
        let override_value = argument.strip_prefix("--config=").or_else(|| {
            matches!(argument.as_str(), "-c" | "--config")
                .then(|| arguments.get(index + 1).map(String::as_str))
                .flatten()
        });
        override_value.is_some_and(|value| {
            value.split_once('=').is_some_and(|(key, _)| {
                matches!(
                    key.trim(),
                    "approval_policy" | "sandbox_mode" | "default_permissions" | "permissions"
                )
            })
        })
    }) {
        return None;
    }
    let flag = |short: &str, long: &str| {
        arguments
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, argument)| {
                argument.strip_prefix(&format!("{long}=")).or_else(|| {
                    (argument == short || argument == long)
                        .then(|| arguments.get(index + 1).map(String::as_str))
                        .flatten()
                })
            })
    };
    let explicit_approval = flag("-a", "--ask-for-approval");
    let explicit_sandbox = flag("-s", "--sandbox");
    if explicit_approval.is_none() || explicit_sandbox.is_none() {
        if config.contains_key("profile")
            || config.contains_key("default_permissions")
            || config.contains_key("permissions")
        {
            return None;
        }
        let user_home = environment("HOME").map(PathBuf::from);
        let user_config =
            root.and_then(|root| std::fs::canonicalize(root.join("config.toml")).ok());
        for directory in recipe.working_directory.ancestors() {
            if user_home.as_deref() == Some(directory) {
                break;
            }
            let path = directory.join(".codex/config.toml");
            // The account's own config can also be an ancestor of cwd. It
            // remains a user layer rather than a second project layer.
            if user_config.is_some() && std::fs::canonicalize(&path).ok() == user_config {
                continue;
            }
            if let Ok(contents) = std::fs::read_to_string(path) {
                let project = contents.parse::<toml::Table>().ok()?;
                if [
                    "approval_policy",
                    "sandbox_mode",
                    "default_permissions",
                    "permissions",
                ]
                .iter()
                .any(|key| project.contains_key(*key))
                {
                    // Trust decides whether Codex loads this layer. Wait
                    // for its authoritative thread policy rather than guess.
                    return None;
                }
            }
        }
    }
    let approval =
        explicit_approval.or_else(|| config.get("approval_policy").and_then(toml::Value::as_str));
    let sandbox =
        explicit_sandbox.or_else(|| config.get("sandbox_mode").and_then(toml::Value::as_str));
    match (approval, sandbox) {
        (Some("never"), Some("danger-full-access")) => Some("yolo".into()),
        (Some("on-request"), Some("workspace-write")) => Some("workspaceWrite".into()),
        (Some("on-request"), Some("read-only")) => Some("readOnly".into()),
        _ => None,
    }
}

fn set_draft_permission_mode(recipe: &mut LaunchRecipe, provider: Provider, mode: &str) {
    if provider == Provider::Claude {
        chat::apply_claude_permission_mode(recipe, mode);
        return;
    }
    if provider != Provider::Codex || !matches!(mode, "yolo" | "readOnly" | "workspaceWrite") {
        return;
    }
    let mut arguments = Vec::with_capacity(recipe.arguments.len() + 2);
    let mut original = recipe.arguments.iter();
    while let Some(argument) = original.next() {
        match argument.as_str() {
            "-a" | "--ask-for-approval" | "-s" | "--sandbox" => {
                original.next();
            }
            "--yolo" | "--dangerously-bypass-approvals-and-sandbox" => {}
            value
                if value.starts_with("--ask-for-approval=") || value.starts_with("--sandbox=") => {}
            _ => arguments.push(argument.clone()),
        }
    }
    match mode {
        "yolo" => arguments.push("--yolo".into()),
        "readOnly" | "workspaceWrite" => arguments.extend([
            "--ask-for-approval".into(),
            "on-request".into(),
            "--sandbox".into(),
            if mode == "readOnly" {
                "read-only"
            } else {
                "workspace-write"
            }
            .into(),
        ]),
        _ => {}
    }
    recipe.arguments = arguments;
}

/// The words a chat whose first message is still waiting to be sent refuses
/// to be left with.
const QUEUED_BLOCKER: &str = "Wait for the first message to be sent before leaving the chat.";

/// How long a queued first message waits, once its provider is ready, for
/// the conversation to list the model it was meant to be sent to, before
/// going to the provider's default instead.
const MODEL_PATIENCE: Duration = Duration::from_secs(10);

impl Cagents {
    /// Keyboard and palette commands use the same pickers as the composer.
    /// Respect the control's availability before capturing a selection gesture.
    pub(super) fn open_chat_control(
        &mut self,
        command: crate::workspace_widget::GlobalShortcut,
    ) -> Task<Message> {
        use crate::workspace_widget::GlobalShortcut;
        let Some(card) = self.workspace.focused() else {
            return Task::none();
        };
        if self.chat_start_pending(card) {
            return Task::none();
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        let target = runtime.target;
        let action = match command {
            GlobalShortcut::ChatAccount
                if runtime.choosing()
                    && self
                        .chat_accounts(
                            runtime.session.launch.provider,
                            &runtime.session.launch.recipe,
                        )
                        .is_some() =>
            {
                Action::AccountPicker(target, !runtime.account_picker)
            }
            GlobalShortcut::ChatModel
                if runtime.is_draft()
                    || (runtime.snapshot.capabilities.model_selection
                        && !runtime.snapshot.models.is_empty()) =>
            {
                Action::ModelPicker(target, runtime.model_picker.is_none())
            }
            GlobalShortcut::ChatPermissions
                if !runtime.snapshot.capabilities.permission_modes.is_empty()
                    && !(runtime.session.launch.provider == Provider::Codex
                        && runtime.snapshot.busy) =>
            {
                Action::PermissionPicker(target, !runtime.permission_picker)
            }
            _ => return Task::none(),
        };
        let focus = match &action {
            Action::AccountPicker(_, true) => Some(card_control_id(card, "chat-account-choice-0")),
            Action::PermissionPicker(_, true) => runtime
                .snapshot
                .permission_mode
                .as_ref()
                .filter(|mode| {
                    runtime
                        .snapshot
                        .capabilities
                        .permission_modes
                        .contains(mode)
                })
                .or_else(|| runtime.snapshot.capabilities.permission_modes.first())
                .map(|mode| card_control_id(card, &format!("chat-permission-mode-{mode}"))),
            _ => None,
        };
        self.device_ui.menu_open = false;
        let task = self.handle_chat_ui(action);
        if let Some(runtime) = self.chat_runtime_mut(target) {
            runtime.picker_keyboard = focus.is_some();
            runtime.picker_focus = if command == GlobalShortcut::ChatPermissions {
                runtime
                    .snapshot
                    .capabilities
                    .permission_modes
                    .iter()
                    .position(|mode| Some(mode) == runtime.snapshot.permission_mode.as_ref())
                    .unwrap_or(0)
            } else {
                0
            };
        }
        match focus {
            Some(id) => Task::batch([
                task,
                iced::widget::operation::focus(id).map(cosmic::Action::App),
            ]),
            None => task,
        }
    }

    pub(super) fn chat_picker_owns_keyboard(&self) -> bool {
        self.workspace
            .focused()
            .and_then(|card| self.cards.get(&card))
            .is_some_and(|card| {
                matches!(card, CardRuntime::Chat(runtime)
                if runtime.picker_keyboard
                    && (runtime.account_picker || (runtime.permission_picker
                        && runtime.permissions_form.page == permissions::Page::Blocked)))
            })
    }

    pub(super) fn chat_picker_key(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        if !self.chat_picker_owns_keyboard() {
            return None;
        }
        let card = self.workspace.focused()?;
        let CardRuntime::Chat(runtime) = self.cards.get(&card)? else {
            return None;
        };
        let backwards = super::is_tab_press(event).or_else(|| match event {
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                modifiers,
                ..
            } if modifiers.is_empty() => match key {
                keyboard::key::Named::ArrowUp => Some(true),
                keyboard::key::Named::ArrowDown => Some(false),
                _ => None,
            },
            _ => None,
        })?;
        let ring: Vec<_> = if runtime.account_picker {
            let accounts = self.chat_accounts(
                runtime.session.launch.provider,
                &runtime.session.launch.recipe,
            )?;
            (0..accounts.choices.len())
                .map(|index| card_control_id(card, &format!("chat-account-choice-{index}")))
                .chain(std::iter::once(card_control_id(
                    card,
                    "chat-manage-accounts",
                )))
                .collect()
        } else {
            runtime
                .snapshot
                .capabilities
                .permission_modes
                .iter()
                .map(|mode| card_control_id(card, &format!("chat-permission-mode-{mode}")))
                .collect()
        };
        if ring.is_empty() {
            return None;
        }
        let target = runtime.target;
        let current = runtime.picker_focus.min(ring.len() - 1);
        let next = if backwards {
            (current + ring.len() - 1) % ring.len()
        } else {
            (current + 1) % ring.len()
        };
        self.chat_runtime_mut(target)?.picker_focus = next;
        Some(iced::widget::operation::focus(ring[next].clone()).map(cosmic::Action::App))
    }

    pub(super) fn chat_find_key(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        let card = self.workspace.focused()?;
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        if !runtime.find_focused {
            return None;
        }
        let target = runtime.target;
        let keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modifiers,
            ..
        } = event
        else {
            return None;
        };
        match key {
            keyboard::key::Named::Enter => Some(self.chat_find_next(target, !modifiers.shift())),
            keyboard::key::Named::Escape => Some(self.handle_chat_ui(Action::CloseFind(target))),
            _ => None,
        }
    }
    pub(super) fn open_chat_find(&mut self) -> Option<Task<Message>> {
        let card = self.workspace.focused()?;
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return None;
        };
        runtime.find.get_or_insert_with(String::new);
        Some(
            iced::widget::operation::focus(card_control_id(card, "chat-find"))
                .map(cosmic::Action::App),
        )
    }

    fn chat_find_next(&mut self, target: Target, forward: bool) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.find_matches.is_empty() {
            return Task::none();
        }
        runtime.find_index = if forward {
            (runtime.find_index + 1) % runtime.find_matches.len()
        } else {
            (runtime.find_index + runtime.find_matches.len() - 1) % runtime.find_matches.len()
        };
        let key = runtime.find_matches[runtime.find_index].clone();
        if let Some(turn) = runtime.turns.iter().find(|turn| turn.key == key) {
            runtime.opened.insert(key.clone(), true);
            let query = runtime.find.as_deref().unwrap_or_default();
            for placed in &turn.steps {
                if let Some(item) = runtime.snapshot.items.get(placed.index)
                    && contains_text(&item.content, query)
                    && runtime.expanded.insert(item.id.clone())
                {
                    let previous = runtime.cache.remove(&item.id);
                    runtime.cache.insert(
                        item.id.clone(),
                        CachedItem::updated(&item.content, runtime.syntax_theme, previous, true),
                    );
                }
            }
            // A delivery folded to its opening would hide a match in its body;
            // its heading is always shown.
            for index in &turn.prompt {
                if let Some(item) = runtime.snapshot.items.get(*index)
                    && matches!(&item.content, ItemContent::Delivery { body, .. } if body.contains(query))
                {
                    runtime.expanded.insert(item.id.clone());
                }
            }
        }
        crate::document_link::scroll_to_heading_anchored(
            card_control_id(target.card, "chat-transcript"),
            iced::advanced::widget::Id::new(format!("chat-turn-{}-{key}", target.card)),
            true,
        )
    }
    fn load_chat_files(&mut self, target: Target, directory: PathBuf) -> Task<Message> {
        if crate::device::is_remote() {
            return self.update_device(super::device_ui::Action::Open);
        }
        if let Some(files) = self.chat_ui.files.get(&directory).cloned() {
            return self.handle_chat_ui(Action::Files(target, directory, files));
        }
        cosmic::task::future(async move {
            let working = directory.clone();
            let files = tokio::task::spawn_blocking(move || {
                std::process::Command::new("git")
                    .args(["ls-files", "-z", "--cached"])
                    .current_dir(working)
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| {
                        output
                            .stdout
                            .split(|byte| *byte == 0)
                            .filter(|path| !path.is_empty())
                            .filter_map(|path| String::from_utf8(path.to_vec()).ok())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            Message::ChatUi(Action::Files(target, directory, Arc::new(files)))
        })
    }
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
        let automatic = self.automatic_accounts_for_directory(provider, &directory);
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
                automatic,
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
        let mut snapshot = draft_snapshot(provider, models, model.clone(), reasoning.clone());
        let recipe = moved
            .as_ref()
            .map_or(&runtime.session.launch.recipe, |session| {
                &session.launch.recipe
            });
        snapshot.permission_mode = draft_permission_mode(recipe, provider);
        if moved.is_none() {
            snapshot.skills.clone_from(&runtime.snapshot.skills);
        }
        runtime.snapshot = Arc::new(snapshot);
        runtime.completion.restart(true);
        if let Some(session) = &moved {
            runtime.session = session.clone();
            runtime.error = None;
        }
        self.remember_chat_model(provider, model, reasoning);
        let skills = if let Some(session) = moved {
            self.replace_chat_conversation(target.card, session);
            self.load_draft_skills(target)
        } else {
            Task::none()
        };
        Task::batch([
            skills,
            iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
                .map(cosmic::Action::App),
        ])
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
        let mut session = match fresh_chat_session(
            account.provider,
            runtime.session.id,
            runtime.session.launch.recipe.working_directory.clone(),
            &account,
            false,
        ) {
            Ok(session) => session,
            Err(error) => {
                runtime.error = Some(error);
                return Task::none();
            }
        };
        // A picker choice is a pin, even if it chooses the same home as the
        // automatic router. Keep it across saves, restarts and terminal views.
        session
            .launch
            .recipe
            .environment
            .remove(account::routing::AUTOMATIC_MARKER);
        if session == runtime.session {
            return Task::none();
        }
        runtime.session = session.clone();
        Arc::make_mut(&mut runtime.snapshot).skills.clear();
        Arc::make_mut(&mut runtime.snapshot).permission_mode =
            draft_permission_mode(&session.launch.recipe, account.provider);
        runtime.completion.restart(true);
        self.replace_chat_conversation(target.card, session);
        self.load_draft_skills(target)
    }

    fn recover_chat_account(&mut self, card: CardId, id: AccountId) -> Task<Message> {
        self.change_chat_account(card, id, None, None)
    }

    fn automatic_chat_account(
        &self,
        target: Target,
        mode: PromptMode,
    ) -> Option<(Account, Option<u64>, u64)> {
        if mode != PromptMode::Send || self.chat_start_pending(target.card) {
            return None;
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) else {
            return None;
        };
        if runtime.target != target
            || runtime.is_draft() && !runtime.choosing()
            || runtime.pastes_pending > 0
            || runtime.composer.text().trim().is_empty() && runtime.attachments.is_empty()
            || runtime
                .session
                .launch
                .recipe
                .environment
                .get(account::routing::AUTOMATIC_MARKER)
                .map(String::as_str)
                != Some("1")
            || slash_invocation(&runtime.composer.text(), &runtime.snapshot).is_some()
        {
            return None;
        }
        if !runtime.is_draft()
            && (!runtime.snapshot.ready
                || runtime.snapshot.busy
                || runtime.snapshot.waiting
                || !runtime.snapshot.requests.is_empty()
                || !runtime.snapshot.queued_messages.is_empty()
                || runtime.pending.is_some()
                || !runtime.followups.is_empty()
                || runtime.disconnected
                || runtime.snapshot.exited)
        {
            return None;
        }
        let id = self.chat_account(
            runtime.session.launch.provider,
            &runtime.session.launch.recipe,
        )?;
        let current = self.known_account(&id)?;
        let alternate = self.automatic_account(&current)?;
        Some((
            alternate,
            (!runtime.is_draft()).then_some(runtime.snapshot.generation),
            runtime.revision,
        ))
    }

    fn change_chat_account(
        &mut self,
        card: CardId,
        id: AccountId,
        generation: Option<u64>,
        submit: Option<(u64, PromptMode)>,
    ) -> Task<Message> {
        if (generation.is_none() && !self.chat_ui.removed_accounts.contains(&card))
            || self.chat_start_pending(card)
        {
            return Task::none();
        }
        let Some(CardKind::Chat(source)) = self.card_state(card).map(|state| &state.kind) else {
            return Task::none();
        };
        let Some(account) = self
            .known_account(&id)
            .filter(|account| account.provider == source.launch.provider)
        else {
            return Task::none();
        };
        let source = source.clone();
        let mut replacement = source.clone();
        replacement.native_continuity = chat::NativeContinuity::ReconnectOnly;
        replacement.reset_owned_codex_listener();
        replacement.id = TerminalId::new();
        replacement.proof = None;
        replacement.terminal = None;
        let variable = account::home_variable(account.provider).unwrap();
        let names = account::credential_overrides(account.provider);
        replacement.launch.recipe.environment.remove(variable);
        replacement
            .launch
            .recipe
            .environment
            .retain(|name, _| !names.contains(&name.as_str()));
        replacement
            .launch
            .recipe
            .unset_environment
            .retain(|name| !names.contains(&name.as_str()));
        replacement.launch.recipe.unset_environment.extend(
            account
                .credential_overrides()
                .iter()
                .map(|name| (*name).to_owned()),
        );
        if let Some(home) = account.resolved_home() {
            let Some(home) = home.to_str() else {
                return Task::none();
            };
            replacement
                .launch
                .recipe
                .environment
                .insert(variable.into(), home.into());
        }
        if !chat::is_account_recovery(&source, &replacement) {
            return Task::none();
        }
        let target = self.chat_ui.next_target(card);
        self.chat_ui.phases.insert(
            card,
            Phase::Starting(Attempt {
                target,
                session: replacement.clone(),
                upgrade_committed: false,
                recovery: Some(AccountRecovery {
                    source: source.clone(),
                    committed: false,
                    submit,
                }),
            }),
        );
        let sender = self.chat_ui.sender.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                Self::started_chat(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || {
                        let wake = move |wake| {
                            let _ = sender.try_send((target, wake));
                        };
                        match generation {
                            Some(generation) => {
                                ClientHandle::switch_account(source, replacement, generation, wake)
                            }
                            None => ClientHandle::recover_account(source, replacement, wake),
                        }
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

    pub(super) fn account_recovery_controls(&self, card: CardId) -> Option<Element<'_, Message>> {
        if !self.chat_ui.removed_accounts.contains(&card) {
            return None;
        }
        let Some(CardKind::Chat(source)) = self.card_state(card).map(|state| &state.kind) else {
            return None;
        };
        if source.launch.session_id.is_none() {
            return None;
        }
        let choices = self
            .chat_accounts(source.launch.provider, &source.launch.recipe)
            .into_iter()
            .flat_map(|accounts| accounts.choices)
            .filter(|choice| !choice.selected)
            .collect::<Vec<_>>();
        let pending = self.chat_start_pending(card);
        let mut content: Vec<Element<'_, Message>> = vec![design::caption(if pending {
            "Checking the saved conversation on the selected account…"
        } else { "Continue this saved conversation on another account. Its identity stays unchanged until the provider confirms it." }).into()];
        if choices.is_empty() && !pending {
            content.push(
                design::caption("Add or sign in to another account in Accounts to continue.")
                    .into(),
            );
        }
        for choice in choices {
            content.push(
                crate::ui_controls::button::secondary(format!(
                    "{}{}",
                    choice.label,
                    if choice.detail.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", choice.detail)
                    }
                ))
                .on_press_maybe(
                    (!pending).then_some(Message::ChatUi(Action::RecoverAccount(card, choice.id))),
                )
                .into(),
            );
        }
        content.push(
            crate::ui_controls::button::secondary("Open Accounts")
                .on_press(Message::AccountUi(account_ui::Action::OpenPane))
                .into(),
        );
        Some(
            widget::container(
                widget::scrollable(widget::column::with_children(content).spacing(6))
                    .height(Length::Shrink),
            )
            .max_height(180)
            .padding(space::S)
            .into(),
        )
    }

    /// Read authored skills on the blocking pool. No daemon or provider is
    /// involved, and a late result cannot replace a different account's list
    /// or the authoritative catalog of a conversation that has started.
    fn load_draft_skills(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.is_draft() {
            return Task::none();
        }
        let launch = runtime.session.launch.clone();
        cosmic::task::future(async move {
            let source = launch.clone();
            let skills = tokio::task::spawn_blocking(move || {
                if crate::device::is_remote() {
                    match crate::device_service::request(crate::device_service::Operation::Skills {
                        launch: source,
                    }) {
                        Ok(crate::device_service::Reply::Skills(skills)) => skills,
                        _ => Vec::new(),
                    }
                } else {
                    chat::skills::discover(&source)
                }
            })
            .await
            .unwrap_or_default();
            Message::ChatUi(Action::DraftSkills(target, launch, skills))
        })
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
                        y: runtime.active_scroll().page_height()
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
        self.new_native_chat(self.preferred_chat_provider(), None)
    }

    fn preferred_chat_provider(&self) -> Provider {
        self.workspace
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
            .unwrap_or(Provider::Codex)
    }

    pub(super) fn add_default_pane(
        &mut self,
        directory: PathBuf,
        pane: settings::DefaultPane,
    ) -> Task<Message> {
        match pane {
            settings::DefaultPane::Terminal => {
                if self.workspace_model.identity == crate::workspace_model::NO_REGISTRY
                    && self.worktree_view_scope(&directory).is_some()
                {
                    self.shared_views.deferred_default = Some(directory.clone());
                    return Task::none();
                }
                self.shared_views.creating_default =
                    self.workspace_model.identity != crate::workspace_model::NO_REGISTRY;
                let task = self.add_terminal(directory);
                self.shared_views.creating_default = false;
                task
            }
            settings::DefaultPane::NativeChat => self.new_native_chat_with_lifetime(
                self.preferred_chat_provider(),
                Some(directory),
                true,
            ),
        }
    }

    pub(super) fn ensure_default_pane(&mut self) -> Task<Message> {
        let Some(directory) = self.active_workspace.clone() else {
            return Task::none();
        };
        if self.workspace.order().any(|card| {
            self.card_state(card).is_some_and(|state| {
                state.existence.is_open()
                    && matches!(state.kind, CardKind::Terminal(_) | CardKind::Chat(_))
            })
        }) {
            return Task::none();
        }
        let pane = if self.durable.active_channel.is_some() {
            self.settings.default_channel_pane
        } else {
            self.settings.default_worktree_pane
        };
        self.add_default_pane(directory, pane)
    }

    pub(super) fn chat_is_unused_default(&self, card: CardId) -> bool {
        self.chat_ui.default_chats.contains(&card) && self.chat_is_unused(card)
    }

    pub(super) fn chat_is_unused(&self, card: CardId) -> bool {
        matches!(self.cards.get(&card), Some(CardRuntime::Chat(runtime))
            if runtime.session.is_unstarted()
                && runtime.choosing()
                && runtime.composer.is_empty()
                && runtime.attachments.is_empty()
                && runtime.pending.is_none())
            && !self.chat_start_pending(card)
    }

    /// Placeholders never reach storage. Once a pane contains work, promote
    /// it before saving so later snapshots cannot silently drop a saved card.
    pub(super) fn persistent_workspace(&mut self) -> Arc<WorkspaceState> {
        let used = self
            .chat_ui
            .default_chats
            .iter()
            .copied()
            .filter(|card| !self.chat_is_unused(*card))
            .collect::<Vec<_>>();
        for card in used {
            self.chat_ui.default_chats.remove(&card);
        }
        let defaults = self
            .chat_ui
            .default_chats
            .iter()
            .copied()
            .chain(self.shared_views.defaults.keys().copied())
            .collect::<HashSet<_>>();
        let open_cards = self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open() && !defaults.contains(&card.id))
            .map(|card| card.id)
            .collect::<std::collections::HashSet<_>>();
        let mut chat_drafts: HashMap<CardId, String> = self
            .chat_ui
            .drafts
            .iter()
            .filter(|(card, draft)| open_cards.contains(card) && !draft.is_empty())
            .map(|(card, draft)| (*card, draft.clone()))
            .collect();
        for (card, runtime) in &self.chat_ui.parked {
            if open_cards.contains(card) {
                let draft = runtime.unsent_text();
                if draft.is_empty() {
                    chat_drafts.remove(card);
                } else {
                    chat_drafts.insert(*card, draft);
                }
            }
        }
        for (card, card_runtime) in &self.cards {
            if let CardRuntime::Chat(runtime) = card_runtime
                && open_cards.contains(card)
            {
                let draft = runtime.unsent_text();
                if draft.is_empty() {
                    chat_drafts.remove(card);
                } else {
                    chat_drafts.insert(*card, draft);
                }
            }
        }
        if defaults.is_empty() && chat_drafts == self.durable.chat_drafts {
            return Arc::clone(&self.durable);
        }
        let mut saved = (*self.durable).clone();
        saved.chat_drafts = chat_drafts;
        if defaults.is_empty() {
            return Arc::new(saved);
        }
        saved.cards.retain(|card| !defaults.contains(&card.id));
        saved.card_scopes.retain(|card, _| !defaults.contains(card));
        saved
            .worktree_focus
            .retain(|_, card| !defaults.contains(card));
        saved.focused = saved.focused.filter(|card| !defaults.contains(card));
        for channel in &mut saved.channels {
            channel.cards.retain(|card| !defaults.contains(card));
            channel.focused = channel.focused.filter(|card| !defaults.contains(card));
        }
        Arc::new(saved)
    }

    /// Remove only automatic placeholders in the scope being left. This is
    /// synchronous: a delayed close message would target the replacement strip
    /// and could delete the channel rather than just its unused placeholder.
    pub(super) fn discard_unused_default_chats(&mut self) -> bool {
        let defaults = self
            .workspace
            .order()
            .filter(|card| self.chat_ui.default_chats.contains(card))
            .collect::<Vec<_>>();
        let mut removed = false;
        for card in defaults {
            self.chat_ui.default_chats.remove(&card);
            let unused = self.chat_is_unused(card);
            if !unused {
                removed = true;
                continue;
            }
            self.chat_discard(card);
            self.cards.remove(&card);
            self.card_chrome.remove(&card);
            self.lifecycle.remove(card);
            self.worktree_owners.remove(&card);
            self.worktree_focus.retain(|_, focused| *focused != card);
            let durable = Arc::make_mut(&mut self.durable);
            durable.card_scopes.remove(&card);
            durable.worktree_focus.retain(|_, focused| *focused != card);
            if durable.focused == Some(card) {
                durable.focused = None;
            }
            // These cards were excluded from every saved snapshot, so there
            // is no durable identity needing a tombstone or daemon retirement.
            durable.cards.retain(|state| state.id != card);
            for channel in &mut durable.channels {
                channel.cards.retain(|id| *id != card);
                if channel.focused == Some(card) {
                    channel.focused = channel.cards.first().copied();
                }
            }
            removed = true;
        }
        removed
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
            || runtime.permission_picker
            || runtime.account_picker;
        runtime.reasoning_picker = false;
        runtime.permission_picker = false;
        runtime.account_picker = false;
        runtime.picker_keyboard = false;
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
        self.new_native_chat_with_lifetime(provider, directory, false)
    }

    fn new_native_chat_with_lifetime(
        &mut self,
        provider: Provider,
        directory: Option<PathBuf>,
        ephemeral: bool,
    ) -> Task<Message> {
        let Some(working_directory) = directory.or_else(|| self.terminal_directory()) else {
            return cosmic::task::message(Message::OpenRepository);
        };
        if self.refuse_in_deleting_worktree(&working_directory) {
            return Task::none();
        }
        let account = self.default_account_for_directory(provider, &working_directory);
        let automatic = self.automatic_accounts_for_directory(provider, &working_directory);
        let session = match fresh_chat_session(
            provider,
            TerminalId::new(),
            working_directory.clone(),
            &account,
            automatic,
        ) {
            Ok(session) => session,
            Err(error) => {
                self.status = Some(TransientNotice::error(error));
                return Task::none();
            }
        };
        let scope = if self.durable.active_channel.is_none()
            && self.active_workspace.as_deref() != Some(working_directory.as_path())
        {
            self.select_worktree(working_directory, false)
        } else {
            Task::none()
        };
        Task::batch([
            scope,
            self.insert_chat_card_with_lifetime(session, None, ephemeral, None),
        ])
    }

    /// Opens a card for the new conversation `session`, as a draft whose first
    /// message starts it: beside the card it was begun from, filed where that
    /// card is, or else where a new terminal would go. A stopped terminal's
    /// restored chat draft can move to the new card before its first snapshot.
    pub(super) fn insert_chat_card(
        &mut self,
        session: chat::Session,
        beside: Option<CardId>,
        draft_from: Option<CardId>,
    ) -> Task<Message> {
        self.insert_chat_card_with_lifetime(session, beside, false, draft_from)
    }

    fn insert_chat_card_with_lifetime(
        &mut self,
        session: chat::Session,
        beside: Option<CardId>,
        ephemeral: bool,
        draft_from: Option<CardId>,
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
        tracing::debug!(target: "cagents::pane_open", stage = "requested", card = ?card_id, kind = "chat");
        // A stopped terminal cannot be converted in place. If its chat had
        // been parked before a restart, the new conversation owns that text.
        if let Some(source) = draft_from
            && let Some(draft) = self.chat_ui.drafts.remove(&source)
        {
            self.chat_ui.drafts.insert(card_id, draft);
        }
        if ephemeral {
            self.chat_ui.default_chats.insert(card_id);
        }
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: chat_card_title(session.launch.provider).into(),
            width_fraction: beside
                .and_then(|(source, _)| self.card_state(source))
                .map_or(self.settings.default_native_pane_fraction, |source| {
                    if matches!(source.kind, CardKind::Chat(_)) {
                        source.width_fraction
                    } else {
                        self.settings.default_native_pane_fraction
                    }
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
        let skills = if session.is_unstarted() {
            self.open_chat_draft(card_id, session)
        } else {
            let intent = if matches!(
                session.native_continuity,
                chat::NativeContinuity::Durable { .. }
            ) {
                chat::Intent::Restore
            } else {
                chat::Intent::Start
            };
            self.open_chat(card_id, session, intent)
        };
        self.persist();
        Task::batch([
            skills,
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
        let skills = self.open_chat_draft(card, session);
        Task::batch([retire, skills])
    }

    /// Puts a draft of `session` in card `card`, on the model last chosen for
    /// its provider.
    fn open_chat_draft(&mut self, card: CardId, session: chat::Session) -> Task<Message> {
        let mut snapshot = self.chat_draft_snapshot(session.launch.provider);
        snapshot.permission_mode =
            draft_permission_mode(&session.launch.recipe, session.launch.provider);
        let target = self.chat_ui.next_target(card);
        let visible = self.workspace.visibility(card) == Some(VisibilityClass::Visible);
        let mut runtime = Runtime::draft(
            target,
            session,
            snapshot,
            visible,
            self.settings.editor_syntax_theme,
        );
        runtime.tool_groups_expanded = self.settings.chat_expand_tool_groups;
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
        self.load_draft_skills(target)
    }

    pub(super) fn chat_subscription(&self) -> Subscription<Message> {
        let wakes = Subscription::run_with(Source(self.chat_ui.receiver.clone()), stream)
            .map(|(target, wake)| Message::ChatUi(Action::Wake(target, wake)));
        if self.cards.values().any(|card| {
            matches!(card, CardRuntime::Chat(runtime)
            if runtime.snapshot.busy && runtime.visible.load(Ordering::Relaxed))
        }) {
            Subscription::batch([
                wakes,
                iced::time::every(Duration::from_secs(1)).map(|_| Message::ChatUi(Action::Tick)),
            ])
        } else {
            wakes
        }
    }
    pub(super) fn shared_chat_has_local_work(&self, card: CardId) -> bool {
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&card).map(Box::as_ref),
        };
        runtime.is_some_and(|runtime| {
            !runtime.unsent_text().is_empty() || runtime.transition_blocker().is_some()
        }) || self
            .chat_ui
            .drafts
            .get(&card)
            .is_some_and(|draft| !draft.is_empty())
    }

    pub(super) fn can_rekey_shared_terminal_draft(&self, alias: CardId, canonical: CardId) -> bool {
        !self.chat_ui.phases.contains_key(&alias)
            && !self.chat_ui.parked.contains_key(&alias)
            && !self.shared_chat_has_local_work(canonical)
    }

    pub(super) fn rekey_shared_terminal_draft(&mut self, alias: CardId, canonical: CardId) -> bool {
        if !self.can_rekey_shared_terminal_draft(alias, canonical) {
            return false;
        }
        if let Some(draft) = self.chat_ui.drafts.remove(&alias) {
            self.chat_ui.drafts.insert(canonical, draft);
        }
        true
    }

    pub(super) fn shared_chat_detach(&mut self, card: CardId) {
        // Dropping a phase also fences a late attachment result. Unlike
        // abandoned creator starts, it grants no authority to close an actor.
        self.chat_ui.phases.remove(&card);
        self.chat_ui.parked.remove(&card);
        self.observe_agent_activity_for_card(card, None);
    }

    pub(super) fn device_switch_chat_blocker(&self) -> Option<&'static str> {
        self.cards
            .values()
            .filter_map(|card| match card {
                CardRuntime::Chat(runtime) => runtime.transition_blocker(),
                _ => None,
            })
            .chain(
                self.chat_ui
                    .parked
                    .values()
                    .filter_map(|runtime| runtime.transition_blocker()),
            )
            .next()
    }

    pub(super) fn chat_forget(&mut self, card: CardId) {
        // The runtime is about to be dropped, so its unsent text is captured
        // here rather than lost. `chat_forget` is the single place a chat
        // runtime stops existing while its card lives on.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) {
            let draft = runtime.unsent_text();
            if draft.is_empty() {
                self.chat_ui.drafts.remove(&card);
            } else {
                self.chat_ui.drafts.insert(card, draft);
            }
        }
        self.chat_ui.phases.remove(&card);
        self.observe_agent_activity_for_card(card, None);
    }
    /// Forgets a chat runtime whose card is going away for good.
    ///
    /// The difference from `chat_forget` is the draft: a card that is about to
    /// be tombstoned will never build a runtime under that id again, so
    /// stashing its unsent text would pin the string for the lifetime of the
    /// process with nothing able to read it back.
    pub(super) fn chat_discard(&mut self, card: CardId) {
        self.chat_ui.default_chats.remove(&card);
        // A start still in flight is not dropped with the card: whatever it
        // leaves in the daemon is given up when it answers, as for a close.
        match self.chat_ui.phases.remove(&card) {
            Some(Phase::Starting(attempt)) => {
                self.chat_ui
                    .abandoned
                    .insert(attempt.target, attempt.session);
            }
            Some(Phase::Closing { target, .. }) => {
                if let Some(session) = self.chat_return_session(card) {
                    self.chat_ui.abandoned.insert(target, session);
                }
            }
            _ => {}
        }
        self.chat_forget(card);
        self.chat_ui.drafts.remove(&card);
        self.chat_ui.parked.remove(&card);
        self.chat_ui.fresh_offers.remove(&card);
        self.chat_ui.removed_accounts.remove(&card);
    }
    pub(super) fn chat_start_pending(&self, card: CardId) -> bool {
        self.chat_ui.phases.contains_key(&card)
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
                    "Open in terminal"
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
        self.chat_switch_blocker(card).is_none()
    }
    /// The control and its handler share the reason a handoff cannot start.
    pub(super) fn chat_switch_blocker(&self, card: CardId) -> Option<Cow<'static, str>> {
        if let Some(phase) = self.chat_ui.phases.get(&card) {
            return Some(Cow::Borrowed(match phase {
                Phase::Starting(_) => {
                    "Connecting to native chat. Wait for the connection to finish."
                }
                Phase::Returning { .. } => {
                    "Returning to the terminal. Wait for the handoff to finish."
                }
                Phase::Closing { .. } => "The conversation is closing. Wait for it to finish.",
            }));
        }
        let Some(state) = self
            .card_state(card)
            .filter(|state| state.existence.is_open())
        else {
            return Some(Cow::Borrowed("This pane is closed."));
        };
        if self.chat_ui.parked.contains_key(&card) {
            return None;
        }
        match &state.kind {
            CardKind::Terminal(terminal) => {
                if self.pending_terminal_spawn(card) {
                    return Some(Cow::Borrowed(
                        "This terminal is still starting. Wait for its agent to start.",
                    ));
                }
                let session = match conversion(terminal) {
                    Ok(session) => session,
                    Err(reason) => return Some(Cow::Owned(reason)),
                };
                if terminal.continuity.live_proof().is_none()
                    && !session.owns_codex_listener()
                    && crate::model::agent_recovery::fresh_recipe(&session.launch.recipe).is_none()
                {
                    Some(Cow::Borrowed(
                        "This terminal's launch options cannot be reused for a native conversation.",
                    ))
                } else {
                    None
                }
            }
            CardKind::Chat(_) => match self.cards.get(&card) {
                Some(CardRuntime::Chat(runtime)) => {
                    let named = runtime.connected()
                        && runtime
                            .handle()
                            .is_some_and(|handle| handle.snapshot().session_id.is_some());
                    if named {
                        runtime.handoff_blocker().map(Cow::Borrowed)
                    } else {
                        runtime.transition_blocker().map(Cow::Borrowed)
                    }
                }
                _ => None,
            },
            _ => Some(Cow::Borrowed(
                "This pane does not contain an agent conversation.",
            )),
        }
    }
    /// The conversation a chat card would return from, as current as the card
    /// knows it: the runtime's session carries the identity the provider has
    /// confirmed since the card was saved.
    fn chat_return_session(&self, card: CardId) -> Option<chat::Session> {
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) {
            let mut session = runtime.session.clone();
            if let Some(handle) = runtime.handle() {
                let observed = handle.session();
                if observed.launch.session_id.is_some() {
                    session.launch.session_id = observed.launch.session_id;
                }
                if matches!(
                    observed.native_continuity,
                    chat::NativeContinuity::Durable { .. }
                ) {
                    session.native_continuity = observed.native_continuity;
                }
            }
            return Some(session);
        }
        match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(session)) => Some(session.clone()),
            _ => None,
        }
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
    /// A retained native owner waits for exact cleanup proof, keeping its saved
    /// descriptor available for a failed-close retry even when a local adapter
    /// still has a connection. That adapter may have failed to attach the owner.
    /// Two other cases wait for a daemon answer: a return to the terminal
    /// already under way closes the card as soon as it lands, and a chat parked
    /// behind its source terminal closes before that terminal's own close, so
    /// the terminal's close never races the actor that owns it.
    pub(super) fn begin_chat_close(&mut self, card: CardId) -> Task<Message> {
        if matches!(
            self.chat_ui.phases.get(&card),
            Some(Phase::Closing { restart: None, .. })
        ) {
            return Task::none();
        }
        if let Some(Phase::Returning {
            close_requested, ..
        }) = self.chat_ui.phases.get_mut(&card)
        {
            *close_requested = true;
            return Task::none();
        }
        // Only a live conversation is ever parked behind its terminal.
        if let Some(runtime) = self.chat_ui.parked.get(&card)
            && let Some(handle) = runtime.handle().cloned()
        {
            let target = runtime.target;
            if self.chat_ui.phases.contains_key(&card) {
                return Task::none();
            }
            self.chat_ui.phases.insert(
                card,
                Phase::Closing {
                    target,
                    restart: None,
                },
            );
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
        // Capture the retirement capability before the synchronous removal
        // drops the surface. In-flight operations move to the detached ledger
        // only after commit succeeds, and retire when their own answer lands.
        let pending = self.chat_ui.phases.contains_key(&card);
        let handle = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) if runtime.connected() => runtime
                .handle()
                .filter(|handle| !handle.snapshot().exited)
                .cloned(),
            _ => None,
        };
        if !pending
            && session.proof.is_some()
            && (session.owns_codex_listener()
                || matches!(
                    session.native_continuity,
                    chat::NativeContinuity::Durable { .. }
                ))
        {
            let target = self.chat_ui.next_target(card);
            self.chat_ui.phases.insert(
                card,
                Phase::Closing {
                    target,
                    restart: None,
                },
            );
            return cosmic::task::future(async move {
                let saved = session.clone();
                let result =
                    tokio::task::spawn_blocking(move || retire_conversation(handle, Some(saved)))
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                Message::ChatUi(Action::RetainedCloseFinished(target, session, result))
            });
        }
        let Some(remove) = self.remove_chat_card(card, None) else {
            return Task::none();
        };
        let retire = if pending {
            Task::none()
        } else {
            self.retire_chat(card, handle, Some(session))
        };
        Task::batch([retire, remove])
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
    /// exact handoff of one conversation; an unnamed chat starts a fresh CLI.
    fn return_chat_locally(&mut self, card: CardId) -> Task<Message> {
        let Some(session) = self.chat_return_session(card) else {
            return Task::none();
        };
        let mut launch = match local_return_launch(&session) {
            Ok(launch) => launch,
            Err(error) => {
                self.status = Some(TransientNotice::error(error));
                return Task::none();
            }
        };
        if session.launch.provider == Provider::Claude
            && let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card)
            && let Some(mode) = runtime.snapshot.permission_mode.as_deref()
        {
            chat::apply_claude_permission_mode(&mut launch, mode);
        }
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
        self.chat_ui.phases.insert(
            card,
            Phase::Returning {
                target,
                local: Some(LocalReturn {
                    session: session.clone(),
                    launch,
                    resumes,
                }),
                close_requested: false,
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

    /// Reserves and commits in one update. Placement, runtime ownership and
    /// durable identity cannot be split by a scope switch between messages.
    fn remove_chat_card(
        &mut self,
        card: CardId,
        replacement: Option<LaunchRecipe>,
    ) -> Option<Task<Message>> {
        let replacement = match replacement {
            Some(launch) => {
                // The return completed asynchronously. Deletion may have
                // begun since it started; keep the source if its replacement
                // would now be refused by insert_terminal_card.
                if self.refuse_in_deleting_worktree(&launch.working_directory) {
                    return None;
                }
                let index = self.workspace.order().position(|id| id == card)?;
                let durable = self
                    .durable
                    .cards
                    .iter()
                    .position(|state| state.id == card)?;
                let succession = match &self.durable.cards[durable].kind {
                    CardKind::Chat(session) => Some((succession_root(session), session.id)),
                    _ => None,
                };
                Some((launch, (index, durable + 1), succession))
            }
            None => None,
        };
        self.reserve_chat_removal(card)?;
        self.commit_card_removal(card, replacement)
    }

    /// Opens the terminal that replaces a chat whose removal has committed.
    pub(super) fn open_chat_replacement(
        &mut self,
        launch: LaunchRecipe,
        slot: (usize, usize),
        succession: Option<(TerminalId, TerminalId)>,
    ) -> Task<Message> {
        let mut session = fresh_terminal_session(launch);
        session.resume_hint = crate::model::agent_recovery::hint_from_launch(&session.launch);
        // The provider's CLI succeeds the chat it replaces.
        session.recovery_origin = succession.map(|(root, _)| root);
        session.predecessor = succession.map(|(_, chat)| chat);
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
        self.chat_ui.phases.insert(
            card,
            Phase::Starting(Attempt {
                target,
                session: session.clone(),
                recovery: None,
                upgrade_committed: false,
            }),
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
            || self
                .card_state(target.card)
                .is_none_or(|state| !state.existence.is_open())
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
        let text = runtime.composer.text();
        let resolved_text = completion::invocations(
            completion::from_leading_skill(&text, &runtime.snapshot.skills),
            &runtime.snapshot.skills,
        )
        .into_owned();
        runtime.queued = Some(Queued {
            text,
            resolved_text,
            attachments: std::mem::take(&mut runtime.attachments),
            model,
            draft: session.clone(),
            since: None,
            rechecking: false,
        });
        runtime.replace_draft(text_editor::Content::new());
        runtime.history_cursor = None;
        runtime.history_stash = None;
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
        let latest = handle.snapshot();
        if latest.ready && !latest.busy && !latest.exited && !runtime.followups.is_empty() {
            let (text, attachments) = runtime.combined_followups();
            // A queued message remains a prompt. Resolving it as an idle
            // slash command could discard everything combined after /compact.
            let command = match submission(&text, &latest, &attachments, PromptMode::Queue) {
                Ok(Command::Prompt {
                    text, attachments, ..
                }) => Command::Prompt {
                    text,
                    attachments,
                    mode: PromptMode::Send,
                },
                Ok(_) => unreachable!("queue submission always constructs a prompt"),
                Err(error) => {
                    runtime.error = Some(error.into());
                    return Task::none();
                }
            };
            let task = self.chat_send(target, command, false);
            if let Some(runtime) = self.chat_runtime_mut(target)
                && let Some(pending) = &mut runtime.pending
            {
                pending.followup = true;
            }
            return task;
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
            queued.submission_text(&snapshot),
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
        let mut snapshot = self.chat_draft_snapshot(provider);
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        let Some(draft) = runtime.unqueue() else {
            return Task::none();
        };
        snapshot.permission_mode = draft_permission_mode(&draft.launch.recipe, provider);
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
        Task::batch([
            self.retire_chat(target.card, handle, Some(draft)),
            self.load_draft_skills(target),
        ])
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
            if let Some(Phase::Starting(attempt)) = self.chat_ui.phases.remove(&target.card) {
                self.chat_ui
                    .abandoned
                    .insert(attempt.target, attempt.session);
            }
            return Task::none();
        }
        self.revert_chat_draft(target, None)
    }

    fn resume_history_navigation(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.pending.is_some()
            || runtime.queued.is_some()
            || runtime.disconnected
            || runtime.selected_subagent.is_some()
        {
            return Task::none();
        }
        let Some(page) = runtime.deferred_page.take() else {
            return Task::none();
        };
        match page {
            HistoryNavigation::Latest => self.handle_chat_action(Action::Latest(target)),
            HistoryNavigation::LatestEdge
                if runtime.scroll.at_bottom()
                    && !runtime.scroll.at_top()
                    && runtime.snapshot.has_later =>
            {
                self.handle_chat_action(Action::Latest(target))
            }
            HistoryNavigation::EarlierExplicit if runtime.snapshot.has_earlier => {
                self.chat_send(target, Command::LoadEarlier, false)
            }
            HistoryNavigation::EarlierEdge
                if runtime.scroll.at_top()
                    && !runtime.scroll.at_bottom()
                    && runtime.snapshot.has_earlier =>
            {
                self.chat_send(target, Command::LoadEarlier, false)
            }
            _ => Task::none(),
        }
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
        if command == Command::LoadEarlier
            && (runtime.selected_subagent.is_some()
                || !runtime.snapshot.has_earlier
                || runtime.snapshot.history_loading
                || runtime.page_request.is_some())
        {
            return Task::none();
        }
        // The ACK confirms dispatch, not arrival of the newest history.
        // Coalesce repeated gestures while that same navigation is in flight.
        if command == Command::Refresh
            && runtime
                .page_request
                .is_some_and(|(page, _)| page == view::Page::Newer)
        {
            return Task::none();
        }
        runtime.next_command = runtime.next_command.wrapping_add(1);
        let id = runtime.next_command;
        let page = match &command {
            Command::LoadEarlier => Some(view::Page::Earlier),
            Command::Refresh => Some(view::Page::Newer),
            _ => None,
        };
        if let Some(page) = page {
            runtime.page_request = Some((page, runtime.snapshot.generation));
            match page {
                view::Page::Earlier => runtime.earlier_asked = Some(runtime.scroll.content()),
                view::Page::Newer => runtime.later_asked = Some(runtime.scroll.content()),
            }
        }
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
            followup: false,
            take_back: None,
            id,
            draft_revision: clear_draft.then_some(runtime.revision),
            request,
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
        let restore = match &action {
            Action::AccountPicker(target, false)
            | Action::PermissionPicker(target, false)
            | Action::ModelPicker(target, false)
            | Action::ChooseAccount(target, _)
            | Action::DraftModel(target, _, _)
            | Action::Model(target, _)
            | Action::PermissionMode(target, _) => {
                self.cards.get(&target.card).and_then(|card| match card {
                    CardRuntime::Chat(runtime)
                        if runtime.target == *target
                            && (runtime.picker_keyboard || runtime.model_picker.is_some()) =>
                    {
                        Some(*target)
                    }
                    _ => None,
                })
            }
            _ => None,
        };
        let task = self.handle_chat_ui_inner(action);
        if let Some(target) = restore
            && let Some(runtime) = self.chat_runtime_mut(target)
            && !runtime.account_picker
            && !runtime.permission_picker
            && runtime.model_picker.is_none()
        {
            runtime.picker_keyboard = false;
            return Task::batch([
                task,
                iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
                    .map(cosmic::Action::App),
            ]);
        }
        task
    }

    fn handle_chat_ui_inner(&mut self, action: Action) -> Task<Message> {
        if crate::device::is_remote() && matches!(&action, Action::PastedImage(_, Some(_))) {
            self.status = Some(TransientNotice::warning(
                "Images need transfer to the selected device before attachment",
            ));
            return Task::none();
        }
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
            && (self.chat_start_pending(target.card)
                || self
                    .card_state(target.card)
                    .is_none_or(|state| !state.existence.is_open()))
        {
            return Task::none();
        }
        if let Some(card) = action.starts_session()
            && self.refuse_for_deleting_card(card)
        {
            return Task::none();
        }
        match action {
            Action::DraftModel(target, provider, model) => {
                return self.choose_draft_model(target, provider, model);
            }
            Action::ChooseAccount(target, id) => {
                return self.choose_chat_account(target, id);
            }
            Action::RecoverAccount(card, id) => return self.recover_chat_account(card, id),
            Action::DraftSkills(target, launch, skills) => {
                if let Some(runtime) = self.chat_card_runtime_mut(target)
                    && runtime.is_draft()
                    && runtime.session.launch == launch
                {
                    Arc::make_mut(&mut runtime.snapshot).skills = skills;
                    runtime.completion.restart(true);
                }
            }
            Action::AccountPicker(target, open) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.picker_keyboard = false;
                    runtime.account_picker = open && runtime.choosing();
                    runtime.model_picker = None;
                    runtime.reasoning_picker = false;
                    runtime.permission_picker = false;
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
                if let Some(reason) = self.chat_switch_blocker(card) {
                    self.status = Some(
                        if self.chat_start_pending(card) || self.pending_terminal_spawn(card) {
                            TransientNotice::warning(reason.into_owned())
                        } else {
                            TransientNotice::error(reason.into_owned())
                        },
                    );
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
                                native_continuity: Default::default(),
                                owned_codex_listener: None,
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
                            return self.insert_chat_card(fresh, Some(card), Some(card));
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
                        if self
                            .card_state(card)
                            .is_none_or(|state| !state.existence.is_open())
                        {
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
                        self.chat_ui.phases.insert(
                            target.card,
                            Phase::Returning {
                                target,
                                local: None,
                                close_requested: false,
                            },
                        );
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
            Action::UpgradeCommitted(target, ready, result) => {
                match result {
                    Ok(()) => {
                        if let Some(Phase::Starting(attempt)) =
                            self.chat_ui.phases.get_mut(&target.card)
                            && attempt.target == target
                        {
                            attempt.upgrade_committed = true;
                        }
                    }
                    Err(error) => {
                        if let Ok(mut reply) = ready.0.lock() {
                            *reply = Some(Err(chat::StartFailure::retry(format!(
                                "The upgraded conversation was not saved: {error}. Its saved identity is retained; retry to reconnect."
                            ))));
                        }
                    }
                }
                return self.update(Message::ChatUi(Action::Ready(target, ready)));
            }
            Action::AccountCommitted(target, ready, result) => {
                match result {
                    Ok(()) => {
                        if let Some(Phase::Starting(attempt)) =
                            self.chat_ui.phases.get_mut(&target.card)
                            && attempt.target == target
                            && let Some(recovery) = &mut attempt.recovery
                        {
                            recovery.committed = true;
                        }
                    }
                    Err(error) => {
                        if let Ok(mut reply) = ready.0.lock() {
                            *reply = Some(Err(chat::StartFailure::retry(format!(
                                "The account change was not saved: {error}. The original conversation is retained."
                            ))));
                        }
                    }
                }
                return self.update(Message::ChatUi(Action::Ready(target, ready)));
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
                let Some(Phase::Starting(mut attempt)) = self
                    .chat_ui
                    .finish(target, |phase| matches!(phase, Phase::Starting(_)))
                else {
                    return Task::none();
                };
                let recovery = attempt.recovery.take();
                let automatic_submit = recovery.as_ref().and_then(|recovery| recovery.submit);
                let account_committed =
                    recovery.as_ref().is_some_and(|recovery| recovery.committed);
                let account_source = recovery.map(|recovery| recovery.source);
                let upgrade_source = result
                    .as_ref()
                    .ok()
                    .and_then(|handle| handle.upgrade_recovery())
                    .map(|(source, _)| source);
                if !self.card_state(target.card).is_some_and(|card| {
                    card.existence.is_open()
                        && match &card.kind {
                            CardKind::Chat(session) => account_source
                                .as_ref()
                                .or(upgrade_source)
                                .map_or(session.id == attempt.session.id, |source| {
                                    session == source
                                }),
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
                        if self.shared_views.attached.contains(&target.card) {
                            // This was an attachment to a daemon-owned shared actor.
                            // A peer removing its pane only detaches this client.
                            drop(handle);
                            return Task::none();
                        }
                        return self.retire_chat(target.card, Some(handle), None);
                    }
                    return Task::none();
                }
                match result {
                    Ok(handle) => {
                        if let Some((source, resolved)) = handle.upgrade_recovery() {
                            let replacement = handle.session();
                            if account_source.is_some()
                                || source != &attempt.session
                                || &replacement != resolved
                                || !chat::is_upgrade_recovery(source, &replacement)
                                || handle.snapshot().session_id != replacement.launch.session_id
                            {
                                self.status = Some(TransientNotice::error(
                                    "The provider did not confirm the exact upgraded conversation. Its saved identity is retained.",
                                ));
                                return Task::none();
                            }
                            if !attempt.upgrade_committed {
                                let receipt = self
                                    .session
                                    .as_ref()
                                    .ok_or_else(|| "Session storage is unavailable".to_owned())
                                    .and_then(|writer| {
                                        writer.rebind_chat_upgrade(
                                            target.card,
                                            source.clone(),
                                            replacement,
                                        )
                                    });
                                self.chat_ui
                                    .phases
                                    .insert(target.card, Phase::Starting(attempt));
                                let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle)))));
                                return cosmic::task::future(async move {
                                    let result = match receipt {
                                        Ok(receipt) => tokio::task::spawn_blocking(move || {
                                            receipt
                                                .recv()
                                                .map_err(|error| {
                                                    format!("Session writer disconnected: {error}")
                                                })
                                                .and_then(|result| result)
                                        })
                                        .await
                                        .map_err(|error| error.to_string())
                                        .and_then(|result| result),
                                        Err(error) => Err(error),
                                    };
                                    Message::ChatUi(Action::UpgradeCommitted(target, ready, result))
                                });
                            }
                        }
                        if let Some(source) = &account_source {
                            let replacement = handle.session();
                            if !chat::is_account_recovery(source, &replacement)
                                || handle.snapshot().session_id != source.launch.session_id
                            {
                                self.status = Some(TransientNotice::error(
                                    "The provider did not confirm the saved conversation. Its original account and identity are retained.",
                                ));
                                return self.retire_chat(target.card, Some(handle), None);
                            }
                            if !account_committed {
                                attempt.recovery = Some(AccountRecovery {
                                    source: source.clone(),
                                    committed: false,
                                    submit: automatic_submit,
                                });
                                self.chat_ui
                                    .phases
                                    .insert(target.card, Phase::Starting(attempt));
                                let receipt = self
                                    .session
                                    .as_ref()
                                    .ok_or_else(|| "Session storage is unavailable".to_owned())
                                    .and_then(|writer| {
                                        writer.rebind_chat_account(
                                            target.card,
                                            source.clone(),
                                            replacement,
                                        )
                                    });
                                let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle)))));
                                return cosmic::task::future(async move {
                                    let result = match receipt {
                                        Ok(receipt) => tokio::task::spawn_blocking(move || {
                                            receipt
                                                .recv()
                                                .map_err(|error| {
                                                    format!("Session writer disconnected: {error}")
                                                })
                                                .and_then(|result| result)
                                        })
                                        .await
                                        .map_err(|error| error.to_string())
                                        .and_then(|result| result),
                                        Err(error) => Err(error),
                                    };
                                    Message::ChatUi(Action::AccountCommitted(target, ready, result))
                                });
                            }
                        }
                        self.chat_ui.fresh_offers.remove(&target.card);
                        self.chat_ui.removed_accounts.remove(&target.card);
                        let mut session = handle.session();
                        session.proof = handle.proof();
                        let snapshot = handle.snapshot();
                        let channel_name = snapshot_channel_name(&snapshot);
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
                            // Request generations belong to the old handle. A
                            // restarted daemon can begin again at generation 1.
                            runtime.page_request = None;
                            runtime.deferred_page = None;
                            runtime.earlier_asked = None;
                            runtime.later_asked = None;
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
                            runtime.tool_groups_expanded = self.settings.chat_expand_tool_groups;
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
                        if account_source.is_some() {
                            self.install_chat_session(target.card, session.clone());
                            self.persist();
                        } else {
                            self.commit_chat_session(target.card, session);
                        }
                        if let Some(title) = title
                            && self.apply_reported_title(target.card, title)
                        {
                            self.persist();
                        }
                        if let Some((name, source)) = channel_name {
                            self.suggest_channel_title(target.card, name, source);
                        }
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        // Reports that raced the attachment woke nothing.
                        let usage = self.take_chat_account_usage(target);
                        let control = match self.cards.get(&target.card) {
                            Some(CardRuntime::Chat(runtime)) => runtime.focus_control(),
                            _ => "chat-composer",
                        };
                        let submitted = automatic_submit
                            .and_then(|(revision, mode)| match self.cards.get(&target.card) {
                                Some(CardRuntime::Chat(runtime))
                                    if runtime.target == target && runtime.revision == revision =>
                                {
                                    Some(mode)
                                }
                                _ => None,
                            })
                            .map(|mode| self.handle_chat_ui(Action::Submit(target, mode)))
                            .unwrap_or_else(Task::none);
                        return Task::batch([
                            submitted,
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
                        draft.reset_owned_codex_listener();
                        draft.proof = None;
                        self.chat_ui.fresh_offers.remove(&target.card);
                        self.chat_forget(target.card);
                        self.replace_chat_conversation(target.card, draft.clone());
                        let skills = self.open_chat_draft(target.card, draft);
                        return Task::batch([
                            skills,
                            self.retire_chat(target.card, None, Some(attempt.session)),
                        ]);
                    }
                    Err(failure) => {
                        let terminal_switch = matches!(
                            self.card_state(target.card).map(|state| &state.kind),
                            Some(CardKind::Terminal(_))
                        );
                        let account_removed =
                            failure.recovery == chat::Recovery::Account || account_source.is_some();
                        if account_removed {
                            self.chat_ui.removed_accounts.insert(target.card);
                        }
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
                        let cleanup = if account_source.is_some() {
                            self.retire_chat(target.card, None, Some(attempt.session))
                        } else {
                            Task::none()
                        };
                        return Task::batch([
                            self.drain_terminal(target.card, 0),
                            cleanup,
                            if terminal_switch {
                                cosmic::task::message(Message::ChromeUi(
                                    chrome_ui::Action::ShowNoticesFor(target.card),
                                ))
                            } else {
                                Task::none()
                            },
                            if account_removed {
                                self.reload_registry()
                            } else {
                                Task::none()
                            },
                        ]);
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
                let Some(Phase::Returning {
                    local:
                        Some(LocalReturn {
                            session,
                            launch,
                            resumes,
                        }),
                    close_requested: close,
                    ..
                }) = self.chat_ui.finish(target, |phase| {
                    matches!(phase, Phase::Returning { local: Some(_), .. })
                })
                else {
                    return Task::none();
                };
                let card = target.card;
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
                    return Task::batch([
                        retry,
                        self.remove_chat_card(card, None).unwrap_or_else(Task::none),
                    ]);
                }
                // The terminal takes the chat's place in its strip, so it can
                // only be placed while that strip is the one on screen.
                if !self.workspace.order().any(|id| id == card) {
                    self.status = Some(TransientNotice::warning(
                        "The chat has stopped. Return to it and switch again to open its terminal.",
                    ));
                    return retry;
                }
                let removal = self
                    .remove_chat_card(card, Some(launch))
                    .unwrap_or_else(Task::none);
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
                            snapshot_channel_name(&handle.snapshot())
                        }
                        _ => None,
                    };
                    if let Wake::Disconnected(error) = wake {
                        runtime.disconnected = true;
                        runtime.error = Some(error);
                    }
                    if let Some((name, source)) = name {
                        self.suggest_channel_title(target.card, name, source);
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
                runtime.refresh_session();
                match wake {
                    Wake::Changed => {
                        runtime.refresh();
                        let fork = if runtime.fork_requested
                            && runtime.snapshot.forked != runtime.fork_seen
                        {
                            runtime.fork_requested = false;
                            runtime.fork_seen = runtime.snapshot.forked.clone();
                            runtime.snapshot.forked.as_ref().and_then(|id| {
                                let mut fork = runtime.session.clone();
                                fork.reset_owned_codex_listener();
                                fork.id = TerminalId::new();
                                fork.proof = None;
                                fork.terminal = None;
                                fork.launch.session_id = Some(id.clone());
                                fork.launch.recipe = fork.launch.terminal_recipe()?;
                                Some(fork)
                            })
                        } else {
                            None
                        };
                        let activity = Activity {
                            provider: runtime.session.launch.provider,
                            state: handle.take_activity(),
                        };
                        let session = runtime.session.clone();
                        let snapshot = handle.snapshot();
                        let channel_name = needs_name
                            .then(|| snapshot_channel_name(&snapshot))
                            .flatten();
                        let title = snapshot.title.clone();
                        if let Some((name, source)) = channel_name {
                            self.suggest_channel_title(target.card, name, source);
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
                            self.resume_history_navigation(target),
                            fork.map_or_else(Task::none, |session| {
                                let scope = if let Some(channel) = self
                                    .durable
                                    .channels
                                    .iter()
                                    .find(|channel| channel.cards.contains(&target.card))
                                    .map(|channel| channel.id)
                                {
                                    self.select_channel(channel)
                                } else {
                                    self.select_worktree(
                                        session.launch.recipe.working_directory.clone(),
                                        false,
                                    )
                                };
                                let insert =
                                    self.insert_chat_card(session, Some(target.card), None);
                                Task::batch([scope, insert])
                            }),
                        ]);
                    }
                    Wake::Observed => {
                        handle.take_observed();
                        let session = runtime.session.clone();
                        self.commit_chat_session(target.card, session);
                        return Task::none();
                    }
                    Wake::Activity => {
                        let activity = Activity {
                            provider: runtime.session.launch.provider,
                            state: handle.take_activity(),
                        };
                        let session = runtime.session.clone();
                        self.commit_chat_session(target.card, session);
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
                if !self.chat_offers_fresh_conversation(card)
                    || self
                        .card_state(card)
                        .is_none_or(|state| !state.existence.is_open())
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
                    self.chat_ui.phases.insert(
                        card,
                        Phase::Closing {
                            target,
                            restart: Some(session),
                        },
                    );
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
                let mut files = None;
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if action.is_edit() {
                        runtime.revision = runtime.revision.wrapping_add(1);
                        // Typing into a recalled message makes it the draft,
                        // so the next Up starts again from the newest.
                        runtime.history_cursor = None;
                        runtime.history_stash = None;
                    }
                    // The completion rows follow the word at the caret, so
                    // anything that can change that word starts them afresh.
                    let rescroll = !matches!(action, text_editor::Action::Scroll { .. })
                        && runtime.completion.restart(action.is_edit());
                    runtime.composer.perform(action);
                    if runtime.files.is_none() && runtime.composer.text().contains('@') {
                        runtime.files = Some(Arc::new(Vec::new()));
                        files = Some(runtime.session.launch.recipe.working_directory.clone());
                    }
                    if rescroll {
                        return Task::batch([
                            view::completion_scroll(target, 0.0),
                            files.map_or_else(Task::none, |directory| {
                                self.load_chat_files(target, directory)
                            }),
                        ]);
                    }
                }
                if let Some(directory) = files {
                    return self.load_chat_files(target, directory);
                }
            }
            Action::Paste(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                runtime.pastes_pending += 1;
                return iced::clipboard::read_data::<clipboard_image::Image>().map(move |image| {
                    cosmic::Action::App(Message::ChatUi(Action::PastedImage(target, image)))
                });
            }
            Action::PastedImage(target, Some(image)) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                // Many apps offer text alongside an image; where the chat
                // cannot take the image, the text is pasted instead, and the
                // refusal is shown only when there is no text. The paste
                // stays pending until that text read resolves.
                let rules = chat::AttachmentRules::for_provider(runtime.session.launch.provider);
                let refusal = if !runtime.snapshot.capabilities.attachments {
                    Some("The current model cannot accept images.")
                } else if rules.max_count.is_some_and(|limit| {
                    runtime.attachments.len() + runtime.image_pastes_saving >= limit
                }) {
                    Some("This chat has reached its attachment limit.")
                } else {
                    None
                };
                if let Some(refusal) = refusal {
                    return iced::clipboard::read().map(move |text| {
                        cosmic::Action::App(Message::ChatUi(match text {
                            Some(text) => Action::PastedText(target, Some(text)),
                            None => Action::PasteRefused(target, refusal),
                        }))
                    });
                }
                runtime.image_pastes_saving += 1;
                return cosmic::task::future(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        clipboard_image::save(image, rules.max_image_bytes)
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                    Message::ChatUi(Action::ImageSaved(target, result))
                });
            }
            Action::PastedImage(target, None) => {
                return iced::clipboard::read().map(move |text| {
                    cosmic::Action::App(Message::ChatUi(Action::PastedText(target, text)))
                });
            }
            Action::PastedText(target, Some(text)) => {
                let submit = if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.pastes_pending = runtime.pastes_pending.saturating_sub(1);
                    (runtime.pastes_pending == 0)
                        .then(|| runtime.submit_after_paste.take())
                        .flatten()
                } else {
                    None
                };
                let edited = self.handle_chat_ui(Action::Composer(
                    target,
                    text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(text))),
                ));
                return Task::batch(
                    std::iter::once(edited).chain(
                        submit.map(|mode| self.handle_chat_ui(Action::Submit(target, mode))),
                    ),
                );
            }
            Action::PastedText(target, None) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.pastes_pending = runtime.pastes_pending.saturating_sub(1);
                    if runtime.pastes_pending == 0
                        && let Some(mode) = runtime.submit_after_paste.take()
                    {
                        return self.handle_chat_ui(Action::Submit(target, mode));
                    }
                }
            }
            Action::PasteRefused(target, refusal) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.pastes_pending = runtime.pastes_pending.saturating_sub(1);
                    runtime.submit_after_paste = None;
                    runtime.error = Some(refusal.into());
                }
            }
            Action::ImageSaved(target, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.pastes_pending = runtime.pastes_pending.saturating_sub(1);
                    runtime.image_pastes_saving = runtime.image_pastes_saving.saturating_sub(1);
                    let rules =
                        chat::AttachmentRules::for_provider(runtime.session.launch.provider);
                    let success = match result {
                        Ok(path)
                            if runtime.snapshot.capabilities.attachments
                                && rules
                                    .max_count
                                    .is_none_or(|limit| runtime.attachments.len() < limit) =>
                        {
                            runtime.attach([path]);
                            true
                        }
                        Ok(path) => {
                            let _ = std::fs::remove_file(path);
                            runtime.error = Some(
                                if runtime.snapshot.capabilities.attachments {
                                    "This chat has reached its attachment limit."
                                } else {
                                    "The current model cannot accept images."
                                }
                                .into(),
                            );
                            false
                        }
                        Err(error) => {
                            runtime.error = Some(error);
                            false
                        }
                    };
                    if !success {
                        runtime.submit_after_paste = None;
                    } else if runtime.pastes_pending == 0
                        && let Some(mode) = runtime.submit_after_paste.take()
                    {
                        return self.handle_chat_ui(Action::Submit(target, mode));
                    }
                } else if let Ok(path) = result {
                    let _ = std::fs::remove_file(path);
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
                if let Some((account, generation, revision)) =
                    self.automatic_chat_account(target, mode)
                {
                    if generation.is_none() {
                        let task = self.choose_chat_account(target, account.id());
                        if let Some(runtime) = self.chat_runtime_mut(target) {
                            runtime
                                .session
                                .launch
                                .recipe
                                .environment
                                .insert(account::routing::AUTOMATIC_MARKER.into(), "1".into());
                            let session = runtime.session.clone();
                            self.replace_chat_conversation(target.card, session);
                        }
                        return Task::batch([
                            task,
                            self.handle_chat_ui(Action::Submit(target, mode)),
                        ]);
                    }
                    return self.change_chat_account(
                        target.card,
                        account.id(),
                        generation,
                        Some((revision, mode)),
                    );
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if matches!(
                    slash_invocation(&runtime.composer.text(), &runtime.snapshot),
                    Some(SlashInvocation::PermissionsArguments)
                ) {
                    runtime.error = Some(
                        "Use /permissions without arguments to open the permissions panel.".into(),
                    );
                    return Task::none();
                }
                if matches!(
                    slash_invocation(&runtime.composer.text(), &runtime.snapshot),
                    Some(SlashInvocation::Permissions)
                ) {
                    if runtime.pastes_pending > 0 {
                        runtime.error = Some(
                            "Wait for the paste to finish, or use the permissions button.".into(),
                        );
                        return Task::none();
                    }
                    runtime.replace_draft(text_editor::Content::new());
                    runtime.error = None;
                    return self.handle_chat_ui(Action::PermissionPicker(target, true));
                }
                if runtime.pastes_pending > 0 {
                    runtime.submit_after_paste = Some(mode);
                    return Task::none();
                }
                if runtime.composer.text().trim().is_empty() && runtime.attachments.is_empty() {
                    return Task::none();
                }
                // Resuming is the composer's own command, answered here in
                // every state the conversation can be in.
                if let Some(query) = resume_invocation(&runtime.composer.text()) {
                    let Some(task) = self.open_resume_from(target.card, query) else {
                        return Task::none();
                    };
                    // The command is spent once the picker answers it; a draft
                    // it leaves untouched gives its place to what is resumed.
                    if let Some(runtime) = self.chat_runtime_mut(target) {
                        runtime.replace_draft(text_editor::Content::new());
                        runtime.error = None;
                    }
                    self.resume_ui.replace =
                        self.chat_is_unused(target.card).then_some(target.card);
                    return task;
                }
                if runtime.is_draft() {
                    return self.start_chat_draft(target);
                }
                if (!runtime.followups.is_empty() && mode != PromptMode::Steer
                    || mode == PromptMode::Queue
                        && !runtime.snapshot.capabilities.queue
                        && runtime.snapshot.busy)
                    && runtime.pending.is_none()
                    && !runtime.disconnected
                    && !runtime.snapshot.exited
                {
                    // Held follow-ups go out as one prompt, so together they
                    // have to fit what one prompt may carry.
                    let (_, held) = runtime.combined_followups();
                    let count = held.len()
                        + runtime
                            .attachments
                            .iter()
                            .filter(|attachment| {
                                !held.iter().any(|old| old.path == attachment.path)
                            })
                            .count();
                    if chat::AttachmentRules::for_provider(runtime.session.launch.provider)
                        .max_count
                        .is_some_and(|limit| count > limit)
                    {
                        runtime.error = Some(
                            "The combined queued message exceeds this chat's attachment limit."
                                .into(),
                        );
                        return Task::none();
                    }
                    let id = runtime.next_followup;
                    runtime.next_followup += 1;
                    let followup = Followup {
                        id,
                        text: runtime.composer.text().trim_end().to_owned(),
                        attachments: std::mem::take(&mut runtime.attachments),
                    };
                    runtime.followups.push(followup);
                    runtime.replace_draft(text_editor::Content::new());
                    runtime.error = None;
                    let _ = runtime.present();
                    return if runtime.snapshot.busy {
                        Task::none()
                    } else {
                        self.dispatch_queued(target)
                    };
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
            Action::TakeBackQueued(target, entry) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(entry) = entry.or_else(|| runtime.newest_take_back()) else {
                    return Task::none();
                };
                let focus =
                    iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
                        .map(cosmic::Action::App);
                match entry {
                    QueueEntry::Local(id) => {
                        if runtime.followups_sending() {
                            return Task::none();
                        }
                        let Some(index) = runtime
                            .followups
                            .iter()
                            .position(|followup| followup.id == id)
                        else {
                            return Task::none();
                        };
                        let followup = runtime.followups.remove(index);
                        runtime.take_back(&followup.text, followup.attachments, PromptMode::Queue);
                        let _ = runtime.present();
                        return focus;
                    }
                    QueueEntry::Provider(id) => {
                        let Some(message) = runtime
                            .snapshot
                            .queued_messages
                            .iter()
                            .find(|message| message.id == id)
                            .filter(|message| runtime.provider_take_back(message))
                            .cloned()
                        else {
                            return Task::none();
                        };
                        // The message returns to the composer only once the
                        // provider has let go of it: one it has already begun
                        // on stays the turn it became.
                        let answered = runtime
                            .snapshot
                            .queue_removals
                            .iter()
                            .filter(|removal| removal.id == id)
                            .count();
                        let sent = runtime.next_command;
                        let task =
                            self.chat_send(target, Command::RemoveQueued { id: id.clone() }, false);
                        if let Some(runtime) = self.chat_runtime_mut(target)
                            && runtime.next_command != sent
                            && let Some(pending) = &mut runtime.pending
                        {
                            pending.take_back = Some(id);
                            runtime.recalling.push(Recall { message, answered });
                        }
                        return Task::batch([task, focus]);
                    }
                }
            }
            Action::RemoveQueued(target, entry) => match entry {
                QueueEntry::Local(id) => {
                    if let Some(runtime) = self.chat_runtime_mut(target)
                        && !runtime.followups_sending()
                    {
                        runtime.followups.retain(|followup| followup.id != id);
                        let _ = runtime.present();
                    }
                }
                QueueEntry::Provider(id) => {
                    return self.chat_send(target, Command::RemoveQueued { id }, false);
                }
            },
            Action::HistoryOlder(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.recall(true);
                }
            }
            Action::PermissionPicker(target, open) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.picker_keyboard = false;
                    runtime.permission_picker = open;
                    if open {
                        runtime.model_picker = None;
                        runtime.reasoning_picker = false;
                        runtime.account_picker = false;
                        runtime.permissions_form.page = permissions::Page::Blocked;
                        if runtime.session.launch.provider == Provider::Claude
                            && !runtime.is_draft()
                        {
                            return self.chat_send(target, Command::RefreshPermissions, false);
                        }
                    }
                }
            }
            Action::PermissionPage(target, page) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.permissions_form.page = page;
                }
            }
            Action::PermissionRuleText(target, text) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.permissions_form.rule = text;
                }
            }
            Action::PermissionBehavior(target, behavior) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.permissions_form.behavior = behavior;
                }
            }
            Action::PermissionEnvironmentText(target, text) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.permissions_form.environment = text;
                }
            }
            Action::PermissionSetting(target, update) => {
                return self.chat_send(target, Command::UpdatePermissionSettings { update }, false);
            }
            Action::PermissionRetry(target, id) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if let Some(denial) = runtime
                    .snapshot
                    .permission_denials
                    .iter()
                    .find(|denial| denial.tool_use_id == id)
                    && let Some(retry) = permissions::retry_draft(denial)
                {
                    let mut draft = runtime.composer.text();
                    if !draft.trim().is_empty() {
                        draft.push_str("\n\n");
                    }
                    draft.push_str(&retry);
                    runtime.replace_draft(text_editor::Content::with_text(&draft));
                    runtime.permission_picker = false;
                    return iced::widget::operation::focus(card_control_id(
                        target.card,
                        "chat-composer",
                    ));
                }
            }
            Action::PermissionDismiss(target, tool_use_id) => {
                return self.chat_send(
                    target,
                    Command::DismissPermissionDenial { tool_use_id },
                    false,
                );
            }
            Action::Files(target, directory, files) => {
                self.chat_ui.files.insert(directory.clone(), files.clone());
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime.session.launch.recipe.working_directory == directory
                {
                    runtime.files = Some(files);
                }
            }
            Action::Find(target, query) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.find = Some(query);
                    runtime.refresh_find();
                    runtime.find_index = runtime.find_matches.len().saturating_sub(1);
                }
                return self.chat_find_next(target, true);
            }
            Action::FindNext(target, forward) => return self.chat_find_next(target, forward),
            Action::CloseFind(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.find = None;
                    runtime.find_focused = false;
                    runtime.find_matches.clear();
                }
                return iced::widget::operation::focus(card_control_id(
                    target.card,
                    "chat-composer",
                ))
                .map(cosmic::Action::App);
            }
            Action::FindFocused(target, focused) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.find_focused = focused;
                }
            }
            Action::HistoryNewer(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.recall(false);
                }
            }
            Action::Command(target, command) => {
                if command == Command::LoadEarlier {
                    if let Some(runtime) = self.chat_runtime_mut(target) {
                        if runtime.selected_subagent.is_some() {
                            return Task::none();
                        }
                        if runtime
                            .pending
                            .as_ref()
                            .is_some_and(|pending| pending.page.is_none())
                            && runtime.deferred_page != Some(HistoryNavigation::Latest)
                        {
                            runtime.deferred_page = Some(HistoryNavigation::EarlierExplicit);
                            return Task::none();
                        }
                    }
                }
                if matches!(command, Command::Branch { .. })
                    && let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime.pending.is_none()
                    && !runtime.snapshot.busy
                {
                    runtime.branch_picker = false;
                    runtime.fork_requested = true;
                    runtime.fork_seen = runtime.snapshot.forked.clone();
                }
                return self.chat_send(target, command, false);
            }
            Action::BranchPicker(target, open) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.branch_picker = open;
                }
                if open {
                    return self.chat_send(target, Command::ListBranches, false);
                }
            }
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
                        if pending.followup {
                            runtime.followups.clear();
                            let _ = runtime.present();
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
                        if pending.page == Some(view::Page::Newer)
                            && runtime.deferred_page == Some(HistoryNavigation::Latest)
                        {
                            runtime.deferred_page = None;
                        }
                        if pending.page == Some(view::Page::Newer)
                            && runtime.selected_subagent.is_none()
                        {
                            let snap = iced::widget::scrollable::snap_to(
                                card_control_id(target.card, "chat-transcript"),
                                iced::widget::scrollable::RelativeOffset {
                                    x: None,
                                    y: Some(0.0),
                                },
                            )
                            .map(cosmic::Action::App);
                            return Task::batch([
                                snap,
                                self.dispatch_queued(target),
                                self.resume_history_navigation(target),
                            ]);
                        }
                        pending.first_prompt
                    }
                    Err(error) => {
                        if pending.page.is_some() {
                            runtime.page_request = None;
                            runtime.earlier_asked = None;
                            runtime.later_asked = None;
                        }
                        if pending.followup {
                            runtime.restore_followups();
                        }
                        if let Some(id) = &pending.take_back {
                            runtime.recalling.retain(|recall| &recall.message.id != id);
                        }
                        runtime.note(error.clone());
                        runtime.fork_requested = false;
                        runtime.error = Some(error);
                        None
                    }
                };
                if let Some(prompt) = first_prompt {
                    self.name_channel_from_prompt(target.card, &prompt);
                }
                // Idle may have arrived while this command was awaiting its
                // acknowledgement. No later provider event is guaranteed to
                // wake the follow-up again once the command releases it.
                return Task::batch([
                    self.dispatch_queued(target),
                    self.resume_history_navigation(target),
                ]);
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
                    // Invert the visible default on the first toggle, then
                    // remember explicit choices across paging. Ignore stale keys.
                    if runtime.turns.iter().any(|turn| turn.key == key) {
                        let open = runtime
                            .opened
                            .entry(key)
                            .or_insert(runtime.tool_groups_expanded);
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
                    runtime.deferred_page = None;
                }
            }
            Action::Copy(target, id, part) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                // The body itself lives in the snapshot; only the extracted
                // code blocks are the cache's own.
                let text = match part {
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
                    let at = Instant::now();
                    runtime.copied = Some((id, at));
                    return Task::batch([
                        iced::clipboard::write(text).map(cosmic::Action::App),
                        cosmic::task::future(async move {
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            Message::ChatUi(Action::ClearCopied(target, at))
                        }),
                    ]);
                }
            }
            Action::OpenUrl(raw) => match Url::parse(&raw) {
                Ok(url) if matches!(url.scheme(), "https" | "http" | "file") => {
                    return self.open_clicked_link(url);
                }
                _ => {
                    self.status = Some(TransientNotice::error(
                        "Only explicit HTTP, HTTPS, and local file links can be opened from chat.",
                    ))
                }
            },
            Action::OpenDocument(target, raw) => {
                if self.chat_runtime_mut(target).is_some() {
                    return self.open_document_link(Some(target.card), &raw);
                }
            }
            Action::OpenImage(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    let gallery = images::Gallery::new(runtime, &id);
                    self.chat_ui.image_viewer = gallery;
                }
            }
            Action::CloseImage => self.chat_ui.image_viewer = None,
            Action::ImageNext(forward) => {
                if let Some(gallery) = &mut self.chat_ui.image_viewer {
                    gallery.next(forward);
                }
            }
            Action::ImageFit => {
                if let Some(gallery) = &mut self.chat_ui.image_viewer {
                    gallery.reset = gallery.reset.wrapping_add(1);
                }
            }
            Action::ImageExternal => {
                if let Some(url) = self
                    .chat_ui
                    .image_viewer
                    .as_ref()
                    .and_then(|gallery| gallery.url())
                {
                    return cosmic::task::future(async move {
                        Message::ExternalLinkOpened(open_system_url(url).await)
                    });
                }
            }
            Action::CopyLiteral(text) => {
                self.status = Some(TransientNotice::success("Copied code block"));
                return iced::clipboard::write(text).map(cosmic::Action::App);
            }
            Action::Tick => {}
            Action::DropHover(target, hovering) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.drop_hover = hovering;
                }
            }
            Action::ClearCopied(target, at) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime
                        .copied
                        .as_ref()
                        .is_some_and(|(_, copied)| *copied == at)
                {
                    runtime.copied = None;
                }
            }
            Action::Latest(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if runtime.selected_subagent.is_none() {
                        if runtime.pending.is_some() {
                            if !runtime
                                .pending
                                .as_ref()
                                .is_some_and(|pending| pending.page == Some(view::Page::Newer))
                            {
                                runtime.deferred_page = Some(HistoryNavigation::Latest);
                            }
                            return Task::none();
                        }
                        if runtime.snapshot.has_later
                            || runtime.snapshot.history_loading
                            || runtime
                                .page_request
                                .is_some_and(|(page, _)| page == view::Page::Earlier)
                        {
                            return self.chat_send(target, Command::Refresh, false);
                        }
                        runtime.unseen_activity = false;
                    }
                }
                return iced::widget::scrollable::snap_to(
                    card_control_id(target.card, "chat-transcript"),
                    iced::widget::scrollable::RelativeOffset {
                        x: None,
                        y: Some(0.0),
                    },
                )
                .map(cosmic::Action::App);
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
                        Ok(Some(paths)) => runtime.attach(paths),
                        Ok(None) => {}
                        Err(error) => runtime.error = Some(error),
                    }
                }
            }
            Action::Dropped(target, payload) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.drop_hover = false;
                }
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
                    runtime.history_stash = None;
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
                runtime.permission_picker = false;
                if !runtime
                    .snapshot
                    .capabilities
                    .permission_modes
                    .contains(&mode)
                {
                    return Task::none();
                }
                if runtime.is_draft() {
                    if !runtime.choosing() {
                        return Task::none();
                    }
                    let provider = runtime.session.launch.provider;
                    set_draft_permission_mode(&mut runtime.session.launch.recipe, provider, &mode);
                    Arc::make_mut(&mut runtime.snapshot).permission_mode = Some(mode);
                    let session = runtime.session.clone();
                    self.replace_chat_conversation(target.card, session);
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
                let Some(Phase::Returning {
                    local: None,
                    close_requested: close,
                    ..
                }) = self.chat_ui.finish(target, |phase| {
                    matches!(phase, Phase::Returning { local: None, .. })
                })
                else {
                    return Task::none();
                };
                // A close asked for during the return runs on whatever the
                // card turned out to be: the terminal it returned to, or the
                // chat that is still there when the return failed.
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
            Action::RetainedCloseFinished(target, session, result) => {
                if self
                    .chat_ui
                    .finish(target, |phase| {
                        matches!(phase, Phase::Closing { restart: None, .. })
                    })
                    .is_none()
                {
                    return Task::none();
                }
                if self.chat_return_session(target.card).as_ref() != Some(&session) {
                    return Task::none();
                }
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(error));
                    return Task::none();
                }
                return self
                    .remove_chat_card(target.card, None)
                    .unwrap_or_else(Task::none);
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
                let Some(Phase::Closing { restart, .. }) = self
                    .chat_ui
                    .finish(target, |phase| matches!(phase, Phase::Closing { .. }))
                else {
                    return Task::none();
                };
                if restart.is_none() {
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
                    self.status = Some(TransientNotice::error(error));
                    self.reread_chat_activity(target);
                    return Task::none();
                }
                if self.refuse_for_deleting_card(target.card) {
                    // The worktree was confirmed for deletion while this
                    // restart was closing the old conversation; its
                    // replacement would start in a directory being removed.
                    // The card keeps the closed conversation instead.
                    if let Some(runtime) = self.chat_runtime_mut(target) {
                        runtime.disconnected = true;
                        runtime.error =
                            Some("Restart cancelled: the worktree was being deleted.".into());
                    }
                    self.observe_agent_activity_for_card(target.card, None);
                    return Task::none();
                }
                if let Some(session) = restart {
                    // Only the conversation is being given up here; the card
                    // stays and immediately starts its replacement, so it is
                    // never left showing a conversation that no longer exists.
                    self.chat_forget(target.card);
                    self.cards.insert(target.card, CardRuntime::Loading);
                    self.replace_chat_conversation(target.card, session.clone());
                    return self.start_chat(target.card, session, chat::Intent::Start, None, false);
                }
            }
            Action::Scrolled(target, subagent, scroll, user_input) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.selected_subagent != subagent {
                    return Task::none();
                }
                if let Some(id) = subagent {
                    runtime.subagent_scroll.insert(id, scroll);
                    return Task::none();
                }
                runtime.scroll = scroll;
                let (at_top, at_bottom) = (scroll.at_top(), scroll.at_bottom());
                if at_bottom && !runtime.snapshot.has_later {
                    runtime.unseen_activity = false;
                }
                // Leaving an end is what arms the page at it, so a reader who
                // scrolls away and comes back asks again, while one who stays
                // put asks once.
                if !at_top {
                    if runtime.deferred_page == Some(HistoryNavigation::EarlierEdge) {
                        runtime.deferred_page = None;
                    }
                    runtime.earlier_asked = None;
                }
                if !at_bottom {
                    if runtime.deferred_page == Some(HistoryNavigation::LatestEdge) {
                        runtime.deferred_page = None;
                    }
                    runtime.later_asked = None;
                }
                let height = Some(scroll.content());
                // A short page has no scroll direction. Its explicit history
                // controls choose the end without triggering a paging loop.
                if user_input
                    && at_top
                    && !at_bottom
                    && runtime.snapshot.has_earlier
                    && runtime.earlier_asked != height
                {
                    if runtime.pending.is_some() {
                        if runtime
                            .pending
                            .as_ref()
                            .is_some_and(|pending| pending.page.is_none())
                            && runtime
                                .deferred_page
                                .is_none_or(|page| page == HistoryNavigation::EarlierEdge)
                        {
                            runtime.deferred_page = Some(HistoryNavigation::EarlierEdge);
                        }
                        return Task::none();
                    }
                    return self.chat_send(target, Command::LoadEarlier, false);
                }
                if user_input
                    && at_bottom
                    && !at_top
                    && runtime.snapshot.has_later
                    && runtime.later_asked != height
                {
                    if runtime.pending.is_some() {
                        if runtime.deferred_page.is_none() {
                            runtime.deferred_page = Some(HistoryNavigation::LatestEdge);
                        }
                        return Task::none();
                    }
                    return self.chat_send(target, Command::Refresh, false);
                }
            }
            Action::ModelPicker(target, open) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                runtime.picker_keyboard = false;
                // A reopened search starts empty: the query a reader typed
                // last time is not the model they are looking for now.
                runtime.model_picker = open.then(String::new);
                runtime.reasoning_picker = false;
                runtime.account_picker = false;
                runtime.permission_picker = false;
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
                    runtime.permission_picker = false;
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
        .attempt(card)
        .expect("switch staged an attempt")
        .target
}

#[cfg(test)]
pub(super) fn ready_message_for_pending_switch(
    app: &Cagents,
    card: CardId,
    snapshot: chat::Snapshot,
) -> Message {
    let attempt = app.chat_ui.attempt(card).expect("switch staged an attempt");
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
    #[test]
    fn native_permissions_opens_locally_even_while_busy_and_preserves_attachments() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("permissions"));
        let runtime = match app.cards.get_mut(&card).unwrap() {
            CardRuntime::Chat(runtime) => runtime,
            _ => unreachable!(),
        };
        let snapshot = Arc::make_mut(&mut runtime.snapshot);
        snapshot.busy = true;
        snapshot.capabilities.permission_modes = vec!["manual".into(), "auto".into()];
        snapshot.commands.push(chat::SlashCommand {
            name: "permissions".into(),
            description: "Provider dialog".into(),
        });
        runtime.replace_draft(text_editor::Content::with_text("/PERMISSIONS"));
        runtime.attachments.push(Attachment {
            path: "retained.png".into(),
        });
        let target = runtime.target;
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.permission_picker);
        assert!(runtime.composer.text().trim().is_empty());
        assert_eq!(runtime.attachments[0].path, PathBuf::from("retained.png"));
        assert!(runtime.followups.is_empty());
        assert!(runtime.pending.is_none());
        assert!(app.dismiss_chat_picker());
        assert!(!chat_runtime(&app, card).permission_picker);
        assert!(
            submission(
                "/permissions allow Bash(*)",
                &chat_runtime(&app, card).snapshot,
                &[],
                PromptMode::Steer
            )
            .is_err()
        );
    }

    #[test]
    fn permission_retry_preserves_the_draft_and_requires_a_current_exact_action() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("permissions"));
        let runtime = match app.cards.get_mut(&card).unwrap() {
            CardRuntime::Chat(runtime) => runtime,
            _ => unreachable!(),
        };
        Arc::make_mut(&mut runtime.snapshot)
            .permission_denials
            .push(chat::PermissionDenial {
                tool_use_id: "blocked".into(),
                tool_name: "Bash".into(),
                tool_input: Some("{\"command\":\"git push origin topic\"}".into()),
                reason: "Auto denied".into(),
                reason_type: Some("classifier".into()),
                agent_id: None,
            });
        runtime.replace_draft(text_editor::Content::with_text("Keep this next task"));
        runtime.attachments.push(Attachment {
            path: "retained.png".into(),
        });
        let target = runtime.target;
        drop(app.handle_chat_ui(Action::PermissionRetry(target, "stale".into())));
        assert_eq!(
            chat_runtime(&app, card).composer.text().trim(),
            "Keep this next task"
        );
        drop(app.handle_chat_ui(Action::PermissionRetry(target, "blocked".into())));
        let runtime = chat_runtime(&app, card);
        let draft = runtime.composer.text();
        assert!(draft.starts_with("Keep this next task\n\nI authorize"));
        assert!(draft.contains("git push origin topic"));
        assert_eq!(runtime.attachments.len(), 1);
        assert!(runtime.pending.is_none());
        assert_eq!(runtime.snapshot.permission_denials.len(), 1);
    }

    #[test]
    fn image_gallery_is_modal_and_reuses_the_transcript_handle() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let runtime = match app.cards.get_mut(&card).unwrap() {
            CardRuntime::Chat(runtime) => runtime,
            _ => unreachable!(),
        };
        let content = ItemContent::Image {
            path: "/fixture/image.png".into(),
            description: "Generated image".into(),
        };
        let mut cached = CachedItem::new(&content, runtime.syntax_theme);
        cached.image = Some(iced::widget::image::Handle::from_rgba(
            1,
            1,
            vec![0, 0, 0, 255],
        ));
        runtime.cache.insert("image".into(), cached);
        Arc::make_mut(&mut runtime.snapshot).items = vec![
            chat::Item {
                id: "image".into(),
                role: chat::Role::Assistant,
                content,
                started_at: None,
                at: None,
            }
            .into(),
        ];
        let target = runtime.target;
        drop(app.handle_chat_ui(Action::OpenImage(target, "image".into())));
        assert!(app.chat_ui.image_viewer.is_some());
        assert!(app.modal_is_open());
        assert!(app.dialog().is_some());
        let before = app.context_inspector_open;
        drop(app.update(Message::ToggleContextInspector));
        assert_eq!(app.context_inspector_open, before);
        assert!(Action::ImageNext(true).allowed_while_modal());
        assert!(!Action::OpenImage(target, "image".into()).allowed_while_modal());
        drop(app.update(Message::ChatUi(Action::CloseImage)));
        assert!(!app.modal_is_open());
    }
    fn followup(id: u64, text: &str, paths: &[&str]) -> Followup {
        Followup {
            id,
            text: text.into(),
            attachments: paths
                .iter()
                .map(|path| Attachment { path: path.into() })
                .collect(),
        }
    }

    fn paths(attachments: &[Attachment]) -> Vec<&Path> {
        attachments
            .iter()
            .map(|attachment| attachment.path.as_path())
            .collect()
    }

    #[test]
    fn a_busy_provider_without_native_queue_keeps_a_followup_editable() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("followup"));
        let runtime = match app.cards.get_mut(&card).unwrap() {
            CardRuntime::Chat(runtime) => runtime,
            _ => unreachable!(),
        };
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
        runtime.replace_draft(text_editor::Content::with_text("next task"));
        let target = runtime.target;
        let _ = app.handle_chat_ui(Action::Submit(target, PromptMode::Queue));
        assert_eq!(chat_runtime(&app, card).followups[0].text, "next task");
        assert!(chat_runtime(&app, card).composer.text().trim().is_empty());
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("later draft"));
        let _ = app.handle_chat_ui(Action::TakeBackQueued(target, None));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.followups.is_empty());
        assert_eq!(runtime.composer.text().trim(), "next task\nlater draft");
        // The caret ends the message that came back, where revising it goes on.
        let caret = runtime.composer.cursor().position;
        assert_eq!((caret.line, caret.column), (0, "next task".len()));
    }

    #[test]
    fn local_followups_stay_separate_and_are_taken_back_or_removed_one_at_a_time() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("followup"));
        let runtime = app
            .chat_runtime_mut(chat_runtime(&app, card).target)
            .unwrap();
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
        let target = runtime.target;
        for (text, paths) in [
            ("first task", vec!["first.png"]),
            ("second task", vec!["first.png", "second.png"]),
            ("", vec!["third.png"]),
        ] {
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.replace_draft(text_editor::Content::with_text(text));
            runtime.attach(paths.into_iter().map(PathBuf::from));
            drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
        }
        let runtime = chat_runtime(&app, card);
        assert_eq!(
            runtime
                .followups
                .iter()
                .map(|followup| followup.text.as_str())
                .collect::<Vec<_>>(),
            ["first task", "second task", ""]
        );
        // They go to the provider as one prompt carrying each file once.
        let (text, attachments) = runtime.combined_followups();
        assert_eq!(text, "first task\nsecond task");
        assert_eq!(
            paths(&attachments),
            [
                Path::new("first.png"),
                Path::new("second.png"),
                Path::new("third.png")
            ]
        );
        assert!(runtime.pending.is_none());
        assert!(runtime.composer.text().trim().is_empty());
        assert!(runtime.transition_blocker().is_some());
        assert!(runtime.handoff_blocker().is_some());
        let second = runtime.followups[1].id;
        let first = runtime.followups[0].id;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.replace_draft(text_editor::Content::with_text("later draft"));
        runtime.attach([PathBuf::from("later.png")]);
        drop(app.handle_chat_ui(Action::TakeBackQueued(
            target,
            Some(QueueEntry::Local(second)),
        )));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text().trim(), "second task\nlater draft");
        assert_eq!(
            paths(&runtime.attachments),
            [
                Path::new("first.png"),
                Path::new("second.png"),
                Path::new("later.png")
            ]
        );
        assert_eq!(runtime.followups.len(), 2);
        drop(app.handle_chat_ui(Action::RemoveQueued(target, QueueEntry::Local(first))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.followups.len(), 1);
        assert_eq!(runtime.composer.text().trim(), "second task\nlater draft");
        // A message of files alone brings its files back and no text.
        drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.followups.is_empty());
        assert_eq!(runtime.composer.text().trim(), "second task\nlater draft");
        assert_eq!(runtime.attachments.len(), 4);
        assert_eq!(runtime.attachments[0].path, Path::new("third.png"));
        assert!(runtime.transition_blocker().is_some(), "attachments remain");
    }

    #[test]
    fn taking_a_message_back_keeps_the_draft_a_history_walk_set_aside() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("followup"));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.remember("sent earlier".into());
        runtime.followups.push(followup(0, "waiting", &[]));
        runtime.replace_draft(text_editor::Content::with_text("unsent"));
        runtime.recall(true);
        assert_eq!(runtime.composer.text().trim(), "sent earlier");
        drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text().trim(), "waiting\nunsent");
        assert!(runtime.history_cursor.is_none());
    }

    #[test]
    fn a_local_followup_resumes_when_idle_arrives_before_an_unrelated_ack() {
        for refreshing in [false, true] {
            for result in [Ok(()), Err("fixture refused selection".into())] {
                let mut app = crate::app::tests::context_inspector_test_app(false);
                let card = live_chat(&mut app, Some("followup"));
                let target = chat_runtime(&app, card).target;
                let runtime = app.chat_runtime_mut(target).unwrap();
                Arc::make_mut(&mut runtime.snapshot).busy = true;
                Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
                runtime.replace_draft(text_editor::Content::with_text("next task"));
                drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
                drop(app.chat_send(
                    target,
                    if refreshing {
                        Command::Refresh
                    } else {
                        Command::SetPermissionMode {
                            mode: "manual".into(),
                        }
                    },
                    false,
                ));
                let id = chat_runtime(&app, card).pending.as_ref().unwrap().id;
                drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
                assert_eq!(chat_runtime(&app, card).pending.as_ref().unwrap().id, id);
                settle(&mut app, card, result);
                let pending = chat_runtime(&app, card).pending.as_ref().unwrap();
                assert_ne!(pending.id, id);
                assert!(pending.followup);
                assert_eq!(pending.sent.as_deref().unwrap().trim(), "next task");
                settle(&mut app, card, Ok(()));
                assert!(chat_runtime(&app, card).followups.is_empty());
            }
        }
    }

    #[test]
    fn a_combined_followup_is_sent_as_one_prompt_and_keeps_the_new_draft() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("followup"));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
        Arc::make_mut(&mut runtime.snapshot).capabilities.compact = true;
        for text in ["/compact", "also include the second task"] {
            app.chat_runtime_mut(target)
                .unwrap()
                .replace_draft(text_editor::Content::with_text(text));
            drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
        }
        drop(app.dispatch_queued(target));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let pending = runtime.pending.as_ref().unwrap();
        assert!(pending.followup);
        assert_eq!(
            pending.sent.as_deref().unwrap().trim(),
            "/compact\nalso include the second task"
        );
        runtime.replace_draft(text_editor::Content::with_text("later draft"));
        settle(&mut app, card, Ok(()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.followups.is_empty());
        assert_eq!(runtime.composer.text().trim(), "later draft");
        assert_eq!(
            runtime.history.last().unwrap().trim(),
            "/compact\nalso include the second task"
        );
    }

    #[test]
    fn a_rejected_combined_followup_restores_order_without_consuming_an_inflight_draft() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("followup"));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.followups = vec![
            followup(0, "first", &["first.png"]),
            followup(1, "second", &[]),
        ];
        drop(app.dispatch_queued(target));
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.replace_draft(text_editor::Content::with_text("later"));
        runtime.attach([PathBuf::from("later.png")]);
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
        assert_eq!(chat_runtime(&app, card).followups.len(), 2);
        assert_eq!(chat_runtime(&app, card).composer.text().trim(), "later");
        for action in [
            Action::TakeBackQueued(target, Some(QueueEntry::Local(0))),
            Action::TakeBackQueued(target, None),
            Action::RemoveQueued(target, QueueEntry::Local(1)),
        ] {
            drop(app.handle_chat_ui(action));
            assert_eq!(
                chat_runtime(&app, card).followups.len(),
                2,
                "an in-flight send keeps what it is carrying"
            );
        }
        settle(&mut app, card, Err("fixture rejected".into()));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text().trim(), "first\nsecond\nlater");
        assert_eq!(
            paths(&runtime.attachments),
            [Path::new("first.png"), Path::new("later.png")]
        );
        assert!(runtime.followups.is_empty());
    }

    #[test]
    fn combined_followup_attachment_limit_keeps_both_messages_editable() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("followup"));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.session.launch.provider = Provider::Claude;
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        Arc::make_mut(&mut runtime.snapshot).capabilities.queue = false;
        let attachments = (0..8)
            .map(|index| Attachment {
                path: format!("{index}.png").into(),
            })
            .collect::<Vec<_>>();
        runtime.followups = vec![Followup {
            id: 0,
            text: "first".into(),
            attachments: attachments.clone(),
        }];
        runtime.replace_draft(text_editor::Content::with_text("second"));
        runtime.attach([PathBuf::from("extra.png")]);
        drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Queue)));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.followups.len(), 1);
        assert_eq!(runtime.followups[0].attachments, attachments);
        assert_eq!(runtime.composer.text().trim(), "second");
        assert_eq!(
            runtime.attachments,
            [Attachment {
                path: "extra.png".into()
            }]
        );
        assert!(runtime.error.as_ref().unwrap().contains("attachment limit"));
    }

    #[test]
    fn sending_a_local_followup_preserves_the_later_composer_draft() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("followup"));
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.followups.push(followup(0, "queued task", &[]));
        runtime.replace_draft(text_editor::Content::with_text("later draft"));
        let _ = app.dispatch_queued(target);
        assert!(chat_runtime(&app, card).pending.as_ref().unwrap().followup);
        settle(&mut app, card, Ok(()));
        assert!(chat_runtime(&app, card).followups.is_empty());
        assert_eq!(
            chat_runtime(&app, card).composer.text().trim(),
            "later draft"
        );
    }
    fn parsed_codes(cached: &super::CachedItem) -> Vec<String> {
        let mut codes = Vec::new();
        super::for_each_fence(cached.markdown.items(), &mut |_, code| {
            codes.push(code.to_owned())
        });
        codes
    }
    use super::*;

    #[test]
    fn new_chat_pins_selected_home_credential_policy_and_system_inherits() {
        for provider in Account::PROVIDERS {
            for account in [
                Account::system(provider),
                Account::managed(provider, uuid::Uuid::new_v4(), "/selected".into()),
                Account::linked(provider, uuid::Uuid::new_v4(), "/linked".into()),
            ] {
                let session = fresh_chat_session(
                    provider,
                    TerminalId::new(),
                    "/project".into(),
                    &account,
                    false,
                )
                .unwrap();
                let expected = account
                    .credential_overrides()
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect();
                assert_eq!(session.launch.recipe.unset_environment, expected);
                for name in account.credential_overrides() {
                    assert!(!session.launch.recipe.environment.contains_key(*name));
                }
                if !account.is_system() {
                    let home = account.resolved_home().unwrap();
                    assert_eq!(
                        session.launch.recipe.environment
                            [account::home_variable(provider).unwrap()],
                        home.to_str().unwrap()
                    );
                }
                let returned = local_return_launch(&session).unwrap();
                assert_eq!(returned.program, provider.executable());
                assert_eq!(
                    returned.working_directory,
                    session.launch.recipe.working_directory
                );
                assert_eq!(returned.environment, session.launch.recipe.environment);
                assert!(!returned.arguments.iter().any(|arg| arg == "unix://"));
                assert_eq!(
                    returned.unset_environment,
                    session.launch.recipe.unset_environment
                );
            }
        }
    }

    #[test]
    fn resumed_and_cli_paired_snapshots_supply_channel_names() {
        let mut snapshot = Snapshot::default();
        assert_eq!(snapshot_channel_name(&snapshot), None);
        snapshot.items.push(
            chat::Item {
                id: "prompt".into(),
                role: chat::Role::User,
                content: text("Implement the requested feature"),
                started_at: None,
                at: None,
            }
            .into(),
        );
        assert_eq!(
            snapshot_channel_name(&snapshot),
            Some((
                "Implement the requested feature".into(),
                crate::model::ChannelTitleSource::Prompt
            ))
        );
        snapshot.title = Some("Provider conversation name".into());
        assert_eq!(
            snapshot_channel_name(&snapshot),
            Some((
                "Provider conversation name".into(),
                crate::model::ChannelTitleSource::Provider
            ))
        );
        snapshot.title = Some("\u{202e}\u{2066}".into());
        assert_eq!(
            snapshot_channel_name(&snapshot).unwrap().0,
            "Implement the requested feature"
        );
    }

    fn text(body: &str) -> ItemContent {
        ItemContent::Text {
            text: body.to_owned(),
            thinking: false,
            phase: chat::Phase::Final,
        }
    }

    #[test]
    fn a_short_historical_page_can_explicitly_return_to_latest() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("history"));
        let target = chat_runtime(&app, card).target;
        Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = true;
        let _ = app.handle_chat_ui(Action::Latest(target));
        assert_eq!(
            chat_runtime(&app, card).pending.as_ref().unwrap().page,
            Some(view::Page::Newer)
        );
    }

    fn history_fixture(app: &mut Cagents) -> (CardId, Target, Arc<ClientHandle>) {
        let card = live_chat(app, Some(KNOWN_THREAD));
        let runtime = app
            .chat_runtime_mut(chat_runtime(app, card).target)
            .unwrap();
        runtime.show(true);
        let target = runtime.target;
        let handle = runtime.handle().unwrap().clone();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.has_earlier = true;
        handle.publish_snapshot_for_test(snapshot);
        runtime.refresh();
        (card, target, handle)
    }

    /// A chat whose provider holds `messages` and lets them be removed.
    fn provider_queue(
        app: &mut Cagents,
        messages: Vec<chat::QueuedMessage>,
    ) -> (CardId, Target, Arc<ClientHandle>) {
        let (card, target, handle) = history_fixture(app);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.capabilities.queue_remove = true;
        snapshot.queued_messages = messages;
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        (card, target, handle)
    }

    fn provider_message(id: &str, text: &str) -> chat::QueuedMessage {
        chat::QueuedMessage::new(id.into(), text.into())
    }

    /// Publishes the provider's queue with `id` gone from it and, where
    /// `removed` says, its answer to the request to remove it.
    fn provider_drops(
        app: &mut Cagents,
        target: Target,
        handle: &ClientHandle,
        id: &str,
        removed: Option<bool>,
    ) {
        let mut snapshot = (*handle.snapshot()).clone();
        if removed != Some(false) {
            snapshot.queued_messages.retain(|message| message.id != id);
        }
        if let Some(removed) = removed {
            snapshot.queue_removals.push(chat::QueueRemoval {
                id: id.into(),
                removed,
            });
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
    }

    #[test]
    fn a_provider_queued_message_returns_to_the_composer_once_the_provider_lets_go() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut newest = provider_message("newest", "revise me");
        newest.attachments = vec![Attachment {
            path: "queued.png".into(),
        }];
        let (card, target, handle) =
            provider_queue(&mut app, vec![provider_message("older", "stay"), newest]);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.replace_draft(text_editor::Content::with_text("typed since"));
        assert_eq!(
            runtime.newest_take_back(),
            Some(QueueEntry::Provider("newest".into()))
        );
        drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
        // The command being accepted says only that the request was written.
        settle(&mut app, card, Ok(()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.is_recalling("newest"));
        assert_eq!(
            runtime.composer.text().trim(),
            "typed since",
            "nothing returns before the provider has let go of it"
        );
        // The next press reaches past the one already on its way back.
        assert_eq!(
            runtime.newest_take_back(),
            Some(QueueEntry::Provider("older".into()))
        );
        provider_drops(&mut app, target, &handle, "newest", Some(true));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text().trim(), "revise me\ntyped since");
        assert_eq!(paths(&runtime.attachments), [Path::new("queued.png")]);
        assert!(runtime.recalling.is_empty() && runtime.taken_back.is_empty());
        // Sent again, it waits for the next turn as it did before.
        assert_eq!(runtime.resend, Some(PromptMode::Queue));
    }

    #[test]
    fn a_message_the_agent_started_on_is_not_taken_back() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = provider_queue(
            &mut app,
            vec![provider_message("queued", "already running")],
        );
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("draft"));
        drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
        settle(&mut app, card, Ok(()));
        // The message left the queue because it was delivered, not removed.
        provider_drops(&mut app, target, &handle, "queued", None);
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text().trim(), "draft");
        assert!(runtime.recalling.is_empty());
        assert!(runtime.error.as_ref().unwrap().contains("already started"));
    }

    #[test]
    fn a_refused_take_back_leaves_the_message_waiting_and_the_composer_alone() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) =
            provider_queue(&mut app, vec![provider_message("queued", "still waiting")]);
        app.chat_runtime_mut(target)
            .unwrap()
            .replace_draft(text_editor::Content::with_text("draft"));
        // Refused before it reached the provider.
        drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
        settle(&mut app, card, Err("fixture refused".into()));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.recalling.is_empty());
        assert!(runtime.error.as_ref().unwrap().contains("fixture refused"));
        // Refused by the provider: the entry is still listed, and offered.
        drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
        settle(&mut app, card, Ok(()));
        provider_drops(&mut app, target, &handle, "queued", Some(false));
        let runtime = chat_runtime(&app, card);
        assert!(runtime.recalling.is_empty());
        assert_eq!(runtime.composer.text().trim(), "draft");
        assert_eq!(
            runtime.newest_take_back(),
            Some(QueueEntry::Provider("queued".into()))
        );
        // That earlier answer does not settle a second attempt.
        drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
        settle(&mut app, card, Ok(()));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(chat_runtime(&app, card).is_recalling("queued"));
        provider_drops(&mut app, target, &handle, "queued", Some(true));
        assert_eq!(
            chat_runtime(&app, card).composer.text().trim(),
            "still waiting\ndraft"
        );
    }

    #[test]
    fn only_a_whole_removable_message_can_be_taken_back() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut partial = provider_message("partial", "preview only");
        partial.editable = false;
        let (card, target, handle) = provider_queue(&mut app, vec![partial]);
        drop(app.handle_chat_ui(Action::TakeBackQueued(target, None)));
        drop(app.handle_chat_ui(Action::TakeBackQueued(
            target,
            Some(QueueEntry::Provider("partial".into())),
        )));
        assert!(chat_runtime(&app, card).pending.is_none());
        // A provider that cannot remove a message cannot give it back either.
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.capabilities.queue_remove = false;
        snapshot.queued_messages = vec![provider_message("whole", "text")];
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert_eq!(chat_runtime(&app, card).newest_take_back(), None);
    }

    #[test]
    fn history_reconnect_discards_old_request_generation_and_navigation() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = history_fixture(&mut app);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.page_request = Some((view::Page::Earlier, 100));
        runtime.deferred_page = Some(HistoryNavigation::Latest);
        runtime.earlier_asked = Some(1000.0);
        runtime.later_asked = Some(1000.0);
        drop(app.handle_chat_ui(Action::Reconnect(target)));
        let reconnect = app.chat_ui.attempt(card).unwrap().target;
        let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation = 1;
        snapshot.has_earlier = true;
        handle.publish_snapshot_for_test(snapshot);
        answer_start(&mut app, reconnect, Ok(handle));
        let runtime = app.chat_runtime_mut(reconnect).unwrap();
        runtime.show(true);
        runtime.refresh();
        assert!(runtime.page_request.is_none());
        assert!(runtime.deferred_page.is_none());
        assert!(runtime.earlier_asked.is_none());
        assert!(runtime.later_asked.is_none());
        drop(app.handle_chat_ui(Action::Command(reconnect, Command::LoadEarlier)));
        assert_eq!(
            chat_runtime(&app, card).pending.as_ref().unwrap().page,
            Some(view::Page::Earlier)
        );
    }

    #[test]
    fn history_loading_outlives_ack_and_releases_on_provider_response() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.history_loading = true;
        handle.publish_snapshot_for_test(snapshot.clone());
        settle(&mut app, card, Ok(()));
        assert_eq!(
            chat_runtime(&app, card).page_request.map(|(page, _)| page),
            Some(view::Page::Earlier)
        );
        drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
        assert!(
            chat_runtime(&app, card).pending.is_none(),
            "duplicate request is suppressed after command ACK"
        );
        snapshot.generation += 1;
        snapshot.history_loading = false;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(chat_runtime(&app, card).page_request.is_none());
    }

    #[test]
    fn history_cached_response_before_ack_does_not_latch_loading_forever() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(chat_runtime(&app, card).page_request.is_some());
        settle(&mut app, card, Ok(()));
        assert!(chat_runtime(&app, card).page_request.is_none());
        drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
        assert_eq!(
            chat_runtime(&app, card).pending.as_ref().unwrap().page,
            Some(view::Page::Earlier)
        );
    }

    #[test]
    fn history_edge_and_short_page_click_wait_for_unrelated_command() {
        for explicit in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (card, target, _) = history_fixture(&mut app);
            let scroll = view::Scroll::new(0.0, if explicit { 200.0 } else { 1000.0 }, 400.0);
            app.chat_runtime_mut(target).unwrap().scroll = scroll;
            drop(app.chat_send(
                target,
                Command::SetPermissionMode {
                    mode: "manual".into(),
                },
                false,
            ));
            drop(app.handle_chat_ui(if explicit {
                Action::Command(target, Command::LoadEarlier)
            } else {
                Action::Scrolled(target, None, scroll, true)
            }));
            assert_eq!(
                chat_runtime(&app, card).deferred_page,
                Some(if explicit {
                    HistoryNavigation::EarlierExplicit
                } else {
                    HistoryNavigation::EarlierEdge
                })
            );
            assert!(chat_runtime(&app, card).earlier_asked.is_none());
            settle(&mut app, card, Ok(()));
            assert_eq!(
                chat_runtime(&app, card).pending.as_ref().unwrap().page,
                Some(view::Page::Earlier)
            );
        }
    }

    #[test]
    fn history_explicit_navigation_survives_wheeling_away_from_the_edge() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = history_fixture(&mut app);
        drop(app.chat_send(
            target,
            Command::SetPermissionMode {
                mode: "manual".into(),
            },
            false,
        ));
        drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
        drop(app.handle_chat_ui(Action::Scrolled(
            target,
            None,
            view::Scroll::new(0.0, 1000.0, 400.0),
            true,
        )));
        drop(app.handle_chat_ui(Action::Scrolled(
            target,
            None,
            view::Scroll::new(200.0, 1000.0, 400.0),
            true,
        )));
        assert_eq!(
            chat_runtime(&app, card).deferred_page,
            Some(HistoryNavigation::EarlierExplicit)
        );
        settle(&mut app, card, Ok(()));
        assert_eq!(
            chat_runtime(&app, card).pending.as_ref().unwrap().page,
            Some(view::Page::Earlier)
        );
    }

    #[test]
    fn history_failure_can_retry_without_leaving_the_boundary() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = history_fixture(&mut app);
        let scroll = view::Scroll::new(0.0, 1000.0, 400.0);
        app.chat_runtime_mut(target).unwrap().scroll = scroll;
        drop(app.handle_chat_ui(Action::Scrolled(target, None, scroll, true)));
        settle(&mut app, card, Err("temporary history failure".into()));
        drop(app.handle_chat_ui(Action::Scrolled(target, None, scroll, true)));
        assert_eq!(
            chat_runtime(&app, card).pending.as_ref().unwrap().page,
            Some(view::Page::Earlier)
        );
    }

    #[test]
    fn history_provider_failure_can_retry_at_the_same_edge_after_ack() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        let scroll = view::Scroll::new(0.0, 1000.0, 400.0);
        app.chat_runtime_mut(target).unwrap().scroll = scroll;
        drop(app.handle_chat_ui(Action::Scrolled(target, None, scroll, true)));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.history_loading = true;
        handle.publish_snapshot_for_test(snapshot.clone());
        settle(&mut app, card, Ok(()));
        snapshot.generation += 1;
        snapshot.history_loading = false;
        snapshot.error = Some("temporary provider error".into());
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        drop(app.handle_chat_ui(Action::Scrolled(target, None, scroll, true)));
        assert_eq!(
            chat_runtime(&app, card).pending.as_ref().unwrap().page,
            Some(view::Page::Earlier)
        );
    }

    #[test]
    fn history_geometry_changes_never_trigger_paging_even_when_page_heights_match() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = history_fixture(&mut app);
        Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = true;
        for scroll in [
            view::Scroll::new(0.0, 1000.0, 400.0),
            view::Scroll::new(600.0, 1000.0, 400.0),
            view::Scroll::new(0.0, 100.0, 400.0),
        ] {
            drop(app.handle_chat_ui(Action::Scrolled(target, None, scroll, false)));
            assert!(chat_runtime(&app, card).pending.is_none());
        }
    }

    #[test]
    fn history_latest_gesture_cancels_when_reader_leaves_bottom() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = history_fixture(&mut app);
        Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = true;
        drop(app.chat_send(
            target,
            Command::SetPermissionMode {
                mode: "manual".into(),
            },
            false,
        ));
        drop(app.handle_chat_ui(Action::Scrolled(
            target,
            None,
            view::Scroll::new(600.0, 1000.0, 400.0),
            true,
        )));
        assert_eq!(
            chat_runtime(&app, card).deferred_page,
            Some(HistoryNavigation::LatestEdge)
        );
        drop(app.handle_chat_ui(Action::Scrolled(
            target,
            None,
            view::Scroll::new(300.0, 1000.0, 400.0),
            true,
        )));
        assert!(chat_runtime(&app, card).deferred_page.is_none());
        settle(&mut app, card, Ok(()));
        assert!(chat_runtime(&app, card).pending.is_none());
    }

    #[test]
    fn history_explicit_latest_survives_leaving_bottom() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.has_later = true;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        drop(app.chat_send(
            target,
            Command::SetPermissionMode {
                mode: "manual".into(),
            },
            false,
        ));
        drop(app.handle_chat_ui(Action::Latest(target)));
        drop(app.handle_chat_ui(Action::Scrolled(
            target,
            None,
            view::Scroll::new(300.0, 1000.0, 400.0),
            true,
        )));
        assert_eq!(
            chat_runtime(&app, card).deferred_page,
            Some(HistoryNavigation::Latest)
        );
        settle(&mut app, card, Ok(()));
        assert_eq!(
            chat_runtime(&app, card).pending.as_ref().unwrap().page,
            Some(view::Page::Newer)
        );
    }

    #[test]
    fn history_bottom_does_not_mark_unloaded_latest_activity_seen() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = history_fixture(&mut app);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.unseen_activity = true;
        Arc::make_mut(&mut runtime.snapshot).has_later = true;
        drop(app.handle_chat_ui(Action::Scrolled(
            target,
            None,
            view::Scroll::new(600.0, 1000.0, 400.0),
            false,
        )));
        assert!(chat_runtime(&app, card).unseen_activity);
        Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).has_later = false;
        drop(app.handle_chat_ui(Action::Scrolled(
            target,
            None,
            view::Scroll::new(600.0, 1000.0, 400.0),
            false,
        )));
        assert!(!chat_runtime(&app, card).unseen_activity);
    }

    #[test]
    fn history_repeated_latest_after_ack_waits_for_provider_history() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.has_later = true;
        handle.publish_snapshot_for_test(snapshot.clone());
        app.chat_runtime_mut(target).unwrap().refresh();
        drop(app.handle_chat_ui(Action::Latest(target)));
        snapshot.generation += 1;
        snapshot.history_loading = true;
        handle.publish_snapshot_for_test(snapshot.clone());
        settle(&mut app, card, Ok(()));
        assert_eq!(
            chat_runtime(&app, card).page_request.map(|(page, _)| page),
            Some(view::Page::Newer)
        );
        drop(app.handle_chat_ui(Action::Latest(target)));
        assert!(chat_runtime(&app, card).pending.is_none());
        snapshot.generation += 1;
        snapshot.history_loading = false;
        snapshot.has_later = false;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(chat_runtime(&app, card).page_request.is_none());
    }

    #[test]
    fn history_repeated_latest_is_fulfilled_by_the_pending_refresh() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.has_later = true;
        handle.publish_snapshot_for_test(snapshot.clone());
        app.chat_runtime_mut(target).unwrap().refresh();
        drop(app.handle_chat_ui(Action::Latest(target)));
        drop(app.handle_chat_ui(Action::Latest(target)));
        snapshot.generation += 1;
        snapshot.has_later = false;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        settle(&mut app, card, Ok(()));
        assert!(chat_runtime(&app, card).deferred_page.is_none());
        assert!(chat_runtime(&app, card).page_request.is_none());
        drop(app.chat_send(
            target,
            Command::SetPermissionMode {
                mode: "manual".into(),
            },
            false,
        ));
        drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
        assert_eq!(
            chat_runtime(&app, card).deferred_page,
            Some(HistoryNavigation::EarlierExplicit)
        );
    }

    #[test]
    fn history_latest_cancels_earlier_even_before_historical_page_arrives() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = history_fixture(&mut app);
        drop(app.handle_chat_ui(Action::Command(target, Command::LoadEarlier)));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.history_loading = true;
        assert!(!snapshot.has_later);
        handle.publish_snapshot_for_test(snapshot);
        settle(&mut app, card, Ok(()));
        drop(app.handle_chat_ui(Action::Latest(target)));
        assert_eq!(
            chat_runtime(&app, card).pending.as_ref().unwrap().page,
            Some(view::Page::Newer)
        );
    }

    #[test]
    fn history_subagent_scroll_and_latest_never_page_parent() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = history_fixture(&mut app);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.selected_subagent = Some("child".into());
        Arc::make_mut(&mut runtime.snapshot).has_later = true;
        let parent = view::Scroll::new(500.0, 1000.0, 400.0);
        runtime.scroll = parent;
        assert_eq!(runtime.active_scroll(), view::Scroll::default());
        let child = view::Scroll::new(0.0, 200.0, 400.0);
        drop(app.handle_chat_ui(Action::Scrolled(target, Some("child".into()), child, true)));
        drop(app.handle_chat_ui(Action::Scrolled(
            target,
            None,
            view::Scroll::new(0.0, 1000.0, 400.0),
            false,
        )));
        assert_eq!(
            chat_runtime(&app, card).scroll,
            parent,
            "queued parent measurement is stale after tab switch"
        );
        assert_eq!(chat_runtime(&app, card).active_scroll(), child);
        drop(app.handle_chat_ui(Action::Latest(target)));
        assert!(chat_runtime(&app, card).pending.is_none());
    }

    #[test]
    fn tool_group_toggles_override_both_default_choices() {
        for default in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, Some("disclosure"));
            let target = chat_runtime(&app, card).target;
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.tool_groups_expanded = default;
            runtime.turns = turn::group(
                &[chat::Item {
                    id: "u1".into(),
                    role: chat::Role::User,
                    content: text("hello"),
                    started_at: None,
                    at: None,
                }],
                &HashSet::new(),
            );
            let _ = app.handle_chat_ui(Action::ToggleTurn(target, "u1".into()));
            assert_eq!(chat_runtime(&app, card).opened.get("u1"), Some(&!default));
            let _ = app.handle_chat_ui(Action::ToggleTurn(target, "u1".into()));
            assert_eq!(chat_runtime(&app, card).opened.get("u1"), Some(&default));
        }
    }

    #[test]
    fn disclosure_choices_survive_history_eviction_and_are_bounded() {
        let mut opened = HashMap::from([("u1".to_owned(), true)]);
        reconcile_disclosure(&mut opened, &[]);
        assert_eq!(opened.get("u1"), Some(&true));
        opened.insert("u1".to_owned(), false);
        reconcile_disclosure(&mut opened, &[]);
        assert_eq!(opened.get("u1"), Some(&false));
        opened.extend((0..4097).map(|index| (format!("old-{index}"), true)));
        reconcile_disclosure(&mut opened, &[]);
        assert!(opened.is_empty());
    }

    const FIXTURE_DIRECTORY: &str = "/workspace/chat-fixture";
    /// Providers name conversations by UUID; resume refuses anything else.
    const KNOWN_THREAD: &str = "0199a2b4-5c6d-7e8f-9a0b-1c2d3e4f5a6b";

    #[test]
    fn new_panes_keep_the_selected_channel_when_launch_directory_changes() {
        for chat in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(true);
            let source = app.workspace.focused().unwrap();
            let root = PathBuf::from(FIXTURE_DIRECTORY);
            let nested = root.join("src");
            let channel = crate::model::ChannelId::new();
            let durable = Arc::make_mut(&mut app.durable);
            durable.channels.push(crate::model::ProjectChannel {
                title_source: Default::default(),
                id: channel,
                project_id: crate::model::ProjectId::new(),
                title: "Conversation".into(),
                ordinal: 0,
                shared: false,
                cards: vec![source],
                focused: Some(source),
            });
            durable.active_channel = Some(channel);
            app.active_workspace = Some(root.clone());
            if chat {
                drop(app.new_native_chat(Provider::Codex, Some(nested.clone())));
            } else {
                drop(app.add_terminal_recipe(LaunchRecipe::shell(nested.clone()).unwrap()));
            }
            let added = app.workspace.focused().unwrap();
            assert_ne!(added, source);
            assert_eq!(app.active_workspace, Some(root));
            assert_eq!(app.durable.active_channel, Some(channel));
            assert!(app.durable.channels[0].cards.contains(&added));
            assert!(app.durable.channels[0].cards.contains(&source));
            assert_eq!(
                crate::app::worktree_ui::card_directory(app.card_state(added).unwrap()),
                Some(nested.as_path())
            );
        }
    }

    #[test]
    fn automatic_draft_routing_waits_for_untouched_send_and_same_account_choice_pins() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let alternate = app.account_ui.add_managed_fixture(Provider::Codex, 105);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        let now = crate::chat::now_ms().unwrap();
        for (id, status) in [
            (
                Account::system(Provider::Codex).id(),
                account::UsageStatus::Limited,
            ),
            (alternate.clone(), account::UsageStatus::Ok),
        ] {
            let _ = app.update(Message::AccountUi(crate::app::account_ui::Action::Probed {
                id,
                outcome: account::probe::Outcome::Answered {
                    identity: None,
                    catalog: None,
                    snapshot: account::UsageSnapshot::state(status, "", now),
                },
            }));
        }
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.replace_draft(text_editor::Content::with_text(
            "Retain this unsent message",
        ));
        assert_eq!(
            app.automatic_chat_account(target, PromptMode::Send)
                .unwrap()
                .0
                .id(),
            alternate
        );
        assert!(
            app.automatic_chat_account(target, PromptMode::Steer)
                .is_none()
        );
        app.chat_runtime_mut(target)
            .unwrap()
            .attachment_chooser_pending = true;
        assert!(
            app.automatic_chat_account(target, PromptMode::Send)
                .is_none()
        );
        app.chat_runtime_mut(target)
            .unwrap()
            .attachment_chooser_pending = false;
        drop(app.choose_chat_account(target, Account::system(Provider::Codex).id()));
        assert!(
            app.automatic_chat_account(target, PromptMode::Send)
                .is_none()
        );
        assert_eq!(
            chat_runtime(&app, card).composer.text().trim(),
            "Retain this unsent message"
        );
        assert!(
            !chat_runtime(&app, card)
                .session
                .launch
                .recipe
                .environment
                .contains_key(account::routing::AUTOMATIC_MARKER)
        );
    }

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

    #[test]
    fn chat_selection_commands_open_exclusive_pickers_without_changing_the_draft() {
        use crate::workspace_widget::GlobalShortcut;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.account_ui.add_managed_fixture(Provider::Codex, 22);
        let card = draft_chat(&mut app);
        let original = chat_runtime(&app, card).session.clone();
        let permission_mode = chat_runtime(&app, card).snapshot.permission_mode.clone();
        for command in [
            GlobalShortcut::ChatAccount,
            GlobalShortcut::ChatModel,
            GlobalShortcut::ChatPermissions,
        ] {
            drop(app.open_chat_control(command));
            let runtime = chat_runtime(&app, card);
            assert_eq!(
                runtime.account_picker,
                command == GlobalShortcut::ChatAccount
            );
            assert_eq!(
                runtime.model_picker.is_some(),
                command == GlobalShortcut::ChatModel
            );
            assert_eq!(
                runtime.permission_picker,
                command == GlobalShortcut::ChatPermissions
            );
            assert_eq!(runtime.session, original);
            assert_eq!(runtime.snapshot.permission_mode, permission_mode);
            assert!(!app.chat_start_pending(card));
            drop(app.open_chat_control(command));
            let runtime = chat_runtime(&app, card);
            assert!(
                !runtime.account_picker
                    && runtime.model_picker.is_none()
                    && !runtime.permission_picker
            );
            assert!(!runtime.picker_keyboard);
        }
        let target = chat_runtime(&app, card).target;
        app.chat_runtime_mut(target)
            .unwrap()
            .attachment_chooser_pending = true;
        drop(app.open_chat_control(GlobalShortcut::ChatAccount));
        assert!(!chat_runtime(&app, card).account_picker);
    }

    #[test]
    fn chat_keyboard_pickers_own_tab_and_restore_the_shell_preference_on_close() {
        use crate::workspace_widget::GlobalShortcut;
        for initial in [false, true] {
            for command in [GlobalShortcut::ChatAccount, GlobalShortcut::ChatPermissions] {
                let mut app = crate::app::tests::context_inspector_test_app(false);
                app.account_ui.add_managed_fixture(Provider::Codex, 22);
                let card = draft_chat(&mut app);
                app.core.set_keyboard_nav(initial);
                drop(app.update(Message::RunCommand(command)));
                assert!(app.chat_picker_owns_keyboard());
                assert!(!app.core.keyboard_nav());
                drop(app.update(Message::RunCommand(command)));
                assert!(!app.chat_picker_owns_keyboard());
                assert_eq!(app.core.keyboard_nav(), initial);

                drop(app.update(Message::RunCommand(command)));
                assert!(!app.core.keyboard_nav());
                assert!(app.dismiss_chat_picker());
                app.sync_popup_keyboard_nav();
                assert_eq!(app.core.keyboard_nav(), initial);
                assert!(!chat_runtime(&app, card).picker_keyboard);
            }
        }
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        app.core.set_keyboard_nav(true);
        drop(app.update(Message::RunCommand(GlobalShortcut::ChatPermissions)));
        let target = chat_runtime(&app, card).target;
        drop(app.update(Message::ChatUi(Action::PermissionPage(
            target,
            permissions::Page::Rules,
        ))));
        assert!(!app.chat_picker_owns_keyboard());
        assert!(
            app.core.keyboard_nav(),
            "Rules page fields retain ordinary Tab traversal"
        );
    }

    #[test]
    fn captured_escape_closes_model_search_without_closing_the_inspector() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        app.context_inspector_open = true;
        drop(app.open_chat_control(crate::workspace_widget::GlobalShortcut::ChatModel));
        assert!(chat_runtime(&app, card).model_picker.is_some());
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            },
            captured: true,
        }));
        assert!(chat_runtime(&app, card).model_picker.is_none());
        assert!(app.context_inspector_open);
    }

    #[test]
    fn account_picker_keyboard_reaches_manage_accounts_and_activates_it() {
        use crate::workspace_widget::GlobalShortcut;
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Rectangle, Size};
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.account_ui.add_managed_fixture(Provider::Codex, 22);
        let renderer = crate::app::tests::headless_renderer();
        let card = draft_chat(&mut app);
        drop(app.open_chat_control(GlobalShortcut::ChatAccount));
        let press = |key, code| keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let up = press(keyboard::key::Named::ArrowUp, keyboard::key::Code::ArrowUp);
        let down = press(
            keyboard::key::Named::ArrowDown,
            keyboard::key::Code::ArrowDown,
        );
        let runtime = chat_runtime(&app, card);
        let accounts = app
            .chat_accounts(
                runtime.session.launch.provider,
                &runtime.session.launch.recipe,
            )
            .unwrap();
        let choices = accounts.choices.len();
        assert!(app.chat_picker_key(&up).is_some());
        assert_eq!(chat_runtime(&app, card).picker_focus, choices);
        assert!(app.chat_picker_key(&down).is_some());
        assert_eq!(chat_runtime(&app, card).picker_focus, 0);
        for _ in 0..choices {
            assert!(app.chat_picker_key(&down).is_some());
        }
        assert_eq!(chat_runtime(&app, card).picker_focus, choices);
        let mut messages = Vec::new();
        {
            let mut element = app.chat_view(card);
            let mut tree = Tree::new(element.as_widget());
            let viewport = Rectangle::with_size(Size::new(320.0, 600.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let mut overlay = element
                .as_widget_mut()
                .overlay(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &viewport,
                    iced::Vector::ZERO,
                )
                .expect("account menu overlay");
            let popup = overlay.as_overlay_mut().layout(&renderer, viewport.size());
            overlay.as_overlay_mut().operate(
                Layout::new(&popup),
                &renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                    card,
                    "chat-manage-accounts",
                )),
            );
            overlay.as_overlay_mut().update(
                &Event::Keyboard(press(
                    keyboard::key::Named::Enter,
                    keyboard::key::Code::Enter,
                )),
                Layout::new(&popup),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
            );
        }
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::AccountUi(crate::app::account_ui::Action::OpenPane)
        )));
        assert!(!app.chat_start_pending(card));
    }

    #[test]
    fn permission_picker_keyboard_navigation_selects_a_mode_on_enter() {
        use crate::workspace_widget::GlobalShortcut;
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Rectangle, Size};
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let renderer = crate::app::tests::headless_renderer();
        let card = draft_chat(&mut app);
        drop(app.open_chat_control(GlobalShortcut::ChatPermissions));
        let press = |key, code| keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let down = press(
            keyboard::key::Named::ArrowDown,
            keyboard::key::Code::ArrowDown,
        );
        let original = chat_runtime(&app, card).snapshot.permission_mode.clone();
        let count = chat_runtime(&app, card)
            .snapshot
            .capabilities
            .permission_modes
            .len();
        assert!(count > 1);
        let initial = chat_runtime(&app, card).picker_focus;
        assert!(app.chat_picker_key(&down).is_some());
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.picker_focus, (initial + 1) % count);
        assert_eq!(runtime.snapshot.permission_mode, original);
        let chosen = runtime.snapshot.capabilities.permission_modes[runtime.picker_focus].clone();
        let mut messages = Vec::new();
        {
            let mut element = runtime.permissions_view(true, 320.0);
            let mut tree = Tree::new(element.as_widget());
            let viewport = Rectangle::with_size(Size::new(320.0, 600.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
                    card,
                    &format!("chat-permission-mode-{chosen}"),
                )),
            );
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Keyboard(press(
                    keyboard::key::Named::Enter,
                    keyboard::key::Code::Enter,
                )),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
        }
        assert!(messages.iter().any(|message| matches!(message, Message::ChatUi(Action::PermissionMode(_, mode)) if mode == &chosen)));
        for message in messages {
            drop(app.update(message));
        }
        assert_eq!(
            chat_runtime(&app, card).snapshot.permission_mode.as_ref(),
            Some(&chosen)
        );
        assert!(!app.chat_start_pending(card));
    }

    #[test]
    fn default_pane_is_ephemeral_but_explicit_chats_are_not() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
        let automatic = app.workspace.focused().unwrap();
        assert!(chat_runtime(&app, automatic).is_draft());
        assert!(!app.chat_start_pending(automatic));
        assert!(
            !app.persistent_workspace()
                .cards
                .iter()
                .any(|card| card.id == automatic)
        );
        let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
        assert_eq!(app.workspace.focused(), Some(automatic));
        let explicit = draft_chat(&mut app);
        let _ = app.select_worktree("/workspace/elsewhere".into(), false);
        assert!(app.card_state(automatic).is_none());
        assert!(!app.cards.contains_key(&automatic));
        assert!(app.card_state(explicit).unwrap().existence.is_open());
        let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
        assert_eq!(app.workspace.focused(), Some(explicit));
    }

    #[test]
    fn default_pane_keeps_unsent_work_and_in_flight_starts() {
        for send in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
            let card = app.workspace.focused().unwrap();
            write(&mut app, card, "keep this message");
            if send {
                submit(&mut app, card);
                assert!(app.chat_start_pending(card));
            }
            let _ = app.select_worktree("/workspace/elsewhere".into(), false);
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(
                app.persistent_workspace()
                    .cards
                    .iter()
                    .any(|state| state.id == card)
            );
            let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
            assert_eq!(app.workspace.focused(), Some(card));
            if !send {
                assert_eq!(
                    chat_runtime(&app, card).composer.text(),
                    "keep this message"
                );
            }
        }
    }

    #[test]
    fn window_close_protects_attachments_local_followups_and_request_answers() {
        for kind in 0..3 {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = draft_chat(&mut app);
            let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                panic!("expected chat");
            };
            match kind {
                0 => runtime.attachments.push(Attachment {
                    path: "/workspace/draft.txt".into(),
                }),
                1 => runtime
                    .followups
                    .push(followup(0, "accepted local task", &[])),
                _ => {
                    runtime.answers.insert(
                        ("request".into(), "question".into()),
                        vec!["partial answer".into()],
                    );
                }
            }
            let window = cosmic::iced::window::Id::unique();
            drop(app.update(Message::WindowCloseRequested(window)));
            assert!(app.exit.is_none());
            assert_eq!(app.transient_work_close, Some(window));
            drop(app.update(Message::CancelTransientWorkClose));
            assert!(app.window_close_transient_blocker().is_some());
            if let Some(CardRuntime::Chat(runtime)) = app.cards.remove(&card) {
                app.chat_ui.parked.insert(card, runtime);
            }
            drop(app.update(Message::WindowCloseRequested(window)));
            assert!(app.exit.is_none(), "hidden chat input also blocks closing");
            assert_eq!(app.transient_work_close, Some(window));
        }
    }

    #[test]
    fn update_snapshot_restores_live_stashed_and_parked_composers() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let live = draft_chat(&mut app);
        write(&mut app, live, "live draft");
        assert!(chat_runtime(&app, live).restart_blocker().is_none());
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&live) else {
            panic!("expected live chat runtime");
        };
        runtime.attachments.push(Attachment {
            path: "/workspace/example.txt".into(),
        });
        assert!(runtime.restart_blocker().is_some());
        runtime.attachments.clear();

        let stashed = draft_chat(&mut app);
        write(&mut app, stashed, "stashed draft");
        let stashed_session = chat_runtime(&app, stashed).session.clone();
        app.chat_forget(stashed);
        app.cards.insert(stashed, CardRuntime::Loading);

        let parked = draft_chat(&mut app);
        write(&mut app, parked, "parked draft");
        let CardRuntime::Chat(runtime) = app.cards.remove(&parked).unwrap() else {
            panic!("expected chat runtime");
        };
        app.chat_ui.parked.insert(parked, runtime);

        let saved = app.persistent_workspace();
        assert_eq!(saved.chat_drafts.get(&live).unwrap(), "live draft");
        assert_eq!(saved.chat_drafts.get(&stashed).unwrap(), "stashed draft");
        assert_eq!(saved.chat_drafts.get(&parked).unwrap(), "parked draft");

        app.chat_ui = State::with_drafts(saved.chat_drafts.clone());
        let _ = app.open_chat_draft(stashed, stashed_session);
        assert_eq!(chat_runtime(&app, stashed).composer.text(), "stashed draft");

        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&live) else {
            panic!("expected live chat runtime");
        };
        runtime.replace_draft(text_editor::Content::new());
        let saved = app.persistent_workspace();
        assert!(!saved.chat_drafts.contains_key(&live));

        close_chat(&mut app, stashed);
        assert!(
            !app.persistent_workspace()
                .chat_drafts
                .contains_key(&stashed)
        );
    }

    #[test]
    fn default_pane_keeps_attached_files() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let _ = app.select_worktree(FIXTURE_DIRECTORY.into(), true);
        let card = app.workspace.focused().unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("chat")
        };
        runtime.attachments.push(Attachment {
            path: "/workspace/example.txt".into(),
        });
        let _ = app.select_worktree("/workspace/elsewhere".into(), false);
        assert!(app.card_state(card).unwrap().existence.is_open());
    }

    #[test]
    fn a_draft_completes_a_skill_and_queues_it_without_starting_early() {
        const INVOCATION: &str = "[$review](/skills/review/SKILL.md)";
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let runtime = chat_runtime(&app, card);
        let target = runtime.target;
        let launch = runtime.session.launch.clone();
        let _ = app.update(Message::ChatUi(Action::DraftSkills(
            target,
            launch,
            vec![chat::Skill {
                name: "review".into(),
                description: "Review the change".into(),
                invocation: INVOCATION.into(),
            }],
        )));
        write(&mut app, card, "/rev");
        let runtime = chat_runtime(&app, card);
        let (_, rows) = runtime
            .completion()
            .expect("skills are offered before startup");
        assert_eq!(rows.len(), 1);
        assert!(runtime.is_draft());
        assert!(runtime.handle().is_none());
        assert!(!app.chat_start_pending(card));
        assert_eq!(rows[0].insertion, "$review");
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .complete("$review", false)
        );
        write(&mut app, card, "this change");
        assert_eq!(
            chat_runtime(&app, card).composer.text().trim(),
            "$review this change"
        );
        submit(&mut app, card);
        assert!(app.chat_start_pending(card));
        assert_eq!(
            chat_runtime(&app, card)
                .queued
                .as_ref()
                .unwrap()
                .text
                .trim(),
            "$review this change"
        );
        assert_eq!(
            chat_runtime(&app, card)
                .queued
                .as_ref()
                .unwrap()
                .resolved_text
                .trim(),
            format!("{INVOCATION} this change")
        );
    }

    #[test]
    fn a_provider_command_takes_precedence_over_a_queued_draft_skill() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let runtime = chat_runtime(&app, card);
        let target = runtime.target;
        let launch = runtime.session.launch.clone();
        let _ = app.update(Message::ChatUi(Action::DraftSkills(
            target,
            launch,
            vec![chat::Skill {
                name: "review".into(),
                description: String::new(),
                invocation: "[$review](/skills/review/SKILL.md)".into(),
            }],
        )));
        write(&mut app, card, "/review this change");
        submit(&mut app, card);
        let queued = chat_runtime(&app, card).queued.as_ref().unwrap();
        assert_eq!(
            queued.resolved_text,
            "[$review](/skills/review/SKILL.md) this change"
        );
        let provider = Snapshot {
            commands: vec![chat::SlashCommand {
                name: "review".into(),
                description: String::new(),
            }],
            ..Snapshot::default()
        };
        assert_eq!(queued.submission_text(&provider), "/review this change");
        assert!(matches!(
            submission(queued.submission_text(&provider), &provider, &[], PromptMode::Send),
            Ok(Command::Slash { command }) if command == "/review this change"
        ));
    }

    #[test]
    fn draft_skill_results_are_scoped_to_the_current_launch_and_card_generation() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let runtime = chat_runtime(&app, card);
        let target = runtime.target;
        let launch = runtime.session.launch.clone();
        let skill = chat::Skill {
            name: "review".into(),
            description: String::new(),
            invocation: "$review".into(),
        };
        let mut other_account = launch.clone();
        other_account
            .recipe
            .environment
            .insert("CODEX_HOME".into(), "/another/account".into());
        for (recipient, source) in [
            (target, other_account),
            (
                Target {
                    generation: target.generation + 1,
                    ..target
                },
                launch.clone(),
            ),
        ] {
            let _ = app.update(Message::ChatUi(Action::DraftSkills(
                recipient,
                source,
                vec![skill.clone()],
            )));
            assert!(chat_runtime(&app, card).snapshot.skills.is_empty());
        }
        let _ = app.update(Message::ChatUi(Action::DraftSkills(
            target,
            launch.clone(),
            vec![skill.clone()],
        )));
        assert_eq!(
            chat_runtime(&app, card).snapshot.skills,
            vec![skill.clone()]
        );
        let _ = app.update(Message::ChatUi(Action::DraftModel(
            target,
            Provider::Codex,
            None,
        )));
        assert_eq!(
            chat_runtime(&app, card).snapshot.skills,
            vec![skill.clone()]
        );
        let _ = app.update(Message::ChatUi(Action::DraftModel(
            target,
            Provider::Claude,
            None,
        )));
        assert!(chat_runtime(&app, card).snapshot.skills.is_empty());
        assert_eq!(
            chat_runtime(&app, card).snapshot.capabilities.skill_reach,
            chat::SkillReach::Leading
        );
        let _ = app.update(Message::ChatUi(Action::DraftSkills(
            target,
            launch,
            vec![skill],
        )));
        assert!(chat_runtime(&app, card).snapshot.skills.is_empty());
        assert!(!app.chat_start_pending(card));
    }

    #[test]
    fn a_running_chat_ignores_late_draft_skill_discovery() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let runtime = chat_runtime(&app, card);
        let target = runtime.target;
        let launch = runtime.session.launch.clone();
        let skills = runtime.snapshot.skills.clone();
        let _ = app.update(Message::ChatUi(Action::DraftSkills(
            target,
            launch,
            vec![chat::Skill {
                name: "late".into(),
                description: String::new(),
                invocation: "$late".into(),
            }],
        )));
        assert_eq!(chat_runtime(&app, card).snapshot.skills, skills);
    }

    fn local_return(app: &Cagents) -> (Target, &LocalReturn) {
        app.chat_ui
            .phases
            .values()
            .find_map(|phase| match phase {
                Phase::Returning {
                    target,
                    local: Some(plan),
                    ..
                } => Some((*target, plan)),
                _ => None,
            })
            .expect("a local return owns the card")
    }

    /// Types `text` into the composer of `card`.
    fn write(app: &mut Cagents, card: CardId, text: &str) {
        let target = chat_runtime(app, card).target;
        let _ = app.update(Message::ChatUi(Action::Composer(
            target,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(text.to_owned()))),
        )));
    }

    #[test]
    fn multiline_prompt_history_restores_the_exact_unsent_draft_and_attachments() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("history"));
        let original = "unsent message\nsecond line\n\n";
        write(&mut app, card, original);
        let target = chat_runtime(&app, card).target;
        let (cursor, selection) = {
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.history = vec!["older\nmultiline".into(), "latest\nmultiline".into()];
            Arc::make_mut(&mut runtime.snapshot).busy = true;
            Arc::make_mut(&mut runtime.snapshot).capabilities.steer = true;
            runtime.attach([PathBuf::from("/workspace/attachment.txt")]);
            runtime.composer.perform(text_editor::Action::Move(
                text_editor::Motion::DocumentStart,
            ));
            runtime
                .composer
                .perform(text_editor::Action::Select(text_editor::Motion::Right));
            assert_eq!(runtime.history_navigation(), (true, false));
            (runtime.composer.cursor(), runtime.composer.selection())
        };
        for (action, expected) in [
            (Action::HistoryOlder(target), "latest\nmultiline"),
            (Action::HistoryOlder(target), "older\nmultiline"),
            (Action::HistoryOlder(target), "older\nmultiline"),
            (Action::HistoryNewer(target), "latest\nmultiline"),
        ] {
            drop(app.update(Message::ChatUi(action)));
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.composer.text(), expected);
            assert_eq!(runtime.history_navigation(), (true, true));
            assert_eq!(
                runtime.attachments[0].path,
                PathBuf::from("/workspace/attachment.txt")
            );
        }
        assert_eq!(app.persistent_workspace().chat_drafts[&card], original);

        drop(app.update(Message::ChatUi(Action::HistoryNewer(target))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.composer.text(), original);
        assert_eq!(runtime.composer.cursor(), cursor);
        assert_eq!(runtime.composer.selection(), selection);
        assert_eq!(runtime.history_cursor, None);
        assert!(runtime.history_stash.is_none());
        assert!(runtime.snapshot.busy && runtime.snapshot.capabilities.steer);
        drop(app.update(Message::ChatUi(Action::HistoryNewer(target))));
        assert_eq!(chat_runtime(&app, card).composer.text(), original);
    }

    #[test]
    fn prompt_history_keeps_multiline_draft_navigation_and_saved_text() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        write(&mut app, card, "unsent\nlast line");
        let target = chat_runtime(&app, card).target;
        {
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.history = vec!["sent prompt".into()];
            runtime
                .composer
                .perform(text_editor::Action::Move(text_editor::Motion::DocumentEnd));
            assert_eq!(
                runtime.history_navigation(),
                (false, false),
                "a multiline draft keeps caret navigation"
            );
            runtime.composer.perform(text_editor::Action::Move(
                text_editor::Motion::DocumentStart,
            ));
        }
        drop(app.update(Message::ChatUi(Action::HistoryOlder(target))));
        app.chat_forget(card);
        assert_eq!(app.chat_ui.drafts[&card], "unsent\nlast line");
    }

    /// Sends what the composer of `card` holds.
    fn submit(app: &mut Cagents, card: CardId) {
        let target = chat_runtime(app, card).target;
        let _ = app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send)));
    }

    #[test]
    fn a_pending_image_paste_stays_with_the_message_it_precedes() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("thread"));
        let target = chat_runtime(&app, card).target;
        write(&mut app, card, "describe this image");
        {
            let runtime = app.chat_runtime_mut(target).unwrap();
            Arc::make_mut(&mut runtime.snapshot)
                .capabilities
                .attachments = true;
            runtime.pastes_pending = 1;
            runtime.image_pastes_saving = 1;
        }
        submit(&mut app, card);
        assert!(chat_runtime(&app, card).pending.is_none());
        assert_eq!(
            chat_runtime(&app, card).submit_after_paste,
            Some(PromptMode::Send)
        );
        assert_eq!(
            chat_runtime(&app, card).composer.text().trim(),
            "describe this image"
        );

        let path = PathBuf::from("/fixture/pasted.png");
        let _ = app.update(Message::ChatUi(Action::ImageSaved(
            target,
            Ok(path.clone()),
        )));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.pastes_pending, 0);
        assert_eq!(runtime.image_pastes_saving, 0);
        assert_eq!(runtime.submit_after_paste, None);
        assert_eq!(runtime.attachments, [Attachment { path }]);
        assert!(
            runtime.pending.is_some(),
            "send resumes after the image is ready"
        );
    }

    #[test]
    fn an_image_the_chat_cannot_take_falls_back_to_clipboard_text() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("thread"));
        let target = chat_runtime(&app, card).target;
        {
            let runtime = app.chat_runtime_mut(target).unwrap();
            Arc::make_mut(&mut runtime.snapshot)
                .capabilities
                .attachments = false;
            runtime.pastes_pending = 1;
            runtime.submit_after_paste = Some(PromptMode::Send);
        }
        let image = clipboard_image::Image::try_from((
            b"\x89PNG\r\n\x1a\n".to_vec(),
            "image/png".to_owned(),
        ))
        .unwrap();
        let _ = app.update(Message::ChatUi(Action::PastedImage(target, Some(image))));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.error, None, "the clipboard text is read first");
        assert_eq!(runtime.pastes_pending, 1);
        assert_eq!(runtime.submit_after_paste, Some(PromptMode::Send));

        let _ = app.update(Message::ChatUi(Action::PastedText(
            target,
            Some("pasted cell".into()),
        )));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.error, None);
        assert_eq!(runtime.pastes_pending, 0);
        assert!(runtime.attachments.is_empty());
        assert!(
            runtime.pending.is_some(),
            "the send waiting on the paste goes ahead with its text"
        );

        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.pastes_pending = 1;
        let _ = app.update(Message::ChatUi(Action::PasteRefused(
            target,
            "The current model cannot accept images.",
        )));
        let runtime = chat_runtime(&app, card);
        assert_eq!(runtime.pastes_pending, 0);
        assert_eq!(
            runtime.error.as_deref(),
            Some("The current model cannot accept images.")
        );
    }

    /// A new conversation whose start, for its first message, is still in
    /// flight.
    fn starting_chat(app: &mut Cagents) -> (CardId, Target) {
        let card = draft_chat(app);
        write(app, card, "hello");
        submit(app, card);
        let target = app
            .chat_ui
            .attempt(card)
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

    fn account_commit_reply(task: Task<Message>) -> Message {
        use cosmic::iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut stream = cosmic::iced::runtime::task::into_stream(task).unwrap();
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::ChatUi(
                            Action::AccountCommitted(..) | Action::UpgradeCommitted(..),
                        ),
                    )) = action
                    {
                        return message;
                    }
                }
                panic!("account commit produced no acknowledgment");
            })
    }

    fn fixture_handle(app: &Cagents, card: CardId, session_id: Option<&str>) -> Arc<ClientHandle> {
        let mut session = app.chat_ui.attempt(card).unwrap().session.clone();
        // The daemon starts a new Codex thread on a listener of its own and
        // hands the thread back naming it.
        let proof = crate::model::HostSessionProof {
            host_session_id: "fixture-host".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "fixture-incarnation".into(),
        };
        session.claim_owned_codex_listener(std::path::Path::new("/daemon-temp-root"), &proof);
        session.proof = Some(proof);
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
            .attempt(card)
            .expect("a start is in flight")
            .target
    }

    /// Closes `card` the way its close button does, and checks that it is gone
    /// for good: off the strip, tombstoned, and held by nothing.
    fn close_chat(app: &mut Cagents, card: CardId) {
        let _ = app.update(Message::CloseCard(card));
        if let Some(Phase::Closing {
            target,
            restart: None,
        }) = app.chat_ui.phases.get(&card)
        {
            let target = *target;
            let source = app.chat_return_session(card).unwrap();
            assert!(app.card_state(card).unwrap().existence.is_open());
            let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
                target,
                source,
                Ok(()),
            )));
        }
        assert_eq!(
            app.workspace.pending_close(),
            None,
            "chat removal commits in the same update"
        );
        let _ = app.update(Message::CommitClose(card)); // A queued duplicate is harmless.
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
            !app.chat_ui.phases.contains_key(&card),
            "no active operation outlives the card"
        );
        assert!(!app.chat_start_pending(card));
    }

    #[test]
    fn late_native_ownership_is_saved_even_while_the_chat_is_hidden() {
        for visible in [true, false] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, Some(KNOWN_THREAD));
            let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                unreachable!()
            };
            let target = runtime.target;
            runtime.show(visible);
            let handle = runtime.handle().unwrap().clone();
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            snapshot.native_continuity = chat::NativeContinuity::Durable {
                provider_version: "0.157.1".into(),
            };
            handle.replace_snapshot_for_test(snapshot);
            let _ = app.update(Message::ChatUi(Action::Wake(target, Wake::Activity)));
            let CardKind::Chat(saved) = &app.card_state(card).unwrap().kind else {
                unreachable!()
            };
            assert!(matches!(
                saved.native_continuity,
                chat::NativeContinuity::Durable { .. }
            ));
            assert_eq!(
                app.chat_return_session(card).unwrap().native_continuity,
                saved.native_continuity
            );
        }
    }

    #[test]
    fn failed_retry_adapter_cannot_acknowledge_retained_native_cleanup() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            unreachable!()
        };
        let target = runtime.target;
        runtime.disconnected = true;
        let _ = app.update(Message::ChatUi(Action::Reconnect(target)));
        let retry = chat_runtime_attempt(&app, card);
        let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.exited = true;
        handle.replace_snapshot_for_test(snapshot);
        answer_start(&mut app, retry, Ok(handle));
        assert!(chat_runtime(&app, card).connected());
        assert!(chat_runtime(&app, card).handle().unwrap().snapshot().exited);
        let source = app.chat_return_session(card).unwrap();
        let _ = app.begin_chat_close(card);
        let close = app.chat_ui.phases[&card].target();
        assert!(app.card_state(card).unwrap().existence.is_open());
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            close,
            source.clone(),
            Err("holder cleanup remains unconfirmed".into()),
        )));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert_eq!(app.chat_return_session(card), Some(source.clone()));
        let _ = app.begin_chat_close(card);
        let close = app.chat_ui.phases[&card].target();
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            close,
            source,
            Ok(()),
        )));
        assert!(!app.card_state(card).unwrap().existence.is_open());
    }

    #[test]
    fn failed_retained_native_close_keeps_saved_identity_until_exact_acknowledgment() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let saved = app.chat_return_session(card).unwrap();
        let _ = app.begin_chat_close(card);
        let target = app.chat_ui.phases[&card].target();
        let _ = app.begin_chat_close(card); // A repeated click cannot discard the pending identity.
        assert!(app.card_state(card).unwrap().existence.is_open());
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            target,
            saved.clone(),
            Err("cleanup remains unconfirmed".into()),
        )));
        assert_eq!(app.chat_return_session(card).as_ref(), Some(&saved));
        assert!(app.card_state(card).unwrap().existence.is_open());
        let _ = app.begin_chat_close(card);
        let retry = app.chat_ui.phases[&card].target();
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            target,
            saved.clone(),
            Ok(()),
        )));
        assert!(
            app.card_state(card).unwrap().existence.is_open(),
            "stale acknowledgment cannot complete a retry"
        );
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            retry,
            saved,
            Ok(()),
        )));
        assert!(!app.card_state(card).unwrap().existence.is_open());
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
        let saved = app.chat_return_session(card).unwrap();
        let _ = app.update(Message::CloseCard(card));
        let target = app.chat_ui.phases[&card].target();
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            target,
            saved.clone(),
            Err("fixture: daemon unreachable".into()),
        )));
        assert_eq!(app.chat_return_session(card), Some(saved));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(
            !errors(&app).is_empty(),
            "a conversation that may still run is reported"
        );
        assert!(!app.chat_ui.transition_pending());
        app.notices.entries.clear();
        close_chat(&mut app, card);
        assert!(
            app.chat_ui.retiring.is_empty(),
            "the acknowledged close already proved retirement"
        );
    }

    /// A successful close committed a removal the workspace had never been
    /// asked for, which failed with "no card removal is pending" and left a
    /// tombstoned card on the strip, spinning, with no way to close it.
    #[test]
    fn a_live_chat_closes_without_leaving_its_card_behind() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("thread"));
        close_chat(&mut app, card);
        assert!(
            app.chat_ui.retiring.is_empty(),
            "the close receipt already retired the owner"
        );
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
        assert!(
            app.chat_ui.retiring.is_empty(),
            "the acknowledged close needs no second retirement"
        );
    }

    #[test]
    fn closing_a_chat_twice_is_one_close() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, None);
        let _ = app.update(Message::CloseCard(card));
        let closing = app.chat_ui.phases[&card].target();
        let _ = app.update(Message::CloseCard(card));
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
        assert_eq!(app.chat_ui.phases[&card].target(), closing);
        let _ = app.update(Message::CommitClose(card));
        assert!(
            app.workspace.order().any(|id| id == card),
            "an unacknowledged owner keeps its card"
        );
        close_chat(&mut app, card);
        assert!(app.chat_ui.retiring.is_empty());
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
        let attempt = app.chat_ui.attempt(card).unwrap();
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
            unset_environment: Default::default(),
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
        assert_eq!(
            state.width_fraction,
            app.settings.default_native_pane_fraction
        );
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
        let target = app.chat_ui.attempt(chat).unwrap().target;
        let handle = fixture_handle(&app, chat, Some("started"));
        answer_start(&mut app, target, Ok(handle));
        let CardKind::Chat(stored) = saved(&app, chat).kind else {
            panic!("the started conversation is saved");
        };
        assert_eq!(stored.launch.session_id.as_deref(), Some("started"));
        assert!(!app.chat_ui.transition_pending());
    }

    #[test]
    fn a_stopped_terminal_moves_its_restored_chat_draft_to_the_new_card() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let terminal = stopped_agent_terminal(&mut app, &["--resume", KNOWN_THREAD], true);
        let text = "Continue this thought\n  with indentation";
        Arc::make_mut(&mut app.durable)
            .chat_drafts
            .insert(terminal, text.into());
        app.chat_ui = State::with_drafts(app.durable.chat_drafts.clone());

        let _ = app.update(Message::ChatUi(Action::Switch(terminal)));
        let chat = app.workspace.focused().unwrap();
        assert_ne!(chat, terminal);
        assert!(matches!(
            app.card_state(terminal).unwrap().kind,
            CardKind::Terminal(_)
        ));
        assert_eq!(chat_runtime(&app, chat).composer.text(), text);
        assert!(!app.chat_ui.drafts.contains_key(&terminal));
        let snapshot = app.persistent_workspace();
        assert!(!snapshot.chat_drafts.contains_key(&terminal));
        assert_eq!(snapshot.chat_drafts.get(&chat).unwrap(), text);
    }

    /// A stopped terminal whose relaunch is still starting may yet run its
    /// agent, so no second conversation is opened for it meanwhile.
    #[test]
    fn a_starting_terminal_opens_no_conversation_beside_it() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = stopped_agent_terminal(&mut app, &[], false);
        app.begin_terminal_start(card);
        assert!(app.pending_terminal_spawn(card));
        let before = app.durable.clone();
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        assert_eq!(app.durable.cards, before.cards);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), [card]);
        assert!(
            !app.chat_ui
                .phases
                .values()
                .any(|phase| matches!(phase, Phase::Starting(_)))
        );
        assert!(errors(&app).is_empty(), "a wait, not a failure");
    }

    #[test]
    fn a_bypass_is_recognised_in_every_form_it_is_carried() {
        let recipe = |arguments: &[&str], environment: &[(&str, &str)]| LaunchRecipe {
            unset_environment: Default::default(),
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

    #[test]
    fn a_fresh_claude_runtime_displays_the_selected_accounts_auto_default() {
        let config = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("settings.json"),
            r#"{"permissions":{"defaultMode":"auto"}}"#,
        )
        .unwrap();
        let account = crate::account::Account::managed(
            Provider::Claude,
            uuid::Uuid::new_v4(),
            config.path().into(),
        );
        let session = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            config.path().into(),
            &account,
        )
        .unwrap();
        let runtime = Runtime::new(
            Target {
                card: CardId::new(),
                generation: 0,
            },
            session,
            Connection::Draft,
            true,
            Default::default(),
        );
        assert_eq!(runtime.snapshot.permission_mode.as_deref(), Some("auto"));

        let mut recipe = runtime.session.launch.recipe.clone();
        recipe.arguments.clear();
        recipe
            .environment
            .insert("CLAUDE_CODE_PERMISSION_MODE".into(), "manual".into());
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Claude).as_deref(),
            Some("manual")
        );
        recipe
            .unset_environment
            .insert("CLAUDE_CODE_PERMISSION_MODE".into());
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Claude).as_deref(),
            Some("auto")
        );
    }

    #[test]
    fn codex_draft_permissions_wait_for_project_and_custom_policy_layers() {
        let home = tempfile::tempdir().unwrap();
        let account = home.path().join(".codex");
        let project = home.path().join("project");
        let cwd = project.join("nested");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::create_dir_all(project.join(".codex")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let user_policy = "approval_policy = 'never'\nsandbox_mode = 'danger-full-access'\n";
        std::fs::write(account.join("config.toml"), user_policy).unwrap();
        let mut recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: Vec::new(),
            working_directory: cwd,
            environment: [
                ("HOME".into(), home.path().to_string_lossy().into_owned()),
                ("CODEX_HOME".into(), account.to_string_lossy().into_owned()),
            ]
            .into(),
        };
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("yolo")
        );
        for policy in [
            "approval_policy = 'on-request'",
            "sandbox_mode = 'read-only'",
            "default_permissions = 'restricted'",
            "[permissions.restricted]\nnetwork = false",
        ] {
            std::fs::write(project.join(".codex/config.toml"), policy).unwrap();
            assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
            recipe.arguments = vec!["--yolo".into()];
            assert_eq!(
                draft_permission_mode(&recipe, Provider::Codex).as_deref(),
                Some("yolo")
            );
            recipe.arguments = ["-a", "on-request", "-s", "read-only"]
                .map(String::from)
                .into();
            assert_eq!(
                draft_permission_mode(&recipe, Provider::Codex).as_deref(),
                Some("readOnly")
            );
            recipe.arguments.clear();
        }
        std::fs::remove_file(project.join(".codex/config.toml")).unwrap();
        for policy in [
            "default_permissions = ':read-only'",
            "[permissions.restricted]\nnetwork = false",
        ] {
            std::fs::write(
                account.join("config.toml"),
                format!("{user_policy}{policy}\n"),
            )
            .unwrap();
            assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
            recipe.arguments = ["-a", "on-request", "-s", "workspace-write"]
                .map(String::from)
                .into();
            assert_eq!(
                draft_permission_mode(&recipe, Provider::Codex).as_deref(),
                Some("workspaceWrite")
            );
            recipe.arguments.clear();
        }
        std::fs::write(account.join("config.toml"), user_policy).unwrap();
        // An account directory linked into the cwd hierarchy must not be
        // mistaken for an independent project override.
        recipe.working_directory = home.path().join("elsewhere");
        recipe.unset_environment.insert("HOME".into());
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("yolo")
        );
    }

    #[test]
    fn codex_draft_permissions_follow_the_selected_account_and_explicit_flags() {
        let home = tempfile::tempdir().unwrap();
        let config = "approval_policy = 'never'\nsandbox_mode = 'danger-full-access'\n";
        std::fs::write(home.path().join("config.toml"), config).unwrap();
        let mut recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from(FIXTURE_DIRECTORY),
            environment: [(
                "CODEX_HOME".into(),
                home.path().to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
        };
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("yolo")
        );
        for arguments in [
            vec!["--profile", "restricted"],
            vec!["-c", "sandbox_mode='read-only'"],
            vec!["--config=approval_policy='on-request'"],
        ] {
            recipe.arguments = arguments.into_iter().map(String::from).collect();
            assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
        }
        recipe.arguments = ["-a", "on-request", "--sandbox=workspace-write"]
            .map(String::from)
            .into();
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("workspaceWrite")
        );
        set_draft_permission_mode(&mut recipe, Provider::Codex, "readOnly");
        assert!(
            !recipe
                .arguments
                .iter()
                .any(|arg| arg.starts_with("--sandbox="))
        );
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("readOnly")
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
            config
        );

        recipe.arguments.clear();
        recipe.working_directory = home.path().to_path_buf();
        recipe.environment.insert("CODEX_HOME".into(), ".".into());
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("yolo")
        );
        recipe
            .environment
            .insert("HOME".into(), home.path().to_string_lossy().into_owned());
        recipe.unset_environment = ["CODEX_HOME".into(), "HOME".into()].into();
        assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
        recipe.unset_environment.clear();

        recipe.arguments.clear();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(
            other.path().join("config.toml"),
            "approval_policy = 'on-request'\nsandbox_mode = 'read-only'\n",
        )
        .unwrap();
        recipe.environment.insert(
            "CODEX_HOME".into(),
            other.path().to_string_lossy().into_owned(),
        );
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("readOnly")
        );
        std::fs::write(
            other.path().join("config.toml"),
            "approval_policy = 'untrusted'\nsandbox_mode = 'read-only'\n",
        )
        .unwrap();
        assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
        std::fs::write(other.path().join("config.toml"), "malformed TOML [").unwrap();
        assert_eq!(draft_permission_mode(&recipe, Provider::Codex), None);
    }

    #[test]
    fn draft_permission_choice_sets_the_first_turns_launch_policy() {
        let mut recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "claude".into(),
            arguments: vec!["--permission-mode".into(), "auto".into()],
            working_directory: PathBuf::from(FIXTURE_DIRECTORY),
            environment: Default::default(),
        };
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Claude).as_deref(),
            Some("auto")
        );
        set_draft_permission_mode(&mut recipe, Provider::Claude, "bypassPermissions");
        assert_eq!(recipe.arguments, ["--permission-mode", "bypassPermissions"]);
        set_draft_permission_mode(&mut recipe, Provider::Claude, "manual");
        assert_eq!(recipe.arguments, ["--permission-mode", "manual"]);

        recipe.arguments.clear();
        recipe.environment.insert(
            "CLAUDE_CODE_PERMISSION_MODE".into(),
            "bypassPermissions".into(),
        );
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Claude).as_deref(),
            Some("bypassPermissions")
        );
        set_draft_permission_mode(&mut recipe, Provider::Claude, "manual");
        assert!(
            !recipe
                .environment
                .contains_key("CLAUDE_CODE_PERMISSION_MODE")
        );

        recipe.program = "codex".into();
        recipe.arguments = vec!["--remote".into(), "unix://".into()];
        set_draft_permission_mode(&mut recipe, Provider::Codex, "yolo");
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("yolo")
        );
        assert!(recipe.arguments.contains(&"--yolo".into()));
        set_draft_permission_mode(&mut recipe, Provider::Codex, "workspaceWrite");
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("workspaceWrite")
        );
        assert_eq!(
            &recipe.arguments[2..],
            &[
                "--ask-for-approval",
                "on-request",
                "--sandbox",
                "workspace-write"
            ]
        );
        set_draft_permission_mode(&mut recipe, Provider::Codex, "readOnly");
        assert_eq!(
            draft_permission_mode(&recipe, Provider::Codex).as_deref(),
            Some("readOnly")
        );
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
        let attempt = app.chat_ui.attempt(card).unwrap();
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
        let attempt = app.chat_ui.attempt(card).unwrap();
        let (target, fresh) = (attempt.target, attempt.session.clone());
        assert_eq!(fresh.launch.session_id, None);
        let handle = fixture_handle(&app, card, Some("fresh"));
        answer_start(&mut app, target, Ok(handle));
        let stored = flushed(&app);
        assert_eq!(identity(&stored), identity(&fresh));
        assert_eq!(stored.launch.session_id.as_deref(), Some("fresh"));
        // A late or repeated press, once nothing offers it, keeps it.
        let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
        assert!(!app.chat_ui.attempt(card).is_some());
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
                Arc::make_mut(&mut runtime.snapshot).items.push(
                    chat::Item {
                        id: "prompt".into(),
                        role: chat::Role::User,
                        content: text("hello"),
                        started_at: None,
                        at: None,
                    }
                    .into(),
                );
                runtime.target
            }
            _ => unreachable!(),
        };
        let _ = app.update(Message::ChatUi(Action::AccountPicker(target, true)));
        let _ = app.update(Message::ChatUi(Action::ChooseAccount(target, managed)));
        assert!(!app.chat_ui.phases.values().any(|phase| matches!(
            phase,
            Phase::Closing {
                restart: Some(_),
                ..
            }
        )));
        assert!(
            !matches!(app.chat_ui.phases.get(&target.card), Some(Phase::Closing { target: active, .. }) if *active == target)
        );
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
        assert!(
            matches!(app.chat_ui.phases.get(&target.card), Some(Phase::Closing { target: active, .. }) if *active == target)
        );
        close_chat(&mut app, card);
        assert!(
            app.chat_ui.retiring.is_empty(),
            "the restart's close is the retirement"
        );
        let _ = app.update(Message::ChatUi(Action::CloseFinished(target, Ok(()))));
        assert!(
            !app.chat_ui
                .phases
                .values()
                .any(|phase| matches!(phase, Phase::Starting(_))),
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
        assert!(
            !app.chat_ui
                .phases
                .values()
                .any(|phase| matches!(phase, Phase::Starting(_)))
        );
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
        assert!(
            matches!(app.chat_ui.phases.get(&target.card), Some(Phase::Closing { target: active, .. }) if *active == target)
        );
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
            !app.chat_ui
                .phases
                .values()
                .any(|phase| matches!(phase, Phase::Starting(_))),
            "the replacement started in the worktree being deleted"
        );
        assert!(!matches!(
            app.chat_ui.phases.get(&card),
            Some(Phase::Closing {
                restart: Some(_),
                ..
            })
        ));
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
        assert_eq!(app.workspace.pending_close(), None);
        let _ = app.update(Message::CommitClose(card)); // Duplicate must not create another terminal.
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.cards.contains_key(&card));
        assert!(!app.chat_ui.phases.contains_key(&card));
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
        assert!(order.contains(&terminal));
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
    fn a_chat_with_no_conversation_to_return_to_opens_its_provider_cli() {
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
            assert_eq!(app.chat_switch_label(card), "Open in terminal");
            assert!(app.chat_switch_ready(card));
            let _ = app.update(Message::ChatUi(Action::Switch(card)));
            assert!(errors(&app).is_empty(), "{:?}", errors(&app));
            let (target, plan) = local_return(&app);
            assert!(!plan.resumes);
            let cli = plan.launch.clone();
            assert_eq!(cli.program, "codex");
            assert_eq!(cli.working_directory, PathBuf::from(FIXTURE_DIRECTORY));
            assert!(
                !cli.arguments
                    .iter()
                    .any(|argument| argument.starts_with("--remote"))
            );
            assert_eq!(cli.environment["CODEX_HOME"], home.display().to_string());
            assert_eq!(
                cli.unset_environment,
                app.default_account(Provider::Codex)
                    .credential_overrides()
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect()
            );
            assert!(!app.chat_switch_ready(card), "one return at a time");

            // The fresh CLI shares no conversation with the actor, so doubt about the
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
            assert_replaced_by_terminal(&mut app, card, &cli);
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
            let (target, plan) = local_return(&app);
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
        let target = local_return(&app).0;
        let _ = app.update(Message::CloseCard(card));
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(
            target,
            Err(ReturnRefused::SourceLive),
        )));
        assert!(terminal_cards(&app).is_empty());
        assert!(!errors(&app).is_empty(), "the refusal says why");
        app.notices.entries.clear();
        // The queued close retains ownership until the daemon proves cleanup.
        assert_eq!(app.workspace.pending_close(), None);
        let _ = app.update(Message::CommitClose(card));
        assert!(app.workspace.order().any(|id| id == card));
        assert!(matches!(
            app.chat_ui.phases.get(&card),
            Some(Phase::Closing { restart: None, .. })
        ));
        close_chat(&mut app, card);
        assert!(app.chat_ui.retiring.is_empty());

        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = local_return(&app).0;
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
        let target = app
            .chat_ui
            .phases
            .values()
            .find_map(|phase| match phase {
                Phase::Returning { target, .. } => Some(*target),
                _ => None,
            })
            .unwrap();
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
            let target = local_return(&app).0;
            let _ = app.update(Message::CloseCard(card));
            assert!(matches!(
                app.chat_ui.phases.get(&card),
                Some(Phase::Returning {
                    close_requested: true,
                    ..
                })
            ));
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
            assert!(!app.chat_ui.phases.values().any(|phase| matches!(
                phase,
                Phase::Returning {
                    close_requested: true,
                    ..
                }
            )));
            assert_eq!(
                app.chat_ui.retiring.is_empty(),
                confirmed,
                "a conversation not confirmed stopped is given up again"
            );
            assert_eq!(
                app.workspace.pending_close(),
                None,
                "the close commits with the return"
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
            let target = local_return(&app).0;
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
            assert!(!app.chat_ui.phases.contains_key(&card));
            assert!(app.card_state(card).unwrap().existence.is_open());
            assert!(matches!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(_)
            ));
            assert!(app.chat_switch_ready(card), "it can be tried again");
            assert!(!app.chat_ui.transition_pending());
        }
    }

    #[test]
    fn resume_is_read_from_a_draft_naming_it() {
        assert_eq!(resume_invocation("/resume").as_deref(), Some(""));
        assert_eq!(
            resume_invocation("  /Resume  flaky tests \n").as_deref(),
            Some("flaky tests")
        );
        assert_eq!(resume_invocation("/resumes"), None);
        assert_eq!(resume_invocation("resume"), None);
        assert_eq!(resume_invocation("please /resume"), None);
    }

    /// `/resume` sends nothing: it opens the picker on the conversation's
    /// provider and project, searching for what follows it, and an untouched
    /// draft gives its place to the conversation chosen there.
    #[test]
    fn resume_command_opens_the_picker_for_this_conversation() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = tempfile::tempdir().unwrap();
        let card = draft_in(&mut app, project.path());
        write(&mut app, card, "/resume flaky tests");
        submit(&mut app, card);
        assert!(app.resume_ui.open);
        assert_eq!(app.resume_ui.query, "flaky tests");
        let scope = app.resume_ui.scope.as_ref().expect("a narrowed search");
        assert_eq!(scope.provider, Provider::Codex);
        assert!(
            scope
                .roots
                .iter()
                .any(|root| project.path().starts_with(root))
        );
        let runtime = chat_runtime(&app, card);
        assert!(runtime.composer.is_empty());
        assert!(runtime.is_draft() && !app.chat_start_pending(card));
        assert_eq!(app.resume_ui.replace, Some(card));

        let _ = app.resume_saved_conversation(earlier(&app, card));
        let resumed = app.workspace.focused().unwrap();
        assert_ne!(resumed, card);
        assert_eq!(app.card_state(resumed).unwrap().title, "Earlier");
        assert!(!app.resume_ui.open);
        assert_eq!(
            app.workspace
                .pending_close()
                .map(|handoff| handoff.unmounting),
            Some(card),
            "the draft gives its place to the conversation"
        );
    }

    /// A draft is replaced only while the picker it asked for is open.
    #[test]
    fn a_draft_outlives_a_cancelled_resume() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = tempfile::tempdir().unwrap();
        let card = draft_in(&mut app, project.path());
        write(&mut app, card, "/resume");
        submit(&mut app, card);
        assert_eq!(app.resume_ui.replace, Some(card));
        let _ = app.update_resume(super::resume_ui::Action::Close);
        assert_eq!(app.resume_ui.replace, None);
        let _ = app.resume_saved_conversation(earlier(&app, card));
        assert!(app.workspace.pending_close().is_none());
        assert!(app.workspace.order().any(|id| id == card));
    }

    /// A `/resume` the picker cannot answer is left for the writer to send
    /// again.
    #[test]
    fn resume_keeps_its_draft_while_another_dialog_holds_the_window() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let project = tempfile::tempdir().unwrap();
        let card = draft_in(&mut app, project.path());
        write(&mut app, card, "/resume flaky");
        app.resume_ui.open = true;
        submit(&mut app, card);
        assert_eq!(
            chat_runtime(&app, card).composer.text().trim(),
            "/resume flaky"
        );
        assert_eq!(app.resume_ui.replace, None);
        assert!(app.resume_ui.query.is_empty());
    }

    /// A new conversation's draft in `directory`, a folder that exists.
    fn draft_in(app: &mut Cagents, directory: &Path) -> CardId {
        let _ = app.new_native_chat(Provider::Codex, Some(directory.into()));
        let card = app.workspace.focused().unwrap();
        assert!(chat_runtime(app, card).is_draft());
        card
    }

    /// Another conversation in the project of `card`'s, as resume lists it.
    fn earlier(app: &Cagents, card: CardId) -> crate::persistence::SavedConversation {
        let mut session = chat_runtime(app, card).session.clone();
        session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        // Resumed directly, without a daemon to prepare it.
        session.native_continuity = chat::NativeContinuity::ReconnectOnly;
        crate::persistence::SavedConversation {
            card: CardId::new(),
            title: "Earlier".into(),
            session,
            open: false,
            updated_at_unix_ms: 1,
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
                title_source: Default::default(),
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

    #[test]
    fn failed_workspace_commit_keeps_runtime_operation_and_saved_identity() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let other = failed_chat(&mut app);
        let (card, target) = starting_chat(&mut app);
        let source = app.card_state(card).unwrap().clone();
        let order = app.workspace.order().collect::<Vec<_>>();
        assert!(app.commit_card_removal(card, None).is_none());
        app.workspace.request_close(other).unwrap();
        assert!(app.commit_card_removal(card, None).is_none());
        assert_eq!(app.card_state(card), Some(&source));
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
        assert_eq!(app.chat_ui.attempt(card).unwrap().target, target);
        assert!(app.cards.contains_key(&card));
        assert!(app.chat_ui.abandoned.is_empty());
        assert!(app.chat_ui.retiring.is_empty());
        app.workspace.cancel_close(other).unwrap();
        app.notices.entries.clear();
        app.status = None;
        close_chat(&mut app, card);
    }

    #[test]
    fn repeated_removal_request_reuses_its_reservation() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let _ = app.request_card_removal(card);
        let reservation = app.workspace.pending_close();
        let _ = app.request_card_removal(card);
        assert_eq!(app.workspace.pending_close(), reservation);
        assert!(app.status.is_none());
        assert!(app.card_state(card).unwrap().existence.is_open());
        close_chat(&mut app, card);
    }

    #[test]
    fn late_handoff_reply_cannot_consume_a_restart_close() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target) = unresumable_chat(&mut app);
        let _ = app.update(Message::ChatUi(Action::StartFresh(card)));
        let _ = app.update(Message::ChatUi(Action::HandoffFinished(
            target,
            Err("old handoff".into()),
        )));
        assert!(matches!(
            app.chat_ui.phases.get(&card),
            Some(Phase::Closing { .. })
        ));
        let _ = app.update(Message::ChatUi(Action::CloseFinished(target, Ok(()))));
        assert!(app.chat_ui.attempt(card).is_some());
    }

    #[test]
    fn local_return_places_terminal_before_a_scope_switch_can_interleave() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let _left = failed_chat(&mut app);
        let card = failed_chat(&mut app);
        let _right = failed_chat(&mut app);
        let before = app.workspace.order().collect::<Vec<_>>();
        let slot = before.iter().position(|id| *id == card).unwrap();
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = local_return(&app).0;
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
        let after = app.workspace.order().collect::<Vec<_>>();
        let terminal = terminal_cards(&app)[0];
        let mut expected = before;
        expected[slot] = terminal;
        assert_eq!(after, expected);
        assert_eq!(app.workspace.pending_close(), None);
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(app.card_state(terminal).unwrap().existence.is_open());
        app.workspace =
            crate::workspace::Workspace::new(crate::workspace::WorkspaceConfig::default()).unwrap();
        let _ = app.update(Message::CommitClose(card));
        assert!(app.card_state(terminal).unwrap().existence.is_open());
        assert!(errors(&app).is_empty(), "{:?}", errors(&app));
    }

    #[test]
    fn deletion_started_during_local_return_keeps_the_source_card() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let source = app.card_state(card).unwrap().clone();
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        let target = local_return(&app).0;
        app.project_ui
            .begin_deleting(PathBuf::from(FIXTURE_DIRECTORY));
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
        assert_eq!(app.card_state(card).unwrap().kind, source.kind);
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.workspace.order().any(|id| id == card));
        assert!(terminal_cards(&app).is_empty());
        assert_eq!(app.workspace.pending_close(), None);
        assert!(!app.chat_ui.transition_pending());
    }

    /// Retry between a chat's close and its commit starts nothing, and a start
    /// still in flight when a card is discarded is given up when it answers
    /// rather than leaving a daemon actor no card can reach.
    #[test]
    fn a_discarded_card_never_strands_a_start() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = failed_chat(&mut app);
        let _ = app.update(Message::CloseCard(card));
        let closing = app.chat_ui.phases[&card].target();
        let saved = app.chat_return_session(card).unwrap();
        let _ = app.update(Message::RetryCard(card));
        assert!(
            matches!(app.chat_ui.phases.get(&card), Some(Phase::Closing { target, restart: None }) if *target == closing),
            "retry cannot replace a pending exact close with a new provider start"
        );
        assert_eq!(app.chat_return_session(card), Some(saved.clone()));
        let _ = app.update(Message::ChatUi(Action::RetainedCloseFinished(
            closing,
            saved,
            Ok(()),
        )));
        let _ = app.update(Message::RetryCard(card));
        assert!(!app.chat_start_pending(card));
        assert!(!app.card_state(card).unwrap().existence.is_open());

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
        let target = app
            .chat_ui
            .phases
            .values()
            .find_map(|phase| match phase {
                Phase::Returning { target, .. } => Some(*target),
                _ => None,
            })
            .unwrap();
        let _ = app.update(Message::CloseCard(card));
        assert!(matches!(
            app.chat_ui.phases.get(&card),
            Some(Phase::Returning {
                close_requested: true,
                ..
            })
        ));
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
            } else if let Some(closing) =
                app.chat_ui.phases.values().find_map(|phase| match phase {
                    Phase::Closing { target, .. } => Some(*target),
                    _ => None,
                })
            {
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
    fn a_chat_replaced_by_its_provider_cli_keeps_its_channel() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let channel = crate::model::ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            title_source: Default::default(),
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
        let (target, plan) = local_return(&app);
        let cli = plan.launch.clone();
        assert_eq!(cli.program, "codex");
        let _ = app.update(Message::ChatUi(Action::ReturnedLocally(target, Ok(()))));
        assert_replaced_by_terminal(&mut app, card, &cli);
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
        let failed = app.chat_ui.attempt(card).unwrap().session.id;
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
        let attempt = app.chat_ui.attempt(card).unwrap();
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
        let started = app.chat_ui.attempt(card).unwrap().session.clone();
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
            items: vec![
                chat::Item {
                    id: "prompt".into(),
                    role: chat::Role::User,
                    content: text("hello"),
                    started_at: None,
                    at: None,
                }
                .into(),
            ],
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
        assert!(
            !app.chat_ui
                .phases
                .values()
                .any(|phase| matches!(phase, Phase::Returning { .. })),
            "no handoff was asked for"
        );
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

    #[test]
    fn removed_account_without_available_alternatives_still_offers_accounts() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some(KNOWN_THREAD));
        let session = chat_runtime(&app, card).session.clone();
        assert!(
            app.chat_accounts(session.launch.provider, &session.launch.recipe)
                .is_none()
        );
        app.chat_ui.removed_accounts.insert(card);
        assert!(app.account_recovery_controls(card).is_some());
        let _ = app.update(Message::AccountUi(account_ui::Action::OpenPane));
        assert!(app.utility_card(&CardKind::Accounts).is_some());
        assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(session));
    }

    #[test]
    fn upgraded_identity_is_installed_only_after_its_exact_durable_receipt() {
        for (stale_reply, codex_resume) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, Some(KNOWN_THREAD));
            let mut source = chat_runtime(&app, card).session.clone();
            if codex_resume {
                source.launch.provider = Provider::Codex;
                source.launch.recipe.program = "codex".into();
                source.launch.recipe.arguments = vec!["--remote".into(), "unix://".into()];
                source.launch.session_id = None;
                let allocator = source.proof.clone().unwrap();
                assert!(source.claim_owned_codex_listener(
                    std::path::Path::new("/run/test-native"),
                    &allocator
                ));
                source.launch.session_id = Some(KNOWN_THREAD.into());
                source.native_continuity = chat::NativeContinuity::Durable {
                    provider_version: "0.100.0".into(),
                };
                app.card_state_mut(card).unwrap().kind = CardKind::Chat(source.clone());
            }
            let directory = tempfile::tempdir().unwrap();
            let (writer, _) = crate::session::SessionRuntime::open(
                directory.path().join("state.sqlite3"),
                crate::model::BootId::new("boot-test"),
            )
            .unwrap();
            app.session = Some(writer);
            app.persist();
            app.session
                .as_ref()
                .unwrap()
                .flush(Duration::from_secs(10))
                .unwrap();
            let mut replacement = source.clone();
            replacement.proof.as_mut().unwrap().host_incarnation = "upgraded-daemon".into();
            if codex_resume {
                replacement
                    .launch
                    .recipe
                    .arguments
                    .splice(0..0, ["resume".into(), KNOWN_THREAD.into()]);
            } else {
                replacement.launch.session_id = Some("00000000-0000-4000-8000-000000000002".into());
            }
            let handle = Arc::new(ClientHandle::fixture_upgrade(
                source.clone(),
                replacement.clone(),
                Snapshot {
                    session_id: replacement.launch.session_id.clone(),
                    ready: true,
                    ..Snapshot::default()
                },
            ));
            let target = app.chat_ui.next_target(card);
            app.chat_ui.phases.insert(
                card,
                Phase::Starting(Attempt {
                    target,
                    session: source.clone(),
                    recovery: None,
                    upgrade_committed: false,
                }),
            );
            app.cards.remove(&card);
            let task = app.update(Message::ChatUi(Action::Ready(
                target,
                Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
            )));
            assert!(app.chat_start_pending(card));
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            assert!(!app.cards.contains_key(&card));
            let receipt = account_commit_reply(task);
            assert!(matches!(
                receipt,
                Message::ChatUi(Action::UpgradeCommitted(_, _, Ok(())))
            ));
            if stale_reply {
                let next = app.chat_ui.next_target(card);
                app.chat_ui.phases.insert(
                    card,
                    Phase::Starting(Attempt {
                        target: next,
                        session: source.clone(),
                        recovery: None,
                        upgrade_committed: false,
                    }),
                );
                let _ = app.update(receipt);
                assert_eq!(app.chat_ui.attempt(card).unwrap().target, next);
                assert!(!app.chat_ui.attempt(card).unwrap().upgrade_committed);
                assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(source));
                assert!(!app.cards.contains_key(&card));
            } else {
                let _ = app.update(receipt);
                assert!(!app.chat_start_pending(card));
                assert_eq!(
                    app.card_state(card).unwrap().kind,
                    CardKind::Chat(replacement.clone())
                );
                assert_eq!(chat_runtime(&app, card).session, replacement.clone());
                app.persist();
                let writer = app.session.as_ref().unwrap();
                writer.flush(Duration::from_secs(10)).unwrap();
                assert_eq!(writer.last_error(), None);
                assert_eq!(
                    writer
                        .operation_store()
                        .load_card(card, None)
                        .unwrap()
                        .unwrap()
                        .kind,
                    CardKind::Chat(replacement)
                );
            }
        }
    }

    #[test]
    fn account_recovery_keeps_the_saved_identity_until_exact_confirmation() {
        for close_before_ack in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, Some(KNOWN_THREAD));
            let mut source = chat_runtime(&app, card).session.clone();
            source
                .launch
                .recipe
                .environment
                .insert("CODEX_HOME".into(), "/removed-account".into());
            app.install_chat_session(card, source.clone());
            let directory = tempfile::tempdir().unwrap();
            let (writer, _) = crate::session::SessionRuntime::open(
                directory.path().join("state.sqlite3"),
                crate::model::BootId::new("boot-test"),
            )
            .unwrap();
            app.session = Some(writer);
            app.persist();
            let saved = |app: &Cagents| {
                let writer = app.session.as_ref().unwrap();
                writer.flush(Duration::from_secs(10)).unwrap();
                assert_eq!(writer.last_error(), None);
                writer
                    .operation_store()
                    .load_card(card, None)
                    .unwrap()
                    .unwrap()
                    .kind
            };
            assert_eq!(saved(&app), CardKind::Chat(source.clone()));
            app.chat_forget(card);
            app.cards.insert(card, CardRuntime::Loading);
            let _ = app.open_chat(card, source.clone(), chat::Intent::Restore);
            let target = chat_runtime_attempt(&app, card);
            answer_start(
                &mut app,
                target,
                Err(chat::StartFailure {
                    message: "The saved account was removed".into(),
                    recovery: chat::Recovery::Account,
                }),
            );
            assert!(app.chat_ui.removed_accounts.contains(&card));
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            let system = Account::system(Provider::Codex).id();
            let _ = app.update(Message::ChatUi(Action::RecoverAccount(
                card,
                system.clone(),
            )));
            let target = chat_runtime_attempt(&app, card);
            assert_ne!(app.chat_ui.attempt(card).unwrap().session.id, source.id);
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            let _ = app.update(Message::ChatUi(Action::RecoverAccount(
                card,
                system.clone(),
            )));
            assert_eq!(
                chat_runtime_attempt(&app, card),
                target,
                "double click does not create another candidate"
            );
            answer_start(
                &mut app,
                target,
                Err(chat::StartFailure::retry(
                    "Conversation unavailable on this account",
                )),
            );
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            assert!(app.chat_ui.removed_accounts.contains(&card));
            let retirement = retired(&mut app, card);
            let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
            assert_eq!(saved(&app), CardKind::Chat(source.clone()));
            let _ = app.update(Message::ChatUi(Action::RecoverAccount(
                card,
                system.clone(),
            )));
            let target = chat_runtime_attempt(&app, card);
            let handle = fixture_handle(&app, card, Some("different-conversation"));
            answer_start(&mut app, target, Ok(handle));
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            let retirement = retired(&mut app, card);
            let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
            let _ = app.update(Message::ChatUi(Action::RecoverAccount(
                card,
                system.clone(),
            )));
            let target = chat_runtime_attempt(&app, card);
            let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
            let mut competing = handle.session();
            competing.reset_owned_codex_listener();
            competing.id = TerminalId::new();
            // A different durable owner wins before this candidate can commit.
            let store = app.session.as_ref().unwrap().operation_store();
            store
                .rebind_chat_account(card, &source, &competing)
                .unwrap();
            let task = app.update(Message::ChatUi(Action::Ready(
                target,
                Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
            )));
            assert!(app.chat_start_pending(card));
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            let reply = account_commit_reply(task);
            assert!(matches!(
                &reply,
                Message::ChatUi(Action::AccountCommitted(_, _, Err(_)))
            ));
            let _ = app.update(reply);
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            assert_eq!(
                store.load_card(card, None).unwrap().unwrap().kind,
                CardKind::Chat(competing.clone())
            );
            assert!(app.chat_ui.removed_accounts.contains(&card));
            let retirement = retired(&mut app, card);
            let _ = app.update(Message::ChatUi(Action::Retired(retirement, Ok(()))));
            store
                .rebind_chat_account(card, &competing, &source)
                .unwrap();

            let _ = app.update(Message::ChatUi(Action::RecoverAccount(card, system)));
            let target = chat_runtime_attempt(&app, card);
            let handle = fixture_handle(&app, card, Some(KNOWN_THREAD));
            let task = app.update(Message::ChatUi(Action::Ready(
                target,
                Ready(Arc::new(Mutex::new(Some(Ok(handle))))),
            )));
            assert!(app.chat_start_pending(card));
            assert_eq!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(source.clone())
            );
            // An unrelated save while acknowledgment is pending still contains
            // the old descriptor; only that descriptor is translated by the lane.
            app.card_state_mut(card).unwrap().title = "Title edited while committing".into();
            app.persist();
            if close_before_ack {
                app.notices.entries.clear();
                close_chat(&mut app, card);
                let reply = account_commit_reply(task);
                let _ = app.update(reply);
                app.session
                    .as_ref()
                    .unwrap()
                    .flush(Duration::from_secs(10))
                    .unwrap();
                let stored = store.load_card(card, None).unwrap().unwrap();
                assert!(
                    !stored.existence.is_open(),
                    "commit acknowledgment cannot resurrect a closed card"
                );
                assert!(!app.cards.contains_key(&card));
                retired(&mut app, card);
                continue;
            }
            let reply = account_commit_reply(task);
            let _ = app.update(reply);
            let CardKind::Chat(recovered) = &app.card_state(card).unwrap().kind else {
                panic!("chat retained");
            };
            assert_ne!(recovered.id, source.id);
            assert_eq!(recovered.launch.session_id, source.launch.session_id);
            assert_eq!(saved(&app), CardKind::Chat(recovered.clone()));
            assert_eq!(
                store.load_card(card, None).unwrap().unwrap().title,
                "Title edited while committing"
            );
            assert!(!app.chat_ui.removed_accounts.contains(&card));
        }
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
        assert_eq!(
            runtime.session.launch.recipe.arguments,
            ["--remote", "unix://"]
        );
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
            unset_environment: Default::default(),
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

    #[test]
    fn unnamed_hosted_chat_attachment_finishes_hydration_without_replacing_identity() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, None);
        let session = chat_runtime(&app, card).session.clone();
        assert!(session.launch.session_id.is_none());
        assert!(!session.is_unstarted());
        app.chat_forget(card);
        app.cards.insert(card, CardRuntime::Loading);
        app.card_state_mut(card).unwrap().residency = Residency::Cold;
        app.lifecycle
            .register(card, Exposure::Offscreen, Residency::Cold, Instant::now());
        let _ = app.open_chat(card, session.clone(), chat::Intent::Restore);
        let target = chat_runtime_attempt(&app, card);
        let handle = Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                busy: true,
                ..Snapshot::default()
            },
        ));
        answer_start(&mut app, target, Ok(handle));
        let runtime = chat_runtime(&app, card);
        assert!(!runtime.is_draft());
        assert!(runtime.handle().unwrap().snapshot().busy);
        // Hidden cards attach for activity without building presentation state.
        // Revealing the adopted card must render that existing busy snapshot.
        app.chat_runtime_mut(target).unwrap().show(true);
        let runtime = chat_runtime(&app, card);
        assert!(runtime.snapshot.busy);
        assert_eq!(runtime.session, session);
        assert_eq!(app.card_state(card).unwrap().residency, Residency::Hot);
        assert_eq!(app.card_state(card).unwrap().kind, CardKind::Chat(session));
        assert!(!app.chat_start_pending(card));
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
        assert!(app.chat_switch_ready(id));
        let _ = app.update(Message::ChatUi(Action::Switch(id)));
        assert!(!app.chat_switch_ready(id));
        assert!(app.chat_switch_blocker(id).unwrap().contains("Connecting"));
        assert!(
            app.notices.entries.is_empty(),
            "progress is shown in the pane, without a warning"
        );
        let target = app.chat_ui.phases[&id].target();
        let _ = app.update(Message::ChatUi(Action::Switch(id)));
        assert_eq!(app.chat_ui.phases[&id].target(), target);
        assert!(app.notices.entries.iter().any(|entry| entry.notice.message
            == "Connecting to native chat. Wait for the connection to finish."));
        let before = app.card_state(id).unwrap().clone();
        let reason = "This Codex terminal does not expose its conversation for native attachment.";
        let task = app.update(Message::ChatUi(Action::Ready(
            target,
            Ready(Arc::new(Mutex::new(Some(Err(chat::StartFailure::retry(
                reason,
            )))))),
        )));
        assert_eq!(
            app.card_state(id).unwrap(),
            &before,
            "a failed switch preserves the terminal"
        );
        assert!(
            app.chat_switch_ready(id),
            "a failed attachment can be retried"
        );
        assert_eq!(errors(&app), [reason]);
        assert!(
            matches!(
                crate::app::tests::task_app_message(task),
                Message::ChromeUi(chrome_ui::Action::ShowNoticesFor(card)) if card == id
            ),
            "the failure opens its explanation"
        );
    }

    #[test]
    fn a_chat_switch_names_the_unsent_attachment_blocker_before_clicking() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            unreachable!()
        };
        runtime.attachments.push(Attachment {
            path: PathBuf::from("/tmp/unsent-attachment.png"),
        });
        let reason = "Send or remove the attached files before leaving the chat.";
        assert_eq!(app.chat_switch_blocker(card).as_deref(), Some(reason));
        assert!(!app.chat_switch_ready(card));
        let _ = app.update(Message::ChatUi(Action::Switch(card)));
        assert!(!app.chat_start_pending(card));
        assert!(matches!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(_)
        ));
        assert!(
            app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.message == reason)
        );
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            unreachable!()
        };
        runtime.attachments.clear();
        assert!(app.chat_switch_ready(card));
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
            parsed_codes(&whole),
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
            streamed.extend(body, THEME);
            streamed.token = ChangeToken::of(&content);
        }
        assert_eq!(parsed_codes(&streamed), parsed_codes(&whole));
        assert_eq!(streamed.token, whole.token);
        assert_eq!(
            streamed.blocks.keys().collect::<HashSet<_>>(),
            whole.blocks.keys().collect::<HashSet<_>>(),
            "the streamed fences are highlighted under the same identities"
        );
    }

    /// A fence that names its language is highlighted in both of the
    /// editor's palettes the moment it is cached, as one paragraph whose lines
    /// are joined the way the fence wrote them, and looked up by exactly what
    /// the Markdown hands the renderer; a fence without one is cached to draw
    /// plain rather than guessed at.
    #[test]
    fn a_named_fence_is_highlighted_in_both_palettes() {
        const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
        let cached = CachedItem::new(
            &text("```rust\nfn main() {}\nlet x = 1;\n```\n\n```\nplain\n```\n"),
            THEME,
        );
        let text_of = |spans: &[iced::widget::text::Span<'static, String>]| {
            spans
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>()
        };
        let block = cached
            .block(Some("rust"), "fn main() {}\nlet x = 1;\n")
            .expect("the Rust fence is cached");
        for dark in [false, true] {
            let spans = block.spans(dark);
            assert_eq!(text_of(spans), "fn main() {}\nlet x = 1;\n");
            assert!(
                spans.iter().any(|span| span.color.is_some()),
                "a keyword takes a color"
            );
        }
        let plain = cached
            .block(None, "plain\n")
            .expect("an unnamed fence is cached too");
        assert_eq!(text_of(plain.spans(true)), "plain\n");
        assert!(plain.spans(true).iter().all(|span| span.color.is_none()));
        assert!(cached.block(Some("rust"), "fn other() {}\n").is_none());
    }

    /// Everything an item's Markdown would put on screen, block by block, as
    /// the reader would read it.
    fn shown(cached: &CachedItem) -> String {
        fn walk(items: &[markdown::Item], style: markdown::Style, out: &mut String) {
            let line = |text: &markdown::Text, out: &mut String| {
                for span in text.spans(style).iter() {
                    out.push_str(&span.text);
                }
                out.push('\n');
            };
            for item in items {
                match item {
                    markdown::Item::Heading(_, text) | markdown::Item::Paragraph(text) => {
                        line(text, out)
                    }
                    markdown::Item::CodeBlock { language, code, .. } => {
                        out.push_str(&format!("[{}]\n{code}", language.as_deref().unwrap_or("")));
                    }
                    markdown::Item::List { bullets, .. } => {
                        for bullet in bullets {
                            let (markdown::Bullet::Point { items }
                            | markdown::Bullet::Task { items, .. }) = bullet;
                            out.push_str("* ");
                            walk(items, style, out);
                        }
                    }
                    markdown::Item::Quote(items) => walk(items, style, out),
                    markdown::Item::Rule => out.push_str("----\n"),
                    markdown::Item::Table { columns, rows } => {
                        for column in columns {
                            walk(&column.header, style, out);
                        }
                        for cell in rows.iter().flat_map(|row| &row.cells) {
                            walk(cell, style, out);
                        }
                    }
                    markdown::Item::Image { alt, .. } => line(alt, out),
                }
            }
        }
        let mut out = String::new();
        walk(
            cached.markdown.items(),
            markdown::Style::from(iced::Theme::Dark),
            &mut out,
        );
        out
    }

    /// The items a parse produced, in full but for the per-line highlighting
    /// iced keeps on a code block, which the transcript never draws and
    /// which depends on how the block was streamed.
    fn outline(items: &[markdown::Item]) -> String {
        items
            .iter()
            .map(|item| match item {
                markdown::Item::CodeBlock { language, code, .. } => {
                    format!("CodeBlock({language:?}, {code:?})")
                }
                markdown::Item::List { start, bullets } => {
                    let bullets = bullets
                        .iter()
                        .map(|bullet| match bullet {
                            markdown::Bullet::Point { items } => outline(items),
                            markdown::Bullet::Task { items, done } => {
                                format!("{done}:{}", outline(items))
                            }
                        })
                        .collect::<Vec<_>>();
                    format!("List({start:?}, {bullets:?})")
                }
                markdown::Item::Quote(items) => format!("Quote({})", outline(items)),
                item => format!("{item:?}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What an agent writes reaches the reader: a separator does not swallow
    /// the section under it, a generic keeps its parameters, and an indented
    /// block is a code block with its code in it.
    #[test]
    fn an_answer_keeps_what_the_parser_would_drop() {
        const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
        for (body, expected) in [
            (
                "Intro\n\n---\nSecond part\nstill second\n\n---\nThird part\n",
                "Intro\n----\nSecond part still second\n----\nThird part\n",
            ),
            ("+++\nkept\n+++\nafter\n", "+++ kept +++ after\n"),
            (
                "Returns Vec<Option<String>> or HashMap<K, V>.\n",
                "Returns Vec<Option<String>> or HashMap<K, V>.\n",
            ),
            (
                "<details>\n<summary>More</summary>\n\nbody\n</details>\n",
                "<details> <summary>More</summary>\nbody </details>\n",
            ),
            ("Press <kbd>Ctrl</kbd> now\n", "Press <kbd>Ctrl</kbd> now\n"),
            ("one<br>two<br/>three\n", "one\ntwo\nthree\n"),
            (
                "See <https://example.com/a> and `Vec<T>`.\n",
                "See https://example.com/a and Vec<T>.\n",
            ),
            (
                "Run:\n\n    cargo test\n    cargo fmt\n\nThen stop.\n",
                "Run:\n[]\ncargo test\ncargo fmt\nThen stop.\n",
            ),
            (
                "```rust\nlet x: Vec<T> = a < b;\n---\n```\n",
                "[rust]\nlet x: Vec<T> = a < b;\n---\n",
            ),
            (
                "| a | b |\n|---|---|\n| one<br>two | Option<T> |\n",
                "a\nb\none\ntwo\nOption<T>\n",
            ),
        ] {
            assert_eq!(
                shown(&CachedItem::new(&text(body), THEME)),
                expected,
                "{body:?}"
            );
        }
        let linked = CachedItem::new(&text("See <https://example.com/a>."), THEME);
        assert!(
            format!("{:?}", linked.markdown.items())
                .contains("link: Some(\"https://example.com/a\")"),
            "an autolink is still a link"
        );
    }

    /// However an answer is cut into deltas, it parses into the items the
    /// whole answer parses into - including across the restart a late `>`
    /// forces on an autolink or a line break.
    ///
    /// The bodies hold no list whose items are separated by blank lines:
    /// iced's incremental parse starts a second list at such an item, with or
    /// without the rewrite, and that is not what this is about.
    #[test]
    fn a_streamed_answer_parses_as_the_whole_one_does() {
        const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
        for body in [
            "Intro\n\n---\nNext <part>\n\n---\n\nUse Vec<Option<String>> and `Vec<T>`.\n",
            "See <https://example.com/a> or <me@example.com>.\n\n| a | b |\n|---|---|\n| one<br>two | Option<T> |\n",
            "Before\n\n```rust,ignore\nfn a<T>() {}\n---\n```\n\n~~~\n<b>\n~~~\n\nAfter <c>\n",
            "Run:\n\n    cargo test\n\n    cargo fmt\n\nThen:\n\n1. One\n\n   ```sh\n   echo <x>\n   ```\n\nDone <kbd>k</kbd>\n",
            "<details>\n<summary>S</summary>\n\nλ body\n</details>\n\n+++\nlast\n",
        ] {
            let whole = CachedItem::new(&text(body), THEME);
            let boundaries: Vec<usize> = (1..=body.len())
                .filter(|end| body.is_char_boundary(*end))
                .collect();
            // A character at a time, then in deltas of a few uneven sizes.
            let mut cuts = vec![boundaries.clone()];
            for stride in [2, 3, 5, 11] {
                let mut cut: Vec<usize> = boundaries.iter().copied().step_by(stride).collect();
                cut.push(body.len());
                cuts.push(cut);
            }
            for cut in cuts {
                let mut streamed = CachedItem::new(&text(""), THEME);
                for end in cut {
                    let content = text(&body[..end]);
                    if streamed.token == ChangeToken::of(&content) {
                        continue;
                    }
                    let grown = streamed
                        .token
                        .appended(&content)
                        .expect("a longer prefix of the same body reads as an append");
                    streamed.extend(grown, THEME);
                    streamed.token = ChangeToken::of(&content);
                }
                assert_eq!(
                    outline(streamed.markdown.items()),
                    outline(whole.markdown.items()),
                    "{body:?}"
                );
                assert_eq!(
                    streamed.blocks.keys().collect::<HashSet<_>>(),
                    whole.blocks.keys().collect::<HashSet<_>>()
                );
            }
        }
    }

    /// A fence's info string names its language first and says other things
    /// after; the grammar and the caption both go by the language.
    #[test]
    fn a_fence_is_highlighted_by_the_first_word_of_its_info_string() {
        const THEME: crate::settings::EditorSyntaxTheme = crate::settings::EditorSyntaxTheme::Auto;
        for (info, language) in [
            ("rust", Some("rust")),
            ("rust,ignore", Some("rust")),
            ("ts title=\"x.ts\"", Some("ts")),
            ("  python  {1,3}", Some("python")),
            (",", None),
            ("", None),
        ] {
            assert_eq!(prose::fence_language(info), language, "{info:?}");
        }
        let cached = CachedItem::new(&text("```rust,ignore\nfn main() {}\n```\n"), THEME);
        let block = cached
            .block(Some("rust,ignore"), "fn main() {}\n")
            .expect("the fence is cached under the info string the parser hands over");
        assert!(
            block.spans(true).iter().any(|span| span.color.is_some()),
            "a keyword takes a color"
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
