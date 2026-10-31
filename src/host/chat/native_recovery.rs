//! Reattach only authenticated, still-owned provider servers. A saved recipe
//! never authorizes launching a replacement for a lost durable server.
use super::*;
use crate::daemon::holder_registry::{self as registry, Kind};

#[cfg(test)]
mod paired_projection_tests;

#[cfg(test)]
thread_local! {
    static BEFORE_PUBLICATION: std::cell::RefCell<Option<Box<dyn FnOnce(&SharedHost)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
struct BeforePublicationHook(std::marker::PhantomData<std::rc::Rc<()>>);

#[cfg(test)]
impl Drop for BeforePublicationHook {
    fn drop(&mut self) {
        BEFORE_PUBLICATION.with(|slot| slot.borrow_mut().take());
    }
}

#[cfg(test)]
fn before_publication_for_test(
    action: impl FnOnce(&SharedHost) + 'static,
) -> BeforePublicationHook {
    BEFORE_PUBLICATION.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "native publication hook already owned"
        );
        *slot.borrow_mut() = Some(Box::new(action));
    });
    BeforePublicationHook(std::marker::PhantomData)
}

fn project_paired_terminal(
    state: &HostState,
    terminal: &crate::model::TerminalSession,
    paired_holder: Option<&crate::terminal::holder::Record>,
    proof: &HostSessionProof,
) -> Result<
    (
        crate::model::TerminalSession,
        crate::terminal::TerminalHandle,
    ),
    String,
> {
    // Raw terminal records retain their original custody descriptor. Current
    // ownership comes from the same authenticated worker, not that old proof.
    let current = state
        .terminals
        .get(&terminal.id)
        .filter(|current| {
            current.handle.holder_identity().is_some()
                && current.handle.holder_identity() == paired_holder
                && current.session.launch == terminal.launch
                && current.session.owned_codex_listener == terminal.owned_codex_listener
        })
        .ok_or("The paired terminal could not be recovered; native ownership was preserved")?;
    let projected = state.native_terminal_projection(terminal.id)?;
    if projected.continuity.live_proof().as_ref() != Some(proof) {
        return Err("The paired terminal belongs to a different daemon owner".into());
    }
    Ok((projected, current.handle.clone()))
}

fn pair_projection_is_current(
    state: &HostState,
    terminal: Option<&crate::model::TerminalSession>,
    handle: Option<&crate::terminal::TerminalHandle>,
) -> bool {
    match (terminal, handle) {
        (None, None) => true,
        (Some(terminal), Some(handle)) => {
            state.terminals.get(&terminal.id).is_some_and(|current| {
                current.handle.same_worker(handle)
                    && state
                        .native_terminal_projection(terminal.id)
                        .is_ok_and(|projected| projected == *terminal)
            })
        }
        _ => false,
    }
}

fn restored_thread_ready(handle: &Handle, expected: Option<&str>) -> Result<bool, String> {
    match handle.confirmed_session_id() {
        None => Ok(false),
        Some(actual) if Some(actual.as_str()) != expected => {
            Err("Retained server confirmed a different conversation".into())
        }
        // Confirmation and durable capability are published separately. Until
        // both arrive, UpdateSource's unsupported no-op is not an ownership ACK.
        Some(_) => Ok(matches!(
            handle.native_continuity(),
            chat::NativeContinuity::Durable { .. }
        )),
    }
}

fn reattachment_failure(error: &str, cleanup: Option<&str>) -> String {
    // Put the custody result and bounded cleanup detail before the failure
    // handoff, whose native diagnosis precedes arbitrary provider RPC text.
    // The existing 4 KiB cache then retains both without copying a raw tail.
    let context = match cleanup {
        None => "Provider ownership was preserved; no replacement was started or prompt resent"
            .to_owned(),
        Some(cleanup) => {
            const CLEANUP_BYTES: usize = 512;
            let end = cleanup.floor_char_boundary(CLEANUP_BYTES);
            let omitted = if end < cleanup.len() {
                " [later cleanup details omitted]"
            } else {
                ""
            };
            format!(
                "Local attachment cleanup is unconfirmed; no replacement was started or prompt resent\nCleanup details: {}{omitted}",
                &cleanup[..end]
            )
        }
    };
    format!("Cannot reattach the retained native conversation. {context}\n{error}")
}

/// Diagnostic data only: it never authorizes attaching, launching or cleanup.
/// Keep hashes rather than another copy of launch environments for every alias.
#[derive(Default)]
pub(in crate::host) struct AdoptionFailures(std::collections::VecDeque<AdoptionFailure>);

struct AdoptionFailure {
    id: TerminalId,
    sources: Vec<[u8; 32]>,
    reason: String,
}

impl AdoptionFailures {
    pub(in crate::host) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    const LIMIT: usize = 128;
    const MESSAGE_BYTES: usize = 4096;

    fn source_key(source: &Session) -> Option<[u8; 32]> {
        use sha2::{Digest, Sha256};
        Some(Sha256::digest(serde_json::to_vec(source).ok()?).into())
    }

    fn remember(&mut self, id: TerminalId, sources: Vec<[u8; 32]>, reason: &str) {
        self.clear(id);
        if self.0.len() == Self::LIMIT {
            self.0.pop_front();
        }
        self.0.push_back(AdoptionFailure {
            id,
            sources,
            reason: reason[..reason.floor_char_boundary(Self::MESSAGE_BYTES)].into(),
        });
    }

    pub(super) fn get(&self, source: &Session) -> Option<&str> {
        let key = Self::source_key(source)?;
        self.0
            .iter()
            .find(|failure| failure.id == source.id && failure.sources.contains(&key))
            .map(|failure| failure.reason.as_str())
    }

    /// Explains a refusal to every descriptor a trusted upgrade checkpoint
    /// issued for this conversation, unless its authenticated holder already
    /// did. A refusal can precede holder authentication, as when the holder
    /// is gone, and the checkpoint is then the only authority on its aliases.
    pub(in crate::host) fn remember_upgrade(
        &mut self,
        saved: &crate::upgrade::handoff::Chat,
        reason: &str,
    ) {
        if self.get(&saved.source).is_some() {
            return;
        }
        let sources = saved
            .previous_sources
            .iter()
            .map(|previous| &previous.source)
            .chain(std::iter::once(&saved.source))
            .filter_map(Self::source_key)
            .collect();
        self.remember(saved.source.id, sources, reason);
    }

    pub(super) fn clear(&mut self, id: TerminalId) {
        self.0.retain(|failure| failure.id != id);
    }
}

pub(super) fn restore(
    host: &SharedHost,
    client: ClientId,
    source: &Session,
) -> Result<ServerResponse, String> {
    restore_with_admission(host, client, source, None)
}

pub(super) fn restore_with_admission(
    host: &SharedHost,
    client: ClientId,
    source: &Session,
    mut admission: Option<capacity::Reservation>,
) -> Result<ServerResponse, String> {
    // Main's durable holder format has no immutable account-home proof.
    super::super::account_lifecycle::ensure_idle(host).map_err(|error| error.message)?;
    let proof = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down
            || (state.chat_spawning.contains_key(&source.id) && admission.is_none())
            || state.native_closing.contains_key(&source.id)
        {
            return Err(
                "Native holder adoption is unavailable while another transition is pending".into(),
            );
        }
        if state
            .chats
            .get(&source.id)
            .is_some_and(|record| record.handle.owns_work())
        {
            return Err("The native conversation already has a live actor; attach to it".into());
        }
        if state.chat_handoffs.contains_key(&source.id)
            || state.chats.values().any(|record| {
                record.handle.owns_work()
                    && same_conversation(&record.session.launch, &source.launch)
            })
            || state.chat_spawning.iter().any(|(id, pending)| {
                *id != source.id && same_conversation(&pending.launch, &source.launch)
            })
        {
            return Err("Native conversation ownership or capacity prevents adoption".into());
        }
        // A prepublication failure still owns a slot. Only its complete issued
        // source may reuse that slot, even when the daemon has spare capacity.
        // This is admission bookkeeping; restore_reserved still authenticates
        // the retained holder before it can publish an actor.
        let retained_startup = if let Some(pending) = state.native_actor_startups.get(&source.id) {
            if &pending.source != source {
                return Err("The retained startup belongs to another exact descriptor".into());
            }
            true
        } else {
            false
        };
        if !state.chats.contains_key(&source.id) && admission.is_none() && !retained_startup {
            capacity::require_available(&state).map_err(|error| error.message)?;
        }
        capacity::Reservation::install(&mut admission, host, &mut state, source)
            .map_err(|error| error.message)?;
        state.inventory_generation += 1;
        state.proof().clone()
    };
    let result = restore_reserved(host, client, source, &proof);
    drop(admission);
    result
}

fn restore_reserved(
    host: &SharedHost,
    client: ClientId,
    source: &Session,
    proof: &HostSessionProof,
) -> Result<ServerResponse, String> {
    let directory = registry::directory(Kind::Native)?;
    let record = registry::recovery_candidate(
        &registry::socket(&directory, source.id),
        Kind::Native,
    )?
    .ok_or(
        "The durable native holder is unavailable; this conversation was not restarted or resent",
    )?;
    if Handle::is_remote_record(&record) {
        return super::retained_actor::restore(host, client, source, proof, record);
    }
    if source.launch.provider == Provider::Claude {
        return Err("The retained Claude owner uses an unsupported private protocol; its owner and evidence were preserved".into());
    }
    let (recovery, exited) = chat::native_holder::inspect(&record, proof)?;
    if exited {
        return Err("The retained native server exited; explicitly resume the conversation to start another server".into());
    }
    if &recovery.source != source && !recovery.previous_sources.contains(source) {
        return Err("Native holder does not recognize this exact issued descriptor".into());
    }
    // Only the authenticated holder's complete issued descriptors can retrieve
    // this failure. An unrecognized caller cannot replace another card's reason.
    let sources = recovery
        .previous_sources
        .iter()
        .chain(std::iter::once(&recovery.source))
        .filter_map(AdoptionFailures::source_key)
        .collect();
    let result = restore_authenticated(host, client, source, proof, record, recovery);
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    match &result {
        Ok(_) => state.native_adoption_failures.clear(source.id),
        Err(reason) => state
            .native_adoption_failures
            .remember(source.id, sources, reason),
    }
    drop(state);
    if result.is_ok() {
        super::super::state::retire_unattached_upgrade_chat(host, source.id);
    }
    result
}

fn restore_authenticated(
    host: &SharedHost,
    client: ClientId,
    source: &Session,
    proof: &HostSessionProof,
    record: registry::Record,
    recovery: chat::native_holder::Recovery,
) -> Result<ServerResponse, String> {
    let mut retained: Vec<_> = recovery
        .previous_sources
        .iter()
        .cloned()
        .chain(std::iter::once(recovery.source.clone()))
        .map(crate::agent_execution::HostedSession::Chat)
        .collect();
    let execution_registry = super::super::agent_execution::registry(host)?;
    let directory = execution_registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retained_agent_directory(&retained)?;
    let (mut spec, mut adapter) = chat::protocol::codex::create_restored(&recovery)?;
    spec.environment = recovery.execution_environment.0.clone();
    spec.execution =
        directory.map(|identity| Arc::new(chat::protocol::ExecutionContext::restored(identity)));
    spec.messages = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .mailbox_seal
        .as_ref()
        .map(|seal| Arc::new(seal.recognizer()));
    let execution_context = spec.execution.clone();
    let mut initial = adapter.initialize()?;
    let store = chat::runtime::preflight_retained(&spec, &mut initial)?;
    let (holder, recovery) = chat::native_holder::adopt(&record, source, proof)?;
    let mut session = recovery.source.clone();
    let execution_console = recovery.execution_console.clone();
    let paired_holder = recovery.paired_holder.clone();
    let expected_thread = recovery.confirmed_thread.clone();
    let mut sources = recovery.previous_sources.clone();
    sources.push(recovery.source.clone());
    let weak = Arc::downgrade(host);
    let id = session.id;
    let execution_instance = uuid::Uuid::new_v4();
    let handle = Handle::start_adopted(
        spec,
        adapter,
        initial,
        store,
        recovery,
        holder,
        move |update| {
            if let Update::Execution { run, update } = update {
                if let Some(host) = weak.upgrade() {
                    super::super::agent_execution::observed(
                        &host,
                        id,
                        execution_instance,
                        run,
                        update,
                    );
                }
            } else {
                changed(&weak, id, update);
            }
        },
    )?;
    // Installation is still reversible: close drops only this new connection
    // until the exact server confirms the retained thread and the host owns it.
    let confirmed = (|| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if crate::daemon::termination_requested()
                || host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down
            {
                return Err("Daemon shutdown interrupted native adoption".into());
            }
            if let Some(error) = handle.failure() {
                return Err(error);
            }
            if handle.is_completed() {
                return Err("Retained server disconnected during adoption".into());
            }
            if restored_thread_ready(&handle, expected_thread.as_deref())? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    "Retained server did not confirm durable continuity for its thread within 30 seconds"
                        .into(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    if let Err(error) = confirmed {
        // A responsive process is not proof of an attachable conversation.
        // In particular, Codex can retain metadata while removed account
        // storage makes its rollout unavailable. Preserve its owner without
        // claiming a live attachment or recreating the missing account.
        return match handle.close() {
            Ok(()) => Err(reattachment_failure(&error, None)),
            Err(cleanup) => Err(reattachment_failure(&error, Some(&cleanup))),
        };
    }
    let paired = (|| {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err("Daemon shutdown interrupted native adoption".to_string());
        }
        let Some(terminal) = &mut session.terminal else {
            return Ok(None);
        };
        // Terminal recovery has already authenticated its own holder lineage.
        // Keep that exact worker through the native holder's acknowledgment;
        // an equal recipe alone cannot authorize a concurrent replacement.
        let (projected, worker) =
            project_paired_terminal(&state, terminal, paired_holder.as_ref(), proof)?;
        *terminal = projected;
        Ok(Some(worker))
    })();
    let paired = match paired {
        Ok(paired) => paired,
        Err(error) => {
            let _ = handle.close();
            return Err(error);
        }
    };
    if let Err(error) = handle.update_native_source(
        session.clone(),
        execution_console.clone(),
        paired
            .as_ref()
            .and_then(|handle| handle.holder_identity().cloned()),
    ) {
        let _ = handle.close();
        return Err(error);
    }
    retained.extend(
        sources
            .iter()
            .cloned()
            .map(crate::agent_execution::HostedSession::Chat),
    );
    if let Err(error) = execution_registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .adopt_authenticated_agent_session(
            &retained,
            &crate::agent_execution::HostedSession::Chat(session.clone()),
        )
    {
        let _ = handle.close();
        return Err(error);
    }
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let pair_unchanged =
        pair_projection_is_current(&state, session.terminal.as_ref(), paired.as_ref());
    if state.shutting_down || !pair_unchanged {
        drop(state);
        let _ = handle.close();
        return Err(
            "Native adoption was interrupted before its paired ownership could be installed".into(),
        );
    }
    let observation = Observation {
        after_generation: handle.observe_next_update(),
        ..Observation::default()
    };
    let previous_sources = retained_upgrade_sources(sources, &session);
    let record = Record {
        execution_instance,
        console_recipe: execution_console,
        paired_terminal: paired,
        session,
        handle,
        source_process: None,
        subscribers: if client == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(client, false)])
        },
        observations: if client == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(client, observation)])
        },
        completion_observed: false,
        returning_terminal: false,
    };
    drop(state);
    let response = session_snapshot(&record.session, &record.handle)?;
    #[cfg(test)]
    BEFORE_PUBLICATION.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action(host);
        }
    });
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down
        || state.proof() != proof
        || state
            .chats
            .get(&id)
            .is_some_and(|current| current.handle.owns_work())
        || !pair_projection_is_current(
            &state,
            record.session.terminal.as_ref(),
            record.paired_terminal.as_ref(),
        )
    {
        drop(state);
        let _ = record.handle.close();
        return Err("Native adoption changed before attachment publication".into());
    }
    // This path constructs a Local adopted handle: committing is an atomic
    // flag, not a holder RPC. Keep it uncommitted through every refusal above
    // so dropping the failed attachment preserves the original provider.
    record.handle.commit_native_installation()?;
    let retired = state.chats.insert(id, record);
    if let Some(context) = execution_context {
        context.installed();
    }
    state.upgrade_chats.insert(id, previous_sources);
    state.inventory_generation += 1;
    drop(state);
    drop(retired);
    Ok(response)
}

pub(super) fn retained_upgrade_sources(
    sources: Vec<Session>,
    successor: &Session,
) -> Vec<crate::upgrade::handoff::ChatSource> {
    sources
        .into_iter()
        .filter(|source| source != successor)
        .map(|source| crate::upgrade::handoff::ChatSource {
            // The holder authenticated these immutable aliases, including the
            // first descriptor issued before the provider named its thread.
            // Their effective continuation is the now-confirmed same thread.
            launch: successor.launch.clone(),
            source,
        })
        .collect()
}

/// `refused` names conversations whose reattachment this startup already
/// attempted and refused, as an upgrade restore's unattached ones: retrying
/// them at once would only repeat that refusal after another confirmation
/// wait. A later discovery still considers them.
pub(in crate::host) fn restore_orphaned_native(
    host: &SharedHost,
    refused: &[TerminalId],
) -> Result<(), String> {
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let directory = registry::directory(Kind::Native)?;
    for record in registry::recovery_candidates(&directory, &proof.boot_id, Kind::Native)? {
        if refused.contains(&record.terminal) {
            continue;
        }
        if host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .contains_key(&record.terminal)
        {
            continue;
        }
        let result = if Handle::is_remote_record(&record) {
            Handle::inspect_remote(&record, proof.clone())
                .and_then(|inspection| restore(host, 0, &inspection.source))
        } else {
            chat::native_holder::inspect(&record, &proof)
                .and_then(|(recovery, _)| restore(host, 0, &recovery.source))
        };
        if let Err(error) = result {
            tracing::warn!(session = %record.terminal, %error, "retained native conversation could not be reattached; holder evidence preserved");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adoption_diagnostics_are_bounded_exact_and_retired() {
        let source = crate::chat::fresh_session(
            Provider::Codex,
            TerminalId::new(),
            "/var/tmp".into(),
            &crate::account::Account::system(Provider::Codex),
        )
        .unwrap();
        let mut failures = AdoptionFailures::default();
        let key = AdoptionFailures::source_key(&source).unwrap();
        failures.remember(source.id, vec![key], &"é".repeat(4096));
        assert_eq!(
            failures.get(&source).unwrap().len(),
            AdoptionFailures::MESSAGE_BYTES
        );
        let mut altered = source.clone();
        altered.launch.recipe.arguments.push("--different".into());
        assert!(failures.get(&altered).is_none());
        failures.remember(source.id, vec![key], "latest provider refusal");
        assert_eq!(failures.0.len(), 1);
        assert_eq!(failures.get(&source), Some("latest provider refusal"));
        for _ in 0..AdoptionFailures::LIMIT {
            failures.remember(TerminalId::new(), vec![], "unavailable");
        }
        assert_eq!(failures.0.len(), AdoptionFailures::LIMIT);
        assert!(
            failures.get(&source).is_none(),
            "oldest diagnostic is evicted"
        );
        failures.remember(source.id, vec![key], "reattachment failed");
        failures.clear(source.id);
        assert!(
            failures.get(&source).is_none(),
            "successful restore/cleanup clears the refusal"
        );
    }

    #[test]
    fn bounded_adoption_failure_keeps_diagnosis_and_cleanup_before_long_rpc_text() {
        let source = crate::chat::fresh_session(
            Provider::Codex,
            TerminalId::new(),
            "/var/tmp".into(),
            &crate::account::Account::system(Provider::Codex),
        )
        .unwrap();
        let key = AdoptionFailures::source_key(&source).unwrap();
        let diagnostic = format!(
            "Native provider exited with status 7\nProvider stderr tail (earlier bytes omitted; stderr capture incomplete; stderr read failed; invalid UTF-8 replaced for display):\n{}",
            "é".repeat(512)
        );
        let raw = format!("Codex RPC error: {}RAW_RPC_END", "é".repeat(4096));
        let failure = format!("{diagnostic}\n{raw}");
        let cleanup = format!(
            "original holder refused Close: {}CLEANUP_END",
            "界".repeat(1024)
        );
        for cleanup in [None, Some(cleanup.as_str())] {
            let reason = reattachment_failure(&failure, cleanup);
            let mut failures = AdoptionFailures::default();
            failures.remember(source.id, vec![key], &reason);
            let cached = failures.get(&source).unwrap();
            assert!(cached.len() <= AdoptionFailures::MESSAGE_BYTES);
            assert!(cached.contains(&diagnostic));
            assert!(cached.contains("no replacement was started or prompt resent"));
            assert!(cached.contains("Codex RPC error: é"));
            assert!(!cached.contains("RAW_RPC_END"));
            if cleanup.is_some() {
                assert!(cached.contains("Local attachment cleanup is unconfirmed"));
                assert!(cached.contains("original holder refused Close:"));
                assert!(cached.contains("[later cleanup details omitted]"));
                assert!(!cached.contains("CLEANUP_END"));
            } else {
                assert!(cached.contains("Provider ownership was preserved"));
            }
            let mut altered = source.clone();
            altered.launch.recipe.arguments.push("--different".into());
            assert!(failures.get(&altered).is_none());
        }
    }

    #[test]
    fn crash_adoption_preserves_unnamed_aliases_for_a_later_planned_upgrade() {
        use crate::{
            chat::{Launch, NativeContinuity, OwnedCodexListener},
            model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalId},
            upgrade::handoff::{Chat, Checkpoint},
        };
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("original"),
            host_incarnation: HostIncarnation::new("original-incarnation"),
            boot_id: BootId::new("same-boot"),
        };
        let id = TerminalId::new();
        let socket = std::path::PathBuf::from(format!("/var/tmp/cagents-codex-{id}/control.sock"));
        let unnamed = Session {
            id,
            launch: Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "codex".into(),
                    arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
                    working_directory: "/var/tmp".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: Some(OwnedCodexListener {
                socket,
                allocator: proof,
            }),
            native_continuity: NativeContinuity::ReconnectOnly,
        };
        let mut confirmed = unnamed.clone();
        confirmed.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        confirmed.native_continuity = NativeContinuity::Durable {
            provider_version: "0.157.1".into(),
        };
        chat::protocol::align_resume_identity(&mut confirmed.launch).unwrap();
        let mut successor = confirmed.clone();
        successor.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("successor");
        let sources = vec![unnamed.clone(), confirmed.clone(), successor.clone()];
        let previous_sources = retained_upgrade_sources(sources, &successor);
        assert_eq!(previous_sources.len(), 2);
        assert_eq!(
            previous_sources[0].source, unnamed,
            "issued descriptors stay immutable"
        );
        assert_eq!(previous_sources[1].source, confirmed);
        let checkpoint = Checkpoint::new(
            successor.proof.clone().unwrap(),
            Vec::new(),
            Vec::new(),
            vec![Chat {
                launch: successor.launch.clone(),
                source: successor,
                previous_sources,
            }],
        );
        checkpoint.validate().unwrap();
        let mut wrong_thread = checkpoint.clone();
        wrong_thread.chats[0].previous_sources[0].launch.session_id =
            Some(uuid::Uuid::new_v4().to_string());
        assert!(wrong_thread.validate().is_err());
        let mut wrong_alias_thread = checkpoint.clone();
        wrong_alias_thread.chats[0].previous_sources[1]
            .source
            .launch
            .session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(wrong_alias_thread.validate().is_err());
        let mut old_boot = checkpoint.clone();
        old_boot.chats[0].previous_sources[0]
            .source
            .proof
            .as_mut()
            .unwrap()
            .boot_id = BootId::new("different-boot");
        assert!(old_boot.validate().is_err());
        let mut duplicate = checkpoint.clone();
        let alias = duplicate.chats[0].previous_sources[0].clone();
        duplicate.chats[0].previous_sources.push(alias);
        assert!(duplicate.validate().is_err());
        let mut ordinary = checkpoint;
        ordinary.chats[0].source.native_continuity = NativeContinuity::ReconnectOnly;
        assert!(
            ordinary.validate().is_err(),
            "ordinary relaunches still require distinct daemon proofs"
        );
    }
}
