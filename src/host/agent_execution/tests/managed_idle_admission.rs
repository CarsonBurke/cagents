//! Local HOST/Registry admission through real actor dequeue and `/bin/cat`
//! echo events. This is neither retained-helper/provider startup nor native
//! caller/UX qualification. The parent registers this test-only leaf.
use super::super::{caller, dispatch_accepted, observed, origin, registry, submit};
use crate::{
    account::Account,
    agent::Provider,
    agent_execution::{
        Accepted, Authority, Destination, HostedSession, Origin, Run, Spec, State, Submit, Surface,
        Target, Update,
    },
    chat::{self, runtime::courier_tests::RequestRaceFixture},
    host::{
        model::Registry,
        state::{HostLock, HostState, SharedHost},
    },
    model::{AgentId, BootId, HostIncarnation, HostSessionId, HostSessionProof, TerminalId},
    workspace_model::ModelEdit,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, RwLock, RwLockWriteGuard, Weak},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(10);
const FIRST: &str = "HOST observed first managed run";
const NEXT: &str = "HOST observed queued managed run";

struct HostFixture {
    host: SharedHost,
    authority: Authority,
    target_session: chat::Session,
    target_instance: uuid::Uuid,
    accepted: Accepted,
    trace: Arc<Mutex<Vec<(crate::model::RunId, Update)>>>,
    caller: Option<RequestRaceFixture>,
    target: Option<RequestRaceFixture>,
    replacements: Vec<RequestRaceFixture>,
    // Retain registry files until actor close and detached-worker observations.
    directory: Option<tempfile::TempDir>,
}

// A failed test must not turn unknown actor closure into TempDir removal.
// Keep exact owners reachable for the test process's custody on every error;
// this is a failed/retained fixture, never successful cleanup qualification.
struct UncertainFixtureCustody {
    _host: SharedHost,
    _caller: Option<RequestRaceFixture>,
    _target: Option<RequestRaceFixture>,
    _replacements: Vec<RequestRaceFixture>,
    _directory: Option<tempfile::TempDir>,
}

static UNCERTAIN_FIXTURE_CUSTODY: Mutex<Vec<UncertainFixtureCustody>> = Mutex::new(Vec::new());

// Own every returned actor before the remaining Registry/session setup can
// fail. No HOST dispatch targets an actor while this guard still owns it.
struct ConstructionOwner(Option<UncertainFixtureCustody>);

impl ConstructionOwner {
    fn new(host: SharedHost, directory: Option<tempfile::TempDir>) -> Self {
        Self(Some(UncertainFixtureCustody {
            _host: host,
            _caller: None,
            _target: None,
            _replacements: Vec::new(),
            _directory: directory,
        }))
    }
}

impl Drop for ConstructionOwner {
    fn drop(&mut self) {
        let Some(mut owned) = self.0.take() else {
            return;
        };
        let mut errors = Vec::new();
        if let Some(target) = owned._target.as_mut() {
            if let Err(error) = target.release_host_barriers() {
                errors.push(error);
            }
            if let Err(error) = target.handle.close() {
                errors.push(error);
            }
        }
        if let Some(caller) = owned._caller.as_mut()
            && let Err(error) = caller.handle.close()
        {
            errors.push(error);
        }
        for replacement in &owned._replacements {
            if let Err(error) = replacement.handle.close() {
                errors.push(error);
            }
        }
        if !errors.is_empty() {
            UNCERTAIN_FIXTURE_CUSTODY
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(owned);
            if thread::panicking() {
                eprintln!(
                    "managed idle construction custody retained after cleanup failure: {errors:?}"
                );
            } else {
                panic!(
                    "managed idle construction custody retained after cleanup failure: {errors:?}"
                );
            }
        }
        // Only successful closes allow the payload's original fixture Drops
        // and private directories to leave this construction owner.
    }
}

fn session(host: &SharedHost, id: TerminalId, actor: &RequestRaceFixture) -> chat::Session {
    chat::Session {
        id,
        proof: Some(host.lock().unwrap().proof().clone()),
        terminal: None,
        native_continuity: Default::default(),
        owned_codex_listener: None,
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: actor.recipe().clone(),
            session_id: None,
        },
    }
}

fn install(
    host: &SharedHost,
    source: &chat::Session,
    actor: &RequestRaceFixture,
    instance: uuid::Uuid,
) {
    host.lock().unwrap().chats.insert(
        source.id,
        crate::host::chat::Record {
            execution_instance: instance,
            session: source.clone(),
            handle: actor.handle.clone(),
            console_recipe: None,
            paired_terminal: None,
            source_process: None,
            subscribers: BTreeMap::new(),
            observations: BTreeMap::new(),
            completion_observed: false,
            returning_terminal: false,
        },
    );
}

fn run(host: &SharedHost, id: crate::model::RunId) -> Run {
    registry(host)
        .unwrap()
        .lock()
        .unwrap()
        .agent_run(id)
        .unwrap()
}

fn await_terminal(host: &SharedHost, accepted: &Accepted, expected: State) -> Run {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let current = run(host, accepted.run.id);
        if current.state.is_terminal() {
            assert_eq!(current.state, expected, "{current:?}");
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "managed run did not settle: {current:?}"
        );
        thread::yield_now();
    }
}

fn worker_gate(host: &SharedHost, agent: AgentId, accepted: &Accepted) -> Weak<Mutex<()>> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        // No upgraded Arc is held: only the actual dispatch worker can keep
        // this private agent's gate alive after its earlier worker has ended.
        let weak = host.lock().unwrap().execution_gates.get(&agent).cloned();
        if let Some(weak) = weak
            && weak.strong_count() != 0
        {
            return weak;
        }
        assert_eq!(run(host, accepted.run.id), accepted.run);
        assert!(
            Instant::now() < deadline,
            "HOST dispatch worker never owned its gate"
        );
        thread::yield_now();
    }
}

fn await_worker_release(host: &SharedHost, agent: AgentId) {
    // Production discards its dispatch JoinHandle. This observes only natural
    // gate release after the durable outcome, never OS exit or an exact join.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let alive = host
            .lock()
            .unwrap()
            .execution_gates
            .get(&agent)
            .is_some_and(|weak| weak.strong_count() != 0);
        if !alive {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "detached HOST dispatch still owns its gate"
        );
        thread::yield_now();
    }
}

fn passive_writer<'a>(
    host: &SharedHost,
    control: &'a RwLock<()>,
    weak: &Weak<Mutex<()>>,
    accepted: &Accepted,
) -> RwLockWriteGuard<'a, ()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Ok(writer) = control.try_write() {
            assert_ne!(
                weak.strong_count(),
                0,
                "a failed/exited worker cannot prove passive waiting"
            );
            assert_eq!(run(host, accepted.run.id), accepted.run);
            {
                // Upgrade only AFTER acquiring the writer, then drop this Arc
                // before any worker-release assertion.
                let gate = weak
                    .upgrade()
                    .expect("the actual worker must retain the gate");
                assert!(
                    gate.try_lock().is_ok(),
                    "passive wait held the per-agent gate"
                );
            }
            return writer;
        }
        assert_eq!(run(host, accepted.run.id), accepted.run);
        assert!(
            Instant::now() < deadline,
            "passive wait held the control reader"
        );
        thread::yield_now();
    }
}

impl HostFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let initial_registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        // Declared unit-test identity, not a kernel or retained-daemon proof.
        let mut state = HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("managed-idle-fixture"),
            boot_id: BootId::new("managed-idle-fixture-boot"),
            host_incarnation: HostIncarnation::new("managed-idle-fixture-incarnation"),
        });
        state.install_model(initial_registry);
        let host = Arc::new(HostLock::new(state));
        let mut construction = ConstructionOwner::new(host.clone(), Some(directory));
        construction.0.as_mut().unwrap()._caller = Some(RequestRaceFixture::new());
        let target_id = TerminalId::new();
        let target_instance = uuid::Uuid::new_v4();
        let trace = Arc::new(Mutex::new(Vec::new()));
        let weak_host = Arc::downgrade(&host);
        let callback_trace = trace.clone();
        construction.0.as_mut().unwrap()._target =
            Some(RequestRaceFixture::with_execution_notify(move |event| {
                if let chat::runtime::Update::Execution { run, update } = event {
                    callback_trace.lock().unwrap().push((run, update.clone()));
                    // Never hold the trace lock while observed() locks Host/Registry.
                    if let Some(host) = weak_host.upgrade() {
                        observed(&host, target_id, target_instance, run, update);
                    }
                }
            }));
        let caller_actor = construction.0.as_ref().unwrap()._caller.as_ref().unwrap();
        let target = construction.0.as_ref().unwrap()._target.as_ref().unwrap();
        let caller_session = session(&host, TerminalId::new(), caller_actor);
        let target_session = session(&host, target_id, target);
        install(&host, &caller_session, caller_actor, uuid::Uuid::new_v4());
        install(&host, &target_session, target, target_instance);
        let store = registry(&host).unwrap();
        let authority = {
            let mut store = store.lock().unwrap();
            store
                .apply(&ModelEdit::OpenProject {
                    repository: target.recipe().working_directory.clone(),
                    name: "Managed idle fixture".into(),
                    kind: Default::default(),
                })
                .unwrap();
            store
                .register_agent_authority(&HostedSession::Chat(caller_session.clone()))
                .unwrap()
        };
        let (derived_origin, issuing_session) = caller(&host, &authority).unwrap();
        assert_eq!(issuing_session, Some(caller_session.id));
        assert!(matches!(derived_origin, Origin::Child { .. }));
        assert_eq!(origin(&host, &authority).unwrap(), derived_origin);
        let accepted = {
            let mut store = store.lock().unwrap();
            let mut accepted = store
                .submit_agent(
                    &derived_origin,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: Some(Account::system(Provider::Claude).id()),
                            target: Target {
                                repository: target.recipe().working_directory.clone(),
                                directory: target.recipe().working_directory.clone(),
                                placement: None,
                            },
                            surface: Surface::Chat,
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: FIRST.into(),
                        idempotency_key: "first".into(),
                    },
                    super::super::now(),
                )
                .unwrap();
            accepted.agent = store
                .bind_agent(
                    accepted.agent.id,
                    None,
                    HostedSession::Chat(target_session.clone()),
                )
                .unwrap();
            accepted
        };
        let mut owned = construction.0.take().unwrap();
        let fixture = Self {
            host,
            authority,
            target_session,
            target_instance,
            accepted,
            trace,
            caller: owned._caller.take(),
            target: owned._target.take(),
            replacements: std::mem::take(&mut owned._replacements),
            directory: owned._directory.take(),
        };
        dispatch_accepted(&fixture.host, fixture.accepted.clone()).unwrap();
        await_terminal(&fixture.host, &fixture.accepted, State::Succeeded);
        await_worker_release(&fixture.host, fixture.accepted.agent.id);
        fixture
    }

    fn target(&self) -> &RequestRaceFixture {
        self.target.as_ref().unwrap()
    }
    fn target_mut(&mut self) -> &mut RequestRaceFixture {
        self.target.as_mut().unwrap()
    }
    fn next_request(&self) -> Submit {
        Submit {
            name: None,
            destination: Destination::Existing(self.accepted.agent.id),
            prompt: NEXT.into(),
            idempotency_key: "next".into(),
        }
    }
    fn assert_background(&self, busy: bool) {
        let snapshot = self.target().handle.snapshot(None).unwrap();
        assert!(snapshot.ready && !snapshot.exited);
        assert_eq!(snapshot.busy, busy);
        assert!(
            snapshot
                .subagents
                .iter()
                .any(|child| child.background && child.status == chat::ToolStatus::Running)
        );
    }
    fn assert_callbacks(&self, id: crate::model::RunId, count: usize) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let trace = self.trace.lock().unwrap();
            let entries = trace
                .iter()
                .filter(|(run, _)| *run == id)
                .collect::<Vec<_>>();
            if entries.len() == count {
                if count == 2 {
                    assert!(
                        matches!(&entries[0].1, Update::Started { provider_key } if provider_key == &format!("managed-fixture-{id}"))
                    );
                    assert!(matches!(
                        &entries[1].1,
                        Update::Finished {
                            state: State::Succeeded,
                            ..
                        }
                    ));
                }
                return;
            }
            assert!(
                entries.len() < count && Instant::now() < deadline,
                "unexpected callback trace: {trace:?}"
            );
            drop(trace);
            thread::yield_now();
        }
    }
}

impl Drop for HostFixture {
    fn drop(&mut self) {
        // Test-local writer guards are declared after this fixture and unwind
        // first. Keep actors/directories/Host alive through worker observation.
        let mut errors = Vec::new();
        if let Some(target) = self.target.as_mut() {
            if let Err(error) = target.release_host_barriers() {
                errors.push(error);
            }
            if let Err(error) = target.handle.close() {
                errors.push(error);
            }
        }
        if let Some(caller) = self.caller.as_mut()
            && let Err(error) = caller.handle.close()
        {
            errors.push(error);
        }
        for replacement in &self.replacements {
            if let Err(error) = replacement.handle.close() {
                errors.push(error);
            }
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let alive = self
                .host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .execution_gates
                .get(&self.accepted.agent.id)
                .is_some_and(|weak| weak.strong_count() != 0);
            if !alive {
                break;
            }
            if Instant::now() >= deadline {
                errors.push("detached HOST worker gate still live after actor close; no dispatch join handle is exposed".into());
                break;
            }
            thread::yield_now();
        }
        if !errors.is_empty() {
            UNCERTAIN_FIXTURE_CUSTODY
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(UncertainFixtureCustody {
                    _host: self.host.clone(),
                    _caller: self.caller.take(),
                    _target: self.target.take(),
                    _replacements: std::mem::take(&mut self.replacements),
                    _directory: self.directory.take(),
                });
            if thread::panicking() {
                eprintln!(
                    "managed idle fixture custody retained after cleanup failure: {errors:?}"
                );
            } else {
                panic!("managed idle fixture custody retained after cleanup failure: {errors:?}");
            }
            return;
        }
        // Existing fixture Drop owns Compact/attempt joins and idempotent close;
        // its TempDir is dropped only after all original actor closes succeeded.
        drop(self.target.take());
        drop(self.caller.take());
        self.replacements.clear();
    }
}

#[test]
fn managed_send_waits_behind_native_parent_and_retries_exact_run_after_request_race() {
    let mut fixture = HostFixture::new();
    let first = run(&fixture.host, fixture.accepted.run.id);
    fixture.assert_callbacks(first.id, 2);
    fixture.target().start_native_parent();
    fixture.assert_background(true);
    assert_eq!(fixture.target().correlated_runs(), vec![first.id]);
    let request = fixture.next_request();
    let next = submit(&fixture.host, &fixture.authority, request.clone()).unwrap();
    assert_eq!(next.run.state, State::Accepted);
    assert!(next.run.started_ms.is_none() && next.run.provider_key.is_none());
    let weak = worker_gate(&fixture.host, next.agent.id, &next);
    let control = fixture.host.lock().unwrap().control_gate.clone();
    let writer = passive_writer(&fixture.host, &control, &weak, &next);
    let retry = submit(&fixture.host, &fixture.authority, request.clone()).unwrap();
    assert!(!retry.created && retry.run.id == next.run.id);
    let mut competing = request.clone();
    competing.idempotency_key = "independent-next".into();
    assert!(
        submit(&fixture.host, &fixture.authority, competing)
            .unwrap_err()
            .contains("unfinished or unverified")
    );
    // Service submit has no outer daemon RPC reader; the writer is intentional.
    fixture.target().settle_native_parent();
    fixture.assert_background(false);
    assert!(fixture.target().handle.is_idle());
    fixture.target_mut().begin_request();
    drop(writer);
    fixture.target().wait_for_queued_prompt();
    fixture.target_mut().release_request();
    let writer = passive_writer(&fixture.host, &control, &weak, &next);
    // The real HOST command was queued behind Compact. It returned to passive
    // waiting with a pending request before command_for_run or any B write.
    // Refused/Unacknowledged would instead settle Failed/Interrupted, so these
    // gates+Accepted witnesses identify the production typed NotIdle route.
    assert_eq!(fixture.target().handle.requests().len(), 1);
    assert_eq!(fixture.target().correlated_runs(), vec![first.id]);
    assert_eq!(fixture.target().prompt_texts(), vec![FIRST]);
    assert_eq!(fixture.target().provider_write_count(), 1);
    fixture.assert_callbacks(next.run.id, 0);
    drop(writer);
    fixture.target().clear_request();
    let finished = await_terminal(&fixture.host, &next, State::Succeeded);
    fixture.assert_callbacks(next.run.id, 2);
    await_worker_release(&fixture.host, next.agent.id);
    assert_eq!(
        finished.provider_key,
        Some(format!("managed-fixture-{}", next.run.id))
    );
    assert_eq!(finished.result.text, format!("Managed result: {NEXT}"));
    assert!(finished.started_ms.is_some() && finished.finished_ms.is_some());
    assert_eq!(run(&fixture.host, first.id), first);
    assert_eq!(
        fixture.target().correlated_runs(),
        vec![first.id, next.run.id]
    );
    assert_eq!(fixture.target().prompt_texts(), vec![FIRST, NEXT]);
    assert_eq!(fixture.target().provider_write_count(), 2);
    fixture.assert_background(false);
    assert!(
        fixture.target().handle.is_idle(),
        "Running background work is not parent contention"
    );
    {
        let host = fixture.host.lock().unwrap();
        let record = &host.chats[&fixture.target_session.id];
        assert_eq!(record.session, fixture.target_session);
        assert_eq!(record.execution_instance, fixture.target_instance);
        assert!(record.handle.same_actor(&fixture.target().handle));
    }
    let retry = submit(&fixture.host, &fixture.authority, request).unwrap();
    assert!(!retry.created && retry.run == finished);
    dispatch_accepted(&fixture.host, retry).unwrap();
    assert_eq!(run(&fixture.host, next.run.id), finished);
    assert_eq!(weak.strong_count(), 0);
    fixture.assert_callbacks(next.run.id, 2);
    assert_eq!(
        fixture.target().correlated_runs(),
        vec![first.id, next.run.id]
    );
    assert_eq!(fixture.target().provider_write_count(), 2);
}

#[test]
fn changed_binding_refuses_the_waiting_managed_run_before_either_actor_receives_it() {
    let mut fixture = HostFixture::new();
    let first = run(&fixture.host, fixture.accepted.run.id);
    fixture.target().start_native_parent();
    let next = submit(&fixture.host, &fixture.authority, fixture.next_request()).unwrap();
    let weak = worker_gate(&fixture.host, next.agent.id, &next);
    let control = fixture.host.lock().unwrap().control_gate.clone();
    let mut construction = ConstructionOwner::new(fixture.host.clone(), None);
    // On setup unwind, release the writer before closing the guarded actor.
    let writer = passive_writer(&fixture.host, &control, &weak, &next);
    construction
        .0
        .as_mut()
        .unwrap()
        ._replacements
        .push(RequestRaceFixture::new());
    let replacement = construction
        .0
        .as_ref()
        .unwrap()
        ._replacements
        .last()
        .unwrap();
    let replacement_session = session(&fixture.host, TerminalId::new(), replacement);
    install(
        &fixture.host,
        &replacement_session,
        replacement,
        uuid::Uuid::new_v4(),
    );
    fixture.replacements.push(
        construction
            .0
            .as_mut()
            .unwrap()
            ._replacements
            .pop()
            .unwrap(),
    );
    registry(&fixture.host)
        .unwrap()
        .lock()
        .unwrap()
        .bind_agent(
            next.agent.id,
            Some(&HostedSession::Chat(fixture.target_session.clone())),
            HostedSession::Chat(replacement_session.clone()),
        )
        .unwrap();
    fixture.target().settle_native_parent();
    drop(writer);
    let failed = await_terminal(&fixture.host, &next, State::Failed);
    await_worker_release(&fixture.host, next.agent.id);
    assert!(failed.started_ms.is_none() && failed.provider_key.is_none());
    assert!(
        failed
            .result
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("binding changed")
    );
    fixture.assert_callbacks(next.run.id, 0);
    let replacement = fixture.replacements.last().unwrap();
    assert_eq!(fixture.target().correlated_runs(), vec![first.id]);
    assert_eq!(fixture.target().provider_write_count(), 1);
    assert!(replacement.correlated_runs().is_empty());
    assert_eq!(replacement.prompt_count(), 0);
    assert_eq!(replacement.provider_write_count(), 0);
    assert_eq!(run(&fixture.host, first.id), first);
    assert_eq!(
        registry(&fixture.host)
            .unwrap()
            .lock()
            .unwrap()
            .agent(next.agent.id)
            .unwrap()
            .session,
        Some(HostedSession::Chat(replacement_session))
    );
    assert!(!fixture.target().handle.is_completed() && !replacement.handle.is_completed());
    // Rebinding is not teardown proof. Each original fixture owner closes and
    // joins only its own actor/child; no early retire_agent_process call occurs.
}
