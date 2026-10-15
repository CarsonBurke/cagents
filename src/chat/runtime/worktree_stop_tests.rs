//! Actual whole-session Stop admission and cleanup contracts.
use super::*;
use crate::chat::protocol::{Adapter, ProcessSpec, Step, TransportSpec};
use std::collections::BTreeMap;

struct Quiet;
impl Adapter for Quiet {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, _: Command) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn receive(&mut self, _: Value) -> Result<Step, String> {
        Ok(Step::default())
    }
}
/// Exercise the real actor queue while retaining its asynchronous reply.
/// Public commands wait synchronously on this same queue on this graph.
fn queue_provider_for_test(
    handle: &Handle,
    command: Command,
) -> crossbeam_channel::Receiver<Result<(), String>> {
    let (reply, response) = crossbeam_channel::bounded(1);
    handle
        .0
        .commands
        .try_send((ActorCommand::Provider { run: None, command, decline_only: false }, reply))
        .unwrap();
    handle.wake();
    response
}

// These fake adapters have a test-owned execution contract. Production
// receipts are minted by typed host preparation after the provider hook.
fn observed_fixture_directory(
    path: &std::path::Path,
) -> Arc<super::super::protocol::ExecutionContext> {
    let context = Arc::new(super::super::protocol::ExecutionContext::new(Some(
        Arc::new(std::fs::File::open(path).unwrap()),
    )));
    context.observe_provider_directory(path).unwrap();
    context
}
#[test]
fn whole_session_stop_preempts_saturated_commands_and_blocked_provider_stdin() {
    struct Stalled {
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
        prompts: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Adapter for Stalled {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, command: Command) -> Result<Step, String> {
            match command {
                Command::SetFastMode { .. } => {
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                    // Two individually legal frames exceed normal outbound
                    // backpressure while the real provider never reads stdin.
                    let frame = serde_json::Value::String("x".repeat(17 * 1024 * 1024));
                    Ok(Step {
                        writes: vec![frame.clone(), frame],
                        ..Default::default()
                    })
                }
                Command::Prompt { .. } => {
                    self.prompts.fetch_add(1, Ordering::AcqRel);
                    Ok(Step::default())
                }
                _ => Err("Unexpected stalled fixture command".into()),
            }
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            Ok(Step::default())
        }
    }
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let (entered, waiting) = crossbeam_channel::bounded(1);
    let (release, resume) = crossbeam_channel::bounded(1);
    let mut custody = whole_stop_fixture::Custody::arm(directory, release);
    let spec = ProcessSpec {
        execution: Some(observed_fixture_directory(custody.root())),
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/sleep".into(),
        arguments: vec!["600".into()],
        working_directory: custody.root().into(),
        environment: BTreeMap::new(),
    };
    let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut initial = Step {
        events: vec![Event::Ready(true)],
        ..Default::default()
    };
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = Handle::start(
        spec,
        Box::new(Stalled {
            entered,
            release: resume,
            prompts: prompts.clone(),
        }),
        initial,
        store,
        None,
        |_| {},
    )
    .unwrap();
    custody.own(&handle);
    custody.observe_original_root();
    until(&handle, |state| state.ready);
    let command = queue_provider_for_test(&handle, Command::SetFastMode { enabled: true });
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    for _ in 0..64 {
        let (reply, _) = crossbeam_channel::bounded(1);
        handle
            .0
            .commands
            .try_send((
                ActorCommand::Provider {
                    run: None,
                    command: Command::Prompt {
                        text: "queued work".into(),
                        attachments: Vec::new(),
                        mode: super::super::PromptMode::Send,
                    },
                },
                reply,
            ))
            .unwrap();
    }
    assert!(handle.0.commands.is_full());
    let stopping = handle.clone();
    let actor = handle.close_observation().actor_id;
    let stop = thread::spawn(move || {
        stopping.fence_session_stop_until(actor, Instant::now() + Duration::from_secs(15))
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while handle.0.session_stop.lock().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    custody.release_adapter().unwrap();
    command
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    stop.join().unwrap().unwrap();
    assert_eq!(
        prompts.load(Ordering::Acquire),
        0,
        "whole-session stop must preempt every queued authorizing command"
    );
    handle
        .close_until(Instant::now() + Duration::from_secs(5))
        .unwrap();
}

#[test]
fn whole_session_stop_checks_actor_identity_then_fences_all_later_work() {
    let directory = tempfile::tempdir_in("/var/tmp").unwrap();
    let (release, _unused) = crossbeam_channel::bounded(1);
    let mut custody = whole_stop_fixture::Custody::arm(directory, release);
    let spec = ProcessSpec {
        execution: Some(observed_fixture_directory(custody.root())),
        messages: None,
        unset_environment: Default::default(),
        transport: TransportSpec::JsonLines,
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: custody.root().into(),
        environment: BTreeMap::new(),
    };
    let mut initial = Step {
        events: vec![Event::Ready(true), Event::TurnStarted],
        ..Default::default()
    };
    let store = preflight(&spec, &mut initial).unwrap();
    let handle = Handle::start(spec, Box::new(Quiet), initial, store, None, |_| {}).unwrap();
    custody.own(&handle);
    custody.observe_original_root();
    until(&handle, |state| state.ready);
    let observed = handle.close_observation();
    assert!(
        handle
            .fence_session_stop_until(
                uuid::Uuid::new_v4(),
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
    );
    assert!(
        !handle.0.stop.load(Ordering::Acquire),
        "a replacement refusal must not stop the original actor"
    );
    handle
        .fence_session_stop_until(observed.actor_id, Instant::now() + Duration::from_secs(2))
        .unwrap();
    assert!(
        handle.0.stop.load(Ordering::Acquire),
        "the stop fence must be published before acknowledgement"
    );
    assert!(
        handle
            .try_command(Command::Prompt {
                text: "must never run".into(),
                attachments: Vec::new(),
                mode: super::super::PromptMode::Send
            })
            .is_err()
    );
    handle
        .close_until(Instant::now() + Duration::from_secs(5))
        .unwrap();
    assert!(handle.is_completed());
    handle
        .stop_session_until(observed.actor_id, Instant::now() + Duration::from_secs(2))
        .unwrap();
}

fn until(handle: &Handle, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = handle.snapshot(None).unwrap();
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "fixture timed out: {snapshot:?}");
        thread::sleep(Duration::from_millis(10));
    }
}
