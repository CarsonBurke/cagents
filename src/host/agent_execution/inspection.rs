//! Read-only chat inspection. Process custody is separate from channel access:
//! these captures never turn a listed session or a client scope into authority.
//! The registry resolver and cache reader own their independent proof/release.
use super::{SharedHost, SharedRegistry};
use crate::{
    agent_execution::{
        Authority, HostedSession,
        inspection::{Coverage, Cursor, Kind, MAX_CURSOR_BYTES, Page, Part},
        same_chat_process,
    },
    chat::{
        self,
        runtime::{Handle, PeerCursor, PeerKind, PeerPage, PeerReader},
    },
    host::{model::channel_work::PeerReadContext, state::HostState},
    model::{HostSessionProof, TerminalId},
    terminal::TerminalHandle,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::sync::Arc;

const CHANGED: &str = "The inspection source changed; request a fresh peer chat page";
const INVALID_CURSOR: &str =
    "The peer cursor no longer matches this inspection; start a fresh read";

/// Comparison data only. Never serialize authority tokens, launch recipes,
/// private paths or a process handle into a public continuation.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CursorScope {
    home: crate::channel_work::Home,
    caller_card: crate::model::CardId,
    target_card: crate::model::CardId,
    caller_epoch: u64,
    target_epoch: u64,
    caller_process: [u8; 32],
    target_process: [u8; 32],
    caller_instance: Option<uuid::Uuid>,
    target_instance: uuid::Uuid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Continuation<T> {
    version: u8,
    scope: CursorScope,
    value: T,
}

struct ProcessDigest(Sha256);
impl Write for ProcessDigest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn process_digest(source: &HostedSession) -> Result<[u8; 32], String> {
    let mut sink = ProcessDigest(Sha256::new());
    match source {
        HostedSession::Chat(source) => serde_json::to_writer(
            &mut sink,
            &(
                source.id,
                &source.proof,
                source.launch.provider,
                &source.launch.recipe,
            ),
        ),
        HostedSession::Terminal(source) => serde_json::to_writer(
            &mut sink,
            &(source.id, &source.launch, source.continuity.live_proof()),
        ),
    }
    .map_err(|_| INVALID_CURSOR)?;
    Ok(sink.0.finalize().into())
}

struct CursorBytes(Vec<u8>);
impl Write for CursorBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > MAX_CURSOR_BYTES / 4 * 3 - 3)
        {
            return Err(io::Error::other("peer cursor exceeds its bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Private staging custody. A page is never returned as an ordinary process
/// reply: consuming this value is the only path to the delivery queue.
pub(in crate::host) struct PreparedInspection {
    authorization: ReadAuthorization,
    page: Page,
    #[cfg(test)]
    final_registry_contended: Option<std::sync::mpsc::SyncSender<()>>,
}

/// Placement permission was observed coherently and explicitly released.
/// Later placement/revocation changes can complete this one immutable page;
/// resident source identity must still be current at queue admission.
struct CheckedInspection(PreparedInspection);

/// The client owns ordinary read admission before calling this function and
/// repeats it before consuming the result. No host/registry guard crosses the
/// Local/Remote read; that boundary returns only an explicitly released value.
pub(in crate::host) fn prepare(
    host: &SharedHost,
    authority: &Authority,
    target: TerminalId,
    cursor: Option<Cursor>,
) -> Result<PreparedInspection, String> {
    let authorization = ReadAuthorization::capture(host, authority, target)?;
    let (mut reader, after): (PeerReader, Option<PeerCursor>) = match cursor {
        Some(cursor) => {
            let (reader, cursor) =
                authorization.decode_cursor::<(PeerReader, PeerCursor)>(&cursor)?;
            (reader, Some(cursor))
        }
        None => (PeerReader::default(), None),
    };
    let cached = authorization
        .sources
        .target_handle
        .peer_page(&mut reader, after)?;
    let page = public_page(&authorization, reader, cached)?;
    authorization.stage(page)
}

fn public_page(
    authorization: &ReadAuthorization,
    reader: PeerReader,
    cached: PeerPage,
) -> Result<Page, String> {
    if !cached.cached_only {
        return Err("Peer inspection requires the existing cached conversation".into());
    }
    let mut page = Page::new(
        authorization.sources.target.id,
        if cached.provider_has_earlier {
            Coverage::Partial
        } else {
            Coverage::Cached
        },
    );
    for part in cached.parts {
        let kind = match part.kind {
            PeerKind::UserText => Kind::UserText,
            PeerKind::AssistantText { phase } => Kind::AssistantText { phase },
            PeerKind::ToolOutput { name, status } => Kind::ToolOutput { name, status },
            PeerKind::CommandOutput { status, exit_code } => {
                Kind::CommandOutput { status, exit_code }
            }
        };
        let part = Part {
            // The codec replacement token, never the provider's raw item ID.
            item_id: part.item.to_string(),
            offset: u64::try_from(part.offset)
                .map_err(|_| "Peer output offset exceeds its wire bound")?,
            text: part.text,
            complete: part.complete,
            kind,
            started_at: part.started_at,
            at: part.at,
        };
        if !page.try_push(part)? {
            // Never consume the cache's final cursor after dropping a part.
            // Refuse this entire staged response; no partial page is published.
            return Err("The cached peer page exceeds its public response bound".into());
        }
    }
    page.at_end = cached.at_end;
    page.next_cursor = if cached.at_end {
        // This logical read is complete, including an empty cache. A later
        // cursor=None starts a new authenticated read, not a follow operation.
        None
    } else {
        let next = cached
            .next
            .ok_or("The peer cache did not provide a progressing cursor")?;
        Some(authorization.encode_cursor((reader, next))?)
    };
    page.validate_for(authorization.sources.target.id)?;
    Ok(page)
}

impl PreparedInspection {
    /// The closure performs only a correlated, nonblocking queue admission
    /// using the supplied proof, never a socket write or HostState reentry.
    /// Permission linearizes at the final coherent registry read below; live
    /// source identity linearizes at the subsequent HostState queue admission.
    pub(in crate::host) fn admit(
        self,
        host: &SharedHost,
        enqueue: impl FnOnce(Page, &HostSessionProof) -> Result<(), String>,
    ) -> Result<(), String> {
        self.check_context()?.enqueue_current(host, enqueue)
    }

    fn check_context(self) -> Result<CheckedInspection, String> {
        self.page
            .validate_for(self.authorization.sources.target.id)?;
        let current = {
            #[cfg(test)]
            if let Some(contended) = &self.final_registry_contended {
                // Report observed contention in the actual admit path, rather
                // than mere thread entry. The test still owns this real mutex
                // until its independent HostState probe completes.
                if !matches!(
                    self.authorization.registry.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ) {
                    return Err("Expected the final registry read to be blocked".into());
                }
                contended
                    .send(())
                    .map_err(|_| "Final registry contention observer closed")?;
            }
            let registry = self
                .authorization
                .registry
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            registry.peer_chat_read_context(
                &self.authorization.authority,
                &self.authorization.sources.caller,
                &self.authorization.sources.target,
            )?
        };
        if current != self.authorization.context {
            return Err(CHANGED.into());
        }
        Ok(CheckedInspection(self))
    }
}

impl CheckedInspection {
    fn enqueue_current(
        self,
        host: &SharedHost,
        enqueue: impl FnOnce(Page, &HostSessionProof) -> Result<(), String>,
    ) -> Result<(), String> {
        // All registry guards and the owned SQLite read are released before
        // HostState. This is not a continuous placement/revocation reservation.
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        self.0.authorization.require_current_in(&state)?;
        enqueue(self.0.page, state.proof())
    }
}

/// Strict issued caller, canonical same-Home placement and exact live actors.
/// It carries no permission to change either peer or its UI attachment.
struct ReadAuthorization {
    registry: SharedRegistry,
    authority: Authority,
    context: PeerReadContext,
    sources: LiveSources,
}

impl ReadAuthorization {
    fn cursor_scope(&self) -> Result<CursorScope, String> {
        Ok(CursorScope {
            home: self.context.home,
            caller_card: self.context.caller_card,
            target_card: self.context.target_card,
            caller_epoch: self.context.caller_placement_epoch,
            target_epoch: self.context.target_placement_epoch,
            caller_process: process_digest(&self.sources.caller)?,
            target_process: process_digest(&HostedSession::Chat(self.sources.target.clone()))?,
            caller_instance: match &self.sources.caller_handle {
                CallerHandle::Chat { instance, .. } => Some(*instance),
                CallerHandle::Terminal(_) => None,
            },
            target_instance: self.sources.target_instance,
        })
    }

    fn encode_cursor<T: Serialize>(&self, value: T) -> Result<Cursor, String> {
        let continuation = Continuation {
            version: 1,
            scope: self.cursor_scope()?,
            value,
        };
        let bytes =
            postcard::to_io(&continuation, CursorBytes(Vec::new())).map_err(|_| INVALID_CURSOR)?;
        Cursor::parse(&format!("p1.{}", URL_SAFE_NO_PAD.encode(bytes.0)))
    }

    fn decode_cursor<T: DeserializeOwned>(&self, cursor: &Cursor) -> Result<T, String> {
        let encoded = cursor.as_str().strip_prefix("p1.").ok_or(INVALID_CURSOR)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| INVALID_CURSOR)?;
        let (continuation, trailing): (Continuation<T>, _) =
            postcard::take_from_bytes(&bytes).map_err(|_| INVALID_CURSOR)?;
        if !trailing.is_empty()
            || continuation.version != 1
            || continuation.scope != self.cursor_scope()?
        {
            return Err(INVALID_CURSOR.into());
        }
        Ok(continuation.value)
    }

    fn capture(
        host: &SharedHost,
        authority: &Authority,
        target: TerminalId,
    ) -> Result<Self, String> {
        let (registry, target) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let registry = state
                .execution_registry
                .clone()
                .ok_or("The agent execution registry is unavailable")?;
            let target = state
                .chats
                .get(&target)
                .ok_or("The peer chat is unavailable")?
                .session
                .clone();
            (registry, target)
        };
        let (caller, context) = {
            let model = registry.lock().unwrap_or_else(|p| p.into_inner());
            // User/absent and malformed authority cannot become an actor here.
            let caller = model
                .agent_authority_source(authority)?
                .ok_or("Peer reads require an authenticated agent session")?;
            let context = model.peer_chat_read_context(authority, &caller, &target)?;
            (caller, context)
        };
        let authorization = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let sources = LiveSources::capture_in(&state, caller, target)?;
            let authorization = Self {
                registry: registry.clone(),
                authority: authority.clone(),
                context,
                sources,
            };
            authorization.require_current_in(&state)?;
            authorization
        };
        Ok(authorization)
    }

    fn require_current_in(&self, state: &HostState) -> Result<(), String> {
        if !state
            .execution_registry
            .as_ref()
            .is_some_and(|registry| Arc::ptr_eq(registry, &self.registry))
        {
            return Err(CHANGED.into());
        }
        self.sources.require_current_in(state)
    }

    /// Caller must supply only a fully released immutable cache projection.
    /// The final admission still reauthenticates both placements and actors.
    fn stage(self, page: Page) -> Result<PreparedInspection, String> {
        page.validate_for(self.sources.target.id)?;
        Ok(PreparedInspection {
            authorization: self,
            page,
            #[cfg(test)]
            final_registry_contended: None,
        })
    }
}

/// Exact resident attachments captured only from daemon-resolved sources.
/// This proves process custody, not same-channel permission or cache identity.
struct LiveSources {
    caller: HostedSession,
    caller_handle: CallerHandle,
    target: chat::Session,
    target_handle: Handle,
    target_instance: uuid::Uuid,
}

enum CallerHandle {
    Chat {
        handle: Handle,
        instance: uuid::Uuid,
    },
    Terminal(TerminalHandle),
}

impl LiveSources {
    fn capture_in(
        state: &HostState,
        caller: HostedSession,
        target: chat::Session,
    ) -> Result<Self, String> {
        if state.shutting_down || !caller.has_complete_proof() {
            return Err(CHANGED.into());
        }
        let caller_handle = match &caller {
            HostedSession::Chat(source) => {
                let record = checked_chat(state, source)?;
                CallerHandle::Chat {
                    handle: record.handle.clone(),
                    instance: record.execution_instance,
                }
            }
            HostedSession::Terminal(source) => {
                if source.continuity.live_proof().as_ref() != Some(state.proof()) {
                    return Err(CHANGED.into());
                }
                let record = state.terminals.get(&source.id).ok_or(CHANGED)?;
                if record.handle.is_completed()
                    || !HostedSession::Terminal(record.session.clone()).same_process(&caller)
                {
                    return Err(CHANGED.into());
                }
                CallerHandle::Terminal(record.handle.clone())
            }
        };
        let record = checked_chat(state, &target)?;
        Ok(Self {
            caller,
            caller_handle,
            target,
            target_handle: record.handle.clone(),
            target_instance: record.execution_instance,
        })
    }

    /// A resident-only check for final admission. No snapshot, source hydration,
    /// provider command, retained RPC, visibility or subscription is requested.
    fn require_current_in(&self, state: &HostState) -> Result<(), String> {
        if state.shutting_down {
            return Err(CHANGED.into());
        }
        match (&self.caller, &self.caller_handle) {
            (HostedSession::Chat(source), CallerHandle::Chat { handle, instance }) => {
                let record = checked_chat(state, source)?;
                if record.execution_instance != *instance || !record.handle.same_attachment(handle)
                {
                    return Err(CHANGED.into());
                }
            }
            (HostedSession::Terminal(source), CallerHandle::Terminal(handle)) => {
                let record = state.terminals.get(&source.id).ok_or(CHANGED)?;
                if source.continuity.live_proof().as_ref() != Some(state.proof())
                    || record.handle.is_completed()
                    || !record.handle.same_worker(handle)
                    || !HostedSession::Terminal(record.session.clone()).same_process(&self.caller)
                {
                    return Err(CHANGED.into());
                }
            }
            _ => return Err(CHANGED.into()),
        }
        let record = checked_chat(state, &self.target)?;
        if record.execution_instance != self.target_instance
            || !record.handle.same_attachment(&self.target_handle)
        {
            return Err(CHANGED.into());
        }
        Ok(())
    }
}

fn checked_chat<'a>(
    state: &'a HostState,
    source: &chat::Session,
) -> Result<&'a crate::host::chat::Record, String> {
    if !HostedSession::Chat(source.clone()).has_complete_proof()
        || source.proof.as_ref() != Some(state.proof())
        || state.chat_spawning.contains_key(&source.id)
        || state.chat_handoffs.contains_key(&source.id)
        || state.native_closing.contains_key(&source.id)
    {
        return Err(CHANGED.into());
    }
    let record = state.chats.get(&source.id).ok_or(CHANGED)?;
    if record.returning_terminal
        || record.handle.is_close_only()
        || record.handle.is_completed()
        || !same_chat_process(&record.session, source)
    {
        return Err(CHANGED.into());
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{
            Destination, Lineage, Origin, Spec, Submit, Surface, Target, inspection::Coverage,
        },
        host::chat::Record,
        host::model::Registry,
        model::{
            AgentId, CardId, CardKind, ChannelId, HostIncarnation, HostSessionId, HostSessionProof,
            TerminalId,
        },
        workspace_model::{
            ModelEdit,
            views::{Change, Pane, Scope},
        },
    };
    use std::{cell::Cell, sync::Mutex};

    fn fixture() -> (HostState, chat::Session, chat::Session) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("peer-inspection-fixture"),
            host_incarnation: HostIncarnation::new("peer-inspection-fixture"),
            boot_id: crate::model::BootId::new("peer-inspection-fixture"),
        };
        let mut state = HostState::new(proof.clone());
        let source = || chat::Session {
            id: TerminalId::new(),
            launch: chat::Launch {
                provider: Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "fixture-only-no-process".into(),
                    arguments: Vec::new(),
                    working_directory: "/fixture".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let caller = source();
        let target = source();
        for session in [&caller, &target] {
            let (handle, _) = Handle::fixture_remote_attachment_pair();
            state.chats.insert(
                session.id,
                Record {
                    execution_instance: uuid::Uuid::new_v4(),
                    console_recipe: None,
                    paired_terminal: None,
                    session: session.clone(),
                    handle,
                    source_process: None,
                    subscribers: Default::default(),
                    observations: Default::default(),
                    completion_observed: false,
                    returning_terminal: false,
                },
            );
        }
        (state, caller, target)
    }

    #[test]
    fn hidden_unattached_sources_keep_resident_custody_without_observation_effects() {
        let (state, caller, target) = fixture();
        let captured =
            LiveSources::capture_in(&state, HostedSession::Chat(caller.clone()), target.clone())
                .unwrap();
        captured.require_current_in(&state).unwrap();
        for session in [caller, target] {
            let record = &state.chats[&session.id];
            assert!(record.subscribers.is_empty());
            assert!(record.observations.is_empty());
            assert!(!record.returning_terminal);
        }
    }

    #[test]
    fn source_proof_or_launch_replacement_refuses_capture_and_revalidation() {
        let (mut state, caller, target) = fixture();
        let captured =
            LiveSources::capture_in(&state, HostedSession::Chat(caller.clone()), target.clone())
                .unwrap();
        state.chats.get_mut(&target.id).unwrap().session.proof = None;
        assert!(captured.require_current_in(&state).is_err());
        assert!(
            LiveSources::capture_in(&state, HostedSession::Chat(caller.clone()), target.clone(),)
                .is_err()
        );
        state.chats.get_mut(&target.id).unwrap().session = target.clone();
        state
            .chats
            .get_mut(&caller.id)
            .unwrap()
            .session
            .launch
            .recipe
            .arguments
            .push("replacement".into());
        assert!(captured.require_current_in(&state).is_err());
        let mut absent = target;
        absent.proof = None;
        assert!(checked_chat(&state, &absent).is_err());
    }

    #[test]
    fn same_actor_with_a_new_proxy_does_not_reuse_a_captured_attachment() {
        let (mut state, caller, target) = fixture();
        let (first, replacement) = Handle::fixture_remote_attachment_pair();
        assert!(first.same_actor(&replacement));
        assert!(!first.same_attachment(&replacement));
        state.chats.get_mut(&target.id).unwrap().handle = first;
        let captured =
            LiveSources::capture_in(&state, HostedSession::Chat(caller), target.clone()).unwrap();
        state.chats.get_mut(&target.id).unwrap().handle = replacement;
        assert!(captured.require_current_in(&state).is_err());
    }

    #[test]
    fn instance_change_retirement_or_terminal_handoff_refuses_a_captured_page() {
        let (mut state, caller, target) = fixture();
        let captured =
            LiveSources::capture_in(&state, HostedSession::Chat(caller), target.clone()).unwrap();
        let original = state.chats[&target.id].execution_instance;
        state.chats.get_mut(&target.id).unwrap().execution_instance = uuid::Uuid::new_v4();
        assert!(captured.require_current_in(&state).is_err());
        state.chats.get_mut(&target.id).unwrap().execution_instance = original;
        state.chats.get_mut(&target.id).unwrap().returning_terminal = true;
        assert!(captured.require_current_in(&state).is_err());
        state.chats.remove(&target.id);
        assert!(captured.require_current_in(&state).is_err());
    }

    #[test]
    fn shutdown_or_unproved_caller_never_captures_resident_sources() {
        let (mut state, mut caller, target) = fixture();
        caller.proof = None;
        assert!(
            LiveSources::capture_in(&state, HostedSession::Chat(caller), target.clone(),).is_err()
        );
        state.shutting_down = true;
        assert!(checked_chat(&state, &target).is_ok());
        assert!(
            LiveSources::capture_in(&state, HostedSession::Chat(target.clone()), target,).is_err()
        );
    }

    struct PolicyFixture {
        host: SharedHost,
        registry: SharedRegistry,
        authority: Authority,
        caller: HostedSession,
        target: chat::Session,
        channel: ChannelId,
        target_card: CardId,
        managed: Option<AgentId>,
        _directory: tempfile::TempDir,
    }

    fn policy_fixture(owner: Option<Origin>) -> PolicyFixture {
        let (mut state, mut caller, mut target) = fixture();
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("project");
        std::fs::create_dir(&repository).unwrap();
        caller.launch.recipe.working_directory = repository.clone();
        target.launch.recipe.working_directory = repository.clone();
        state.chats.get_mut(&caller.id).unwrap().session = caller.clone();
        state.chats.get_mut(&target.id).unwrap().session = target.clone();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "Peer policy fixture".into(),
                kind: Default::default(),
            })
            .unwrap();
        let channel = ChannelId::new();
        registry
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository: repository.clone(),
                title: "Peer policy fixture".into(),
                title_source: Default::default(),
            })
            .unwrap();
        let caller = HostedSession::Chat(caller);
        let authority = registry.register_agent_authority(&caller).unwrap();
        registry
            .register_agent_authority(&HostedSession::Chat(target.clone()))
            .unwrap();
        let managed = owner.map(|owner| {
            let accepted = registry
                .submit_agent(
                    &owner,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: None,
                            target: Target {
                                repository: repository.clone(),
                                directory: repository.clone(),
                                placement: Some(Scope::Channel(channel)),
                            },
                            surface: Surface::Chat,
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "Fixture records only; no provider is started".into(),
                        idempotency_key: uuid::Uuid::new_v4().to_string(),
                    },
                    1,
                )
                .unwrap();
            registry
                .bind_agent(accepted.agent.id, None, HostedSession::Chat(target.clone()))
                .unwrap();
            accepted.agent.id
        });
        let target_card = managed.map_or_else(CardId::new, |id| CardId::from_uuid(id.as_uuid()));
        let pane = |id, session| Pane {
            id,
            title: "Peer policy fixture".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: CardKind::Chat(session),
            scope: None,
        };
        let HostedSession::Chat(caller_chat) = &caller else {
            unreachable!()
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: Scope::Channel(channel),
                panes: vec![
                    pane(CardId::new(), caller_chat.clone()),
                    pane(target_card, target.clone()),
                ],
                closed: Vec::new(),
            })
            .unwrap();
        let registry = Arc::new(Mutex::new(registry));
        state.execution_registry = Some(registry.clone());
        PolicyFixture {
            host: Arc::new(crate::host::state::HostLock::new(state)),
            registry,
            authority,
            caller,
            target,
            channel,
            target_card,
            managed,
            _directory: directory,
        }
    }

    fn staged(f: &PolicyFixture) -> PreparedInspection {
        let mut page = Page::new(f.target.id, Coverage::Cached);
        page.at_end = true;
        ReadAuthorization::capture(&f.host, &f.authority, f.target.id)
            .unwrap()
            .stage(page)
            .unwrap()
    }

    fn transfer(f: &PolicyFixture, from: ChannelId, to: ChannelId) {
        let mut registry = f.registry.lock().unwrap();
        let scope = Scope::Channel(from);
        let expected_revision = registry.view_revision(&scope).unwrap();
        registry
            .apply(&ModelEdit::ChangeView {
                scope,
                expected_revision,
                operation: uuid::Uuid::new_v4(),
                change: Change::Transfer {
                    id: f.target_card,
                    destination: Scope::Channel(to),
                    before: None,
                },
            })
            .unwrap();
    }

    #[test]
    fn same_channel_unmanaged_user_and_foreign_child_peers_are_readable_without_owner_control() {
        for owner in [
            None,
            Some(Origin::User),
            Some(Origin::Child {
                parent: Lineage(TerminalId::new()),
            }),
        ] {
            let f = policy_fixture(owner);
            let admitted = Cell::new(0);
            let expected_proof = f.host.lock().unwrap().proof().clone();
            staged(&f)
                .admit(&f.host, |page, proof| {
                    assert_eq!(page.session, f.target.id);
                    assert_eq!(*proof, expected_proof);
                    admitted.set(admitted.get() + 1);
                    Ok(())
                })
                .unwrap();
            assert_eq!(admitted.get(), 1);
            if let Some(id) = f.managed {
                assert!(super::super::status(&f.host, &f.authority, id).is_err());
                for operation in [
                    crate::agent_execution::Operation::Requests(id),
                    crate::agent_execution::Operation::Close(id),
                    crate::agent_execution::Operation::Control {
                        agent: id,
                        command: crate::agent_execution::Control::Interrupt {
                            including_subagents: false,
                        },
                    },
                ] {
                    assert!(
                        super::super::process(&f.host, f.authority.clone(), operation).is_err()
                    );
                }
            }
            let state = f.host.lock().unwrap();
            assert!(
                state
                    .chats
                    .values()
                    .all(|record| record.subscribers.is_empty() && record.observations.is_empty())
            );
        }
    }

    #[test]
    fn absent_invalid_and_unknown_authority_never_fall_back_to_user_scope() {
        let f = policy_fixture(None);
        for authority in [
            Authority::default(),
            Authority::session(String::new()),
            Authority::session(uuid::Uuid::new_v4().to_string()),
        ] {
            assert!(ReadAuthorization::capture(&f.host, &authority, f.target.id).is_err());
        }
    }

    #[test]
    fn ordinary_process_cannot_extract_a_peer_page_before_checked_admission() {
        let f = policy_fixture(None);
        assert!(
            super::super::process(
                &f.host,
                f.authority.clone(),
                crate::agent_execution::Operation::InspectChat {
                    session: f.target.id,
                    cursor: None
                },
            )
            .is_err()
        );
        staged(&f).admit(&f.host, |_, _| Ok(())).unwrap();
    }

    #[test]
    fn continuation_binds_context_sources_and_instances_without_exporting_authority_or_paths() {
        let f = policy_fixture(None);
        let mut authorization =
            ReadAuthorization::capture(&f.host, &f.authority, f.target.id).unwrap();
        let value = (uuid::Uuid::new_v4(), 19u64);
        let cursor = authorization.encode_cursor(value).unwrap();
        assert_eq!(
            authorization
                .decode_cursor::<(uuid::Uuid, u64)>(&cursor)
                .unwrap(),
            value
        );
        let bytes = URL_SAFE_NO_PAD
            .decode(cursor.as_str().strip_prefix("p1.").unwrap())
            .unwrap();
        for private in [
            f.authority.token().unwrap(),
            f.target.launch.recipe.working_directory.to_str().unwrap(),
            f.target.launch.recipe.program.as_str(),
        ] {
            assert!(
                !bytes
                    .windows(private.len())
                    .any(|window| window == private.as_bytes())
            );
        }
        authorization.context.target_placement_epoch += 1;
        assert!(
            authorization
                .decode_cursor::<(uuid::Uuid, u64)>(&cursor)
                .is_err()
        );
        authorization.context.target_placement_epoch -= 1;
        authorization.sources.target_instance = uuid::Uuid::new_v4();
        assert!(
            authorization
                .decode_cursor::<(uuid::Uuid, u64)>(&cursor)
                .is_err()
        );
        let other = policy_fixture(None);
        let other =
            ReadAuthorization::capture(&other.host, &other.authority, other.target.id).unwrap();
        assert!(other.decode_cursor::<(uuid::Uuid, u64)>(&cursor).is_err());
    }

    #[test]
    fn continuation_ignores_thread_metadata_but_rejects_process_launch_changes() {
        let f = policy_fixture(None);
        let mut authorization =
            ReadAuthorization::capture(&f.host, &f.authority, f.target.id).unwrap();
        let cursor = authorization.encode_cursor(7u64).unwrap();
        authorization.sources.target.launch.session_id = Some("learned-provider-thread".into());
        assert_eq!(authorization.decode_cursor::<u64>(&cursor).unwrap(), 7);
        authorization
            .sources
            .target
            .launch
            .recipe
            .arguments
            .push("changed-process".into());
        assert!(authorization.decode_cursor::<u64>(&cursor).is_err());
    }

    #[test]
    fn malformed_oversized_and_trailing_continuations_fail_closed() {
        let f = policy_fixture(None);
        let authorization = ReadAuthorization::capture(&f.host, &f.authority, f.target.id).unwrap();
        assert!(
            authorization
                .encode_cursor("x".repeat(MAX_CURSOR_BYTES))
                .is_err()
        );
        for token in ["p1.a", "p1._", "p2.YQ", "YQ"] {
            let cursor = Cursor::parse(token).unwrap();
            assert!(authorization.decode_cursor::<u64>(&cursor).is_err());
        }
        let cursor = authorization.encode_cursor(7u64).unwrap();
        let mut bytes = URL_SAFE_NO_PAD
            .decode(cursor.as_str().strip_prefix("p1.").unwrap())
            .unwrap();
        bytes.push(0);
        let trailing = Cursor::parse(&format!("p1.{}", URL_SAFE_NO_PAD.encode(bytes))).unwrap();
        assert!(authorization.decode_cursor::<u64>(&trailing).is_err());
    }

    fn cached_fixture(parts: serde_json::Value) -> PeerPage {
        serde_json::from_value(serde_json::json!({
            "incarnation": uuid::Uuid::new_v4(), "parts": parts, "next": null,
            "at_end": true, "cached_only": true, "provider_has_earlier": false, "omitted_items": 0,
        }))
        .unwrap()
    }

    #[test]
    fn cache_conversion_keeps_safe_kinds_uuid_tokens_phase_status_and_unicode() {
        let f = policy_fixture(None);
        let authorization = ReadAuthorization::capture(&f.host, &f.authority, f.target.id).unwrap();
        let kinds = [
            Kind::UserText,
            Kind::AssistantText {
                phase: chat::Phase::Commentary,
            },
            Kind::ToolOutput {
                name: "read".into(),
                status: chat::ToolStatus::Succeeded,
            },
            Kind::CommandOutput {
                status: chat::ToolStatus::Failed,
                exit_code: Some(2),
            },
        ];
        let ids: Vec<_> = kinds.iter().map(|_| uuid::Uuid::new_v4()).collect();
        let parts: Vec<_> = kinds
            .iter()
            .zip(&ids)
            .map(|(kind, id)| {
                serde_json::json!({
                    "item": id, "kind": kind, "started_at": 1, "at": 2,
                    "offset": 3, "text": "雪🦀", "complete": true,
                })
            })
            .collect();
        let mut cached = cached_fixture(serde_json::Value::Array(parts));
        cached.provider_has_earlier = true;
        cached.next = Some(
            serde_json::from_value(serde_json::json!({
                "cache": cached.incarnation, "item": ids[3], "offset": 7,
                "emitted": true, "skipped": false,
            }))
            .unwrap(),
        );
        let page = public_page(&authorization, PeerReader::default(), cached).unwrap();
        assert_eq!(page.coverage, Coverage::Partial);
        assert!(page.at_end);
        assert!(page.next_cursor.is_none());
        for ((part, kind), id) in page.parts.iter().zip(kinds).zip(ids) {
            assert_eq!(part.kind, kind);
            assert_eq!(part.item_id, id.to_string());
            assert_eq!(part.offset, 3);
            assert_eq!(part.text, "雪🦀");
            assert_eq!((part.started_at, part.at), (Some(1), Some(2)));
        }
    }

    #[test]
    fn nonfinal_continuation_preserves_actual_reader_pin_and_cache_cursor_values() {
        let f = policy_fixture(None);
        let authorization = ReadAuthorization::capture(&f.host, &f.authority, f.target.id).unwrap();
        let mut cached = cached_fixture(serde_json::json!([]));
        cached.at_end = false;
        let cursor: PeerCursor = serde_json::from_value(serde_json::json!({
            "cache": cached.incarnation, "item": uuid::Uuid::new_v4(), "offset": 4096,
            "emitted": false, "skipped": true,
        }))
        .unwrap();
        cached.next = Some(cursor.clone());
        let reader: PeerReader =
            serde_json::from_value(serde_json::json!({ "incarnation": cached.incarnation }))
                .unwrap();
        let original = serde_json::to_value((&reader, &cursor)).unwrap();
        let page = public_page(&authorization, reader, cached).unwrap();
        assert!(!page.at_end);
        let returned = authorization
            .decode_cursor::<(PeerReader, PeerCursor)>(&page.next_cursor.unwrap())
            .unwrap();
        assert_eq!(serde_json::to_value(returned).unwrap(), original);
    }

    #[test]
    fn cache_conversion_refuses_entire_overbudget_or_nonprogressing_page() {
        let f = policy_fixture(None);
        let authorization = ReadAuthorization::capture(&f.host, &f.authority, f.target.id).unwrap();
        let parts: Vec<_> = (0..16).map(|_| serde_json::json!({
            "item": uuid::Uuid::new_v4(), "kind": "UserText", "started_at": null, "at": null,
            "offset": 0, "text": "\u{1}".repeat(8192), "complete": true,
        })).collect();
        assert!(
            public_page(
                &authorization,
                PeerReader::default(),
                cached_fixture(serde_json::Value::Array(parts))
            )
            .is_err()
        );
        let mut cached = cached_fixture(serde_json::json!([]));
        cached.at_end = false;
        assert!(public_page(&authorization, PeerReader::default(), cached).is_err());
    }

    #[test]
    fn retired_caller_authority_discards_staged_bytes_without_queue_admission() {
        let f = policy_fixture(None);
        let page = staged(&f);
        f.registry
            .lock()
            .unwrap()
            .retire_agent_authority(&f.caller)
            .unwrap();
        let admitted = Cell::new(false);
        assert!(
            page.admit(&f.host, |_, _| {
                admitted.set(true);
                Ok(())
            })
            .is_err()
        );
        assert!(!admitted.get());
    }

    #[test]
    fn cross_channel_and_away_then_back_cannot_admit_an_old_page() {
        let f = policy_fixture(None);
        let page = staged(&f);
        let other = ChannelId::new();
        {
            let mut registry = f.registry.lock().unwrap();
            let repository = f.target.launch.recipe.working_directory.clone();
            registry
                .apply(&ModelEdit::CreateChannel {
                    id: other,
                    repository,
                    title: "Other fixture channel".into(),
                    title_source: Default::default(),
                })
                .unwrap();
        }
        transfer(&f, f.channel, other);
        assert!(ReadAuthorization::capture(&f.host, &f.authority, f.target.id).is_err());
        transfer(&f, other, f.channel);
        let admitted = Cell::new(false);
        assert!(
            page.admit(&f.host, |_, _| {
                admitted.set(true);
                Ok(())
            })
            .is_err()
        );
        assert!(!admitted.get());
        staged(&f).admit(&f.host, |_, _| Ok(())).unwrap();
    }

    #[test]
    fn placement_or_revocation_after_final_read_can_complete_only_the_observed_page() {
        for revoke in [false, true] {
            let f = policy_fixture(None);
            let checked = staged(&f).check_context().unwrap();
            if revoke {
                f.registry
                    .lock()
                    .unwrap()
                    .retire_agent_authority(&f.caller)
                    .unwrap();
            } else {
                f.registry
                    .lock()
                    .unwrap()
                    .apply(&ModelEdit::DeleteChannel { id: f.channel })
                    .unwrap();
            }
            let admitted = Cell::new(false);
            checked
                .enqueue_current(&f.host, |_, _| {
                    admitted.set(true);
                    Ok(())
                })
                .unwrap();
            assert!(admitted.get());
            assert!(ReadAuthorization::capture(&f.host, &f.authority, f.target.id).is_err());
        }
    }

    #[test]
    fn blocked_final_registry_read_does_not_hold_host_state() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for revoke in [false, true] {
            let f = policy_fixture(None);
            let mut page = staged(&f);
            let registry = f.registry.lock().unwrap();
            let (contended_tx, contended_rx) = std::sync::mpsc::sync_channel(0);
            page.final_registry_contended = Some(contended_tx);
            let admitted = Arc::new(AtomicUsize::new(0));
            let published = admitted.clone();
            let host = f.host.clone();
            let pending = std::thread::spawn(move || {
                page.admit(&host, |_, _| {
                    published.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            });
            let contended = contended_rx.recv_timeout(std::time::Duration::from_secs(1));
            // A delayed observer must not strand its zero-capacity send after
            // the bounded receive timed out.
            drop(contended_rx);
            let (progress_tx, progress_rx) = std::sync::mpsc::channel();
            let host = f.host.clone();
            let unrelated = std::thread::spawn(move || {
                let state = host.lock().unwrap();
                let _ = progress_tx.send(state.chats.len());
            });
            let progress = progress_rx.recv_timeout(std::time::Duration::from_secs(1));
            let revoked = if revoke {
                registry.retire_agent_authority(&f.caller)
            } else {
                Ok(())
            };
            // Always unblock/reap the exact authored threads before asserting,
            // including timeout, observer or final authorization failures.
            drop(registry);
            let progressed = unrelated.join();
            let result = pending.join();
            assert!(contended.is_ok());
            assert_eq!(progress.unwrap(), 2);
            progressed.unwrap();
            revoked.unwrap();
            let result = result.unwrap();
            if revoke {
                assert!(result.is_err());
                assert_eq!(admitted.load(Ordering::SeqCst), 0);
            } else {
                result.unwrap();
                assert_eq!(admitted.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[test]
    fn final_admission_refuses_new_attachment_and_replaced_registry_even_with_equal_model() {
        let f = policy_fixture(None);
        let page = staged(&f);
        let (_, replacement) = Handle::fixture_remote_attachment_pair();
        f.host
            .lock()
            .unwrap()
            .chats
            .get_mut(&f.target.id)
            .unwrap()
            .handle = replacement;
        let admitted = Cell::new(false);
        assert!(
            page.admit(&f.host, |_, _| {
                admitted.set(true);
                Ok(())
            })
            .is_err()
        );
        assert!(!admitted.get());

        let f = policy_fixture(None);
        let page = staged(&f);
        let replacement = f.registry.lock().unwrap().execution_connection().unwrap();
        f.host.lock().unwrap().execution_registry = Some(Arc::new(Mutex::new(replacement)));
        assert!(
            page.admit(&f.host, |_, _| {
                admitted.set(true);
                Ok(())
            })
            .is_err()
        );
        assert!(!admitted.get());
    }
}
