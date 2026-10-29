//! Exact admission unit regressions plus real helper semantic retries. The
//! in-memory proxy fixtures below never exercise helper authentication; the
//! isolated tests use the production constructor and close protocol.
use super::*;
use crate::chat::protocol::{LaunchCleanup, LaunchOutcome};
#[cfg(target_os = "linux")]
use crate::chat::runtime::startup_observation::{Event, Observation, Spawned};

/// Installed before the actual constructor. This owns observation of only its
/// exact source/context and never signals a PID, provider or descendant.
#[cfg(target_os = "linux")]
struct UninstalledFixture {
    source: Session,
    context: Arc<chat::protocol::ExecutionContext>,
    observation: Observation,
    spawned: Option<Spawned>,
    spawn_error: Option<String>,
    waited: Option<std::process::ExitStatus>,
    repairs: Vec<(std::path::PathBuf, &'static str)>,
    attempted: bool,
}

#[cfg(target_os = "linux")]
impl UninstalledFixture {
    fn new(source: Session, context: Arc<chat::protocol::ExecutionContext>) -> Self {
        Self {
            observation: Observation::install(source.clone(), context.clone()),
            source,
            context,
            spawned: None,
            spawn_error: None,
            waited: None,
            repairs: Vec::new(),
            attempted: false,
        }
    }

    fn repair_on_failure(&mut self, path: std::path::PathBuf, sql: &'static str) {
        self.repairs.push((path, sql));
    }

    fn observe(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Spawned(result) => {
                if self.spawned.is_some() || self.spawn_error.is_some() {
                    return Err("duplicate original helper spawn observation".into());
                }
                match result {
                    Ok(spawned) => self.spawned = Some(spawned),
                    Err(error) => self.spawn_error = Some(error),
                }
            }
            Event::Reaped(result) => {
                if self.waited.is_some() {
                    return Err("duplicate original sole Child.wait observation".into());
                }
                self.waited = Some(result?);
            }
        }
        Ok(())
    }

    fn finish(&mut self, failure_only: bool) -> Result<(), String> {
        use std::os::fd::AsRawFd;
        self.attempted = true;
        while let Ok(event) = self.observation.events.try_recv() {
            self.observe(event)?;
        }
        if failure_only {
            for (path, sql) in &self.repairs {
                if let Err(error) = rusqlite::Connection::open(path)
                    .map_err(|error| error.to_string())
                    .and_then(|db| db.execute_batch(sql).map_err(|error| error.to_string()))
                {
                    eprintln!("Targeted fixture fault repair failed: {error}");
                }
            }
            // This is failure-only rollback, never the semantic oracle. The
            // fixture source never calls commit_native_installation. The
            // narrow test seam authenticates installed=false again and closes
            // only the shared ORIGINAL startup stream; all Handle clones see
            // that release. No cleanup outcome is written here.
            let owner = self.context.request_ordinary_cleanup();
            if self.waited.is_none()
                && let Some(original) = owner.remote
            {
                let identity = original
                    .actor_identity()
                    .ok_or("original startup attachment lost its exact actor")?;
                if identity.record.terminal != self.source.id
                    || self
                        .spawned
                        .as_ref()
                        .is_some_and(|spawned| identity.record.holder != spawned.process)
                {
                    return Err(
                        "original startup attachment differs from the observed helper".into(),
                    );
                }
                if let Ok(inspection) = Handle::inspect_remote(
                    &identity.record,
                    self.source
                        .proof
                        .clone()
                        .ok_or("fixture source lost its original proof")?,
                ) {
                    if inspection.identity != identity
                        || inspection.state.installed
                        || (inspection.source != self.source
                            && !inspection.previous_sources.contains(&self.source))
                    {
                        return Err(
                            "uninstalled rollback lost its exact authenticated source".into()
                        );
                    }
                }
                if let Err(error) = original.rollback_uninstalled_for_test() {
                    // An already retired helper can finish its original wait
                    // before inspection. Only the actual receipt below can
                    // settle that race; this refusal is never proof.
                    eprintln!("Failure-only original startup rollback request: {error}");
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.waited.is_none() {
            let captured = self.context.ownership_snapshot();
            let current = self.context.poll_cleanup(&captured);
            if self.spawned.is_none()
                && self.spawn_error.is_none()
                && current.ordinary_requested
                && current.outcome == LaunchOutcome::NoProvider
            {
                eprintln!("Failure-only fixture cleanup settled without any helper spawn");
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("original helper sole Child.wait remains unconfirmed; fixture evidence retained".into());
            }
            let event = self
                .observation
                .events
                .recv_timeout(remaining)
                .map_err(|error| {
                    format!("original helper sole Child.wait did not settle: {error}")
                })?;
            self.observe(event)?;
        }
        if self
            .waited
            .as_ref()
            .and_then(std::process::ExitStatus::code)
            != Some(73)
        {
            return Err(
                "the actual original sole Child.wait did not return teardown status73".into(),
            );
        }
        let helper = self.spawned.as_ref().ok_or_else(|| {
            format!(
                "original helper spawn identity/pidfd was not captured: {:?}",
                self.spawn_error
            )
        })?;
        let mut poll = libc::pollfd {
            fd: helper.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 0) } < 0 || poll.revents & libc::POLLIN == 0 {
            return Err(
                "the original helper pidfd did not confirm exit after its actual wait".into(),
            );
        }
        match crate::daemon::holder_registry::Process::read(helper.process.pid) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(current) if current != helper.process => {}
            _ => return Err("the original helper remains unreaped after its actual wait".into()),
        }
        eprintln!(
            "Original helper fixture settled: failure_only_cleanup={failure_only}, sole_Child_wait_exit=73, pidfd_exited=true, original_reaped=true"
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for UninstalledFixture {
    fn drop(&mut self) {
        if !self.attempted
            && let Err(error) = self.finish(true)
        {
            eprintln!(
                "Exact uninstalled fixture cleanup is unconfirmed; evidence retained: {error}"
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn finish_fixture(
    guard: &mut UninstalledFixture,
    checked: std::thread::Result<()>,
    root: &std::path::Path,
) {
    guard.finish(checked.is_err()).unwrap_or_else(|error| {
        panic!("Original fixture cleanup remains unconfirmed; evidence retained: {error}")
    });
    if let Err(panic) = checked {
        // Manual rollback cannot turn the original semantic assertion into a
        // pass. Rethrow only after the original sole waiter and kernel agree.
        std::panic::resume_unwind(panic);
    }
    std::fs::write(
        root.join("original-helper-settled"),
        "actual original Child.wait73\n",
    )
    .unwrap();
}

struct Pending;
impl LaunchCleanup for Pending {
    fn cleanup(&self) -> Result<LaunchOutcome, String> {
        Ok(LaunchOutcome::Uncertain)
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    host: SharedHost,
    pending: Startup,
    identity: chat::runtime::ActorIdentity,
    attachment: Handle,
    successor: Handle,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let (attachment, successor) = Handle::fixture_remote_attachment_pair();
        let identity = attachment.actor_identity().unwrap();
        let proof = HostSessionProof {
            host_session_id: "semantic-startup-capture".into(),
            boot_id: identity.record.boot.clone(),
            host_incarnation: "original".into(),
        };
        let source = Session {
            id: identity.record.terminal,
            launch: chat::Launch {
                provider: Provider::Claude,
                session_id: None,
                recipe: crate::model::LaunchRecipe {
                    program: "claude".into(),
                    arguments: Vec::new(),
                    working_directory: root.path().into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        };
        let context = Arc::new(chat::protocol::ExecutionContext::default());
        let token = context.claim_startup_owner(Arc::new(Pending));
        assert!(context.transfer_startup_owner(token, Arc::new(Pending), identity.clone()));
        let pending = Startup {
            source: source.clone(),
            requested: source,
            context,
            failed_close: None,
        };
        let mut state = HostState::new(proof);
        state.install_model(
            crate::host::model::Registry::at(&root.path().join("registry.sqlite3")).unwrap(),
        );
        state
            .native_actor_startups
            .insert(pending.source.id, pending.clone());
        state.begin_ordinary_exit_cleanup();
        let host = Arc::new(HostLock::new(state));
        pending.context.request_ordinary_cleanup();
        Self {
            _root: root,
            host,
            pending,
            identity,
            attachment,
            successor,
        }
    }

    fn capture(&self) -> Capture {
        capture_startup(&self.host.lock().unwrap(), &self.pending).unwrap()
    }

    fn record(&self, handle: Handle) -> Record {
        Record {
            execution_instance: self.identity.instance,
            console_recipe: None,
            paired_terminal: None,
            session: self.pending.source.clone(),
            handle,
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: true,
        }
    }

    fn normal_host(&self) -> SharedHost {
        let mut state = HostState::new(self.host.lock().unwrap().proof().clone());
        state
            .native_actor_startups
            .insert(self.pending.source.id, self.pending.clone());
        state
            .chats
            .insert(self.pending.source.id, self.record(self.attachment.clone()));
        Arc::new(HostLock::new(state))
    }
}

#[test]
fn captured_startup_refuses_changed_descriptors_context_and_failed_target() {
    let fixture = Fixture::new();
    let capture = fixture.capture();
    assert!(capture.matches_startup(&fixture.pending));
    for mutation in 0..4 {
        let mut successor = fixture.pending.clone();
        match mutation {
            0 => successor
                .source
                .launch
                .recipe
                .arguments
                .push("changed-source".into()),
            1 => successor
                .requested
                .launch
                .recipe
                .arguments
                .push("changed-request".into()),
            2 => successor.context = Arc::new(chat::protocol::ExecutionContext::default()),
            3 => {
                successor.failed_close = Some(super::super::super::CloseTarget::from_record(
                    &fixture.record(fixture.attachment.clone()),
                ))
            }
            _ => unreachable!(),
        }
        fixture
            .host
            .lock()
            .unwrap()
            .native_actor_startups
            .insert(successor.source.id, successor.clone());
        assert!(!capture.matches_startup(&successor));
        assert!(capture.require_host(&fixture.host).is_err());
        fixture
            .host
            .lock()
            .unwrap()
            .native_actor_startups
            .insert(fixture.pending.source.id, fixture.pending.clone());
    }
    // A late startup claim cannot downgrade permanent semantic ownership or
    // replace the exact generation captured by ordinary shutdown.
    let before = fixture.pending.context.ownership_snapshot();
    let token = fixture
        .pending
        .context
        .claim_startup_owner(Arc::new(Pending));
    assert_eq!(Some(token), before.token);
    assert_eq!(
        fixture.pending.context.ownership_snapshot().kind,
        before.kind
    );
    assert!(capture.matches_startup(&fixture.pending));
    assert!(capture.require_host(&fixture.host).is_ok());
}

#[test]
fn transient_stop_quiescence_does_not_mint_ordinary_cleanup_authority() {
    let fixture = Fixture::new();
    let mut transient = HostState::new(fixture.host.lock().unwrap().proof().clone());
    // Accepted Stop can quiesce admission before ordinary cleanup has been
    // atomically established. That transient state is not a cleanup permit.
    transient.shutting_down = true;
    transient
        .native_actor_startups
        .insert(fixture.pending.source.id, fixture.pending.clone());
    transient.chats.insert(
        fixture.pending.source.id,
        fixture.record(fixture.attachment.clone()),
    );
    assert!(capture_startup(&transient, &fixture.pending).is_err());
    assert!(capture_hosted(&transient, &fixture.pending.source, &fixture.attachment).is_err());
    assert!(fixture.capture().require_state(&transient).is_err());
}

#[test]
fn execution_capture_without_native_row_refuses_replaced_intent_and_context() {
    let fixture = Fixture::new();
    let agent = crate::model::AgentId::new();
    let intent = crate::agent_execution::HostedSession::Chat(fixture.pending.source.clone());
    {
        let mut state = fixture.host.lock().unwrap();
        state.native_actor_startups.clear();
        state
            .execution_startups
            .insert(agent, (intent.clone(), fixture.pending.context.clone()));
    }
    let capture = capture_execution(
        &fixture.host.lock().unwrap(),
        agent,
        &intent,
        &fixture.pending.context,
    )
    .unwrap();
    assert!(capture.require_host(&fixture.host).is_ok());
    let mut successor = fixture.pending.source.clone();
    successor.launch.recipe.arguments.push("successor".into());
    fixture.host.lock().unwrap().execution_startups.insert(
        agent,
        (
            crate::agent_execution::HostedSession::Chat(successor),
            fixture.pending.context.clone(),
        ),
    );
    assert!(capture.require_host(&fixture.host).is_err());
    fixture.host.lock().unwrap().execution_startups.insert(
        agent,
        (
            intent.clone(),
            Arc::new(chat::protocol::ExecutionContext::default()),
        ),
    );
    assert!(capture.require_host(&fixture.host).is_err());
    fixture
        .host
        .lock()
        .unwrap()
        .execution_startups
        .insert(agent, (intent, fixture.pending.context.clone()));
    assert!(capture.require_host(&fixture.host).is_ok());
}

#[test]
fn captured_attachment_refuses_same_actor_replacement_and_keeps_public_close_fence() {
    let fixture = Fixture::new();
    fixture.host.lock().unwrap().chats.insert(
        fixture.pending.source.id,
        fixture.record(fixture.attachment.clone()),
    );
    let capture = capture_hosted(
        &fixture.host.lock().unwrap(),
        &fixture.pending.source,
        &fixture.attachment,
    )
    .unwrap();
    let target =
        super::super::super::CloseTarget::from_record(&fixture.record(fixture.attachment.clone()));
    assert!(target.require_current(&fixture.host).is_err());
    assert!(
        target
            .require_current_with_shutdown(&fixture.host, Some(&capture))
            .is_err()
    );
    fixture
        .host
        .lock()
        .unwrap()
        .chat_spawning
        .insert(fixture.pending.source.id, fixture.pending.source.clone());
    assert!(
        target
            .require_current_with_shutdown(&fixture.host, Some(&capture))
            .is_ok()
    );
    fixture.host.lock().unwrap().chats.insert(
        fixture.pending.source.id,
        fixture.record(fixture.successor.clone()),
    );
    assert!(capture.require_host(&fixture.host).is_err());
    assert!(
        target
            .require_current_with_shutdown(&fixture.host, Some(&capture))
            .is_err()
    );
}

#[test]
fn failed_target_capture_refuses_another_attachment_of_the_same_actor() {
    let mut fixture = Fixture::new();
    let original = fixture.record(fixture.attachment.clone());
    fixture.pending.failed_close = Some(super::super::super::CloseTarget::from_record(&original));
    {
        let mut state = fixture.host.lock().unwrap();
        state.chats.insert(fixture.pending.source.id, original);
        state
            .native_actor_startups
            .insert(fixture.pending.source.id, fixture.pending.clone());
    }
    let capture = fixture.capture();
    let mut successor = fixture.pending.clone();
    successor.failed_close = Some(super::super::super::CloseTarget::from_record(
        &fixture.record(fixture.successor.clone()),
    ));
    fixture
        .host
        .lock()
        .unwrap()
        .native_actor_startups
        .insert(successor.source.id, successor.clone());
    assert!(!capture.matches_startup(&successor));
    assert!(capture.require_host(&fixture.host).is_err());
}

#[test]
fn startup_capture_never_enrolls_an_installed_unknown_or_changed_actor() {
    let fixture = Fixture::new();
    let capture = fixture.capture();
    let source = &fixture.pending.source;
    let refusal = capture
        .enroll_uninstalled(
            &fixture.host,
            &fixture.identity,
            source,
            &[],
            Err("captured unknown scope"),
            true,
        )
        .unwrap_err();
    assert!(refusal.contains("no prior durable ownership intent"));
    let mut changed = fixture.identity.clone();
    changed.instance = uuid::Uuid::new_v4();
    assert!(
        capture
            .enroll_uninstalled(
                &fixture.host,
                &changed,
                source,
                &[],
                Err("captured unknown scope"),
                false
            )
            .is_err()
    );
    let registry = crate::host::agent_execution::registry(&fixture.host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_ownerships(None, 128)
            .unwrap()
            .is_empty()
    );
    capture
        .enroll_uninstalled(
            &fixture.host,
            &fixture.identity,
            source,
            &[],
            Err("captured unknown scope"),
            false,
        )
        .unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_ownership(fixture.identity.instance)
            .unwrap()
            .unwrap()
            .is_pending()
    );
    // Normal publication may not consume ordinary shutdown's exact evidence.
    forget_startup(&fixture.host, source, &fixture.pending.context);
    assert!(
        fixture
            .host
            .lock()
            .unwrap()
            .native_actor_startups
            .contains_key(&source.id)
    );
}

#[test]
fn absent_failed_target_capture_requires_retirement_and_rejects_every_host_successor() {
    let mut fixture = Fixture::new();
    fixture.pending.failed_close = Some(super::super::super::CloseTarget::from_record(
        &fixture.record(fixture.attachment.clone()),
    ));
    let agent = crate::model::AgentId::new();
    let intent = crate::agent_execution::HostedSession::Chat(fixture.pending.source.clone());
    {
        let mut state = fixture.host.lock().unwrap();
        state
            .native_actor_startups
            .insert(fixture.pending.source.id, fixture.pending.clone());
        state
            .execution_startups
            .insert(agent, (intent.clone(), fixture.pending.context.clone()));
    }
    let capture = fixture.capture();
    let execution = capture_execution(
        &fixture.host.lock().unwrap(),
        agent,
        &intent,
        &fixture.pending.context,
    )
    .unwrap();
    for captured in [&capture, &execution] {
        assert!(captured.require_host(&fixture.host).is_ok());
        assert!(captured.require_live_adoption().is_err());
        assert!(
            require_verified_failed_retirement(captured, RetirementInspection::Running).is_err()
        );
        assert!(
            require_verified_failed_retirement(captured, RetirementInspection::ExitedUnproved)
                .is_err()
        );
    }
    // Even the old exact attachment reappearing is a changed HOST state. The
    // receipt-only capture cannot use it or a successor as live close input.
    for handle in [&fixture.attachment, &fixture.successor] {
        fixture
            .host
            .lock()
            .unwrap()
            .chats
            .insert(fixture.pending.source.id, fixture.record(handle.clone()));
        assert!(capture.require_host(&fixture.host).is_err());
        assert!(execution.require_host(&fixture.host).is_err());
        fixture
            .host
            .lock()
            .unwrap()
            .chats
            .remove(&fixture.pending.source.id);
    }
    let mut stale = fixture.pending.clone();
    stale.failed_close = Some(super::super::super::CloseTarget::from_record(
        &fixture.record(fixture.successor.clone()),
    ));
    fixture
        .host
        .lock()
        .unwrap()
        .native_actor_startups
        .insert(stale.source.id, stale);
    assert!(capture.require_host(&fixture.host).is_err());
    assert!(execution.require_host(&fixture.host).is_err());
    let (foreign, _) = Handle::fixture_remote_attachment_pair();
    fixture.pending.failed_close = Some(super::super::super::CloseTarget::from_record(
        &fixture.record(foreign),
    ));
    fixture
        .host
        .lock()
        .unwrap()
        .native_actor_startups
        .insert(fixture.pending.source.id, fixture.pending.clone());
    assert!(capture_startup(&fixture.host.lock().unwrap(), &fixture.pending).is_err());
}

#[test]
fn normal_failed_target_replacement_after_capture_is_refused_before_reservation() {
    let mut fixture = Fixture::new();
    let original = fixture.record(fixture.attachment.clone());
    let captured = super::super::super::CloseTarget::from_record(&original);
    fixture.pending.failed_close = Some(captured.clone());
    let mut state = HostState::new(fixture.host.lock().unwrap().proof().clone());
    state
        .native_actor_startups
        .insert(fixture.pending.source.id, fixture.pending.clone());
    state.chats.insert(fixture.pending.source.id, original);
    let binding = StartupBinding::capture(&fixture.pending).unwrap();
    binding.require_state(&state).unwrap();
    // This is the interval between the normal helper's preflight and the
    // shared close reservation. The actor is the same, the attachment is not.
    state.chats.insert(
        fixture.pending.source.id,
        fixture.record(fixture.successor.clone()),
    );
    assert!(binding.require_state(&state).is_err());
    let host = Arc::new(HostLock::new(state));
    let refusal = close_with_target(
        &host,
        &captured.source,
        Some(fixture.identity.record.clone()),
        Some(&captured),
        None,
        None,
        true,
        false,
    )
    .unwrap_err();
    assert!(
        refusal.contains("exact hosted attachment changed"),
        "{refusal}"
    );
    assert!(host.lock().unwrap().chat_spawning.is_empty());
    assert!(
        host.lock().unwrap().chats[&fixture.pending.source.id]
            .handle
            .same_attachment(&fixture.successor)
    );
}

#[test]
fn named_failure_publication_serializes_with_ordinary_intent_in_both_orders() {
    let fixture = Fixture::new();
    let host = fixture.normal_host();
    let handle = fixture.attachment.clone();
    let source = fixture.pending.source.clone();
    let requested = fixture.pending.requested.clone();
    let context = fixture.pending.context.clone();
    let instance = fixture.identity.instance;
    let worker_host = host.clone();
    let (entered, started) = std::sync::mpsc::channel();
    let (finished, completion) = std::sync::mpsc::channel();
    let mut held = host.lock().unwrap();
    let worker = thread::spawn(move || {
        entered.send(()).unwrap();
        let result = super::super::super::capture_failed_startup_close(
            &worker_host,
            &handle,
            instance,
            &source,
            &requested,
            Some(&context),
            "fixture named confirmation failure",
        )
        .map(|_| ())
        .map_err(|error| error.message);
        finished.send(result).unwrap();
    });
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        completion.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    held.begin_ordinary_exit_cleanup();
    let frozen_none = held.native_actor_startups[&fixture.pending.source.id].clone();
    assert!(frozen_none.failed_close.is_none());
    drop(held);
    let refusal = completion
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap_err();
    worker.join().unwrap();
    assert!(refusal.contains("Ordinary shutdown retains"), "{refusal}");
    assert!(
        host.lock().unwrap().native_actor_startups[&fixture.pending.source.id]
            .matches(&frozen_none)
    );

    let host = fixture.normal_host();
    super::super::super::capture_failed_startup_close(
        &host,
        &fixture.attachment,
        fixture.identity.instance,
        &fixture.pending.source,
        &fixture.pending.requested,
        Some(&fixture.pending.context),
        "fixture named confirmation failure",
    )
    .unwrap();
    let frozen_some = {
        let mut state = host.lock().unwrap();
        assert!(
            state.native_actor_startups[&fixture.pending.source.id]
                .failed_close
                .is_some()
        );
        state.begin_ordinary_exit_cleanup();
        state.native_actor_startups[&fixture.pending.source.id].clone()
    };
    assert!(
        super::super::super::capture_failed_startup_close(
            &host,
            &fixture.attachment,
            fixture.identity.instance,
            &fixture.pending.source,
            &fixture.pending.requested,
            Some(&fixture.pending.context),
            "fixture named confirmation failure",
        )
        .is_err()
    );
    assert!(
        host.lock().unwrap().native_actor_startups[&fixture.pending.source.id]
            .matches(&frozen_some)
    );
}

#[test]
fn named_failure_publication_refuses_changed_startup_context_or_descriptor() {
    let fixture = Fixture::new();
    for mutation in 0..3 {
        let host = fixture.normal_host();
        {
            let mut state = host.lock().unwrap();
            let pending = state
                .native_actor_startups
                .get_mut(&fixture.pending.source.id)
                .unwrap();
            match mutation {
                0 => pending
                    .source
                    .launch
                    .recipe
                    .arguments
                    .push("successor source".into()),
                1 => pending
                    .requested
                    .launch
                    .recipe
                    .arguments
                    .push("successor request".into()),
                2 => pending.context = Arc::new(chat::protocol::ExecutionContext::default()),
                _ => unreachable!(),
            }
        }
        assert!(
            super::super::super::capture_failed_startup_close(
                &host,
                &fixture.attachment,
                fixture.identity.instance,
                &fixture.pending.source,
                &fixture.pending.requested,
                Some(&fixture.pending.context),
                "fixture named confirmation failure",
            )
            .is_err()
        );
        assert!(
            host.lock().unwrap().native_actor_startups[&fixture.pending.source.id]
                .failed_close
                .is_none()
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn ordinary_semantic_persistence_failure_retries_in_the_same_host() {
    let Some(root) = isolated_child("CAGENTS_ORDINARY_SEMANTIC_RETRY_TEST") else {
        return;
    };
    fn wait(mut predicate: impl FnMut() -> bool, reason: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !predicate() {
            assert!(Instant::now() < deadline, "{reason}");
            thread::sleep(Duration::from_millis(20));
        }
    }
    install_boot_permission_provider(&root.join("bin/claude"));
    let proof = HostSessionProof {
        host_session_id: "ordinary-semantic-retry".into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: "same-daemon".into(),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let mut state = HostState::new(proof.clone());
    state.install_model(crate::host::model::Registry::at(&root.join("registry.sqlite3")).unwrap());
    let host = Arc::new(HostLock::new(state));
    let _notifications = crate::host::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(_notifications.handle());
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.proof = Some(proof.clone());
    let registry = crate::host::agent_execution::registry(&host).unwrap();
    registry
        .lock()
        .unwrap()
        .fresh_agent_authority(&crate::agent_execution::HostedSession::Chat(source.clone()))
        .unwrap();
    let context =
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    let mut guard = UninstalledFixture::new(source.clone(), context.clone());
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (mut spec, adapter) = chat::protocol::create(&source.launch).unwrap();
        spec.execution = Some(context.clone());
        let prepared = adapter.prepared_launch_identity().unwrap();
        let environment = spec.environment.clone();
        let original =
            Handle::start_remote_owned(spec, source.clone(), prepared, environment, |_| {})
                .unwrap();
        let identity = original.actor_identity().unwrap();
        wait(
            || original.attachment_snapshot().ready,
            "actual helper did not initialize its provider",
        );
        std::fs::write(root.join("emit-boot-permission"), "emit").unwrap();
        wait(
            || {
                Handle::inspect_remote(&identity.record, proof.clone()).is_ok_and(|inspection| {
                    inspection.state.outbox_latest > inspection.state.outbox_ack
                })
            },
            "provider emitted no semantic outbox evidence",
        );
        let pending = Startup {
            source: source.clone(),
            requested: source.clone(),
            context: context.clone(),
            failed_close: None,
        };
        {
            let mut state = host.lock().unwrap();
            state
                .native_actor_startups
                .insert(source.id, pending.clone());
            state.begin_ordinary_exit_cleanup();
        }
        context.request_ordinary_cleanup();
        let capture = capture_startup(&host.lock().unwrap(), &pending).unwrap();
        let attention = crate::instance::project_dirs()
            .unwrap()
            .data_dir()
            .join("agent-attention.sqlite3");
        wait(
            || {
                rusqlite::Connection::open(&attention).is_ok_and(|db| {
                    db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seen')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false)
                })
            },
            "notification journal did not initialize",
        );
        guard.repair_on_failure(
            attention.clone(),
            "DROP TRIGGER IF EXISTS reject_ordinary_attention;",
        );
        let fault = rusqlite::Connection::open(attention).unwrap();
        fault.execute_batch("CREATE TRIGGER reject_ordinary_attention BEFORE INSERT ON seen BEGIN SELECT RAISE(ABORT, 'fixture ordinary semantic commit failure'); END;").unwrap();
        let refusal = cleanup(&host, &capture).unwrap_err();
        assert!(
            refusal.contains("fixture ordinary semantic commit failure"),
            "{refusal}"
        );
        let held = Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
        assert!(held.state.outbox_latest > held.state.outbox_ack);
        assert!(
            host.lock().unwrap().chats[&source.id]
                .handle
                .is_close_only()
        );
        assert!(capture.matches_startup(&host.lock().unwrap().native_actor_startups[&source.id]));
        assert_eq!(context.outcome(), LaunchOutcome::Uncertain);
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_cursor_for_test(identity.instance)
                .unwrap()
                .is_some()
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_none()
        );
        fault
            .execute_batch("DROP TRIGGER reject_ordinary_attention;")
            .unwrap();
        cleanup(&host, &capture).unwrap();
        let token = context.ownership_snapshot().token.unwrap();
        wait(
            || context.complete_semantic_cleanup(token, &identity),
            "semantic settlement did not receive the original helper's actual wait proof",
        );
        assert_eq!(
            context.ownership_snapshot().original_helper,
            chat::protocol::HelperWaitProof::TeardownProved
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_some()
        );
        assert!(!host.lock().unwrap().chats.contains_key(&source.id));
        assert!(capture.matches_startup(&host.lock().unwrap().native_actor_startups[&source.id]));
        assert!(
            !root.join("prompts").exists(),
            "cleanup must never send provider input"
        );
    }));
    finish_fixture(&mut guard, checked, &root);
}

#[cfg(target_os = "linux")]
#[test]
fn execution_startup_without_native_row_retries_from_verified_retirement() {
    receipt_only_startup(false);
}

#[cfg(target_os = "linux")]
#[test]
fn absent_failed_target_survives_actual_pending_wait_and_closes_only_from_retirement() {
    receipt_only_startup(true);
}

#[cfg(target_os = "linux")]
fn receipt_only_startup(absent_failed_target: bool) {
    let marker = if absent_failed_target {
        "CAGENTS_ABSENT_FAILED_STARTUP_TEST"
    } else {
        "CAGENTS_EXECUTION_RETIRED_STARTUP_TEST"
    };
    let Some(root) = isolated_child(marker) else {
        return;
    };
    crate::host::chat::retained_actor_tests::install_provider(&root.join("bin/claude"));
    let proof = HostSessionProof {
        host_session_id: "execution-startup-retirement-retry".into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: "same-daemon".into(),
    };
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    let registry_path = root.join("registry.sqlite3");
    let mut state = HostState::new(proof.clone());
    state.install_model(crate::host::model::Registry::at(&registry_path).unwrap());
    let host = Arc::new(HostLock::new(state));
    let mut source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    source.proof = Some(proof.clone());
    let registry = crate::host::agent_execution::registry(&host).unwrap();
    registry
        .lock()
        .unwrap()
        .fresh_agent_authority(&crate::agent_execution::HostedSession::Chat(source.clone()))
        .unwrap();
    let context =
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    let mut guard = UninstalledFixture::new(source.clone(), context.clone());
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (mut spec, adapter) = chat::protocol::create(&source.launch).unwrap();
        spec.execution = Some(context.clone());
        let prepared = adapter.prepared_launch_identity().unwrap();
        let environment = spec.environment.clone();
        let original =
            Handle::start_remote_owned(spec, source.clone(), prepared, environment, |_| {})
                .unwrap();
        let identity = original.actor_identity().unwrap();
        let inspection = Handle::inspect_remote(&identity.record, proof.clone()).unwrap();
        assert!(!inspection.state.installed);
        assert_eq!(inspection.state.outbox_latest, inspection.state.outbox_ack);
        enroll_pending(
            &host,
            &inspection.identity,
            &inspection.source,
            &inspection.previous_sources,
            inspection
                .launch_account_scope
                .as_ref()
                .map_err(String::as_str),
            false,
        )
        .unwrap();
        let agent = crate::model::AgentId::new();
        let intent = crate::agent_execution::HostedSession::Chat(source.clone());
        host.lock()
            .unwrap()
            .execution_startups
            .insert(agent, (intent.clone(), context.clone()));
        let ordinary = if absent_failed_target {
            assert_eq!(
                context.ownership_snapshot().original_helper,
                chat::protocol::HelperWaitProof::Pending,
                "the original live helper must still await its actual sole Child.wait"
            );
            // Model the exact no-HOST gap left by normal semantic close while
            // retaining its real failed attachment and original context. This
            // fixture does not force or fabricate the watcher's scheduling.
            let failed_record = Record {
                execution_instance: identity.instance,
                console_recipe: None,
                paired_terminal: None,
                session: source.clone(),
                handle: original.clone(),
                source_process: None,
                subscribers: BTreeMap::new(),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: true,
            };
            let pending = Startup {
                source: source.clone(),
                requested: source.clone(),
                context: context.clone(),
                failed_close: Some(super::super::super::CloseTarget::from_record(
                    &failed_record,
                )),
            };
            {
                let mut state = host.lock().unwrap();
                state
                    .native_actor_startups
                    .insert(source.id, pending.clone());
                state.begin_ordinary_exit_cleanup();
            }
            context.request_ordinary_cleanup();
            let capture = capture_startup(&host.lock().unwrap(), &pending).unwrap();
            let execution =
                capture_execution(&host.lock().unwrap(), agent, &intent, &context).unwrap();
            for captured in [&capture, &execution] {
                let refusal = cleanup(&host, captured).unwrap_err();
                assert!(
                    refusal.contains("requires exact verified retirement"),
                    "{refusal}"
                );
            }
            assert!(host.lock().unwrap().chats.is_empty());
            assert!(host.lock().unwrap().chat_spawning.is_empty());
            original.refresh_retained_state().unwrap();
            assert!(
                capture.matches_startup(&host.lock().unwrap().native_actor_startups[&source.id])
            );
            Some(capture)
        } else {
            None
        };
        // There are no semantic events in this fixture. The genuine helper close
        // and retirement create its authenticated capsule before any HOST chat is
        // published; no process disappearance is accepted as a close receipt.
        original.close().unwrap();
        original.finish_retained_close().unwrap();
        assert!(matches!(
            Handle::inspect_retained_retirement(&identity.record, &proof).unwrap(),
            RetirementInspection::Verified(_)
        ));
        assert!(
            !crate::host::chat::has_native_owner(&source).unwrap(),
            "the live discovery record must be naturally absent"
        );
        guard.repair_on_failure(
            registry_path.clone(),
            "DROP TRIGGER IF EXISTS reject_execution_retirement;",
        );
        let fault = rusqlite::Connection::open(registry_path).unwrap();
        fault.execute_batch("CREATE TRIGGER reject_execution_retirement BEFORE INSERT ON native_actor_retirements BEGIN SELECT RAISE(ABORT, 'fixture execution retirement commit failure'); END;").unwrap();
        let refusal = match &ordinary {
            Some(capture) => cleanup(&host, capture),
            None => cleanup_execution_startup(&host, agent, &source, &context),
        }
        .unwrap_err();
        assert!(
            refusal.contains("fixture execution retirement commit failure"),
            "{refusal}"
        );
        assert_eq!(context.outcome(), LaunchOutcome::Uncertain);
        if let Some(capture) = &ordinary {
            assert!(
                capture.matches_startup(&host.lock().unwrap().native_actor_startups[&source.id])
            );
        } else {
            assert!(host.lock().unwrap().native_actor_startups.is_empty());
        }
        assert!(host.lock().unwrap().chats.is_empty());
        assert_eq!(host.lock().unwrap().execution_startups[&agent].0, intent);
        assert!(Arc::ptr_eq(
            &host.lock().unwrap().execution_startups[&agent].1,
            &context
        ));
        fault
            .execute_batch("DROP TRIGGER reject_execution_retirement;")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        if let Some(capture) = &ordinary {
            cleanup(&host, capture).unwrap();
            let token = context.ownership_snapshot().token.unwrap();
            while !context.complete_semantic_cleanup(token, &identity) {
                assert!(
                    Instant::now() < deadline,
                    "original helper actual wait never completed"
                );
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                capture.matches_startup(&host.lock().unwrap().native_actor_startups[&source.id])
            );
        } else {
            loop {
                match cleanup_execution_startup(&host, agent, &source, &context) {
                    Ok(()) => break,
                    Err(error) => {
                        assert!(
                            error.contains("original helper wait remains pending"),
                            "{error}"
                        );
                        assert!(
                            Instant::now() < deadline,
                            "original helper actual wait never completed"
                        );
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
        assert_eq!(context.outcome(), LaunchOutcome::TeardownProved);
        assert_eq!(
            context.ownership_snapshot().original_helper,
            chat::protocol::HelperWaitProof::TeardownProved
        );
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            host.lock().unwrap().execution_startups[&agent].0,
            intent,
            "semantic cleanup leaves exact durable-intent settlement to its caller"
        );
        assert!(!root.join("prompts").exists());
    }));
    finish_fixture(&mut guard, checked, &root);
}

#[cfg(target_os = "linux")]
fn isolated_child(marker: &str) -> Option<std::path::PathBuf> {
    const PRESENCE: &str = "CAGENTS_SEMANTIC_FIXTURE_AUTHORITY_PRESENCE";
    let _helper = crate::host::chat::retained_actor_tests::retained_helper_executable();
    if std::env::var_os(marker).is_some() {
        let expected = match std::env::var(PRESENCE).as_deref() {
            Ok("present") => true,
            Ok("absent") => false,
            _ => panic!("missing original opaque authority presence"),
        };
        assert_eq!(
            crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default(),
            expected
        );
        return Some(std::path::PathBuf::from(
            std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap(),
        ));
    }
    // Keep the entire private evidence directory on every failure, including
    // setup or cleanup failure. Delete it only after the child reports success
    // and its exact original helper's actual wait has been verified.
    let root = tempfile::tempdir_in("/var/tmp").unwrap().keep();
    for path in [
        "home/.claude",
        "run",
        "data",
        "config",
        "cache",
        "bin",
        "project",
    ] {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }
    let exact = thread::current().name().unwrap().to_owned();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    // Preserve these inherited values opaquely. Never read/copy/reset the
    // authority value or replace the caller's validated helper executable.
    for (key, _) in std::env::vars_os() {
        if key != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV)
            && key != std::ffi::OsStr::new("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
        {
            command.env_remove(key);
        }
    }
    let output = command
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env(marker, "1")
        .env(
            PRESENCE,
            if crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default()
            {
                "present"
            } else {
                "absent"
            },
        )
        .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
        .env("CAGENTS_HOLDER_ROOT", &root)
        .env("HOME", root.join("home"))
        .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .output()
        .unwrap();
    std::fs::write(root.join("fixture.stdout"), &output.stdout).unwrap();
    std::fs::write(root.join("fixture.stderr"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "private semantic fixture failed; evidence retained at {}: stdout={} stderr={}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        root.join("original-helper-settled").is_file(),
        "original helper settlement missing; evidence retained at {}",
        root.display()
    );
    std::fs::remove_dir_all(root).unwrap();
    None
}

#[cfg(target_os = "linux")]
fn install_boot_permission_provider(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, r#"#!/usr/bin/python3
import json, pathlib, select, sys
root=pathlib.Path(__file__).resolve().parent.parent
initialized=False
sent=False
def send(value): print(json.dumps(value),flush=True)
while True:
    if initialized and not sent and (root/'emit-boot-permission').exists():
        send({'type':'control_request','request_id':'ordinary-boot-permission','request':{'subtype':'can_use_tool','tool_name':'Read','tool_use_id':'ordinary-boot-read','input':{'file_path':'/fixture/read-only'}}})
        sent=True
    ready,_,_=select.select([sys.stdin],[],[],0.02)
    if not ready: continue
    line=sys.stdin.readline()
    if not line: break
    frame=json.loads(line)
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
        if frame.get('request',{}).get('subtype')=='initialize': initialized=True
    elif frame.get('type')=='user':
        with (root/'prompts').open('a') as log: log.write('prompt\n')
"#).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
