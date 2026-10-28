//! Real retained-helper recovery: framed HOST requests and authenticated native
//! discovery/retirement, in the existing bounded private subprocess wrapper.
//! These cases require the same-head cagentsd helper; no provider service is used.
use super::saved_account_tests::{Fixture, THREAD};
use super::*;
use crate::host::model::Registry;
use std::path::PathBuf;

const OTHER_THREAD: &str = "98765432-1234-4234-8234-123456789abc";

struct HiddenSnapshotSocket {
    socket: PathBuf,
    held: PathBuf,
}
impl Drop for HiddenSnapshotSocket {
    fn drop(&mut self) {
        let _ = std::fs::rename(&self.held, &self.socket);
    }
}
struct InitialSnapshotFailure {
    id: TerminalId,
    keep_unavailable: bool,
    owner: Option<crate::daemon::holder_registry::Record>,
    hidden: Option<HiddenSnapshotSocket>,
}
static INITIAL_SNAPSHOT_FAILURE: std::sync::Mutex<Option<InitialSnapshotFailure>> =
    std::sync::Mutex::new(None);
struct SnapshotFailureGuard;
impl Drop for SnapshotFailureGuard {
    fn drop(&mut self) {
        INITIAL_SNAPSHOT_FAILURE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

/// Test-only bracket at the actual first HOST attachment read. The exact real
/// helper remains running; only its private socket name is temporarily absent.
pub(super) fn initial_snapshot_transport(
    handle: &Handle,
    snapshot: impl FnOnce() -> Result<ServerResponse, String>,
) -> Result<ServerResponse, String> {
    let Some(identity) = handle.actor_identity() else {
        return snapshot();
    };
    let mut fault = {
        let mut current = INITIAL_SNAPSHOT_FAILURE.lock().unwrap();
        if current
            .as_ref()
            .is_none_or(|fault| fault.id != identity.record.terminal)
            || current.as_ref().is_some_and(|fault| fault.owner.is_some())
        {
            drop(current);
            return snapshot();
        }
        current.take().unwrap()
    };
    use crate::daemon::holder_registry::{self as holders, Kind};
    let directory = holders::directory(Kind::Native).unwrap();
    let socket = holders::socket(&directory, identity.record.terminal);
    let discovered = holders::recovery_candidate(&socket, Kind::Native)
        .unwrap()
        .unwrap();
    assert_eq!(discovered, identity.record);
    assert!(discovered.holder.is_current());
    let held = socket.with_extension("initial-snapshot-held");
    std::fs::rename(&socket, &held).unwrap();
    let hidden = HiddenSnapshotSocket { socket, held };
    fault.owner = Some(discovered);
    let response = snapshot();
    if fault.keep_unavailable {
        fault.hidden = Some(hidden);
    } else {
        drop(hidden);
    }
    *INITIAL_SNAPSHOT_FAILURE.lock().unwrap() = Some(fault);
    response
}

pub(super) struct RetainedFixture {
    pub(super) fixture: Fixture,
    _broker: super::super::notifications::Broker,
    _lease: crate::daemon::HostLease,
}
impl RetainedFixture {
    pub(super) fn new() -> Self {
        let fixture = Fixture::new(true);
        let proof = fixture.host.lock().unwrap().proof().clone();
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        fixture.host.lock().unwrap().install_model(
            Registry::at(&fixture.directory.path().join("registry.sqlite3")).unwrap(),
        );
        let broker = super::super::notifications::Broker::start(
            &fixture.host,
            crate::notifications::Application::Cagents,
        )
        .unwrap();
        fixture.host.lock().unwrap().notifications = Some(broker.handle());
        Self {
            fixture,
            _broker: broker,
            _lease: lease,
        }
    }
    fn retirements(&self, id: TerminalId) -> Vec<crate::host::model::NativeActorRetirement> {
        let registry = super::super::agent_execution::registry(&self.fixture.host).unwrap();
        let proof = self.fixture.host.lock().unwrap().proof().clone();
        let owners = registry
            .lock()
            .unwrap()
            .native_actor_ownerships(None, 128)
            .unwrap();
        owners
            .into_iter()
            .filter(|ownership| ownership.source().id == id)
            .map(|ownership| {
                let retirement = registry
                    .lock()
                    .unwrap()
                    .native_actor_retirement(ownership.actor())
                    .unwrap()
                    .expect("a failed candidate must have committed exact retirement");
                assert!(!retirement.is_pending());
                assert!(matches!(
                    Handle::inspect_retained_retirement(ownership.owner(), &proof).unwrap(),
                    chat::runtime::RetirementInspection::Verified(_)
                ));
                retirement
            })
            .collect()
    }
    pub(super) fn assert_recovered(&self, response: ServerResponse) {
        let ServerResponse::ChatAttached { session, snapshot } = response else {
            panic!("saved recovery must return its actual confirmed attachment")
        };
        assert_eq!(session.id, self.fixture.replacement.id);
        assert_eq!(snapshot.session_id.as_deref(), Some(THREAD));
        assert_eq!(snapshot.permission_mode.as_deref(), Some("plan"));
        assert_eq!(
            std::fs::read_to_string(self.fixture.directory.path().join("restored-mode")).unwrap(),
            "plan"
        );
        let handle = self.fixture.host.lock().unwrap().chats[&session.id]
            .handle
            .clone();
        assert!(
            handle.actor_identity().is_some(),
            "the production Remote owner is required"
        );
        assert!(
            !handle.supports_account_switch(),
            "saved recovery must not enable live switching"
        );
        assert!(
            !snapshot.items.iter().any(|item| matches!(&item.content,
                chat::ItemContent::Text { text, .. }
                    if text == chat::runtime::ACCOUNT_SWITCH_NOTICE
            )),
            "saved recovery cannot claim success in provider history before the durable UI rebind"
        );
    }
}

impl Drop for RetainedFixture {
    fn drop(&mut self) {
        let handles = self
            .fixture
            .host
            .lock()
            .unwrap()
            .chats
            .values()
            .filter(|record| record.handle.actor_identity().is_some())
            .map(|record| record.handle.clone())
            .collect::<Vec<_>>();
        for handle in handles {
            // Teardown only. Assertions use HOST's production close/retirement
            // path; private-wrapper cleanup contains any teardown failure.
            if handle.close().is_ok() {
                let _ = handle.finish_retained_close();
            }
        }
    }
}

fn wait(mut predicate: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "{reason}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn retry_after_retirement(confirmed: Option<&str>) {
    let mut retained = RetainedFixture::new();
    let provider = PathBuf::from(&retained.fixture.source.launch.recipe.program);
    let script = std::fs::read_to_string(&provider).unwrap();
    let init = "print(json.dumps({'type': 'system', 'subtype': 'init', 'session_id': thread}), flush=True)";
    assert_eq!(script.matches(init).count(), 1);
    std::fs::write(
        &provider,
        script.replace(init, &format!("if thread:\n    {init}")),
    )
    .unwrap();
    std::fs::write(&retained.fixture.confirmation, confirmed.unwrap_or("")).unwrap();
    let failed_id = retained.fixture.replacement.id;
    assert!(
        retained.fixture.recover().is_err(),
        "wrong or absent provider identity must refuse binding"
    );
    assert!(
        !retained
            .fixture
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&failed_id)
    );
    let receipts = retained.retirements(failed_id);
    assert_eq!(
        receipts.len(),
        1,
        "use the real candidate's committed retirement"
    );
    assert_eq!(receipts[0].confirmed_session(), confirmed);
    std::fs::write(&retained.fixture.confirmation, THREAD).unwrap();
    retained.fixture.replacement.id = TerminalId::new();
    retained.assert_recovered(retained.fixture.recover().unwrap());
    assert_eq!(
        std::fs::read_to_string(&retained.fixture.starts)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn saved_retained_recovery_restores_controls_without_premature_system_notice() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    let retained = RetainedFixture::new();
    retained.assert_recovered(retained.fixture.recover().unwrap());
}

#[test]
fn saved_retained_initial_snapshot_failure_retires_only_proved_candidates() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    for keep_unavailable in [false, true] {
        let mut retained = RetainedFixture::new();
        let candidate = retained.fixture.replacement.id;
        *INITIAL_SNAPSHOT_FAILURE.lock().unwrap() = Some(InitialSnapshotFailure {
            id: candidate,
            keep_unavailable,
            owner: None,
            hidden: None,
        });
        let fault = SnapshotFailureGuard;
        let failure = retained.fixture.recover().unwrap_err();
        assert!(
            failure.message.contains("attachment snapshot"),
            "{}",
            failure.message
        );
        let owner = INITIAL_SNAPSHOT_FAILURE
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .owner
            .clone()
            .expect("the installed real helper must reach the first attachment read");
        assert!(
            !retained
                .fixture
                .host
                .lock()
                .unwrap()
                .chats
                .contains_key(&candidate)
        );
        if keep_unavailable {
            assert!(failure.message.contains("could not be confirmed stopped"));
            assert!(owner.holder.is_current());
            let pending = retained
                .fixture
                .host
                .lock()
                .unwrap()
                .native_actor_startups
                .get(&candidate)
                .cloned()
                .expect("unknown cleanup must keep its exact startup owner");
            let starts = std::fs::read_to_string(&retained.fixture.starts).unwrap();
            retained.fixture.replacement.id = TerminalId::new();
            assert!(retained.fixture.recover().is_err());
            assert_eq!(
                std::fs::read_to_string(&retained.fixture.starts).unwrap(),
                starts
            );
            // Restore transport and use the real authenticated cleanup only
            // for teardown; the negative assertions above require no receipt.
            drop(fault);
            retained_actor::close_unattached(&retained.fixture.host, &pending.source, Some(owner))
                .unwrap();
            pending.context.installed();
            retained_actor::forget_startup(
                &retained.fixture.host,
                &pending.source,
                &pending.context,
            );
        } else {
            assert!(
                !retained
                    .fixture
                    .host
                    .lock()
                    .unwrap()
                    .native_actor_startups
                    .contains_key(&candidate)
            );
            assert_eq!(retained.retirements(candidate).len(), 1);
            drop(fault);
            retained.fixture.replacement.id = TerminalId::new();
            retained.assert_recovered(retained.fixture.recover().unwrap());
            assert_eq!(
                std::fs::read_to_string(&retained.fixture.starts)
                    .unwrap()
                    .lines()
                    .count(),
                2
            );
        }
    }
}

#[test]
fn saved_retained_wrong_thread_candidate_can_retry_after_verified_retirement() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    retry_after_retirement(Some(OTHER_THREAD));
}

#[test]
fn saved_retained_unconfirmed_candidate_can_retry_after_verified_retirement() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    retry_after_retirement(None);
}

/// A real installed helper intentionally absent from HOST/roster/lineage. This
/// models the unadoptable orphan which whole-directory discovery must inspect.
struct Orphan {
    handle: Handle,
    owner: crate::daemon::holder_registry::Record,
}
impl Orphan {
    fn start(retained: &RetainedFixture, issued: &str, confirmed: Option<&str>) -> Self {
        let fixture = &retained.fixture;
        std::fs::write(&fixture.confirmation, confirmed.unwrap_or("")).unwrap();
        if confirmed.is_none() {
            let provider = PathBuf::from(&fixture.source.launch.recipe.program);
            let script = std::fs::read_to_string(&provider).unwrap();
            let init = "print(json.dumps({'type': 'system', 'subtype': 'init', 'session_id': thread}), flush=True)";
            assert_eq!(script.matches(init).count(), 1);
            std::fs::write(
                &provider,
                script.replace(init, &format!("if thread:\n    {init}")),
            )
            .unwrap();
        }
        let mut source = fixture.replacement.clone();
        source.id = TerminalId::new();
        source.proof = Some(fixture.host.lock().unwrap().proof().clone());
        source.launch.session_id = Some(issued.into());
        let transcript = fixture
            .target_transcript
            .with_file_name(format!("{issued}.jsonl"));
        if transcript != fixture.target_transcript {
            std::fs::copy(&fixture.target_transcript, transcript).unwrap();
        }
        let (mut spec, adapter) = chat::protocol::create(&source.launch).unwrap();
        let identity = adapter.prepared_launch_identity().unwrap();
        spec.execution = Some(Arc::new(
            chat::protocol::ExecutionContext::capture(&source.launch.recipe.working_directory)
                .unwrap(),
        ));
        let handle =
            Handle::start_remote_owned(spec, source.clone(), identity, BTreeMap::new(), |_| {})
                .unwrap();
        handle.commit_native_installation().unwrap();
        wait(
            || handle.attachment_snapshot().ready,
            "orphan's actual provider setup did not finish",
        );
        assert_eq!(handle.confirmed_session_id().as_deref(), confirmed);
        let identity = handle.actor_identity().expect("actual retained helper");
        let registry = super::super::agent_execution::registry(&fixture.host).unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_ownership(identity.instance)
                .unwrap()
                .is_none()
        );
        assert!(!fixture.host.lock().unwrap().chats.contains_key(&source.id));
        std::fs::write(&fixture.confirmation, THREAD).unwrap();
        Self {
            handle,
            owner: identity.record,
        }
    }
}
impl Drop for Orphan {
    fn drop(&mut self) {
        if self.handle.close().is_ok() && self.handle.finish_retained_close().is_ok() {
            return;
        }
        // Failure containment only; never a positive retirement oracle.
        for process in [&self.owner.root, &self.owner.holder] {
            if process.pid != std::process::id() as i32 && process.is_current() {
                unsafe {
                    libc::kill(process.pid, libc::SIGKILL);
                }
            }
        }
    }
}

#[test]
fn saved_recovery_finds_same_conversation_helper_without_any_roster_row() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    let retained = RetainedFixture::new();
    let orphan = Orphan::start(&retained, THREAD, Some(THREAD));
    let starts = std::fs::read_to_string(&retained.fixture.starts).unwrap();
    let failure = retained.fixture.recover().unwrap_err();
    assert_eq!(failure.code, ProtocolErrorCode::AccountSwitchUnavailable);
    assert!(
        failure.message.contains("retained native owner"),
        "{}",
        failure.message
    );
    assert!(!orphan.handle.is_completed());
    assert_eq!(
        std::fs::read_to_string(&retained.fixture.starts).unwrap(),
        starts,
        "no candidate starts beside the unregistered writer"
    );
}

#[test]
fn saved_recovery_refuses_unconfirmed_unregistered_helper() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    let retained = RetainedFixture::new();
    let orphan = Orphan::start(&retained, OTHER_THREAD, None);
    let starts = std::fs::read_to_string(&retained.fixture.starts).unwrap();
    let failure = retained.fixture.recover().unwrap_err();
    assert_eq!(failure.code, ProtocolErrorCode::AccountSwitchUnavailable);
    assert!(!orphan.handle.is_completed());
    assert_eq!(
        std::fs::read_to_string(&retained.fixture.starts).unwrap(),
        starts
    );
}

#[test]
fn saved_recovery_allows_authenticated_unrelated_unregistered_helper() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    let retained = RetainedFixture::new();
    let orphan = Orphan::start(&retained, OTHER_THREAD, Some(OTHER_THREAD));
    retained.assert_recovered(retained.fixture.recover().unwrap());
    assert!(
        !orphan.handle.is_completed(),
        "unrelated owner must be preserved"
    );
}

#[test]
fn saved_retained_candidate_with_missing_cleanup_proof_refuses_another_start() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    let mut retained = RetainedFixture::new();
    std::fs::write(&retained.fixture.confirmation, OTHER_THREAD).unwrap();
    let failed = retained.fixture.replacement.id;
    assert!(retained.fixture.recover().is_err());
    let retirements = retained.retirements(failed);
    assert_eq!(retirements.len(), 1);
    let owner = retirements[0].owner();
    let directory =
        crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Native)
            .unwrap();
    let receipt = directory.join(format!(
        "{}-{}-{}.retired",
        owner.terminal, owner.holder.pid, owner.holder.start
    ));
    let held = receipt.with_extension("test-held-retired");
    // Simulate the loss of the real authenticated cleanup artifact. A durable
    // row alone must never be promoted into positive stopped-owner evidence.
    std::fs::rename(&receipt, &held).unwrap();
    std::fs::write(&retained.fixture.confirmation, THREAD).unwrap();
    retained.fixture.replacement.id = TerminalId::new();
    let starts = std::fs::read_to_string(&retained.fixture.starts).unwrap();
    let failure = retained.fixture.recover().unwrap_err();
    assert_eq!(failure.code, ProtocolErrorCode::AccountSwitchUnavailable);
    assert_eq!(
        std::fs::read_to_string(&retained.fixture.starts).unwrap(),
        starts
    );
    assert!(
        !retained
            .fixture
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&retained.fixture.replacement.id)
    );
    std::fs::rename(held, receipt).unwrap();
}

#[test]
fn saved_retained_unconfirmed_control_reply_never_replays_or_reports_success() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    let retained = RetainedFixture::new();
    let provider = PathBuf::from(&retained.fixture.source.launch.recipe.program);
    let script = std::fs::read_to_string(&provider).unwrap();
    let anchor = "        request = frame.get('request_id')";
    assert_eq!(script.matches(anchor).count(), 1);
    let gate = r#"        if frame.get('request', {}).get('subtype') == 'set_permission_mode':
            with (root / 'unconfirmed-controls').open('a') as log:
                log.write('set_permission_mode\n')
            continue
"#;
    std::fs::write(
        &provider,
        script.replace(anchor, &format!("{gate}{anchor}")),
    )
    .unwrap();
    let candidate = retained.fixture.replacement.id;
    assert!(
        retained.fixture.recover().is_err(),
        "provider controls must be confirmed before reporting saved recovery"
    );
    assert_eq!(
        std::fs::read_to_string(
            retained
                .fixture
                .directory
                .path()
                .join("unconfirmed-controls")
        )
        .unwrap()
        .lines()
        .count(),
        1,
        "a missing provider control response must never issue a replacement command"
    );
    assert!(
        !retained
            .fixture
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&candidate)
    );
    assert_eq!(retained.retirements(candidate).len(), 1);
}

#[test]
fn saved_retained_recovery_refuses_provider_rotation_during_control_restore() {
    if super::retained_resolution_tests::isolated().is_none() {
        return;
    }
    let retained = RetainedFixture::new();
    let provider = PathBuf::from(&retained.fixture.source.launch.recipe.program);
    let script = std::fs::read_to_string(&provider).unwrap();
    let response = "            print(json.dumps({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': request, 'response': {'commands': [], 'models': []}}}), flush=True)";
    assert_eq!(script.matches(response).count(), 1);
    // The original exact UUID has already passed confirmation before HOST can
    // send this restore command. Rotate before acknowledging that control,
    // in one small atomic pipe write through the real provider adapter.
    let rotated = format!(
        r#"            if frame.get('request', {{}}).get('subtype') == 'set_permission_mode':
                frames = [
                    {{'type': 'conversation_reset'}},
                    {{'type': 'system', 'subtype': 'init', 'session_id': '{OTHER_THREAD}'}},
                    {{'type': 'control_response', 'response': {{'subtype': 'success', 'request_id': request, 'response': {{}}}}}},
                ]
                os.write(1, (''.join(json.dumps(value) + '\n' for value in frames)).encode())
            else:
    {response}"#
    );
    std::fs::write(&provider, script.replace(response, &rotated)).unwrap();
    let candidate = retained.fixture.replacement.id;
    assert!(
        retained.fixture.recover().is_err(),
        "a later provider UUID cannot become the saved conversation's replacement"
    );
    assert!(
        !retained
            .fixture
            .host
            .lock()
            .unwrap()
            .chats
            .contains_key(&candidate)
    );
    assert_eq!(retained.retirements(candidate).len(), 1);
}
