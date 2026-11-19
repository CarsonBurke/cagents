mod session_links;
use super::*;
use crate::account::{Account, AccountId};
use crate::agent::{Activity, Provider};
use crate::app::account_ui::ChatAccounts;
use crate::chat::client::{ClientHandle, Wake};
use crate::chat::{self, Attachment, Command, ItemContent, PromptMode, Snapshot};
use crate::file_drop;
use crate::model::{SavedQueue, SentAfter};
use crate::performance::chat_stage::{self, Stage};
use base64::Engine as _;
use cosmic::widget::text_editor;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// A value the whole application shares, such as an admission queue. Each
/// libtest thread is its own application, so a concurrent test can neither
/// take nor release what this test holds. Call it on the UI thread and keep
/// the returned `Arc`; it stays valid on whichever thread it moves to.
macro_rules! application_shared {
    ($shared:ty, $init:expr) => {{
        #[cfg(not(test))]
        let shared = {
            static SHARED: std::sync::OnceLock<Arc<$shared>> = std::sync::OnceLock::new();
            SHARED.get_or_init(|| Arc::new($init)).clone()
        };
        #[cfg(test)]
        let shared = {
            thread_local! {
                static SHARED: Arc<$shared> = Arc::new($init);
            }
            SHARED.with(Arc::clone)
        };
        shared
    }};
}

/// A bounded executor that the whole application shares, under the same
/// per-test-thread rule as `application_shared`. A permit keeps its semaphore
/// alive on whichever thread it drops.
macro_rules! application_capacity {
    ($permits:expr) => {
        application_shared!(
            tokio::sync::Semaphore,
            tokio::sync::Semaphore::new($permits)
        )
    };
}

/// A zeroed atomic the whole application shares, such as which selection owns
/// the keyboard, under the same per-test-thread rule as
/// `application_shared`. The returned reference may be stored and used from
/// any thread. Each test thread leaks one cell.
macro_rules! application_atomic {
    ($vis:vis fn $name:ident() -> $atomic:ident) => {
        $vis fn $name() -> &'static $atomic {
            #[cfg(not(test))]
            {
                static VALUE: $atomic = $atomic::new(0);
                &VALUE
            }
            #[cfg(test)]
            {
                thread_local! {
                    static VALUE: &'static $atomic = Box::leak(Box::new($atomic::new(0)));
                }
                VALUE.with(|value| *value)
            }
        }
    };
}

mod artifacts;
mod attachment_draft;
mod auth_recovery;
#[cfg(test)]
pub(in crate::app) mod blocked_recovery_tests;
mod changes;
mod child_navigation;
pub(in crate::app) use child_navigation::SubagentTally;
mod clipboard_image;
mod close_refusal;
#[cfg(test)]
pub(in crate::app) use close_refusal::{close_refusal_dismiss_id, close_refusal_end_id};
mod closed_draft;
mod commands;
use commands::ChosenModel;
mod completion;
mod coordination;
#[cfg(test)]
mod diagnostic_chronology_tests;
mod disclosure;
mod failure;
mod find;
mod find_navigation;
mod flow_layout;
mod fold;
#[cfg(test)]
mod frozen_source_tests;
mod hover;
pub(super) mod images;
mod kept_draft;
use kept_draft::ConfirmedLoss;
use revised::Revised;
mod layout;
mod literal_content;
mod logical_prose;
mod markdown;
mod markdown_source;
mod monospace;
mod opening_name;
mod pending_admission;
mod permissions;
mod preparation;
mod prompt;
mod prose;
mod prose_document;
mod pull_request_links;
mod reasoning;
pub(super) use pull_request_links::Board as PullRequestBoard;
mod reader_source;
mod retained;
mod revised;
pub(super) use closed_draft::closed_draft_source_matches;
pub(super) mod blocked_recovery;
pub(in crate::app) use update_recovery::UpdateWait;
mod scroll;
mod selectable_prose;
mod selection;
mod step;
mod stopped;
mod text_copy;
mod thumbnails;
mod tool_text;
mod tool_title;
mod turn;
mod update_recovery;
mod view;
mod virtualized;
mod warm;

pub(super) fn invalidate_copy_intent() {
    text_copy::invalidate();
}

pub(super) fn prune_pasted_images() {
    clipboard_image::prune_cache();
}

/// Names lead the chat hierarchy; prose and operational metadata keep their
/// existing scale. Keep this local so workbench labels remain unchanged.
fn chat_title<'a>(
    text: impl Into<Cow<'a, str>> + 'a,
) -> cosmic::widget::text::Text<'a, cosmic::Theme, cosmic::Renderer> {
    design::body(text)
        .size(design::TEXT_PROSE)
        .font(iced::Font {
            weight: iced::font::Weight::Semibold,
            ..cosmic::font::default()
        })
}

fn action(action: Action) -> Message {
    Message::ChatUi(action)
}

/// How long a cancel that would also stop background subagents waits for
/// its second press.
const INTERRUPT_CONFIRMATION: Duration = Duration::from_secs(4);

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

impl Ready {
    /// Turns a started reply into a retryable failure, returning the actor the
    /// start produced so the caller decides what becomes of it.
    fn fail(&self, message: String) -> Option<Arc<ClientHandle>> {
        let mut reply = self.0.lock().ok()?;
        let started = reply.take().and_then(Result::ok);
        *reply = Some(Err(chat::StartFailure::retry(message)));
        started
    }
}

/// Waits for the session writer's answer to a card rebind.
async fn rebind_receipt(
    receipt: Result<std::sync::mpsc::Receiver<Result<(), String>>, String>,
) -> Result<(), String> {
    let receipt = receipt?;
    tokio::task::spawn_blocking(move || {
        receipt
            .recv()
            .map_err(|error| format!("Session writer disconnected: {error}"))
            .and_then(|result| result)
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|result| result)
}

#[derive(Debug, Clone)]
pub enum CopyPart {
    Text,
    Input,
    Output,
    Change(usize),
}

/// What a copy of an excerpted step puts ahead of the whole output it reads,
/// joined as `copy_text` joins a step shown whole: the command or input for
/// the step's own Copy, nothing for its output's. `None` for a part that has
/// no whole output to read.
fn step_copy_prefix(part: &CopyPart, content: &ItemContent) -> Option<String> {
    match (part, content) {
        (CopyPart::Text, ItemContent::Command { command, .. }) => Some(command.clone()),
        (CopyPart::Text, ItemContent::Tool { input, .. }) => Some(input.clone()),
        (CopyPart::Output, ItemContent::Command { .. } | ItemContent::Tool { .. }) => {
            Some(String::new())
        }
        _ => None,
    }
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
    PreparationAvailable,
    /// The daemon's answer about its update restore (see `update_recovery`).
    UpdateRecoveryAnswered(Result<crate::terminal::ipc::StartupState, String>),
    /// Time to ask the daemon about its update restore again.
    UpdateRecoveryDue,
    /// The daemon finished its update restore: release what it refused.
    UpdateRecoveryReady,
    /// Sends a first message the update restore refused, once the daemon no
    /// longer says that refusal stands.
    SendAgain(Target),
    /// Starts again a conversation the update stopped (see
    /// `blocked_recovery::Reopen::UpdateStopped`).
    ReopenStopped(CardId),
    FlowAvailable,
    FlowRequest(Target, flow_layout::Request),
    FlowReady(
        Target,
        flow_layout::Request,
        Option<Arc<flow_layout::Prepared>>,
    ),
    /// Points a draft at a model of any provider, or at a provider's default
    /// model where none is named.
    DraftModel(Target, Provider, Option<String>),
    /// Points a draft at another account of its provider.
    ChooseAccount(Target, AccountId),
    RecoverAccount(CardId, AccountId),
    /// Asks the device to list a conversation's account again: its home is
    /// still there, but the account list lost it.
    RestoreAccount(CardId),
    AccountRestored(CardId, Result<account::remote::RestoredHome, String>),
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
    ResumeCommitted(Target, Ready, Result<(), String>),
    RetainedCopyCommitted(CardId, Result<(), String>),
    /// The result of giving up a conversation's daemon actor after its card
    /// stopped holding it: a closed card, an abandoned start, or a start that
    /// finished for a card which could no longer hold it.
    Retired(Target, Result<(), String>),
    /// A chat card with no daemon handoff to take has given up its actor, so
    /// a terminal can take its place.
    ReturnedLocally(Target, Result<(), ReturnRefused>),
    Wake(Target, Wake),
    /// What the opening of a pane's conversation names its channel, read
    /// once because the chat's loaded window starts after it.
    OpeningName(CardId, String, Option<String>),
    UpdatesAcknowledged(Target, Result<(), String>),
    Reconnect(Target),
    DraftStartCheckpointed(Target, uuid::Uuid, Result<(), String>),
    /// Explicitly continue a settled saved-history reader in this same pane.
    /// Restoring a pane never acquires this launch permission.
    ResumeSaved(Target),
    CopyClosedDraft(Target, u64),
    DiscardClosedDraft(Target, u64),
    SignIn(Target, Account),
    ResumeAfterSignIn(Target),
    /// Starts a new conversation in the same session directory and working
    /// directory as a saved one that cannot be resumed.
    ///
    /// Separate from `Reconnect` because the two are not interchangeable: one
    /// asks for the saved conversation again, the other gives it up. A card
    /// that cannot resume offers both and does neither on its own.
    StartFresh(CardId),
    Composer(Target, text_editor::Action),
    /// The composer's draft while its conversation changes hands: it can be
    /// scrolled, selected and copied, but edits are dropped.
    ReadComposer(Target, text_editor::Action),
    /// Lets the laid-out editor move Up before starting prompt history.
    ComposerUp(Target),
    ComposerCursor(Target, text_editor::Cursor),
    Paste(Target),
    PastedImage(
        Target,
        attachment_draft::Ticket,
        Option<clipboard_image::Image>,
    ),
    PastedText(Target, attachment_draft::Ticket, Option<String>),
    /// A clipboard image the chat cannot take, offered without any text to
    /// paste in its place.
    PasteRefused(Target, attachment_draft::Ticket, &'static str),
    ImageSaved(Target, attachment_draft::Ticket, Result<Attachment, String>),
    AttachmentsPrepared(
        Target,
        attachment_draft::Ticket,
        Result<Vec<Attachment>, String>,
    ),
    Submit(Target, PromptMode),
    /// A press of one of the composer's send controls. It submits like
    /// [`Action::Submit`], then returns the keyboard to the draft: the control
    /// shows only while there is a draft, so the one just pressed is gone
    /// once the draft is sent, and its focus would otherwise go with it.
    SubmitControl(Target, PromptMode),
    /// Recalls the message sent before the one the composer shows, or the
    /// unsent draft again once the recall has walked forward past the newest.
    HistoryOlder(Target),
    HistoryNewer(Target),
    AppendProviderDraft(Target),
    Command(Target, Command),
    /// Cancels the active turn, or, where that would also stop subagents
    /// running in the background, asks for the cancel to be pressed again.
    Interrupt(Target),
    /// Lets a cancel waiting for its second press lapse, if it is that one.
    DisarmInterrupt(Target, Instant),
    CommandFinished(Target, u64, Result<(), String>),
    /// Looks again at a message the provider took once its turn has had
    /// time to begin, as no report may come to show it never will.
    CommandsDue(Target),
    Toggle(Target, String),
    /// Opens or closes one group's step strip, addressed by the id of the item
    /// that started the group.
    ToggleTurn(Target, String),
    /// Opens a child agent's transcript, or returns to the main conversation.
    SelectSubagent(Target, Option<String>),
    SubagentPicker(Target, bool),
    SubagentFailures(Target),
    SubagentQuery(Target, String),
    ToggleFailure(Target, String),
    CopyFailure(Target, String),
    FailureCopied(
        Target,
        String,
        text_copy::FailureRequest,
        Result<String, String>,
    ),
    SubagentFilter(Target, child_navigation::Filter),
    SubagentPage(Target, bool),
    /// Shows the whole of one folded part of a coordination card, addressed
    /// by the card's item id and the part's name, or folds it again.
    Unfold(Target, String),
    /// Brings forward the open session whose id starts with the given one,
    /// as an agent addressed it in a command.
    OpenSession(String),
    /// Brings forward the tab of the agent whose ID starts with the given
    /// one, as the CLI printed it when a conversation started the agent.
    OpenAgent(String),
    SessionResolved(Result<session_links::Destination, String>),
    SessionLinkOpened(Result<(), String>),
    SessionLinkWindowBackend(String, TerminalId, bool),
    SessionLinkToken(String, TerminalId, Option<String>),
    Copy(Target, String, CopyPart),
    CopyMessage(Target, String, Option<chat::TextCopySource>),
    /// Copies a step whose row shows only an excerpt of its output, reading
    /// the whole output from its source.
    CopyStep(Target, String, CopyPart, chat::TextCopySource),
    MessageCopied(Target, String, text_copy::Request, Result<String, String>),
    /// A complete message read for a window that shows only part of it.
    LogicalProseRead(Target, chat::TextCopySource, Result<String, String>),
    MarkdownPrepared(Target, String, u64, preparation::Ready),
    LocalPromptPrepared(Target, u64, preparation::Ready),
    RetryMarkdown(Target),
    OpenUrl(String),
    OpenDocument(Target, String),
    OpenSharedDocument(Target, markdown::Uri),
    OpenSharedUrl(Target, markdown::Uri),
    /// Captured by the rendered surface, independently of later focus changes.
    OpenDocumentFrom(Target, Option<PathBuf>, String),
    OpenChangedFile(Target, PathBuf),
    ChangedFileResolved(Target, u64, PathBuf, Result<PathBuf, String>),
    OpenImage(Target, String),
    LoadImage(Target, String),
    ImageLoaded(Target, Arc<chat::Item>, u64, images::Loaded),
    /// A Markdown image came near the viewport and may need loading.
    LoadFigure(Target, u64),
    FigureLoaded(Target, u64, u64, images::figure::Outcome),
    /// A while after a Markdown image could not be shown: it is looked at
    /// again if it is still near the viewport.
    RecheckFigure(Target, u64),
    /// A Markdown image left the viewport, so coming back asks for it again.
    FigureOutOfSight,
    /// A click or context menu entry acting on a Markdown image.
    Figure(Target, u64, images::figure::Command),
    ThumbnailLoaded(Target, Arc<PathBuf>, images::Loaded),
    RetryThumbnails(Target),
    /// Keyboard to the pane's image viewer, from its composer's Escape.
    FocusImage(Target),
    CloseImage(images::ViewerHost),
    ImageNext(images::ViewerHost, bool),
    ImageFit(images::ViewerHost),
    ImageExternal(images::ViewerHost),
    /// A context menu, viewer control or shortcut acting on displayed pixels.
    Image(images::ImageRef, images::ImageCommand),
    /// PNG encoding finished; `None` when a newer copy superseded it first.
    ImageCopied(
        text_copy::Intent,
        Result<Option<images::clipboard::Png>, String>,
    ),
    CopyLiteral(String),
    CopySharedCode(Target, Arc<str>),
    CopyQueued(Target, QueueCopy),
    Latest(Target),
    /// Takes a waiting message back into the composer to be revised: the one
    /// named, or the newest that can be taken back.
    TakeBackQueued(Target, Option<QueueEntry>),
    RemoveQueued(Target, QueueEntry),
    /// Puts away the notice that messages a provider was holding when it
    /// stopped are back in the composer.
    DismissRestoredWaiting(Target),
    ComposerActions(Target, bool),
    /// What the reader has typed into the `+` menu's search.
    ComposerActionsQuery(Target, String),
    /// Enter in the `+` menu's search: its first row that can be pressed.
    ComposerActionsSubmit(Target),
    /// A command or skill chosen from the `+` menu, by the text slash
    /// completion would put in the draft for it.
    ComposerName(Target, String),
    BranchPicker(Target, bool),
    ChangesPicker(Target, bool),
    ChangeFiles(Target, Option<chat::FileChangeKey>),
    ChangeSelect(Target, chat::FileChangeKey),
    ChangeBack(Target),
    ChangeListScrolled(Target, u64, u64, bool, f32),
    ChangePatches(Target, Option<chat::FilePatchCursor>),
    ChangePatch(Target, usize),
    ChangesLoaded(Target, u64, Result<chat::FileChangeReply, String>),
    Files(Target, PathBuf, Arc<Vec<String>>),
    Find(Target, String),
    FindNext(Target, bool),
    FindAvailable,
    FindRetry(Target),
    FindGroups(Target, u64, u64, u64, Option<find_navigation::RawResult>),
    FindHighlight(Target, u64),
    FindHighlighted(
        Target,
        u64,
        Arc<AtomicBool>,
        Option<find_navigation::HighlightResult>,
    ),
    FindResume(Target, find_navigation::Intent, bool),
    FindExpired(Target, find_navigation::Intent),
    FindIndexed(
        Target,
        find_navigation::Intent,
        find_navigation::DocumentEpoch,
        Arc<AtomicBool>,
        Option<find_navigation::CatalogResult>,
    ),
    FindLocated(
        Target,
        find_navigation::Intent,
        find_navigation::DocumentEpoch,
        Option<scroll::LogicalLocation>,
    ),
    CloseFind(Target),
    Tick,
    ClearCopied(Target, Instant),
    DropHover(Target, bool),
    Attach(Target),
    Attached(
        Target,
        attachment_draft::Ticket,
        Result<Option<Vec<PathBuf>>, String>,
    ),
    /// Files or text dropped on the conversation.
    Dropped(Target, file_drop::Payload),
    RemoveAttachment(Target, usize),
    Model(Target, String),
    Reasoning(Target, String, Option<String>),
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
    AcceptSuggestion(Target, chat::PromptSuggestion),
    /// Moves the completion highlight down, or up when `false`.
    CompletionMove(Target, bool),
    /// Sends the completion list away until the reader types again.
    CompletionDismiss(Target),
    /// Where the completion list has been scrolled to.
    CompletionScrolled(Target, f32),
    HandoffFinished(Target, Result<TerminalSession, String>),
    CloseFinished(Target, Result<(), String>),
    RetainedCloseFinished(Target, chat::Session, Result<(), String>),
    FreshCloseFinished(
        Target,
        Box<chat::Session>,
        Box<chat::Session>,
        Result<(), String>,
    ),
    ToggleBlockedDetails(CardId, chat::Session),
    CopyBlockedDetails(CardId, chat::Session),
    /// Copies the unsent message an unreachable agent's card would discard.
    CopyBlockedDraft(CardId, chat::Session),
    /// The processes of an unreachable agent's conversation the daemon
    /// proved still running.
    UnnamedAgentRunning(
        CardId,
        chat::Session,
        Vec<crate::terminal::ipc::ProcessStart>,
    ),
    /// What ending an unreachable agent's processes, before its card
    /// closes, left running.
    UnnamedAgentEnded(
        CardId,
        chat::Session,
        Result<Vec<crate::terminal::ipc::LostProcess>, String>,
    ),
    /// Asks the daemon what still runs for a conversation whose Close it
    /// refused.
    AskCloseHolders(Target, chat::Session),
    /// What still runs for a conversation whose Close was refused.
    CloseHolders(
        Target,
        chat::Session,
        Result<Vec<crate::terminal::ipc::LostProcess>, String>,
    ),
    /// Ends what the daemon proves still runs for the conversation, then
    /// closes it again.
    EndCloseHolders(Target),
    CloseHoldersEnded(
        Target,
        chat::Session,
        Result<Vec<crate::terminal::ipc::LostProcess>, String>,
    ),
    DismissCloseRefusal(Target),
    /// Native viewport geometry and, for deliberate navigation, its direction.
    /// A collapsed page can fit at both edges; geometry cannot choose its next page.
    Scrolled(Target, Option<String>, view::Scroll, Option<view::Page>),
    /// Opens or closes the model search.
    ModelPicker(Target, bool),
    PermissionPicker(Target, bool),
    ModelQuery(Target, String),
}

impl std::fmt::Debug for Action {
    /// The variant's name alone, so diagnostics such as the UI trace can tell
    /// the actions apart. App-message diagnostics must never expose secret
    /// answers, composer text, attachment paths, or provider authorization
    /// URLs, so no payload is formatted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What an action may confine itself to: one conversation, or the
/// background work of every visible one.
#[derive(Debug, Clone, Copy)]
pub(super) enum LocalScope {
    Card(CardId),
    Visible,
}

/// The facts a workspace projection reads that a conversation-local action
/// can change, captured on either side of it.
#[derive(PartialEq)]
pub(super) struct ProjectionInputs {
    card: Option<CardState>,
    presence: Option<PanePresence>,
    channel_title: Option<String>,
    scope: Option<std::path::PathBuf>,
    activity: Option<crate::agent::Activity>,
    attention: sidebar::AttentionSummary,
    attention_stale: bool,
    update: bool,
    preview: Option<usize>,
    cards: usize,
    channels: usize,
    active_channel: Option<crate::model::ChannelId>,
    focused: Option<CardId>,
}

impl ProjectionInputs {
    fn capture(app: &Cagents, scope: LocalScope) -> Self {
        let card = match scope {
            LocalScope::Card(card) => Some(card),
            LocalScope::Visible => None,
        };
        let state = card.and_then(|card| app.card_state(card));
        let session = state.and_then(|state| match &state.kind {
            CardKind::Chat(session) => Some(session.id),
            _ => None,
        });
        Self {
            card: state.cloned(),
            presence: card.map(|card| app.presence(card).clone()),
            channel_title: card.and_then(|card| {
                app.durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&card))
                    .map(|channel| channel.title.clone())
            }),
            // The worktree the pane holds, which its protections and
            // sidebar row follow.
            scope: card.and_then(|card| {
                app.worktree_owners
                    .get(&card)
                    .or_else(|| app.durable.card_scopes.get(&card))
                    .cloned()
            }),
            activity: card.and_then(|card| app.sidebar_state.card_activity(card)),
            attention: card.map_or_else(Default::default, |card| {
                app.sidebar_state.card_attention(card)
            }),
            // Attention can change owner without changing its summary.
            attention_stale: app.sidebar_state.attention_projection_stale(),
            update: card.is_some_and(|card| app.sidebar_state.has_card_update(card)),
            preview: session
                .and_then(|session| app.notification_ui.previews.get(&session))
                .map(|preview| preview.rank),
            cards: app.durable.cards.len(),
            channels: app.durable.channels.len(),
            active_channel: app.durable.active_channel,
            focused: app.workspace.focused(),
        }
    }

    /// A changed title reaches only the chrome, as a terminal's does; any
    /// other change reaches the whole projection.
    fn projection(&self, after: &Self) -> Projection {
        if self == after {
            return Projection::Unchanged;
        }
        let retitled = match (&self.card, &after.card) {
            (Some(before), Some(after)) => {
                before.title != after.title
                    && CardState {
                        title: after.title.clone(),
                        ..before.clone()
                    } == *after
            }
            _ => false,
        };
        let others_unchanged = self.presence == after.presence
            && self.channel_title == after.channel_title
            && self.scope == after.scope
            && self.activity == after.activity
            && self.attention == after.attention
            && self.attention_stale == after.attention_stale
            && self.update == after.update
            && self.preview == after.preview
            && self.cards == after.cards
            && self.channels == after.channels
            && self.active_channel == after.active_channel
            && self.focused == after.focused;
        if retitled && others_unchanged {
            Projection::Chrome
        } else {
            Projection::Full
        }
    }
}

impl Action {
    /// What the action confines itself to, when it never touches the
    /// workspace beyond the facts [`ProjectionInputs`] holds.
    ///
    /// These are a conversation's wakes and the results of the background
    /// work they start. An account report changes the window's accounts, so
    /// its wake is not among them.
    pub(super) fn local_scope(&self) -> Option<LocalScope> {
        match self {
            Self::Wake(_, Wake::AccountUsage) => None,
            Self::Wake(target, _)
            | Self::FlowRequest(target, ..)
            | Self::FlowReady(target, ..)
            | Self::LogicalProseRead(target, ..)
            | Self::MarkdownPrepared(target, ..)
            | Self::LocalPromptPrepared(target, ..)
            | Self::ThumbnailLoaded(target, ..)
            | Self::FindGroups(target, ..)
            | Self::FindHighlight(target, ..)
            | Self::FindHighlighted(target, ..)
            | Self::FindResume(target, ..)
            | Self::FindExpired(target, ..)
            | Self::FindIndexed(target, ..)
            | Self::FindLocated(target, ..)
            // Typing, caret moves and scrolling arrive at input rate.
            | Self::Composer(target, ..)
            | Self::ReadComposer(target, ..)
            | Self::ComposerCursor(target, ..)
            | Self::Scrolled(target, ..)
            | Self::CompletionScrolled(target, ..)
            | Self::CompletionMove(target, ..)
            | Self::ChangeListScrolled(target, ..)
            | Self::DropHover(target, ..)
            | Self::ClearCopied(target, ..) => Some(LocalScope::Card(target.card)),
            // Asking changes nothing, and an answer changes only what the
            // panes waiting on it say; releasing them is its own action.
            Self::PreparationAvailable
            | Self::FlowAvailable
            | Self::FindAvailable
            | Self::UpdateRecoveryAnswered(..)
            | Self::UpdateRecoveryDue => {
                Some(LocalScope::Visible)
            }
            _ => None,
        }
    }

    /// The variant's name, which carries none of its payload.
    fn name(&self) -> &'static str {
        match self {
            Self::PreparationAvailable => "PreparationAvailable",
            Self::UpdateRecoveryAnswered(..) => "UpdateRecoveryAnswered",
            Self::UpdateRecoveryDue => "UpdateRecoveryDue",
            Self::UpdateRecoveryReady => "UpdateRecoveryReady",
            Self::SendAgain(..) => "SendAgain",
            Self::ReopenStopped(..) => "ReopenStopped",
            Self::FlowAvailable => "FlowAvailable",
            Self::FlowRequest(..) => "FlowRequest",
            Self::FlowReady(..) => "FlowReady",
            Self::DraftModel(..) => "DraftModel",
            Self::ChooseAccount(..) => "ChooseAccount",
            Self::RecoverAccount(..) => "RecoverAccount",
            Self::RestoreAccount(..) => "RestoreAccount",
            Self::AccountRestored(..) => "AccountRestored",
            Self::DraftSkills(..) => "DraftSkills",
            Self::AccountPicker(..) => "AccountPicker",
            Self::DispatchQueued(..) => "DispatchQueued",
            Self::CancelQueued(..) => "CancelQueued",
            Self::Switch(..) => "Switch",
            Self::Ready(..) => "Ready",
            Self::AccountCommitted(..) => "AccountCommitted",
            Self::UpgradeCommitted(..) => "UpgradeCommitted",
            Self::ResumeCommitted(..) => "ResumeCommitted",
            Self::RetainedCopyCommitted(..) => "RetainedCopyCommitted",
            Self::Retired(..) => "Retired",
            Self::ReturnedLocally(..) => "ReturnedLocally",
            Self::Wake(..) => "Wake",
            Self::OpeningName(..) => "OpeningName",
            Self::UpdatesAcknowledged(..) => "UpdatesAcknowledged",
            Self::Reconnect(..) => "Reconnect",
            Self::DraftStartCheckpointed(..) => "DraftStartCheckpointed",
            Self::ResumeSaved(..) => "ResumeSaved",
            Self::CopyClosedDraft(..) => "CopyClosedDraft",
            Self::DiscardClosedDraft(..) => "DiscardClosedDraft",
            Self::SignIn(..) => "SignIn",
            Self::ResumeAfterSignIn(..) => "ResumeAfterSignIn",
            Self::StartFresh(..) => "StartFresh",
            Self::FreshCloseFinished(..) => "FreshCloseFinished",
            Self::ToggleBlockedDetails(..) => "ToggleBlockedDetails",
            Self::CopyBlockedDetails(..) => "CopyBlockedDetails",
            Self::CopyBlockedDraft(..) => "CopyBlockedDraft",
            Self::UnnamedAgentRunning(..) => "UnnamedAgentRunning",
            Self::UnnamedAgentEnded(..) => "UnnamedAgentEnded",
            Self::Composer(..) => "Composer",
            Self::ReadComposer(..) => "ReadComposer",
            Self::ComposerUp(..) => "ComposerUp",
            Self::ComposerCursor(..) => "ComposerCursor",
            Self::Paste(..) => "Paste",
            Self::PastedImage(..) => "PastedImage",
            Self::PastedText(..) => "PastedText",
            Self::PasteRefused(..) => "PasteRefused",
            Self::ImageSaved(..) => "ImageSaved",
            Self::AttachmentsPrepared(..) => "AttachmentsPrepared",
            Self::Submit(..) => "Submit",
            Self::SubmitControl(..) => "SubmitControl",
            Self::HistoryOlder(..) => "HistoryOlder",
            Self::HistoryNewer(..) => "HistoryNewer",
            Self::AppendProviderDraft(..) => "AppendProviderDraft",
            Self::Command(..) => "Command",
            Self::Interrupt(..) => "Interrupt",
            Self::DisarmInterrupt(..) => "DisarmInterrupt",
            Self::CommandFinished(..) => "CommandFinished",
            Self::CommandsDue(..) => "CommandsDue",
            Self::Toggle(..) => "Toggle",
            Self::ToggleTurn(..) => "ToggleTurn",
            Self::SelectSubagent(..) => "SelectSubagent",
            Self::SubagentPicker(..) => "SubagentPicker",
            Self::SubagentFailures(..) => "SubagentFailures",
            Self::SubagentQuery(..) => "SubagentQuery",
            Self::ToggleFailure(..) => "ToggleFailure",
            Self::CopyFailure(..) => "CopyFailure",
            Self::FailureCopied(..) => "FailureCopied",
            Self::SubagentFilter(..) => "SubagentFilter",
            Self::SubagentPage(..) => "SubagentPage",
            Self::Unfold(..) => "Unfold",
            Self::OpenSession(..) => "OpenSession",
            Self::OpenAgent(..) => "OpenAgent",
            Self::SessionResolved(..) => "SessionResolved",
            Self::SessionLinkOpened(..) => "SessionLinkOpened",
            Self::SessionLinkWindowBackend(..) => "SessionLinkWindowBackend",
            Self::SessionLinkToken(..) => "SessionLinkToken",
            Self::Copy(..) => "Copy",
            Self::CopyMessage(..) => "CopyMessage",
            Self::CopyStep(..) => "CopyStep",
            Self::MessageCopied(..) => "MessageCopied",
            Self::LogicalProseRead(..) => "LogicalProseRead",
            Self::MarkdownPrepared(..) => "MarkdownPrepared",
            Self::LocalPromptPrepared(..) => "LocalPromptPrepared",
            Self::RetryMarkdown(..) => "RetryMarkdown",
            Self::OpenUrl(..) => "OpenUrl",
            Self::OpenDocument(..) => "OpenDocument",
            Self::OpenSharedDocument(..) => "OpenSharedDocument",
            Self::OpenSharedUrl(..) => "OpenSharedUrl",
            Self::OpenDocumentFrom(..) => "OpenDocumentFrom",
            Self::OpenChangedFile(..) => "OpenChangedFile",
            Self::ChangedFileResolved(..) => "ChangedFileResolved",
            Self::OpenImage(..) => "OpenImage",
            Self::LoadImage(..) => "LoadImage",
            Self::ImageLoaded(..) => "ImageLoaded",
            Self::LoadFigure(..) => "LoadFigure",
            Self::FigureLoaded(..) => "FigureLoaded",
            Self::RecheckFigure(..) => "RecheckFigure",
            Self::FigureOutOfSight => "FigureOutOfSight",
            Self::Figure(..) => "Figure",
            Self::ThumbnailLoaded(..) => "ThumbnailLoaded",
            Self::RetryThumbnails(..) => "RetryThumbnails",
            Self::FocusImage(..) => "FocusImage",
            Self::CloseImage(..) => "CloseImage",
            Self::ImageNext(..) => "ImageNext",
            Self::ImageFit(..) => "ImageFit",
            Self::ImageExternal(..) => "ImageExternal",
            Self::Image(..) => "Image",
            Self::ImageCopied(..) => "ImageCopied",
            Self::CopyLiteral(..) => "CopyLiteral",
            Self::CopySharedCode(..) => "CopySharedCode",
            Self::CopyQueued(..) => "CopyQueued",
            Self::Latest(..) => "Latest",
            Self::TakeBackQueued(..) => "TakeBackQueued",
            Self::RemoveQueued(..) => "RemoveQueued",
            Self::DismissRestoredWaiting(..) => "DismissRestoredWaiting",
            Self::ComposerActions(..) => "ComposerActions",
            Self::ComposerActionsQuery(..) => "ComposerActionsQuery",
            Self::ComposerActionsSubmit(..) => "ComposerActionsSubmit",
            Self::ComposerName(..) => "ComposerName",
            Self::BranchPicker(..) => "BranchPicker",
            Self::ChangesPicker(..) => "ChangesPicker",
            Self::ChangeFiles(..) => "ChangeFiles",
            Self::ChangeSelect(..) => "ChangeSelect",
            Self::ChangeBack(..) => "ChangeBack",
            Self::ChangeListScrolled(..) => "ChangeListScrolled",
            Self::ChangePatches(..) => "ChangePatches",
            Self::ChangePatch(..) => "ChangePatch",
            Self::ChangesLoaded(..) => "ChangesLoaded",
            Self::Files(..) => "Files",
            Self::Find(..) => "Find",
            Self::FindNext(..) => "FindNext",
            Self::FindAvailable => "FindAvailable",
            Self::FindRetry(..) => "FindRetry",
            Self::FindGroups(..) => "FindGroups",
            Self::FindHighlight(..) => "FindHighlight",
            Self::FindHighlighted(..) => "FindHighlighted",
            Self::FindResume(..) => "FindResume",
            Self::FindExpired(..) => "FindExpired",
            Self::FindIndexed(..) => "FindIndexed",
            Self::FindLocated(..) => "FindLocated",
            Self::CloseFind(..) => "CloseFind",
            Self::Tick => "Tick",
            Self::ClearCopied(..) => "ClearCopied",
            Self::DropHover(..) => "DropHover",
            Self::Attach(..) => "Attach",
            Self::Attached(..) => "Attached",
            Self::Dropped(..) => "Dropped",
            Self::RemoveAttachment(..) => "RemoveAttachment",
            Self::Model(..) => "Model",
            Self::Reasoning(..) => "Reasoning",
            Self::PermissionMode(..) => "PermissionMode",
            Self::PermissionRetry(..) => "PermissionRetry",
            Self::PermissionDismiss(..) => "PermissionDismiss",
            Self::PermissionPage(..) => "PermissionPage",
            Self::PermissionRuleText(..) => "PermissionRuleText",
            Self::PermissionBehavior(..) => "PermissionBehavior",
            Self::PermissionEnvironmentText(..) => "PermissionEnvironmentText",
            Self::PermissionSetting(..) => "PermissionSetting",
            Self::Field(..) => "Field",
            Self::Choice(..) => "Choice",
            Self::Answer(..) => "Answer",
            Self::Complete(..) => "Complete",
            Self::AcceptSuggestion(..) => "AcceptSuggestion",
            Self::CompletionMove(..) => "CompletionMove",
            Self::CompletionDismiss(..) => "CompletionDismiss",
            Self::CompletionScrolled(..) => "CompletionScrolled",
            Self::HandoffFinished(..) => "HandoffFinished",
            Self::CloseFinished(..) => "CloseFinished",
            Self::RetainedCloseFinished(..) => "RetainedCloseFinished",
            Self::AskCloseHolders(..) => "AskCloseHolders",
            Self::CloseHolders(..) => "CloseHolders",
            Self::EndCloseHolders(..) => "EndCloseHolders",
            Self::CloseHoldersEnded(..) => "CloseHoldersEnded",
            Self::DismissCloseRefusal(..) => "DismissCloseRefusal",
            Self::Scrolled(..) => "Scrolled",
            Self::ModelPicker(..) => "ModelPicker",
            Self::PermissionPicker(..) => "PermissionPicker",
            Self::ModelQuery(..) => "ModelQuery",
        }
    }

    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(
            self,
            Self::Ready(..)
                | Self::DraftStartCheckpointed(..)
                | Self::FindAvailable
                | Self::FindGroups(..)
                | Self::FindHighlight(..)
                | Self::FindHighlighted(..)
                | Self::FindResume(..)
                | Self::FindExpired(..)
                | Self::FindIndexed(..)
                | Self::FindLocated(..)
                | Self::PreparationAvailable
                | Self::FlowAvailable
                | Self::FlowRequest(..)
                | Self::FlowReady(..)
                | Self::AccountCommitted(..)
                | Self::UpgradeCommitted(..)
                | Self::ResumeCommitted(..)
                | Self::RetainedCopyCommitted(..)
                | Self::Wake(..)
                | Self::UpdatesAcknowledged(..)
                | Self::CommandFinished(..)
                | Self::CommandsDue(..)
                | Self::ChangesLoaded(..)
                | Self::ChangedFileResolved(..)
                | Self::MessageCopied(..)
                | Self::LogicalProseRead(..)
                | Self::Attached(..)
                | Self::PastedImage(..)
                | Self::PastedText(..)
                | Self::PasteRefused(..)
                | Self::ImageSaved(..)
                | Self::AttachmentsPrepared(..)
                | Self::ImageLoaded(..)
                | Self::LoadFigure(..)
                | Self::FigureLoaded(..)
                | Self::RecheckFigure(..)
                | Self::FigureOutOfSight
                | Self::MarkdownPrepared(..)
                | Self::LocalPromptPrepared(..)
                | Self::ThumbnailLoaded(..)
                | Self::RetryThumbnails(..)
                | Self::HandoffFinished(..)
                | Self::ReturnedLocally(..)
                | Self::CloseFinished(..)
                | Self::RetainedCloseFinished(..)
                | Self::FreshCloseFinished(..)
                | Self::CloseHolders(..)
                | Self::CloseHoldersEnded(..)
                | Self::UnnamedAgentRunning(..)
                | Self::UnnamedAgentEnded(..)
                | Self::Retired(..)
                | Self::DispatchQueued(..)
                | Self::ClearCopied(..)
                | Self::DisarmInterrupt(..)
                | Self::Files(..)
                | Self::Tick
                | Self::AccountPicker(_, false)
                // The dialog's own controls; a pane's viewer sits under a
                // dialog like the rest of its pane.
                | Self::CloseImage(images::ViewerHost::Dialog)
                | Self::ImageNext(images::ViewerHost::Dialog, _)
                | Self::ImageFit(images::ViewerHost::Dialog)
                | Self::ImageExternal(images::ViewerHost::Dialog)
                | Self::Image(images::ImageRef::Viewer(images::ViewerHost::Dialog, _), _)
                | Self::ImageCopied(..)
        )
    }

    /// The card whose provider session this action starts afresh.
    fn starts_session(&self) -> Option<CardId> {
        match self {
            Self::Reconnect(target)
            | Self::ResumeSaved(target)
            | Self::ResumeAfterSignIn(target) => Some(target.card),
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
            | Self::ResumeSaved(target)
            | Self::SignIn(target, _)
            | Self::ResumeAfterSignIn(target)
            | Self::AcceptSuggestion(target, _)
            | Self::Submit(target, _)
            | Self::DiscardClosedDraft(target, _)
            | Self::AppendProviderDraft(target)
            | Self::Command(target, _)
            | Self::Interrupt(target)
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
    /// Whether this begins the launch or puts a saved conversation back,
    /// which decides whether a failure is reported as a start or a reopen.
    intent: chat::Intent,
    session: chat::Session,
    recovery: Option<AccountRecovery>,
    upgrade_committed: bool,
    /// Whether the daemon-prepared successor of a stranded retained card has
    /// been saved in that card's place.
    resume_committed: bool,
    /// Exact authoritative target; only authenticated upgrade resolution may bind its draft.
    upgrade_admission: Option<chat::Session>,
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
    SavingDraft {
        target: Target,
    },
    Starting(Box<Attempt>),
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
            Self::SavingDraft { target } => *target,
            Self::Starting(attempt) => attempt.target,
            Self::Returning { target, .. } | Self::Closing { target, .. } => *target,
        }
    }
}

/// The save intent is private until this exact writer accepts it. Neither
/// shared views nor a provider may borrow a candidate from a pending save.
struct DraftStartCheckpoint {
    target: Target,
    attempt: uuid::Uuid,
    source: chat::Session,
    candidate: chat::Session,
    writer: std::sync::Weak<crate::persistence::StateStore>,
    registry: uuid::Uuid,
    scope: Option<crate::workspace_model::views::Scope>,
    backend: Option<(String, String, String)>,
    revision: u64,
    text: String,
    resolved_text: String,
    attachments: Vec<Attachment>,
    model: Option<(String, Option<String>)>,
}

fn draft_start_backend() -> Option<(String, String, String)> {
    crate::device::selected().map(|device| {
        (
            device.name.clone(),
            device.host.clone(),
            device.executable.clone(),
        )
    })
}

/// Why an agent's card that never saved its provider's conversation ID can't
/// reopen it; [`blocked_recovery::Reopen::UnnamedAgent`] says what to do.
fn unnamed_agent_conversation(session: &chat::Session) -> String {
    format!(
        "{} hadn't reported this agent's conversation ID when cagents' background service stopped, so this tab has no saved conversation to reconnect to.",
        session.launch.provider.label()
    )
}

/// Why sending cannot move a conversation to another account yet: a
/// background subagent holds it, which the daemon would refuse to close.
fn account_switch_hold(snapshot: &chat::Snapshot) -> Option<String> {
    let hold = snapshot.work_hold();
    let held = hold.subagents();
    (!held.is_empty()).then(|| {
        format!(
            "Sending would move this conversation to another account, but it can't move while background work is still going. {} Stop {} in the conversation, then send again.",
            held.detail(),
            held.where_to_stop()
        )
    })
}

/// The account home `session` launches its provider in, as its launch names it.
fn chat_account_home(session: &chat::Session) -> Option<&str> {
    let variable = account::home_variable(session.launch.provider)?;
    session
        .launch
        .recipe
        .environment
        .get(variable)
        .map(String::as_str)
}

/// The account a refused start was on, which the account list lost but whose
/// home the device kept.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RestorableAccount {
    provider: Provider,
    key: uuid::Uuid,
    /// The home the refused start used, as its launch named it.
    home: String,
    /// While the device is restoring it.
    restoring: bool,
    /// Why the last restore left it unlisted.
    failure: Option<String>,
    /// A draft's message the refusal returned to its composer, sent once the
    /// account is back.
    resend: Option<Resend>,
}

/// What a refused draft was about to send. Once the reader edits the message
/// or chooses another account, it is no longer what they asked to send.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resend {
    text: String,
    /// The home the draft's account choice names; `None` while it lets the
    /// router choose.
    account: Option<String>,
}

impl Resend {
    fn of(runtime: &Runtime) -> Option<Self> {
        let text = runtime.composer.text();
        (runtime.is_draft() && !text.trim().is_empty()).then(|| Self {
            text,
            account: chat_account_home(&runtime.session).map(str::to_owned),
        })
    }
}

pub(super) struct State {
    channel_names: HashMap<Target, ChannelNameObservation>,
    /// What the opening of each pane's conversation named, read at most once
    /// while the window runs for a channel with no name: `None` while the
    /// read runs, then the name it found, if any.
    opening_names: HashMap<(CardId, String), Option<Option<String>>>,
    /// The image viewer as a dialog, for an image no conversation shows. A
    /// conversation's own images open in its pane: [`Runtime::image_viewer`].
    pub(super) image_dialog: Option<images::Gallery>,
    files: HashMap<PathBuf, Arc<Vec<String>>>,
    sender: async_channel::Sender<(Target, Wake)>,
    receiver: async_channel::Receiver<(Target, Wake)>,
    generation: u64,
    phases: HashMap<CardId, Phase>,
    draft_checkpoints: HashMap<CardId, DraftStartCheckpoint>,
    /// Transient user intent, never an execution proof. Automatic restoration
    /// cannot acquire this permission; an exact failed History start can retry.
    explicit_starts: HashMap<CardId, chat::Session>,
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
    draft_attachments: HashMap<CardId, Vec<Attachment>>,
    /// The conversation each card attached to when another window started its
    /// draft. If that window's start fails, the shared pane goes back to the
    /// same conversation as a draft, and so does this card.
    peer_started: HashMap<CardId, TerminalId>,
    /// The provider queue each card last saved, until a connection to its
    /// conversation shows whether the provider that held it is still there.
    waiting: HashMap<CardId, SavedQueue>,
    /// What the reader confirmed a card's close discards, for the
    /// conversation the question was about, so that close keeps no more
    /// than the rest to bring back.
    confirmed_losses: HashMap<CardId, (TerminalId, ConfirmedLoss)>,
    /// Cards reopening a conversation with the message their last card was
    /// still sending as it closed, with what the reader had said before it.
    /// Their composer drops it once the transcript shows it arrived.
    sending_on_close: HashMap<CardId, SentAfter>,
    /// Drafts another window started, whose cards are attaching to that
    /// conversation. Their carried-over unsent text may repeat its first
    /// message, which is checked once the transcript shows it.
    started_elsewhere: HashSet<CardId>,
    /// Automatically opened placeholders, discarded when their scope is left
    /// until the reader puts work into them or starts their conversation.
    default_chats: HashSet<CardId>,
    parked: HashMap<CardId, Box<Runtime>>,
    /// The cards whose saved conversation cannot be resumed by trying again.
    ///
    /// A card in this set has no chat runtime to hold the offer, because its
    /// start never produced one, so the failed card itself has to carry it.
    fresh_offers: HashSet<CardId>,
    /// Capacity failures before a chat runtime exists (saved-pane hydration).
    capacity_offers: HashSet<CardId>,
    /// Starts refused as nonretryable before a runtime exists.
    blocked_starts: HashMap<CardId, blocked_recovery::BlockedStart>,
    /// What the daemon last said about its update restore, for the panes
    /// that wait on it.
    update_recovery: update_recovery::Watch,
    /// Expanded diagnostics belong to the exact failed source, never a replacement.
    blocked_details: HashMap<CardId, chat::Session>,
    removed_accounts: HashSet<CardId>,
    /// Cards refused because the account list does not name their account,
    /// whose home is still on the device and can be restored.
    restorable_accounts: HashMap<CardId, RestorableAccount>,
    /// The model last chosen for each provider, and its effort, which a new
    /// conversation of that provider starts on.
    last_models: HashMap<Provider, (String, Option<String>)>,
}
impl Default for State {
    fn default() -> Self {
        let (sender, receiver) = async_channel::unbounded();
        Self {
            image_dialog: None,
            channel_names: HashMap::new(),
            opening_names: HashMap::new(),
            files: HashMap::new(),
            sender,
            receiver,
            generation: 0,
            phases: HashMap::new(),
            draft_checkpoints: HashMap::new(),
            explicit_starts: HashMap::new(),
            abandoned: HashMap::new(),
            retiring: HashSet::new(),
            drafts: HashMap::new(),
            draft_attachments: HashMap::new(),
            peer_started: HashMap::new(),
            waiting: HashMap::new(),
            confirmed_losses: HashMap::new(),
            sending_on_close: HashMap::new(),
            started_elsewhere: HashSet::new(),
            default_chats: HashSet::new(),
            parked: HashMap::new(),
            fresh_offers: HashSet::new(),
            capacity_offers: HashSet::new(),
            blocked_starts: HashMap::new(),
            update_recovery: update_recovery::Watch::default(),
            blocked_details: HashMap::new(),
            removed_accounts: HashSet::new(),
            restorable_accounts: HashMap::new(),
            last_models: HashMap::new(),
        }
    }
}
impl State {
    #[cfg(test)]
    pub(super) fn with_drafts(drafts: HashMap<CardId, String>) -> Self {
        Self {
            drafts,
            ..Self::default()
        }
    }
    pub(super) fn with_draft_attachments(
        drafts: HashMap<CardId, String>,
        draft_attachments: HashMap<CardId, Vec<Attachment>>,
    ) -> Self {
        Self {
            drafts,
            draft_attachments,
            ..Self::default()
        }
    }
    pub(super) fn with_waiting_messages(mut self, waiting: HashMap<CardId, SavedQueue>) -> Self {
        self.waiting = waiting;
        self
    }
}

impl State {
    /// Give parked chats a changed syntax theme: one can still be warm, and is
    /// shown again as it is.
    pub(super) fn set_parked_syntax_theme(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        for runtime in self.parked.values_mut() {
            runtime.set_syntax_theme(theme);
        }
    }

    pub(super) fn transition_pending(&self) -> bool {
        !self.phases.is_empty() || !self.abandoned.is_empty() || !self.retiring.is_empty()
    }

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
/// This bounded digest is a hint, not proof of complete content identity.
/// Earlier bytes may differ while length and trailing bytes remain identical.
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

/// A bounded hint describing an item's shape, length and trailing bytes.
///
/// Different complete contents can have the same token. Reusing a presentation
/// requires immutable item identity or exact content equality; extending its
/// parser additionally requires the exact previous text prefix.
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
            chat::ToolStatus::Stopped => 4,
            chat::ToolStatus::Waiting => 5,
            chat::ToolStatus::Ended => 6,
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
                shape: 9 + Self::status_offset(*status),
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
                shape: 14,
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
                shape: 15 + Self::status_offset(*status),
                body: output.len(),
                digest: tail_digest(tail_digest(tail_digest(0, name), input), output),
            },
            ItemContent::Image {
                path, description, ..
            } => Self {
                shape: 20,
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
                    chat::DeliveryKind::Report => 21,
                    chat::DeliveryKind::Message => 22,
                    chat::DeliveryKind::Notice => 23,
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
    #[cfg(test)]
    fn appended<'a>(&self, previous: &str, content: &'a ItemContent) -> Option<&'a str> {
        self.appended_known(previous, content, false)
    }
    fn appended_known<'a>(
        &self,
        previous: &str,
        content: &'a ItemContent,
        proven: bool,
    ) -> Option<&'a str> {
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
        if proven {
            return Some(text.as_str());
        }
        let parsed = text.get(..self.body)?;
        (parsed == previous).then_some(text.as_str())
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
        1_000..60_000 => format!("{:.1}s", millis as f64 / 1_000.0),
        _ => format!("{}m {}s", millis / 60_000, (millis % 60_000) / 1_000),
    }
}

/// Remember reader choices across bounded history pages without retaining transcript bodies.
fn reconcile_disclosure(opened: &mut Revised<HashMap<String, bool>>, turns: &[turn::Turn]) {
    if opened.len() > 4096 {
        let keys: HashSet<&str> = turns.iter().map(|turn| turn.key.as_str()).collect();
        if opened.keys().any(|key| !keys.contains(key.as_str())) {
            opened.edit().retain(|key, _| keys.contains(key.as_str()));
        }
    }
}

/// A page can retain a tool while evicting the prompt that named its group.
/// Match overlapping stable tool IDs before replacing the bounded snapshot.
fn carry_step_disclosures(
    opened: &mut Revised<HashMap<String, bool>>,
    search: &mut find::Disclosure,
    previous: &[turn::Turn],
    previous_items: &[Arc<chat::Item>],
    next: &[turn::Turn],
    next_items: &[Arc<chat::Item>],
) {
    // Streaming and status updates normally keep every group identity. No
    // disclosure can move in that case, so avoid scratch maps on the hot path.
    if previous.len() == next.len()
        && previous
            .iter()
            .zip(next)
            .all(|(old, new)| old.key == new.key)
    {
        return;
    }
    let choices: Vec<_> = previous
        .iter()
        .map(|turn| search.reader_turn(&turn.key, opened))
        .collect();
    let owners: HashMap<_, _> = previous
        .iter()
        .enumerate()
        .flat_map(|(index, turn)| {
            turn.steps
                .iter()
                .filter_map(move |step| Some((previous_items.get(step.index)?.id.as_str(), index)))
        })
        .collect();
    let previous_keys: HashSet<_> = previous.iter().map(|turn| turn.key.as_str()).collect();
    for turn in next {
        if previous_keys.contains(turn.key.as_str()) {
            continue;
        }
        // At most one counter per loaded group, never retained transcript data.
        let mut overlap = vec![0usize; previous.len()];
        for step in &turn.steps {
            if let Some(owner) = next_items
                .get(step.index)
                .and_then(|item| owners.get(item.id.as_str()))
                && choices[*owner].is_some()
            {
                overlap[*owner] += 1;
            }
        }
        if let Some((owner, count)) = overlap
            .iter()
            .enumerate()
            .max_by_key(|(index, count)| (**count, std::cmp::Reverse(*index)))
            && *count > 0
            && let Some(choice) = choices[owner]
        {
            search.inherit_turn(&turn.key, choice, opened);
        }
    }
}

/// How a step ended when that is worth a word: without doing what it was
/// asked. A step that worked says nothing, as its group's mark already does,
/// and one still running says so in its verb ("Running", "Editing").
fn unsettled_word(status: chat::ToolStatus) -> Option<&'static str> {
    let settled = matches!(
        status,
        chat::ToolStatus::Succeeded | chat::ToolStatus::Running
    );
    (!settled).then(|| status_word(status))
}

fn status_word(status: chat::ToolStatus) -> &'static str {
    match status {
        chat::ToolStatus::Running => "Running",
        chat::ToolStatus::Succeeded => "Completed",
        chat::ToolStatus::Failed => "Failed",
        chat::ToolStatus::Denied => "Denied",
        chat::ToolStatus::Stopped => "Stopped",
        chat::ToolStatus::Ended => "Ended",
        chat::ToolStatus::Waiting => "Waiting on background work",
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
        (
            CopyPart::Text,
            ItemContent::Image {
                path, description, ..
            },
        ) => joined(&[description, path]),
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

#[cfg(test)]
fn contains_text(content: &ItemContent, query: &str) -> bool {
    match content {
        ItemContent::Text { text, .. } => find::contains(text, query),
        ItemContent::Command {
            command,
            output,
            label,
            working_directory,
            ..
        } => {
            find::contains(command, query)
                || find::contains(output, query)
                || find::contains(label, query)
                || find::contains(working_directory, query)
        }
        ItemContent::Changes { changes, .. } => changes.iter().any(|change| {
            find::contains(&change.diff, query) || find::contains(&change.path, query) || matches!(&change.kind, chat::ChangeKind::Renamed { from } if find::contains(from, query))
        }),
        ItemContent::Tool {
            name,
            input,
            output,
            ..
        } => {
            find::contains(name, query)
                || find::contains(input, query)
                || find::contains(output, query)
        }
        ItemContent::WebSearch {
            query: search,
            results,
        } => {
            find::contains(search, query)
                || results.iter().any(|result| {
                    find::contains(&result.title, query) || find::contains(&result.url, query) || find::contains(&result.snippet, query)
                })
        }
        ItemContent::Image { path, description, .. } => {
            find::contains(path, query) || find::contains(description, query)
        }
        ItemContent::Delivery {
            from,
            sender,
            summary,
            body,
            ..
        } => {
            find::contains(from, query)
                || find::contains(sender, query)
                || find::contains(summary, query)
                || find::contains(body, query)
        }
    }
}

/// One transcript item's presentation, retained across refreshes.
///
/// Retain the immutable item without copying its body. Tail tokens are cheap
/// hints; exact content comparisons authorize parser and presentation reuse.
struct CachedItem {
    preparing: bool,
    /// A nonselectable loading caption, never conversation content.
    placeholder: bool,
    stamp: Option<(u64, String, String)>,
    preview: tool_text::Preview,
    tool_input: Option<tool_text::ToolText>,
    prepared_tools: Option<Arc<tool_text::PreparedSources>>,
    tool_output: Option<tool_text::ToolText>,
    patches: Vec<tool_text::ToolText>,
    token: ChangeToken,
    item: Option<Arc<chat::Item>>,
    /// Unique presentation identity for retained turns, including tail collisions.
    revision: u64,
    diagnostic: bool,
    report_expanded: Option<bool>,
    /// The body as the Markdown parser is given it, resumed on each append.
    source: markdown_source::Source,
    markdown: markdown::Content,
    reasoning: Option<reasoning::Preview>,
    document: Option<prose_document::Document>,
    prompt: Option<Box<prompt::Prepared>>,
    literal: Option<literal_content::Content>,
    flow_cache: flow_layout::Cache,
    reader_memory: scroll::Memory,
    reader_context: Option<String>,
    /// The conversation's Markdown images, which this item's images draw from.
    figures: images::figure::Shared,
    /// Each fenced block the Markdown parsed, ready to draw, keyed by the
    /// block's language and text: what a frame looks a block up by. The
    /// palette is the runtime's, applied when the block is highlighted, so a
    /// change of syntax theme rehighlights rather than restyles.
    blocks: HashMap<u64, prose::CodeBlock>,
    palette_views: Option<prose::Palettes>,
    /// The key of the last fence the Markdown held when its blocks were last
    /// prepared. Only that fence can still be growing, and its key changes
    /// with every byte it gains, so this is how the next pass finds the block
    /// to continue.
    open: Option<u64>,
    /// A step's trailing metadata — exit status and duration — formed once per
    /// change rather than once per frame.
    detail: Option<String>,
    image: Option<iced::widget::image::Handle>,
    image_source: Option<Arc<chat::Item>>,
    image_item: Option<Arc<chat::Item>>,
    image_unavailable: bool,
    image_error: Option<String>,
    image_attempt: u64,
    image_wait: Option<images::Pressure>,
    image_evicted: bool,
    /// The decoded preview's pixel size, kept after its pixels are released
    /// so the preview's room is held while they are read again.
    image_size: Option<(u32, u32)>,
    /// What the step did to work with other agents, where it did.
    coordination: Option<Box<coordination::Coordinated>>,
    /// What a plain tool call's row says it did, read from its input once
    /// per change to the call rather than while drawing or regrouping.
    title: Option<tool_title::Title>,
    /// The pull requests the prepared text links to or mentions by number,
    /// so a change of labels finds what it relabels without reading any
    /// text.
    pull_request_links: Box<[pull_request_links::Named]>,
}
impl Drop for CachedItem {
    fn drop(&mut self) {
        if self.markdown.has_retained_allocations()
            || self.source.retained_capacity() != 0
            || !self.blocks.is_empty()
            || self.prepared_tools.is_some()
            || self.prompt.is_some()
        {
            preparation::retire_presentation(preparation::Presentation {
                parsed: preparation::Parsed {
                    source: std::mem::take(&mut self.source),
                    markdown: std::mem::take(&mut self.markdown),
                },
                blocks: std::mem::take(&mut self.blocks),
                palettes: self.palette_views.take(),
                item: self.item.take(),
                prepared_tools: self.prepared_tools.take(),
                prompt: self.prompt.take(),
            });
        }
    }
}

/// What grouping reads from a transcript's presentations, each formed once
/// per change to its item.
struct Presentations<'c>(&'c HashMap<String, CachedItem>);

impl<'c> turn::Presented<'c> for Presentations<'c> {
    fn coordinated(&self, item: &chat::Item) -> Option<&'c coordination::Coordinated> {
        self.0.get(&item.id)?.coordination.as_deref()
    }
    fn titled(&self, item: &chat::Item) -> Option<&'c tool_title::Title> {
        self.0.get(&item.id)?.title_of(item)
    }
}

/// What a live transcript's grouping reads: its presentations, and the
/// failure notices it retains outside its items.
struct Grouping<'c> {
    presentations: Presentations<'c>,
    provider: Provider,
    failures: &'c [failure::Note],
}

impl<'c> turn::Presented<'c> for Grouping<'c> {
    fn coordinated(&self, item: &chat::Item) -> Option<&'c coordination::Coordinated> {
        self.presentations.coordinated(item)
    }
    fn titled(&self, item: &chat::Item) -> Option<&'c tool_title::Title> {
        self.presentations.titled(item)
    }
    fn separates(&self, typed: &chat::Item, incoming: &chat::Item) -> bool {
        failure::separates_prompt(self.provider, self.failures, &typed.id, &incoming.id)
    }
}

/// Format a recorded message time once when its metadata changes. The
/// current day only chooses the short label; it never supplies a missing time.
fn recorded_message_stamp(
    at: u64,
    today: Option<&chat::LocalTime>,
) -> Option<(u64, String, String)> {
    let time = chat::local_time(i64::try_from(at / 1000).ok()?)?;
    let full = format!(
        "{:04}-{:02}-{:02} {}",
        time.year + 1900,
        time.month + 1,
        time.day,
        time.clock()
    );
    let label = if today
        .is_some_and(|today| today.year == time.year && today.day_of_year == time.day_of_year)
    {
        time.clock()
    } else {
        full.clone()
    };
    Some((at, label, full))
}

impl CachedItem {
    /// Whether this item's Markdown, or Markdown it carries, shows an image.
    fn shows_figures(&self) -> bool {
        !self.markdown.images().is_empty()
            || self.coordination.as_ref().is_some_and(|coordinated| {
                coordinated
                    .contents()
                    .any(|content| !content.images().is_empty())
            })
    }
    /// Marks this message to be prepared again with the current labels, and
    /// gives the id to forget its preparation by. What is shown stays until
    /// the worker returns the relabelled text, as when the syntax theme
    /// changes.
    fn relabel(&mut self) -> Option<String> {
        // A closed report and a diagnostic are not prepared until shown, and
        // then with the labels of the time.
        if self.diagnostic || self.report_expanded == Some(false) {
            return None;
        }
        let id = self.item.as_ref()?.id.clone();
        self.preparing = true;
        self.revision = Self::next_revision();
        Some(id)
    }
    /// What `item`'s row says it did, when this presentation was formed from
    /// that very call. A presentation of an earlier state of the call, or one
    /// still being prepared, has none to offer.
    fn title_of(&self, item: &chat::Item) -> Option<&tool_title::Title> {
        self.title.as_ref().filter(|_| {
            self.item
                .as_deref()
                .is_some_and(|formed| std::ptr::eq(formed, item) || formed.content == item.content)
        })
    }

    fn preparing(item: &Arc<chat::Item>, theme: crate::settings::EditorSyntaxTheme) -> Self {
        let empty = ItemContent::Text {
            text: String::new(),
            thinking: false,
            phase: chat::Phase::Final,
        };
        let mut cached = Self::updated(&empty, theme, None, false, false, None);
        cached.preparing = true;
        cached.placeholder = true;
        cached.token = ChangeToken::of(&item.content);
        cached.item = Some(item.clone());
        // Preview decoding has its own bounded worker/admission. Its immutable
        // source metadata must also exist while literal text is being prepared.
        if matches!(item.content, ItemContent::Image { .. }) {
            cached.image_item = Some(item.clone());
            cached.image_source = Some(item.clone());
        }
        cached
    }

    fn preparing_replacement(
        item: &Arc<chat::Item>,
        theme: crate::settings::EditorSyntaxTheme,
        previous: Option<&Self>,
    ) -> Self {
        let mut cached = Self::preparing(item, theme);
        if let Some(previous) = previous.filter(|previous| {
            matches!(item.content, ItemContent::Image { .. })
                && previous
                    .image_item
                    .as_ref()
                    .is_some_and(|old| old.content == item.content)
        }) {
            // An equal-content replacement needs its own exact literal source,
            // but does not invalidate an accepted/in-flight image decode.
            cached.image = previous.image.clone();
            cached.image_source = previous.image_source.clone();
            cached.image_unavailable = previous.image_unavailable;
            cached.image_error = previous.image_error.clone();
            cached.image_attempt = previous.image_attempt;
            cached.image_wait = previous.image_wait.clone();
            cached.image_evicted = previous.image_evicted;
            cached.image_size = previous.image_size;
        }
        cached
    }
    /// Approximate exposed presentation storage. Opaque Markdown/parser
    /// allocations and widget/shaping trees are intentionally not estimated.
    fn retained_bytes(&self) -> usize {
        self.literal
            .as_ref()
            .map_or(0, literal_content::Content::retained_bytes)
            + self
                .prompt
                .as_ref()
                .map_or(0, |prompt| prompt.retained_bytes())
            + self.flow_cache.retained_bytes()
            + self.source.retained_capacity()
            + self
                .blocks
                .values()
                .map(prose::CodeBlock::retained_bytes)
                .sum::<usize>()
            + self
                .reasoning
                .as_ref()
                .map_or(0, reasoning::Preview::retained_bytes)
            + self.preview.retained_bytes()
            + self
                .tool_input
                .iter()
                .chain(self.tool_output.iter())
                .chain(self.patches.iter())
                .map(tool_text::ToolText::retained_bytes)
                .sum::<usize>()
    }

    fn next_revision() -> u64 {
        static REVISION: AtomicU64 = AtomicU64::new(1);
        REVISION.fetch_add(1, Ordering::Relaxed)
    }

    #[cfg(test)]
    fn new(content: &ItemContent, theme: crate::settings::EditorSyntaxTheme) -> Self {
        Self::updated(content, theme, None, false, false, None)
    }

    /// `growing` is whether the item's text may still be appended to, and
    /// `coordination` what the item did to work with other agents.
    fn updated(
        content: &ItemContent,
        theme: crate::settings::EditorSyntaxTheme,
        mut previous: Option<Self>,
        expanded: bool,
        growing: bool,
        coordination: Option<coordination::Prepared>,
    ) -> Self {
        // Only the exact immutable item authorizes descriptor reuse. A tail
        // token cannot prove that a same-ID replacement has the same body.
        let prepared_tools = previous
            .as_ref()
            .filter(|cached| {
                CachedItem::prose(content).is_none()
                    && cached
                        .item
                        .as_ref()
                        .is_some_and(|item| std::ptr::eq(&item.content, content))
            })
            .map(|cached| {
                cached
                    .prepared_tools
                    .clone()
                    .unwrap_or_else(|| Arc::new(tool_text::PreparedSources::retained(cached)))
            });
        let literal = previous
            .as_mut()
            .filter(|cached| {
                cached
                    .item
                    .as_ref()
                    .is_some_and(|item| std::ptr::eq(&item.content, content))
            })
            .and_then(|cached| cached.literal.take());
        // A title is formed from the call's input, so the same call keeps
        // the one it has; a toggle or a retheme does not read it again.
        let title = match content {
            ItemContent::Tool {
                name,
                input,
                status,
                ..
            } => Some(
                previous
                    .as_mut()
                    .filter(|cached| {
                        cached
                            .item
                            .as_ref()
                            .is_some_and(|item| std::ptr::eq(&item.content, content))
                    })
                    .and_then(|cached| cached.title.take())
                    .unwrap_or_else(|| tool_title::Title::of(name, input, *status)),
            ),
            _ => None,
        };
        let fresh_coordination = coordination.is_some();
        let retained_coordination = previous
            .as_mut()
            .filter(|cached| {
                cached
                    .item
                    .as_ref()
                    .is_some_and(|item| std::ptr::eq(&item.content, content))
                    && cached.coordination.is_some()
            })
            .map(|cached| {
                (
                    cached.coordination.take(),
                    std::mem::take(&mut cached.blocks),
                    cached.palette_views.take(),
                    // The kept card still shows the labels of these links.
                    std::mem::take(&mut cached.pull_request_links),
                )
            });
        let (retained_coordination, retained_blocks, retained_palettes, retained_links) =
            retained_coordination.unwrap_or_default();
        let item = previous.as_ref().and_then(|cached| cached.item.clone());
        let stamp = previous.as_ref().and_then(|cached| cached.stamp.clone());
        // Weak descriptors survive append publication; only mounted row owners
        // retain glyphs. Each leaf independently proves styled-prefix lineage.
        let flow_cache = previous
            .as_ref()
            .map(|cached| cached.flow_cache.clone())
            .unwrap_or_default();
        let reader_memory = previous
            .as_ref()
            .map(|cached| cached.reader_memory.clone())
            .unwrap_or_default();
        let reader_context = previous
            .as_ref()
            .and_then(|cached| cached.reader_context.clone());
        let figures = previous
            .as_ref()
            .map(|cached| cached.figures.clone())
            .unwrap_or_default();
        let (
            image,
            image_source,
            image_item,
            image_unavailable,
            image_error,
            image_attempt,
            image_wait,
            image_evicted,
            image_size,
        ) = previous
            .as_ref()
            .filter(|cached| cached.token == ChangeToken::of(content))
            .map(|cached| {
                (
                    cached.image.clone(),
                    cached.image_source.clone(),
                    cached.image_item.clone(),
                    cached.image_unavailable,
                    cached.image_error.clone(),
                    cached.image_attempt,
                    cached.image_wait.clone(),
                    cached.image_evicted,
                    cached.image_size,
                )
            })
            .unwrap_or_default();
        let (input, output, preview, patches) = previous
            .map(|mut cached| {
                (
                    cached.tool_input.take(),
                    cached.tool_output.take(),
                    Some(std::mem::replace(
                        &mut cached.preview,
                        tool_text::Preview::empty(theme),
                    )),
                    std::mem::take(&mut cached.patches),
                )
            })
            .unwrap_or_default();
        use tool_text::{Language, ToolText};
        let patches = match content {
            ItemContent::Changes { changes, .. } if expanded => {
                let mut previous = patches.into_iter();
                changes
                    .iter()
                    .enumerate()
                    .map(|(index, change)| {
                        let old = previous.next();
                        if let Some(prepared) = prepared_tools
                            .as_ref()
                            .and_then(|sources| sources.patches.get(index))
                        {
                            ToolText::from_prepared(old, prepared)
                        } else {
                            ToolText::update(old, &change.diff, Language::Diff)
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        let prepare_text =
            |previous, text: &str, language, prepared: Option<&tool_text::PreparedText>| {
                if let Some(prepared) = prepared {
                    ToolText::from_prepared(previous, prepared)
                } else {
                    ToolText::update(previous, text, language)
                }
            };
        let prepared_input = prepared_tools
            .as_ref()
            .and_then(|sources| sources.input.as_ref());
        let prepared_output = prepared_tools
            .as_ref()
            .and_then(|sources| sources.output.as_ref());
        let (tool_input, tool_output) = match content {
            _ if !expanded => (None, None),
            ItemContent::Command {
                command,
                output: text,
                ..
            } => (
                Some(prepare_text(
                    input,
                    command,
                    Language::Shell,
                    prepared_input,
                )),
                Some(prepare_text(
                    output,
                    text,
                    if prepared_output.is_some() {
                        Language::Plain
                    } else {
                        Language::structured(text)
                    },
                    prepared_output,
                )),
            ),
            ItemContent::Tool {
                input: text,
                output: result,
                ..
            } => (
                Some(prepare_text(
                    input,
                    text,
                    if prepared_input.is_some() {
                        Language::Plain
                    } else {
                        Language::structured(text)
                    },
                    prepared_input,
                )),
                Some(prepare_text(
                    output,
                    result,
                    if prepared_output.is_some() {
                        Language::Plain
                    } else {
                        Language::structured(result)
                    },
                    prepared_output,
                )),
            ),
            _ => (None, None),
        };
        let mut cached = Self {
            preparing: false,
            placeholder: false,
            diagnostic: false,
            report_expanded: matches!(
                content,
                ItemContent::Delivery {
                    kind: chat::DeliveryKind::Report,
                    ..
                }
            )
            .then_some(expanded),
            preview: tool_text::Preview::update(preview, content, title.as_ref(), theme),
            tool_input,
            prepared_tools,
            tool_output,
            patches,
            token: ChangeToken::of(content),
            item,
            revision: Self::next_revision(),
            source: markdown_source::Source::default(),
            markdown: markdown::Content::new(),
            reasoning: reasoning::Preview::of(content),
            document: None,
            prompt: None,
            literal,
            flow_cache,
            reader_memory,
            reader_context,
            figures,
            blocks: retained_blocks,
            palette_views: retained_palettes,
            open: None,
            detail: None,
            stamp,
            image,
            image_source,
            image_item,
            image_unavailable,
            image_error,
            image_attempt,
            image_wait,
            image_evicted,
            image_size,
            coordination: coordination
                .map(|prepared| Box::new(coordination::Coordinated::from_prepared(prepared)))
                .or(retained_coordination),
            title,
            pull_request_links: retained_links,
        };
        if expanded
            && matches!(
                content,
                ItemContent::Command { .. }
                    | ItemContent::Tool { .. }
                    | ItemContent::Changes { .. }
            )
        {
            cached.prepared_tools = Some(Arc::new(tool_text::PreparedSources::retained(&cached)));
        }
        if fresh_coordination {
            cached.highlight(theme);
            cached.settle_blocks();
        }
        match content {
            ItemContent::Text { .. } | ItemContent::Delivery { .. } => {
                // Reports stay wholly behind their disclosure, including a
                // one-line API diagnostic. Keep large hidden reports out of
                // Markdown preparation; Toggle and Find prepare the original
                // body when they explicitly open it.
                let hidden_report = !expanded
                    && matches!(
                        content,
                        ItemContent::Delivery {
                            kind: chat::DeliveryKind::Report,
                            ..
                        }
                    );
                if !hidden_report && let Some(body) = Self::prose(content) {
                    cached.extend(body, growing, theme);
                }
            }
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
                // A command that ended without an exit code to say so says
                // how it ended; one that worked needs no word for it.
                if !matches!(exit_code, Some(code) if *code != 0)
                    && let Some(word) = unsettled_word(*status)
                {
                    detail = if detail.is_empty() {
                        word.to_owned()
                    } else {
                        format!("{word} · {detail}")
                    };
                }
                cached.detail = (!detail.is_empty()).then_some(detail);
            }
            ItemContent::Changes { status, .. } => {
                // The row counts the files; this says only how the edit went.
                cached.detail = unsettled_word(*status).map(str::to_owned);
            }
            ItemContent::WebSearch { results, .. } => {
                // A search reported without its results, as Codex reports
                // one, found nothing a reader could count.
                cached.detail = match results.len() {
                    0 => None,
                    1 => Some("1 result".to_owned()),
                    count => Some(format!("{count} results")),
                };
            }
            ItemContent::Tool { status, .. } => {
                cached.detail = unsettled_word(*status).map(str::to_owned);
            }
            ItemContent::Image { .. } => {}
        }
        cached
    }
    /// The text of `content` that is drawn as Markdown.
    fn prose(content: &ItemContent) -> Option<&str> {
        match content {
            ItemContent::Text { text, .. } => Some(text),
            // A notice's body is a log line or a raw result, drawn verbatim.
            ItemContent::Delivery { kind, body, .. } if *kind != chat::DeliveryKind::Notice => {
                Some(body)
            }
            _ => None,
        }
    }

    /// Parses the part of `body` not yet seen, `body` being what was parsed
    /// before with more appended, and `growing` whether more may follow.
    ///
    /// The normalizer and parser preserve their trailing state. Plain tails
    /// and open fences have incremental paths; unfinished rich paragraphs
    /// can still require full trailing-block reparsing. Large changed prose
    /// uses the background preparation engine rather than this UI helper.
    fn extend(&mut self, body: &str, growing: bool, theme: crate::settings::EditorSyntaxTheme) {
        if self.reasoning.is_some() {
            self.reasoning = Some(reasoning::Preview::new(body));
        }
        {
            let _stage = chat_stage::scope(Stage::Parse);
            match self.source.advance_append(body, growing) {
                markdown_source::Step::Append(text) => self.markdown.push_str(&text),
                markdown_source::Step::Restart(text) => {
                    self.markdown =
                        markdown::Content::parse_labelled(&text, self.markdown.labels().clone());
                }
            }
        }
        self.document = Some(prose_document::Document::snapshot(&self.markdown));
        self.highlight(theme);
        if !growing {
            self.suspend_continuations();
        }
        self.revision = Self::next_revision();
    }

    /// Hands the parser what was kept back from it while `content`, which is
    /// what this was last parsed from, could still grow.
    fn settle(&mut self, content: &ItemContent, theme: crate::settings::EditorSyntaxTheme) {
        if self.source.provisional()
            && let Some(body) = Self::prose(content)
        {
            self.extend(body, false, theme);
        }
        self.suspend_continuations();
    }

    /// The displayed Markdown/spans and their widget identities remain valid.
    /// Only continuation scratch is shed; a later append reparses its actual
    /// whole body after the rewriter reports Restart.
    fn suspend_continuations(&mut self) {
        self.source.suspend();
        self.markdown.discard_continuation();
        self.settle_blocks();
    }

    /// A fence larger than this is drawn plain: a syntect pass over it would
    /// stall the frame it is cached on, and a block that size is a file the
    /// agent pasted rather than a snippet it is showing.
    const HIGHLIGHT_LIMIT: usize = 64 * 1024;

    /// Prepares every fenced block the Markdown now holds, keeping the blocks
    /// that are unchanged since the last pass. A streamed block changes on
    /// every refresh until its fence closes, so it is the one block a
    /// refresh usually prepares again, and it is continued from its last
    /// whole line rather than highlighted from its first.
    fn highlight(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        let _stage = chat_stage::scope(Stage::Highlight);
        self.palette_views = None;
        let mut previous = std::mem::take(&mut self.blocks);
        let mut open = self.open.take();
        let mut blocks = HashMap::new();
        let mut last = None;
        let coordinated = self
            .coordination
            .iter()
            .flat_map(|coordinated| coordinated.contents())
            .flat_map(markdown::Content::items);
        for_each_fence(
            self.markdown.items().iter().chain(coordinated),
            &mut |language, code| {
                // Above the syntax budget, raw bounded visible chunks are
                // prepared by prose. Do not hash/copy the full growing fence.
                if code.len() > Self::HIGHLIGHT_LIMIT {
                    return;
                }
                let key = fence_key(language.unwrap_or_default(), code);
                last = Some(key);
                if blocks.contains_key(&key) {
                    return;
                }
                let block = previous
                    .remove(&key)
                    .or_else(|| {
                        let mut block = previous.remove(&open.take()?)?;
                        block
                            .extend(language.and_then(prose::fence_language), code)
                            .then_some(block)
                    })
                    .unwrap_or_else(|| {
                        prose::CodeBlock::open(
                            language.and_then(prose::fence_language),
                            code,
                            theme,
                        )
                    });
                blocks.insert(key, block);
            },
        );
        for (key, block) in &mut blocks {
            if Some(*key) != last {
                block.settle();
            }
        }
        self.blocks = blocks;
        self.open = last;
    }

    /// Gives up what the last fence would be continued from, once nothing is
    /// streaming into the item.
    fn settle_blocks(&mut self) {
        if let Some(block) = self.open.take().and_then(|key| self.blocks.get_mut(&key)) {
            block.settle();
        }
    }

    /// Discards the highlighting and does it again in `theme`.
    fn rehighlight(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        let continuing = self.open.is_some();
        self.preview.rehighlight(theme);
        self.blocks.clear();
        self.open = None;
        self.highlight(theme);
        if !continuing {
            self.settle_blocks();
        }
    }

    /// The fence whose info string is `language` and whose text is `code`,
    /// where the cache holds it.
    #[cfg(test)]
    fn block(&self, language: Option<&str>, code: &str) -> Option<&prose::CodeBlock> {
        if code.len() > Self::HIGHLIGHT_LIMIT {
            return None;
        }
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
            // A bounded recent window cannot establish the conversation's
            // first task. Keep its saved name until the actual head or a
            // supported provider title is available; never page just to name.
            if snapshot.has_earlier {
                return None;
            }
            snapshot
                .items
                .iter()
                .take(128)
                .find_map(|item| match (&item.role, &item.content) {
                    (chat::Role::User, ItemContent::Text { text, .. }) => {
                        crate::workspace_model::channel_title_from_prompt(text)
                            .map(|title| (title, crate::model::ChannelTitleSource::Prompt))
                    }
                    _ => None,
                })
        })
}

/// Prompt discovery is bounded and runs once per turn/history edge, rather
/// than scanning retained history for every streamed token. Provider names
/// remain cheap to observe and may improve throughout the same session.
#[derive(Default)]
struct ChannelNameObservation {
    session: Option<String>,
    scanned: Option<(u64, usize, Option<String>, bool)>,
    title: Option<(String, crate::model::ChannelTitleSource)>,
    pending: bool,
    last_view: Option<(crate::model::ChannelId, u64)>,
}
impl ChannelNameObservation {
    fn observe(
        &mut self,
        snapshot: &Snapshot,
    ) -> Option<(String, crate::model::ChannelTitleSource)> {
        if self.session != snapshot.session_id {
            *self = Self {
                session: snapshot.session_id.clone(),
                ..Self::default()
            };
        }
        let previous = self.title.clone();
        if let Some(title) = snapshot
            .title
            .as_deref()
            .and_then(crate::workspace_model::channel_title)
        {
            self.title = Some((title, crate::model::ChannelTitleSource::Provider));
        } else if self.title.is_none() {
            let stamp = (
                snapshot.turn_generation,
                snapshot.items.len(),
                snapshot.items.first().map(|item| item.id.clone()),
                snapshot.has_earlier,
            );
            if self.scanned.as_ref() != Some(&stamp) {
                self.scanned = Some(stamp);
                self.title = snapshot_channel_name(snapshot);
            }
        }
        self.pending |= self.title != previous;
        self.pending.then(|| self.title.clone()).flatten()
    }
}

/// A row of the composer's `+` menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ComposerRow {
    Attach,
    Plan,
    Steering,
    Fork,
    Compact,
}

impl ComposerRow {
    /// The row's control id within its card.
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Attach => "chat-attach",
            Self::Plan => "chat-actions-plan",
            Self::Steering => "chat-actions-steering",
            Self::Fork => "chat-actions-fork",
            Self::Compact => "chat-actions-compact",
        }
    }

    /// The words the menu's search finds the row by, which include its
    /// label in each state it can show.
    fn keywords(self) -> &'static str {
        match self {
            Self::Attach => "attach files images add upload attachment",
            Self::Plan => "plan mode leave permissions explore",
            Self::Steering => "steering steer after turn tools interrupt",
            Self::Fork => "fork conversation branch",
            Self::Compact => "compact conversation summarize context",
        }
    }
}

/// One entry of the `+` menu: an action of the composer's own, or a command
/// or skill it names into the draft exactly as slash completion would.
enum MenuEntry<'a> {
    Row(ComposerRow),
    Named(completion::Candidate<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryNavigation {
    EarlierEdge,
    EarlierExplicit,
    NewerEdge,
    Latest,
}

#[derive(Clone, Copy)]
// Deferring the history fetch must not resurrect a superseded reader snap.
enum LatestReader {
    Follow,
    Preserve,
}

/// When and how the last turn ended, as the transcript's closing stamp says.
struct FinishedStamp {
    at: u64,
    clock: String,
    full: String,
    /// Settled when the turn finishes, so a later notice or an unrelated
    /// local error does not rewrite how it ended.
    outcome: &'static str,
}

/// [`Action::CommandsDue`] for `target`, once a turn has had time to begin.
fn commands_due(target: Target) -> Task<Message> {
    cosmic::task::future(async move {
        tokio::time::sleep(commands::TURN_PATIENCE).await;
        Message::ChatUi(Action::CommandsDue(target))
    })
}

/// Sends `command` to the conversation `handle` reaches, off the UI thread,
/// and answers with its outcome. Without one, it is answered as not sent.
fn run_chat_command(
    target: Target,
    id: u64,
    handle: Option<Arc<ClientHandle>>,
    command: Command,
) -> Task<Message> {
    cosmic::task::future(async move {
        let result = match handle {
            Some(handle) => tokio::task::spawn_blocking(move || handle.send(command))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result),
            None => Err("The conversation disconnected before this was sent.".to_owned()),
        };
        Message::ChatUi(Action::CommandFinished(target, id, result))
    })
}

/// What a chat card's conversation runs through.
enum Connection {
    /// Nothing runs yet. A new conversation starts its CLI when its first
    /// message is sent, so until then the provider, model and account are
    /// choices that cost nothing to change, and an unused card leaves no
    /// process behind.
    Draft,
    /// A cached shared descriptor has not been admitted by the current view.
    AwaitingAdmission,
    /// Canonical admission has started an attachment, with no handle yet.
    Connecting,
    Live(Arc<ClientHandle>),
}

#[derive(Clone)]
pub enum QueueCopy {
    Initial(std::sync::Weak<prompt::LocalSource>),
    Provider(std::sync::Weak<prompt::QueuePreview>, String),
    Local(u64),
}

/// A draft's first message, held while its conversation starts.
///
/// A provider does not take commands the moment its connection opens:
/// Claude lists its models on initializing, Codex once its thread is attached
/// and OMP once its history has loaded. The message waits here until the
/// conversation takes it, and returns to the composer if it never does.
struct Queued {
    /// Immutable local input; never a fabricated provider-history Item.
    prompt: prompt::LocalPrompt,
    /// Resolve draft skill choices before the provider replaces the draft
    /// catalog; two provider skills may share a name after startup.
    resolved_text: String,
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
        if slash_invocation(self.prompt.text(), snapshot).is_some() {
            self.prompt.text()
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
    preview: String,
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

/// The messages a conversation's last provider was holding that the provider
/// it reconnected to does not account for, in their order.
///
/// Each message the provider lists accounts for one saved message: first one
/// with its id and words, then one with its words (some providers' ids only
/// name a place in the queue, so the same id may now hold another message),
/// then one with its id alone (a listing may carry only a preview). A saved
/// message is also accounted for when the provider's history shows a user
/// message delivered under its id.
fn unaccounted(
    snapshot: &chat::Snapshot,
    saved: Vec<chat::QueuedMessage>,
) -> Vec<chat::QueuedMessage> {
    fn same_words(a: &chat::QueuedMessage, b: &chat::QueuedMessage) -> bool {
        a.text == b.text && a.attachments == b.attachments
    }
    type Claim = fn(&chat::QueuedMessage, &chat::QueuedMessage) -> bool;
    let claims: [Claim; 3] = [
        |listed, saved| listed.id == saved.id && same_words(listed, saved),
        |listed, saved| same_words(listed, saved),
        |listed, saved| listed.id == saved.id,
    ];
    let mut listed: Vec<&chat::QueuedMessage> = snapshot.queued_messages.iter().collect();
    let mut remaining = saved;
    for claim in claims {
        remaining.retain(|message| {
            let Some(index) = listed.iter().position(|queued| claim(queued, message)) else {
                return true;
            };
            listed.swap_remove(index);
            false
        });
    }
    remaining.retain(|message| {
        !snapshot.items.iter().any(|item| {
            item.role == chat::Role::User
                && item
                    .id
                    .strip_prefix(message.id.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
        })
    });
    remaining
}

/// The reader's messages `snapshot` shows, oldest first, with their item ids.
fn user_messages(snapshot: &Snapshot) -> impl DoubleEndedIterator<Item = (&str, &str)> {
    snapshot
        .items
        .iter()
        .filter_map(|item| match &item.content {
            chat::ItemContent::Text { text, .. } if item.role == chat::Role::User => {
                Some((item.id.as_str(), text.as_str()))
            }
            _ => None,
        })
}

/// What follows `message` in `text`, where `text` begins with it as a
/// message of its own: all of it, or its first lines.
fn leading<'a>(text: &'a str, message: &str) -> Option<&'a str> {
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    let rest = text.trim_start().strip_prefix(message)?;
    if rest.trim().is_empty() {
        return Some("");
    }
    rest.strip_prefix('\n')
}

/// What the reader had said last in `snapshot`'s conversation, as `sent`
/// goes to it.
fn sent_after(snapshot: &Snapshot, sent: &str) -> SentAfter {
    if snapshot.has_later {
        // The conversation's latest messages are not in view.
        return SentAfter::Unseen;
    }
    if snapshot
        .queued_messages
        .iter()
        .any(|queued| !queued.editable || queued.text.trim() == sent.trim())
    {
        // A message the provider still holds may arrive before this one,
        // and nothing shows where it will: one reading the same would pass
        // for this one. A preview may not be the whole message, so it may
        // read the same too.
        return SentAfter::Unseen;
    }
    match user_messages(snapshot).next_back() {
        Some((id, text)) if text.trim() == sent.trim() => SentAfter::Same(id.to_owned()),
        Some(_) => SentAfter::Different,
        None if snapshot.has_earlier => SentAfter::Unseen,
        None => SentAfter::Different,
    }
}

/// Which message of the conversation carried-over unsent text may repeat.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DraftEcho {
    /// The first, which another window started the conversation with.
    First,
    /// The latest, which the conversation's last card was still sending as it
    /// closed, after the message this says the reader had sent before it.
    Latest(SentAfter),
}

/// What came back to the composer from a provider queue that did not survive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RestoredWaiting {
    count: usize,
    /// Whether every message came back whole; a provider lists only a
    /// preview of a long message or of input the composer cannot hold, and
    /// may list an attachment without a file it can name.
    whole: bool,
}

impl RestoredWaiting {
    /// What the notice beside the draft says: what came back, that it was
    /// not sent again, and that the agent may have it anyway.
    fn caption(self) -> String {
        let mut text = if self.count == 1 {
            "Back in your draft: a message that was still queued when this conversation stopped. It has not been resent; the agent may already have received it.".to_owned()
        } else {
            format!(
                "Back in your draft: {} messages that were still queued when this conversation stopped. They have not been resent; the agent may already have received some of them.",
                self.count
            )
        };
        if !self.whole {
            text.push_str(
                " Only part of what was queued could be restored; check it before sending.",
            );
        }
        text
    }
}

const CAPACITY_GUIDANCE: &str = "Inspect usage with cagents chat capacity. Check eligibility with cagents chat release ID --dry-run; release requires verified saved history and no protected work. Then retry. Your conversation and draft are kept.";

pub(super) struct Runtime {
    preparation: preparation::State,
    find: Option<String>,
    /// The groups Find matched, in transcript order. Every turn asks whether
    /// it is among them on every frame, so membership is a lookup.
    find_matches: indexmap::IndexSet<String>,
    find_disclosure: find::Disclosure,
    child_find_disclosure: HashMap<String, find::Disclosure>,
    find_index: usize,
    find_navigation: find_navigation::State,
    copied: Option<(String, Instant)>,
    /// When a cancel that would also stop background subagents was pressed,
    /// until it is pressed again or lapses.
    pub(super) interrupt_armed: Option<Instant>,
    drop_hover: bool,
    files: Option<Arc<Vec<String>>>,
    pub(super) target: Target,
    pub(super) session: chat::Session,
    connection: Connection,
    /// Set only by exact shared-view retirement, independently of transport failure.
    closed_elsewhere: bool,
    /// The usage limit stopping the conversation, as the daemon judges it.
    usage_limit: Option<crate::agent_execution::directory::UsageLimit>,
    pub(super) limit_refresh_pending: bool,
    visible_limit_episode: Option<failure::LimitEpisode>,
    /// How the provider addresses this conversation's subagents, as the
    /// daemon last said: what a message card resolves its recipient through.
    agent_addresses: crate::chat::client::AgentAddresses,
    /// The first message of a draft, while its conversation starts.
    queued: Option<Queued>,
    /// Messages held here for the next turn, oldest first.
    followups: Vec<Followup>,
    /// The place of the next thing the reader writes: a held follow-up's ID,
    /// or a message's, so that what comes back to the composer from both
    /// comes back in the order written.
    next_written: u64,
    /// Provider queue entries asked back, until the provider says whether it
    /// let go of each.
    recalling: Vec<Recall>,
    /// Provider queue entries already back in the composer, hidden until the
    /// provider's own listing drops them.
    taken_back: Vec<String>,
    /// How the draft was queued before it was taken back, which is how
    /// sending it again queues it unless the reader chooses otherwise.
    resend: Option<PromptMode>,
    /// The messages a provider that did not come back was still holding when
    /// this conversation stopped, now returned to the composer unsent. Shown
    /// beside the draft until it is sent or the notice is dismissed.
    restored_waiting: Option<RestoredWaiting>,
    /// The provider queue a parked runtime last saved, by entry, as a parked
    /// runtime does not refresh the snapshot a live one compares against.
    parked_queue: Vec<String>,
    composer_actions: bool,
    /// The `+` menu's search, cleared each time the menu opens.
    composer_actions_query: String,
    /// The permission mode plan mode replaced, which leaving it restores.
    mode_before_plan: Option<String>,
    branch_picker: bool,
    changes: changes::State,
    /// Joins the parts of a long message back into one before presentation.
    logical_prose: logical_prose::State,
    fork_requested: bool,
    fork_seen: Option<String>,
    snapshot: Arc<Snapshot>,
    provider_queue: std::cell::RefCell<prompt::ProviderQueue>,
    source_clock: Option<chat::client::SourceClock>,
    finished_stamp: Option<FinishedStamp>,
    cache: HashMap<String, CachedItem>,
    /// Only the selected child has presentation. Parent grouping continues to
    /// use its own cache, even when a child uses the same opaque item IDs.
    child_cache: HashMap<String, CachedItem>,
    child_cache_context: Option<String>,
    presentation_revision: u64,
    presentation_evicted: bool,
    /// Small tail identity survives body eviction so rehydrating an unchanged
    /// publication does not invent an unread message while scrolled back.
    last_presented_tail: Option<(String, ChangeToken)>,
    /// Only the latest explicitly requested preview is retained outside recency.
    preview_request: Option<String>,
    /// The palette the cache's code blocks are highlighted in.
    syntax_theme: crate::settings::EditorSyntaxTheme,
    /// How the conversation's pull request links read, from what the window
    /// already holds of the pull requests it mentions.
    pull_request_labels: Arc<pull_request_links::Labels>,
    /// What the inspector last listed for this conversation.
    pull_request_inspected: pull_request_links::Inspected,
    /// What the labels were last built from.
    pull_request_labels_built_from: Option<LabelSources>,
    /// The snapshot's items grouped into stretches of work, rebuilt only when
    /// the snapshot changes rather than once per frame.
    turns: Revised<Vec<turn::Turn>>,
    selected_subagent: Option<String>,
    observed_scroll: scroll::Observations,
    scroll_memory: scroll::Memory,
    /// The images answers' Markdown shows, decoded as they come into view.
    figures: images::figure::Figures,
    child_picker: Option<child_navigation::Picker>,
    /// The image viewer over this conversation's transcript. It stays open
    /// as the conversation moves on, and closes only when the reader closes
    /// it, searches the transcript, or the pane goes.
    pub(super) image_viewer: Option<images::Gallery>,
    /// The child whose tab holds keyboard focus. The tab strip reports it, so
    /// the next frame keeps that tab mounted wherever its state sorts it.
    child_tab_focus: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    child_ordinals: child_navigation::Ordinals,
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
    opened: Revised<HashMap<String, bool>>,
    pub(super) tool_groups_expanded: bool,
    composer: text_editor::Content<cosmic::Renderer>,
    revision: u64,
    attachments: Vec<Attachment>,
    thumbnails: thumbnails::Cache,
    /// Clipboard reads and image writes still destined for this draft.
    pastes_pending: usize,
    attachment_bookmarks: HashMap<uuid::Uuid, attachment_draft::Bookmark>,
    attachment_jobs: HashMap<uuid::Uuid, attachment_draft::Lease>,
    attachment_order: Vec<uuid::Uuid>,
    image_pastes_saving: usize,
    submit_after_paste: Option<PromptMode>,
    attachment_chooser_pending: Option<uuid::Uuid>,
    /// The composer revision at which carried-over unsent text may still
    /// repeat a message the conversation received, and which one. Cleared
    /// once the transcript settles it, or the reader edits the text.
    draft_echo: Option<(u64, DraftEcho)>,
    expanded: Revised<HashSet<String>>,
    /// Each child's own disclosures, stamped separately so pruning departed
    /// children never disturbs the selected child's.
    child_expanded: HashMap<String, Revised<HashSet<String>>>,
    child_unfolded: HashMap<String, HashSet<String>>,
    /// Coordination card parts shown in full, as `{item id}#{part}`.
    unfolded: HashSet<String>,
    answers: view::Answers,
    commands: commands::Commands,
    next_command: u64,
    disconnected: bool,
    /// A fingerprint of the models this conversation last listed to the
    /// catalog, which takes each list once rather than on every change.
    listed_models: u64,
    visible: AtomicBool,
    /// While hidden, the order this chat was hidden in if it keeps its
    /// presentation and its client's transcript; see [`warm`].
    warm: Option<u64>,
    error: Option<String>,
    /// A typed admission refusal, independent of provider/account failures.
    capacity_refused: bool,
    /// A Close the daemon refused, shown on the pane until it closes.
    close_refusal: Option<close_refusal::CloseRefusal>,
    /// Resuming this card's stopped provider failed; the reason stands in
    /// `error`, and retry is offered only while it can help.
    resume_failed: bool,
    /// The daemon says repeating this start cannot help, and when the
    /// conversation can come back.
    start_blocked: Option<blocked_recovery::Reopen>,
    /// What the daemon last said about its update restore, which a pane
    /// waiting on it explains.
    update_wait: UpdateWait,
    /// Resume may retire this actor before its replacement answers. Keep the
    /// source body until a successful attachment supplies its successor.
    resume_source_pinned: bool,
    /// Local send errors anchored to the exchange that produced them.
    failures: Vec<failure::Note>,
    /// Notices raised while an earlier page is shown. That page holds
    /// neither the latest prompt nor the latest work, so they are recorded
    /// when the latest page next arrives.
    unplaced_failures: Vec<String>,
    failure_ui: failure::State,
    permission_picker: bool,
    permissions_form: permissions::Form,
    /// Whether the account choice is open.
    account_picker: bool,
    picker_focus: usize,
    picker_keyboard: bool,
    /// Messages the provider accepted, oldest first, for Up and Down in the
    /// composer to walk through the way a shell does.
    history: Vec<String>,
    history_images: Vec<Vec<Attachment>>,
    history_attachment_stash: Option<Vec<Attachment>>,
    /// Which entry the composer is showing while the reader walks the
    /// history; `None` while they are writing.
    history_cursor: Option<usize>,
    /// The draft that was in the composer when the walk began, restored when
    /// the walk goes past the newest message.
    history_stash: Option<text_editor::Content<cosmic::Renderer>>,
    /// The most recent transcript viewport, used for history pagination.
    selection_bookmarks: std::cell::RefCell<
        HashMap<Option<String>, std::rc::Rc<std::cell::RefCell<selection::Bookmark>>>,
    >,
    selection_document: std::cell::RefCell<Option<(view::SelectionKey, Arc<selection::Catalog>)>>,
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
    suggestion_dismissed: Option<(Option<String>, u64)>,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_runtime", chat_id = %self.session.id,
            released = true, resident_bytes = 0usize, cache_entries = 0usize, exposed_presentation_bytes = 0usize);
    }
}

/// The name the composer accepts for compaction where the provider offers it
/// as a capability rather than as one of its listed commands.
pub(super) const COMPACT_COMMAND: &str = "compact";
/// The `+` menu's search field.
pub(super) const COMPOSER_SEARCH: &str = "chat-actions-search";
/// The permission mode in which a provider plans before it changes anything.
pub(super) const PLAN_MODE: &str = "plan";
pub(super) const PERMISSIONS_COMMAND: &str = "permissions";

fn permissions_available(snapshot: &Snapshot) -> bool {
    !snapshot.capabilities.permission_modes.is_empty()
        || !snapshot.permission_denials.is_empty()
        || snapshot
            .requests
            .iter()
            .any(|request| request.kind == chat::RequestKind::Approval)
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

/// Whether `current` only appends to `previous`. The client's wire lineage
/// proves it for provider items; a long message presented as one is a new
/// item on each publication, so its source's append-only revision proves it.
fn known_append(
    handle: Option<&Arc<ClientHandle>>,
    prose: &logical_prose::State,
    previous: &Arc<chat::Item>,
    current: &Arc<chat::Item>,
) -> bool {
    handle.is_some_and(|handle| handle.known_append(previous, current))
        || prose.proves(previous, current)
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
        self.apply_completion(&token, insertion);
        true
    }

    /// Puts `insertion` over `token` as one edit of the draft, which ends any
    /// history browsing and invalidates attachment positions read before it.
    fn apply_completion(&mut self, token: &completion::Token, insertion: &str) {
        completion::apply(&mut self.composer, token, insertion);
        self.named_in_draft();
    }

    /// Settles the draft after a name was put in it: the edit moved text
    /// under any attachment placed by position, and ends the completion and
    /// history walk it came from.
    fn named_in_draft(&mut self) {
        for bookmark in self.attachment_bookmarks.values_mut() {
            bookmark.valid = false;
        }
        self.attachment_jobs.clear();
        self.completion = completion::State::default();
        self.revision = self.revision.wrapping_add(1);
        self.history_cursor = None;
        self.history_stash = None;
        self.history_attachment_stash = None;
    }

    /// Retain exact receipt metadata across take-back, queue edits and retry.
    fn merge_attachments(&mut self, attachments: impl IntoIterator<Item = Attachment>) {
        let before = self.attachments.len();
        for attachment in attachments {
            if !self
                .attachments
                .iter()
                .any(|old| match (&old.reference, &attachment.reference) {
                    (Some(a), Some(b)) => a.id == b.id && a.device == b.device,
                    (None, None) => old.path == attachment.path,
                    _ => false,
                })
            {
                self.attachments.push(attachment);
            }
        }
        self.thumbnails.sync(&self.attachments);
        if self.attachments.len() != before {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Legacy test/helper files carry no inferred identity.
    #[cfg(test)]
    fn attach(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.merge_attachments(paths.into_iter().map(Attachment::file));
    }

    fn try_attachment_insert(&mut self) -> Option<attachment_draft::Ticket> {
        let Some((ticket, lease)) = attachment_draft::admit() else {
            self.error = Some(
                "Two attachments are already loading. Wait for them before attaching more.".into(),
            );
            return None;
        };
        self.attachment_jobs.insert(ticket.id, lease);
        self.attachment_order.push(ticket.id);
        self.attachment_bookmarks.insert(
            ticket.id,
            attachment_draft::Bookmark::capture(&self.composer),
        );
        self.pastes_pending += 1;
        Some(ticket)
    }

    #[cfg(test)]
    fn begin_attachment_insert(&mut self) -> attachment_draft::Ticket {
        self.try_attachment_insert()
            .expect("test attachment admission")
    }

    #[cfg(test)]
    fn attachment_ticket(&self, id: uuid::Uuid) -> Option<attachment_draft::Ticket> {
        self.attachment_jobs.get(&id).map(|lease| lease.ticket(id))
    }

    fn composer_action(&mut self, action: text_editor::Action) {
        match action {
            text_editor::Action::Edit(edit) => {
                if self.attachment_bookmarks.is_empty() {
                    self.composer.perform(text_editor::Action::Edit(edit));
                } else {
                    match attachment_draft::edit(&mut self.composer, edit) {
                        Ok(Some(change)) => {
                            for bookmark in self.attachment_bookmarks.values_mut() {
                                bookmark.changed(&change);
                            }
                        }
                        Err(()) => {
                            for bookmark in self.attachment_bookmarks.values_mut() {
                                bookmark.valid = false;
                            }
                        }
                        Ok(None) => {}
                    }
                }
            }
            other => self.composer.perform(other),
        }
        self.cancel_invalid_attachment_jobs();
        self.settle_restored_notice();
    }

    /// The restored-messages notice describes the draft; once the reader
    /// has cleared the draft away, there is nothing left for it to describe.
    fn settle_restored_notice(&mut self) {
        if self.restored_waiting.is_some()
            && self.attachments.is_empty()
            && self.composer.text().trim().is_empty()
        {
            self.restored_waiting = None;
        }
    }

    fn cancel_invalid_attachment_jobs(&mut self) {
        self.attachment_jobs.retain(|id, _| {
            self.attachment_bookmarks
                .get(id)
                .is_some_and(|bookmark| bookmark.valid)
        });
    }

    /// Insert at the captured, measured caret. Keep a user who moved elsewhere
    /// at their own mapped caret; conflicting selection edits refuse insertion.
    fn finish_attachment_insert(
        &mut self,
        id: uuid::Uuid,
        text: &str,
        attachments: Vec<Attachment>,
    ) -> bool {
        let earlier = self
            .attachment_order
            .iter()
            .take_while(|other| **other != id)
            .copied()
            .collect::<HashSet<_>>();
        self.attachment_order.retain(|other| *other != id);
        self.attachment_jobs.remove(&id);
        let Some(bookmark) = self.attachment_bookmarks.remove(&id) else {
            return false;
        };
        self.pastes_pending = self.pastes_pending.saturating_sub(1);
        if !bookmark.valid {
            self.error = Some("The image insertion point changed while loading. Attach the image again at the new caret.".into());
            self.submit_after_paste = None;
            return false;
        }
        // An empty clipboard completes the job without editing the draft.
        // Its caller may still resume an explicitly deferred Submit.
        if text.is_empty() && attachments.is_empty() {
            return true;
        }
        let current = attachment_draft::Bookmark::capture(&self.composer);
        if !current.valid {
            self.error = Some("The current text selection could not be preserved. Attach the image again at the new caret.".into());
            self.submit_after_paste = None;
            return false;
        }
        let current = current.cursor;
        attachment_draft::move_to(&mut self.composer, bookmark.cursor);
        let mut insertion = text.to_owned();
        if !attachments.is_empty()
            && self
                .composer
                .line(bookmark.cursor.position.line)
                .and_then(|line| {
                    line.text
                        .get(..bookmark.cursor.position.column)
                        .map(str::to_owned)
                })
                .is_some_and(|left| left.chars().next_back().is_some_and(|c| !c.is_whitespace()))
        {
            insertion.push(' ');
        }
        for attachment in &attachments {
            if let Some(reference) = &attachment.reference {
                if !insertion.is_empty() && !insertion.ends_with(char::is_whitespace) {
                    insertion.push(' ');
                }
                insertion.push_str(&reference.token());
                insertion.push(' ');
            }
        }
        if !insertion.is_empty()
            && let Ok(Some(change)) = attachment_draft::edit(
                &mut self.composer,
                text_editor::Edit::Paste(Arc::new(insertion)),
            )
        {
            for (other_id, other) in &mut self.attachment_bookmarks {
                // Equal collapsed anchors follow paste initiation order,
                // independent of worker completion order. User caret
                // mapping retains its normal left affinity.
                let later_at_start = !earlier.contains(other_id)
                    && other.cursor.selection.is_none()
                    && other.cursor.position == change.start;
                other.changed(&change);
                if later_at_start && other.valid {
                    other.cursor.position = change.inserted_end();
                }
            }
            self.cancel_invalid_attachment_jobs();
            if current != bookmark.cursor
                && let Some(mapped) = attachment_draft::mapped(current, &change)
            {
                attachment_draft::move_to(&mut self.composer, mapped);
            }
        }
        self.merge_attachments(attachments);
        self.revision = self.revision.wrapping_add(1);
        self.completion.restart(true);
        self.history_cursor = None;
        self.history_stash = None;
        self.history_attachment_stash = None;
        true
    }

    fn cancel_attachment_insert(&mut self, id: uuid::Uuid, error: Option<String>) {
        self.attachment_order.retain(|other| *other != id);
        self.attachment_jobs.remove(&id);
        if self.attachment_bookmarks.remove(&id).is_some() {
            self.pastes_pending = self.pastes_pending.saturating_sub(1);
            self.submit_after_paste = None;
            if let Some(error) = error {
                self.error = Some(error);
            }
        }
    }

    fn remove_image_token(&mut self, token: &str) -> bool {
        let text = self.composer.text();
        if text.contains(token) && !attachment_draft::Bookmark::capture(&self.composer).valid {
            self.error = Some("The current text selection could not be preserved. Clear it before removing this image.".into());
            return false;
        }
        let excerpt = attachment_draft::excerpt(
            &self.composer,
            0,
            self.composer.line_count().saturating_sub(1),
            false,
        );
        for (offset, _) in text
            .match_indices(token)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let current = attachment_draft::Bookmark::capture(&self.composer).cursor;
            attachment_draft::move_to(
                &mut self.composer,
                text_editor::Cursor {
                    position: excerpt.position(offset + token.len()),
                    selection: Some(excerpt.position(offset)),
                },
            );
            if let Ok(Some(change)) = attachment_draft::edit(
                &mut self.composer,
                text_editor::Edit::Paste(Arc::new(String::new())),
            ) {
                for bookmark in self.attachment_bookmarks.values_mut() {
                    bookmark.changed(&change);
                }
                if let Some(mapped) = attachment_draft::mapped(current, &change) {
                    attachment_draft::move_to(&mut self.composer, mapped);
                }
            }
        }
        self.cancel_invalid_attachment_jobs();
        self.history_cursor = None;
        self.history_stash = None;
        self.history_attachment_stash = None;
        true
    }

    /// The draft's attachments leave with the draft, when it is sent or held,
    /// and so does what the restored-messages notice said about it.
    fn take_attachments(&mut self) -> Vec<Attachment> {
        self.restored_waiting = None;
        self.thumbnails.clear();
        std::mem::take(&mut self.attachments)
    }

    fn replace_attachments(&mut self, attachments: Vec<Attachment>) -> Vec<Attachment> {
        self.thumbnails.clear();
        let old = std::mem::replace(&mut self.attachments, attachments);
        self.thumbnails.sync(&self.attachments);
        old
    }

    /// Puts a message that was waiting back in the composer, ahead of
    /// whatever was typed after it, with the files it was queued with.
    fn take_back(&mut self, text: &str, attachments: Vec<Attachment>, queued: PromptMode) {
        self.prepend_to_draft(text, attachments);
        self.resend = Some(queued);
    }

    /// Puts `text` in the composer ahead of whatever is typed there, with the
    /// caret at its end, and adds `attachments` before the draft's own.
    fn prepend_to_draft(&mut self, text: &str, attachments: Vec<Attachment>) {
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
        // The draft a walk through sent messages set aside is in `combined`.
        self.history_cursor = None;
        self.history_stash = None;
        self.history_attachment_stash = None;
        let later = self.replace_attachments(attachments);
        self.merge_attachments(later);
    }

    /// The provider queue as it should be saved: what the provider last said
    /// it holds, less what has already come back to the composer. A parked
    /// runtime does not refresh its snapshot, so the connection is asked.
    fn waiting_messages(&self) -> Vec<chat::QueuedMessage> {
        let snapshot = self
            .handle()
            .map_or_else(|| Arc::clone(&self.snapshot), |handle| handle.snapshot());
        snapshot
            .queued_messages
            .iter()
            .filter(|message| !self.taken_back.contains(&message.id))
            .cloned()
            .collect()
    }

    /// Returns to the composer the messages a provider that did not come back
    /// was holding. They are never sent from here: the provider may have
    /// started on any of them before it went, so the reader decides.
    fn restore_waiting(&mut self, messages: Vec<chat::QueuedMessage>) {
        if messages.is_empty() {
            return;
        }
        let mut text = String::new();
        let mut attachments: Vec<Attachment> = Vec::new();
        let mut whole = messages.iter().all(|message| message.editable);
        for message in &messages {
            let body = message.text.trim_end();
            if !body.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(body);
            }
            // A path the provider could not vouch for names no file here.
            for attachment in &message.attachments {
                if attachment.reference.is_none() && attachment.path.as_os_str().is_empty() {
                    whole = false;
                } else if !attachments.contains(attachment) {
                    attachments.push(attachment.clone());
                }
            }
        }
        self.prepend_to_draft(&text, attachments);
        self.restored_waiting = Some(RestoredWaiting {
            count: messages.len(),
            whole,
        });
    }

    /// Returns every held follow-up to the composer, in the order written.
    fn restore_followups(&mut self) {
        if self.return_followups() {
            let _ = self.present();
        }
    }

    /// [`Self::restore_followups`], leaving the pane to be presented by its
    /// caller. Whether there were any.
    fn return_followups(&mut self) -> bool {
        if self.followups.is_empty() {
            return false;
        }
        let (text, attachments) = self.combined_followups();
        self.followups.clear();
        self.take_back(&text, attachments, PromptMode::Queue);
        true
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
                if !attachments
                    .iter()
                    .any(|old| match (&old.reference, &attachment.reference) {
                        (Some(a), Some(b)) => a.id == b.id && a.device == b.device,
                        (None, None) => old.path == attachment.path,
                        _ => false,
                    })
                {
                    attachments.push(attachment.clone());
                }
            }
        }
        (text, attachments)
    }

    /// Whether a command is carrying held follow-ups to the provider.
    fn followups_sending(&self) -> bool {
        self.commands.carrying_followups()
    }

    /// Whether the held follow-up `id` is on its way to the provider, and so
    /// no longer the reader's to take back or remove.
    fn followup_sending(&self, id: u64) -> bool {
        self.commands
            .iter()
            .any(|pending| pending.followups.contains(&id))
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
        match self.followups.last() {
            // One on its way is no longer the reader's to take back.
            Some(followup) => {
                (!self.followup_sending(followup.id)).then_some(QueueEntry::Local(followup.id))
            }
            None => self
                .snapshot
                .queued_messages
                .iter()
                .rev()
                .find(|message| self.provider_take_back(message))
                .map(|message| QueueEntry::Provider(message.id.clone())),
        }
    }

    /// Records a message the provider accepted. The same message sent twice
    /// in a row is one entry, as a shell keeps it, and the walk restarts
    /// from the newest.
    #[cfg(test)]
    fn remember(&mut self, sent: String) {
        self.remember_with_attachments(sent, Vec::new());
    }
    fn remember_with_attachments(&mut self, sent: String, attachments: Vec<Attachment>) {
        self.history_images
            .resize_with(self.history.len(), Vec::new);
        if self.history.last() != Some(&sent) || self.history_images.last() != Some(&attachments) {
            self.history.push(sent);
            self.history_images.push(attachments);
        }
        self.history_cursor = None;
        self.history_stash = None;
        self.history_attachment_stash = None;
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
                self.history_attachment_stash = Some(self.attachments.clone());
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
        let attachments = match next {
            Some(index) => {
                let mut attachments = self
                    .history_attachment_stash
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .filter(|attachment| attachment.reference.is_none())
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some(images) = self.history_images.get(index) {
                    attachments.extend(images.clone());
                }
                attachments
            }
            None => self.history_attachment_stash.take().unwrap_or_default(),
        };
        self.replace_attachments(attachments);
        self.history_cursor = next;
        self.replace_draft(composer);
    }

    /// Whether Up may reach history, and Down is browsing it. Logical line
    /// zero is only a candidate: the native editor resolves its wrapped rows
    /// when handling ComposerUp. Recalled prompts remain whole entries.
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

    fn unsent_attachments(&self) -> &[Attachment] {
        self.history_attachment_stash
            .as_deref()
            .filter(|_| self.history_cursor.is_some())
            .unwrap_or(&self.attachments)
    }

    /// Checks carried-over unsent text against the message `echo` names,
    /// once the transcript shows it.
    fn await_draft_echo(&mut self, echo: DraftEcho) {
        self.draft_echo = Some((self.revision, echo));
        self.settle_draft_echo();
    }

    /// Drops from the composer the message it may repeat, which the
    /// conversation already has, so it is not offered again: the whole text
    /// where it is exactly that message, or, for one still being sent as its
    /// chat closed, the lines it leads. Any other text is the reader's own
    /// and stays.
    fn settle_draft_echo(&mut self) {
        let Some((revision, echo)) = self.draft_echo.take() else {
            return;
        };
        if revision != self.revision || !self.attachments.is_empty() {
            // Edited since, or more than the text: no longer a copy.
            return;
        }
        if self.snapshot.history_loading || !self.visible.load(Ordering::Relaxed) {
            self.draft_echo = Some((revision, echo));
            return;
        }
        let composer = self.composer.text();
        let repeated = {
            let mut sent = user_messages(&self.snapshot);
            let message = match &echo {
                // The first message counts only where the transcript starts.
                DraftEcho::First => sent.next().filter(|_| !self.snapshot.has_earlier),
                DraftEcho::Latest(SentAfter::Different) => sent.last(),
                DraftEcho::Latest(SentAfter::Same(before)) => {
                    // The message sent before it read the same, so only a
                    // message shown after that one can be this one. Where that
                    // one is not in view, or the provider shows it under another
                    // id, nothing here tells them apart, and the text stays.
                    let sent = sent.collect::<Vec<_>>();
                    match sent.iter().position(|(id, _)| id == before) {
                        Some(index) => match sent[index + 1..].last() {
                            Some(message) => Some(*message),
                            None => return,
                        },
                        None if sent.is_empty() => None,
                        None => return,
                    }
                }
                // Nothing told the message apart as it was sent.
                DraftEcho::Latest(SentAfter::Unseen) => return,
            };
            let Some((_, message)) = message else {
                if !self.snapshot.has_earlier {
                    // The transcript may yet show it.
                    self.draft_echo = Some((revision, echo));
                }
                // Otherwise the message is not in view, so nothing can be shown
                // to repeat it.
                return;
            };
            // A message still being sent leads what was written after it,
            // which is the reader's own however it went.
            match &echo {
                DraftEcho::First => (composer.trim() == message.trim()).then(String::new),
                DraftEcho::Latest(_) => {
                    leading(&composer, message).map(|rest| rest.trim_end().to_owned())
                }
            }
        };
        if let Some(rest) = repeated {
            self.replace_draft(text_editor::Content::with_text(&rest));
        }
    }

    /// A first prompt is still local work until the provider accepts it, as
    /// are follow-ups held for a provider with no queue of its own. Keep them
    /// and any later composer edits together as an inert recovery draft, in
    /// the order they were written; restoring it never automatically submits
    /// any part.
    fn persistent_draft(&self) -> (String, Vec<Attachment>) {
        self.unsent_with_held(self.queued.as_ref())
    }

    /// The composer's text and attachments, after `queued`, the messages on
    /// their way the provider has not taken yet, and the held follow-ups,
    /// each part on its own line, every attachment once.
    fn unsent_with_held(&self, queued: Option<&Queued>) -> (String, Vec<Attachment>) {
        let followups = (!self.followups.is_empty()).then(|| self.combined_followups());
        let held = queued
            .map(|queued| &queued.prompt)
            .into_iter()
            .chain(self.commands.unsent().map(|outgoing| &outgoing.prompt))
            .map(|prompt| (prompt.text(), prompt.attachments.as_slice()))
            .chain(
                followups
                    .iter()
                    .map(|(text, attachments)| (text.as_str(), attachments.as_slice())),
            );
        let draft = self.unsent_text();
        let mut text = String::new();
        let mut attachments: Vec<Attachment> = Vec::new();
        for (part, files) in held.chain([(draft.as_str(), self.unsent_attachments())]) {
            if !part.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
            for attachment in files {
                if !attachments.contains(attachment) {
                    attachments.push(attachment.clone());
                }
            }
        }
        (text, attachments)
    }

    /// An empty draft can show a suggestion only for its exact current turn.
    fn prompt_suggestion(&self) -> Option<&chat::PromptSuggestion> {
        let suggestion = self.snapshot.prompt_suggestion.as_ref()?;
        if self.disconnected
            || !self.commands.is_idle()
            || self.queued.is_some()
            || self.handle().is_none()
            || self.turn_under_way()
            || !self.snapshot.ready
            || self.snapshot.exited
            || !self.composer.text().trim_end_matches('\n').is_empty()
            || !self.attachments.is_empty()
            || self.snapshot.session_id.as_deref() != Some(suggestion.session_id.as_str())
            || self.snapshot.turn_generation != suggestion.turn_generation
            || self
                .suggestion_dismissed
                .as_ref()
                .is_some_and(|(session, generation)| {
                    session == &self.snapshot.session_id
                        && *generation == self.snapshot.turn_generation
                })
            || !chat::PromptSuggestion::valid_text(&suggestion.text)
        {
            return None;
        }
        Some(suggestion)
    }

    fn dismiss_suggestion(&mut self) {
        self.suggestion_dismissed = Some((
            self.snapshot.session_id.clone(),
            self.snapshot.turn_generation,
        ));
    }

    fn accept_suggestion(&mut self, expected: &chat::PromptSuggestion) -> bool {
        if self.prompt_suggestion() != Some(expected) {
            return false;
        }
        self.dismiss_suggestion();
        self.replace_draft(text_editor::Content::with_text(&expected.text));
        self.history_cursor = None;
        self.history_stash = None;
        self.history_attachment_stash = None;
        true
    }

    fn cancel_attachment_jobs(&mut self) {
        self.attachment_jobs.clear();
        self.attachment_bookmarks.clear();
        self.attachment_order.clear();
        self.pastes_pending = 0;
        self.image_pastes_saving = 0;
        self.submit_after_paste = None;
    }

    /// Callback targets are lifetime fences. Moving a retained runtime must
    /// retire old insert ownership before those callbacks become unreachable.
    fn retarget(&mut self, target: Target) {
        if self.target != target {
            self.cancel_attachment_jobs();
            self.attachment_chooser_pending = None;
            self.target = target;
        }
    }

    /// Puts a whole new draft in the composer. Whatever the completion list
    /// was doing belonged to the old one.
    fn replace_draft(&mut self, composer: text_editor::Content<cosmic::Renderer>) {
        self.cancel_attachment_jobs();
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
            preparation: preparation::State::default(),
            target,
            session,
            snapshot: Arc::new(Snapshot {
                permission_mode,
                ..Snapshot::default()
            }),
            provider_queue: std::cell::RefCell::default(),
            source_clock: None,
            finished_stamp: None,
            connection,
            closed_elsewhere: false,
            usage_limit: None,
            limit_refresh_pending: false,
            visible_limit_episode: None,
            agent_addresses: Default::default(),
            queued: None,
            followups: Vec::new(),
            next_written: 0,
            recalling: Vec::new(),
            taken_back: Vec::new(),
            resend: None,
            restored_waiting: None,
            parked_queue: Vec::new(),
            composer_actions: false,
            composer_actions_query: String::new(),
            mode_before_plan: None,
            branch_picker: false,
            changes: changes::State::default(),
            logical_prose: logical_prose::State::default(),
            fork_requested: false,
            fork_seen: None,
            files: None,
            find: None,
            find_matches: indexmap::IndexSet::new(),
            find_disclosure: find::Disclosure::default(),
            child_find_disclosure: HashMap::new(),
            find_index: 0,
            find_navigation: find_navigation::State::default(),
            copied: None,
            interrupt_armed: None,
            drop_hover: false,
            cache: HashMap::new(),
            child_cache: HashMap::new(),
            child_cache_context: None,
            presentation_revision: 0,
            presentation_evicted: false,
            last_presented_tail: None,
            preview_request: None,
            syntax_theme,
            pull_request_labels: Arc::default(),
            pull_request_inspected: Default::default(),
            pull_request_labels_built_from: None,
            turns: Revised::default(),
            selected_subagent: None,
            observed_scroll: Default::default(),
            scroll_memory: Default::default(),
            figures: Default::default(),
            child_picker: None,
            image_viewer: None,
            child_tab_focus: Default::default(),
            child_ordinals: Default::default(),
            asked_after: HashSet::new(),
            asked: HashSet::new(),
            opened: Revised::default(),
            tool_groups_expanded: false,
            composer: text_editor::Content::new(),
            revision: 0,
            attachments: Vec::new(),
            thumbnails: thumbnails::Cache::default(),
            pastes_pending: 0,
            attachment_bookmarks: HashMap::new(),
            attachment_jobs: HashMap::new(),
            attachment_order: Vec::new(),
            image_pastes_saving: 0,
            submit_after_paste: None,
            attachment_chooser_pending: None,
            draft_echo: None,
            expanded: Revised::default(),
            child_expanded: HashMap::new(),
            child_unfolded: HashMap::new(),
            unfolded: HashSet::new(),
            answers: HashMap::new(),
            commands: commands::Commands::default(),
            next_command: 0,
            disconnected: false,
            listed_models: 0,
            visible: AtomicBool::new(visible),
            warm: None,
            error: None,
            capacity_refused: false,
            close_refusal: None,
            resume_failed: false,
            start_blocked: None,
            update_wait: UpdateWait::default(),
            resume_source_pinned: false,
            failures: Vec::new(),
            unplaced_failures: Vec::new(),
            failure_ui: failure::State::default(),
            permission_picker: false,
            permissions_form: permissions::Form::default(),
            account_picker: false,
            picker_focus: 0,
            picker_keyboard: false,
            history: Vec::new(),
            history_images: Vec::new(),
            history_attachment_stash: None,
            history_cursor: None,
            history_stash: None,
            selection_bookmarks: Default::default(),
            selection_document: Default::default(),
            scroll: view::Scroll::default(),
            subagent_scroll: HashMap::new(),
            page_request: None,
            deferred_page: None,
            unseen_activity: false,
            earlier_asked: None,
            later_asked: None,
            model_picker: None,
            completion: completion::State::default(),
            suggestion_dismissed: None,
        };
        runtime.refresh();
        runtime
    }
    /// Drops everything drawn from the conversation's transcript, for a card
    /// going back to a draft, which has none.
    fn forget_transcript(&mut self) {
        self.provider_queue.borrow_mut().clear();
        self.preparation.clear();
        self.cache.clear();
        self.child_cache.clear();
        self.child_cache_context = None;
        self.presentation_evicted = false;
        self.last_presented_tail = None;
        self.turns = Revised::default();
        self.selected_subagent = None;
        self.subagent_scroll.clear();
        self.page_request = None;
        self.deferred_page = None;
        self.scroll = view::Scroll::default();
        self.observed_scroll.borrow_mut().clear();
        self.scroll_memory.clear();
        self.selection_document.get_mut().take();
        self.selection_bookmarks.get_mut().clear();
        self.asked_after.clear();
        self.asked.clear();
        self.find_disclosure = find::Disclosure::default();
        self.child_find_disclosure.clear();
        self.opened = Revised::default();
        self.expanded = Revised::default();
        self.child_expanded.clear();
        self.child_unfolded.clear();
        self.answers.clear();
        self.failures.clear();
        self.unplaced_failures.clear();
        self.failure_ui = failure::State::default();
        self.visible_limit_episode = None;
        self.limit_refresh_pending = false;
        self.child_picker = None;
        self.child_tab_focus.borrow_mut().take();
        self.listed_models = 0;
        self.earlier_asked = None;
        self.later_asked = None;
    }
    fn notice_anchor<'a>(
        &self,
        snapshot: &'a Snapshot,
        turns: &'a [turn::Turn],
    ) -> Option<&'a str> {
        failure::prompt_anchor(self.session.launch.provider, &snapshot.items)
            .or_else(|| turns.last().map(|turn| turn.key.as_str()))
    }
    /// Groups `snapshot`'s items as this transcript draws them.
    fn transcript_turns(&self, snapshot: &Snapshot) -> Vec<turn::Turn> {
        turn::group(
            &snapshot.items,
            &self.asked_after,
            Grouping {
                presentations: Presentations(&self.cache),
                provider: self.session.launch.provider,
                failures: &self.failures,
            },
        )
    }
    /// Records a notice in the transcript, after the message it concerns.
    fn note(&mut self, notice: String) {
        if self.snapshot.has_later {
            self.defer_note(notice);
            self.refresh_failure_ui();
            return;
        }
        let anchor = self
            .notice_anchor(&self.snapshot, &self.turns)
            .map(str::to_owned);
        let after = failure::closing_item(&self.snapshot, &self.turns).map(str::to_owned);
        if self.note_at(anchor, after, notice) {
            self.refresh_failure_ui();
        }
    }
    fn defer_note(&mut self, notice: String) {
        if !self.unplaced_failures.contains(&notice) {
            if self.unplaced_failures.len() == 32 {
                self.unplaced_failures.remove(0);
            }
            self.unplaced_failures.push(notice);
        }
    }
    /// Records `notice` for the prompt message `anchor`, drawn after the
    /// group containing the item `after`: work a delivery started after that
    /// prompt still precedes the notice. Returns whether the notes changed;
    /// the caller refreshes their presentation once its inputs agree.
    fn note_at(&mut self, anchor: Option<String>, after: Option<String>, notice: String) -> bool {
        let provider = self.session.launch.provider;
        let mut changed = false;
        if chat::requires_sign_in(provider, &notice) {
            // An updated verification failure replaces this exchange's active
            // auth warning; other exchanges and tool failures remain history.
            let before = self.failures.len();
            self.failures.retain(|note| {
                !failure::same_prompt(provider, note.message.as_deref(), anchor.as_deref())
                    || !chat::requires_sign_in(provider, &note.text)
            });
            changed = self.failures.len() != before;
        }
        if let Some(index) = self.failures.iter().position(|note| {
            failure::same_prompt(provider, note.message.as_deref(), anchor.as_deref())
                && note.text == notice
        }) {
            // The same failure recurring later in this exchange is still one
            // notice, now after the work that produced it again. It becomes
            // the latest note, which places an incident it shares.
            if after.is_some() && self.failures[index].after != after {
                let mut note = self.failures.remove(index);
                note.after = after;
                self.failures.push(note);
                changed = true;
            }
            return changed;
        }
        if self.failures.len() == 32 {
            self.failures.remove(0);
        }
        self.failures.push(failure::Note {
            message: anchor,
            after,
            text: notice,
        });
        true
    }
    /// Puts a first message that was never sent back in the composer, ahead
    /// of anything written since, with its attachments.
    fn unqueue(&mut self) -> Option<chat::Session> {
        let mut queued = self.queued.take()?;
        self.preparation.clear_local();
        // Nothing waits on the snapshot of a hidden card any more; a
        // connection that has gone has nothing to be told.
        let _ = self.present();
        let written = self.composer.text();
        let text = if written.is_empty() {
            queued.prompt.text().to_owned()
        } else {
            format!("{}\n{written}", queued.prompt.text())
        };
        self.replace_draft(text_editor::Content::with_text(&text));
        let written = self.replace_attachments(std::mem::take(&mut queued.prompt.attachments));
        self.merge_attachments(written);
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
            Connection::Draft | Connection::AwaitingAdmission | Connection::Connecting => None,
            Connection::Live(handle) => Some(handle),
        }
    }
    /// The current confirmed connection's owner, without issuing a request.
    pub(super) fn startup_host_proof(&self) -> Option<crate::model::HostSessionProof> {
        if self.disconnected {
            return None;
        }
        self.handle()
            .filter(|handle| handle.is_connected())
            .and_then(|handle| handle.proof())
            .filter(|proof| proof.is_complete())
    }

    /// Readiness for the updater's saved-pane gate; no provider is launched here.
    pub(super) fn restored_pane_ready(&self) -> Result<bool, String> {
        if self.is_draft() {
            return Err("A hosted conversation became an unstarted draft".into());
        }
        if self.disconnected {
            return Err(self
                .error
                .clone()
                .unwrap_or_else(|| "Conversation is disconnected".into()));
        }
        let handle = self
            .handle()
            .ok_or("Conversation has no native attachment")?;
        if !handle.is_connected() {
            return Err("Conversation connection closed during recovery".into());
        }
        let snapshot = handle.snapshot();
        if self.is_saved_history_snapshot(&snapshot) {
            return Ok(!snapshot.history_loading);
        }
        if snapshot.exited || snapshot.read_only_reason.is_some() {
            return Err("Conversation cannot accept a message after recovery".into());
        }
        Ok(snapshot.ready && !snapshot.history_loading)
    }

    fn is_saved_history_snapshot(&self, snapshot: &Snapshot) -> bool {
        // The attached reader has already passed the daemon's lineage check.
        // Rendering uses its actual read-only snapshot, never an on-disk index
        // which an update helper may change while this window is still open.
        matches!(
            self.session.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        ) && self.session.terminal.is_none()
            && self.session.owned_codex_listener.is_none()
            && self
                .session
                .proof
                .as_ref()
                .is_some_and(HostSessionProof::is_complete)
            && snapshot.exited
            && snapshot.read_only_reason.is_some()
            && snapshot.session_id == self.session.launch.session_id
            && self.handle().is_some_and(|handle| {
                handle.attached_session_id() == snapshot.session_id.as_deref()
            })
            && matches!(
                snapshot.native_continuity,
                crate::chat::NativeContinuity::ReconnectOnly
            )
            && snapshot.error.is_none()
            && !snapshot.busy
            && !snapshot.waiting
            && snapshot.requests.is_empty()
    }

    fn has_connected_saved_history(&self, snapshot: &Snapshot) -> bool {
        !self.disconnected
            && self.handle().is_some_and(|handle| handle.is_connected())
            && self.is_saved_history_snapshot(snapshot)
    }

    /// A history reader has no provider commands, but its local draft can
    /// still hold Claude attachments for an explicit, separately admitted
    /// resume. Do not turn this into a provider capability or send permission.
    fn accepts_draft_attachments(&self) -> bool {
        self.snapshot.capabilities.attachments || self.has_connected_saved_history(&self.snapshot)
    }

    /// A conversation attached to here whose first start has yet to finish. Its
    /// provider would refuse a message, so one waits in the composer until it
    /// can go. One with history that isn't ready has lost its provider instead,
    /// and sending there is what tells the reader so.
    pub(super) fn provider_coming_up(&self) -> bool {
        !self.is_draft()
            && self.connected()
            && !self.snapshot.ready
            && !self.snapshot.exited
            && self.snapshot.error.is_none()
            && self.snapshot.items.is_empty()
            && self.queued.is_none()
    }

    /// Whether the composer takes a message now. A first or queued message
    /// still waiting goes before anything else is sent. A command on its way
    /// never closes it: what is given meanwhile waits behind that command.
    pub(super) fn composer_available(&self, transitioning: bool) -> bool {
        !transitioning
            && !self.closed_elsewhere
            && !self.disconnected
            && !self.snapshot.exited
            && self.queued.is_none()
    }

    /// The `+` menu's rows in the order it lists them, each with whether it
    /// can be pressed now. Attaching stays listed while a draft cannot take
    /// one, as the paperclip it replaced stayed visible but disabled.
    pub(super) fn composer_action_rows(&self, transitioning: bool) -> Vec<(ComposerRow, bool)> {
        let snapshot = &self.snapshot;
        let available = self.composer_available(transitioning);
        let commands =
            available && snapshot.ready && !snapshot.busy && snapshot.read_only_reason.is_none();
        let attach = !transitioning
            && (!snapshot.exited || self.has_connected_saved_history(snapshot))
            && !self.closed_elsewhere
            && !self.disconnected
            && self.queued.is_none()
            && self.attachment_chooser_pending.is_none();
        [
            (
                self.accepts_draft_attachments(),
                ComposerRow::Attach,
                attach,
            ),
            (
                self.plan_toggle().is_some(),
                ComposerRow::Plan,
                // The footer's mode control is offered on the same terms.
                !transitioning
                    && !(self.session.launch.provider == Provider::Codex && snapshot.busy),
            ),
            (
                !snapshot.capabilities.interrupt_modes.is_empty(),
                ComposerRow::Steering,
                available,
            ),
            (snapshot.capabilities.branch, ComposerRow::Fork, commands),
            (
                snapshot.capabilities.compact,
                ComposerRow::Compact,
                commands,
            ),
        ]
        .into_iter()
        .filter_map(|(listed, row, enabled)| listed.then_some((row, enabled)))
        .collect()
    }

    /// The permission mode the `+` menu's plan row switches to: plan mode,
    /// or out of it to the mode it replaced. `None` where the provider has no
    /// plan mode, or nothing else to return to.
    pub(super) fn plan_toggle(&self) -> Option<&str> {
        let modes = &self.snapshot.capabilities.permission_modes;
        modes.iter().any(|mode| mode == PLAN_MODE).then_some(())?;
        if self.chosen_permission_mode() != Some(PLAN_MODE) {
            return Some(PLAN_MODE);
        }
        let other = |mode: &&String| mode.as_str() != PLAN_MODE;
        self.mode_before_plan
            .as_ref()
            .filter(|mode| modes.contains(mode))
            .filter(other)
            .or_else(|| {
                modes
                    .iter()
                    .find(|mode| matches!(mode.as_str(), "default" | "manual"))
            })
            .or_else(|| modes.iter().find(other))
            .map(String::as_str)
    }

    /// What pressing a row of the `+` menu does: the action the control it
    /// stands for has always sent.
    pub(super) fn composer_row_action(&self, row: ComposerRow) -> Action {
        let target = self.target;
        match row {
            ComposerRow::Attach => Action::Attach(target),
            ComposerRow::Plan => {
                Action::PermissionMode(target, self.plan_toggle().unwrap_or(PLAN_MODE).to_owned())
            }
            ComposerRow::Steering => {
                let wait = self.snapshot.interrupt_mode.as_deref() == Some("wait");
                Action::Command(
                    target,
                    Command::SetInterruptMode {
                        mode: if wait { "immediate" } else { "wait" }.into(),
                    },
                )
            }
            ComposerRow::Fork => Action::BranchPicker(target, true),
            ComposerRow::Compact => Action::Command(target, Command::Compact),
        }
    }

    /// The commands and skills the `+` menu names into the draft: the ones
    /// slash completion offers for a `/` at the caret, matching the menu's
    /// search. Commands are offered only for an empty draft, the one place
    /// the provider reads them as commands, as completion offers them only
    /// at the head of a message. Compaction has a row of its own.
    fn composer_menu_names(&self) -> Vec<completion::Candidate<'_>> {
        let blank = self.attachments.is_empty()
            && self
                .composer
                .lines()
                .all(|line| line.text.trim().is_empty());
        self.names_matching(&self.composer_actions_search(), blank)
    }

    /// The menu's search as it is matched: a slash or skill sigil typed out
    /// of habit is not part of any name.
    fn composer_actions_search(&self) -> String {
        self.composer_actions_query
            .trim()
            .trim_start_matches([completion::COMMAND_SIGIL, '$'])
            .to_lowercase()
    }

    /// Whether the `+` menu has commands or skills to search among, which
    /// with the composer's own `/resume` is every chat it is offered in. It
    /// does not depend on the draft, so the search keeps its place while
    /// words typed into the draft set the commands aside, and while a query
    /// narrows the list to nothing, so it can be undone.
    pub(super) fn composer_menu_searchable(&self) -> bool {
        !self.names_matching("", true).is_empty()
    }

    fn names_matching(&self, query: &str, blank: bool) -> Vec<completion::Candidate<'_>> {
        completion::candidates(
            &self.snapshot,
            &completion::Token {
                line: 0,
                start: 0,
                end: 0,
                sigil: completion::COMMAND_SIGIL,
                query: query.to_owned(),
                tail: String::new(),
                leading: blank,
            },
        )
        .into_iter()
        .filter(|row| !(row.kind == completion::Kind::Command && row.name == COMPACT_COMMAND))
        .collect()
    }

    /// The `+` menu in the order it lists its entries, narrowed by its
    /// search, each with whether it can be pressed now.
    fn composer_menu(&self, transitioning: bool) -> Vec<(MenuEntry<'_>, bool)> {
        let query = self.composer_actions_search();
        // Every word searched for starts a word of what the row does.
        let found = |row: ComposerRow| {
            query.split_whitespace().all(|word| {
                row.keywords()
                    .split_whitespace()
                    .any(|keyword| keyword.starts_with(word))
            })
        };
        // Naming something changes only the draft, which stays editable
        // while a turn runs and while a message waits to go.
        let names = !transitioning && !self.closed_elsewhere && !self.disconnected;
        self.composer_action_rows(transitioning)
            .into_iter()
            .filter(|(row, _)| found(*row))
            .map(|(row, enabled)| (MenuEntry::Row(row), enabled))
            .chain(
                self.composer_menu_names()
                    .into_iter()
                    .map(|name| (MenuEntry::Named(name), names)),
            )
            .collect()
    }

    /// Names `insertion` in the draft as a word of its own, through the same
    /// edit completion makes: at the caret, or at the head of the message for
    /// a skill the provider would not load from the caret, as Claude Code
    /// loads only a skill the message opens with. The caret stays with the
    /// words it was among.
    fn name_in_draft(&mut self, insertion: &str, kind: completion::Kind) {
        let caret = self.composer.cursor().position;
        let at_caret = self.spaced_name(caret, insertion);
        // A message that opens with a command or a shell escape is read as
        // that, so a skill is never put ahead of it.
        let opened = self
            .composer
            .text()
            .trim_start()
            .starts_with([completion::COMMAND_SIGIL, '!']);
        let lead = kind == completion::Kind::Skill && !opened && {
            let offset = self
                .composer
                .lines()
                .take(caret.line)
                .map(|line| line.text.len() + line.ending.as_str().len())
                .sum::<usize>()
                + caret.column;
            let mut message = self.composer.text();
            message.insert_str(offset.min(message.len()), &at_caret);
            completion::unloaded(
                &message,
                &self.snapshot.skills,
                self.snapshot.capabilities.skill_reach,
            )
            .contains(&insertion)
        };
        let at = if lead {
            text_editor::Position { line: 0, column: 0 }
        } else {
            caret
        };
        let named = if lead {
            self.spaced_name(at, insertion)
        } else {
            at_caret
        };
        self.composer.move_to(text_editor::Cursor {
            position: at,
            selection: None,
        });
        self.composer
            .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                Arc::new(named.clone()),
            )));
        self.named_in_draft();
        if lead {
            let column = caret.column + if caret.line == 0 { named.len() } else { 0 };
            self.composer.move_to(text_editor::Cursor {
                position: text_editor::Position { column, ..caret },
                selection: None,
            });
        }
    }

    /// `insertion` as it is put in the draft at `at`: set apart by a space
    /// from a word before it and from what follows, unless that is a space
    /// already or punctuation that closes a phrase, and followed by one at
    /// the end of a line, ready for the rest of the sentence.
    fn spaced_name(&self, at: text_editor::Position, insertion: &str) -> String {
        let line = self.composer.line(at.line);
        let text = line.as_ref().map_or("", |line| &line.text);
        let before = text
            .get(..at.column)
            .and_then(|text| text.chars().next_back());
        let after = text.get(at.column..).and_then(|text| text.chars().next());
        let lead = if before.is_some_and(|before| !before.is_whitespace()) {
            " "
        } else {
            ""
        };
        let trail = match after {
            Some(after) if after.is_whitespace() || ",.;!?)]}".contains(after) => "",
            _ => " ",
        };
        format!("{lead}{insertion}{trail}")
    }

    pub(super) fn parent_transcript_selected(&self) -> bool {
        self.selected_subagent.is_none()
    }

    /// Whether nothing has started yet, a start for the first message
    /// included.
    fn is_draft(&self) -> bool {
        matches!(self.connection, Connection::Draft)
    }
    /// Lift a permanent first-start refusal once the draft names a different
    /// launch. That refusal and its diagnostic answered the previous one.
    fn release_start_block(&mut self) {
        if self.start_blocked.take().is_some() {
            self.error = None;
        }
    }
    /// Whether this draft's provider, model and account can still change: it
    /// has not started, and its first message is not on its way.
    fn choosing(&self) -> bool {
        self.is_draft() && self.queued.is_none() && self.attachment_chooser_pending.is_none()
    }
    /// Reads pull request links as `labels` say from now on, and parses again
    /// only the messages whose links now read otherwise; the rest keep their
    /// parsed text and their place.
    fn set_pull_request_labels(&mut self, labels: Arc<pull_request_links::Labels>) {
        if *self.pull_request_labels == *labels {
            return;
        }
        let previous = std::mem::replace(&mut self.pull_request_labels, labels.clone());
        self.preparation.set_labels(labels.clone());
        let mut changed = false;
        // Each prepared message knows what it links to, so this reads no
        // text. One still being prepared is checked when it arrives.
        for cached in self.cache.values_mut().chain(self.child_cache.values_mut()) {
            if cached
                .pull_request_links
                .iter()
                .all(|key| previous.reads_alike(&labels, key))
            {
                continue;
            }
            if let Some(id) = cached.relabel() {
                self.preparation.forget(&id);
                changed = true;
            }
        }
        if changed && self.visible.load(Ordering::Relaxed) {
            let snapshot = self.snapshot.clone();
            self.prepare_transcript_cache(&snapshot);
        }
        if changed {
            // A search reads links as their labels, which have just changed.
            self.refresh_find_work();
        }
    }

    /// Highlights the transcript's code blocks in `theme` from now on.
    pub(super) fn set_syntax_theme(&mut self, theme: crate::settings::EditorSyntaxTheme) {
        if self.syntax_theme == theme {
            return;
        }
        self.syntax_theme = theme;
        self.preparation.clear();
        for cached in self.cache.values_mut().chain(self.child_cache.values_mut()) {
            if cached.item.as_ref().is_some_and(|item| {
                CachedItem::prose(&item.content).is_some()
                    || chat::coordination::may_recognize(item, self.session.launch.provider)
            }) {
                // Keep logical text/selection while the new palette is
                // prepared, without highlighting all historical fences here.
                preparation::retire_styles(
                    std::mem::take(&mut cached.blocks),
                    cached.palette_views.take(),
                );
                cached.open = None;
                cached.preparing = true;
                cached.revision = CachedItem::next_revision();
            } else {
                cached.rehighlight(theme);
            }
        }
        if self.visible.load(Ordering::Relaxed) {
            let snapshot = self.snapshot.clone();
            self.prepare_transcript_cache(&snapshot);
        }
    }
    fn source_now_ms(&self) -> Option<u64> {
        self.source_clock
            .map(chat::client::SourceClock::now)
            .or_else(chat::now_ms)
    }

    fn parent_scroll(&self) -> view::Scroll {
        self.observed_scroll
            .borrow()
            .get(&None)
            .copied()
            .unwrap_or(self.scroll)
    }
    fn item_expanded(&self, id: &str) -> bool {
        match &self.selected_subagent {
            Some(child) => self
                .child_expanded
                .get(child)
                .is_some_and(|set| set.contains(id)),
            None => self.expanded.contains(id),
        }
    }
    fn part_unfolded(&self, key: &str) -> bool {
        match &self.selected_subagent {
            Some(child) => self
                .child_unfolded
                .get(child)
                .is_some_and(|set| set.contains(key)),
            None => self.unfolded.contains(key),
        }
    }
    fn prepared_prompt(&self, item: &chat::Item) -> Option<&prompt::Prepared> {
        self.presentation_cache()
            .get(&item.id)
            .filter(|cached| {
                !cached.placeholder
                    && cached
                        .item
                        .as_deref()
                        .is_some_and(|shown| std::ptr::eq(shown, item))
            })?
            .prompt
            .as_deref()
    }
    fn presentation_cache(&self) -> &HashMap<String, CachedItem> {
        if self.selected_subagent.is_some() {
            &self.child_cache
        } else {
            &self.cache
        }
    }
    fn presentation_cache_mut(&mut self) -> &mut HashMap<String, CachedItem> {
        if self.selected_subagent.is_some() {
            &mut self.child_cache
        } else {
            &mut self.cache
        }
    }
    fn active_item(&self, id: &str) -> Option<&Arc<chat::Item>> {
        let items = match self.selected_subagent.as_deref() {
            Some(id) => {
                &self
                    .snapshot
                    .subagents
                    .iter()
                    .find(|agent| agent.id == id)?
                    .items
            }
            None => &self.snapshot.items,
        };
        items.iter().find(|item| item.id == id)
    }
    fn prepare_child_cache(&mut self, snapshot: &Snapshot) {
        self.prepare_transcript_cache(snapshot);
    }
    fn active_scroll(&self) -> view::Scroll {
        self.observed_scroll
            .borrow()
            .get(&self.selected_subagent)
            .copied()
            .unwrap_or_else(|| match self.selected_subagent.as_ref() {
                Some(id) => self.subagent_scroll.get(id).copied().unwrap_or_default(),
                None => self.scroll,
            })
    }

    fn prepare_transcript_cache(&mut self, snapshot: &Snapshot) -> preparation::MetadataRetirement {
        self.preparation.take_deferred();
        self.prepare_transcript_cache_from(snapshot, None)
    }

    fn prepare_local_prompt(&mut self) {
        if !self.visible.load(Ordering::Relaxed) {
            self.preparation.clear_local();
            return;
        }
        match self.local_prompt() {
            Some(prompt) => {
                if prompt.presentation.prepared.is_none() {
                    let source = prompt.source.clone();
                    self.preparation
                        .stage_local(self.target, &source, self.syntax_theme);
                }
            }
            None => self.preparation.clear_local(),
        }
    }

    fn resume_preparation(&mut self) {
        self.prepare_local_prompt();
        if !self.visible.load(Ordering::Relaxed) {
            return;
        }
        if let Some(cursor) = self.preparation.take_deferred() {
            let snapshot = self.snapshot.clone();
            self.prepare_transcript_cache_from(&snapshot, Some(cursor));
        }
    }

    fn prepare_transcript_cache_from(
        &mut self,
        snapshot: &Snapshot,
        resume: Option<usize>,
    ) -> preparation::MetadataRetirement {
        let mut retired_metadata = preparation::MetadataRetirement::default();
        let _stage = chat_stage::scope(Stage::PrepareChild);
        let mut presentation_changed = false;
        let context = self.selected_subagent.clone();
        let theme = self.syntax_theme;
        let provider = self.session.launch.provider;
        let handle = self.handle().cloned();
        let reader_proofs = self.scroll_memory.reader_proofs(context.as_deref());
        self.preparation.set_reader_proofs(reader_proofs.clone());
        self.preparation.set_provider(provider);
        let expanded = context
            .as_ref()
            .and_then(|id| self.child_expanded.get(id))
            .unwrap_or(&self.expanded);
        let cache = if context.is_some() {
            &mut self.child_cache
        } else {
            &mut self.cache
        };
        // Each transcript has its own streaming lifecycle: a child can keep
        // running after the parent finishes. Only its newest item can grow;
        // older items release their parser and highlighter continuation.
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| snapshot.subagents.iter().find(|agent| &agent.id == id));
        let (items, busy) = selected.map_or((snapshot.items.as_slice(), snapshot.busy), |agent| {
            (
                agent.items.as_slice(),
                agent.status == chat::ToolStatus::Running,
            )
        });
        if resume.is_none()
            && self.preparation.context(
                self.target,
                snapshot.session_id.as_deref(),
                context.as_deref(),
                items,
            )
        {
            // The same opaque item ID may exist in two different transcripts.
            // Never display another actor/child's old prose while loading it.
            cache.retain(|_, cached| {
                cached.reader_context == context
                    && cached
                        .item
                        .as_ref()
                        .is_some_and(|item| items.iter().any(|current| Arc::ptr_eq(current, item)))
            });
            presentation_changed = true;
        }
        let newest = items.len().wrapping_sub(1);
        let start = resume.unwrap_or(0).min(items.len());
        let end = resume.map_or(items.len(), |_| start.saturating_add(128).min(items.len()));
        let mut stopped = false;
        // Admit the rows a returning reader's saved window showed first, then
        // from the newest item back: the streaming answer and the rows a
        // reader at the latest edge sees come next, and admission that runs
        // out leaves the oldest history waiting. The cursor stores the
        // newest-back position; the saved window's rows lead every pass, and
        // are cheap to pass over once staged.
        let reading = self.scroll_memory.reading_rows(context.as_deref());
        let returning: Vec<usize> = (0..items.len())
            .rev()
            .filter(|&index| reading.contains(&items[index].id))
            .map(|index| newest - index)
            .collect();
        let walk = returning.iter().copied().chain(
            (start..end).filter(|&position| !reading.contains(&items[newest - position].id)),
        );
        for position in walk {
            let index = newest - position;
            let item = &items[index];
            let report = matches!(
                &item.content,
                ItemContent::Delivery {
                    kind: chat::DeliveryKind::Report,
                    ..
                }
            );
            // A Report is an immutable delivery publication, even while its
            // parent or selected child is working. Its Ready must retire the
            // worker owner instead of retaining a streaming canonical copy.
            let growing = busy && index == newest && !report;
            if cache.get(&item.id).is_some_and(|cached| {
                cached.diagnostic
                    && cached
                        .item
                        .as_ref()
                        .is_some_and(|previous| Arc::ptr_eq(previous, item))
            }) {
                continue;
            }
            if failure::diagnostic_item(provider, item) {
                let unchanged = cache
                    .get(&item.id)
                    .and_then(|cached| cached.item.as_ref())
                    .is_some_and(|previous| Arc::ptr_eq(previous, item));
                if !unchanged {
                    let placeholder = ItemContent::Text {
                        text: String::new(),
                        thinking: false,
                        phase: chat::Phase::Commentary,
                    };
                    let mut cached =
                        CachedItem::updated(&placeholder, theme, None, false, false, None);
                    cached.token = ChangeToken::of(&item.content);
                    cached.diagnostic = true;
                    cached.item = Some(item.clone());
                    cached.reader_memory = self.scroll_memory.clone();
                    cached.figures = self.figures.shared(context.is_some());
                    cached.reader_context.clone_from(&context);
                    cache.insert(item.id.clone(), cached);
                    presentation_changed = true;
                }
                continue;
            }
            if report && !expanded.contains(&item.id) {
                let unchanged = cache.get(&item.id).is_some_and(|cached| {
                    cached.report_expanded == Some(false)
                        && cached
                            .item
                            .as_ref()
                            .is_some_and(|old| Arc::ptr_eq(old, item))
                });
                if !unchanged {
                    let previous = cache.remove(&item.id);
                    let mut cached =
                        CachedItem::updated(&item.content, theme, previous, false, false, None);
                    cached.item = Some(item.clone());
                    cached.reader_memory = self.scroll_memory.clone();
                    cached.figures = self.figures.shared(context.is_some());
                    cached.reader_context.clone_from(&context);
                    cache.insert(item.id.clone(), cached);
                    presentation_changed = true;
                }
                continue;
            }
            let report_reveal = report
                && cache
                    .get(&item.id)
                    .is_none_or(|cached| cached.report_expanded != Some(true));
            let coordinated = chat::coordination::may_recognize(item, provider);
            let reader_only = !reader_proofs.is_empty()
                && preparation::has_tool_reader_proofs(item, &reader_proofs);
            if CachedItem::prose(&item.content).is_some()
                || literal_content::Prepared::required(&item.content)
                || reader_only
                || coordinated
            {
                // Only validated append lineage with equal endpoint lengths
                // proves unchanged text without scanning a large body on UI.
                // Shape/role/context changes and unknown full replacements
                // retain ordinary admitted worker preparation.
                let previous = cache
                    .get(&item.id)
                    .filter(|cached| {
                        !cached.placeholder
                            && cached.coordination.is_none()
                            && cached.reader_context == context
                    })
                    .and_then(|cached| cached.item.as_ref())
                    .cloned();
                let metadata_proven = previous.as_ref().is_some_and(|previous| {
                    !Arc::ptr_eq(previous, item)
                        && previous.role == item.role
                        && matches!((&previous.content, &item.content),
                            (ItemContent::Text { text: old, thinking: a, phase: x },
                             ItemContent::Text { text: next, thinking: b, phase: y })
                            if old.len() == next.len() && a == b && x == y)
                        && known_append(handle.as_ref(), &self.logical_prose, previous, item)
                });
                let metadata_reused = metadata_proven
                    && self.preparation.retarget_metadata(
                        previous.as_ref().unwrap(),
                        item,
                        growing,
                        theme,
                        cache[&item.id].preparing,
                        cache[&item.id].source.provisional(),
                        &mut retired_metadata,
                    );
                if metadata_reused {
                    let old = cache
                        .get_mut(&item.id)
                        .unwrap()
                        .item
                        .replace(item.clone())
                        .unwrap();
                    retired_metadata.push(old);
                }
                let current = cache.get(&item.id);
                let exact = current
                    .and_then(|cached| cached.item.as_ref())
                    .is_some_and(|previous| Arc::ptr_eq(previous, item));
                let pending = current.is_some_and(|cached| cached.preparing)
                    || self.preparation.contains(&item.id);
                let proven_from_cache = current
                    .and_then(|cached| cached.item.as_ref())
                    .is_some_and(|previous| {
                        previous.role == item.role
                            && ChangeToken::of(&previous.content).shape
                                == ChangeToken::of(&item.content).shape
                            && known_append(handle.as_ref(), &self.logical_prose, previous, item)
                    });
                // Bytes alone do not bound rich-block parsing or thousands
                // of tiny fences. Every changed prose publication goes to the
                // worker; the last proven prefix remains visible meanwhile.
                if !metadata_reused && (pending || !exact || report_reveal) {
                    let replaced = self.preparation.stage_reusing(
                        item,
                        growing && (!reader_only || coordinated),
                        theme,
                        |old, new| known_append(handle.as_ref(), &self.logical_prose, old, new),
                        || {
                            current
                                .into_iter()
                                .flat_map(|cached| {
                                    cached.blocks.iter().filter(|(key, block)| {
                                        block.is_settled() && Some(**key) != cached.open
                                    })
                                })
                                .map(|(key, block)| (*key, block.settled_clone()))
                                .collect()
                        },
                    );
                    // The answer being written, then the rows the reader
                    // returns to, then everything else from the newest back.
                    self.preparation.rank(
                        &item.id,
                        if growing {
                            0
                        } else if reading.contains(&item.id) {
                            1
                        } else {
                            2 + position as u64
                        },
                    );
                    // A presentation that reaches further into the same
                    // revision of a long message is not an append when its
                    // window starts elsewhere, but what is shown stays true
                    // of it until the worker finishes.
                    let superseded = current
                        .filter(|cached| !cached.placeholder)
                        .and_then(|cached| cached.item.as_ref())
                        .is_some_and(|previous| self.logical_prose.supersedes(previous, item));
                    if replaced && !exact && !proven_from_cache && !superseded {
                        let replacement = CachedItem::preparing_replacement(
                            item,
                            theme,
                            cache
                                .get(&item.id)
                                .filter(|cached| cached.reader_context == context),
                        );
                        cache.insert(item.id.clone(), replacement);
                        presentation_changed = true;
                    }
                    if !cache.contains_key(&item.id) {
                        presentation_changed = true;
                    }
                    let cached = cache
                        .entry(item.id.clone())
                        .or_insert_with(|| CachedItem::preparing(item, theme));
                    cached.preparing = true;
                    if report {
                        cached.report_expanded = Some(true);
                        // A newly opened report has no authorized parsed body
                        // until the admitted worker returns its exact DTO.
                        cached.placeholder |= report_reveal;
                    }
                    cached.reader_memory = self.scroll_memory.clone();
                    cached.figures = self.figures.shared(context.is_some());
                    cached.reader_context.clone_from(&context);
                    if self.preparation.was_deferred() {
                        // A saved row that waits is met again at the head of
                        // the next pass; the newest-back walk resumes where
                        // this pass's own range starts.
                        self.preparation.defer(if reading.contains(&item.id) {
                            start
                        } else {
                            position
                        });
                        if resume.is_some() {
                            stopped = true;
                            break;
                        }
                    }
                    continue;
                }
            }
            let token = ChangeToken::of(&item.content);
            let mut image_changed = false;
            let append_proven = cache
                .get(&item.id)
                .and_then(|cached| cached.item.as_ref())
                .filter(|previous| !Arc::ptr_eq(previous, item))
                .is_some_and(|previous| {
                    known_append(handle.as_ref(), &self.logical_prose, previous, item)
                });
            if let Some(cached) = cache.get_mut(&item.id) {
                cached.reader_memory = self.scroll_memory.clone();
                cached.figures = self.figures.shared(context.is_some());
                cached.reader_context.clone_from(&context);
                if matches!(item.content, ItemContent::Image { .. }) {
                    // Snapshot items are immutable. A retained Arc makes the
                    // common unchanged-item check constant-time; compare image
                    // content only when a replacement Arc arrives. Tail tokens
                    // alone can collide for different same-length image paths.
                    image_changed = cached.image_item.as_ref().is_none_or(|previous| {
                        !Arc::ptr_eq(previous, item) && previous.content != item.content
                    });
                    if !image_changed {
                        cached.image_item = Some(item.clone());
                    }
                }
                let unchanged = cached.item.as_ref().is_some_and(|previous| {
                    Arc::ptr_eq(previous, item) || previous.content == item.content
                });
                if unchanged {
                    cached.item = Some(item.clone());
                    if !growing {
                        let previous_revision = cached.revision;
                        cached.settle(&item.content, theme);
                        presentation_changed |= cached.revision != previous_revision;
                    }
                    continue;
                }
                if let Some(previous) = cached.item.as_ref()
                    && let ItemContent::Text { text, .. } = &previous.content
                    && let Some(body) =
                        cached
                            .token
                            .appended_known(text, &item.content, append_proven)
                {
                    cached.extend(body, growing, theme);
                    presentation_changed = true;
                    cached.token = token;
                    cached.item = Some(item.clone());
                    continue;
                }
            }
            let previous = cache.remove(&item.id).filter(|_| !image_changed);
            let mut cached = CachedItem::updated(
                &item.content,
                theme,
                previous,
                expanded.contains(&item.id),
                growing,
                None,
            );
            if matches!(item.content, ItemContent::Image { .. }) {
                cached.image_source = Some(item.clone());
                cached.image_item = Some(item.clone());
            }
            cached.item = Some(item.clone());
            cached.reader_memory = self.scroll_memory.clone();
            cached.figures = self.figures.shared(context.is_some());
            cached.reader_context.clone_from(&context);
            cache.insert(item.id.clone(), cached);
            presentation_changed = true;
        }
        let today = chat::now_ms().and_then(|now| chat::local_time((now / 1000) as i64));
        for position in returning.iter().copied().chain(start..end) {
            let index = newest - position;
            let item = &items[index];
            if let Some(cached) = cache.get_mut(&item.id)
                && cached.stamp.as_ref().map(|stamp| stamp.0) != item.at
            {
                cached.stamp = item
                    .at
                    .and_then(|at| recorded_message_stamp(at, today.as_ref()));
            }
        }
        if resume.is_some() && !stopped && end < items.len() {
            self.preparation.defer(end);
            self.preparation.reschedule();
        }
        self.child_cache_context.clone_from(&context);
        if presentation_changed {
            self.presentation_revision = self.presentation_revision.wrapping_add(1);
            self.find_navigation.changed_document();
        }
        retired_metadata
    }

    /// Install the answer being written in the update that staged it, so
    /// each streamed chunk costs one frame rather than two. Waiting for the
    /// parse, rather than drawing the old text meanwhile, costs the UI
    /// thread less than that frame would.
    fn settle_streaming_answer(&mut self) -> Task<Message> {
        /// Streamed publications parse in under a millisecond at the 95th
        /// percentile, and a frame of the transcript costs about 3 ms.
        const BUDGET: Duration = Duration::from_millis(2);
        // Only a chat on screen parses, as only it has subscriptions.
        if !self.presents() || !self.visible.load(Ordering::Relaxed) {
            return Task::none();
        }
        match self.preparation.settle_tail(self.target, BUDGET) {
            preparation::Settled::Idle => Task::none(),
            preparation::Settled::Ready(id, run, ready) => {
                self.markdown_prepared(self.target, &id, run, ready);
                Task::none()
            }
            preparation::Settled::Pending(task) => task,
        }
    }

    fn markdown_prepared(&mut self, target: Target, id: &str, run: u64, ready: preparation::Ready) {
        if !self.presents() {
            self.preparation.unclaim(id, run);
            return;
        }
        // Observe any replacement already published by the client before
        // validating the worker lineage; provider and UI events may interleave.
        self.refresh();
        let Some(prepared) = self.preparation.accept(target, id, run, ready) else {
            return;
        };
        let managed = self.preparation.contains(id);
        let expanded = self.item_expanded(id);
        if self
            .presentation_cache()
            .get(id)
            .is_some_and(|cached| cached.diagnostic)
            || (!expanded
                && matches!(
                    &prepared.item().content,
                    ItemContent::Delivery {
                        kind: chat::DeliveryKind::Report,
                        ..
                    }
                ))
        {
            // Disclosure/source may change while a worker is running. Retire
            // its owned output through the existing queue, never resurrect a
            // closed report or overwrite an exact diagnostic placeholder.
            prepared.discard();
            self.resume_preparation();
            return;
        }
        let labels = self.pull_request_labels.clone();
        let Some(cached) = self.presentation_cache_mut().get_mut(id) else {
            prepared.discard();
            return;
        };
        let regroup = cached.coordination.is_some() || prepared.is_coordination();
        let parsed_with = prepared.labels().clone();
        prepared.install(cached, managed, expanded);
        // The labels changed while it was being prepared.
        let stale = !Arc::ptr_eq(&parsed_with, &labels)
            && cached
                .pull_request_links
                .iter()
                .any(|key| !parsed_with.reads_alike(&labels, key));
        if stale && let Some(id) = cached.relabel() {
            self.preparation.forget(&id);
            if self.visible.load(Ordering::Relaxed) {
                let snapshot = self.snapshot.clone();
                self.prepare_transcript_cache(&snapshot);
            }
        }
        self.resume_preparation();
        if regroup && self.selected_subagent.is_none() {
            let turns = self.transcript_turns(&self.snapshot);
            carry_step_disclosures(
                &mut self.opened,
                &mut self.find_disclosure,
                &self.turns,
                &self.snapshot.items,
                &turns,
                &self.snapshot.items,
            );
            self.turns = turns.into();
            reconcile_disclosure(&mut self.opened, &self.turns);
        }
        self.presentation_revision = self.presentation_revision.wrapping_add(1);
        self.find_navigation.changed_document();
        self.refresh_find();
    }

    fn refresh(&mut self) {
        self.prepare_local_prompt();
        self.refresh_session();
        self.refresh_snapshot();
        self.settle_draft_echo();
    }

    fn refresh_snapshot(&mut self) {
        if self.retains_source_body() {
            // Warmth only owns disposable rendering. The retired source can
            // no longer hydrate this window, so neither eviction nor later
            // metadata from its connected client may replace it or its clock.
            if self.visible.load(Ordering::Relaxed) && self.presentation_evicted {
                self.install_presented_snapshot(self.snapshot.clone(), self.source_clock);
            }
            return;
        }
        if self.warm.is_some()
            && self
                .handle()
                .is_some_and(|handle| handle.resident_snapshot_with_clock().is_none())
        {
            // The client gave up the body it was keeping (a resync, or the
            // connection ended), so this presentation can no longer follow it.
            self.cool();
        }
        if self.warm.is_some()
            && !self.visible.load(Ordering::Relaxed)
            && self.handle().is_some_and(|handle| {
                handle
                    .resident_snapshot_with_clock()
                    .is_some_and(|(snapshot, _)| snapshot.generation != self.snapshot.generation)
            })
        {
            // A warm chat with queued work reads while hidden. Rather than
            // prepare what it reads where nobody sees it, it goes cold and
            // follows its hidden metadata as any other hidden chat does.
            self.cool();
        }
        if !self.presents() {
            if let Some((snapshot, clock)) = self
                .handle()
                .and_then(|handle| handle.hidden_snapshot_with_clock())
            {
                self.install_snapshot(snapshot, clock);
            }
            return;
        }
        let Some(handle) = self.handle() else {
            return;
        };
        if let Some((snapshot, source_clock)) = handle.resident_snapshot_with_clock() {
            self.install_snapshot(snapshot, source_clock);
        } else if !handle.is_connected() && self.presentation_evicted {
            // A failed hide can leave the client with metadata while this
            // runtime still owns its last already-presented publication.
            self.install_presented_snapshot(self.snapshot.clone(), self.source_clock);
        }
    }

    /// Install an authoritative hydrated publication without replacing reader
    /// interaction state. Archived conversations use the same presentation path.
    pub(super) fn install_snapshot(
        &mut self,
        mut snapshot: Arc<Snapshot>,
        source_clock: Option<chat::client::SourceClock>,
    ) {
        // Hidden publications number new children too, so a child keeps the
        // number it first appeared with for as long as it stays listed.
        self.child_ordinals.observe(&snapshot.subagents);
        if !self.presents() {
            self.release_transcript_presentation();
            self.settle_commands(&snapshot, false);
            Arc::make_mut(&mut snapshot).evict_transcript();
            self.snapshot = snapshot;
            self.source_clock = source_clock;
            return;
        }
        let snapshot = self.logical_prose.project(snapshot);
        self.install_presented_snapshot(snapshot, source_clock);
    }

    /// Rebuild disposable presentation from a publication that is already
    /// projected. In particular a frozen Resume source has no raw-part owner
    /// or cache-only read subscription left to recreate.
    fn install_presented_snapshot(
        &mut self,
        mut snapshot: Arc<Snapshot>,
        source_clock: Option<chat::client::SourceClock>,
    ) {
        self.scroll_memory.latest_source(
            self.page_request
                .filter(|(page, _)| *page == view::Page::Latest)
                .map(|(_, generation)| generation),
            snapshot.generation,
            snapshot.has_later,
            snapshot.history_loading,
        );
        if self.page_request.is_some_and(|(_, generation)| {
            snapshot.generation > generation && !snapshot.history_loading
        }) && self.commands.iter().all(|pending| pending.page.is_none())
        {
            self.page_request = None;
            self.earlier_asked = None;
            self.later_asked = None;
        }
        let selected_items = self
            .selected_subagent
            .as_ref()
            .and_then(|id| snapshot.subagents.iter().find(|agent| &agent.id == id))
            .map_or(snapshot.items.as_slice(), |agent| agent.items.as_slice());
        if !self.presentation_evicted
            && self.snapshot.generation == snapshot.generation
            && Arc::ptr_eq(&self.snapshot, &snapshot)
            && selected_items
                .iter()
                .all(|item| self.presentation_cache().contains_key(&item.id))
        {
            return;
        }
        if self.finished_stamp.as_ref().map(|stamp| stamp.at) != snapshot.finished_at {
            let outcome = view::turn_outcome(&snapshot);
            self.finished_stamp = snapshot.finished_at.and_then(|at| {
                let time = chat::local_time(i64::try_from(at / 1000).ok()?)?;
                let full = format!(
                    "{:04}-{:02}-{:02} {}",
                    time.year + 1900,
                    time.month + 1,
                    time.day,
                    time.clock()
                );
                Some(FinishedStamp {
                    at,
                    clock: time.clock(),
                    full,
                    outcome,
                })
            });
        }
        self.subagent_scroll
            .retain(|id, _| snapshot.subagents.iter().any(|agent| &agent.id == id));
        self.observed_scroll.borrow_mut().retain(|id, _| {
            id.as_ref()
                .is_none_or(|id| snapshot.subagents.iter().any(|agent| &agent.id == id))
        });
        self.selection_bookmarks.get_mut().retain(|id, _| {
            id.as_ref()
                .is_none_or(|id| snapshot.subagents.iter().any(|agent| &agent.id == id))
        });
        self.scroll_memory.retain_children(&snapshot.subagents);
        self.settle_recalls(&snapshot);
        if self.settle_commands(&snapshot, true) {
            self.prepare_local_prompt();
        }
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
        if !self.parent_scroll().at_bottom()
            && !snapshot.has_later
            && self.page_request.is_none()
            && !snapshot.history_loading
            && snapshot.items.last().is_some_and(|item| {
                let token = ChangeToken::of(&item.content);
                self.cache.get(&item.id).map_or_else(
                    || {
                        self.last_presented_tail
                            .as_ref()
                            .is_none_or(|(id, previous)| id != &item.id || *previous != token)
                    },
                    |cached| cached.token != token,
                )
            })
        {
            self.unseen_activity = true;
        }
        // Restored and paged conversations carry their own prompt history.
        // Preserve a recalled index while the reader is walking it.
        if self.history_cursor.is_none() {
            let mut prompt_count = 0;
            self.history_images.clear();
            let mut prompt: Option<(String, usize, bool)> = None;
            for item in &snapshot.items {
                match (&item.role, &item.content) {
                    (chat::Role::User, ItemContent::Text { text, .. }) => {
                        // A blank original user block still starts a fresh
                        // image-only prompt instead of borrowing older prose.
                        let index = prompt_count;
                        prompt_count += 1;
                        if let Some(previous) = self.history.get_mut(index) {
                            if previous != text {
                                previous.clone_from(text);
                            }
                        } else {
                            self.history.push(text.clone());
                        }
                        self.history_images.push(Vec::new());
                        let key =
                            attachment_draft::message_key(self.session.launch.provider, &item.id)
                                .unwrap_or(&item.id)
                                .to_owned();
                        prompt = Some((key, index, text.trim().is_empty()));
                    }
                    (
                        chat::Role::User,
                        ItemContent::Image {
                            path,
                            reference: Some(reference),
                            ..
                        },
                    ) => {
                        if reference.validate().is_ok() {
                            // Snapshot paths belong to the producing backend.
                            // Never synthesize a UI-local store path; replay
                            // resolves the immutable receipt on that backend.
                            let attachment = Attachment {
                                path: path.into(),
                                reference: Some(reference.clone()),
                            };
                            let key = attachment_draft::message_key(
                                self.session.launch.provider,
                                &item.id,
                            )
                            .unwrap_or(&item.id)
                            .to_owned();
                            if prompt
                                .as_ref()
                                .is_none_or(|(current, _, _)| current != &key)
                            {
                                let index = prompt_count;
                                prompt_count += 1;
                                if let Some(previous) = self.history.get_mut(index) {
                                    previous.clear();
                                } else {
                                    self.history.push(String::new());
                                }
                                self.history_images.push(Vec::new());
                                prompt = Some((key, index, true));
                            }
                            let (_, index, image_only) = prompt.as_ref().unwrap();
                            let (index, image_only) = (*index, *image_only);
                            if image_only {
                                if !self.history[index].is_empty() {
                                    self.history[index].push(' ');
                                }
                                self.history[index].push_str(&reference.token());
                            }
                            self.history_images[index].push(attachment);
                        }
                    }
                    (chat::Role::User, _) => {}
                    _ => prompt = None,
                }
            }
            self.history.truncate(prompt_count);
            // Preserve index alignment while omitting empty nonimage records.
            let pairs = std::mem::take(&mut self.history)
                .into_iter()
                .zip(std::mem::take(&mut self.history_images))
                .filter(|(text, images)| !text.trim().is_empty() || !images.is_empty())
                .collect::<Vec<_>>();
            (self.history, self.history_images) = pairs.into_iter().unzip();
        }
        if self
            .selected_subagent
            .as_ref()
            .is_some_and(|id| !snapshot.subagents.iter().any(|agent| &agent.id == id))
        {
            self.restore_find_disclosures();
            self.selected_subagent = None;
            // The selected identity disappeared even when parent IDs did not
            // change. Its separate prepared owners cannot survive fallback.
            self.child_cache.clear();
            self.child_cache_context = None;
            self.find_matches.clear();
            self.find_index = 0;
        }
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| snapshot.subagents.iter().find(|agent| &agent.id == id));
        let previous_selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| self.snapshot.subagents.iter().find(|agent| &agent.id == id));
        let same_ids = |old: &[Arc<chat::Item>], new: &[Arc<chat::Item>]| {
            old.len() == new.len() && old.iter().zip(new).all(|(old, new)| old.id == new.id)
        };
        if !same_ids(&self.snapshot.items, &snapshot.items)
            || !same_ids(
                previous_selected.map_or(&[], |agent| agent.items.as_slice()),
                selected.map_or(&[], |agent| agent.items.as_slice()),
            )
        {
            self.cache
                .retain(|id, _| snapshot.items.iter().any(|item| &item.id == id));
            self.child_cache.retain(|id, _| {
                selected.is_some_and(|agent| agent.items.iter().any(|item| &item.id == id))
            });
            self.presentation_revision = self.presentation_revision.wrapping_add(1);
            self.find_navigation.changed_document();
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
            self.restore_find_disclosures();
            self.selected_subagent = None;
            self.find_matches.clear();
            self.find_index = 0;
        }
        let child_ids: HashSet<&str> = snapshot
            .subagents
            .iter()
            .map(|agent| agent.id.as_str())
            .collect();
        self.child_expanded
            .retain(|id, _| child_ids.contains(id.as_str()));
        self.child_unfolded
            .retain(|id, _| child_ids.contains(id.as_str()));
        // A child's identity can outlive many bounded transcript windows.
        // Share the history budget across children instead of retaining every
        // previously expanded item for each long-running child indefinitely.
        let child_disclosures: usize = self
            .child_expanded
            .values()
            .map(|choices| choices.len())
            .chain(self.child_unfolded.values().map(HashSet::len))
            .sum();
        if child_disclosures > 4096 {
            for child in &snapshot.subagents {
                let live: HashSet<_> = child.items.iter().map(|item| item.id.as_str()).collect();
                if let Some(expanded) = self.child_expanded.get_mut(&child.id)
                    && expanded.iter().any(|id| !live.contains(id.as_str()))
                {
                    expanded.edit().retain(|id| live.contains(id.as_str()));
                }
                if let Some(unfolded) = self.child_unfolded.get_mut(&child.id) {
                    // The final separator belongs to the part name; opaque
                    // item IDs may themselves contain '#'.
                    unfolded.retain(|key| {
                        key.rsplit_once('#')
                            .is_some_and(|(id, _)| live.contains(id))
                    });
                }
            }
            self.child_expanded.retain(|_, choices| !choices.is_empty());
            self.child_unfolded.retain(|_, choices| !choices.is_empty());
            let empty = HashSet::new();
            let no_turns = HashMap::new();
            for (child, disclosure) in &mut self.child_find_disclosure {
                disclosure.retain(
                    self.child_expanded.get(child).map_or(&empty, |set| &**set),
                    self.child_unfolded.get(child).unwrap_or(&empty),
                    &no_turns,
                );
            }
        }
        // Keep disclosure choices across page eviction, with a bounded budget.
        // Question anchors only describe the currently loaded window.
        let parent_items_pruned = self.expanded.len() > 4096;
        if parent_items_pruned
            && self
                .expanded
                .iter()
                .any(|id| !live_ids.contains(id.as_str()))
        {
            self.expanded
                .edit()
                .retain(|id| live_ids.contains(id.as_str()));
        }
        self.asked_after.retain(|id| live_ids.contains(id.as_str()));
        let retired_metadata = self.prepare_transcript_cache(&snapshot);
        if self.selected_subagent.is_some() {
            // Parent payload preparation stays lazy while a child is selected,
            // but accepted recorded-time metadata also belongs to retained
            // parent footers. Updating a stamp never examines the body.
            let today = chat::now_ms().and_then(|now| chat::local_time((now / 1000) as i64));
            for item in &snapshot.items {
                if let Some(cached) = self.cache.get_mut(&item.id)
                    && cached.stamp.as_ref().map(|stamp| stamp.0) != item.at
                {
                    cached.stamp = item
                        .at
                        .and_then(|at| recorded_message_stamp(at, today.as_ref()));
                }
            }
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
        let mut grouping_dirty = false;
        for request in &snapshot.requests {
            if self.asked.insert(request.id.clone())
                && let Some(last) = snapshot.items.last()
            {
                grouping_dirty |= self.asked_after.insert(last.id.clone());
            }
        }
        self.source_clock = source_clock;
        let turns = (grouping_dirty
            || !turn::same_structure(&self.snapshot.items, &snapshot.items))
        .then(|| self.transcript_turns(&snapshot));
        if let Some(turns) = &turns {
            carry_step_disclosures(
                &mut self.opened,
                &mut self.find_disclosure,
                &self.turns,
                &self.snapshot.items,
                turns,
                &snapshot.items,
            );
        }
        let current_anchor = self.notice_anchor(&self.snapshot, &self.turns);
        let next_anchor = self.notice_anchor(&snapshot, turns.as_deref().unwrap_or(&self.turns));
        if self
            .snapshot
            .error
            .as_deref()
            .is_some_and(|error| chat::requires_sign_in(self.session.launch.provider, error))
            && snapshot.error.is_none()
            && !self.snapshot.busy
            && !snapshot.busy
            && snapshot.ready
            && !snapshot.exited
            && snapshot.turn_generation == self.snapshot.turn_generation
            && snapshot.session_id == self.snapshot.session_id
            && failure::same_prompt(self.session.launch.provider, current_anchor, next_anchor)
        {
            let provider = self.session.launch.provider;
            self.failures.retain(|note| {
                !failure::same_prompt(provider, note.message.as_deref(), current_anchor)
                    || !chat::requires_sign_in(provider, &note.text)
            });
        }
        let next_anchor = next_anchor.map(str::to_owned);
        let next_after = failure::closing_item(&snapshot, turns.as_deref().unwrap_or(&self.turns))
            .map(str::to_owned);
        let parent_turns_pruned = self.opened.len() > 4096;
        if let Some(turns) = turns {
            self.turns = turns.into();
            reconcile_disclosure(&mut self.opened, &self.turns);
        }
        if parent_items_pruned || parent_turns_pruned {
            self.find_disclosure
                .retain(&self.expanded, &self.unfolded, &self.opened);
        }
        // Presentation refreshes below, once the snapshot is installed.
        if snapshot.error != self.snapshot.error
            && let Some(error) = &snapshot.error
        {
            if snapshot.has_later {
                self.defer_note(error.clone());
            } else {
                self.note_at(next_anchor.clone(), next_after.clone(), error.clone());
            }
        }
        if !snapshot.has_later {
            for notice in std::mem::take(&mut self.unplaced_failures) {
                self.note_at(next_anchor.clone(), next_after.clone(), notice);
            }
        }
        // A cancel armed for one turn must not carry over to the next.
        if !snapshot.busy || snapshot.turn_generation != self.snapshot.turn_generation {
            self.interrupt_armed = None;
        }
        self.last_presented_tail = snapshot
            .items
            .last()
            .map(|item| (item.id.clone(), ChangeToken::of(&item.content)));
        self.snapshot = snapshot;
        drop(retired_metadata);
        self.presentation_evicted = false;
        tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_runtime", chat_id = %self.session.id, resident_bytes = self.snapshot.transcript_bytes(), cache_entries = self.cache.len(), exposed_presentation_bytes = self.cache.values().map(CachedItem::retained_bytes).sum::<usize>());
        self.refresh_failure_ui();
        images::sync(self);
        self.refresh_find();
    }

    /// How many running subagents cancelling the turn would also stop.
    pub(super) fn interrupt_casualties(&self) -> usize {
        self.snapshot
            .subagents
            .iter()
            .filter(|agent| agent.stopped_by_interrupt(&self.snapshot.capabilities))
            .count()
    }

    fn restore_find_disclosures(&mut self) {
        self.restore_failure_find();
        self.find_disclosure
            .restore(&mut self.expanded, &mut self.unfolded, &mut self.opened);
        // Restoring reads a child's disclosures before it writes them; an
        // absent child has nothing to restore, so give it no entry.
        let mut no_items = Revised::default();
        for (child, mut disclosure) in self.child_find_disclosure.drain() {
            disclosure.restore(
                self.child_expanded.get_mut(&child).unwrap_or(&mut no_items),
                self.child_unfolded.entry(child).or_default(),
                &mut Revised::default(),
            );
        }
        // A temporary Find reveal owns the report's parsed body too. Drop it
        // again when search restores a collapsed report; manual disclosures
        // retain both their state and their prepared body.
        fn collapse_reports(
            cache: &mut HashMap<String, CachedItem>,
            expanded: &HashSet<String>,
            theme: crate::settings::EditorSyntaxTheme,
        ) {
            let closed: Vec<_> = cache
                .iter()
                .filter_map(|(id, cached)| {
                    (cached.report_expanded == Some(true) && !expanded.contains(id))
                        .then(|| cached.item.as_ref().map(|item| (id.clone(), item.clone())))
                        .flatten()
                })
                .collect();
            for (id, item) in closed {
                let previous = cache.remove(&id);
                let mut cached =
                    CachedItem::updated(&item.content, theme, previous, false, false, None);
                cached.item = Some(item);
                cache.insert(id, cached);
            }
        }
        collapse_reports(&mut self.cache, &self.expanded, self.syntax_theme);
        if let Some(child) = &self.selected_subagent {
            collapse_reports(
                &mut self.child_cache,
                self.child_expanded
                    .get(child)
                    .map_or(&HashSet::new(), |set| &**set),
                self.syntax_theme,
            );
        }
    }

    fn find_anchor(&self, key: &str) -> iced::advanced::widget::Id {
        iced::advanced::widget::Id::new(match &self.selected_subagent {
            // Length framing keeps opaque child/item IDs collision-free.
            Some(child) => format!(
                "chat-child-item-{}-{}:{child}{key}",
                self.target.card,
                child.len()
            ),
            None => format!("chat-turn-{}-{key}", self.target.card),
        })
    }

    fn reveal_child_find_match(&mut self, key: &str) {
        self.reveal_failure_for_find(key);
        self.apply_find_plan(key);
    }

    fn refresh_find(&mut self) {
        self.refresh_failure_previews();
        self.refresh_find_work();
    }
    pub(super) fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }

    pub(super) fn set_visible(&mut self, visible: bool) -> Result<(), String> {
        if !visible {
            self.find_navigation.cancel_all();
            self.preparation.clear_local();
            self.forget_local_presentations();
        }
        let changed = self.visible.swap(visible, Ordering::Relaxed) != visible;
        let rewarmed = visible && self.warm.take().is_some();
        if !visible && changed && self.warm.is_none() && self.keeps_warm() {
            self.warm = Some(warm::stamp());
        }
        images::sync(self);
        let result = self.present();
        if visible {
            if changed {
                self.refresh();
                if self.presentation_evicted
                    && (self.snapshot.error.is_some() || self.error.is_some())
                {
                    // A disconnected cold chat may retain only current error
                    // metadata and will never send a hydration wake.
                    self.refresh_failure_ui();
                }
                self.observe_visible_limit();
            }
            if rewarmed {
                // Work that waited while hidden: items still loading when the
                // chat hid, and a syntax theme changed meanwhile. Everything
                // else is already prepared, so this only revisits the window.
                let snapshot = self.snapshot.clone();
                drop(self.prepare_transcript_cache(&snapshot));
            }
        } else if result.is_err() {
            // The client could not be told to keep the body, so this chat is
            // not warm whatever it was before.
            self.warm = None;
            self.release_hidden(changed, false);
        } else if self.warm.is_none() {
            self.release_hidden(changed, true);
        }
        result
    }

    /// Whether this chat's presentation stays in place while it is hidden.
    fn presents(&self) -> bool {
        self.visible.load(Ordering::Relaxed) || self.warm.is_some()
    }

    /// Whether hiding this chat may keep its disposable presentation. A
    /// frozen source owns its one projected window; other chats also need
    /// their client to retain the body they can refresh.
    fn keeps_warm(&self) -> bool {
        !self.presentation_evicted
            && (self.retains_source_body()
                || self.handle().is_some_and(|handle| {
                    handle.is_connected() && handle.resident_snapshot_with_clock().is_some()
                }))
            && self.warm_bytes() <= warm::CHAT_BYTES
    }

    /// The heap this chat holds while warm: its transcript, and what was
    /// prepared from it, with parsed documents estimated from their size
    /// (see [`warm::NODE_BYTES`]). Constant work per item.
    pub(super) fn warm_bytes(&self) -> usize {
        let prepared = |cached: &CachedItem| {
            let [text, nodes, _] = cached
                .document
                .as_ref()
                .map_or([0; 3], prose_document::Document::census);
            cached.retained_bytes() + text + nodes * warm::NODE_BYTES + warm::ITEM_BYTES
        };
        self.snapshot.transcript_bytes()
            + self
                .cache
                .values()
                .chain(self.child_cache.values())
                .map(prepared)
                .sum::<usize>()
    }

    /// The order this chat was hidden in, while it is warm.
    pub(super) fn warm_stamp(&self) -> Option<u64> {
        self.warm
    }

    /// Release a warm hidden chat's disposable presentation. A frozen source
    /// keeps its one projected window after cooling; its actor cannot reload it.
    /// A visible or cold chat is unchanged.
    pub(super) fn cool(&mut self) {
        if self.warm.take().is_none() || self.visible.load(Ordering::Relaxed) {
            return;
        }
        let told = self.present();
        self.release_hidden(true, told.is_ok());
        if let Err(error) = told {
            self.error = Some(error);
        }
    }

    /// Shed a cold hidden chat's presentation: all of it when the daemon can
    /// hydrate it again, or only its render caches when it cannot, including
    /// when the client was not `told` it is hidden.
    fn release_hidden(&mut self, changed: bool, told: bool) {
        if self.retains_source_body() {
            // Keep the sole projected publication, not amplification or old
            // read subscriptions. A connected socket need not own its actor.
            self.release_transcript_caches();
            self.logical_prose.release();
            if self.history_cursor.is_none() {
                self.history = Vec::new();
                self.history_images = Vec::new();
            }
            tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_runtime", chat_id = %self.session.id,
                resident_bytes = self.snapshot.transcript_bytes(), cache_entries = 0usize,
                exposed_presentation_bytes = 0usize);
            return;
        }
        if told
            && self.handle().is_some_and(|handle| {
                // Notice suppression is not permission to discard the last
                // body: a lost client may already have installed metadata.
                // A live daemon can hydrate again; a disconnected resident
                // publication is also sufficient to restore this presentation.
                handle.is_connected() || handle.resident_snapshot_with_clock().is_some()
            })
            && (changed
                || !self.cache.is_empty()
                || !self.child_cache.is_empty()
                || !self.snapshot.items.is_empty()
                || self
                    .snapshot
                    .subagents
                    .iter()
                    .any(|child| !child.items.is_empty()))
        {
            self.release_transcript_presentation();
        } else if self.handle().is_some()
            && (!self.cache.is_empty()
                || !self.child_cache.is_empty()
                || self.selection_document.borrow().is_some())
        {
            // An unreachable daemon cannot restore its body yet. Keep that
            // last readable publication, but shed render-ready amplification.
            self.release_transcript_caches();
            tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_runtime", chat_id = %self.session.id,
                resident_bytes = self.snapshot.transcript_bytes(), cache_entries = 0usize,
                exposed_presentation_bytes = 0usize);
        }
    }
    fn release_transcript_caches(&mut self) {
        // Diagnostic metadata retains exact source Arcs. Release these while
        // the snapshot still owns its items, keeping only small disclosure
        // choices for the authoritative rehydration.
        self.failure_ui.release_presentation();
        self.provider_queue.borrow_mut().clear();
        self.forget_local_presentations();
        self.preparation.clear();
        self.cache = HashMap::new();
        self.child_cache = HashMap::new();
        self.child_cache_context = None;
        self.selection_document.get_mut().take();
        self.presentation_revision = self.presentation_revision.wrapping_add(1);
        self.find_navigation.changed_document();
        self.presentation_evicted = true;
    }

    /// Evict disposable body/presentation owners while keeping the small
    /// interaction state needed by either a live or archived rehydration.
    pub(super) fn release_transcript_presentation(&mut self) {
        let bytes = self.snapshot.transcript_bytes();
        let cache_entries = self.cache.len();
        self.release_transcript_caches();
        self.logical_prose.release();
        Arc::make_mut(&mut self.snapshot).evict_transcript();
        if self.history_cursor.is_none() {
            self.history = Vec::new();
            self.history_images = Vec::new();
        }
        tracing::trace!(target: "cagents::memory", process_id = std::process::id(), owner = "chat_runtime", chat_id = %self.session.id,
            resident_bytes = 0usize, cache_entries = 0usize, evicted_bytes = bytes,
            evicted_cache_entries = cache_entries, exposed_presentation_bytes = 0usize);
    }

    /// Resume can retire the old actor before its replacement answers. Keep
    /// the last source body until an actual attachment replaces it, including
    /// when that start fails while the old IPC stream remains open.
    fn retains_source_body(&self) -> bool {
        self.resume_source_pinned
    }

    fn freeze_resume_source(&mut self) {
        self.resume_source_pinned = true;
        // The frozen snapshot is already the visible logical projection.
        // Raw parts, complete-read budgets and their subscriptions no longer
        // belong to an actor Resume can retire before its reply arrives.
        self.logical_prose.release();
        if self.history_cursor.is_none() {
            self.history = Vec::new();
            self.history_images = Vec::new();
        }
        // Dropping the old client's interest also drops its resident body
        // synchronously. A concurrent disconnect is handled by Resume itself.
        let _ = self.present();
    }

    /// Tells the connection whether to keep the card's snapshot current.
    ///
    /// A first message waiting to be sent keeps it current while the card is
    /// hidden, since its provider becoming ready, and the models it lists,
    /// reach the card only through that snapshot.
    ///
    /// A frozen source has already taken readability from the connection and
    /// releases its raw body even if the reader died before its UI wake. Ordinary
    /// closed cards keep the client's final body as their cold readable fallback.
    ///
    /// A warm hidden chat keeps its body without reading further: what it
    /// missed arrives as one change, or nothing, when it is shown again.
    fn present(&self) -> Result<(), String> {
        let frozen = self.retains_source_body();
        let retain_body = !frozen && self.presents();
        let interested = !frozen
            && (self.visible.load(Ordering::Relaxed)
                || self.queued.is_some()
                || !self.followups.is_empty());
        match self.handle() {
            Some(handle) if frozen || handle.is_connected() => {
                let result = if frozen {
                    handle.hand_off_source_body()
                } else {
                    handle.set_interest(interested, retain_body)
                };
                match result {
                    // The connection can drop between the check and the call, and
                    // is then as lost as one found lost: not this pass's to report.
                    Err(_) if !handle.is_connected() => Ok(()),
                    result => result,
                }
            }
            _ => Ok(()),
        }
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
        if self.child_picker.is_some() {
            "chat-subagent-search"
        } else if self.model_picker.is_some() {
            "chat-model-search"
        } else {
            "chat-composer"
        }
    }

    /// Whether a menu or picker of this card is open over the workspace.
    /// Resting the pointer on another pane then leaves focus where it is,
    /// rather than moving the keyboard away from the menu still showing.
    pub(super) fn has_open_menu(&self) -> bool {
        self.child_picker.is_some()
            || self.model_picker.is_some()
            || self.permission_picker
            || self.account_picker
            || self.composer_actions
            || self.branch_picker
    }

    /// The source this runtime can identify, including provider identity
    /// learned after its durable descriptor was installed.
    fn return_session(&self) -> chat::Session {
        let mut session = self.session.clone();
        if let Some(handle) = self.handle() {
            let observed = handle.session();
            if let Some(id) = &observed.launch.session_id {
                session.launch.session_id = Some(id.clone());
            }
            session.learn_native_continuity_from(&observed);
        }
        session
    }

    /// Explicit Fresh is available after a permanent refusal or when the
    /// provider ended before loading the requested conversation. Its recipe
    /// must independently support a fresh start; retirement is checked later.
    fn offers_fresh_conversation(&self) -> bool {
        let source = self.return_session();
        source.launch.session_id.is_some()
            && self.start_blocked.map_or(
                self.snapshot.exited && self.snapshot.session_id != source.launch.session_id,
                blocked_recovery::Reopen::offers_fresh,
            )
            && source.fresh_start().is_some()
    }
    /// Whether this card's provider stopped while the daemon that ran it still
    /// holds its ended actor, so a new provider can resume the same
    /// conversation in this card. Only a retained Claude actor proves its end
    /// that way; a saved-history reader resumes through its own action.
    pub(super) fn resumes_in_place(&self) -> bool {
        self.connected()
            && self.snapshot.exited
            && self.session.launch.provider == crate::agent::Provider::Claude
            && matches!(
                self.session.native_continuity,
                chat::NativeContinuity::RetainedClaudeActor { .. }
            )
            && self.session.proof.is_some()
            && !self.has_connected_saved_history(&self.snapshot)
    }
    /// Whether this runtime still has a daemon connection to its own actor.
    fn connected(&self) -> bool {
        self.handle().is_some() && !self.disconnected
    }
    /// Whether a message of this window's has yet to reach the provider, so
    /// that ending the conversation now would lose it.
    fn message_on_its_way(&self) -> bool {
        self.queued.is_some()
            || !self.followups.is_empty()
            || self.commands.unsent().next().is_some()
            || self.followups_sending()
    }
    /// Ownership can become durable after the initial attachment, including
    /// while the pane is hidden. Preserve that confirmed capability before
    /// any reconnect or saved-descriptor update.
    fn refresh_session(&mut self) {
        let Some(handle) = self.handle() else { return };
        let observed = handle.session();
        if let Some(id) = &observed.launch.session_id {
            self.session.launch.session_id = Some(id.clone());
        }
        self.session.learn_native_continuity_from(&observed);
    }
    /// Why this chat cannot be swapped for its terminal view right now.
    ///
    /// An unsent draft is deliberately absent: `Cagents::chat_forget` stashes
    /// it and a returning runtime restores it, so typing a single character no
    /// longer makes the card impossible to leave. Attached files and managed
    /// image receipts are stashed alongside it. An image with a malformed
    /// receipt and half-entered request answers still need this runtime.
    pub(super) fn transition_blocker(&self) -> Option<&'static str> {
        if self.attachment_chooser_pending.is_some() {
            Some("Finish or cancel the file chooser before leaving the chat.")
        } else if self.pastes_pending != 0 {
            Some("Wait for the image attachment to finish before leaving the chat.")
        } else if !self.commands.is_idle() {
            Some("Wait for the pending chat command to finish before leaving the chat.")
        } else if self.queued.is_some() {
            Some(QUEUED_BLOCKER)
        } else if !self.followups.is_empty() {
            Some("Send or take back the queued follow-up before leaving the chat.")
        } else if self
            .attachments
            .iter()
            .chain(self.unsent_attachments())
            .any(|attachment| {
                attachment
                    .reference
                    .as_ref()
                    .is_some_and(|reference| reference.validate().is_err())
            })
        {
            Some("Send or remove the attached images before leaving the chat.")
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
    /// Composer text, attached files and managed image receipts are included
    /// in the workspace snapshot flushed before an update. An image whose
    /// receipt is malformed, answers and in-flight work still need a live UI.
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
        Provider::Antigravity => "Antigravity chat",
        Provider::Muse => "Muse chat",
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
    if !provider.has_native_chat() {
        return Err(provider.native_chat_refusal());
    }
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
        terminal: Some(Box::new(session.clone())),
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
        return Ok(TerminalSession::clone(source));
    }
    let recipe = session
        .launch
        .terminal_recipe()
        .ok_or_else(|| "Cannot form a safe exact-session terminal resume command.".to_owned())?;
    let id = TerminalId::new();
    let mut terminal = session
        .terminal
        .as_deref()
        .cloned()
        .unwrap_or_else(|| TerminalSession {
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
    end_conversation(handle, session, super::channel_ui::close_sessions)
}

/// Gives up a failed start the card replaced on its own, like
/// [`retire_conversation`], but never retires an open agent that runs as it:
/// the card's agent check can predate the agents the daemon reports.
fn give_up_failed_start(
    handle: Option<Arc<ClientHandle>>,
    session: Option<chat::Session>,
) -> Result<(), String> {
    end_conversation(
        handle,
        session,
        super::channel_ui::close_sessions_sparing_agents,
    )
}

/// Closes a conversation the reader closed, like [`retire_conversation`],
/// and also retires the agent still bound to it after a daemon stop. Giving
/// up a failed start must not do that, so only an explicit Close comes here.
///
/// A reopened conversation that was given no work yet leaves its agent
/// stopped when its process closes, as a reopening its card gave up would; so
/// a live conversation's agent is retired by its session once it has closed.
fn close_conversation(
    handle: Option<Arc<ClientHandle>>,
    session: Option<chat::Session>,
) -> Result<(), String> {
    let live = handle.is_some();
    end_conversation(
        handle,
        session.clone(),
        super::channel_ui::close_sessions_retiring_agents,
    )?;
    match session {
        Some(session) if live => {
            super::channel_ui::close_sessions_retiring_agents(vec![CardKind::Chat(session)])
        }
        _ => Ok(()),
    }
}

/// Fresh must retire the captured source, including its retained owner, without
/// following a newer conversation that happens to reuse the same native ID.
fn close_fresh_conversation(
    _handle: Option<Arc<ClientHandle>>,
    source: Option<chat::Session>,
) -> Result<(), String> {
    // Even abandoned Fresh cleanup must not follow an ID-only live handle.
    let source = source
        .ok_or_else(|| "The hosted chat does not match the exact saved conversation".to_owned())?;
    super::channel_ui::close_captured_chat(source)
}

fn end_conversation(
    handle: Option<Arc<ClientHandle>>,
    session: Option<chat::Session>,
    close: fn(Vec<CardKind>) -> Result<(), String>,
) -> Result<(), String> {
    let refused = match handle.map(|handle| handle.close()) {
        Some(Ok(())) => return Ok(()),
        Some(Err(error)) => Some(error),
        None => None,
    };
    let Some(session) = session else {
        return refused.map_or(Ok(()), Err);
    };
    // The daemon usually refuses both the same way; say it once.
    close(vec![CardKind::Chat(session)]).map_err(|error| match refused {
        Some(refused) if refused != error => format!("{refused}; {error}"),
        _ => error,
    })
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
                // An ACP agent's own modes, by what they mean, until its
                // session lists the ones it offers.
                Provider::Antigravity | Provider::Muse => provider
                    .acp()
                    .into_iter()
                    .flat_map(|acp| acp.mode_meanings())
                    .map(str::to_owned)
                    .collect(),
                Provider::Omp => Vec::new(),
            },
            skill_reach: match provider {
                Provider::Codex => chat::SkillReach::Anywhere,
                // ACP agents expand a command the prompt opens with.
                Provider::Claude | Provider::Antigravity | Provider::Muse => {
                    chat::SkillReach::Leading
                }
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
        Provider::Antigravity | Provider::Muse => chat::acp_permission_mode(recipe, provider),
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
    if chat::apply_acp_permission_mode(recipe, provider, mode) {
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

/// What a chat's pull request labels were built from: the generations of
/// the window's pull request and worktree indexes, the conversation, and
/// whether the inspector held a current list of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LabelSources {
    board: u64,
    worktrees: u64,
    conversation: TerminalId,
    watched: bool,
}

impl Cagents {
    /// Keeps every chat's pull request labels current with what the window
    /// holds. Run after each update: unless the projects' pull requests, their
    /// repositories or the worktree rows changed, it compares a revision per
    /// project and a stamp per chat and does nothing else.
    pub(super) fn sync_pull_request_labels(&mut self) {
        let github = &self.github_availability;
        self.pull_request_board
            .refresh(&self.sidebar_model.projects, |project| {
                github
                    .get(&project)
                    .and_then(GitHubAvailability::repository_key)
            });
        let (board, worktrees) = (
            self.pull_request_board.generation(),
            self.worktree_paths.generation(),
        );
        // A conversation counts as watched once the shown inspector holds a
        // list taken since it last turned to it, not merely while it is shown:
        // until then what a chat took earlier may no longer be true.
        let watched = self.listed_watched_conversations();
        let stale = self
            .cards
            .iter()
            .filter_map(|(card, runtime)| match runtime {
                CardRuntime::Chat(runtime) => {
                    let conversation = runtime.session.id;
                    let built_from = LabelSources {
                        board,
                        worktrees,
                        conversation,
                        watched: watched.contains(&conversation),
                    };
                    (runtime.pull_request_labels_built_from != Some(built_from))
                        .then_some((*card, built_from))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for (card, built_from) in stale {
            self.relabel_chat(card, built_from);
        }
    }

    /// Builds one chat's labels from the window's index and what the
    /// inspector listed for its conversation.
    fn relabel_chat(&mut self, card: CardId, built_from: LabelSources) {
        // The project whose repository the chat works in, whatever channel it
        // is filed under: the sidebar's channel filing is not where it works.
        let project = self.card_state(card).and_then(|state| {
            let directory = self.unchanneled_card_worktree_indexed(state, &&self.worktree_paths)?;
            self.worktree_paths.project_of(directory)
        });
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return;
        };
        // What was listed for a conversation the chat has since left says
        // nothing of the one it is in.
        if runtime
            .pull_request_inspected
            .conversation()
            .is_some_and(|listed| listed != runtime.session.id)
        {
            runtime.pull_request_inspected = pull_request_links::Inspected::default();
        }
        // A list the inspector took before this runtime existed, or while it
        // was parked, is still the latest for its conversation.
        let conversation = runtime.session.id;
        if let Some((revision, list)) = self.pull_request_inspector.conversation_list(conversation)
        {
            runtime
                .pull_request_inspected
                .accept(conversation, revision, true, list);
        }
        let labels = self.pull_request_board.labels(
            project,
            &runtime.pull_request_inspected,
            built_from.watched,
        );
        runtime.pull_request_labels_built_from = Some(built_from);
        runtime.set_pull_request_labels(Arc::new(labels));
    }

    /// Labels the inspected conversation's chats from the list the inspector
    /// was just sent, so they never watch the conversation themselves.
    pub(super) fn label_chat_pull_request_links(
        &mut self,
        watch: &crate::app::pull_request_ui::ConversationWatch,
    ) {
        let crate::app::pull_request_ui::ConversationWatch::Listed {
            conversation,
            revision,
            list,
            first,
        } = watch
        else {
            return;
        };
        // A list just sent is current, though the inspector takes it only
        // after this.
        let built_from = LabelSources {
            board: self.pull_request_board.generation(),
            worktrees: self.worktree_paths.generation(),
            conversation: *conversation,
            watched: self.inspector_watches(*conversation),
        };
        let changed = self
            .cards
            .iter_mut()
            .filter_map(|(card, runtime)| match runtime {
                CardRuntime::Chat(runtime) if runtime.session.id == *conversation => runtime
                    .pull_request_inspected
                    .accept(*conversation, *revision, *first, list)
                    .then_some(*card),
                _ => None,
            })
            .collect::<Vec<_>>();
        for card in changed {
            self.relabel_chat(card, built_from);
        }
    }
}

impl Cagents {
    /// Whether the focused chat has a menu or picker open; see
    /// [`Runtime::has_open_menu`]. Only the focused pane counts: a menu is
    /// opened from the pane that has focus, and one left flagged open in a
    /// pane no longer shown, a hidden channel's for example, is not drawn
    /// and must not hold the pointer's focus everywhere.
    pub(super) fn chat_menu_open(&self) -> bool {
        self.workspace
            .focused()
            .and_then(|card| self.cards.get(&card))
            .is_some_and(
                |runtime| matches!(runtime, CardRuntime::Chat(runtime) if runtime.has_open_menu()),
            )
    }

    /// Whether `card` is a chat whose message input, or the picker search
    /// standing in for it, is in the widget tree: it has a runtime that is
    /// neither starting, changing session nor waiting for admission.
    pub(super) fn chat_input_drawn(&self, card: CardId) -> bool {
        matches!(
            self.cards.get(&card),
            Some(CardRuntime::Chat(runtime)) if !runtime.awaiting_admission()
        ) && !self.chat_ui.phases.contains_key(&card)
    }

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
        if self.chat_start_pending(card) || self.pane_content_inputs_blocked(card) {
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
        let permission_controls = runtime.permission_controls(
            !runtime.disconnected && !runtime.snapshot.exited && runtime.queued.is_none(),
        );
        let focus = match &action {
            Action::AccountPicker(_, true) => Some(card_control_id(card, "chat-account-choice-0")),
            Action::PermissionPicker(_, true) => runtime
                .snapshot
                .permission_mode
                .as_ref()
                .map(|mode| card_control_id(card, &format!("chat-permission-mode-{mode}")))
                .filter(|id| permission_controls.contains(id))
                .or_else(|| permission_controls.first().cloned()),
            _ => None,
        };
        self.device_ui.menu_open = false;
        let task = self.handle_chat_ui(action);
        if let Some(runtime) = self.chat_runtime_mut(target) {
            runtime.picker_keyboard = focus.is_some();
            runtime.picker_focus = focus
                .as_ref()
                .and_then(|id| permission_controls.iter().position(|control| control == id))
                .filter(|_| command == GlobalShortcut::ChatPermissions)
                .unwrap_or(0);
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
            .filter(|card| !self.pane_content_inputs_blocked(*card))
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
        if self.pane_content_inputs_blocked(card) {
            return None;
        }
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
            runtime.permission_controls(
                !self.chat_start_pending(card)
                    && !runtime.disconnected
                    && !runtime.snapshot.exited
                    && runtime.queued.is_none(),
            )
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
        let task = if self.chat_runtime_mut(target)?.permission_picker {
            iced::advanced::widget::operate(iced::advanced::widget::operation::scoped(
                card_control_id(card, "chat-permissions-surface"),
                iced::advanced::widget::operation::focusable::focus(ring[next].clone()),
            ))
        } else {
            iced::widget::operation::focus(ring[next].clone())
        };
        Some(task.map(cosmic::Action::App))
    }

    pub(super) fn open_chat_find(&mut self) -> Option<Task<Message>> {
        let card = self.workspace.focused()?;
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return None;
        };
        runtime.find.get_or_insert_with(String::new);
        // Find shows its matches in the transcript the viewer covers.
        runtime.image_viewer = None;
        Some(
            iced::widget::operation::focus(card_control_id(card, "chat-find"))
                .map(cosmic::Action::App),
        )
    }

    fn chat_find_next(&mut self, target: Target, forward: bool) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        runtime.refresh_find();
        if runtime.find_matches.is_empty() {
            if runtime.find.as_ref().is_some_and(|query| !query.is_empty()) {
                return self.begin_find_navigation(target, None);
            }
            runtime.find_navigation.cancel();
            return Task::none();
        }
        runtime.find_index = if forward {
            (runtime.find_index + 1) % runtime.find_matches.len()
        } else {
            (runtime.find_index + runtime.find_matches.len() - 1) % runtime.find_matches.len()
        };
        let key = runtime.find_matches[runtime.find_index].clone();
        runtime.reveal_child_find_match(&key);
        self.begin_find_navigation(target, Some(key))
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
        // A first Send is saving this draft's identity; its receipt decides
        // what storage holds, so the draft's source is fixed until it answers.
        if self.chat_start_pending(target.card) {
            return Task::none();
        }
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
        // Re-picking the current choice names the same refused launch.
        let changed =
            runtime.session.launch.provider != provider || runtime.snapshot.selected_model != model;
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
        if changed {
            // The refusal answered the previous launch; this one was never tried.
            runtime.release_start_block();
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
        if self.chat_start_pending(target.card) {
            return Task::none();
        }
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
        runtime.release_start_block();
        Arc::make_mut(&mut runtime.snapshot).skills.clear();
        Arc::make_mut(&mut runtime.snapshot).permission_mode =
            draft_permission_mode(&session.launch.recipe, account.provider);
        runtime.completion.restart(true);
        self.replace_chat_conversation(target.card, session);
        self.load_draft_skills(target)
    }

    /// The managed account `session` was recorded on, when the account list
    /// lost it but its home may still be on the device, and that home.
    fn restorable_chat_account(
        &self,
        session: &chat::Session,
    ) -> Option<(Provider, uuid::Uuid, String)> {
        let home = chat_account_home(session)?;
        self.account_ui
            .restorable_home(Path::new(home))
            .filter(|(provider, _)| *provider == session.launch.provider)
            .map(|(provider, key)| (provider, key, home.to_owned()))
    }

    /// Offers to restore `own`'s account after the daemon refused `card`
    /// because its account list does not name it, and asks for that at once.
    ///
    /// Restoring is what the daemon does by itself for such a home, so the
    /// refusal asks for it rather than waiting on a click or the daemon's next
    /// look. Unless `again`, a later refusal for the same account keeps its
    /// reason on screen and leaves retrying, or another account, to the
    /// reader. A draft's message, back in its composer, is remembered to be
    /// sent once the account is restored.
    fn offer_chat_account_restore(
        &mut self,
        card: CardId,
        own: &chat::Session,
        again: bool,
    ) -> Task<Message> {
        let Some((provider, key, home)) = self.restorable_chat_account(own) else {
            self.chat_ui.restorable_accounts.remove(&card);
            return Task::none();
        };
        let resend = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Resend::of(runtime),
            _ => None,
        };
        let offered = self
            .chat_ui
            .restorable_accounts
            .get(&card)
            .filter(|offered| (offered.provider, offered.key) == (provider, key));
        if offered.is_some() && !again {
            return Task::none();
        }
        let restoring = offered.is_some_and(|offered| offered.restoring);
        self.chat_ui.restorable_accounts.insert(
            card,
            RestorableAccount {
                provider,
                key,
                home,
                restoring,
                failure: None,
                resend,
            },
        );
        self.restore_chat_account(card)
    }

    /// Lists the conversation's own account again, on the device that keeps
    /// its home, so the conversation continues where its history is.
    fn restore_chat_account(&mut self, card: CardId) -> Task<Message> {
        if self.chat_retry_blocked(card) || self.chat_start_pending(card) {
            return Task::none();
        }
        let Some(restorable) = self.chat_ui.restorable_accounts.get_mut(&card) else {
            return Task::none();
        };
        if restorable.restoring {
            return Task::none();
        }
        restorable.restoring = true;
        restorable.failure = None;
        let operation = account::remote::Operation::Restore {
            provider: restorable.provider,
            key: restorable.key,
        };
        // A draft has no recovery controls; its refusal says what is under way.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card)
            && runtime.is_draft()
        {
            runtime.error = Some(
                "This account isn't in the account list, but its files were kept. Checking its sign-in to restore it…"
                    .into(),
            );
        }
        cosmic::task::future(async move {
            let result =
                tokio::task::spawn_blocking(
                    move || match crate::device_service::request_for_reader(
                        crate::device_service::Operation::AccountLifecycle { operation },
                    )? {
                        crate::device_service::Reply::AccountLifecycle(value) => {
                            serde_json::from_value(value).map_err(|error| error.to_string())
                        }
                        _ => Err("The device did not restore the account".into()),
                    },
                )
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::AccountRestored(card, result))
        })
    }

    fn chat_account_restored(
        &mut self,
        card: CardId,
        result: Result<account::remote::RestoredHome, String>,
    ) -> Task<Message> {
        if self.chat_retry_blocked(card) {
            // A stale card-specific completion must not reopen the source or
            // overwrite its refusal. The account registry still completes.
            if let Some(restorable) = self.chat_ui.restorable_accounts.get_mut(&card) {
                restorable.restoring = false;
            }
            if let Ok(restored) = result {
                self.account_ui.set_registry(restored.registry);
            }
            return Task::none();
        }
        let Some(restorable) = self.chat_ui.restorable_accounts.get_mut(&card) else {
            if let Ok(restored) = result {
                self.account_ui.set_registry(restored.registry);
            }
            return Task::none();
        };
        restorable.restoring = false;
        let restored = match result {
            Ok(restored) => restored,
            Err(error) => {
                // The card shows why, in place of the promise that the account
                // could be restored: a draft in place of the conversation it
                // has not started, where sending again asks again.
                if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card)
                    && runtime.is_draft()
                {
                    runtime.error = Some(format!("{error} Choose another account, or send again."));
                }
                restorable.failure = Some(error);
                return Task::none();
            }
        };
        let restorable = self
            .chat_ui
            .restorable_accounts
            .remove(&card)
            .expect("checked above");
        self.chat_ui.removed_accounts.remove(&card);
        let email = restored
            .registry
            .find(restorable.key)
            .and_then(|account| account.email.clone());
        self.account_ui.set_registry(restored.registry);
        let Some(CardKind::Chat(source)) = self.card_state(card).map(|state| &state.kind) else {
            return Task::none();
        };
        let session = source.clone();
        let name =
            email.unwrap_or_else(|| format!("The {} account", session.launch.provider.label()));
        let notice = match restored.restored {
            account::registry::Restored::Archived { .. } => format!(
                "{name} was restored and archived, because another account uses the same login."
            ),
            _ => format!("{name} was restored."),
        };
        self.status = Some(TransientNotice::success(notice).with_key(NoticeKey::ChatAttach(card)));
        // A draft has no conversation to reopen: the refusal answered its
        // first send, whose message is back in the composer. Sending it again
        // finishes what the reader asked for - if it is still what they
        // asked, on the account that was restored.
        if session.launch.session_id.is_none() {
            let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
                return Task::none();
            };
            if !runtime.is_draft() {
                return Task::none();
            }
            runtime.error = None;
            if restorable.resend.is_none() || restorable.resend != Resend::of(runtime) {
                return Task::none();
            }
            let target = runtime.target;
            return self.handle_chat_ui(Action::Submit(target, PromptMode::Send));
        }
        let intent = self.chat_retry_intent(card, &session);
        self.start_chat(card, session, intent, None, false)
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
                || !runtime.commands.is_idle()
                || runtime.turn_under_way()
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
        if self.chat_retry_blocked(card)
            || (generation.is_none() && !self.chat_ui.removed_accounts.contains(&card))
            || self.chat_start_pending(card)
            || self
                .chat_ui
                .restorable_accounts
                .get(&card)
                .is_some_and(|restorable| restorable.restoring)
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
        let names = account::credential_overrides(account.provider);
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
        if account.bind_home(&mut replacement.launch.recipe).is_err() {
            return Task::none();
        }
        if !chat::is_account_recovery(&source, &replacement) {
            return Task::none();
        }
        // What answered an earlier send, such as a refused switch, no longer
        // describes this attempt, which may take as long to answer.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card)
            && runtime
                .error
                .as_deref()
                .is_some_and(chat::failure::answers_one_send)
        {
            runtime.error = None;
        }
        let target = self.chat_ui.next_target(card);
        self.chat_ui.phases.insert(
            card,
            Phase::Starting(Box::new(Attempt {
                target,
                intent: chat::Intent::Restore,
                session: replacement.clone(),
                upgrade_committed: false,
                resume_committed: false,
                upgrade_admission: None,
                recovery: Some(AccountRecovery {
                    source: source.clone(),
                    committed: false,
                    submit,
                }),
            })),
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
        if self.chat_retry_blocked(card) || !self.chat_ui.removed_accounts.contains(&card) {
            return None;
        }
        let Some(CardKind::Chat(source)) = self.card_state(card).map(|state| &state.kind) else {
            return None;
        };
        source.launch.session_id.as_ref()?;
        let choices = self
            .chat_accounts(source.launch.provider, &source.launch.recipe)
            .into_iter()
            .flat_map(|accounts| accounts.choices)
            .filter(|choice| !choice.selected)
            .collect::<Vec<_>>();
        let pending = self.chat_start_pending(card);
        let restorable = self.chat_ui.restorable_accounts.get(&card);
        let restoring = restorable.map(|restorable| restorable.restoring);
        // Choosing another account while the conversation's own is being
        // restored would start it twice.
        let busy = pending || restoring == Some(true);
        let mut content: Vec<Element<'_, Message>> = Vec::new();
        if let Some(restorable) = restorable {
            // The conversation's own account is the one that holds its
            // history, so getting it back comes before moving elsewhere.
            content.push(
                design::caption(match (&restorable.failure, restorable.restoring) {
                    (_, true) => "Checking the account's sign-in…".into(),
                    (Some(failure), false) => failure.clone(),
                    (None, false) => {
                        "Restore the account to continue this conversation where its history is."
                            .to_owned()
                    }
                })
                .into(),
            );
            // Once a restore has failed, asking again is one way forward
            // among the other accounts, not the obvious next step.
            let restore = Message::ChatUi(Action::RestoreAccount(card));
            content.push(if restorable.failure.is_some() {
                crate::ui_controls::button::secondary("Try again")
                    .on_press_maybe((!busy).then_some(restore))
                    .into()
            } else {
                crate::ui_controls::button::primary("Restore account")
                    .on_press_maybe((!busy).then_some(restore))
                    .into()
            });
        }
        content.push(
            design::caption(if pending {
                "Checking the saved conversation on the selected account…"
            } else if restoring.is_some() {
                "Or continue on another account. The conversation's identity stays unchanged until the provider confirms it."
            } else {
                "Continue this saved conversation on another account. Its identity stays unchanged until the provider confirms it."
            })
            .into(),
        );
        if choices.is_empty() && !pending && restoring.is_none() {
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
                .on_press_maybe((!busy).then_some(Message::ChatUi(Action::RecoverAccount(
                    card,
                    choice.id.clone(),
                ))))
                .into(),
            );
        }
        content.push(
            crate::ui_controls::button::secondary("Open Accounts")
                .on_press(Message::AccountUi(account_ui::Action::OpenPane))
                .into(),
        );
        Some(
            widget::container(crate::interaction::overflow_fade_y(
                widget::scrollable(
                    widget::column::with_children(content)
                        .spacing(6)
                        .padding(space::S),
                )
                .height(Length::Shrink),
                design::base,
            ))
            .max_height(180)
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

    /// Shows and launches in draft `card` the launch options another window
    /// chose for it, which the shared view brought here.
    ///
    /// The shared pane keeps the draft's conversation, so its runtime stays
    /// attached, but the runtime still holds the options this window had: it
    /// would show them, start the conversation on them, and build any later
    /// choice of its own from them, dropping the other window's. A draft
    /// already sending its first message keeps the launch it is sending.
    pub(super) fn adopt_shared_draft_session(
        &mut self,
        card: CardId,
        session: &chat::Session,
    ) -> Task<Message> {
        // A conversation another window started is not a launch option; it
        // is attached, or recovered, as the conversation it is. An operation
        // already under way on the card keeps the launch it began with.
        if !session.is_unstarted() || self.chat_start_pending(card) {
            return Task::none();
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        if !runtime.choosing() || runtime.session.id != session.id || runtime.session == *session {
            return Task::none();
        }
        let provider = session.launch.provider;
        let moved = runtime.session.launch.provider != provider;
        let fresh = moved.then(|| self.chat_draft_snapshot(provider));
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return Task::none();
        };
        let previous = std::mem::replace(&mut runtime.session, session.clone());
        // Which skills exist depends on the provider's home and directory, so
        // a list found for others is wrong until discovery answers again.
        let rehomed = moved
            || previous.launch.recipe.environment != session.launch.recipe.environment
            || previous.launch.recipe.working_directory != session.launch.recipe.working_directory;
        if let Some(fresh) = fresh {
            // Pickers listed the earlier provider's choices.
            runtime.model_picker = None;
            runtime.account_picker = false;
            runtime.permission_picker = false;
            runtime.snapshot = Arc::new(fresh);
        } else if rehomed {
            Arc::make_mut(&mut runtime.snapshot).skills.clear();
        }
        Arc::make_mut(&mut runtime.snapshot).permission_mode =
            draft_permission_mode(&session.launch.recipe, provider);
        if rehomed {
            // A failed start's reason belongs to the account it failed on.
            runtime.error = None;
            runtime.completion.restart(true);
        }
        let target = runtime.target;
        self.load_draft_skills(target)
    }

    /// Takes the draft out of `card` when another window has started the
    /// conversation it shows, under the draft's identity or the new one a
    /// first send saves, so the card can attach to it.
    ///
    /// The draft runtime cannot attach in place: its start attempt would be
    /// taken for this window's own first send, and a failure would retire the
    /// other window's running conversation. A window with a start of its own
    /// under way keeps it, whatever the shared view says. So does a file choice
    /// or paste under way, whose result the draft runtime has to receive;
    /// `adopt_stored_draft_session` catches the card up once each has ended.
    pub(super) fn release_draft_started_elsewhere(
        &mut self,
        card: CardId,
        session: &chat::Session,
    ) -> bool {
        if session.is_unstarted() || self.chat_start_pending(card) {
            return false;
        }
        let draft_of_session = matches!(
            self.cards.get(&card),
            Some(CardRuntime::Chat(runtime))
                if runtime.is_draft()
                    && runtime.session.is_unstarted()
                    && runtime.attachment_chooser_pending.is_none()
                    && runtime.pastes_pending == 0
        );
        if !draft_of_session {
            return false;
        }
        // A first message whose start was given up is unsent writing again.
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
            runtime.unqueue();
        }
        self.carry_unsent_chat(card, true);
        self.cards.remove(&card);
        self.lifecycle.remove(card);
        self.shared_views.attached.insert(card);
        self.chat_ui.peer_started.insert(card, session.id);
        true
    }

    /// Takes down what `card` attached to when the window that started its
    /// conversation has given the start up, so the card can show the draft
    /// that window is back on. The attachment is dropped, not retired: the
    /// conversation is the other window's to end.
    pub(super) fn release_chat_returned_to_draft(
        &mut self,
        card: CardId,
        session: &chat::Session,
    ) -> bool {
        if !session.is_unstarted()
            || self.chat_ui.peer_started.get(&card) != Some(&session.id)
            || self.chat_is_draft(card)
        {
            return false;
        }
        self.chat_ui.peer_started.remove(&card);
        self.chat_ui.phases.remove(&card);
        self.carry_unsent_chat(card, false);
        self.cards.remove(&card);
        self.lifecycle.remove(card);
        self.resolve_notice(NoticeKey::ChatAttach(card));
        self.observe_agent_activity_for_card(card, None);
        true
    }

    pub(super) fn forget_peer_started(&mut self, card: CardId) {
        self.chat_ui.peer_started.remove(&card);
    }

    /// Whether draft `card` is choosing or loading files, whose result its
    /// draft runtime has to receive before the card can follow a
    /// conversation another window started from it.
    pub(super) fn chat_draft_inserting(&self, card: CardId) -> bool {
        !self.chat_start_pending(card)
            && matches!(self.cards.get(&card), Some(CardRuntime::Chat(runtime))
                if runtime.is_draft()
                    && runtime.session.is_unstarted()
                    && (runtime.attachment_chooser_pending.is_some() || runtime.pastes_pending != 0))
    }

    /// The conversation this window's own first send from draft `card` has
    /// already queued for storage, while that send is still saving.
    pub(super) fn draft_start_candidate(&self, card: CardId) -> Option<&chat::Session> {
        self.chat_ui
            .draft_checkpoints
            .get(&card)
            .map(|pending| &pending.candidate)
    }

    pub(in crate::app) fn chat_is_draft(&self, card: CardId) -> bool {
        matches!(self.cards.get(&card), Some(CardRuntime::Chat(runtime)) if runtime.is_draft())
    }

    /// Catches draft `card` up, once it can choose again, with what another
    /// window did while it could not: launch options chosen, or the
    /// conversation started, while its file chooser was open, a paste was
    /// under way or its first message was on the way.
    fn adopt_stored_draft_session(&mut self, card: CardId) -> Task<Message> {
        let Some(CardKind::Chat(session)) = self.card_state(card).map(|state| state.kind.clone())
        else {
            return Task::none();
        };
        if !session.is_unstarted() {
            return self.attach_draft_started_elsewhere(card, &session);
        }
        self.adopt_shared_draft_session(card, &session)
    }

    pub(super) fn chat_scroll_key(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
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
        if self.pane_content_inputs_blocked(card) {
            return None;
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return None;
        };
        // An image viewer covers the transcript: it would scroll unseen.
        if runtime.image_viewer.is_some() {
            return None;
        }
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
            Named::Home | Named::End => {
                let edge = if *key == Named::Home {
                    scroll::Edge::Start
                } else {
                    scroll::Edge::End
                };
                runtime.find_navigation.cancel();
                runtime
                    .scroll_memory
                    .request_edge(runtime.selected_subagent.as_deref(), edge);
                return Some(
                    scroll::snap_to_edge(
                        runtime.target,
                        edge,
                        runtime.find_navigation.guard(),
                        runtime.find_navigation.document_epoch(),
                    )
                    .map(cosmic::Action::App),
                );
            }
            _ => return None,
        };
        runtime.find_navigation.cancel();
        Some(task.map(cosmic::Action::App))
    }

    pub(super) fn new_native_thread(&mut self) -> Task<Message> {
        self.new_native_chat(self.preferred_chat_provider(), None)
    }

    /// Row actions preserve their selected directory even if a focused shell
    /// last reported a different cwd. Provider/account choices use the newly
    /// selected workspace, through the ordinary configured draft constructor.
    pub(super) fn new_native_thread_at(&mut self, directory: PathBuf) -> Task<Message> {
        self.new_native_chat(self.preferred_chat_provider(), Some(directory))
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
                if self.shared_registry_connecting()
                    && self.worktree_view_scope(&directory).is_some()
                {
                    self.shared_views.deferred_default = self
                        .default_pane_scope(&directory)
                        .map(|scope| (scope, directory.clone()));
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
        self.ensure_default_pane_after_loading(true)
    }

    /// A channel created by this window is known to be empty. Opening an
    /// existing channel instead waits for its shared panes before offering an
    /// automatic composer that could receive typing intended for those panes.
    pub(super) fn ensure_new_channel_default_pane(&mut self) -> Task<Message> {
        self.ensure_default_pane_after_loading(false)
    }

    /// Offers the automatic pane a loading scope deferred, once that scope
    /// is still the one shown. Its shared panes, if any, are on the strip by
    /// now, so the pane appears only when the scope proved to be empty.
    pub(super) fn run_deferred_default_pane(&mut self) -> Task<Message> {
        let Some((scope, path)) = self.shared_views.deferred_default.take() else {
            return Task::none();
        };
        if self.active_workspace.as_ref() != Some(&path)
            || self.default_pane_scope(&path).as_ref() != Some(&scope)
        {
            return Task::none();
        }
        self.ensure_default_pane()
    }

    pub(super) fn default_pane_scope(
        &self,
        directory: &std::path::Path,
    ) -> Option<crate::workspace_model::views::Scope> {
        self.durable
            .active_channel
            .map(crate::workspace_model::views::Scope::Channel)
            .or_else(|| self.worktree_view_scope(directory))
    }

    fn ensure_default_pane_after_loading(&mut self, wait_for_shared: bool) -> Task<Message> {
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
        // A daemon without a shared model never admits the scope, so its
        // windows keep offering the pane at once, as before.
        if wait_for_shared
            && let Some(scope) = self.default_pane_scope(&directory)
            && (self.shared_registry_connecting()
                || (self.workspace_model.identity != crate::workspace_model::NO_REGISTRY
                    && !self.shared_views.scope_settled(&scope)))
        {
            self.shared_views.deferred_default = Some((scope, directory));
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
                && runtime.commands.is_idle())
            && !self.chat_start_pending(card)
    }

    /// The workspace as storage is to hold it: every snapshot handed to the
    /// session writer is taken here, so what storage requires of one is
    /// established here.
    ///
    /// Placeholders never reach storage. Once a pane contains work, promote
    /// it before saving so later snapshots cannot silently drop a saved card.
    pub(super) fn persistent_workspace(&mut self) -> Arc<WorkspaceState> {
        self.capture_workspace_history();
        self.settle_focus();
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
        // A pane still reopening holds a conversation nobody knows the
        // identity of yet; it is saved once it starts.
        let defaults = self
            .chat_ui
            .default_chats
            .iter()
            .copied()
            .chain(self.shared_views.defaults.keys().copied())
            .chain(self.resume_ui.reopening.keys().copied())
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
        let mut chat_draft_attachments = self
            .chat_ui
            .draft_attachments
            .iter()
            .filter(|(card, attachments)| open_cards.contains(card) && !attachments.is_empty())
            .map(|(card, attachments)| (*card, attachments.clone()))
            .collect::<HashMap<_, _>>();
        for (card, runtime) in &self.chat_ui.parked {
            if open_cards.contains(card) {
                let (draft, attachments) = runtime.persistent_draft();
                if attachments.is_empty() {
                    chat_draft_attachments.remove(card);
                } else {
                    chat_draft_attachments.insert(*card, attachments);
                }
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
                let (draft, attachments) = runtime.persistent_draft();
                if attachments.is_empty() {
                    chat_draft_attachments.remove(card);
                } else {
                    chat_draft_attachments.insert(*card, attachments);
                }
                if draft.is_empty() {
                    chat_drafts.remove(card);
                } else {
                    chat_drafts.insert(*card, draft);
                }
            }
        }
        let chat_waiting_messages = self
            .durable
            .cards
            .iter()
            .filter(|state| {
                open_cards.contains(&state.id) && matches!(state.kind, CardKind::Chat(_))
            })
            .filter_map(|state| {
                let waiting = self.chat_waiting_to_save(state.id);
                (!waiting.messages.is_empty()).then_some((state.id, waiting))
            })
            .collect::<HashMap<_, _>>();
        let editors = self
            .durable
            .cards
            .iter()
            .filter(|state| {
                open_cards.contains(&state.id) && matches!(state.kind, CardKind::Repository(_))
            })
            .map(|state| state.id)
            .collect::<HashSet<_>>();
        let mut repository_navigation = self.durable.repository_navigation.clone();
        repository_navigation.retain(|card, _| editors.contains(card));
        for (card, runtime) in &self.cards {
            if let CardRuntime::Repository(runtime) = runtime
                && open_cards.contains(card)
            {
                if let Some(navigation) = runtime.navigation() {
                    repository_navigation.insert(*card, navigation);
                } else {
                    repository_navigation.remove(card);
                }
            }
        }
        // Unread state belongs to the saved workspace, not its visible strip.
        // Update durable before the unchanged-snapshot fast path so reads also
        // persist when no draft or editor navigation changed.
        let mut agent_updates = self.sidebar_state.unread_updates().clone();
        agent_updates.retain(|card, _| open_cards.contains(card));
        if agent_updates != self.durable.agent_updates {
            Arc::make_mut(&mut self.durable).agent_updates = agent_updates;
        }
        let admission_sources = self
            .chat_ui
            .phases
            .iter()
            .filter_map(|(card, phase)| match phase {
                Phase::Starting(attempt) if attempt.upgrade_admission.is_some() => {
                    Some((*card, &attempt.session))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let writer_identity = self
            .session
            .as_ref()
            .map(|writer| Arc::downgrade(&writer.operation_store()));
        let draft_starts = self
            .chat_ui
            .draft_checkpoints
            .iter()
            .filter(|(card, pending)| {
                let same_writer = writer_identity
                    .as_ref()
                    .is_some_and(|writer| writer.ptr_eq(&pending.writer));
                same_writer
                    && self.card_state(**card).is_some_and(|state| {
                        matches!(&state.kind, CardKind::Chat(source) if source == &pending.source)
                    })
            })
            .map(|(card, pending)| (*card, &pending.candidate))
            .collect::<HashMap<_, _>>();
        // Durable holds what was last saved of these too, as it does unread
        // state: a draft written once would otherwise differ from durable at
        // every later save, each of which then copied the whole workspace.
        if chat_drafts != self.durable.chat_drafts {
            Arc::make_mut(&mut self.durable).chat_drafts = chat_drafts;
        }
        if chat_draft_attachments != self.durable.chat_draft_attachments {
            Arc::make_mut(&mut self.durable).chat_draft_attachments = chat_draft_attachments;
        }
        if chat_waiting_messages != self.durable.chat_waiting_messages {
            Arc::make_mut(&mut self.durable).chat_waiting_messages = chat_waiting_messages;
        }
        if repository_navigation != self.durable.repository_navigation {
            let durable = Arc::make_mut(&mut self.durable);
            let previous =
                std::mem::replace(&mut durable.repository_navigation, repository_navigation);
            for (card, navigation) in previous {
                if !durable.repository_navigation.contains_key(&card) {
                    self.released_repository_navigation.insert(card, navigation);
                }
            }
        }
        if defaults.is_empty() && draft_starts.is_empty() && admission_sources.is_empty() {
            return Arc::clone(&self.durable);
        }
        let mut saved = (*self.durable).clone();
        for card in &mut saved.cards {
            if let Some(pending) = draft_starts.get(&card.id) {
                // Only the writer sees B before its receipt. Autosaves of A
                // must not race the explicit A -> B checkpoint back to A.
                card.kind = CardKind::Chat((*pending).clone());
            }
            if let Some(source) = admission_sources.get(&card.id) {
                // The canonical view permits an attachment, not a speculative
                // snapshot rewrite of the saved conversation's ownership.
                card.kind = CardKind::Chat((**source).clone());
                saved
                    .pending_chat_admissions
                    .insert(card.id, (**source).clone());
            }
        }
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
    ///
    /// Returns whether a placeholder someone had started using became an
    /// ordinary pane, which the caller saves now. An unused placeholder was
    /// never saved or shared (see [`Self::persistent_workspace`]), so removing
    /// it changes nothing a save writes, and it is saved with the navigation.
    pub(super) fn discard_unused_default_chats(&mut self) -> bool {
        let defaults = self
            .workspace
            .order()
            .filter(|card| self.chat_ui.default_chats.contains(card))
            .collect::<Vec<_>>();
        let mut promoted = false;
        for card in defaults {
            self.chat_ui.default_chats.remove(&card);
            let unused = self.chat_is_unused(card);
            if !unused {
                promoted = true;
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
        }
        promoted
    }

    /// Closes one conversation's `+` menu, including its fork choices.
    pub(super) fn close_composer_actions(&mut self, target: Target) {
        if let Some(runtime) = self.chat_runtime_mut(target) {
            runtime.composer_actions = false;
            runtime.branch_picker = false;
        }
    }

    pub(super) fn dismiss_chat_picker(&mut self) -> bool {
        let Some(card) = self.workspace.focused() else {
            return false;
        };
        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) else {
            return false;
        };
        let open = runtime.model_picker.take().is_some()
            || runtime.permission_picker
            || runtime.account_picker
            || runtime.composer_actions
            || runtime.branch_picker;
        runtime.permission_picker = false;
        runtime.account_picker = false;
        runtime.composer_actions = false;
        runtime.branch_picker = false;
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
            self.insert_chat_card_with_lifetime(session, None, ephemeral, None, None),
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
        self.insert_chat_card_with_lifetime(session, beside, false, draft_from, None)
    }

    /// Opens a saved conversation in a new card, restoring its manual pane
    /// name before its first shared publication or provider attachment can
    /// report another title. `draft_from` is the card it was closed in, whose
    /// kept draft the new card takes. With `reopening`, the card waits for
    /// that preparation, unsaved and unshared, and starts nothing yet.
    pub(super) fn insert_resumed_chat_card(
        &mut self,
        session: chat::Session,
        beside: Option<CardId>,
        user_title: Option<String>,
        draft_from: Option<CardId>,
        reopening: Option<resume_ui::Reopening>,
    ) -> (CardId, Task<Message>) {
        self.insert_chat_card_as(session, beside, false, draft_from, user_title, reopening)
    }

    fn insert_chat_card_with_lifetime(
        &mut self,
        session: chat::Session,
        beside: Option<CardId>,
        ephemeral: bool,
        draft_from: Option<CardId>,
        user_title: Option<String>,
    ) -> Task<Message> {
        self.insert_chat_card_as(session, beside, ephemeral, draft_from, user_title, None)
            .1
    }

    fn insert_chat_card_as(
        &mut self,
        session: chat::Session,
        beside: Option<CardId>,
        ephemeral: bool,
        draft_from: Option<CardId>,
        user_title: Option<String>,
        reopening: Option<resume_ui::Reopening>,
    ) -> (CardId, Task<Message>) {
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
        if let Some(source) = draft_from {
            self.move_chat_draft(source, card_id);
        }
        if ephemeral {
            self.chat_ui.default_chats.insert(card_id);
        }
        let save_title = user_title.is_some();
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: user_title
                .as_deref()
                .unwrap_or_else(|| chat_card_title(session.launch.provider))
                .into(),
            width_fraction: self.new_pane_fraction(
                beside
                    .and_then(|(source, _)| self.card_state(source))
                    .map_or(self.settings.default_native_pane_fraction, |source| {
                        if matches!(source.kind, CardKind::Chat(_)) {
                            source.width_fraction
                        } else {
                            self.settings.default_native_pane_fraction
                        }
                    }),
            ),
            existence: Existence::Open,
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
        self.set_presence(card_id, Exposure::Visible, Residency::Hot);
        if let Some(title) = user_title {
            self.chrome_ui.user_titles.insert(card_id, title);
        }
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
        let skills = match reopening {
            Some(reopening) => {
                self.resume_ui.reopening.insert(card_id, reopening);
                Task::none()
            }
            None => self.open_placed_chat(card_id, session),
        };
        // An automatic placeholder is neither saved nor shared until someone
        // uses it (see `persistent_workspace`), so opening one, as entering an
        // empty worktree does, saves with the navigation that opened it.
        let save = if ephemeral {
            self.save_navigation_soon()
        } else {
            self.persist();
            Task::none()
        };
        let title = if save_title {
            self.save_settings_task(false)
        } else {
            Task::none()
        };
        // The new pane's input takes the keyboard through the workspace's
        // focus request, now if it is drawn or once it starts.
        let task = Task::batch([
            skills,
            self.apply_viewport(update, true, DurableChange::None),
            title,
            save,
        ]);
        (card_id, task)
    }

    /// Opens the conversation `session` in the card `card` just placed for
    /// it: its draft, when nothing has ever run it, or else a connection to it.
    pub(super) fn open_placed_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
    ) -> Task<Message> {
        if session.is_unstarted() {
            return self.open_chat_draft(card, session);
        }
        let intent = if session.native_continuity.retains_provider() {
            chat::Intent::Restore
        } else {
            chat::Intent::Start
        };
        self.open_chat(card, session, intent)
    }

    /// Preserve only the exact explicit launch the failed pane was trying.
    pub(super) fn chat_retry_intent(&self, card: CardId, session: &chat::Session) -> chat::Intent {
        if self.chat_ui.explicit_starts.get(&card) == Some(session)
            && match self.cards.get(&card) {
                Some(CardRuntime::Failed(_)) => true,
                Some(CardRuntime::Chat(runtime)) => {
                    runtime.disconnected && &runtime.session == session
                }
                _ => false,
            }
        {
            chat::Intent::RetryStart
        } else {
            chat::Intent::Restore
        }
    }

    /// Opens the conversation card `card` holds: its draft, when nothing has
    /// ever run it, or else a connection to it.
    ///
    /// A shared draft can be observed while another window is starting it.
    /// Restoring its saved descriptor gives this window no ownership of that
    /// execution, so opening the draft must not schedule provider retirement.
    pub(super) fn open_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
        intent: chat::Intent,
    ) -> Task<Message> {
        if self.chat_awaiting_admission(card) {
            return Task::none();
        }
        if !session.is_unstarted() {
            return self.start_chat(card, session, intent, None, false);
        }
        self.open_chat_draft(card, session)
    }

    /// Puts a draft of `session` in card `card`, on the model last chosen for
    /// its provider.
    fn open_chat_draft(&mut self, card: CardId, session: chat::Session) -> Task<Message> {
        self.chat_ui.capacity_offers.remove(&card);
        self.chat_ui.blocked_starts.remove(&card);
        self.chat_ui.blocked_details.remove(&card);
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
        self.restore_saved_draft(card, &mut runtime);
        // Nothing runs, so there is nothing for the lifecycle to hydrate or
        // hibernate: the card is whole as it stands.
        self.lifecycle.remove(card);
        self.set_residency(card, Residency::Hot);
        self.card_chrome.remove(&card);
        self.cards
            .insert(card, CardRuntime::Chat(Box::new(runtime)));
        self.load_draft_skills(target)
    }

    /// Keep the warm hidden chats within [`warm`]'s budget, releasing the
    /// least recently hidden. Cheap unless a chat went warm since last time.
    pub(super) fn trim_warm_chats(&mut self) {
        if !warm::take_dirty() {
            return;
        }
        let mut runtimes: Vec<&mut Runtime> = self
            .cards
            .values_mut()
            .filter_map(|card| match card {
                CardRuntime::Chat(runtime) => Some(runtime.as_mut()),
                _ => None,
            })
            .chain(self.chat_ui.parked.values_mut().map(Box::as_mut))
            .filter(|runtime| runtime.warm_stamp().is_some())
            .collect();
        let warm: Vec<_> = runtimes
            .iter()
            .map(|runtime| (runtime.warm_stamp().unwrap_or(0), runtime.warm_bytes()))
            .collect();
        for index in warm::over_budget(&warm) {
            runtimes[index].cool();
        }
    }

    pub(super) fn chat_subscription(&self) -> Subscription<Message> {
        let wakes = Subscription::run_with(Source(self.chat_ui.receiver.clone()), stream)
            .map(|(target, wake)| Message::ChatUi(Action::Wake(target, wake)));
        let mut subscriptions = vec![wakes];
        for card in self.cards.values() {
            if let CardRuntime::Chat(runtime) = card {
                subscriptions.extend(images::subscriptions(runtime));
                if runtime.visible.load(Ordering::Relaxed) {
                    subscriptions.extend(logical_prose::subscriptions(runtime));
                    subscriptions.extend(runtime.preparation.subscriptions(runtime.target));
                    subscriptions.extend(runtime.thumbnails.subscriptions(runtime.target));
                }
            }
        }
        // A freed slot matters only to work waiting for one, so the signal is
        // not delivered, and the whole update run, for every finished job.
        let visible = || {
            self.cards.values().filter_map(|card| match card {
                CardRuntime::Chat(runtime) if runtime.visible.load(Ordering::Relaxed) => {
                    Some(runtime)
                }
                _ => None,
            })
        };
        if visible().any(|runtime| flow_layout::has_deferred(runtime.target)) {
            subscriptions.push(flow_layout::availability());
        }
        if visible().any(|runtime| runtime.find.is_some()) {
            subscriptions.push(find_navigation::availability());
        }
        if self.cards.values().any(|card| {
            matches!(card, CardRuntime::Chat(runtime) if runtime.visible.load(Ordering::Relaxed) && runtime.preparation.waiting())
        }) {
            subscriptions.push(preparation::availability());
        }
        if self.cards.values().any(|card| {
            matches!(card, CardRuntime::Chat(runtime)
            if runtime.snapshot.busy && runtime.visible.load(Ordering::Relaxed))
        }) {
            subscriptions.push(
                iced::time::every(Duration::from_secs(1)).map(|_| Message::ChatUi(Action::Tick)),
            );
        }
        Subscription::batch(subscriptions)
    }
    pub(super) fn retain_chat_after_creation_refusal(
        &mut self,
        card: CardId,
        scope: &crate::workspace_model::views::Scope,
    ) -> bool {
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&card).map(Box::as_ref),
        };
        if self.draft_start_scope(card).as_ref() != Some(scope)
            || !runtime.is_some_and(|runtime| {
                runtime.handle().is_some()
                    && self.card_state(card).is_some_and(|state| {
                        state.existence.is_open()
                            && matches!(&state.kind, CardKind::Chat(session)
                                if !session.is_unstarted() && session == &runtime.session)
                    })
            })
        {
            return false;
        }
        let message = if runtime.is_some_and(|runtime| runtime.disconnected) {
            "This conversation could not be shared. It is kept only in this window. Reconnect to continue viewing it."
        } else {
            "This conversation could not be shared. It is kept only in this window; the current conversation is still attached."
        };
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
            runtime.error = Some(message.into());
        }
        if let Some(runtime) = self.chat_ui.parked.get_mut(&card) {
            runtime.error = Some(message.into());
        }
        true
    }

    /// A draft whose first publication was refused was never in a shared
    /// view, so no other client closed it. Keep it in this window, where it
    /// can still start, and say why other windows will not show it.
    pub(super) fn retain_draft_after_creation_refusal(
        &mut self,
        card: CardId,
        scope: &crate::workspace_model::views::Scope,
    ) -> bool {
        if self.draft_start_scope(card).as_ref() != Some(scope)
            || !self.shared_chat_has_local_work(card)
            || !self.card_state(card).is_some_and(|state| {
                state.existence.is_open()
                    && matches!(&state.kind, CardKind::Chat(session) if session.is_unstarted())
            })
        {
            return false;
        }
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
            runtime.error = Some("This pane could not be shared with your other windows. It stays in this window, and sending starts the conversation here.".into());
        }
        true
    }

    /// What `card`'s provider is holding, to be saved in case it does not
    /// come back: the queue saved before, until a connection settles it, then
    /// whatever its chat runtime last heard, each message once, with the
    /// daemon that hosted the provider listing them.
    pub(super) fn chat_waiting_to_save(&self, card: CardId) -> SavedQueue {
        let mut saved = self.chat_ui.waiting.get(&card).cloned().unwrap_or_default();
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&card).map(Box::as_ref),
        };
        if let Some(runtime) = runtime {
            let listed = runtime.waiting_messages();
            if saved.messages.is_empty() && !listed.is_empty() {
                saved.host = runtime.session.proof.clone();
            }
            for message in listed {
                if !saved.messages.iter().any(|kept| kept.id == message.id) {
                    saved.messages.push(message);
                }
            }
        }
        saved.messages.truncate(chat::QueuedMessage::LIMIT);
        saved
    }

    /// Gives a new runtime for `card` the draft saved when its last one
    /// left: the text and every attachment, files as well as images. Text
    /// carried from a draft another window started is checked against that
    /// conversation's first message, once.
    pub(super) fn restore_saved_draft(&mut self, card: CardId, runtime: &mut Runtime) {
        if let Some(draft) = self.chat_ui.drafts.remove(&card)
            && !draft.is_empty()
        {
            runtime.replace_draft(text_editor::Content::with_text(&draft));
        }
        if let Some(attachments) = self.chat_ui.draft_attachments.remove(&card) {
            runtime.replace_attachments(attachments);
        }
        if self.chat_ui.started_elsewhere.remove(&card) {
            runtime.await_draft_echo(DraftEcho::First);
        } else if let Some(after) = self.chat_ui.sending_on_close.remove(&card) {
            runtime.await_draft_echo(DraftEcho::Latest(after));
        }
    }

    pub(super) fn shared_chat_has_local_work(&self, card: CardId) -> bool {
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&card).map(Box::as_ref),
        };
        runtime.is_some_and(|runtime| {
            runtime.closed_elsewhere
                || runtime.has_unsent_text()
                || !runtime.unsent_attachments().is_empty()
                || runtime.transition_blocker().is_some()
        }) || self
            .chat_ui
            .drafts
            .get(&card)
            .is_some_and(|draft| !draft.is_empty())
            || self
                .chat_ui
                .draft_attachments
                .get(&card)
                .is_some_and(|images| !images.is_empty())
    }

    /// A live or starting conversation no authoritative view has ever listed.
    pub(super) fn chat_runs_unobserved_by_peers(&self, card: CardId) -> bool {
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&card).map(Box::as_ref),
        };
        !self.shared_views.has_observed_chat(card)
            && (self.chat_start_pending(card)
                || runtime.is_some_and(|runtime| {
                    runtime.handle().is_some() && !runtime.disconnected && !runtime.snapshot.exited
                }))
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
        if let Some(attachments) = self.chat_ui.draft_attachments.remove(&alias) {
            self.chat_ui
                .draft_attachments
                .insert(canonical, attachments);
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
        self.release_draft_start_capture(card);
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
        self.chat_ui.explicit_starts.remove(&card);
        self.chat_ui.capacity_offers.remove(&card);
        self.release_draft_start_capture(card);
        self.stash_unsent_chat(card);
        self.chat_ui.phases.remove(&card);
        self.observe_agent_activity_for_card(card, None);
    }

    /// Keeps what the reader is composing in draft `card`, whose conversation
    /// another window replaced, for the composer that opens in its place, and
    /// whether its text may only repeat the first message of a conversation
    /// started from it.
    pub(super) fn carry_unsent_chat(&mut self, card: CardId, started: bool) {
        self.stash_unsent_chat(card);
        if started && self.chat_is_draft(card) {
            self.chat_ui.started_elsewhere.insert(card);
        } else {
            self.chat_ui.started_elsewhere.remove(&card);
        }
    }

    /// Captures the unsent text and attachments of `card`'s chat runtime, which is
    /// about to be dropped while the card lives on, so whatever runtime the
    /// card builds next opens with them rather than losing them.
    pub(super) fn stash_unsent_chat(&mut self, card: CardId) {
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) {
            let (draft, attachments) = runtime.persistent_draft();
            if attachments.is_empty() {
                self.chat_ui.draft_attachments.remove(&card);
            } else {
                self.chat_ui.draft_attachments.insert(card, attachments);
            }
            if draft.is_empty() {
                self.chat_ui.drafts.remove(&card);
            } else {
                self.chat_ui.drafts.insert(card, draft);
            }
        }
    }

    /// A first save that will not start its provider may still have
    /// committed B. Queue the B -> A replacement behind it, so storage
    /// converges on the conversation the composer still shows, including
    /// settings changed while saving: when B never landed, storage already
    /// holds A and the replacement is an idempotent no-op. The prompt and
    /// image captures are released with the checkpoint.
    fn release_draft_start_capture(&mut self, card: CardId) {
        let Some(pending) = self.chat_ui.draft_checkpoints.remove(&card) else {
            return;
        };
        let Some(current) = self.card_state(card).and_then(|state| match &state.kind {
            CardKind::Chat(source)
                if state.existence.is_open() && source.id == pending.source.id =>
            {
                Some(source.clone())
            }
            _ => None,
        }) else {
            return;
        };
        let same_writer = self.session.as_ref().is_some_and(|writer| {
            pending
                .writer
                .ptr_eq(&Arc::downgrade(&writer.operation_store()))
        });
        if !same_writer {
            return;
        }
        let snapshot = self.persistent_workspace();
        self.record_shared_chat_identities();
        let offer = self.snapshot_offer(snapshot, true);
        if let Some(writer) = self.session.as_ref() {
            let accepted = writer
                .offer_chat_identity(card, pending.candidate.id, current, offer)
                .is_ok();
            self.snapshot_offer_answered(accepted);
        }
    }
    /// Forgets a chat runtime whose card is going away for good.
    ///
    /// The difference from `chat_forget` is the draft: a card that is about to
    /// be tombstoned will never build a runtime under that id again, so
    /// stashing its unsent text would pin the string for the lifetime of the
    /// process with nothing able to read it back.
    pub(super) fn chat_discard(&mut self, card: CardId) {
        self.chat_ui.default_chats.remove(&card);
        // A late preparation answer for it then finds nothing to start.
        self.forget_reopening(card);
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
        self.chat_ui.draft_attachments.remove(&card);
        self.chat_ui.waiting.remove(&card);
        self.chat_ui.confirmed_losses.remove(&card);
        self.chat_ui.sending_on_close.remove(&card);
        self.chat_ui.started_elsewhere.remove(&card);
        self.chat_ui.parked.remove(&card);
        self.chat_ui.fresh_offers.remove(&card);
        self.chat_ui.blocked_starts.remove(&card);
        self.chat_ui.blocked_details.remove(&card);
        self.chat_ui.removed_accounts.remove(&card);
        self.chat_ui.restorable_accounts.remove(&card);
    }
    pub(super) fn chat_capacity_guidance(&self, card: CardId) -> Option<&'static str> {
        (self.chat_ui.capacity_offers.contains(&card)
            && !self.chat_start_pending(card)
            && matches!(
                self.card_state(card).map(|state| &state.kind),
                Some(CardKind::Chat(_))
            ))
        .then_some(CAPACITY_GUIDANCE)
    }

    /// Whether retrying this card's start would only meet the refusal it has
    /// already met. One that waits on the update restore stands only while
    /// the daemon says it does.
    pub(super) fn chat_retry_blocked(&self, card: CardId) -> bool {
        let stands = |reopen: blocked_recovery::Reopen| {
            reopen != blocked_recovery::Reopen::UpdateRecovery
                || self.chat_ui.update_recovery.wait.holds()
        };
        match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime.start_blocked.is_some_and(stands),
            _ => self
                .chat_ui
                .blocked_starts
                .get(&card)
                .is_some_and(|blocked| {
                    stands(blocked.reopen)
                        && self.card_state(card).is_some_and(|state| {
                            matches!(&state.kind, CardKind::Chat(current) if *current == blocked.source)
                        })
                }),
        }
    }

    pub(super) fn chat_start_pending(&self, card: CardId) -> bool {
        // A reopened conversation still being prepared starts once that ends.
        self.chat_ui.phases.contains_key(&card) || self.reopen_preparing(card)
    }
    /// Whether `card` has begun starting its conversation, as opposed to
    /// waiting for that start to be allowed.
    #[cfg(test)]
    pub(super) fn chat_start_began(&self, card: CardId) -> bool {
        self.chat_ui.phases.contains_key(&card)
    }
    /// Unsent text held for a card that has no chat runtime.
    #[cfg(test)]
    pub(super) fn held_chat_draft(&self, card: CardId) -> Option<&str> {
        self.chat_ui.drafts.get(&card).map(String::as_str)
    }
    /// A shared echo may arrive before a starting chat has a runtime. Only
    /// the exact attempted descriptor reserves that attachment; a canonical
    /// replacement must still be allowed to supersede it.
    pub(super) fn chat_start_matches(&self, card: CardId, session: &chat::Session) -> bool {
        matches!(self.chat_ui.phases.get(&card), Some(Phase::Starting(attempt)) if &attempt.session == session)
    }
    /// Whether `session` is the conversation an unfinished resume of `card`
    /// is starting in place of the one the card holds.
    pub(super) fn chat_resume_in_flight(&self, card: CardId, session: &chat::Session) -> bool {
        matches!(self.chat_ui.phases.get(&card), Some(Phase::Starting(attempt)) if chat::is_retained_resume_commit(&attempt.session, session))
    }
    /// Whether a card that could not put its saved conversation back on screen
    /// should offer to start a new one in the same place.
    ///
    /// A failed card carries the offer itself because its start never built a
    /// runtime to hold it; a card whose provider exited after starting asks
    /// its runtime instead.
    pub(super) fn chat_offers_fresh_conversation(&self, card: CardId) -> bool {
        if self.chat_start_pending(card)
            || self.pane_transfer_pending(card)
            || self
                .card_state(card)
                .is_none_or(|state| !state.existence.is_open())
        {
            return false;
        }
        let offered = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => {
                // A paste or attachment still arriving only disables Fresh in
                // place (see `stopped`); withdrawing the offer would swap the
                // primary button for Close under the pointer.
                runtime.offers_fresh_conversation()
                    && self.card_state(card).is_some_and(|state| matches!(&state.kind, CardKind::Chat(current) if current == &runtime.session || current == &runtime.return_session()))
            }
            _ => {
                (self
                    .chat_ui
                    .blocked_starts
                    .get(&card)
                    .is_some_and(|blocked| blocked.reopen.offers_fresh())
                    && self.chat_retry_blocked(card))
                    || self.chat_ui.fresh_offers.contains(&card)
            }
        };
        offered
            && self.chat_return_session(card).is_some_and(|source| {
                // A typed Fresh offer may refer to an unnamed converted CLI.
                // Only an unknown permanent refusal needs a named saved source.
                (source.launch.session_id.is_some() || !self.chat_retry_blocked(card))
                    && source.fresh_start().is_some()
                    && !self.chat_runs_as_open_agent(card, &source)
            })
    }
    /// An open agent's pane belongs to that agent: closing its conversation
    /// retires the agent and removes the pane, so Fresh has no card to start
    /// in. Close is that pane's recovery.
    fn chat_runs_as_open_agent(&self, card: CardId, source: &chat::Session) -> bool {
        let managed = crate::model::AgentId::from_uuid(card.as_uuid());
        self.workspace_model.agents.iter().any(|agent| {
            !agent.closed
                && (agent.id == managed
                    || agent
                        .session
                        .as_ref()
                        .is_some_and(|owner| owner.id() == source.id))
        })
    }
    pub(super) fn chat_fork_target(&self, card: CardId) -> Option<Target> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        let snapshot = &runtime.snapshot;
        (!self.chat_start_pending(card)
            && !runtime.disconnected
            && !snapshot.exited
            && runtime.commands.is_idle()
            && runtime.queued.is_none()
            && snapshot.ready
            && !runtime.turn_under_way()
            && snapshot.read_only_reason.is_none()
            && snapshot.capabilities.branch)
            .then_some(runtime.target)
    }

    pub(super) fn chat_changes_target(&self, card: CardId) -> Option<Target> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        (runtime.snapshot.file_changes.total_files > 0
            || runtime.snapshot.file_changes.coverage.has_uncached_changes
            || runtime.changes.open)
            .then_some(runtime.target)
    }

    pub(super) fn chat_switch_available(&self, card: CardId) -> bool {
        match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(_)) => true,
            // Only an agent cagents can run as native chat switches to it,
            // and an ACP agent's terminal only once it names its
            // conversation: it publishes no other sign of which one it is in.
            Some(CardKind::Terminal(session)) => session
                .agent_resume_recipe()
                .or_else(|| observed_launch(session))
                .and_then(|recipe| {
                    let provider = Provider::from_program(&recipe.program)?;
                    (provider.has_native_chat()
                        && (!provider.speaks_acp()
                            || provider.resume_id(&recipe.arguments).is_some()))
                    .then_some(())
                })
                .is_some(),
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
        if self.pane_transfer_pending(card) {
            return Some(Cow::Borrowed(
                "This pane is moving to a channel. Wait for the move to finish before changing views.",
            ));
        }
        if let Some(phase) = self.chat_ui.phases.get(&card) {
            return Some(Cow::Borrowed(match phase {
                Phase::SavingDraft { .. } => "Saving this conversation before connecting.",
                Phase::Starting(_) => {
                    "Connecting to native chat. Wait for the connection to finish."
                }
                Phase::Returning { .. } => {
                    "Returning to the terminal. Wait for the handoff to finish."
                }
                Phase::Closing { .. } => "The conversation is closing. Wait for it to finish.",
            }));
        }
        // Returning it to a terminal would retire the actor the service is
        // preparing, or reopen what nobody has confirmed can be reopened.
        if self.reopen_preparing(card) {
            return Some(Cow::Borrowed(
                "This conversation is reopening. Wait for it to connect.",
            ));
        }
        if self.reopen_failed(card) {
            return Some(Cow::Borrowed(
                "This conversation couldn't be reopened. Retry or close it first.",
            ));
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
            // Only while the card still names the conversation that was
            // refused: a replacement is judged on its own.
            CardKind::Chat(_) if self.unnamed_agent(card).is_some() => {
                Some(Cow::Borrowed(blocked_recovery::UNNAMED_AGENT_TERMINAL))
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
            return Some(runtime.return_session());
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
    /// The conversation a close of `card` would stop while it is working, and
    /// whether that is known. This window may be unable to tell: it is still
    /// attaching, failed to attach to a start made elsewhere, or still holds
    /// the draft that start came from.
    pub(super) fn chat_working_session(&self, card: CardId) -> Option<(TerminalId, bool)> {
        let Some(CardKind::Chat(session)) = self.card_state(card).map(|state| &state.kind) else {
            return None;
        };
        let pending = self.chat_start_pending(card);
        // A return already under way has given the conversation up; closing
        // only says not to keep its terminal.
        if (session.is_unstarted() && !pending)
            || self.chat_close_owned(card)
            || self.chat_closed_elsewhere(card)
            || matches!(
                self.chat_ui.phases.get(&card),
                Some(Phase::Returning { .. })
            )
        {
            return None;
        }
        // A window sending its own first message knows what it just started.
        if session.is_unstarted() {
            return Some((session.id, true));
        }
        let certain = match self.cards.get(&card) {
            // The handle's snapshot, not the runtime's copy: that one is only
            // refreshed while the pane is on screen, and a tab can be closed
            // without ever being shown.
            Some(CardRuntime::Chat(runtime)) if runtime.connected() && !pending => {
                let snapshot = runtime.handle()?.snapshot();
                match snapshot.activity() {
                    crate::agent::ActivityState::Working | crate::agent::ActivityState::Waiting => {
                        true
                    }
                    crate::agent::ActivityState::Unknown => return None,
                    // The provider says nothing of what it has not been handed.
                    crate::agent::ActivityState::Idle if runtime.message_on_its_way() => true,
                    // Nor can one that has not come up yet say it is idle.
                    crate::agent::ActivityState::Idle
                        if !snapshot.ready && snapshot.error.is_none() =>
                    {
                        false
                    }
                    crate::agent::ActivityState::Idle => return None,
                }
            }
            // A pane that lost its connection or provider says so already, and
            // closing is how the reader clears it.
            Some(CardRuntime::Chat(runtime))
                if runtime.disconnected
                    || runtime
                        .handle()
                        .is_some_and(|handle| handle.snapshot().exited) =>
            {
                return None;
            }
            Some(CardRuntime::Chat(_)) => false,
            Some(CardRuntime::Loading) if pending => false,
            Some(CardRuntime::Failed(_)) if self.chat_ui.peer_started.contains_key(&card) => false,
            _ => return None,
        };
        Some((session.id, certain))
    }

    /// The first message this window is still sending, which closing would
    /// discard.
    pub(super) fn chat_first_message_on_its_way(&self, card: CardId) -> Option<String> {
        match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime
                .queued
                .as_ref()
                .map(|queued| queued.prompt.text().to_owned()),
            _ => None,
        }
    }

    /// Whether closing or a close-after-return already owns this card.
    pub(super) fn chat_close_owned(&self, card: CardId) -> bool {
        matches!(
            self.chat_ui.phases.get(&card),
            Some(
                Phase::Closing { restart: None, .. }
                    | Phase::Returning {
                        close_requested: true,
                        ..
                    }
            )
        )
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
        self.clear_close_refusal(card);
        if let Some(closed) = self.close_fenced_card(card) {
            return closed;
        }
        if self.chat_awaiting_admission(card) {
            let Some(remove) = self.remove_chat_card(card, None) else {
                return Task::none();
            };
            Arc::make_mut(&mut self.durable)
                .pending_chat_admissions
                .remove(&card);
            return remove;
        }
        // A reopened conversation still being prepared has started nothing
        // from this pane, so closing it leaves the conversation as it was
        // before the reopen; the preparation's answer is then ignored.
        if self.reopen_pending(card) {
            return self.remove_chat_card(card, None).unwrap_or_else(Task::none);
        }
        if self.pane_transfer_pending(card) {
            self.status = Some(TransientNotice::warning(
                "This pane is moving to a channel. Wait for the move to finish before closing it.",
            ));
            return Task::none();
        }
        if matches!(
            self.chat_ui.phases.get(&card),
            Some(Phase::Closing { restart: None, .. })
        ) {
            return Task::none();
        }
        // Its questions are answered, so "Close and end agent" ends what it
        // said still runs before the card goes.
        if let Some(ending) = self.end_unnamed_agent_before_close(card) {
            return ending;
        }
        if !self.prepare_shared_removal(card) {
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
            && (session.owns_codex_listener() || session.native_continuity.retains_provider())
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
                    tokio::task::spawn_blocking(move || close_conversation(handle, Some(saved)))
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
            self.end_chat(card, handle, Some(session), close_conversation)
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
        self.end_chat(card, handle, session, retire_conversation)
    }

    fn end_chat(
        &mut self,
        card: CardId,
        handle: Option<Arc<ClientHandle>>,
        session: Option<chat::Session>,
        end: fn(Option<Arc<ClientHandle>>, Option<chat::Session>) -> Result<(), String>,
    ) -> Task<Message> {
        let target = self.chat_ui.next_target(card);
        self.chat_ui.retiring.insert(target);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || end(handle, session))
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
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card)
            && let Some(mode) = runtime.snapshot.permission_mode.as_deref()
        {
            // The terminal resumes in the mode the conversation is in now.
            if session.launch.provider == Provider::Claude {
                chat::apply_claude_permission_mode(&mut launch, mode);
            } else {
                chat::apply_acp_permission_mode(&mut launch, session.launch.provider, mode);
            }
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
        // The chat's close ran beside the pane holding the only removal. It
        // waits for its turn; a replacement isn't a close, and reports.
        if replacement.is_none()
            && self
                .workspace
                .pending_close()
                .is_some_and(|pending| pending.unmounting != card)
        {
            self.queue_card_close(card, false);
            return None;
        }
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
            snapshot
                .active_model
                .as_ref()
                .map(|model| (&model.id, &model.default_reasoning))
                .hash(&mut hasher);
            hasher.finish()
        };
        if snapshot.models.is_empty()
            || std::mem::replace(&mut runtime.listed_models, fingerprint) == fingerprint
        {
            return Task::none();
        }
        let provider = runtime.session.launch.provider;
        self.observe_listed_models(
            provider,
            snapshot.models.clone(),
            snapshot.active_model.as_ref(),
        )
    }

    pub(super) fn start_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
        intent: chat::Intent,
        source: Option<HostSessionProof>,
        terminate_source: bool,
    ) -> Task<Message> {
        if self.chat_awaiting_admission(card) {
            return Task::none();
        }
        self.start_chat_with_upgrade(card, session, intent, source, terminate_source, None, None)
    }

    pub(in crate::app) fn start_shared_chat_upgrade(
        &mut self,
        card: CardId,
        source: chat::Session,
        canonical: chat::Session,
    ) -> Task<Message> {
        self.start_chat_with_upgrade(
            card,
            source,
            chat::Intent::Restore,
            None,
            false,
            Some(canonical),
            None,
        )
    }

    fn start_chat_with_upgrade(
        &mut self,
        card: CardId,
        session: chat::Session,
        intent: chat::Intent,
        source: Option<HostSessionProof>,
        terminate_source: bool,
        upgrade_admission: Option<chat::Session>,
        pane_start: Option<crate::terminal_ipc::PaneChatStart>,
    ) -> Task<Message> {
        // A fenced pane's saved session is unconfirmed: reconnecting or
        // resuming it could act for an agent that is no longer this pane's.
        if self.shared_views.quarantined.contains(&card) {
            self.status = Some(TransientNotice::warning(
                "This pane can't reconnect because cagents can't confirm its saved session. Close it instead.",
            ));
            return Task::none();
        }
        let initial_session = upgrade_admission
            .as_ref()
            .map_or(session.id, |canonical| canonical.id);
        let intent = self
            .startup_gate
            .initial_panes
            .chat_intent(card, initial_session, intent);
        if intent == chat::Intent::Start
            && source.is_none()
            && !terminate_source
            && upgrade_admission.is_none()
            && session.terminal.is_none()
            && session.launch.session_id.is_some()
            && !session.native_continuity.retains_provider()
        {
            self.chat_ui.explicit_starts.insert(card, session.clone());
        } else if self.chat_ui.explicit_starts.get(&card) != Some(&session) {
            self.chat_ui.explicit_starts.remove(&card);
        }
        self.chat_ui.capacity_offers.remove(&card);
        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
            runtime.capacity_refused = false;
            if intent == chat::Intent::Resume && runtime.resumes_in_place() {
                runtime.freeze_resume_source();
            }
        }
        let selection = None;
        let initially_visible = self.workspace.visibility(card) == Some(VisibilityClass::Visible);
        let target = self.chat_ui.next_target(card);
        self.chat_ui.phases.insert(
            card,
            Phase::Starting(Box::new(Attempt {
                target,
                intent,
                session: session.clone(),
                recovery: None,
                upgrade_committed: false,
                resume_committed: false,
                upgrade_admission: upgrade_admission.clone(),
            })),
        );
        let sender = self.chat_ui.sender.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                // A panic in the provider handshake has to come back as a
                // failed start. Unwinding here fails the blocking task; the
                // UI thread then only sees a join error and can still draw.
                Self::started_chat(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || {
                        let wake = move |wake| {
                            let _ = sender.try_send((target, wake));
                        };
                        match upgrade_admission {
                            Some(canonical) => ClientHandle::connect_upgrade_with_intent(
                                session, canonical, intent, wake,
                            ),
                            None => match pane_start {
                                Some(pane) => ClientHandle::connect_in_pane(pane, session, wake),
                                None => ClientHandle::connect(
                                    session,
                                    intent,
                                    source,
                                    terminate_source,
                                    selection,
                                    wake,
                                ),
                            },
                        }
                        .map(|handle| {
                            // Background restorations must not queue full bodies
                            // in Ready messages while the UI processes other panes.
                            // Ready rechecks actual visibility before rendering.
                            if !initially_visible {
                                let _ = handle.set_visible(false);
                            }
                            Arc::new(handle)
                        })
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
    fn observe_channel_name(
        &mut self,
        target: Target,
        snapshot: &Snapshot,
    ) -> Option<(String, crate::model::ChannelTitleSource)> {
        if !self.chat_ui.channel_names.contains_key(&target) {
            self.chat_ui
                .channel_names
                .retain(|previous, _| previous.card != target.card);
        }
        self.chat_ui
            .channel_names
            .entry(target)
            .or_default()
            .observe(snapshot)
    }

    fn publish_observed_channel_name(
        &mut self,
        target: Target,
        title: String,
        source: crate::model::ChannelTitleSource,
    ) {
        let proof = self
            .workspace_model
            .views
            .iter()
            .find_map(|view| match view.scope {
                crate::workspace_model::views::Scope::Channel(id)
                    if view.primary_chat() == Some(target.card) =>
                {
                    Some((id, view.revision))
                }
                _ => None,
            });
        let settled = self.durable.channels.iter().any(|channel| {
            channel.cards.contains(&target.card)
                && (channel.title_source == crate::model::ChannelTitleSource::Manual
                    && !channel.title.is_empty()
                    || channel.title == title && channel.title_source == source)
        });
        if (settled || self.suggest_channel_title(target.card, title, source))
            && let Some(observation) = self.chat_ui.channel_names.get_mut(&target)
        {
            observation.pending = false;
            observation.last_view = proof;
        }
    }

    /// The target of the chat that names channel `id`, where this window
    /// runs it.
    fn channel_naming_chat(&self, id: crate::model::ChannelId) -> Option<Target> {
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.id == id)?;
        let card = self.channel_primary_chat(channel)?;
        match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.target),
            _ => self.chat_ui.parked.get(&card).map(|runtime| runtime.target),
        }
    }

    /// The name channel `id`'s conversation has given it, as this window's
    /// copy of that conversation knows it.
    pub(super) fn automatic_channel_name(&self, id: crate::model::ChannelId) -> Option<&str> {
        let target = self.channel_naming_chat(id)?;
        self.chat_ui
            .channel_names
            .get(&target)?
            .title
            .as_ref()
            .map(|(title, _)| title.as_str())
    }

    /// Lets the next refresh offer channel `id` its automatic name again,
    /// once its name was cleared here or in another window. A name offered
    /// at the channel's current view is otherwise never offered twice, and
    /// clearing a name leaves the view as it was.
    pub(super) fn offer_channel_name_again(&mut self, id: crate::model::ChannelId) {
        if let Some(target) = self.channel_naming_chat(id)
            && let Some(observation) = self.chat_ui.channel_names.get_mut(&target)
        {
            observation.last_view = None;
        }
    }

    /// Names an unnamed channel from the first prompt its primary chat
    /// delivered, and keeps that name as the chat's observation. The
    /// suggestion can be refused, as when the pane has not yet recorded its
    /// conversation; a chat on no screen reads no snapshot, so only the kept
    /// observation offers the name again at the channel's next view.
    pub(super) fn name_channel_from_prompt(&mut self, target: Target, prompt: &str) {
        if !self.channel_lacks_name(target.card) {
            return;
        }
        let Some(title) = crate::workspace_model::channel_title_from_prompt(prompt) else {
            return;
        };
        if !self.chat_ui.channel_names.contains_key(&target) {
            self.chat_ui
                .channel_names
                .retain(|previous, _| previous.card != target.card);
        }
        let observation = self.chat_ui.channel_names.entry(target).or_default();
        // A name the conversation already reported is published by its own
        // observation.
        if observation.title.is_some() {
            return;
        }
        let source = crate::model::ChannelTitleSource::Prompt;
        observation.title = Some((title.clone(), source));
        observation.pending = true;
        self.publish_observed_channel_name(target, title, source);
    }

    pub(super) fn defer_channel_title_suggestion(
        &mut self,
        edit: &crate::workspace_model::ModelEdit,
    ) {
        let crate::workspace_model::ModelEdit::SuggestConversationTitle {
            id,
            card,
            expected_revision,
            title,
            source,
        } = edit
        else {
            return;
        };
        for (target, observation) in &mut self.chat_ui.channel_names {
            if target.card == *card
                && observation.last_view == Some((*id, *expected_revision))
                && observation
                    .title
                    .as_ref()
                    .is_some_and(|(current, kind)| current == title && kind == source)
            {
                observation.pending = true;
            }
        }
        // No task is emitted: a future genuine Wake or changed view proof
        // samples/adopts current metadata before another optional publication.
    }

    /// A title may precede its pane's first shared-view acknowledgement. Retry
    /// the retained candidate after projection, with the newly accepted proof.
    pub(super) fn refresh_channel_title_suggestions(&mut self) -> Task<Message> {
        for target in self
            .chat_ui
            .channel_names
            .keys()
            .copied()
            .collect::<Vec<_>>()
        {
            if self.chat_runtime_mut(target).is_none() {
                self.chat_ui.channel_names.remove(&target);
            }
        }
        let proofs = self
            .workspace_model
            .views
            .iter()
            .filter_map(|view| match view.scope {
                crate::workspace_model::views::Scope::Channel(id) => {
                    view.primary_chat().map(|card| (card, (id, view.revision)))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        // A refused original revision is never rewritten in the publisher.
        // A newly accepted projection can make a fresh suggestion from the
        // current runtime; unchanged proofs do not create a retry loop.
        let mut openings = Vec::new();
        for (card, proof) in &proofs {
            if !self.channel_needs_name(*card) {
                continue;
            }
            let runtime = match self.cards.get(card) {
                Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
                _ => self.chat_ui.parked.get(card).map(Box::as_ref),
            };
            let Some(runtime) = runtime else {
                continue;
            };
            let target = runtime.target;
            if self
                .chat_ui
                .channel_names
                .get(&target)
                .is_some_and(|observation| observation.last_view == Some(*proof))
            {
                continue;
            }
            // A secondary conversation may have received its name while no
            // channel was eligible. Sample the current cached client snapshot
            // once at this new proof; do not wait for another provider wake.
            let Some(snapshot) = runtime.handle().map(|handle| handle.snapshot()) else {
                continue;
            };
            self.observe_channel_name(target, &snapshot);
            if let Some(observation) = self.chat_ui.channel_names.get_mut(&target) {
                observation.pending = observation.title.is_some();
                observation.last_view = Some(*proof);
            }
            // A pane that became a channel's primary chat, or a channel that
            // lost its name, may need its conversation's opening.
            openings.extend(self.request_opening_name(target));
        }
        let candidates = self
            .chat_ui
            .channel_names
            .iter()
            .filter_map(|(target, observation)| {
                observation
                    .pending
                    .then(|| observation.title.clone())
                    .flatten()
                    .map(|(title, source)| (*target, title, source))
            })
            .collect::<Vec<_>>();
        for (target, title, source) in candidates {
            if self.chat_runtime_mut(target).is_some() {
                self.publish_observed_channel_name(target, title, source);
            } else {
                self.chat_ui.channel_names.remove(&target);
            }
        }
        if openings.is_empty() {
            Task::none()
        } else {
            Task::batch(openings)
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
            || self.pane_transfer_pending(target.card)
        {
            return Task::none();
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) else {
            return Task::none();
        };
        if runtime.target != target || !runtime.choosing() || !runtime.session.is_unstarted() {
            return Task::none();
        }
        let source = runtime.session.clone();
        if !self.card_state(target.card).is_some_and(
            |state| matches!(&state.kind, CardKind::Chat(current) if current == &source),
        ) {
            return Task::none();
        }
        let Some(writer) = self.session.as_ref() else {
            if let Some(runtime) = self.chat_card_runtime_mut(target) {
                runtime.error = Some(
                    "Session storage is unavailable. Nothing was started; your draft is retained."
                        .into(),
                );
            }
            return Task::none();
        };
        let writer_identity = Arc::downgrade(&writer.operation_store());
        // Every Send saves a fresh identity. An earlier attempt that did not
        // start was already returned to A behind its own write.
        let mut candidate = source.clone();
        candidate.id = TerminalId::new();
        let text = runtime.composer.text();
        let resolved_text = completion::invocations(
            completion::from_leading_skill(&text, &runtime.snapshot.skills),
            &runtime.snapshot.skills,
        )
        .into_owned();
        let attachments = runtime.attachments.clone();
        let model = runtime
            .snapshot
            .selected_model
            .clone()
            .map(|model| (model, runtime.snapshot.reasoning.clone()));
        let attempt = uuid::Uuid::new_v4();
        let pending = DraftStartCheckpoint {
            target,
            attempt,
            source: source.clone(),
            candidate: candidate.clone(),
            writer: writer_identity,
            registry: self.workspace_model.identity,
            scope: self.draft_start_scope(target.card),
            backend: draft_start_backend(),
            revision: runtime.revision,
            text,
            resolved_text,
            attachments,
            model,
        };
        self.chat_ui.draft_checkpoints.insert(target.card, pending);
        self.chat_ui
            .phases
            .insert(target.card, Phase::SavingDraft { target });
        // The composer and public source are still A. The snapshot projection
        // contains the exact candidate B and the still-editable text/images.
        let snapshot = self.persistent_workspace();
        self.record_shared_chat_identities();
        // Exact replacement equality is idempotent when B already landed;
        // otherwise the original A identity must still match. This explicit
        // Send never re-arms unrelated quarantined workspace writes.
        let offer = self.snapshot_offer(snapshot, true);
        let writer = self.session.as_ref().expect("writer was checked above");
        let accepted = writer
            .offer_chat_identity(target.card, source.id, candidate, offer)
            .is_ok();
        self.snapshot_offer_answered(accepted);
        let receipt = self
            .session
            .as_ref()
            .expect("writer was checked above")
            .request_flush();
        cosmic::task::future(async move {
            let result = match receipt {
                Ok(receipt) => tokio::task::spawn_blocking(move || {
                    receipt
                        .recv_timeout(Duration::from_secs(30))
                        .map_err(|error| format!("Session save did not complete: {error}"))?
                })
                .await
                .map_err(|_| "The session save worker stopped".to_owned())
                .and_then(|r| r),
                Err(error) => Err(error),
            };
            Message::ChatUi(Action::DraftStartCheckpointed(target, attempt, result))
        })
    }

    fn draft_start_scope(&self, card: CardId) -> Option<crate::workspace_model::views::Scope> {
        use crate::workspace_model::views::Scope;
        self.durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&card))
            .map(|channel| Scope::Channel(channel.id))
            .or_else(|| {
                self.card_state(card)
                    .and_then(|state| self.card_worktree(state))
                    .and_then(|path| self.worktree_view_scope(path))
            })
    }

    fn restore_failed_draft_start_focus(
        &self,
        target: Target,
        source: &chat::Session,
    ) -> Task<Message> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) else {
            return Task::none();
        };
        if !self.refresh_ui.focused
            || !self.card_holds_keyboard(target.card)
            || runtime.target != target
            || runtime.session != *source
            || !runtime.choosing()
            || !runtime.session.is_unstarted()
            || !self.card_state(target.card).is_some_and(|state| {
                state.existence.is_open()
                    && matches!(&state.kind, CardKind::Chat(source) if source == &runtime.session)
            })
        {
            return Task::none();
        }
        // Saving temporarily replaces the editor. Restore it only if the
        // reader has not given the keyboard to another control meanwhile.
        workspace_widget::focus_after_loading(
            self.card_header_widget_id(target.card),
            card_control_id(target.card, "chat-composer"),
        )
        .map(cosmic::Action::App)
    }

    fn draft_start_checkpointed(
        &mut self,
        target: Target,
        attempt: uuid::Uuid,
        result: Result<(), String>,
    ) -> Task<Message> {
        let Some(pending) = self.chat_ui.draft_checkpoints.get(&target.card) else {
            return Task::none();
        };
        if pending.target != target || pending.attempt != attempt {
            return Task::none();
        }
        if !matches!(self.chat_ui.phases.get(&target.card), Some(Phase::SavingDraft { target: active }) if *active == target)
        {
            // Its save outlived the phase that would have started it.
            self.release_draft_start_capture(target.card);
            return Task::none();
        }
        let source_matches = self.card_state(target.card).is_some_and(|card| {
            card.existence.is_open()
                && matches!(&card.kind, CardKind::Chat(source) if source == &pending.source)
        });
        let context_matches = self.workspace_model.identity == pending.registry
            && self.draft_start_scope(target.card) == pending.scope
            && draft_start_backend() == pending.backend
            && self.session.as_ref().is_some_and(|writer| {
                pending
                    .writer
                    .ptr_eq(&Arc::downgrade(&writer.operation_store()))
            });
        let creation_revoked =
            self.shared_chat_creation_revoked(target.card, pending.scope.as_ref());
        let message_matches = self.cards.get(&target.card).is_some_and(|card| match card {
            CardRuntime::Chat(runtime)
                if runtime.target == target
                    && runtime.choosing()
                    && runtime.session == pending.source =>
            {
                let resolved = completion::invocations(
                    completion::from_leading_skill(
                        &runtime.composer.text(),
                        &runtime.snapshot.skills,
                    ),
                    &runtime.snapshot.skills,
                )
                .into_owned();
                runtime.revision == pending.revision
                    && runtime.composer.text() == pending.text
                    && runtime.attachments == pending.attachments
                    && runtime
                        .snapshot
                        .selected_model
                        .clone()
                        .map(|model| (model, runtime.snapshot.reasoning.clone()))
                        == pending.model
                    && resolved == pending.resolved_text
            }
            _ => false,
        });
        let source = pending.source.clone();
        self.chat_ui.phases.remove(&target.card);
        if !source_matches || !context_matches {
            self.release_draft_start_capture(target.card);
            // Another window's launch options wait for this save's answer. The
            // composer adopts them now, or the next Send refuses a stale source.
            let adopted = self.adopt_stored_draft_session(target.card);
            let Some(runtime) = self.chat_card_runtime_mut(target) else {
                return adopted;
            };
            runtime.error = Some(if source_matches {
                "The workspace changed while saving. Nothing was started; your draft is retained, so send it again when ready.".into()
            } else {
                "This draft changed in another window while saving. Nothing was started; your draft is retained, so review it and send again.".into()
            });
            // The same draft keeps the keyboard; another conversation's reader
            // in this card owns its own.
            let shown = runtime.session.clone();
            let focus = if shown.id == source.id {
                self.restore_failed_draft_start_focus(target, &shown)
            } else {
                Task::none()
            };
            return Task::batch([adopted, focus]);
        }
        if creation_revoked {
            self.release_draft_start_capture(target.card);
            if let Some(runtime) = self.chat_card_runtime_mut(target) {
                runtime.error = Some("This pane was closed or its creation was refused. Nothing was started. Your draft is retained; copy it before closing this pane.".into());
            }
            return self.restore_failed_draft_start_focus(target, &source);
        }
        if !message_matches || self.modal_is_open() {
            self.release_draft_start_capture(target.card);
            if let Some(runtime) = self.chat_card_runtime_mut(target) {
                runtime.error = Some("The draft or its workspace changed while saving. Nothing was started; review it and send again.".into());
            }
            return self.restore_failed_draft_start_focus(target, &source);
        }
        if let Err(error) = result {
            self.release_draft_start_capture(target.card);
            tracing::warn!(card = %target.card, %error, "First conversation checkpoint did not complete");
            if let Some(runtime) = self.chat_card_runtime_mut(target) {
                runtime.error = Some("The conversation could not be saved, so nothing was started. Your draft is retained; check the workspace save notice before trying again.".into());
            }
            return self.restore_failed_draft_start_focus(target, &source);
        }
        let pending = self
            .chat_ui
            .draft_checkpoints
            .remove(&target.card)
            .expect("matching checkpoint");
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        runtime.preparation.clear_local();
        let attachments = runtime.take_attachments();
        runtime.queued = Some(Queued {
            prompt: prompt::LocalPrompt::new(pending.text, attachments),
            resolved_text: pending.resolved_text,
            model: pending.model,
            draft: pending.candidate.clone(),
            since: None,
            rechecking: false,
        });
        runtime.replace_draft(text_editor::Content::new());
        runtime.history_cursor = None;
        runtime.history_stash = None;
        runtime.history_attachment_stash = None;
        runtime.error = None;
        runtime.model_picker = None;
        runtime.account_picker = false;
        runtime.session = pending.candidate.clone();
        // Only the accepted checkpoint authorizes B's public projection and
        // the first provider handshake. No fallback or first prompt precedes it.
        self.commit_chat_session(target.card, pending.candidate.clone());
        let pane_start = Some(crate::terminal_ipc::PaneChatStart {
            registry: pending.registry,
            card: target.card,
            source: pending.source,
            allow_local: pending.scope.is_none()
                || self.durable.local_only_cards.contains(&target.card),
        });
        self.start_chat_with_upgrade(
            target.card,
            pending.candidate,
            chat::Intent::Start,
            None,
            false,
            None,
            pane_start,
        )
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
        if !runtime.commands.is_idle() || runtime.disconnected {
            return Task::none();
        }
        let latest = handle.snapshot();
        // Held follow-ups wait until a message just sent has shown, which
        // is when the turn it starts can be seen.
        if latest.ready
            && !latest.busy
            && !latest.exited
            && !runtime.followups.is_empty()
            && runtime.commands.outgoing().next().is_none()
        {
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
            let ids = runtime
                .followups
                .iter()
                .map(|followup| followup.id)
                .collect();
            let message =
                commands::Outgoing::new(text, attachments, PromptMode::Send, &runtime.snapshot);
            let sent = runtime.next_command;
            let task = self.chat_send(target, command, false);
            if let Some(runtime) = self.chat_runtime_mut(target)
                && runtime.next_command != sent
                && let Some(pending) = runtime.commands.newest_mut()
            {
                pending.carry_followups(ids, message);
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
            if snapshot.selected_model.as_ref() == Some(&model) && snapshot.reasoning == reasoning {
                queued.model = None;
            } else if let Some(listed) = snapshot.models.iter().find(|listed| listed.id == model) {
                let reasoning = reasoning.filter(|reasoning| listed.reasoning.contains(reasoning));
                return self.chat_send_step(
                    target,
                    Command::SetModel { model, reasoning },
                    false,
                    Some(QueuedStep::Model),
                    LatestReader::Follow,
                );
            } else if !snapshot.models.is_empty() || waited >= MODEL_PATIENCE {
                queued.model = None;
                runtime.note(format!(
                    "This conversation does not offer {model}, so this message will use its default model."
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
            &queued.prompt.attachments,
            PromptMode::Send,
        ) {
            Ok(command) => self.chat_send_step(
                target,
                command,
                false,
                Some(QueuedStep::Prompt),
                LatestReader::Follow,
            ),
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
                let name = model.as_deref().unwrap_or("the model");
                match chat::failure::DeliveryOutcome::parse(&error) {
                    // Claude did not take the request; the model itself may
                    // be fine, so it stays on offer.
                    // Whether the message itself is sent is reported apart.
                    Some(outcome) if outcome.unknown => runtime.note(format!(
                        "Claude hasn't confirmed the switch to {name} yet, so this message might not use it."
                    )),
                    Some(_) => runtime.note(format!(
                        "Claude didn't take the switch to {name}, so this message won't use it."
                    )),
                    None => {
                        tracing::warn!(%error, "a queued model choice was refused");
                        runtime.note(format!(
                            "{} couldn't be chosen, so this message won't use it.",
                            model.as_deref().unwrap_or("The model")
                        ));
                        if let Some(model) = model {
                            self.forget_chat_model(provider, &model);
                        }
                    }
                }
                None
            }
            (QueuedStep::Prompt, Ok(())) => Some(None),
            (QueuedStep::Prompt, Err(error)) => {
                // The conversation is new, so a message of the reader's in
                // it can only be this one.
                let arrived = runtime
                    .handle()
                    .is_some_and(|handle| handle.has_accepted_user_input());
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
        let text = runtime
            .queued
            .take()
            .map(|queued| (queued.prompt.text().to_owned(), queued.prompt.attachments));
        let _ = runtime.present();
        // A delivery report about a message that arrived anyway says
        // nothing more the reader can act on.
        if let Some(error) = error
            && chat::failure::DeliveryOutcome::parse(&error).is_none()
        {
            runtime.note(format!(
                "The first message arrived, but its sending reported: {error}"
            ));
        }
        if let Some((text, attachments)) = text {
            runtime.remember_with_attachments(text, attachments);
        }
        runtime.refresh();
        if let Some(prompt) = first_prompt {
            self.name_channel_from_prompt(target, &prompt);
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
            Connection::Draft | Connection::AwaitingAdmission | Connection::Connecting => None,
        };
        runtime.session = draft.clone();
        runtime.snapshot = Arc::new(snapshot);
        runtime.forget_transcript();
        let unsent = runtime.commands.abandon();
        runtime.restore_outgoing(unsent);
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
                .commands
                .iter()
                .any(|pending| pending.queued == Some(QueuedStep::Prompt))
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
            return self.adopt_stored_draft_session(target.card);
        }
        self.revert_chat_draft(target, None)
    }

    fn resume_history_navigation(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.commands.is_idle()
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
            HistoryNavigation::Latest
                if runtime.scroll_memory.edge(None) == Some(scroll::Edge::Latest) =>
            {
                self.handle_chat_action(Action::Latest(target))
            }
            // The fetch survives Home, Find, or a wheel; its original snap does not.
            HistoryNavigation::Latest
                if runtime.snapshot.has_later
                    || runtime.snapshot.history_loading
                    || runtime
                        .page_request
                        .is_some_and(|(page, _)| page == view::Page::Earlier) =>
            {
                self.chat_send_step(
                    target,
                    Command::Refresh,
                    false,
                    None,
                    LatestReader::Preserve,
                )
            }
            HistoryNavigation::Latest => Task::none(),
            HistoryNavigation::NewerEdge
                if runtime.parent_scroll().prefetch_later() && runtime.snapshot.has_later =>
            {
                self.chat_send(target, Command::LoadLater, false)
            }
            HistoryNavigation::EarlierExplicit if runtime.snapshot.has_earlier => {
                self.chat_send(target, Command::LoadEarlier, false)
            }
            HistoryNavigation::EarlierEdge
                if runtime.parent_scroll().prefetch_earlier() && runtime.snapshot.has_earlier =>
            {
                self.chat_send(target, Command::LoadEarlier, false)
            }
            _ => Task::none(),
        }
    }

    fn chat_send(&mut self, target: Target, command: Command, clear_draft: bool) -> Task<Message> {
        self.chat_send_step(target, command, clear_draft, None, LatestReader::Follow)
    }
    /// Sends `command`, which is `step` of a queued first message where that
    /// is given.
    fn chat_send_step(
        &mut self,
        target: Target,
        command: Command,
        clear_draft: bool,
        step: Option<QueuedStep>,
        latest_reader: LatestReader,
    ) -> Task<Message> {
        if self.chat_start_pending(target.card) || self.chat_closed_elsewhere(target.card) {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        // Saved history accepts local draft edits and history navigation, not
        // provider work. A stale submit/command must not install a pending
        // prompt that could later be mistaken for work to retry after resume.
        if (runtime.snapshot.exited || runtime.snapshot.read_only_reason.is_some())
            && matches!(
                &command,
                Command::Prompt { .. } | Command::Slash { .. } | Command::Compact
            )
        {
            return Task::none();
        }
        // A queued first message goes out ahead of anything else, and
        // nothing is sent past it while it waits. Anything else given while
        // a command is on its way waits behind it, in order.
        if runtime.disconnected
            || (runtime.queued.is_some() != step.is_some())
            || (step.is_some() && !runtime.commands.is_idle())
        {
            return Task::none();
        }
        let Some(handle) = runtime.handle().cloned() else {
            return Task::none();
        };
        if let Command::Answer {
            request_id, action, ..
        } = &command
            && !runtime.snapshot.requests.iter().any(|request| {
                &request.id == request_id
                    && request.actions.iter().any(|choice| &choice.id == action)
            })
        {
            return Task::none();
        }
        if runtime.commands.repeats(&command) {
            return Task::none();
        }
        if command == Command::LoadEarlier
            && (runtime.selected_subagent.is_some()
                || !runtime.snapshot.has_earlier
                || runtime.snapshot.history_loading
                || runtime.page_request.is_some())
        {
            return Task::none();
        }
        if command == Command::LoadLater
            && (runtime.selected_subagent.is_some()
                || !runtime.snapshot.has_later
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
                .is_some_and(|(page, _)| page == view::Page::Latest)
        {
            return Task::none();
        }
        runtime.next_command = runtime.next_command.wrapping_add(1);
        let id = runtime.next_command;
        let mut pending = commands::PendingCommand::new(id, &command);
        pending.queued = step;
        if let Some(sent) = &pending.sent {
            // A message still on its way reading the same may arrive first,
            // and nothing would tell the two apart.
            pending.sent_after = if runtime
                .commands
                .outgoing()
                .any(|outgoing| outgoing.prompt.text().trim() == sent.trim())
            {
                SentAfter::Unseen
            } else {
                sent_after(&runtime.snapshot, sent)
            };
        }
        if let Some(page) = pending.page {
            if page == view::Page::Latest {
                match latest_reader {
                    LatestReader::Follow => runtime
                        .scroll_memory
                        .request_edge(None, scroll::Edge::Latest),
                    LatestReader::Preserve => {}
                }
                runtime
                    .scroll_memory
                    .preserve_refresh(id, runtime.snapshot.generation);
            }
            runtime.page_request = Some((page, runtime.snapshot.generation));
            match page {
                view::Page::Earlier => {
                    runtime.earlier_asked = Some(runtime.parent_scroll().content())
                }
                view::Page::Newer | view::Page::Latest => {
                    runtime.later_asked = Some(runtime.parent_scroll().content())
                }
            }
        }
        if clear_draft {
            // The message leaves the composer now, for where it is going,
            // rather than when the provider answers: the reader can write
            // the next one meanwhile, and one the provider refuses comes
            // back with the reason.
            let text = runtime.composer.text().trim_end().to_owned();
            let attachments = runtime.take_attachments();
            runtime.replace_draft(text_editor::Content::new());
            let mode = match &command {
                Command::Prompt { mode, .. } => *mode,
                _ => PromptMode::Send,
            };
            let mut outgoing = commands::Outgoing::new(text, attachments, mode, &runtime.snapshot);
            outgoing.written = runtime.next_written;
            runtime.next_written += 1;
            pending.outgoing = Some(outgoing);
            runtime.prepare_local_prompt();
        }
        runtime.error = None;
        let snapshot = runtime.snapshot.clone();
        match runtime.commands.push(pending, command, &snapshot) {
            Some((id, command)) => run_chat_command(target, id, Some(handle), command),
            None => Task::none(),
        }
    }

    /// Sends the next command waiting behind one the provider has answered.
    fn next_chat_command(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let snapshot = runtime.snapshot.clone();
        let Some((id, command)) = runtime.commands.next(&snapshot) else {
            return Task::none();
        };
        let handle = runtime.handle().cloned().filter(|_| !runtime.disconnected);
        run_chat_command(target, id, handle, command)
    }
    /// [`Self::handle_chat_ui`] for the update loop, which also answers how
    /// much of the workspace projection the action changed.
    ///
    /// A visible streaming conversation brings several actions per
    /// presentation: its wake, then the layouts, prose and Find work the new
    /// text starts. Each confines itself to the conversation's own runtime
    /// unless it changes the facts [`ProjectionInputs`] holds, so it reports
    /// [`Projection::Unchanged`] then, rather than rebuilding every card's
    /// sidebar row and chrome. `None` leaves the update's own choice.
    pub(super) fn update_chat_ui(&mut self, action: Action) -> (Task<Message>, Option<Projection>) {
        let Some(scope) = action.local_scope() else {
            return (self.handle_chat_ui(action), None);
        };
        let before = ProjectionInputs::capture(self, scope);
        let task = self.handle_chat_ui(action);
        let projection = before.projection(&ProjectionInputs::capture(self, scope));
        (task, Some(projection))
    }

    pub(super) fn handle_chat_ui(&mut self, action: Action) -> Task<Message> {
        let closed_refresh = match &action {
            Action::Ready(target, _) => Some(target.card),
            _ => None,
        };
        let restore = match &action {
            Action::AccountPicker(target, false)
            | Action::PermissionPicker(target, false)
            | Action::ModelPicker(target, false)
            | Action::SelectSubagent(target, _)
            | Action::ChooseAccount(target, _)
            | Action::DraftModel(target, _, _)
            | Action::Model(target, _)
            | Action::Reasoning(target, _, _)
            | Action::PermissionMode(target, _) => {
                self.cards.get(&target.card).and_then(|card| match card {
                    CardRuntime::Chat(runtime)
                        if runtime.target == *target
                            && (runtime.picker_keyboard
                                || runtime.model_picker.is_some()
                                || runtime.child_picker.is_some()) =>
                    {
                        Some(*target)
                    }
                    _ => None,
                })
            }
            _ => None,
        };
        let insert_ended = match &action {
            Action::PastedImage(target, ..)
            | Action::PastedText(target, ..)
            | Action::PasteRefused(target, ..)
            | Action::ImageSaved(target, ..)
            | Action::AttachmentsPrepared(target, ..) => Some(*target),
            _ => None,
        };
        let task = self.handle_chat_ui_inner(action);
        let task = match insert_ended {
            Some(target)
                if self
                    .chat_runtime_mut(target)
                    .is_some_and(|runtime| runtime.pastes_pending == 0) =>
            {
                Task::batch([task, self.adopt_stored_draft_session(target.card)])
            }
            _ => task,
        };
        self.reconcile_chat_failure_presentations();
        if let Some(card) = closed_refresh {
            self.sync_closed_chat_draft(card);
        }
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

    pub(super) fn reconcile_chat_failure_presentations(&mut self) {
        for card in self.cards.values_mut() {
            if let CardRuntime::Chat(runtime) = card
                && runtime.visible.load(Ordering::Relaxed)
            {
                runtime.reconcile_failure_ui();
            }
        }
    }

    fn stage_chat_attachments(
        &mut self,
        target: Target,
        id: attachment_draft::Ticket,
        paths: Vec<PathBuf>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(bookmark) = runtime.attachment_bookmarks.get(&id.id) else {
            return Task::none();
        };
        if !bookmark.valid {
            runtime.cancel_attachment_insert(id.id, Some("The image insertion point changed while loading. Attach the image again at the new caret.".into()));
            return Task::none();
        }
        if crate::device::is_remote() {
            runtime.cancel_attachment_insert(
                id.id,
                Some("Local files need transfer to the selected device before attachment.".into()),
            );
            return Task::none();
        }
        let rules = chat::AttachmentRules::for_provider(runtime.session.launch.provider);
        if rules
            .max_count
            .is_some_and(|limit| runtime.attachments.len() + paths.len() > limit)
        {
            runtime.cancel_attachment_insert(
                id.id,
                Some("This chat has reached its attachment limit.".into()),
            );
            return Task::none();
        }
        cosmic::task::future(async move {
            let worker_ticket = id.clone();
            let result = images::run_worker(move || {
                worker_ticket.check()?;
                paths
                    .into_iter()
                    .map(|path| {
                        clipboard_image::stage_file_active(
                            path,
                            rules.max_image_bytes,
                            &worker_ticket,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .and_then(|r| r);
            Message::ChatUi(Action::AttachmentsPrepared(target, id, result))
        })
    }

    fn handle_chat_ui_inner(&mut self, action: Action) -> Task<Message> {
        if crate::device::is_remote()
            && let Action::PastedImage(target, id, Some(_)) = &action
        {
            if let Some(runtime) = self.chat_runtime_mut(*target) {
                runtime.cancel_attachment_insert(
                    id.id,
                    Some("Images need transfer to the selected device before attachment.".into()),
                );
            }
            return Task::none();
        }
        let focus = if let Action::Switch(card) = &action {
            self.workspace
                .focus(*card)
                .ok()
                .map(|update| self.apply_viewport(update, true, DurableChange::None))
        } else {
            None
        };
        let action = self.handle_chat_action(action);
        Task::batch(focus.into_iter().chain(std::iter::once(action)))
    }
    fn handle_chat_action(&mut self, action: Action) -> Task<Message> {
        if let Some(target) = action.mutation_target()
            && (self.chat_awaiting_admission(target.card)
                || self.chat_start_pending(target.card)
                || self
                    .card_state(target.card)
                    .is_none_or(|state| !state.existence.is_open()))
        {
            return Task::none();
        }
        if let Some(card) = action.starts_session()
            && (self.chat_awaiting_admission(card)
                || self.chat_closed_elsewhere(card)
                || self.refuse_for_deleting_card(card))
        {
            return Task::none();
        }
        if matches!(&action, Action::Submit(target, _) if self.chat_closed_elsewhere(target.card)) {
            return Task::none();
        }
        match action {
            Action::UpdateRecoveryAnswered(answer) => {
                return self.update_recovery_answered(answer);
            }
            Action::UpdateRecoveryDue => return self.update_recovery_due(),
            Action::UpdateRecoveryReady => return self.release_update_recovery(),
            Action::SendAgain(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.start_blocked != Some(blocked_recovery::Reopen::UpdateRecovery)
                    || runtime.update_wait.holds()
                    || !runtime.is_draft()
                {
                    return Task::none();
                }
                runtime.release_start_block();
                return self.handle_chat_ui_inner(Action::Submit(target, PromptMode::Send));
            }
            Action::ReopenStopped(card) => return self.reopen_stopped(card),
            Action::CopyClosedDraft(target, revision) => {
                return self.copy_closed_chat_draft(target, revision);
            }
            Action::DiscardClosedDraft(target, revision) => {
                return self.discard_closed_chat_draft(target, revision);
            }
            Action::PreparationAvailable => {
                for card in self.cards.values_mut() {
                    if let CardRuntime::Chat(runtime) = card {
                        runtime.resume_preparation();
                    }
                }
            }
            Action::FlowAvailable => {
                let targets: Vec<_> = self
                    .cards
                    .values()
                    .filter_map(|card| match card {
                        CardRuntime::Chat(runtime) if runtime.visible.load(Ordering::Relaxed) => {
                            Some(runtime.target)
                        }
                        _ => None,
                    })
                    .collect();
                let mut tasks = Vec::new();
                for target in targets {
                    for request in flow_layout::deferred_requests(target) {
                        tasks.push(self.handle_chat_action(Action::FlowRequest(target, request)));
                    }
                }
                return Task::batch(tasks);
            }
            Action::FlowRequest(target, request) => {
                if self.chat_runtime_mut(target).is_some()
                    && let Some(entry) = flow_layout::find(target, &request)
                {
                    match flow_layout::run(target, request.clone()) {
                        Ok(task) => return task,
                        Err(epoch) => entry.borrow_mut().defer(&request, epoch),
                    }
                }
            }
            Action::FlowReady(target, request, ready) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(entry) = flow_layout::find(target, &request)
                    && entry.borrow_mut().complete(&request, ready)
                {
                    for cached in runtime
                        .cache
                        .values_mut()
                        .chain(runtime.child_cache.values_mut())
                    {
                        let owns = cached.flow_cache.find(request.id).is_some()
                            || cached
                                .tool_input
                                .iter()
                                .chain(cached.tool_output.iter())
                                .chain(cached.patches.iter())
                                .any(|tool| tool.flow_cache.find(request.id).is_some());
                        if owns {
                            cached.revision = CachedItem::next_revision();
                        }
                    }
                }
                return self.resume_current_find(target, true);
            }

            Action::LocalPromptPrepared(target, run, ready) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.refresh();
                    if runtime.visible.load(Ordering::Relaxed)
                        && let Some(source) =
                            runtime.local_prompt().map(|prompt| prompt.source.clone())
                        && let Some(prepared) = runtime
                            .preparation
                            .accept_local(target, run, &source, ready)
                        && let Some(prompt) = runtime.local_prompt_mut()
                    {
                        prepared.install_local(&mut prompt.presentation);
                        runtime.presentation_revision =
                            runtime.presentation_revision.wrapping_add(1);
                        runtime.find_navigation.changed_document();
                    }
                }
            }
            Action::MarkdownPrepared(target, id, run, ready) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.markdown_prepared(target, &id, run, ready);
                }
                return self.resume_current_find(target, true);
            }
            Action::RetryMarkdown(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime.visible.load(Ordering::Relaxed)
                {
                    runtime.refresh();
                    runtime.preparation.retry();
                    runtime.prepare_local_prompt();
                    let snapshot = runtime.snapshot.clone();
                    runtime.prepare_transcript_cache(&snapshot);
                    runtime.refresh_find();
                }
            }
            Action::DraftModel(target, provider, model) => {
                return self.choose_draft_model(target, provider, model);
            }
            Action::ChooseAccount(target, id) => {
                return self.choose_chat_account(target, id);
            }
            Action::RecoverAccount(card, id) => return self.recover_chat_account(card, id),
            Action::RestoreAccount(card) => return self.restore_chat_account(card),
            Action::AccountRestored(card, result) => {
                return self.chat_account_restored(card, result);
            }
            Action::OpeningName(card, conversation, title) => {
                self.opening_name_read(card, conversation, title);
            }
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
                    runtime.composer_actions = false;
                    runtime.branch_picker = false;
                    runtime.picker_keyboard = false;
                    runtime.account_picker = open && runtime.choosing();
                    runtime.model_picker = None;
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
                    // Its labels missed whatever changed while it was parked.
                    runtime.pull_request_labels_built_from = None;
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
            Action::RetainedCopyCommitted(card, result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(format!(
                        "The saved draft copy could not be stored: {error}. Keep this window open and retry saving."
                    )));
                } else if !self
                    .card_state(card)
                    .is_some_and(|state| state.existence.is_open())
                {
                    // An explicit disposal may have followed the checkpoint.
                    // Its ordinary snapshot remains the latest intention.
                    self.persist();
                }
                return Task::none();
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
                    Err(failure) => {
                        // The upgraded actor keeps the card's own chat ID, so
                        // it is the conversation the card still names: it is
                        // let go, not closed, and a retry reconnects to it.
                        drop(ready.fail(format!(
                            "The upgraded conversation was not saved: {failure}. Its saved identity is retained; retry to reconnect."
                        )));
                    }
                }
                return self.dispatch(Message::ChatUi(Action::Ready(target, ready)));
            }
            Action::ResumeCommitted(target, ready, result) => {
                match result {
                    Ok(()) => {
                        if let Some(Phase::Starting(attempt)) =
                            self.chat_ui.phases.get_mut(&target.card)
                            && attempt.target == target
                        {
                            attempt.resume_committed = true;
                        }
                    }
                    Err(failure) => {
                        // The successor runs under a new chat ID that the card
                        // may not name, so it is closed rather than left running
                        // beside the conversation the card still names. Nothing
                        // was sent to it, so closing it loses nothing; were the
                        // rebind in fact committed, the card reopens the closed
                        // successor from its saved history.
                        let started = ready.fail(format!(
                            "The reopened conversation was not saved: {failure}. Its saved identity is retained; retry to reopen it."
                        ));
                        let retire = started.map_or_else(Task::none, |handle| {
                            self.retire_chat(target.card, Some(handle), None)
                        });
                        return Task::batch([
                            retire,
                            self.dispatch(Message::ChatUi(Action::Ready(target, ready))),
                        ]);
                    }
                }
                return self.dispatch(Message::ChatUi(Action::Ready(target, ready)));
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
                    Err(failure) => {
                        // The failed start then closes the replacement by its
                        // chat ID, as for any account recovery that failed.
                        drop(ready.fail(format!(
                            "The account change was not saved: {failure}. The original conversation is retained."
                        )));
                    }
                }
                return self.dispatch(Message::ChatUi(Action::Ready(target, ready)));
            }
            Action::DraftStartCheckpointed(target, attempt, result) => {
                return self.draft_start_checkpointed(target, attempt, result);
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
                // Drain replica-only debt while the current start phase still
                // supplies the snapshot's admission projection. Recovery below
                // can commit identity authority without a complete snapshot.
                if matches!(self.chat_ui.phases.get(&target.card),
                    Some(Phase::Starting(attempt)) if attempt.target == target)
                {
                    self.materialize_replica_snapshot();
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
                let resumed_source = result
                    .as_ref()
                    .ok()
                    .and_then(|handle| handle.resumed_from())
                    .cloned();
                if !self.card_state(target.card).is_some_and(|card| {
                    card.existence.is_open()
                        && match &card.kind {
                            CardKind::Chat(session) => {
                                attempt.upgrade_admission.as_ref().map_or_else(
                                    || {
                                        account_source
                                            .as_ref()
                                            .or(upgrade_source)
                                            .or(resumed_source.as_ref())
                                            .map_or(session.id == attempt.session.id, |source| {
                                                session == source
                                            })
                                    },
                                    |canonical| session == canonical,
                                )
                            }
                            CardKind::Terminal(session) => {
                                attempt.session.terminal.as_ref().is_some_and(|source| {
                                    source.id == session.id && source.launch == session.launch
                                })
                            }
                            _ => false,
                        }
                }) {
                    if attempt.upgrade_admission.is_some() {
                        // A newer authoritative descriptor cannot inherit
                        // this old source's Content, even if a receipt landed
                        // before its UI acknowledgment. Preserve it under a
                        // fresh inert identity; the late handle only detaches.
                        drop(result);
                        let retained = self.retain_shared_upgrade_source(target.card);
                        self.persist();
                        return retained.0;
                    }
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
                    // A winning peer may have gained proof while this start
                    // was pending. Its model edge deferred attachment until
                    // the losing start answered; finish that catch-up now.
                    return self.adopt_stored_draft_session(target.card);
                }
                self.startup_gate.initial_panes.settled(
                    target.card,
                    attempt
                        .upgrade_admission
                        .as_ref()
                        .map_or(attempt.session.id, |canonical| canonical.id),
                );
                if result.is_err() && attempt.upgrade_admission.is_some() {
                    // Unproved lineage cannot bind the saved Content to the
                    // canonical actor. Keep it as an inert copy and leave
                    // reconnection explicit; no provider restart is authorized.
                    if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                        runtime.connection = Connection::AwaitingAdmission;
                    }
                    if let Some(pane) = self
                        .workspace_model
                        .views
                        .iter()
                        .flat_map(|view| &view.panes)
                        .find(|pane| {
                            pane.id == target.card
                                && matches!(&pane.kind, CardKind::Chat(session)
                                if Some(session) == attempt.upgrade_admission.as_ref())
                        })
                        .cloned()
                    {
                        let retained = self.retain_unresolved_shared_upgrade(&pane);
                        self.persist();
                        return retained.0;
                    }
                    return Task::none();
                }
                match result {
                    Ok(handle) => {
                        if attempt.upgrade_admission.as_ref().is_some_and(|canonical| {
                            handle.upgrade_recovery().is_none_or(|(source, resolved)| {
                                source != &attempt.session || resolved != canonical
                            })
                        }) {
                            self.status = Some(TransientNotice::error(
                                "The shared conversation did not confirm its saved upgrade lineage.",
                            ));
                            return Task::none();
                        }
                        if let Some(recovery) = handle.authenticated_upgrade_recovery() {
                            let source = recovery.source();
                            let resolved = recovery.replacement();
                            let replacement = handle.session();
                            if account_source.is_some()
                                || source != &attempt.session
                                || &replacement != resolved
                                || !recovery.validate()
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
                                        writer.rebind_authenticated_chat_upgrade(
                                            target.card,
                                            recovery.clone(),
                                        )
                                    });
                                self.chat_ui
                                    .phases
                                    .insert(target.card, Phase::Starting(attempt));
                                let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle)))));
                                return cosmic::task::future(async move {
                                    let result = rebind_receipt(receipt).await;
                                    Message::ChatUi(Action::UpgradeCommitted(target, ready, result))
                                });
                            }
                        }
                        if let Some(source) = &resumed_source {
                            let replacement = handle.session();
                            if account_source.is_some()
                                || source != &attempt.session
                                || !chat::is_retained_resume_commit(source, &replacement)
                                // A successor started on a saved conversation
                                // names it in its first snapshot; one that the
                                // daemon proved has none starts its own.
                                || replacement.launch.session_id.as_ref().is_some_and(|id| {
                                    handle.snapshot().session_id.as_ref() != Some(id)
                                })
                            {
                                self.status = Some(TransientNotice::error(
                                    "The reopened conversation did not match the saved one. Its saved identity is retained.",
                                ));
                                return self.retire_chat(target.card, Some(handle), None);
                            }
                            if !attempt.resume_committed {
                                let receipt = self
                                    .session
                                    .as_ref()
                                    .ok_or_else(|| "Session storage is unavailable".to_owned())
                                    .and_then(|writer| {
                                        writer.rebind_chat_resume(
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
                                    let result = rebind_receipt(receipt).await;
                                    Message::ChatUi(Action::ResumeCommitted(target, ready, result))
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
                                    let result = rebind_receipt(receipt).await;
                                    Message::ChatUi(Action::AccountCommitted(target, ready, result))
                                });
                            }
                        }
                        if attempt.upgrade_admission.is_some() {
                            Arc::make_mut(&mut self.durable)
                                .pending_chat_admissions
                                .remove(&target.card);
                        }
                        self.chat_ui.fresh_offers.remove(&target.card);
                        self.chat_ui.blocked_starts.remove(&target.card);
                        self.chat_ui.removed_accounts.remove(&target.card);
                        self.chat_ui.restorable_accounts.remove(&target.card);
                        let mut session = handle.session();
                        session.proof = handle.proof();
                        let snapshot = handle.snapshot();
                        // A saved proof names the daemon that hosted the
                        // conversation. Another daemon started its provider
                        // afresh, with none of the queue the last one held.
                        let other_daemon = attempt.session.proof.is_none()
                            || attempt.session.proof != session.proof;
                        let saved = self
                            .chat_ui
                            .waiting
                            .remove(&target.card)
                            .unwrap_or_default();
                        // A queue saved under an earlier daemon than the one
                        // this conversation last connected through is older
                        // than that connection: a build that keeps no queue
                        // ran in between, and the queue is no longer news.
                        // What a close stopped while its provider still held it
                        // is held nowhere now, and comes back with the reopened
                        // conversation unless the conversation shows it arrived.
                        let (mut waiting, stopped) = if saved.stopped {
                            (Vec::new(), saved.messages)
                        } else if saved.host.is_some() && saved.host == attempt.session.proof {
                            (saved.messages, Vec::new())
                        } else {
                            (Vec::new(), Vec::new())
                        };
                        let channel_name = self.observe_channel_name(target, &snapshot);
                        let title = channel_name.as_ref().map(|(title, _)| title.clone());
                        let activity = Activity {
                            provider: session.launch.provider,
                            state: handle.take_activity(),
                        };
                        handle.take_observed();
                        let visible = self.workspace.visibility(target.card)
                            == Some(VisibilityClass::Visible);
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            // A provider that ended took its queue with it,
                            // whichever daemon starts the next one.
                            let ended = runtime.snapshot.exited;
                            if ended || other_daemon {
                                for message in runtime.waiting_messages() {
                                    if !waiting.iter().any(|saved| saved.id == message.id) {
                                        waiting.push(message);
                                    }
                                }
                            } else {
                                waiting.clear();
                            }
                            runtime.preparation.clear();
                            runtime.forget_local_presentations();
                            if runtime.retains_source_body() {
                                // The validated successor owns a new source,
                                // even if its generation and item IDs repeat.
                                runtime.warm = None;
                                runtime.release_transcript_caches();
                            }
                            runtime.retarget(target);
                            runtime.connection = Connection::Live(handle);
                            runtime.model_picker = None;
                            runtime.account_picker = false;
                            runtime.session = session.clone();
                            runtime.disconnected = false;
                            // Whatever the old handle was still sending may
                            // or may not have arrived. It comes back to the
                            // composer, which lets go of it if the
                            // conversation shows it did.
                            let after = runtime.commands.first_unsent_after();
                            let unsent = runtime.commands.abandon();
                            if !unsent.is_empty() {
                                runtime.restore_outgoing(unsent);
                                runtime.await_draft_echo(DraftEcho::Latest(after));
                            }
                            // Request generations belong to the old handle. A
                            // restarted daemon can begin again at generation 1.
                            runtime.page_request = None;
                            runtime.deferred_page = None;
                            runtime.earlier_asked = None;
                            runtime.later_asked = None;
                            runtime.error = None;
                            runtime.capacity_refused = false;
                            runtime.resume_failed = false;
                            runtime.start_blocked = None;
                            runtime.logical_prose.release();
                            runtime.resume_source_pinned = false;
                            runtime.show(visible);
                            // Show the new connection's state at once, as a
                            // new runtime does, rather than the ended one's
                            // until the next publication: a resumed card must
                            // not offer Resume again in between.
                            runtime.refresh();
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
                            self.restore_saved_draft(target.card, &mut runtime);
                            runtime.tool_groups_expanded = self.settings.chat_expand_tool_groups;
                            runtime.show(visible);
                            if let Some(CardRuntime::Terminal(terminal)) = self
                                .cards
                                .insert(target.card, CardRuntime::Chat(Box::new(runtime)))
                            {
                                self.retire_switched_terminal(terminal, &session);
                            }
                            // The same daemon still hosts the provider that
                            // held the saved queue, and lists what it holds.
                            if !other_daemon {
                                waiting.clear();
                            }
                        }
                        // What the new provider lists is still waiting there,
                        // and what its history shows sent has arrived. The
                        // rest may never have reached the agent, or may have
                        // just before the last one went: either way it is the
                        // reader's to send again, edit or discard.
                        for message in stopped {
                            if !waiting.iter().any(|saved| saved.id == message.id) {
                                waiting.push(message);
                            }
                        }
                        let waiting = unaccounted(&snapshot, waiting);
                        let restored_waiting = !waiting.is_empty();
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            runtime.restore_waiting(waiting);
                        }
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) {
                            session = runtime.session.clone();
                        }
                        self.chat_ui.explicit_starts.remove(&target.card);
                        self.resolve_notice(NoticeKey::ChatAttach(target.card));
                        if account_source.is_some()
                            || resumed_source.is_some()
                            || attempt.upgrade_admission.is_some()
                        {
                            // Shared projection may already hold the canonical
                            // descriptor. Still save the confirmed draft and
                            // removal of its pending marker after the receipt.
                            self.install_chat_session(target.card, session.clone());
                            self.persist();
                        } else {
                            self.commit_chat_session(target.card, session);
                        }
                        // The draft that now holds them and the queue saved
                        // without them are written together.
                        if title.is_some_and(|title| self.apply_reported_title(target.card, title))
                            || restored_waiting
                        {
                            self.persist();
                        }
                        if let Some((name, source)) = channel_name {
                            self.publish_observed_channel_name(target, name, source);
                        }
                        let opening_name =
                            self.request_opening_name(target).unwrap_or_else(Task::none);
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        // A new request can reject before its first Changed
                        // wake reaches this window. Historical reopen alone
                        // does not manufacture a fresh limit occurrence.
                        if matches!(
                            attempt.intent,
                            chat::Intent::Start | chat::Intent::RetryStart | chat::Intent::Resume
                        ) && let Some(CardRuntime::Chat(runtime)) =
                            self.cards.get_mut(&target.card)
                            && matches!(
                                runtime.session.launch.provider,
                                Provider::Claude | Provider::Codex
                            )
                            && runtime.failure_ui.active_account_limit()
                        {
                            runtime.limit_refresh_pending = true;
                        }
                        let limits = self.refresh_chat_limit_accounts();
                        // Reports that raced the attachment woke nothing.
                        let usage = self.take_chat_account_usage(target);
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
                        // The focused pane's input takes the keyboard once it
                        // is drawn, unless the person chose another control
                        // while it attached. An attachment that finishes
                        // behind another pane leaves the keyboard with the
                        // one in use.
                        if self.workspace.focused() == Some(target.card) {
                            self.keyboard_owed = Some(target.card);
                        }
                        return Task::batch([
                            submitted,
                            usage,
                            limits,
                            opening_name,
                            self.list_chat_models(target.card),
                            self.dispatch_queued(target),
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
                        // Each refusal follows the reader's own send, so
                        // each one asks for its account to be restored.
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            runtime.unqueue();
                            runtime.capacity_refused = failure.recovery == chat::Recovery::Capacity;
                            runtime.start_blocked = blocked_recovery::Reopen::of(failure.recovery);
                            runtime.error = Some(failure.message);
                        }
                        let restore = match failure.recovery {
                            chat::Recovery::Account => {
                                self.offer_chat_account_restore(target.card, &attempt.session, true)
                            }
                            chat::Recovery::UpdateRecovery => self.update_refused(),
                            _ => Task::none(),
                        };
                        return Task::batch([
                            self.retire_chat(target.card, None, Some(attempt.session)),
                            restore,
                        ]);
                    }
                    Err(failure)
                        if matches!(
                            failure.recovery,
                            chat::Recovery::Busy
                                | chat::Recovery::Unresponsive
                                | chat::Recovery::Unreached
                                | chat::Recovery::NotAdmitted
                        ) && account_source.is_some()
                            && automatic_submit.is_some() =>
                    {
                        // The daemon never admitted the automatic switch, so
                        // nothing moved: the conversation is still on its own
                        // account and the candidate never started, so there
                        // is nothing to retire and no account to restore. The
                        // card stays exactly as it was. While it is still
                        // connected it says why the send did nothing; one
                        // that lost its connection during the wait keeps that
                        // reason and its own reconnect instead of advice to
                        // send again.
                        tracing::info!(card = %target.card, recovery = ?failure.recovery, reason = %failure.message, "the daemon did not admit an automatic account switch");
                        let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card)
                        else {
                            return Task::none();
                        };
                        // Wakes for the original connection go unread while a
                        // switch is pending, so what happened to it during
                        // the wait is read from the connection itself.
                        if let Some(handle) = runtime.handle().cloned() {
                            runtime.refresh();
                            if !handle.is_connected() {
                                runtime.disconnected = true;
                            }
                        }
                        if !runtime.disconnected && !runtime.snapshot.exited {
                            runtime.error = Some(match failure.recovery {
                                chat::Recovery::Busy => {
                                    chat::failure::delivery::ACCOUNT_SWITCH_BUSY.to_owned()
                                }
                                chat::Recovery::Unresponsive => {
                                    chat::failure::delivery::ACCOUNT_SWITCH_UNANSWERED.to_owned()
                                }
                                chat::Recovery::NotAdmitted => {
                                    chat::failure::account_switch_refused(&failure.message)
                                }
                                _ => chat::failure::delivery::ACCOUNT_SWITCH_UNREACHED.to_owned(),
                            });
                        }
                        // As a wake read now would have left it.
                        let session = runtime.session.clone();
                        self.commit_chat_session(target.card, session);
                        self.observe_agent_activity_for_card(target.card, None);
                        return Task::none();
                    }
                    Err(failure)
                        if failure.recovery == chat::Recovery::Fresh
                            && attempt.session.launch.session_id.is_none()
                            && attempt.session.terminal.is_none()
                            && matches!(
                                self.card_state(target.card).map(|state| &state.kind),
                                Some(CardKind::Chat(_))
                            )
                            // An agent ran there, and giving up its card
                            // would end the agent and its conversation.
                            && !self.chat_runs_as_open_agent(target.card, &attempt.session) =>
                    {
                        // A conversation its provider never named, and that
                        // no CLI terminal of the reader's holds, has nothing
                        // to resume, so there is nothing to ask the reader
                        // about: the card opens as a draft, as it would have
                        // if nothing had started it. Chats opened before a
                        // first message started anything come back this way
                        // after an update.
                        // Nothing is left to reopen, so an earlier report
                        // that reopening failed no longer applies.
                        self.resolve_notice(NoticeKey::ChatAttach(target.card));
                        let mut draft = attempt.session.clone();
                        draft.reset_owned_codex_listener();
                        draft.proof = None;
                        self.chat_ui.fresh_offers.remove(&target.card);
                        self.chat_forget(target.card);
                        self.replace_chat_conversation(target.card, draft.clone());
                        let skills = self.open_chat_draft(target.card, draft);
                        return Task::batch([
                            skills,
                            self.end_chat(
                                target.card,
                                None,
                                Some(attempt.session),
                                give_up_failed_start,
                            ),
                        ]);
                    }
                    Err(mut failure) => {
                        // Starting over would give up the agent's card, so its
                        // conversation is refused for good, with Close to end it.
                        let unnamed_agent = failure.recovery == chat::Recovery::Fresh
                            && attempt.session.launch.session_id.is_none()
                            && self.chat_runs_as_open_agent(target.card, &attempt.session);
                        if unnamed_agent {
                            failure.recovery = chat::Recovery::Blocked;
                            failure.message = unnamed_agent_conversation(&attempt.session);
                        }
                        let blocked = if unnamed_agent {
                            Some(blocked_recovery::Reopen::UnnamedAgent)
                        } else {
                            blocked_recovery::Reopen::of(failure.recovery)
                        };
                        let mut running = Task::none();
                        let update_refused =
                            blocked == Some(blocked_recovery::Reopen::UpdateRecovery);
                        if let Some(reopen) = blocked {
                            self.chat_ui.blocked_starts.insert(
                                target.card,
                                blocked_recovery::BlockedStart {
                                    source: attempt.session.clone(),
                                    reopen,
                                    liveness: Default::default(),
                                },
                            );
                            if unnamed_agent {
                                running = self.ask_unnamed_agent_running(
                                    target.card,
                                    attempt.session.clone(),
                                );
                            }
                        } else {
                            self.chat_ui.blocked_starts.remove(&target.card);
                        }
                        let terminal_switch = matches!(
                            self.card_state(target.card).map(|state| &state.kind),
                            Some(CardKind::Terminal(_))
                        );
                        let account_removed =
                            failure.recovery == chat::Recovery::Account || account_source.is_some();
                        let mut restore = Task::none();
                        if account_removed {
                            self.chat_ui.removed_accounts.insert(target.card);
                            // The conversation's own account, also while
                            // another one was being tried for it.
                            let own = account_source.as_ref().unwrap_or(&attempt.session);
                            restore = self.offer_chat_account_restore(target.card, own, false);
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
                            runtime.capacity_refused = failure.recovery == chat::Recovery::Capacity;
                            runtime.error = Some(failure.message);
                            runtime.start_blocked = blocked;
                            // A resume that failed leaves its card where it
                            // was, stopped with its conversation kept, so it
                            // tries again. Reconnecting instead would look for
                            // the actor the resume may already have ended.
                            runtime.resume_failed = attempt.intent == chat::Intent::Resume
                                && runtime.resumes_in_place();
                            runtime.disconnected = !runtime.resume_failed;
                            self.observe_agent_activity_for_card(target.card, None);
                        } else if matches!(
                            self.card_state(target.card).map(|state| &state.kind),
                            Some(CardKind::Chat(_))
                        ) {
                            if failure.recovery == chat::Recovery::Capacity {
                                self.chat_ui.capacity_offers.insert(target.card);
                            } else {
                                self.chat_ui.capacity_offers.remove(&target.card);
                            }
                            self.fail_runtime(target.card, failure.message.clone());
                            if update_refused
                                || blocked.is_some_and(blocked_recovery::Reopen::reopens)
                            {
                                // The pane says what the update did to it and
                                // offers what works now, so an error that it
                                // could not reopen would only alarm.
                                self.resolve_notice(NoticeKey::ChatAttach(target.card));
                            } else {
                                // The notice says what the reader lost in their
                                // terms; the pane keeps the daemon's reason, and
                                // the attempt that succeeds withdraws the notice.
                                let pane = self
                                    .notice_pane_name(target.card)
                                    .unwrap_or_else(|| "a conversation".to_owned());
                                // A launch that names no conversation had none to
                                // reopen: starting it is what failed.
                                // An agent's conversation that ran is reopened,
                                // even though it was never named.
                                let verb = if attempt.intent == chat::Intent::Start
                                    && attempt.session.launch.session_id.is_none()
                                    && !unnamed_agent
                                {
                                    "start"
                                } else {
                                    "reopen"
                                };
                                self.status = Some(
                                    TransientNotice::error(format!("Could not {verb} {pane}."))
                                        .with_details(failure.message)
                                        .with_key(NoticeKey::ChatAttach(target.card)),
                                );
                            }
                        } else {
                            // Failed recognition/attachment must never replace or
                            // tear down the user's still-running terminal.
                            self.chat_ui.fresh_offers.remove(&target.card);
                            let pane = self
                                .notice_pane_name(target.card)
                                .unwrap_or_else(|| "a terminal".to_owned());
                            self.status = Some(
                                TransientNotice::error(format!(
                                    "Could not switch {pane} to native chat."
                                ))
                                .with_details(failure.message)
                                .with_key(NoticeKey::ChatAttach(target.card)),
                            );
                        }
                        let cleanup = if account_source.is_some() {
                            self.retire_chat(target.card, None, Some(attempt.session))
                        } else {
                            Task::none()
                        };
                        return Task::batch([
                            self.drain_terminal(target.card, 0),
                            cleanup,
                            running,
                            if update_refused {
                                self.update_refused()
                            } else {
                                Task::none()
                            },
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
                            restore,
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
                    let current = match runtime.handle() {
                        Some(handle) if matches!(wake, Wake::Changed) => Some(handle.snapshot()),
                        _ => None,
                    };
                    // A queue that changes off screen is saved as it changes.
                    let mut queue_changed = false;
                    if let Some(snapshot) = &current {
                        let ids = snapshot.queued_messages.iter().map(|message| &message.id);
                        if !ids.clone().eq(runtime.parked_queue.iter()) {
                            runtime.parked_queue = ids.cloned().collect();
                            queue_changed = true;
                        }
                    }
                    if let Wake::Disconnected(error) = wake {
                        runtime.disconnected = true;
                        runtime.error = Some(error);
                    }
                    if let Some((name, source)) = current
                        .filter(|_| needs_name)
                        .and_then(|snapshot| self.observe_channel_name(target, &snapshot))
                    {
                        self.publish_observed_channel_name(target, name, source);
                    }
                    if queue_changed {
                        self.persist();
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
                        let before = Arc::clone(&runtime.snapshot);
                        let previous_limit = runtime.failure_ui.limit_occurrence;
                        let rehydrating = runtime.presentation_evicted;
                        runtime.refresh();
                        // An exit can be reported after what answered the
                        // last send, as the daemon reports one that happened
                        // while it was busy only once it is free. Sending
                        // again no longer applies: the provider's own report
                        // and the way back to it stand instead.
                        if runtime.snapshot.exited
                            && runtime
                                .error
                                .as_deref()
                                .is_some_and(chat::failure::answers_one_send)
                        {
                            runtime.error = None;
                        }
                        let settled = runtime.settle_streaming_answer();
                        // First hydration can load historical provider errors
                        // never classified in this window. Current Snapshot/
                        // Local rejection handoff is recorded by refresh itself.
                        let new_limit =
                            !rehydrating && runtime.failure_ui.limit_occurrence != previous_limit;
                        runtime.limit_refresh_pending |= new_limit
                            && runtime.visible.load(Ordering::Relaxed)
                            && matches!(
                                runtime.session.launch.provider,
                                Provider::Claude | Provider::Codex
                            );
                        // What the provider holds is saved as it changes, so a
                        // queue outlives a provider that goes with the computer.
                        let queue_changed =
                            before.queued_messages != runtime.snapshot.queued_messages;
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
                        runtime.usage_limit = handle.limit();
                        runtime.agent_addresses = handle.agent_addresses();
                        let limited = runtime.usage_limit.is_some();
                        let session = runtime.session.clone();
                        let snapshot = handle.snapshot();
                        // Pane names use the same bounded observation cache as
                        // channel names, including secondary conversations. Only
                        // the current primary may publish a channel suggestion.
                        let observed_name = self.observe_channel_name(target, &snapshot);
                        let title = observed_name.as_ref().map(|(title, _)| title.clone());
                        if needs_name && let Some((name, source)) = observed_name {
                            self.publish_observed_channel_name(target, name, source);
                        }
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        self.sidebar_state.set_card_limited(target.card, limited);
                        self.commit_chat_session(target.card, session);
                        self.trim_warm_chats();
                        if title.is_some_and(|title| self.apply_reported_title(target.card, title))
                            || queue_changed
                        {
                            self.persist();
                        }
                        let refresh_limits = self.refresh_chat_limit_accounts();
                        return Task::batch([
                            settled,
                            refresh_limits,
                            self.list_chat_models(target.card),
                            self.resume_current_find(target, true),
                            // A command waiting for the turn a message
                            // starts goes once the conversation shows it.
                            self.next_chat_command(target),
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
                    // A hidden conversation learns of its limit this way too,
                    // without reading its transcript.
                    Wake::Limit => {
                        runtime.usage_limit = handle.limit();
                        let limited = runtime.usage_limit.is_some();
                        self.sidebar_state.set_card_limited(target.card, limited);
                        return Task::none();
                    }
                    // A card's turn is drawn again only for what its
                    // fingerprint holds, which counts the addresses.
                    Wake::AgentAddresses => {
                        runtime.agent_addresses = handle.agent_addresses();
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
                        // The reader can accept the first message before its
                        // command callback reaches the UI. Preserve that
                        // execution across EOF, as the command-error path does.
                        if runtime.queued.is_some() && !handle.has_accepted_user_input() {
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
            Action::SignIn(target, account) => return self.sign_in_chat(target, account),
            Action::ResumeAfterSignIn(target) => {
                return self.resume_after_sign_in(target);
            }
            Action::Reconnect(target) => {
                if self.chat_start_pending(target.card) || self.chat_retry_blocked(target.card) {
                    return Task::none();
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.attachment_chooser_pending.is_some() {
                    runtime.error = Some(
                        "Finish or cancel the file chooser before reconnecting the chat.".into(),
                    );
                    return Task::none();
                }
                if runtime.pastes_pending > 0 {
                    runtime.error = Some(
                        "Wait for the attachments to finish loading before reconnecting the chat."
                            .into(),
                    );
                    return Task::none();
                }
                let session = runtime.session.clone();
                // Attaching again to an actor whose provider exited would only
                // bring back the same stopped snapshot. Its conversation is
                // resumed instead; the card keeps its transcript and draft
                // while the daemon proves the old actor gone.
                let intent = if runtime.resumes_in_place() {
                    chat::Intent::Resume
                } else {
                    self.chat_retry_intent(target.card, &session)
                };
                return self.start_chat(target.card, session, intent, None, false);
            }
            Action::ResumeSaved(target) => {
                if self.chat_start_pending(target.card) || self.chat_retry_blocked(target.card) {
                    return Task::none();
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let session = runtime.session.clone();
                if !runtime.commands.is_idle()
                    || runtime.attachment_chooser_pending.is_some()
                    || runtime.pastes_pending > 0
                {
                    return Task::none();
                }
                if self.chat_retry_intent(target.card, &session) == chat::Intent::RetryStart {
                    return self.start_chat(
                        target.card,
                        session,
                        chat::Intent::RetryStart,
                        None,
                        false,
                    );
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(handle) = runtime.handle() else {
                    return Task::none();
                };
                let snapshot = handle.snapshot();
                if runtime.disconnected || !handle.is_connected() {
                    // The reader can disconnect after its button was drawn.
                    // Recover its normal read-only attachment first; a stale
                    // Resume click must neither become a launch nor do nothing.
                    return self.start_chat(
                        target.card,
                        session,
                        chat::Intent::Restore,
                        None,
                        false,
                    );
                }
                if !runtime.is_saved_history_snapshot(&snapshot)
                    || snapshot.history_loading
                    || !runtime.commands.is_idle()
                    || runtime.attachment_chooser_pending.is_some()
                {
                    return Task::none();
                }
                // Normal authenticated Start already supports replacing this
                // exact closed reader. Its account, owner, conversation and
                // admission fences run before any provider starts. Keep the
                // issued allocation/proof and the Runtime's unsent content;
                // this action neither sends the draft nor creates a new chat.
                let session = runtime.session.clone();
                return self.start_chat(target.card, session, chat::Intent::Start, None, false);
            }
            Action::StartFresh(card) => return self.begin_fresh_conversation(card),
            Action::FreshCloseFinished(target, source, requested, result) => {
                return self.finish_fresh_conversation(target, *source, *requested, result);
            }
            Action::ToggleBlockedDetails(card, source) => {
                if self.blocked_details_source(card) == Some(&source) {
                    if self.chat_ui.blocked_details.get(&card) == Some(&source) {
                        self.chat_ui.blocked_details.remove(&card);
                    } else {
                        self.chat_ui.blocked_details.insert(card, source);
                    }
                }
            }
            Action::CopyBlockedDetails(card, source) => {
                if self.blocked_details_source(card) == Some(&source)
                    && let Some(text) = self.blocked_details_text(card)
                {
                    text_copy::invalidate();
                    return iced::clipboard::write(text.to_owned()).map(cosmic::Action::App);
                }
            }
            Action::UnnamedAgentRunning(card, source, running) => {
                self.record_unnamed_agent_running(card, &source, running);
            }
            Action::UnnamedAgentEnded(card, source, result) => {
                return self.unnamed_agent_ended(card, &source, result);
            }
            Action::CopyBlockedDraft(card, source) => {
                if self
                    .unnamed_agent(card)
                    .is_some_and(|blocked| blocked.source == source)
                    && let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card)
                {
                    let text = runtime.unsent_text();
                    if !text.trim().is_empty() {
                        text_copy::invalidate();
                        self.status = Some(TransientNotice::success("Copied message"));
                        return iced::clipboard::write(text).map(cosmic::Action::App);
                    }
                }
            }
            Action::ComposerCursor(target, cursor) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.composer.move_to(cursor);
                    if runtime.completion.restart(false) {
                        return view::completion_scroll(target, 0.0);
                    }
                }
            }
            Action::ComposerUp(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.history_cursor.is_some() {
                    runtime.recall(true);
                    return Task::none();
                }
                let before = runtime.composer.cursor();
                // Movement belongs to the native editor, including wrapping,
                // preferred column, scrolling and selection collapse. Only a
                // press that cannot move beyond its first rendered row starts
                // history; reaching that row still takes a normal caret step.
                let task = self.handle_chat_ui(Action::Composer(
                    target,
                    text_editor::Action::Move(text_editor::Motion::Up),
                ));
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && before.selection.is_none()
                    && before.position.line == 0
                    && runtime.composer.cursor() == before
                {
                    runtime.recall(true);
                }
                return task;
            }
            Action::ReadComposer(target, action) => {
                if !action.is_edit()
                    && let Some(runtime) = self.chat_runtime_mut(target)
                {
                    runtime.composer.perform(action);
                }
            }
            Action::Composer(target, action) => {
                let mut files = None;
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if action.is_edit() {
                        runtime.dismiss_suggestion();
                        runtime.revision = runtime.revision.wrapping_add(1);
                        // Typing into a recalled message makes it the draft,
                        // so the next Up starts again from the newest.
                        runtime.history_cursor = None;
                        runtime.history_stash = None;
                        runtime.history_attachment_stash = None;
                    }
                    // The completion rows follow the word at the caret, so
                    // anything that can change that word starts them afresh.
                    let rescroll = !matches!(action, text_editor::Action::Scroll { .. })
                        && runtime.completion.restart(action.is_edit());
                    runtime.composer_action(action);
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
                if runtime.pastes_pending >= 2 {
                    runtime.error = Some(
                        "Two attachments are already loading. Wait for them before pasting again."
                            .into(),
                    );
                    return Task::none();
                }
                let Some(id) = runtime.try_attachment_insert() else {
                    return Task::none();
                };
                return iced::clipboard::read_data::<clipboard_image::Image>().map(move |image| {
                    cosmic::Action::App(Message::ChatUi(Action::PastedImage(
                        target,
                        id.clone(),
                        image,
                    )))
                });
            }
            Action::PastedImage(target, id, Some(image)) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(bookmark) = runtime.attachment_bookmarks.get(&id.id) else {
                    return Task::none();
                };
                if !bookmark.valid {
                    runtime.cancel_attachment_insert(id.id, Some("The image insertion point changed while loading. Attach the image again at the new caret.".into()));
                    return Task::none();
                }
                let rules = chat::AttachmentRules::for_provider(runtime.session.launch.provider);
                let refusal = if !runtime.accepts_draft_attachments() {
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
                            Some(text) => Action::PastedText(target, id.clone(), Some(text)),
                            None => Action::PasteRefused(target, id.clone(), refusal),
                        }))
                    });
                }
                runtime.image_pastes_saving += 1;
                return cosmic::task::future(async move {
                    let worker_ticket = id.clone();
                    let result = images::run_worker(move || {
                        clipboard_image::save_active(image, rules.max_image_bytes, &worker_ticket)
                    })
                    .await
                    .and_then(|r| r);
                    Message::ChatUi(Action::ImageSaved(target, id, result))
                });
            }
            Action::PastedImage(target, id, None) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !id.active()
                    || !runtime
                        .attachment_bookmarks
                        .get(&id.id)
                        .is_some_and(|bookmark| bookmark.valid)
                {
                    runtime.cancel_attachment_insert(id.id, None);
                    return Task::none();
                }
                return iced::clipboard::read().map(move |text| {
                    cosmic::Action::App(Message::ChatUi(Action::PastedText(
                        target,
                        id.clone(),
                        text,
                    )))
                });
            }
            Action::PastedText(target, id, text) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    let success = runtime.finish_attachment_insert(
                        id.id,
                        text.as_deref().unwrap_or_default(),
                        Vec::new(),
                    );
                    if success
                        && runtime.pastes_pending == 0
                        && let Some(mode) = runtime.submit_after_paste.take()
                    {
                        return self.handle_chat_ui(Action::Submit(target, mode));
                    }
                }
            }
            Action::PasteRefused(target, id, refusal) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.cancel_attachment_insert(id.id, Some(refusal.into()));
                }
            }
            Action::ImageSaved(target, id, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if !runtime.attachment_bookmarks.contains_key(&id.id) {
                        return Task::none();
                    }
                    runtime.image_pastes_saving = runtime.image_pastes_saving.saturating_sub(1);
                    let rules =
                        chat::AttachmentRules::for_provider(runtime.session.launch.provider);
                    let success = match result {
                        Ok(attachment)
                            if runtime.accepts_draft_attachments()
                                && rules
                                    .max_count
                                    .is_none_or(|limit| runtime.attachments.len() < limit) =>
                        {
                            runtime.finish_attachment_insert(id.id, "", vec![attachment])
                        }
                        Ok(_) => {
                            runtime.cancel_attachment_insert(
                                id.id,
                                Some(
                                    if runtime.accepts_draft_attachments() {
                                        "This chat has reached its attachment limit."
                                    } else {
                                        "The current model cannot accept images."
                                    }
                                    .into(),
                                ),
                            );
                            false
                        }
                        Err(error) => {
                            runtime.cancel_attachment_insert(id.id, Some(error));
                            false
                        }
                    };
                    if success
                        && runtime.pastes_pending == 0
                        && let Some(mode) = runtime.submit_after_paste.take()
                    {
                        return self.handle_chat_ui(Action::Submit(target, mode));
                    }
                }
                // No direct unlink: a late/cancelled draft may be referenced
                // by another view. The hard quota and explicit scoped cleanup
                // bound storage without guessing persisted-draft liveness.
            }
            Action::AttachmentsPrepared(target, id, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    let rules =
                        chat::AttachmentRules::for_provider(runtime.session.launch.provider);
                    match result {
                        Ok(attachments) if runtime.accepts_draft_attachments()
                            && rules.max_count.is_none_or(|limit| runtime.attachments.len() + attachments.len() <= limit) => {
                            let success = runtime.finish_attachment_insert(id.id, "", attachments);
                            if success && runtime.pastes_pending == 0 && let Some(mode) = runtime.submit_after_paste.take() {
                                return self.handle_chat_ui(Action::Submit(target, mode));
                            }
                        }
                        Ok(_) => runtime.cancel_attachment_insert(id.id, Some("The model or attachment limit changed while files loaded. Attach them again.".into())),
                        Err(error) => runtime.cancel_attachment_insert(id.id, Some(error)),
                    }
                }
            }
            Action::AppendProviderDraft(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(provided) = &runtime.snapshot.composer
                {
                    let mut text = runtime.composer.text();
                    if !text.trim().is_empty() {
                        text.push('\n');
                    }
                    text.push_str(provided);
                    // Appending promotes the recalled prompt to the unsent
                    // draft, just as typing or completing a word does.
                    runtime.history_cursor = None;
                    runtime.history_stash = None;
                    runtime.history_attachment_stash = None;
                    runtime.replace_draft(text_editor::Content::with_text(&text));
                }
            }
            Action::SubmitControl(target, mode) => {
                let submitted = self.handle_chat_ui(Action::Submit(target, mode));
                // A picker the draft named, `/permissions` or `/resume`, takes
                // the keyboard itself.
                // Starting a draft can replace its runtime, so the card, not
                // the generation, names the composer that keeps the keyboard.
                let composer = match self.cards.get(&target.card) {
                    Some(CardRuntime::Chat(runtime)) => !runtime.permission_picker,
                    _ => false,
                };
                if self.resume_ui.open || !composer {
                    return submitted;
                }
                return Task::batch([
                    submitted,
                    iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
                        .map(cosmic::Action::App),
                ]);
            }
            Action::Submit(target, mode) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                // Match the disabled Send control before account routing,
                // paste deferral or queue mutation can consume the draft.
                // Nothing goes past a first message still on its way; the
                // draft stays as written.
                if runtime.start_blocked.is_some()
                    || runtime.queued.is_some()
                    || (!runtime.is_draft()
                        && (runtime.disconnected
                            || runtime.snapshot.exited
                            || runtime.snapshot.read_only_reason.is_some()
                            || runtime.provider_coming_up()))
                {
                    return Task::none();
                }
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
                    // The daemon closes the conversation before moving it, and
                    // a conversation that looks finished can still hold a
                    // subagent, perhaps indefinitely. Say which, and keep the
                    // message, rather than send it on the account that needs
                    // replacing or attempt a switch the daemon refuses.
                    if let Some(runtime) = self.chat_runtime_mut(target)
                        && let Some(refusal) = account_switch_hold(&runtime.snapshot)
                    {
                        runtime.error = Some(refusal);
                        return Task::none();
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
                        && runtime.turn_under_way())
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
                    let id = runtime.next_written;
                    runtime.next_written += 1;
                    let text = runtime.composer.text().trim_end().to_owned();
                    let followup = Followup {
                        id,
                        preview: prompt::queue_preview(&text),
                        text,
                        attachments: runtime.take_attachments(),
                    };
                    runtime.followups.push(followup);
                    runtime.replace_draft(text_editor::Content::new());
                    runtime.error = None;
                    let _ = runtime.present();
                    return if runtime.turn_under_way() {
                        Task::none()
                    } else {
                        self.dispatch_queued(target)
                    };
                }
                if (mode == PromptMode::Steer && !runtime.snapshot.capabilities.steer)
                    || (mode == PromptMode::Queue && !runtime.snapshot.capabilities.queue)
                    || (mode == PromptMode::Send && runtime.turn_under_way())
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
                        if runtime.followup_sending(id) {
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
                            && let Some(pending) = runtime.commands.newest_mut()
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
                        && !runtime.followup_sending(id)
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
                    runtime.composer_actions = false;
                    runtime.branch_picker = false;
                    runtime.picker_keyboard = false;
                    runtime.permission_picker = open;
                    if open {
                        runtime.model_picker = None;
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
                    // Preparing a retry edits the recalled prompt into the
                    // real unsent draft, just like typing in the composer.
                    runtime.history_cursor = None;
                    runtime.history_stash = None;
                    runtime.history_attachment_stash = None;
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
                    runtime.find_navigation.cancel_all();
                    runtime.restore_find_disclosures();
                    runtime.find = Some(query);
                    runtime.refresh_find();
                    runtime.find_index = runtime.find_matches.len().saturating_sub(1);
                }
                return self.chat_find_next(target, true);
            }
            // Pressing the Previous or Next button blurs the search field;
            // the keyboard goes back to it, so Enter keeps stepping through
            // matches instead of reaching the composer and sending the draft.
            Action::FindNext(target, forward) => {
                return Task::batch([
                    self.chat_find_next(target, forward),
                    iced::widget::operation::focus(card_control_id(target.card, "chat-find"))
                        .map(cosmic::Action::App),
                ]);
            }
            Action::FindRetry(target) => return self.retry_find(target),
            Action::FindAvailable => {
                let targets: Vec<_> = self
                    .cards
                    .values()
                    .filter_map(|card| match card {
                        CardRuntime::Chat(runtime)
                            if runtime.visible.load(Ordering::Relaxed)
                                && runtime.find.is_some() =>
                        {
                            Some(runtime.target)
                        }
                        _ => None,
                    })
                    .collect();
                return Task::batch(
                    targets
                        .into_iter()
                        .map(|target| self.resume_current_find(target, true)),
                );
            }
            Action::FindGroups(target, epoch, query_epoch, run, result) => {
                return self.find_groups(target, epoch, query_epoch, run, result);
            }
            Action::FindHighlight(target, id) => return self.find_highlight(target, id),
            Action::FindHighlighted(target, id, attempt, ready) => {
                return self.find_highlighted(target, id, attempt, ready);
            }
            Action::FindResume(target, intent, geometry_changed) => {
                return self.resume_find_navigation(target, intent, geometry_changed);
            }
            Action::FindExpired(target, intent) => self.expire_find_navigation(target, intent),
            Action::FindIndexed(target, intent, epoch, attempt, hit) => {
                return self.find_indexed(target, intent, epoch, attempt, hit);
            }
            Action::FindLocated(target, intent, epoch, location) => {
                return self.find_located(target, intent, epoch, location);
            }
            Action::CloseFind(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.find_navigation.cancel_all();
                    runtime.restore_find_disclosures();
                    runtime.find = None;
                    runtime.refresh_failure_previews();
                    runtime.find_matches.clear();
                }
                return iced::widget::operation::focus(card_control_id(
                    target.card,
                    "chat-composer",
                ))
                .map(cosmic::Action::App);
            }
            Action::HistoryNewer(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.recall(false);
                }
            }
            Action::Command(target, command) => {
                if matches!(command, Command::Compact | Command::Branch { .. })
                    && let Some(runtime) = self.chat_runtime_mut(target)
                {
                    runtime.composer_actions = false;
                    runtime.branch_picker = false;
                }
                if command == Command::LoadEarlier
                    && let Some(runtime) = self.chat_runtime_mut(target)
                {
                    if runtime.selected_subagent.is_some() {
                        return Task::none();
                    }
                    if runtime
                        .commands
                        .iter()
                        .any(|pending| pending.page.is_none())
                        && runtime.deferred_page != Some(HistoryNavigation::Latest)
                    {
                        runtime.deferred_page = Some(HistoryNavigation::EarlierExplicit);
                        return Task::none();
                    }
                }
                if matches!(command, Command::Branch { .. })
                    && let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime.commands.is_idle()
                    && !runtime.turn_under_way()
                {
                    runtime.branch_picker = false;
                    runtime.fork_requested = true;
                    runtime.fork_seen = runtime.snapshot.forked.clone();
                }
                return self.chat_send(target, command, false);
            }
            Action::Interrupt(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.interrupt_casualties() > 0 && runtime.interrupt_armed.is_none() {
                    let at = Instant::now();
                    runtime.interrupt_armed = Some(at);
                    return cosmic::task::future(async move {
                        tokio::time::sleep(INTERRUPT_CONFIRMATION).await;
                        Message::ChatUi(Action::DisarmInterrupt(target, at))
                    });
                }
                runtime.interrupt_armed = None;
                // Before the turn has begun, Stop also takes back what was
                // lined up behind it. The provider is still told to stop, so
                // the turn ends as it begins.
                runtime.withdraw_before_turn();
                return self.chat_send(target, Command::Interrupt, false);
            }
            Action::DisarmInterrupt(target, at) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime.interrupt_armed == Some(at)
                {
                    runtime.interrupt_armed = None;
                }
            }
            Action::ChangesPicker(target, open) => {
                if !matches!(self.cards.get(&target.card), Some(CardRuntime::Chat(runtime)) if runtime.target == target)
                {
                    return Task::none();
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if open {
                    runtime.composer_actions = false;
                    runtime.branch_picker = false;
                    runtime.changes.show();
                } else {
                    runtime.changes.close();
                    return iced::widget::operation::focus(card_control_id(
                        target.card,
                        "chat-composer",
                    ))
                    .map(cosmic::Action::App);
                }
                return Task::batch([
                    self.chat_change_files(target, None),
                    iced::widget::operation::focus(card_control_id(
                        target.card,
                        "chat-changes-close",
                    ))
                    .map(cosmic::Action::App),
                ]);
            }
            Action::ChangeFiles(target, after) => {
                return self.chat_change_files(target, after);
            }
            Action::ChangeSelect(target, key) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    let resolved_path = runtime
                        .changes
                        .list
                        .as_ref()
                        .unwrap_or(runtime.snapshot.file_changes.as_ref())
                        .files
                        .iter()
                        .find(|file| file.key == key)
                        .and_then(|file| file.resolved_path.clone());
                    runtime.changes.select(key);
                    runtime.changes.resolved_path = resolved_path;
                }
                return self.chat_change_patches(target, None);
            }
            Action::ChangeBack(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.changes.back();
                }
                // The remounted list restores its saved viewport in layout.
            }
            Action::ChangeListScrolled(target, page, epoch, narrow, offset) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.changes.record_scroll(
                        page,
                        runtime.snapshot.file_changes.request_id,
                        epoch,
                        narrow,
                        offset,
                    );
                }
            }
            Action::ChangePatches(target, cursor) => {
                return self.chat_change_patches(target, cursor);
            }
            Action::ChangePatch(target, index) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.changes.patch_index = index;
                    runtime.changes.sync();
                }
            }
            Action::ChangesLoaded(target, id, result) => {
                let files = matches!(&result, Ok(chat::FileChangeReply::Files(_)));
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime.changes.complete(id, result)
                {
                    return iced::widget::scrollable::scroll_to(
                        card_control_id(
                            target.card,
                            if files {
                                "chat-changes-files"
                            } else {
                                "chat-changes-detail"
                            },
                        ),
                        iced::widget::scrollable::AbsoluteOffset {
                            x: Some(0.0),
                            y: Some(0.0),
                        },
                    )
                    .map(cosmic::Action::App);
                }
            }
            Action::ComposerActions(target, open) => {
                if !matches!(self.cards.get(&target.card), Some(CardRuntime::Chat(runtime)) if runtime.target == target)
                {
                    return Task::none();
                }
                if open {
                    self.dismiss_chat_picker();
                }
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.composer_actions = open;
                    runtime.branch_picker = false;
                    if open {
                        runtime.composer_actions_query.clear();
                    }
                }
                // The menu draws over the timeline, after everything the
                // keyboard already walks through, so opening it starts there:
                // in its search when it has commands or skills to find, or
                // else on its first row that can be pressed, so Tab and Enter
                // work from where it opened. Closing leaves focus alone: a
                // press elsewhere has already put it where the person meant,
                // and Escape returns it to the draft on its own.
                let transitioning = self.chat_ui.phases.contains_key(&target.card);
                let first = match self.cards.get(&target.card) {
                    Some(CardRuntime::Chat(runtime)) if open => {
                        if runtime.composer_menu_searchable() {
                            Some(COMPOSER_SEARCH)
                        } else {
                            runtime
                                .composer_action_rows(transitioning)
                                .into_iter()
                                .find_map(|(row, enabled)| enabled.then(|| row.id()))
                        }
                    }
                    _ => None,
                };
                if let Some(id) = first {
                    return iced::widget::operation::focus(card_control_id(target.card, id))
                        .map(cosmic::Action::App);
                }
            }
            Action::ComposerActionsQuery(target, query) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.composer_actions_query = query;
                }
            }
            Action::ComposerActionsSubmit(target) => {
                let transitioning = self.chat_ui.phases.contains_key(&target.card);
                let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) else {
                    return Task::none();
                };
                if runtime.target != target || !runtime.composer_actions {
                    return Task::none();
                }
                let Some((chosen, refocus)) = runtime
                    .composer_menu(transitioning)
                    .into_iter()
                    .find(|(_, enabled)| *enabled)
                    .map(|(entry, _)| match entry {
                        MenuEntry::Row(row) => (
                            runtime.composer_row_action(row),
                            // A row that closes the menu takes the search
                            // with it, so typing returns to the draft; a
                            // chooser or the fork picker takes focus itself,
                            // and steering leaves the menu open.
                            matches!(row, ComposerRow::Plan | ComposerRow::Compact),
                        ),
                        MenuEntry::Named(name) => (
                            Action::ComposerName(target, name.insertion.into_owned()),
                            false,
                        ),
                    })
                else {
                    return Task::none();
                };
                // Through the app's own dispatch, so every guard a pressed row
                // passes, such as a remote device refusing local files, holds
                // for Enter too.
                let task = self.dispatch(Message::ChatUi(chosen));
                if !refocus {
                    return task;
                }
                // Not after the action's reply: typing meanwhile belongs in
                // the draft too.
                return Task::batch([
                    task,
                    iced::widget::operation::focus(card_control_id(target.card, "chat-composer"))
                        .map(cosmic::Action::App),
                ]);
            }
            Action::ComposerName(target, insertion) => {
                let transitioning = self.chat_ui.phases.contains_key(&target.card);
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                // Only a name the menu still offers, and still lets be
                // pressed, is put in the draft: a command offered for an
                // empty draft is not one once the draft has words in it.
                let offered = runtime.composer_menu(transitioning).into_iter().find_map(
                    |(entry, enabled)| match entry {
                        MenuEntry::Named(name) if enabled && name.insertion == insertion => {
                            Some(name.kind)
                        }
                        _ => None,
                    },
                );
                let Some(kind) = offered else {
                    return Task::none();
                };
                runtime.composer_actions = false;
                runtime.branch_picker = false;
                runtime.name_in_draft(&insertion, kind);
                return iced::widget::operation::focus(card_control_id(
                    target.card,
                    "chat-composer",
                ))
                .map(cosmic::Action::App);
            }
            Action::BranchPicker(target, open) => {
                if open && self.chat_fork_target(target.card) != Some(target) {
                    return Task::none();
                }
                if open {
                    self.dismiss_chat_picker();
                }
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.composer_actions = true;
                    runtime.branch_picker = open;
                    if open {
                        runtime.changes.close();
                    }
                }
                if open {
                    return self.chat_send(target, Command::ListBranches, false);
                }
            }
            Action::CommandFinished(target, id, result) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(mut pending) = runtime.commands.finish(id) else {
                    return Task::none();
                };
                let provider = runtime.session.launch.provider;
                if result.is_ok()
                    && let Some(commands::Choice::Model { model, reasoning }) =
                        pending.choice.clone()
                {
                    self.remember_chat_model(provider, Some(model), reasoning);
                }
                if let Some(step) = pending.queued {
                    return Task::batch([
                        self.queued_step_finished(target, step, pending.first_prompt, result),
                        self.next_chat_command(target),
                    ]);
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let mut followups_sent = false;
                let mut due = Task::none();
                let first_prompt = match result {
                    Ok(()) => {
                        if !pending.followups.is_empty() {
                            let before = runtime.followups.len();
                            runtime
                                .followups
                                .retain(|followup| !pending.followups.contains(&followup.id));
                            followups_sent = runtime.followups.len() != before;
                            let _ = runtime.present();
                        }
                        if let Some(sent) = pending.sent.take() {
                            runtime.remember_with_attachments(
                                sent,
                                std::mem::take(&mut pending.sent_attachments),
                            );
                        }
                        if let Some(request) = &pending.request {
                            runtime.answers.retain(|(id, _), _| id != request);
                        }
                        runtime.refresh();
                        let snapshot = runtime.snapshot.clone();
                        runtime.commands.accept(&mut pending, &snapshot);
                        let items = runtime.presents();
                        runtime.settle_commands(&snapshot, items);
                        runtime.prepare_local_prompt();
                        if runtime.commands.turn_coming(&snapshot) {
                            due = commands_due(target);
                        }
                        if pending.page == Some(view::Page::Latest)
                            && runtime.deferred_page == Some(HistoryNavigation::Latest)
                        {
                            runtime.deferred_page = None;
                        }
                        if pending.page == Some(view::Page::Latest)
                            && runtime.selected_subagent.is_none()
                            && runtime.scroll_memory.edge(None) == Some(scroll::Edge::Latest)
                        {
                            let snap = scroll::snap_to_edge(
                                target,
                                scroll::Edge::Latest,
                                runtime.find_navigation.guard(),
                                runtime.find_navigation.document_epoch(),
                            )
                            .map(cosmic::Action::App);
                            if followups_sent {
                                self.persist();
                            }
                            return Task::batch([
                                snap,
                                due,
                                self.next_chat_command(target),
                                self.dispatch_queued(target),
                                self.resume_history_navigation(target),
                            ]);
                        }
                        pending.first_prompt
                    }
                    Err(error) => {
                        if pending.page == Some(view::Page::Latest) {
                            runtime.scroll_memory.clear_latest();
                            runtime.scroll_memory.cancel_preserve(pending.id);
                        }
                        if pending.page.is_some() {
                            runtime.page_request = None;
                            runtime.earlier_asked = None;
                            runtime.later_asked = None;
                        }
                        // A message the provider did not take goes back to
                        // the composer, and so does any written after it:
                        // sent without it, they would arrive out of order.
                        // Held follow-ups were written after it too, or are
                        // what it carried.
                        let unsent = pending.take_unsent();
                        if let Some(id) = &pending.take_back {
                            runtime.recalling.retain(|recall| &recall.message.id != id);
                        }
                        if let Some(outgoing) = unsent {
                            let mut messages = vec![outgoing];
                            messages.extend(runtime.commands.withdraw_messages());
                            runtime.take_back_lined_up(messages);
                            runtime.prepare_local_prompt();
                        } else if !pending.followups.is_empty() {
                            runtime.restore_followups();
                        }
                        runtime.note(error.clone());
                        runtime.fork_requested = false;
                        runtime.error = Some(error);
                        None
                    }
                };
                // Sent follow-ups were saved with the draft, so a restart
                // before the next save would offer them to send again.
                if followups_sent {
                    self.persist();
                }
                if let Some(prompt) = first_prompt {
                    self.name_channel_from_prompt(target, &prompt);
                }
                // Idle may have arrived while this command was awaiting its
                // acknowledgement. No later provider event is guaranteed to
                // wake the follow-up again once the command releases it.
                return Task::batch([
                    due,
                    self.next_chat_command(target),
                    self.dispatch_queued(target),
                    self.resume_history_navigation(target),
                ]);
            }
            Action::CommandsDue(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let snapshot = runtime.snapshot.clone();
                let items = runtime.presents();
                if runtime.settle_commands(&snapshot, items) {
                    runtime.prepare_local_prompt();
                }
                // A conversation still working reports again when it stops,
                // and its report settles the message then.
                return Task::batch([self.next_chat_command(target), self.dispatch_queued(target)]);
            }
            Action::Toggle(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.find_navigation.cancel();
                    let expanded = if let Some(child) = &runtime.selected_subagent {
                        if let Some(disclosure) = runtime.child_find_disclosure.get_mut(child) {
                            disclosure.manual_item(&id);
                        }
                        runtime.child_expanded.entry(child.clone()).or_default()
                    } else {
                        runtime.find_disclosure.manual_item(&id);
                        &mut runtime.expanded
                    };
                    let expanded = expanded.edit();
                    let open = !expanded.remove(&id);
                    if open {
                        expanded.insert(id.clone());
                    }
                    if runtime.selected_subagent.is_none() {
                        runtime.follow_lone_step(&id, open);
                    }
                    if runtime.active_item(&id).is_some_and(|item| {
                        matches!(
                            &item.content,
                            ItemContent::Delivery {
                                kind: chat::DeliveryKind::Report,
                                ..
                            }
                        )
                    }) {
                        let snapshot = runtime.snapshot.clone();
                        runtime.prepare_transcript_cache(&snapshot);
                        return Task::none();
                    }
                    if let Some(item) = runtime.active_item(&id).cloned().filter(|item| {
                        CachedItem::prose(&item.content).is_none()
                            || runtime
                                .presentation_cache()
                                .get(&id)
                                .is_some_and(|cached| cached.coordination.is_some())
                    }) {
                        // A collapsed item can still be awaiting authenticated
                        // bookmark-prefix verification. Opening it must not
                        // replace that owner with an unverified cold reader;
                        // completion installs using the current disclosure.
                        if runtime
                            .presentation_cache()
                            .get(&id)
                            .is_some_and(|cached| cached.preparing)
                        {
                            return Task::none();
                        }
                        let previous = runtime.presentation_cache_mut().remove(&id);
                        let cached = CachedItem::updated(
                            &item.content,
                            runtime.syntax_theme,
                            previous,
                            runtime.item_expanded(&id),
                            false,
                            None,
                        );
                        runtime.presentation_cache_mut().insert(id.clone(), cached);
                    }
                }
            }
            Action::ToggleTurn(target, key) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.find_navigation.cancel();
                    // Invert the visible default on the first toggle, then
                    // remember explicit choices across paging. Ignore stale keys.
                    if let Some(turn) = runtime.turns.iter().find(|turn| turn.key == key) {
                        let open = !runtime.group_open(turn);
                        runtime.find_disclosure.manual_turn(&key);
                        runtime.opened.edit().insert(key, open);
                    }
                }
            }
            Action::ToggleFailure(target, key) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime
                        .failure_ui
                        .incidents
                        .iter()
                        .any(|incident| incident.key == key)
                {
                    runtime.failure_ui.find_expanded.remove(&key);
                    let expanded = runtime.failure_ui.expanded.edit();
                    if !expanded.remove(&key) {
                        expanded.insert(key.clone());
                        runtime.cache_failure_preview(&key);
                    }
                }
            }
            Action::CopyFailure(target, key) => {
                if self
                    .chat_runtime_mut(target)
                    .is_some_and(|runtime| runtime.failure_originals_retained(&key).is_some())
                {
                    return self.start_failure_copy(target, key);
                }
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(text) = runtime.failure_details(&key)
                {
                    text_copy::invalidate();
                    return iced::clipboard::write(text).map(cosmic::Action::App);
                }
            }
            Action::FailureCopied(target, key, request, result) => {
                return self.finish_failure_copy(target, key, request, result);
            }
            Action::SubagentFailures(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    let mut picker = child_navigation::Picker::new(&runtime.snapshot.subagents);
                    picker.filter = child_navigation::Filter::Failed;
                    runtime.child_picker = Some(picker);
                    return widget::text_input::focus(card_control_id(
                        target.card,
                        "chat-subagent-search",
                    ))
                    .map(cosmic::Action::App);
                }
            }
            Action::SubagentPicker(target, open) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.child_picker =
                        open.then(|| child_navigation::Picker::new(&runtime.snapshot.subagents));
                    if open {
                        return widget::text_input::focus(card_control_id(
                            target.card,
                            "chat-subagent-search",
                        ))
                        .map(cosmic::Action::App);
                    }
                    return iced::widget::operation::focus(card_control_id(
                        target.card,
                        if runtime.snapshot.subagents.is_empty() {
                            "chat-composer"
                        } else {
                            "chat-subagent-picker"
                        },
                    ))
                    .map(cosmic::Action::App);
                }
            }
            Action::SubagentQuery(target, query) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(picker) = runtime.child_picker.as_mut()
                {
                    picker.query = query;
                    picker.page = 0;
                    return iced::widget::scrollable::scroll_to(
                        card_control_id(target.card, "chat-subagent-list"),
                        iced::widget::scrollable::AbsoluteOffset {
                            x: Some(0.0),
                            y: Some(0.0),
                        },
                    )
                    .map(cosmic::Action::App);
                }
            }
            Action::SubagentPage(target, next) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(picker) = runtime.child_picker.as_mut()
                {
                    let last = picker
                        .rows(&runtime.snapshot.subagents, &runtime.child_ordinals)
                        .len()
                        .saturating_sub(1)
                        / 24;
                    picker.page = if next {
                        picker.page.saturating_add(1).min(last)
                    } else {
                        picker.page.saturating_sub(1)
                    };
                    return iced::widget::scrollable::scroll_to(
                        card_control_id(target.card, "chat-subagent-list"),
                        iced::widget::scrollable::AbsoluteOffset {
                            x: Some(0.0),
                            y: Some(0.0),
                        },
                    )
                    .map(cosmic::Action::App);
                }
            }
            Action::SubagentFilter(target, filter) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(picker) = runtime.child_picker.as_mut()
                {
                    picker.filter = filter;
                    picker.page = 0;
                    return iced::widget::scrollable::scroll_to(
                        card_control_id(target.card, "chat-subagent-list"),
                        iced::widget::scrollable::AbsoluteOffset {
                            x: Some(0.0),
                            y: Some(0.0),
                        },
                    )
                    .map(cosmic::Action::App);
                }
            }
            Action::SelectSubagent(target, id) => {
                text_copy::invalidate();
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.find_navigation.cancel_all();
                    runtime.child_picker = None;
                    let selected = id.filter(|id| {
                        runtime
                            .snapshot
                            .subagents
                            .iter()
                            .any(|agent| &agent.id == id)
                    });
                    if selected != runtime.selected_subagent {
                        runtime.restore_find_disclosures();
                        runtime.find_matches.clear();
                        runtime.find_index = 0;
                        runtime.selected_subagent = selected;
                        runtime.child_cache.clear();
                        runtime.child_cache_context = None;
                        // Selection is a presentation change, including when
                        // the current publication/Arc has not changed.
                        if runtime.visible.load(Ordering::Relaxed) {
                            let snapshot = runtime.snapshot.clone();
                            runtime.prepare_child_cache(&snapshot);
                        }
                        runtime.refresh_find();
                    }
                    runtime.deferred_page = None;
                    runtime.selection_document.get_mut().take();
                    let snapshot = runtime.snapshot.clone();
                    runtime.prepare_transcript_cache(&snapshot);
                    runtime.refresh_find();
                    images::sync(runtime);
                }
            }
            Action::CopyMessage(target, id, source) => {
                return self.start_message_copy(target, id, source, String::new());
            }
            Action::CopyStep(target, id, part, source) => {
                let prefix = self
                    .chat_runtime_mut(target)
                    .and_then(|runtime| runtime.active_item(&id))
                    .and_then(|item| step_copy_prefix(&part, &item.content));
                if let Some(prefix) = prefix {
                    return self.start_message_copy(target, id, Some(source), prefix);
                }
            }
            Action::MessageCopied(target, id, request, result) => {
                return self.finish_message_copy(target, id, request, result);
            }
            Action::LogicalProseRead(target, source, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && runtime.logical_prose.complete(&source, result)
                    && runtime.visible.load(Ordering::Relaxed)
                    && let Some(raw) = runtime.logical_prose.raw()
                {
                    let clock = runtime.source_clock;
                    runtime.install_snapshot(raw, clock);
                }
            }
            Action::Copy(target, id, part) => {
                text_copy::invalidate();
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                // The body itself lives in the snapshot; only the extracted
                // code blocks are the cache's own.
                let text = runtime.active_item(&id).and_then(|item| {
                    let card = runtime
                        .presentation_cache()
                        .get(&item.id)
                        .and_then(|cached| cached.coordination.as_deref())
                        .filter(|coordinated| coordinated.is_card());
                    match card {
                        // A message read from a file copies as the
                        // command that sent it.
                        Some(card) if matches!(part, CopyPart::Text) => {
                            card.copy_text().or_else(|| copy_text(part, &item.content))
                        }
                        _ => copy_text(part, &item.content),
                    }
                });
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
            Action::Unfold(target, key) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.find_navigation.cancel();
                    let unfolded = if let Some(child) = &runtime.selected_subagent {
                        if let Some(disclosure) = runtime.child_find_disclosure.get_mut(child) {
                            disclosure.manual_part(&key);
                        }
                        runtime.child_unfolded.entry(child.clone()).or_default()
                    } else {
                        runtime.find_disclosure.manual_part(&key);
                        &mut runtime.unfolded
                    };
                    if !unfolded.remove(&key) {
                        unfolded.insert(key);
                    }
                }
            }
            Action::OpenSession(prefix) => {
                return self.open_session_link(prefix);
            }
            Action::OpenAgent(prefix) => {
                return self.open_agent_tab(&prefix);
            }
            Action::SessionResolved(result) => {
                return self.session_link_resolved(result);
            }
            Action::SessionLinkWindowBackend(slot, session, wayland) => {
                return self.session_window_backend(slot, session, wayland);
            }
            Action::SessionLinkToken(slot, session, token) => {
                return session_links::handoff_session_window(slot, session, token);
            }
            Action::SessionLinkOpened(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::warning(error));
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
            Action::OpenChangedFile(target, path) => {
                return self.chat_open_changed_file(target, path);
            }
            Action::ChangedFileResolved(target, id, reported, result) => {
                return self.chat_changed_file_resolved(target, id, reported, result);
            }
            Action::OpenDocument(target, raw) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    let document = runtime.document_link_origin();
                    return self.open_document_link_from(
                        Some(target.card),
                        document.as_deref(),
                        &raw,
                    );
                }
            }
            Action::OpenDocumentFrom(target, document, raw) => {
                if self.chat_runtime_mut(target).is_some() {
                    return self.open_document_link_from(
                        Some(target.card),
                        document.as_deref(),
                        &raw,
                    );
                }
            }
            Action::OpenSharedUrl(target, url) => {
                if self.chat_runtime_mut(target).is_some() {
                    return self.handle_chat_ui(Action::OpenUrl(url.to_string()));
                }
            }
            Action::OpenSharedDocument(target, raw) => {
                return self.handle_chat_ui(Action::OpenDocument(target, raw.to_string()));
            }
            Action::LoadImage(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    images::request(runtime, &id);
                }
            }
            Action::LoadFigure(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    return images::figure::load(runtime, id);
                }
            }
            Action::FigureLoaded(target, id, attempt, outcome) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    return images::figure::complete(runtime, id, attempt, outcome);
                }
            }
            Action::RecheckFigure(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    images::figure::recheck(runtime, id);
                }
            }
            Action::FigureOutOfSight => {}
            Action::Figure(target, id, command) => {
                return self.figure_command(target, id, command);
            }
            Action::RetryThumbnails(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.thumbnails.retry_deferred();
                }
            }
            Action::ThumbnailLoaded(target, path, loaded) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.thumbnails.complete(&path, loaded);
                }
            }
            Action::ImageLoaded(target, item, attempt, loaded) => {
                let explicit_pressure = self
                    .chat_runtime_mut(target)
                    .and_then(|runtime| images::complete(runtime, &item, attempt, loaded));
                if let Some(mut missing) = explicit_pressure {
                    // Explicit reader interest outranks automatic previews in
                    // other panes too. Their live widget/gallery clones keep
                    // their leases until redraw or close actually retires them.
                    for card in self.cards.values_mut() {
                        if let CardRuntime::Chat(runtime) = card
                            && runtime.target != target
                        {
                            missing = images::evict_ready(runtime, missing);
                            if missing == 0 {
                                break;
                            }
                        }
                    }
                }
            }
            Action::OpenImage(..)
            | Action::FocusImage(_)
            | Action::CloseImage(_)
            | Action::ImageNext(..)
            | Action::ImageFit(_)
            | Action::ImageExternal(_) => {
                return self.image_viewer_action(&action).unwrap_or_else(Task::none);
            }
            Action::Image(image, command) => return self.image_command(image, command),
            // A superseded copy reports nothing, not even its failure.
            Action::ImageCopied(intent, _) if !intent.is_current() => {}
            Action::ImageCopied(intent, result) => match result {
                Ok(Some(png)) => {
                    self.status = Some(TransientNotice::success("Copied image"));
                    return intent.write_data(png);
                }
                Ok(None) => {}
                Err(error) => {
                    self.status = Some(
                        TransientNotice::error("Could not copy the image").with_details(error),
                    );
                }
            },
            Action::DismissRestoredWaiting(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.restored_waiting = None;
                }
            }
            Action::CopyQueued(target, source) => {
                if self
                    .card_state(target.card)
                    .is_none_or(|state| !state.existence.is_open())
                {
                    return Task::none();
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let text: Option<Arc<str>> = match source {
                    QueueCopy::Initial(source) => runtime
                        .local_prompt_by_source(&source)
                        .map(|prompt| Arc::from(prompt.text())),
                    QueueCopy::Provider(source, id) => {
                        let current = runtime
                            .provider_queue
                            .borrow_mut()
                            .get(&runtime.snapshot, &id);
                        if current
                            .as_ref()
                            .is_none_or(|current| source.as_ptr() != Arc::as_ptr(current))
                        {
                            return Task::none();
                        }
                        current.map(|current| Arc::from(current.source.text()))
                    }
                    QueueCopy::Local(id) => runtime
                        .followups
                        .iter()
                        .find(|message| message.id == id)
                        .map(|message| Arc::from(message.text.as_str())),
                };
                let Some(text) = text else {
                    return Task::none();
                };
                self.status = Some(TransientNotice::success("Copied queued message"));
                return text_copy::write_shared_literal(text);
            }
            Action::CopySharedCode(target, text) => {
                if self.chat_runtime_mut(target).is_none()
                    || self
                        .card_state(target.card)
                        .is_none_or(|state| !state.existence.is_open())
                {
                    return Task::none();
                }
                self.status = Some(TransientNotice::success("Copied code block"));
                return text_copy::write_shared_literal(text);
            }
            Action::CopyLiteral(text) => {
                text_copy::invalidate();
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
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                runtime.find_navigation.cancel();
                if runtime.selected_subagent.is_none() {
                    if !runtime.commands.is_idle() {
                        runtime
                            .scroll_memory
                            .request_edge(None, scroll::Edge::Latest);
                        if !runtime
                            .commands
                            .iter()
                            .any(|pending| pending.page == Some(view::Page::Latest))
                        {
                            runtime.deferred_page = Some(HistoryNavigation::Latest);
                        }
                        return Task::none();
                    }
                    // The ACK can settle before newest history arrives. A
                    // repeated explicit Latest still supersedes later Home,
                    // while reusing the already accepted provider request.
                    if runtime
                        .page_request
                        .is_some_and(|(page, _)| page == view::Page::Latest)
                    {
                        runtime
                            .scroll_memory
                            .request_edge(None, scroll::Edge::Latest);
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
                runtime
                    .scroll_memory
                    .request_edge(runtime.selected_subagent.as_deref(), scroll::Edge::Latest);
                return scroll::snap_to_edge(
                    target,
                    scroll::Edge::Latest,
                    runtime.find_navigation.guard(),
                    runtime.find_navigation.document_epoch(),
                )
                .map(cosmic::Action::App);
            }
            Action::Attach(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime.accepts_draft_attachments()
                    || runtime.attachment_chooser_pending.is_some()
                {
                    return Task::none();
                }
                if runtime.pastes_pending >= 2 {
                    runtime.error = Some(
                        "Two attachments are already loading. Wait for them before attaching more."
                            .into(),
                    );
                    return Task::none();
                }
                let Some(id) = runtime.try_attachment_insert() else {
                    return Task::none();
                };
                runtime.attachment_chooser_pending = Some(id.id);
                // The chooser answers the `+` menu's choice; the menu has
                // nothing left to offer while it is open.
                runtime.composer_actions = false;
                runtime.branch_picker = false;
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
                    Message::ChatUi(Action::Attached(target, id, result))
                });
            }
            Action::Attached(target, id, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if runtime.attachment_chooser_pending != Some(id.id) {
                        return Task::none();
                    }
                    runtime.attachment_chooser_pending = None;
                    let staged = if !runtime.attachment_bookmarks.contains_key(&id.id) {
                        Task::none()
                    } else {
                        match result {
                            Ok(Some(paths)) => self.stage_chat_attachments(target, id, paths),
                            Ok(None) => {
                                runtime.cancel_attachment_insert(id.id, None);
                                Task::none()
                            }
                            Err(error) => {
                                runtime.cancel_attachment_insert(id.id, Some(error));
                                Task::none()
                            }
                        }
                    };
                    return Task::batch([staged, self.adopt_stored_draft_session(target.card)]);
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
                let mut staged = None;
                let dropped = match payload.decode() {
                    file_drop::Dropped::Items(items) if runtime.accepts_draft_attachments() => {
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
                        if !files.is_empty() {
                            if runtime.pastes_pending >= 2 {
                                runtime.error = Some("Two attachments are already loading. Dropped files were inserted as paths; wait before attaching more.".into());
                                rest.extend(files.into_iter().map(file_drop::Item::File));
                            } else if let Some((id, lease)) = attachment_draft::admit() {
                                staged = Some((id, files, lease));
                            } else {
                                runtime.error = Some("Two attachments are already loading. Dropped files were inserted as paths; wait before attaching more.".into());
                                rest.extend(files.into_iter().map(file_drop::Item::File));
                            }
                        }
                        file_drop::Dropped::Items(rest)
                    }
                    dropped => dropped,
                };
                let insertion = dropped.insertion(file_drop::Spelling::Plain);
                if let Some(refusal) = insertion.refusal() {
                    self.status = Some(TransientNotice::warning(refusal));
                }
                let has_dropped_text = !insertion.text.is_empty();
                let edited = if !has_dropped_text {
                    Task::none()
                } else {
                    self.handle_chat_ui(Action::Composer(
                        target,
                        text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                            insertion.text,
                        ))),
                    ))
                };
                return Task::batch([
                    edited,
                    staged.map_or_else(Task::none, |(id, files, lease)| {
                        let Some(runtime) = self.chat_runtime_mut(target) else {
                            return Task::none();
                        };
                        // The same synchronous drop has already inserted its
                        // prose. Capture its resulting caret before any IO.
                        runtime.attachment_bookmarks.insert(
                            id.id,
                            attachment_draft::Bookmark::capture(&runtime.composer),
                        );
                        runtime.attachment_jobs.insert(id.id, lease);
                        runtime.attachment_order.push(id.id);
                        runtime.pastes_pending += 1;
                        self.stage_chat_attachments(target, id, files)
                    }),
                    cosmic::task::message(Message::FocusCard(target.card)),
                ]);
            }
            Action::RemoveAttachment(target, index) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && index < runtime.attachments.len()
                {
                    let token = runtime.attachments[index]
                        .reference
                        .as_ref()
                        .map(chat::ImageReference::token);
                    if let Some(token) = token
                        && !runtime.remove_image_token(&token)
                    {
                        return Task::none();
                    }
                    runtime.attachments.remove(index);
                    runtime.thumbnails.sync(&runtime.attachments);
                    runtime.revision = runtime.revision.wrapping_add(1);
                    runtime.settle_restored_notice();
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
                    .chosen_model()
                    .reasoning
                    .map(str::to_owned)
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
                runtime.model_picker = None;
                let Some(model) = runtime
                    .chosen_model()
                    .model(&runtime.snapshot)
                    .filter(|model| model.id == model_id)
                else {
                    return Task::none();
                };
                if reasoning
                    .as_ref()
                    .is_some_and(|effort| !model.reasoning.contains(effort))
                {
                    return Task::none();
                }
                let model = model.id.clone();
                let provider = runtime.session.launch.provider;
                if runtime.is_draft() {
                    if !runtime.choosing() {
                        return Task::none();
                    }
                    let snapshot = Arc::make_mut(&mut runtime.snapshot);
                    snapshot.selected_model = Some(model.clone());
                    snapshot.reasoning = reasoning.clone();
                    self.remember_chat_model(provider, Some(model), reasoning);
                    return Task::none();
                }
                let command = Command::SetModel { model, reasoning };
                return self.chat_send(target, command, false);
            }
            Action::PermissionMode(target, mode) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                runtime.permission_picker = false;
                runtime.composer_actions = false;
                if !runtime
                    .snapshot
                    .capabilities
                    .permission_modes
                    .contains(&mode)
                {
                    return Task::none();
                }
                // Leaving plan mode from the `+` menu returns to the mode it
                // replaced, whichever control entered it.
                if mode == PLAN_MODE
                    && let Some(current) = runtime
                        .chosen_permission_mode()
                        .filter(|current| *current != PLAN_MODE)
                        .map(str::to_owned)
                {
                    runtime.mode_before_plan = Some(current);
                }
                if runtime.is_draft() {
                    if !runtime.choosing() {
                        return Task::none();
                    }
                    let provider = runtime.session.launch.provider;
                    let before = runtime.session.launch.recipe.clone();
                    set_draft_permission_mode(&mut runtime.session.launch.recipe, provider, &mode);
                    if runtime.session.launch.recipe != before {
                        // A different launch recipe was never refused.
                        runtime.release_start_block();
                    }
                    Arc::make_mut(&mut runtime.snapshot).permission_mode = Some(mode);
                    let session = runtime.session.clone();
                    self.replace_chat_conversation(target.card, session);
                    // Skills found for the earlier launch are not accepted for
                    // this one, so a discovery under way would be dropped.
                    return self.load_draft_skills(target);
                }
                return self.chat_send(target, Command::SetPermissionMode { mode }, false);
            }
            Action::Field(target, request, field, value) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(schema) = runtime
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
            Action::Choice(target, request, field, value) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Some(schema) = runtime
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
                    && schema.choices.iter().any(|choice| choice.id == value)
                {
                    let values = runtime.answers.entry((request, field)).or_default();
                    if schema.multiple {
                        if let Some(index) = values.iter().position(|selected| selected == &value) {
                            values.remove(index);
                        } else {
                            values.push(value);
                        }
                    } else {
                        *values = vec![value];
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
            Action::AcceptSuggestion(target, suggestion) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    runtime.accept_suggestion(&suggestion);
                }
                return Task::none();
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
                let (row, viewport, inset) = view::completion_extent(count);
                if let Some(offset) = runtime.completion.step(down, count, row, viewport, inset) {
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
                    runtime.session.terminal = Some(Box::new(terminal.clone()));
                }
                let session = runtime.session.clone();
                self.commit_chat_session(target.card, session);
                if preserve
                    && let Some(CardRuntime::Chat(mut runtime)) = self.cards.remove(&target.card)
                {
                    runtime.show(false);
                    runtime.parked_queue = snapshot
                        .queued_messages
                        .iter()
                        .map(|message| message.id.clone())
                        .collect();
                    self.chat_ui.parked.insert(target.card, runtime);
                    self.trim_warm_chats();
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
                        self.continue_card_close(target.card, false)
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
                    return self.refuse_chat_close(target.card, session, error);
                }
                return self
                    .remove_chat_card(target.card, None)
                    .unwrap_or_else(Task::none);
            }
            Action::AskCloseHolders(target, session) => {
                return self.ask_close_holders(target, session);
            }
            Action::CloseHolders(target, session, result) => {
                return self.show_close_holders(target, session, result);
            }
            Action::EndCloseHolders(target) => return self.end_close_holders(target),
            Action::CloseHoldersEnded(target, session, result) => {
                return self.close_holders_ended(target, session, result);
            }
            Action::DismissCloseRefusal(target) => return self.dismiss_close_refusal(target),
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
                    return self.continue_card_close(target.card, false);
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
            Action::Scrolled(target, subagent, scroll, direction) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.selected_subagent != subagent {
                    return Task::none();
                }
                if direction.is_some() {
                    runtime.find_navigation.cancel();
                }
                if let Some(id) = subagent {
                    runtime.subagent_scroll.insert(id, scroll);
                    return Task::none();
                }
                runtime.scroll = scroll;
                if scroll.at_bottom() && !runtime.snapshot.has_later {
                    runtime.unseen_activity = false;
                }
                // Pages are fetched ahead of the reader, within a couple of
                // screens of a loaded edge rather than at it.
                let (at_top, at_bottom) = (scroll.prefetch_earlier(), scroll.prefetch_later());
                // Leaving that reach is what arms the page at it, so a reader
                // who scrolls away and comes back asks again, while one who
                // stays put asks once.
                if !at_top || direction == Some(view::Page::Newer) {
                    if runtime.deferred_page == Some(HistoryNavigation::EarlierEdge) {
                        runtime.deferred_page = None;
                    }
                    runtime.earlier_asked = None;
                }
                if !at_bottom || direction == Some(view::Page::Earlier) {
                    if runtime.deferred_page == Some(HistoryNavigation::NewerEdge) {
                        runtime.deferred_page = None;
                    }
                    runtime.later_asked = None;
                }
                let height = Some(scroll.content());
                // A byte-bounded page can contain only collapsed tool output.
                // It fits at both edges, so the gesture chooses the next page;
                // layout and provider updates never start another fetch.
                if direction == Some(view::Page::Earlier)
                    && at_top
                    && runtime.snapshot.has_earlier
                    && runtime.earlier_asked != height
                {
                    if !runtime.commands.is_idle() {
                        if runtime
                            .commands
                            .iter()
                            .any(|pending| pending.page.is_none())
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
                if direction == Some(view::Page::Newer)
                    && at_bottom
                    && runtime.snapshot.has_later
                    && runtime.later_asked != height
                {
                    if !runtime.commands.is_idle() {
                        if runtime.deferred_page.is_none() {
                            runtime.deferred_page = Some(HistoryNavigation::NewerEdge);
                        }
                        return Task::none();
                    }
                    return self.chat_send(target, Command::LoadLater, false);
                }
            }
            Action::ModelPicker(target, open) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                runtime.composer_actions = false;
                runtime.branch_picker = false;
                runtime.picker_keyboard = false;
                // A reopened search starts empty: the query a reader typed
                // last time is not the model they are looking for now.
                runtime.model_picker = open.then(String::new);
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
        }
        Task::none()
    }
}

/// The permission mode draft `card` shows in its footer, and the launch its
/// first message would start.
#[cfg(test)]
pub(super) fn draft_launch_for_test(
    app: &Cagents,
    card: CardId,
) -> (Option<String>, chat::Session) {
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("a draft has a chat runtime");
    };
    assert!(runtime.choosing(), "the draft can still change its options");
    (
        runtime.snapshot.permission_mode.clone(),
        runtime.session.clone(),
    )
}

/// Opens a prepared live chat on `card` whose `turns` answers each link to
/// three pull requests, from #1000 up in `acme/widgets`, shown or not.
#[cfg(test)]
pub(super) fn open_linked_chat_for_test(
    app: &mut Cagents,
    card: CardId,
    turns: usize,
    shown: bool,
) {
    let mut runtime = view::linked_runtime_for_test(turns);
    runtime.target.card = card;
    runtime.visible.store(shown, Ordering::Relaxed);
    app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
}

/// Runs every chat's queued preparation and installs what it prepared.
#[cfg(test)]
pub(super) fn drain_preparation_for_test(app: &mut Cagents) {
    for runtime in app.cards.values_mut() {
        if let CardRuntime::Chat(runtime) = runtime {
            preparation::drain_fixture(runtime);
        }
    }
}

/// The reason draft `card` last failed to start, which it shows.
#[cfg(test)]
pub(super) fn draft_error_for_test(app: &mut Cagents, card: CardId) -> &mut Option<String> {
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("a draft has a chat runtime");
    };
    &mut runtime.error
}

/// Runs `during` while draft `card`'s file chooser is open, then dismisses the
/// chooser as its dialog would.
#[cfg(test)]
pub(super) fn with_attachment_chooser_open_for_test(
    app: &mut Cagents,
    card: CardId,
    during: impl FnOnce(&mut Cagents),
) {
    let _admission = attachment_draft::test_admission();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("a draft has a chat runtime");
    };
    let target = runtime.target;
    let ticket = runtime.begin_attachment_insert();
    runtime.attachment_chooser_pending = Some(ticket.id);
    during(app);
    drop(app.update(Message::ChatUi(Action::Attached(target, ticket, Ok(None)))));
}

/// Sends draft `card`'s first message through its save, returning the launch it starts.
#[cfg(test)]
pub(super) fn start_draft_for_test(app: &mut Cagents, card: CardId) -> chat::Session {
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("a draft has a chat runtime");
    };
    let target = runtime.target;
    let (target, attempt, result) =
        first_start_checkpoint_tests::checkpoint(app.start_chat_draft(target));
    drop(app.handle_chat_ui(Action::DraftStartCheckpointed(target, attempt, result)));
    let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
        panic!("a starting draft keeps its chat runtime");
    };
    runtime
        .queued
        .as_ref()
        .expect("the first message waits for its conversation")
        .draft
        .clone()
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
pub(super) fn chat_error_for_test(app: &Cagents, card: CardId) -> Option<&str> {
    match app.cards.get(&card) {
        Some(CardRuntime::Chat(runtime)) => runtime.error.as_deref(),
        _ => None,
    }
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
    let items = snapshot.items.len();
    let child_items: Vec<_> = snapshot
        .subagents
        .iter()
        .map(|agent| (agent.id.clone(), agent.items.len()))
        .collect();
    let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
        panic!("chat snapshot requires a chat runtime");
    };
    // A hidden client intentionally discards incoming bodies. Establish the
    // real presentation interest before publishing this visible fixture.
    runtime.show(true);
    runtime
        .handle()
        .expect("a chat snapshot is shown on a live conversation")
        .replace_snapshot_for_test(snapshot);
    runtime.show(true);
    preparation::drain_fixture(runtime);
    assert_eq!(
        runtime.snapshot.items.len(),
        items,
        "visible fixtures must retain their published parent body"
    );
    assert_eq!(
        runtime
            .snapshot
            .subagents
            .iter()
            .map(|agent| (agent.id.clone(), agent.items.len()))
            .collect::<Vec<_>>(),
        child_items,
        "visible fixtures must retain their published child bodies"
    );
    assert_eq!(
        runtime.snapshot.requests.len(),
        requests,
        "the drawn transcript has to be the snapshot just installed"
    );
    requests
}

#[cfg(test)]
pub(in crate::app) mod tests;

#[cfg(test)]
#[path = "chat_ui/first_start_checkpoint_tests.rs"]
mod first_start_checkpoint_tests;

#[cfg(test)]
#[path = "chat_ui/absence_tests.rs"]
mod absence_tests;

#[cfg(test)]
#[path = "chat_ui/peer_draft_tests.rs"]
mod peer_draft_tests;

#[cfg(test)]
#[path = "chat_ui/startup_restore_tests.rs"]
mod startup_restore_tests;
