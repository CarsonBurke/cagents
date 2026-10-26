//! Real SDK contract checks using only the parent fixture's private profile and
//! loopback Responses endpoint. These are not GUI, account readiness or model
//! quality tests. No SDK credentials, provider fallback or goal budget is added.
use super::*;
use cagents::chat::goal::{Goal, Operation, ReadState, Status, Support};
use std::os::fd::AsRawFd;

const OBJECTIVE: &str = "SCENARIO goal: observe the private fixture; never call a tool";

fn start(fixture: &Fixture) -> (Window, Session) {
    fixture.start("goal", "never");
    let mut window = fixture.window();
    let chat = until(
        Duration::from_secs(30),
        "the exact Goal conversation",
        || {
            let chats = window.chats();
            assert!(chats.len() <= 1);
            chats
                .into_iter()
                .next()
                .filter(|chat| chat.launch.session_id.is_some())
        },
    );
    window.attach(chat.clone());
    until(
        Duration::from_secs(30),
        "the harmless initial turn to settle",
        || {
            let snapshot = window.read(chat.id);
            (snapshot.ready && !snapshot.busy && snapshot.requests.is_empty()).then_some(())
        },
    );
    (window, chat)
}

fn publication(window: &mut Window, chat: &Session) -> cagents::chat::goal::Snapshot {
    until(
        Duration::from_secs(30),
        "a free actual Goal publication",
        || {
            let snapshot = window.read(chat.id);
            assert!(!snapshot.exited, "the original actor exited");
            let goal = snapshot.goal?;
            assert_eq!(
                Some(goal.thread_id.as_str()),
                chat.launch.session_id.as_deref()
            );
            (!goal.pending).then_some(goal)
        },
    )
}

fn control(window: &mut Window, chat: &Session, operation: Operation) {
    let before = publication(window, chat);
    let response = window.request(ClientRequest::ChatCommand {
        chat_id: chat.id,
        command: ChatCommand::Goal {
            thread_id: before.thread_id,
            revision: before.revision,
            operation,
        },
    });
    // Admission is deliberately not the test's success condition. Callers
    // subsequently wait for a real provider-derived Goal observation.
    assert!(matches!(response, ServerResponse::Accepted));
}

fn observed(
    window: &mut Window,
    chat: &Session,
    predicate: impl Fn(&Option<Goal>) -> bool,
) -> Option<Goal> {
    until(
        Duration::from_secs(30),
        "provider-confirmed Goal state",
        || {
            let state = publication(window, chat);
            assert_ne!(
                state.support,
                Support::Unsupported,
                "native Goal API unavailable"
            );
            if state.support != Support::Supported || state.uncertain {
                return None;
            }
            let ReadState::Known(goal) = state.read else {
                return None;
            };
            predicate(&goal).then_some(goal)
        },
    )
}

fn is_status(goal: &Option<Goal>, status: Status) -> bool {
    goal.as_ref()
        .is_some_and(|goal| goal.objective == OBJECTIVE && goal.status == status)
}

fn pause(window: &mut Window, chat: &Session) -> Goal {
    control(window, chat, Operation::Pause);
    observed(window, chat, |goal| is_status(goal, Status::Paused)).unwrap()
}

fn finish(fixture: &Fixture, window: &mut Window, chat: &Session) {
    pause(window, chat);
    control(window, chat, Operation::Clear);
    assert!(observed(window, chat, Option::is_none).is_none());
    // A paused goal need not have stopped a turn already in progress.
    until(
        Duration::from_secs(30),
        "the no-tool turn to settle",
        || {
            let snapshot = window.read(chat.id);
            (!snapshot.busy && snapshot.requests.is_empty()).then_some(())
        },
    );
    close(fixture, window, chat);
    assert!(
        !fixture
            .model
            .over_limit
            .load(std::sync::atomic::Ordering::Acquire)
    );
}

#[test]
#[ignore = "needs the explicitly supplied real SDK and matched Goal-enabled binaries"]
fn native_goal_controls_are_provider_confirmed_without_supplying_a_budget_or_an_extra_prompt() {
    let fixture = Fixture::with_model_mode(ModelMode::Goal);
    let (mut window, chat) = start(&fixture);
    control(&mut window, &chat, Operation::Get);
    assert!(observed(&mut window, &chat, Option::is_none).is_none());
    // A persisted Active goal is metadata, not proof the native runtime did
    // work. Establish an actually completed initial turn before the sole Set.
    let (baseline, baseline_requests) = until(
        Duration::from_secs(30),
        "the initial native no-tool response and turn completion",
        || {
            let snapshot = window.read(chat.id);
            assert_eq!(snapshot.session_id, chat.launch.session_id);
            assert!(
                !snapshot.exited,
                "the original actor exited before Goal Set"
            );
            let requests = fixture.model.requests().len();
            let final_response = snapshot.items.iter().any(|item| {
                item.role == cagents::chat::Role::Assistant
                    && matches!(&item.content, cagents::chat::ItemContent::Text {
                        text, phase: cagents::chat::Phase::Final, ..
                    } if text.contains("GOAL_FIXTURE_OBSERVED_NO_TOOLS"))
            });
            (snapshot.ready
                && !snapshot.busy
                && snapshot.error.is_none()
                && snapshot.requests.is_empty()
                && snapshot.turn_generation > 0
                && snapshot.finished_at.is_some()
                && requests > 0
                && final_response)
                .then_some((snapshot, requests))
        },
    );
    let baseline_items: BTreeSet<_> = baseline.items.iter().map(|item| item.id.clone()).collect();
    control(
        &mut window,
        &chat,
        Operation::SetObjective(OBJECTIVE.into()),
    );
    let active = observed(&mut window, &chat, |goal| is_status(goal, Status::Active)).unwrap();
    // Between the baseline and this observation the only effectful request is
    // that one Goal Set: no Prompt, turn/start or mutation retry can make a
    // metadata-only implementation pass. The model counter alone also cannot
    // pass (e.g. a prediction request is not a new original-thread response).
    // Do not require a sampled idle gap while the goal may continue again.
    let observation_started = Instant::now();
    let (continued_turn, continued_requests) = until(
        Duration::from_secs(30),
        "Goal Set alone to produce a model response in a new native turn",
        || {
            let snapshot = window.read(chat.id);
            assert_eq!(snapshot.session_id, baseline.session_id);
            assert!(
                !snapshot.exited,
                "the original actor exited during native Goal work"
            );
            assert!(
                !fixture
                    .model
                    .over_limit
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            let requests = fixture.model.requests().len();
            let new_final_response = snapshot.items.iter().any(|item| {
                !baseline_items.contains(&item.id)
                    && item.role == cagents::chat::Role::Assistant
                    && matches!(&item.content, cagents::chat::ItemContent::Text {
                        text, phase: cagents::chat::Phase::Final, ..
                    } if text.contains("GOAL_FIXTURE_OBSERVED_NO_TOOLS"))
            });
            (requests > baseline_requests
                && snapshot.turn_generation > baseline.turn_generation
                && snapshot.error.is_none()
                && snapshot.requests.is_empty()
                && new_final_response)
                .then_some((snapshot.turn_generation, requests))
        },
    );
    // Share the original 30-second work-observation budget with settlement;
    // the existing Pause retains its own unchanged RPC observation bound.
    let settlement_budget = Duration::from_secs(30).saturating_sub(observation_started.elapsed());
    // This is the existing first Pause, moved directly after positive work:
    // Pause prevents further Goal continuation but does not stop a live turn.
    let paused = pause(&mut window, &chat);
    assert_eq!(paused.objective, active.objective);
    assert_eq!(paused.token_budget, active.token_budget);
    let settled_turn = until(
        settlement_budget,
        "the autonomous native turn to settle after explicit Goal Pause",
        || {
            let snapshot = window.read(chat.id);
            assert_eq!(snapshot.session_id, baseline.session_id);
            assert!(
                !snapshot.exited,
                "the original actor exited before settlement"
            );
            assert!(
                !fixture
                    .model
                    .over_limit
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            let still_paused = snapshot.goal.as_ref().is_some_and(|goal| {
                goal.support == Support::Supported
                    && !goal.pending
                    && !goal.uncertain
                    && matches!(&goal.read, ReadState::Known(Some(goal))
                        if goal.objective == OBJECTIVE && goal.status == Status::Paused)
            });
            (snapshot.turn_generation >= continued_turn
                && snapshot.finished_at.is_some()
                && !snapshot.busy
                && snapshot.error.is_none()
                && snapshot.requests.is_empty()
                && still_paused)
                .then_some(snapshot.turn_generation)
        },
    );
    eprintln!(
        "NATIVE_GOAL_AUTONOMOUS_NO_TOOL_TURN {}",
        json!({
            "thread_id": chat.launch.session_id.as_deref(),
            "baseline_turn": baseline.turn_generation,
            "continued_turn": continued_turn,
            "settled_turn_after_pause": settled_turn,
            "baseline_model_requests": baseline_requests,
            "continued_model_requests": continued_requests
        })
    );
    // The operation has no budget field. A provider-returned default is an
    // observation, not proof this client supplied a budget.
    eprintln!("SDK-observed Goal budget: {:?}", active.token_budget);
    control(&mut window, &chat, Operation::Resume);
    let resumed = observed(&mut window, &chat, |goal| is_status(goal, Status::Active)).unwrap();
    assert_eq!(resumed.objective, active.objective);
    assert_eq!(resumed.token_budget, active.token_budget);
    // Native continuation can produce model requests. This asserts a finite
    // no-tool script, not that a real active goal is continuously idle.
    finish(&fixture, &mut window, &chat);
    assert!(
        fixture
            .model
            .requests()
            .iter()
            .all(|request| request.output.is_none())
    );
}

#[test]
#[ignore = "requires an already continuity-qualified SDK; never extends the allowlist"]
fn an_active_goal_reconnects_with_the_original_server_thread_and_physical_scope() {
    let mut fixture = Fixture::with_model_mode(ModelMode::Goal);
    let (mut window, chat) = start(&fixture);
    // This prerequisite intentionally fails on e.g. 0.160.0 rather than
    // converting a schema observation into durable continuity qualification.
    assert_eq!(
        chat.native_continuity,
        NativeContinuity::Durable {
            provider_version: fixture.version.clone(),
        }
    );
    control(
        &mut window,
        &chat,
        Operation::SetObjective(OBJECTIVE.into()),
    );
    observed(&mut window, &chat, |goal| is_status(goal, Status::Active));
    let servers = fixture.servers();
    assert!(!servers.is_empty());
    drop(window);
    fixture.replace_daemon(); // The existing, explicit crash boundary only.
    assert_eq!(fixture.servers(), servers);
    let mut window = fixture.window();
    let restored = fixture.durable_chat(&mut window);
    assert_eq!(restored.id, chat.id);
    assert_eq!(restored.launch, chat.launch);
    window.attach(restored.clone());
    control(&mut window, &restored, Operation::Get);
    observed(&mut window, &restored, |goal| {
        is_status(goal, Status::Active)
    });
    pause(&mut window, &restored);
    until(
        Duration::from_secs(30),
        "the paused no-tool turn to settle",
        || {
            let snapshot = window.read(restored.id);
            (!snapshot.busy && snapshot.requests.is_empty()).then_some(())
        },
    );
    let model_before_refusal = fixture.model.requests().len();

    // Paused, so no new autonomous turn is authorized while this original
    // directory is removed. Get and stop controls still target the same actor.
    let original = fixture.root.path().join("project");
    let moved = fixture.root.path().join("original-project");
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();
    let before = publication(&mut window, &restored);
    let result = goal_request(
        &mut window,
        ClientRequest::ChatCommand {
            chat_id: restored.id,
            command: ChatCommand::Goal {
                thread_id: before.thread_id,
                revision: before.revision,
                operation: Operation::Resume,
            },
        },
    );
    let error = result.expect_err("replacement directory authorized new Goal work");
    assert!(
        error.contains("approved agent directory changed"),
        "a different refusal cannot prove the original-directory guard: {error}"
    );
    control(&mut window, &restored, Operation::Get);
    observed(&mut window, &restored, |goal| {
        is_status(goal, Status::Paused)
    });
    assert_eq!(
        fixture.model.requests().len(),
        model_before_refusal,
        "a refused Resume or read-only Get started model work"
    );
    finish(&fixture, &mut window, &restored);
    assert!(fixture.servers().is_empty());
}

// Like the parent's exchange, but preserves an expected public refusal instead
// of turning it into a panic or silently issuing another command.
fn goal_request(window: &mut Window, request: ClientRequest) -> Result<ServerResponse, String> {
    let id = window.next;
    window.next += 1;
    write_frame(
        &mut window.stream,
        &ClientEnvelope {
            version: PROTOCOL_VERSION,
            request_id: id,
            proof: Some(window.proof.clone()),
            request,
        },
    )
    .unwrap();
    loop {
        let frame: ServerEnvelope = read_frame(&mut window.stream).unwrap().unwrap();
        if frame.request_id != Some(id) {
            continue;
        }
        match frame.message {
            ServerMessage::Response { response } => return Ok(response),
            ServerMessage::Error(error) => return Err(format!("{error:?}")),
            _ => {}
        }
    }
}

/// A bounded documented SDK-contract subphase, not a second product transport.
/// It uses the same private profile after public close proved its actor gone.
/// EOF/actual exit must settle this child; there is deliberately no kill path.
struct Sdk<'a> {
    fixture: &'a mut Fixture,
    child: Child,
    input: Option<std::process::ChildStdin>,
    output: std::process::ChildStdout,
    buffered: Vec<u8>,
    notifications: Vec<Value>,
    next: u64,
    exited: bool,
}

impl<'a> Sdk<'a> {
    fn start(fixture: &'a mut Fixture) -> Self {
        let native = fs::canonicalize(fixture.root.path().join("bin/codex")).unwrap();
        let log = fs::File::create(fixture.root.path().join("goal-sdk.log")).unwrap();
        let mut child = Fixture::command(fixture.root.path(), &native)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        Self {
            fixture,
            child,
            input: Some(input),
            output,
            buffered: Vec::new(),
            notifications: Vec::new(),
            next: 1,
            exited: false,
        }
    }

    fn send(&mut self, value: Value) {
        let input = self.input.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &value).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    }

    fn frame(&mut self, deadline: Instant) -> Value {
        loop {
            if let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                let bytes: Vec<_> = self.buffered.drain(..=end).collect();
                return serde_json::from_slice(&bytes).expect("SDK stdout must be JSON-RPC");
            }
            assert!(
                self.buffered.len() < 2 * 1024 * 1024,
                "SDK frame exceeded fixture bound"
            );
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "SDK contract request timed out");
            let mut fd = libc::pollfd {
                fd: self.output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fd, 1, left.as_millis().min(1000) as i32) };
            if ready < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            assert!(ready >= 0, "cannot poll SDK stdout");
            if ready == 0 {
                continue;
            }
            let mut bytes = [0; 8192];
            let count = self.output.read(&mut bytes).unwrap();
            assert!(count > 0, "SDK closed before its contract response");
            assert!(
                self.buffered.len().saturating_add(count) <= 2 * 1024 * 1024,
                "SDK frame exceeded fixture bound"
            );
            self.buffered.extend_from_slice(&bytes[..count]);
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next;
        self.next += 1;
        self.send(json!({"id":id,"method":method,"params":params}));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let frame = self.frame(deadline);
            if frame["id"] == id {
                return match frame.get("result") {
                    Some(result) => Ok(result.clone()),
                    None => Err(frame["error"].clone()),
                };
            }
            self.notification(frame);
        }
    }

    fn notification(&mut self, frame: Value) {
        if matches!(
            frame["method"].as_str(),
            Some("thread/goal/updated" | "thread/goal/cleared")
        ) {
            assert!(
                self.notifications.len() < 64,
                "SDK Goal notification bound exceeded"
            );
            self.notifications.push(frame);
        }
    }

    fn await_notification(&mut self, method: &str, thread_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(frame) = self
                .notifications
                .iter()
                .find(|frame| frame["method"] == method && frame["params"]["threadId"] == thread_id)
            {
                return frame.clone();
            }
            let frame = self.frame(deadline);
            self.notification(frame);
        }
    }

    fn finish(mut self) {
        self.input.take(); // Ordinary stdin EOF; no forced SDK cleanup.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "SDK did not exit normally: {status}");
                self.exited = true;
                return;
            }
            assert!(
                Instant::now() < deadline,
                "SDK EOF did not settle; retain private fixture"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Sdk<'_> {
    fn drop(&mut self) {
        self.input.take();
        if !self.exited {
            self.fixture.root.disable_cleanup(true);
            eprintln!(
                "Unsettled SDK contract child {}; private evidence retained at {}",
                self.child.id(),
                self.fixture.root.path().display()
            );
        }
    }
}

#[test]
#[ignore = "real SDK contract observation only; no thread/resume is sent"]
fn a_paused_persisted_goal_can_be_read_before_resuming_an_unloaded_thread() {
    let mut fixture = Fixture::with_model_mode(ModelMode::Goal);
    let (mut window, chat) = start(&fixture);
    control(
        &mut window,
        &chat,
        Operation::SetObjective(OBJECTIVE.into()),
    );
    observed(&mut window, &chat, |goal| is_status(goal, Status::Active));
    pause(&mut window, &chat);
    close(&fixture, &mut window, &chat);
    let before = fixture.model.requests().len();
    let thread_id = chat.launch.session_id.as_deref().unwrap();
    let mut sdk = Sdk::start(&mut fixture);
    let initialized = sdk
        .call(
            "initialize",
            json!({"clientInfo":{
                "name":"cagents-goal-qualification", "version":env!("CARGO_PKG_VERSION")
            }}),
        )
        .unwrap();
    assert_eq!(
        initialized["codexHome"].as_str(),
        sdk.fixture.root.path().join("home/.codex").to_str()
    );
    sdk.send(json!({"method":"initialized","params":{}}));
    let loaded = sdk.call("thread/loaded/list", json!({"limit":10})).unwrap();
    assert_eq!(
        loaded["data"],
        json!([]),
        "the SDK contract phase must start unloaded"
    );
    assert!(loaded.get("nextCursor").is_none_or(Value::is_null));
    let read = sdk.call("thread/goal/get", json!({"threadId":thread_id}));
    // Capture the real API error if unloaded reads are unsupported. Still try
    // the exact private clear, then require normal child exit before verdict.
    let paused = sdk.call(
        "thread/goal/set",
        json!({"threadId":thread_id,"status":"paused"}),
    );
    if paused.is_ok() {
        let updated = sdk.await_notification("thread/goal/updated", thread_id);
        let actual: Goal = serde_json::from_value(updated["params"]["goal"].clone()).unwrap();
        actual.validate(thread_id).unwrap();
        assert_eq!(actual.status, Status::Paused);
        assert_eq!(actual.objective, OBJECTIVE);
    }
    let cleared = sdk.call("thread/goal/clear", json!({"threadId":thread_id}));
    if cleared.as_ref().is_ok_and(|value| value["cleared"] == true) {
        sdk.await_notification("thread/goal/cleared", thread_id);
    }
    sdk.finish();
    assert_eq!(
        fixture.model.requests().len(),
        before,
        "unloaded Goal read/stop started model work"
    );
    let result = read.unwrap_or_else(|error| panic!("UNLOADED_GOAL_GET_UNAVAILABLE: {error}"));
    let goal: Goal = serde_json::from_value(result["goal"].clone()).unwrap();
    goal.validate(thread_id).unwrap();
    assert_eq!(goal.objective, OBJECTIVE);
    assert_eq!(goal.status, Status::Paused);
    assert!(
        paused.is_ok(),
        "SDK did not accept the exact paused state: {paused:?}"
    );
    assert_eq!(cleared.unwrap()["cleared"], true);
    assert!(
        !fixture
            .model
            .over_limit
            .load(std::sync::atomic::Ordering::Acquire)
    );
}

/// External model-only fixture for the existing Root native GUI runner.
/// Invoke the compiled test binary directly, with an owned stdin pipe and
/// --exact --ignored --nocapture --test-threads=1. Cargo may replace stdin.
#[test]
#[ignore = "finite external Goal model; Root must own stdin and normal GUI cleanup"]
fn native_gui_model_endpoint_until_owned_stdin_eof() {
    super::goal_model::run().expect("the external Goal model did not retire normally");
}
