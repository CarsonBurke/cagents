//! What a daemon admits while startup recovery holds original ownership.
//!
//! The hold exists so that nothing races a retained original whose commit
//! outcome is unknown: no second writer for one of its conversations or
//! terminals, and no work resubmitted on its behalf. That is a property of the
//! originals, not of the daemon. A request that can be shown to touch none of
//! them is admitted: reads, the explicit Close the hold offers, and work that
//! starts fresh beside the originals. Everything that names or could reuse a
//! retained identity keeps the hold's refusal; a client tells it apart from
//! other refusals by asking [`ClientRequest::StartupState`], which the hold
//! always answers.
//!
//! What recovery holds is every identity its restore could still adopt or has
//! adopted: the conversations and terminals the update's checkpoint names
//! (with their provider conversations), whatever this daemon hosts that it
//! did not start during the hold, and any allocation an authenticated holder
//! still owns. A conversation or terminal started during the hold is recorded
//! as independent, so it can send, read, close and reopen like any other.
use super::state::{HostState, SharedHost};
use crate::{
    agent::Provider,
    agent_execution::Operation as AgentOperation,
    chat::Session,
    device_service::{GitOperation, Operation as DeviceOperation},
    model::{TerminalId, TerminalSession},
    terminal_ipc::{ClientRequest, ProtocolError, ProtocolErrorCode},
    workspace_model::ModelEdit,
};

/// How a request relates to the originals recovery holds.
enum Access<'a> {
    /// Reads, presentation private to the connection, or state no original
    /// depends on.
    Open,
    /// The explicit, exact Close of an original, which is how the hold is
    /// meant to be resolved by hand.
    Close(Option<TerminalId>),
    /// Ordinary shutdown, which the hold admits only once no original owns work.
    Shutdown,
    /// A conversation start; admitted only when it is shown to be fresh.
    StartChat(&'a Session),
    /// A resume of an earlier conversation, admitted only for one started
    /// during the hold.
    PrepareResume(TerminalId),
    /// Moves a conversation started during the hold to another account.
    RecoverAccount {
        source: TerminalId,
        replacement: &'a Session,
    },
    /// A new terminal; admitted only when it is shown to be fresh.
    Spawn(&'a TerminalSession),
    /// Acts on one hosted conversation or terminal.
    Target(TerminalId),
    /// Names, could reuse, or changes what an original depends on.
    Held,
}

/// Classified without a fallback arm, so every new request has to choose.
fn access(request: &ClientRequest) -> Access<'_> {
    use ClientRequest as R;
    match request {
        R::Connect { .. }
        | R::Authenticate { .. }
        | R::BuildInfo
        | R::StartupState
        | R::List
        | R::ListPage { .. }
        | R::ChatList
        | R::Attach { .. }
        | R::Detach { .. }
        | R::Snapshot { .. }
        | R::TerminalAcknowledge { .. }
        | R::AcknowledgeRedraw { .. }
        | R::SearchTerminal { .. }
        | R::CopyTerminalSelection { .. }
        | R::WatchTerminals
        | R::WatchWorkspaceModel
        | R::ReadWorkspaceModel
        | R::ReadWorkspaceView { .. }
        | R::ReadWorkspaceSnapshot
        | R::ReadWorkspaceMetadata { .. }
        | R::ReadTerminalRemovalBasis { .. }
        | R::ReadCapturedTerminalRemovalBasis { .. }
        | R::ChatCapacityInspect
        | R::ChatReleaseInspect { .. }
        | R::ChatCandidates { .. }
        | R::UpgradeImpact { .. }
        | R::ObserveReviewRequests { .. }
        | R::WatchNotifications { .. }
        | R::NotificationPresence { .. }
        | R::ReadNotifications
        | R::NotificationPreferencesChanged { .. }
        | R::AcknowledgeNotification { .. }
        | R::SendTestNotification
        | R::WatchWork
        | R::WatchConversationPullRequests { .. }
        | R::Work(
            crate::work::Request::Board { .. }
            | crate::work::Request::RemovalPlan { .. }
            | crate::work::Request::Snapshots { .. }
            | crate::work::Request::Pin { .. },
        )
        // Admitted only so its handler can explain that it was removed.
        | R::RetiredChannelWork
        // Inspects what holds a lost helper's Close, ending only processes
        // the person chose and the daemon proves are that conversation's.
        | R::ChatLostProcesses { .. } => Access::Open,
        R::ApplyWorkspaceEdit { edit } | R::EditWorkspaceModel { edit } => model_access(edit),
        // Closing an original the reader is still attached to, as when the
        // hold is raised while it runs, is the Close the refusal offers.
        R::ChatClose { chat_id } => Access::Close(Some(*chat_id)),
        R::CloseTerminalChecked { terminal_id, .. } => Access::Close(Some(*terminal_id)),
        R::CloseTerminalSource { session, .. } => Access::Close(Some(session.id)),
        R::ChatCloseRetained { session } => Access::Close(Some(session.id)),
        R::Shutdown | R::ShutdownV1 { .. } => Access::Shutdown,
        R::ChatStart {
            session,
            source: None,
            terminate_source: false,
            ..
        }
        | R::ChatStartInPane { session, .. } => Access::StartChat(session),
        R::ChatPrepareResume { source } => Access::PrepareResume(source.id),
        R::ChatRecoverAccount {
            source,
            replacement,
            ..
        } => Access::RecoverAccount {
            source: source.id,
            replacement,
        },
        R::Spawn { session, .. } => Access::Spawn(session),
        R::ChatAttach { session } => Access::Target(session.id),
        R::ChatDetach { chat_id }
        | R::ChatCommand { chat_id, .. }
        | R::ChatVisibility { chat_id, .. }
        | R::ChatRead { chat_id, .. }
        | R::ChatAcknowledge { chat_id, .. }
        | R::ChatReturnTerminal { chat_id } => Access::Target(*chat_id),
        R::ChatRelease { proof, .. } => Access::Target(proof.session.id),
        R::ChatCommitTerminal { terminal_id }
        | R::Input { terminal_id, .. }
        | R::Paste { terminal_id, .. }
        | R::Key { terminal_id, .. }
        | R::Mouse { terminal_id, .. }
        | R::Focus { terminal_id, .. }
        | R::Scroll { terminal_id, .. }
        | R::Resize { terminal_id, .. }
        | R::SetVisibility { terminal_id, .. }
        | R::Hibernate { terminal_id }
        | R::Resume { terminal_id, .. }
        | R::SetTheme { terminal_id, .. }
        | R::ClearTerminalScrollback { terminal_id }
        | R::ShutdownTerminal { terminal_id }
        | R::RefreshAgentActivity { terminal_id } => Access::Target(*terminal_id),
        R::ReleaseCompletedTerminal { session, .. } => Access::Target(session.id),
        R::Agent { operation, .. } => agent_access(operation),
        R::Automation { operation, .. } => automation_access(operation),
        R::DeviceOperation { operation } => device_access(operation),
        // A start from a terminal names that source, and the rest carry the
        // update's or a pending start's own authority.
        R::ChatStart { .. }
        | R::ChatPreview { .. }
        | R::ChatStartSelected { .. }
        | R::ResolveUpgradeChat { .. }
        | R::CompletePendingTerminal { .. }
        | R::RetryPendingTerminalSave { .. }
        | R::PrepareUpgrade { .. }
        | R::PrepareConfirmedUpgrade { .. }
        | R::ChatCapacitySet { .. }
        | R::Work(crate::work::Request::Remove(_))
        // Recovery may still adopt an original that is not resident yet, and
        // a window prunes the transcripts this leaves out.
        | R::ListResidentTerminals => Access::Held,
    }
}

/// Edits of the shared model start, attach and close nothing. Replacing the
/// source of a pane whose conversation still runs, or removing a terminal pane
/// whose worker lives, is refused by the edit's own ownership checks, and
/// Close itself stays on its explicit requests. Deleting a channel or closing
/// a project removes many panes at once with no such check, among them those
/// of originals the restore never reached, which are the reader's way back to
/// them; those wait for the hold to end.
fn model_access(edit: &ModelEdit) -> Access<'static> {
    match edit {
        ModelEdit::OpenProject { .. }
        | ModelEdit::ReopenProject { .. }
        | ModelEdit::CreateChannel { .. }
        | ModelEdit::ResetNavigation { .. }
        | ModelEdit::ReorderNavigation { .. }
        | ModelEdit::RetitleChannel { .. }
        | ModelEdit::SuggestChannelTitle { .. }
        | ModelEdit::SeedView { .. }
        | ModelEdit::ChangeView { .. }
        | ModelEdit::SuggestConversationTitle { .. }
        | ModelEdit::RetiredChannelWorkChanged
        // Only a channel whose view holds no pane.
        | ModelEdit::RetireEmptyChannel { .. } => Access::Open,
        ModelEdit::CloseProject { .. }
        | ModelEdit::CloseConfirmedProject { .. }
        | ModelEdit::DeleteChannel { .. }
        | ModelEdit::AgentsChanged => Access::Held,
    }
}

/// Reads, and the caller's own mailbox, are open. Anything that prompts,
/// controls or closes an agent can reach a retained conversation.
fn agent_access(operation: &AgentOperation) -> Access<'static> {
    use AgentOperation as O;
    match operation {
        O::Status(_)
        | O::Run(_)
        | O::List
        | O::Directory(_)
        | O::AccountInfo(_)
        | O::Requests(_)
        | O::Inbox(_)
        | O::PeekInbox(_)
        | O::AcknowledgeInbox(_)
        | O::Transcript(_)
        | O::Members(_)
        | O::Sessions(_)
        | O::Context { .. } => Access::Open,
        O::Submit(_)
        | O::Close(_)
        | O::Console { .. }
        | O::Control { .. }
        | O::SendMessage(_)
        | O::Send(_)
        | O::Prompt { .. }
        | O::Deliver(_) => Access::Held,
    }
}

/// Reading and changing definitions and proposals is open; running one, or
/// cancelling a run, acts on agents.
fn automation_access(request: &crate::automations::Request) -> Access<'static> {
    use crate::automations::Request as R;
    match request {
        R::List { .. }
        | R::Show { .. }
        | R::Create { .. }
        | R::Edit { .. }
        | R::SetEnabled { .. }
        | R::Remove { .. }
        | R::History { .. }
        | R::ShowFiring { .. }
        | R::ShowRun { .. }
        | R::Propose { .. }
        | R::Proposal { .. }
        | R::PendingProposals { .. }
        | R::ResolveProposal { .. }
        | R::Audit { .. } => Access::Open,
        R::RunNow { .. } | R::Cancel { .. } => Access::Held,
    }
}

/// A new conversation reads accounts, skills and files, and may need its
/// account signed in again first; a login refreshes credentials an original
/// keeps using. Replacing or removing an account home, or removing a worktree,
/// can pull them from under an original.
fn device_access(operation: &DeviceOperation) -> Access<'static> {
    use DeviceOperation as O;
    match operation {
        O::Home
        | O::ProjectNavigation { .. }
        | O::AccountsRead
        | O::AccountsCache { .. }
        | O::Skills { .. }
        | O::AccountLogin { .. }
        | O::AccountProbe { .. }
        | O::AccountProbeAfterLogin { .. }
        | O::Discover { .. }
        | O::List { .. }
        | O::Read { .. }
        | O::Write { .. }
        | O::AccountDefault { .. }
        | O::ReadImage { .. }
        | O::ReadImageAsset { .. }
        | O::AccountReset { .. } => Access::Open,
        O::Git { operation, .. } => match operation {
            GitOperation::RemoveWorktree { .. } => Access::Held,
            GitOperation::Status
            | GitOperation::Worktrees
            | GitOperation::CreateWorktree { .. }
            | GitOperation::Diff { .. }
            | GitOperation::Stage { .. }
            | GitOperation::Unstage { .. }
            | GitOperation::Commit { .. } => Access::Open,
        },
        O::RemoveMergedWorktree { .. } | O::AccountLifecycle { .. } | O::AccountsUpdate { .. } => {
            Access::Held
        }
    }
}

/// Refuses what startup recovery's hold on original ownership does not admit.
/// Called before and after the control gate; admitting a fresh start records
/// its identity as independent of recovery, which repeating does not change.
pub(super) fn admit(host: &SharedHost, request: &ClientRequest) -> Result<(), ProtocolError> {
    let access = access(request);
    let mut guard = host.lock().unwrap_or_else(|p| p.into_inner());
    let mut state: &mut HostState = &mut guard;
    let Some(detail) = state.startup_recovery().map(str::to_owned) else {
        return Ok(());
    };
    let admitted = match access {
        Access::Open => true,
        Access::Close(target) => {
            if let Some(id) = target {
                state
                    .recovery_independence
                    .note_conversation(&state.chats, id);
            }
            true
        }
        Access::Shutdown => !state.recovery_owns_work(),
        Access::Target(id) => {
            state
                .recovery_independence
                .note_conversation(&state.chats, id);
            state.recovery_independence.contains(id)
        }
        Access::PrepareResume(source) => state.recovery_independence.contains(source),
        Access::RecoverAccount {
            source,
            replacement,
        } => {
            if !state.recovery_independence.contains(source)
                || replacement.terminal.is_some()
                || replacement.owned_codex_listener.is_some()
            {
                false
            } else {
                drop(guard);
                let owned = super::chat::has_native_owner(replacement)?;
                guard = host.lock().unwrap_or_else(|p| p.into_inner());
                state = &mut guard;
                let admitted = !owned && !held_by_recovery(state, replacement.id);
                if admitted {
                    state.recovery_independence.insert(replacement.id);
                }
                admitted
            }
        }
        Access::StartChat(session) => {
            drop(guard);
            let fresh = fresh_chat(host, session)?;
            guard = host.lock().unwrap_or_else(|p| p.into_inner());
            state = &mut guard;
            // Recovery cannot adopt this identity between the two checks: it
            // adopts only what its checkpoint and holders already name.
            let admitted = fresh && !held_by_recovery(state, session.id);
            if admitted {
                state.recovery_independence.insert(session.id);
            }
            admitted
        }
        Access::Spawn(session) => {
            drop(guard);
            let fresh = fresh_terminal(session)?;
            guard = host.lock().unwrap_or_else(|p| p.into_inner());
            state = &mut guard;
            let admitted = fresh
                && !held_by_recovery(state, session.id)
                && independent_selection(state, &session.launch);
            if admitted {
                state.recovery_independence.insert(session.id);
            }
            admitted
        }
        Access::Held => false,
    };
    if admitted {
        return Ok(());
    }
    Err(refusal(&detail))
}

/// The hold's refusal. A client recognises it by
/// [`crate::chat::STARTUP_RECOVERY_REFUSAL`]; the rest names the daemon's
/// retained reason for the person reading it.
pub(super) fn refusal(detail: &str) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::TerminalUnavailable,
        format!(
            "{}; inspect available sessions or explicitly Close the original actor: {detail}",
            crate::chat::STARTUP_RECOVERY_REFUSAL
        ),
        false,
    )
}

/// Whether `id` is an identity recovery holds: one its checkpoint names, or
/// one this daemon hosts, is starting or adopting that it did not start
/// during the hold.
fn held_by_recovery(state: &HostState, id: TerminalId) -> bool {
    if state.recovery_independence.contains(id) {
        return false;
    }
    state.is_live(&id)
        || state.chats.contains_key(&id)
        || state.terminals.contains_key(&id)
        || state.startup_custody_contains(id)
        || state
            .retained_startup_checkpoint()
            .is_some_and(|checkpoint| checkpoint_names(checkpoint, id))
}

fn checkpoint_names(checkpoint: &crate::upgrade::handoff::Checkpoint, id: TerminalId) -> bool {
    checkpoint.chats.iter().any(|chat| {
        std::iter::once(&chat.source)
            .chain(
                chat.previous_sources
                    .iter()
                    .map(|previous| &previous.source),
            )
            .any(|source| source.id == id || source.terminal.as_ref().is_some_and(|t| t.id == id))
    }) || checkpoint.terminals.iter().any(|terminal| {
        std::iter::once(&terminal.source)
            .chain(&terminal.previous_sources)
            .any(|source| source.id == id)
    }) || checkpoint.carried.iter().any(|terminal| {
        std::iter::once(&terminal.source)
            .chain(&terminal.previous_sources)
            .any(|source| source.id == id)
    })
}

/// Provider conversations recovery holds: those its checkpoint names and
/// those of conversations and terminals this daemon hosts that it did not
/// start during the hold. A resume of one would be a second writer for an
/// original.
fn conversation_held_by_recovery(state: &HostState, provider: Provider, id: &str) -> bool {
    let runs = |session: &TerminalSession| {
        named_by(&session.launch, provider, id)
            || session
                .resume_hint
                .as_deref()
                .and_then(crate::model::agent_recovery::recipe_from_hint)
                .is_some_and(|recipe| named_by(&recipe, provider, id))
    };
    if state.terminals.iter().any(|(terminal, record)| {
        !state.recovery_independence.contains(*terminal) && runs(&record.session)
    }) || state
        .retained_startup_checkpoint()
        .is_some_and(|checkpoint| {
            let lineage = |source, previous: &'_ Vec<TerminalSession>| {
                std::iter::once(source).chain(previous).any(runs)
            };
            checkpoint.terminals.iter().any(|terminal| {
                lineage(&terminal.source, &terminal.previous_sources)
                    || named_by(&terminal.recipe, provider, id)
            }) || checkpoint
                .carried
                .iter()
                .any(|terminal| lineage(&terminal.source, &terminal.previous_sources))
        })
    {
        return true;
    }
    let named = |session: &Session, launch: &crate::chat::Launch| {
        session.launch.provider == provider
            && (launch.session_id.as_deref() == Some(id)
                || session.launch.session_id.as_deref() == Some(id))
    };
    state.chats.iter().any(|(chat, record)| {
        !state.recovery_independence.contains(*chat)
            && record.session.launch.provider == provider
            && (record.handle.session_id().as_deref() == Some(id)
                || record.session.launch.session_id.as_deref() == Some(id))
    }) || state
        .retained_startup_checkpoint()
        .is_some_and(|checkpoint| {
            checkpoint.chats.iter().any(|chat| {
                named(&chat.source, &chat.launch)
                    || chat
                        .previous_sources
                        .iter()
                        .any(|previous| named(&previous.source, &previous.launch))
            })
        })
}

/// A conversation start is independent of recovery when it starts a new
/// provider conversation in a new allocation, with no terminal, retained
/// provider or listener, and no authenticated holder owns that allocation. A
/// resume is independent only of a provider conversation that was itself
/// started during the hold.
fn fresh_chat(host: &SharedHost, session: &Session) -> Result<bool, ProtocolError> {
    if session.terminal.is_some()
        || session.owned_codex_listener.is_some()
        || session.native_continuity != crate::chat::NativeContinuity::ReconnectOnly
    {
        return Ok(false);
    }
    if let Some(id) = &session.launch.session_id {
        let mut guard = host.lock().unwrap_or_else(|p| p.into_inner());
        let state: &mut HostState = &mut guard;
        state.recovery_independence.note_live(&state.chats);
        if !state
            .recovery_independence
            .contains_conversation(session.launch.provider, id)
            || conversation_held_by_recovery(state, session.launch.provider, id)
        {
            return Ok(false);
        }
    }
    Ok(!super::chat::has_native_owner(session)?)
}

/// A terminal start is independent of recovery when it relaunches nothing, no
/// authenticated holder owns its allocation, and the conversation its provider
/// opens is one [`independent_selection`] admits.
fn fresh_terminal(session: &TerminalSession) -> Result<bool, ProtocolError> {
    if session.predecessor.is_some()
        || session.recovery_origin.is_some()
        || session.owned_codex_listener.is_some()
        || session.resume_hint.is_some()
    {
        return Ok(false);
    }
    use crate::daemon::holder_registry::{self as registry, Kind};
    let unavailable = |error: String| {
        ProtocolError::new(
            ProtocolErrorCode::TerminalUnavailable,
            format!("Cannot read terminal holders: {error}"),
            true,
        )
    };
    let directory = registry::directory(Kind::Terminal).map_err(unavailable)?;
    let owned =
        registry::recovery_candidate(&registry::socket(&directory, session.id), Kind::Terminal)
            .map_err(unavailable)?
            .is_some();
    Ok(!owned)
}

/// Which of its provider's conversations a launch opens.
#[derive(Debug, PartialEq, Eq)]
enum Selection {
    /// Not a provider, or a new conversation the provider names itself.
    Nothing,
    /// A new conversation under an ID the launch chose.
    New(Provider, String),
    /// The saved conversation with this ID.
    Existing(Provider, String),
    /// One the provider picks or the launch names inexactly, such as the
    /// latest one, which could be an original's.
    Unknown,
}

/// Reads every argument, not only an interactive launch's: a provider's
/// subcommands, such as `codex exec resume`, resume conversations too. It
/// reads the launches cagents builds, which run the provider directly; a
/// shell command line is the person's own, as anything typed into one is.
fn selection(launch: &crate::model::LaunchRecipe) -> Selection {
    let Some((provider, arguments)) = Provider::launched(launch) else {
        return Selection::Nothing;
    };
    let mut selected = Selection::Nothing;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        // A pull request's linked session is one the provider picks too.
        if provider.is_latest_selector(flag) || flag == "--from-pr" {
            return Selection::Unknown;
        }
        if provider.is_identity_option(flag) {
            let value = inline.or_else(|| arguments.next().map(String::as_str));
            let Some(id) = value.filter(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
            else {
                return Selection::Unknown;
            };
            if selected != Selection::Nothing {
                return Selection::Unknown;
            }
            selected = if provider == Provider::Claude && flag == "--session-id" {
                Selection::New(provider, id.to_owned())
            } else {
                Selection::Existing(provider, id.to_owned())
            };
        } else if inline.is_none()
            && (provider.is_value_option(flag) || provider.is_cwd_option(flag))
        {
            arguments.next();
        }
    }
    selected
}

/// Whether `launch` opens the provider conversation `id`, new or saved.
fn named_by(launch: &crate::model::LaunchRecipe, provider: Provider, id: &str) -> bool {
    matches!(selection(launch),
        Selection::New(selected, ref conversation) | Selection::Existing(selected, ref conversation)
            if selected == provider && conversation == id)
}

/// Whether the conversation a launch opens is independent of recovery: a new
/// one under an ID nothing recovery holds uses, or one started during the
/// hold, as [`fresh_chat`] admits for conversations.
fn independent_selection(state: &mut HostState, launch: &crate::model::LaunchRecipe) -> bool {
    match selection(launch) {
        Selection::Nothing => true,
        Selection::New(provider, id) => !conversation_held_by_recovery(state, provider, &id),
        Selection::Existing(provider, id) => {
            state.recovery_independence.note_live(&state.chats);
            state
                .recovery_independence
                .contains_conversation(provider, &id)
                && !conversation_held_by_recovery(state, provider, &id)
        }
        Selection::Unknown => false,
    }
}

/// What this daemon started while startup recovery held original ownership.
/// None of it is, or could become, a retained original.
#[derive(Default)]
pub(super) struct Independence {
    allocations: std::collections::BTreeSet<TerminalId>,
    conversations: std::collections::BTreeSet<(Provider, String)>,
}

impl Independence {
    pub(super) fn contains(&self, id: TerminalId) -> bool {
        self.allocations.contains(&id)
    }

    fn insert(&mut self, id: TerminalId) {
        self.allocations.insert(id);
    }

    /// A replacement that takes over an independent conversation, such as the
    /// terminal it returns to, is as independent of recovery as it was.
    pub(in crate::host) fn extend(&mut self, source: TerminalId, replacement: TerminalId) {
        if self.contains(source) {
            self.insert(replacement);
        }
    }

    fn contains_conversation(&self, provider: Provider, id: &str) -> bool {
        self.conversations.contains(&(provider, id.to_owned()))
    }

    /// Remembers the provider conversation an independent allocation hosts,
    /// so it can be reopened after its actor is gone.
    fn note_conversation(
        &mut self,
        chats: &std::collections::BTreeMap<TerminalId, super::chat::Record>,
        id: TerminalId,
    ) {
        if !self.allocations.contains(&id) {
            return;
        }
        if let Some(record) = chats.get(&id)
            && let Some(conversation) = record.handle.session_id()
        {
            self.conversations
                .insert((record.session.launch.provider, conversation));
        }
    }

    fn note_live(&mut self, chats: &std::collections::BTreeMap<TerminalId, super::chat::Record>) {
        let independent = self.allocations.iter().copied().collect::<Vec<_>>();
        for id in independent {
            self.note_conversation(chats, id);
        }
    }
}

#[cfg(test)]
#[path = "recovery_admission/tests.rs"]
mod tests;
