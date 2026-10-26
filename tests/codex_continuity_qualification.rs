#![cfg(feature = "host")]

//! Qualifies one real Codex CLI for durable native continuity.
//!
//! `VERIFIED_CODEX_VERSIONS` in `chat/native_holder.rs` is a claim that a
//! Codex app-server keeps a running turn alive when its only client vanishes,
//! and that the replacement daemon reattaches to that same server and thread
//! without anything being sent twice. These tests are how a version earns its
//! place there. Each one hosts the real executable in an isolated daemon, with
//! a local stand-in for the Responses API so no model is ever called, brings a
//! turn to one boundary, kills the daemon with `SIGKILL`, starts another, and
//! checks what survived:
//!
//! - a command that is still running,
//! - a poll of a command that yielded while it runs, and
//! - an approval that nobody has answered.
//!
//! They are ignored by default because they need an executable this
//! repository does not ship. To qualify a version, add it to the allowlist
//! locally, then run
//!
//! ```sh
//! Run each of the three continuity scenarios explicitly; the separate GUI
//! model-only ignored parent requires an owned stdin pipe, not Cargo stdin.
//!
//! CAGENTS_QUALIFY_CODEX=/path/to/native/codex cargo test --locked \
//!     --no-default-features --features host \
//!     --test codex_continuity_qualification a_running_command_survives_a_killed_daemon \
//!     -- --exact --ignored --test-threads=1
//! CAGENTS_QUALIFY_CODEX=/path/to/native/codex cargo test --locked \
//!     --no-default-features --features host \
//!     --test codex_continuity_qualification a_yielded_command_poll_survives_a_killed_daemon \
//!     -- --exact --ignored --test-threads=1
//! CAGENTS_QUALIFY_CODEX=/path/to/native/codex cargo test --locked \
//!     --no-default-features --features host \
//!     --test codex_continuity_qualification a_pending_approval_survives_a_killed_daemon_unanswered \
//!     -- --exact --ignored --test-threads=1
//! ```
//!
//! and keep the allowlist entry only if all three pass. The path must name the
//! native executable, not a Node launcher: the fixture's `PATH` holds nothing
//! but that one program. Codex itself may still reach the network for its own
//! housekeeping; the model endpoint never leaves this process.

#[path = "support/hosted_agent_cleanup.rs"]
mod hosted_agent_cleanup;

#[path = "codex_continuity_qualification/goal.rs"]
mod goal;
#[path = "support/codex_goal_model.rs"]
mod goal_model;

use cagents::{
    chat::{Command as ChatCommand, NativeContinuity, Session, Snapshot},
    model::{HostSessionProof, TerminalId},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, read_frame, write_frame,
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const CODEX: &str = "CAGENTS_QUALIFY_CODEX";
/// Long enough that a replacement daemon attaches while the command still
/// runs, even on a loaded machine.
const COMMAND_SECONDS: u64 = 15;
const LONG_MARKER: &str = "LONG_COMMAND_FINISHED";
const YIELD_MARKER: &str = "YIELDED_COMMAND_FINISHED";
const APPROVAL_MARKER: &str = "approved-command-ran";

/// One request the provider made to the model endpoint.
#[derive(Clone, Debug)]
struct ModelRequest {
    /// Every user prompt in the request's input, in order.
    prompts: Vec<String>,
    /// The tool output the request ends with, when it reports one.
    output: Option<String>,
    /// How the provider names the conversation to the endpoint.
    conversation: Option<String>,
}

/// A scripted Responses API. It answers a prompt with the tool call its
/// scenario names, and a tool output with either the next call or a final
/// message, so the provider's own turn loop does the rest.
#[derive(Clone, Copy)]
enum ModelMode {
    Continuity,
    Goal,
}

struct Model {
    port: u16,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    over_limit: Arc<std::sync::atomic::AtomicBool>,
    goal_response_gate: Arc<Mutex<Option<Arc<GoalResponseGate>>>>,
}

/// SDK2 alone paces completed Responses, not native Goal commands or turns.
/// Other SDK/Continuity cases and the external GUI endpoint stay unpaced.
struct GoalResponseGate {
    state: Mutex<GoalResponseState>,
    changed: std::sync::Condvar,
}

struct GoalResponseState {
    permitted_through: usize,
    held: usize,
    cancelled: bool,
    timed_out: bool,
}

impl GoalResponseGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(GoalResponseState {
                permitted_through: 1, // The ordinary initial turn only.
                held: 0,
                cancelled: false,
                timed_out: false,
            }),
            changed: std::sync::Condvar::new(),
        }
    }

    fn permit_through(&self, number: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Keep the existing eight-request refusal meaningful. No refill,
        // automatic credit or late permission after cancellation is allowed.
        if number > 8 || number < state.permitted_through || state.cancelled {
            return false;
        }
        state.permitted_through = number;
        self.changed.notify_all();
        true
    }

    fn release_after_confirmed_pause(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(next) = state
            .permitted_through
            .checked_add(1)
            .filter(|next| *next <= 8)
        else {
            return false;
        };
        if state.cancelled {
            return false;
        }
        // Pause prevents the next continuation, but does not interrupt a
        // response already in progress. Let exactly that one settle.
        state.permitted_through = next;
        self.changed.notify_all();
        true
    }

    fn wait(&self, number: usize, deadline: Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.held += 1;
        self.changed.notify_all();
        while !state.cancelled && number > state.permitted_through {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.timed_out = true;
                state.cancelled = true;
                self.changed.notify_all();
                break;
            }
            (state, _) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.held -= 1;
        self.changed.notify_all();
        !state.cancelled
    }

    fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        self.changed.notify_all();
    }
}

/// Declare after Fixture: its drop wakes held HTTP handlers BEFORE Fixture's
/// public agent/daemon cleanup, including on an assertion panic. The model's
/// slot remains permanently cancelled, so cleanup cannot bypass the gate.
struct GoalResponsePacing(Arc<GoalResponseGate>);

impl GoalResponsePacing {
    fn permit_through(&self, number: usize) {
        assert!(
            self.0.permit_through(number),
            "invalid Goal response allowance"
        );
    }

    fn after_confirmed_pause(&self) {
        assert!(
            self.0.release_after_confirmed_pause(),
            "invalid paused response allowance"
        );
    }

    fn assert_healthy(&self) {
        let state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !state.timed_out && !state.cancelled,
            "Goal response gate failed closed"
        );
    }
}

impl Drop for GoalResponsePacing {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[test]
fn goal_response_permits_are_bounded_and_owner_drop_wakes_held_responses() {
    fn held(gate: &GoalResponseGate) {
        let state = gate.state.lock().unwrap();
        let (state, wait) = gate
            .changed
            .wait_timeout_while(state, Duration::from_secs(20), |state| state.held == 0)
            .unwrap();
        assert!(!wait.timed_out() && state.held == 1);
    }
    let gate = Arc::new(GoalResponseGate::new());
    let owner = GoalResponsePacing(gate.clone());
    assert!(gate.wait(1, Instant::now()));
    assert!(!gate.permit_through(9));
    assert_eq!(gate.state.lock().unwrap().permitted_through, 1);
    let (sent, received) = std::sync::mpsc::channel();
    let waiting = gate.clone();
    let worker = thread::spawn(move || {
        sent.send(waiting.wait(2, Instant::now() + Duration::from_secs(20)))
            .unwrap();
    });
    held(&gate);
    assert!(matches!(
        received.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert!(gate.permit_through(2));
    assert!(received.recv_timeout(Duration::from_secs(20)).unwrap());
    worker.join().unwrap();
    assert!(
        !gate.permit_through(1),
        "old credits cannot reset the allowance"
    );
    assert!(gate.release_after_confirmed_pause());
    assert_eq!(gate.state.lock().unwrap().permitted_through, 3);
    let (sent, received) = std::sync::mpsc::channel();
    let waiting = gate.clone();
    let worker = thread::spawn(move || {
        sent.send(waiting.wait(4, Instant::now() + Duration::from_secs(20)))
            .unwrap();
    });
    held(&gate);
    drop(owner); // The same guard used before Fixture's public cleanup.
    assert!(!received.recv_timeout(Duration::from_secs(20)).unwrap());
    worker.join().unwrap();
    assert!(!gate.permit_through(4));
    assert!(!gate.release_after_confirmed_pause());
    assert!(
        !gate.wait(1, Instant::now()),
        "cancellation cannot reply even to an old allowance"
    );
    let expired = GoalResponseGate::new();
    assert!(!expired.wait(2, Instant::now()));
    let state = expired.state.lock().unwrap();
    assert!(state.timed_out && state.cancelled && state.held == 0);
    drop(state);
    assert!(
        !expired.permit_through(2),
        "expiry never auto-releases a successful response"
    );
    let bounded = GoalResponseGate::new();
    assert!(bounded.permit_through(8));
    assert!(!bounded.release_after_confirmed_pause());
    assert_eq!(bounded.state.lock().unwrap().permitted_through, 8);
}

impl Model {
    fn start(mode: ModelMode) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let over_limit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rejected = over_limit.clone();
        let goal_response_gate = Arc::new(Mutex::new(None::<Arc<GoalResponseGate>>));
        let response_gate = goal_response_gate.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let seen = seen.clone();
                let rejected = rejected.clone();
                let gate = response_gate.lock().unwrap().clone();
                thread::spawn(move || {
                    serve_with_goal_gate(stream, &seen, mode, &rejected, gate.as_deref())
                });
            }
        });
        Self {
            port,
            requests,
            over_limit,
            goal_response_gate,
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn pace_goal_responses(&self) -> GoalResponsePacing {
        assert!(
            self.requests().is_empty(),
            "install pacing before starting the SDK"
        );
        let gate = Arc::new(GoalResponseGate::new());
        let mut slot = self.goal_response_gate.lock().unwrap();
        assert!(slot.is_none(), "Goal response pacing was already installed");
        *slot = Some(gate.clone());
        GoalResponsePacing(gate)
    }
}

/// Keep the external Goal fixture's header/chunk lines bounded without
/// changing the existing Continuity parser or Responses implementation.
fn model_line(
    reader: &mut impl BufRead,
    line: &mut String,
    mode: ModelMode,
) -> std::io::Result<usize> {
    if matches!(mode, ModelMode::Goal) {
        let count = reader.take(16 * 1024 + 1).read_line(line)?;
        if count > 16 * 1024 {
            return Err(std::io::Error::other(
                "Goal model line exceeded fixture bound",
            ));
        }
        Ok(count)
    } else {
        reader.read_line(line)
    }
}

fn serve(
    stream: TcpStream,
    seen: &Mutex<Vec<ModelRequest>>,
    mode: ModelMode,
    over_limit: &std::sync::atomic::AtomicBool,
) {
    serve_with_goal_gate(stream, seen, mode, over_limit, None);
}

fn serve_with_goal_gate(
    stream: TcpStream,
    seen: &Mutex<Vec<ModelRequest>>,
    mode: ModelMode,
    over_limit: &std::sync::atomic::AtomicBool,
    goal_response_gate: Option<&GoalResponseGate>,
) {
    if matches!(mode, ModelMode::Goal) {
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(20)))
            .unwrap();
    }
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    let request_line_read = model_line(&mut reader, &mut request_line, mode);
    let request_line_bytes = if matches!(mode, ModelMode::Goal) {
        request_line_read.expect("Goal model request line could not be read")
    } else {
        request_line_read.unwrap_or(0)
    };
    if request_line_bytes == 0 {
        return;
    }
    let mut length = 0;
    let mut chunked = false;
    let mut conversation = None;
    let mut header_bytes = request_line.len();
    loop {
        let mut header = String::new();
        model_line(&mut reader, &mut header, mode).unwrap();
        if matches!(mode, ModelMode::Goal) {
            header_bytes += header.len();
            assert!(
                header_bytes <= 32 * 1024,
                "Goal model headers exceeded fixture bound"
            );
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap();
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap(),
            "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
            "content-encoding" if matches!(mode, ModelMode::Goal) => {
                panic!("Goal model does not accept encoded request bodies")
            }
            "content-encoding" => panic!("the fixture cannot read a {value} request body"),
            "session_id" | "conversation_id" => conversation = Some(value.to_owned()),
            _ => {}
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            model_line(&mut reader, &mut size, mode).unwrap();
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            if matches!(mode, ModelMode::Goal) {
                assert!(
                    body.len().saturating_add(size) <= 2 * 1024 * 1024,
                    "Goal model request exceeded fixture bound"
                );
            }
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).unwrap();
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
        if matches!(mode, ModelMode::Goal) {
            assert!(
                length <= 2 * 1024 * 1024,
                "Goal model request exceeded fixture bound"
            );
        }
        body.resize(length, 0);
        reader.read_exact(&mut body).unwrap();
    }
    let mut stream = reader.into_inner();
    if !request_line.starts_with("POST ") || !request_line.contains("/responses") {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let body: Value = serde_json::from_slice(&body).unwrap();
    let input = body["input"].as_array().cloned().unwrap_or_default();
    let prompts = input
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .filter_map(|item| {
            let text = match &item["content"] {
                Value::String(text) => text.clone(),
                content => content
                    .as_array()?
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            text.contains("SCENARIO").then_some(text)
        })
        .collect::<Vec<_>>();
    let last = input.last().cloned().unwrap_or(Value::Null);
    let output = last["type"]
        .as_str()
        .is_some_and(|kind| kind.ends_with("_output"))
        .then(|| match &last["output"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
    let number = {
        let mut seen = seen.lock().unwrap();
        // Goal controls may trigger native continuation. Answer no tools, and
        // refuse beyond this finite fixture bound rather than feeding a loop.
        if matches!(mode, ModelMode::Goal) && seen.len() >= 8 {
            over_limit.store(true, std::sync::atomic::Ordering::Release);
            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        seen.push(ModelRequest {
            prompts: prompts.clone(),
            output: output.clone(),
            conversation: conversation
                .or_else(|| body["prompt_cache_key"].as_str().map(str::to_owned)),
        });
        seen.len()
    };
    // The request counter's mutex has been released. This wait holds neither
    // a host/Window lock nor a socket operation; native Get/Pause/Clear remain
    // serviceable while a no-tool model response is held. Expiry closes this
    // connection without a successful response and cannot qualify the case.
    if let Some(gate) = goal_response_gate
        && !gate.wait(number, Instant::now() + Duration::from_secs(20))
    {
        return;
    }
    let id = format!("response-{number}");
    let shell = |call: &str, command: String, yield_ms: u64| {
        tool_call(
            call,
            "exec_command",
            json!({"cmd":command,"yield_time_ms":yield_ms}),
        )
    };
    let scenario = prompts.last().map(String::as_str).unwrap_or_default();
    let item = match &output {
        _ if matches!(mode, ModelMode::Goal) => json!({
            "type":"message", "role":"assistant", "id":format!("message-{number}"),
            "content":[{"type":"output_text", "text":"GOAL_FIXTURE_OBSERVED_NO_TOOLS"}]
        }),
        Some(output) if last["call_id"] == "yield-start" => {
            // The command yielded; poll it the way a model does, with a wait
            // that outlasts it.
            let session = output
                .split("session ID ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|id| id.parse::<u64>().ok())
                .expect("a yielded command reports its session");
            tool_call(
                "yield-poll",
                "write_stdin",
                json!({"session_id":session,"chars":"","yield_time_ms":(COMMAND_SECONDS + 15) * 1000}),
            )
        }
        Some(_) => json!({
            "type":"message","role":"assistant","id":format!("message-{number}"),
            "content":[{"type":"output_text","text":"SCENARIO_COMPLETE"}]
        }),
        None if scenario.contains("SCENARIO long") => shell(
            "long",
            format!("sleep {COMMAND_SECONDS}; echo {LONG_MARKER}"),
            (COMMAND_SECONDS + 15) * 1000,
        ),
        None if scenario.contains("SCENARIO yield") => shell(
            "yield-start",
            format!("sleep {COMMAND_SECONDS}; echo {YIELD_MARKER}"),
            1000,
        ),
        None if scenario.contains("SCENARIO approval") => {
            shell("approval", format!("touch {APPROVAL_MARKER}"), 10_000)
        }
        None => panic!("the provider sent a prompt this fixture never issued: {prompts:?}"),
    };
    let events = [
        json!({"type":"response.created","response":{"id":id}}),
        json!({"type":"response.output_item.done","item":item}),
        json!({"type":"response.completed","response":{"id":id,"usage":{
            "input_tokens":1,"input_tokens_details":null,"output_tokens":1,
            "output_tokens_details":null,"total_tokens":2}}}),
    ];
    let mut reply = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    for event in events {
        reply.push_str(&format!(
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        ));
    }
    let _ = stream.write_all(reply.as_bytes());
}

fn tool_call(call: &str, name: &str, arguments: Value) -> Value {
    json!({
        "type":"function_call","id":format!("item-{call}"),"call_id":call,"name":name,
        "arguments":arguments.to_string()
    })
}

/// A process as the kernel identifies it: a recycled PID has another start.
type Identity = (u32, u64);

/// The interactive client's view of the daemon: what a window does to restore
/// a conversation and answer its requests.
struct Window {
    stream: UnixStream,
    proof: HostSessionProof,
    next: u64,
}

impl Window {
    fn connect(socket: &Path) -> Self {
        // `daemon status` can report a replacement daemon running a moment
        // before its socket exists.
        let mut stream = until(Duration::from_secs(20), "the daemon's socket", || {
            UnixStream::connect(socket).ok()
        });
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let connected = exchange(
            &mut stream,
            0,
            None,
            ClientRequest::Connect {
                expected_host: None,
                resume: Vec::new(),
            },
        );
        let ServerResponse::Connected { proof, .. } = connected else {
            panic!("expected the connect response, got {connected:?}");
        };
        Self {
            stream,
            proof,
            next: 1,
        }
    }

    fn request(&mut self, request: ClientRequest) -> ServerResponse {
        let request_id = self.next;
        self.next += 1;
        exchange(
            &mut self.stream,
            request_id,
            Some(self.proof.clone()),
            request,
        )
    }

    fn chats(&mut self) -> Vec<Session> {
        match self.request(ClientRequest::ChatList) {
            ServerResponse::ChatListed { sessions } => sessions,
            response => panic!("expected a chat list, got {response:?}"),
        }
    }

    /// Attaches the way a window showing the conversation does: a hidden
    /// chat has no presentation to read.
    fn attach(&mut self, session: Session) -> Snapshot {
        let chat_id = session.id;
        let snapshot = match self.request(ClientRequest::ChatAttach { session }) {
            ServerResponse::ChatAttached { snapshot, .. } => snapshot,
            response => panic!("expected an attached chat, got {response:?}"),
        };
        self.request(ClientRequest::ChatVisibility {
            chat_id,
            visible: true,
        });
        snapshot
    }

    /// Closes the conversation, which is what ends a durable holder and the
    /// server it owns; stopping the daemon deliberately does not.
    fn close(&mut self, chat_id: TerminalId) {
        match self.request(ClientRequest::ChatClose { chat_id }) {
            ServerResponse::ChatClosed { .. } => {}
            response => panic!("expected a closed chat, got {response:?}"),
        }
    }

    fn read(&mut self, chat_id: TerminalId) -> Snapshot {
        match self.request(ClientRequest::ChatRead {
            chat_id,
            after_generation: None,
        }) {
            ServerResponse::ChatSnapshot {
                snapshot: Some(snapshot),
                ..
            } => snapshot,
            response => panic!("expected a chat snapshot, got {response:?}"),
        }
    }
}

/// Sends one request and returns its response, past whatever the daemon
/// interleaves: an attached chat's notifications arrive between responses.
fn exchange(
    stream: &mut UnixStream,
    request_id: u64,
    proof: Option<HostSessionProof>,
    request: ClientRequest,
) -> ServerResponse {
    write_frame(
        stream,
        &ClientEnvelope {
            version: PROTOCOL_VERSION,
            request_id,
            proof,
            request,
        },
    )
    .unwrap();
    loop {
        let envelope: ServerEnvelope = read_frame(stream)
            .unwrap()
            .expect("the daemon closed the connection");
        if envelope.request_id != Some(request_id) {
            continue;
        }
        match envelope.message {
            ServerMessage::Response { response } => return response,
            ServerMessage::Error(error) => panic!("the daemon refused a request: {error:?}"),
            _ => {}
        }
    }
}

// Every filesystem path and every process belongs to this fixture: the daemon
// has its own runtime, data and account directories, so it can be killed
// without touching an installed one.
struct Fixture {
    root: tempfile::TempDir,
    daemon: Child,
    model: Model,
    version: String,
    agents: hosted_agent_cleanup::Agents,
}

impl Fixture {
    fn command(root: &Path, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .env("TERM", "xterm-256color")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .current_dir(root)
            .stdin(Stdio::null());
        command
    }

    fn new() -> Self {
        Self::with_model_mode(ModelMode::Continuity)
    }

    fn with_model_mode(mode: ModelMode) -> Self {
        let codex = std::env::var_os(CODEX).unwrap_or_else(|| {
            panic!("set {CODEX} to the native Codex executable this run qualifies")
        });
        let codex = fs::canonicalize(&codex)
            .unwrap_or_else(|error| panic!("{CODEX}={codex:?} is not an executable: {error}"));
        let reported = Command::new(&codex).arg("--version").output().unwrap();
        let reported = String::from_utf8_lossy(&reported.stdout);
        let version = reported
            .trim()
            .strip_prefix("codex-cli ")
            .unwrap_or_else(|| {
                panic!(
                    "{} did not report a Codex version: {reported:?}",
                    codex.display()
                )
            })
            .to_owned();

        let root = tempfile::tempdir().unwrap();
        for directory in [
            "home/.codex",
            "run",
            "data",
            "config",
            "cache",
            "tmp",
            "bin",
            "project",
        ] {
            fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        fs::set_permissions(root.path().join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&codex, root.path().join("bin/codex")).unwrap();
        let model = Model::start(mode);
        // No sandbox, so a command's fate depends only on the approval policy
        // each scenario asks for; no retries, so a resent request is visible
        // as a request rather than absorbed.
        fs::write(
            root.path().join("home/.codex/config.toml"),
            format!(
                r#"model = "qualification"
model_provider = "fixture"
sandbox_mode = "danger-full-access"

[model_providers.fixture]
name = "fixture"
base_url = "http://127.0.0.1:{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0

[features]
unified_exec = true
"#,
                model.port
            ),
        )
        .unwrap();
        let daemon = Self::daemon(root.path());
        let fixture = Self {
            root,
            daemon,
            model,
            version,
            agents: Default::default(),
        };
        fixture.await_daemon();
        fixture
    }

    fn daemon(root: &Path) -> Child {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("daemon.log"))
            .unwrap();
        Self::command(root, Path::new(env!("CARGO_BIN_EXE_cagentsd")))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap()
    }

    fn await_daemon(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let output = self.output(&["daemon", "status"]);
            if output.status.success()
                && serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"]["running"]
                    == true
            {
                return;
            }
            assert!(Instant::now() < deadline, "fixture daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn output(&self, arguments: &[&str]) -> Output {
        let mut child = Self::command(self.root.path(), Path::new(env!("CARGO_BIN_EXE_cagents")))
            .arg("--json")
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(90);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!("CLI timed out: {arguments:?}: {output:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        self.agents.observe(&output);
        output
    }

    fn cli(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], true, "{value}");
        value["data"].clone()
    }

    fn window(&self) -> Window {
        Window::connect(
            &self
                .root
                .path()
                .join("run")
                .join(cagents::instance::RUNTIME_SUBDIRECTORY)
                .join(cagents::terminal_ipc::SOCKET_FILE_NAME),
        )
    }

    /// Starts a conversation on `scenario` under `approval`, Codex's own
    /// approval policy, and returns the agent that owns it.
    fn start(&self, scenario: &str, approval: &str) -> String {
        let project = self.root.path().join("project");
        let project = project.to_str().unwrap();
        self.cli(&["project", "open", project]);
        let accepted = self.cli(&[
            "agent",
            "spawn",
            "--provider",
            "codex",
            "--repository",
            project,
            "--directory",
            project,
            "--permission-mode",
            approval,
            "--prompt",
            &format!("SCENARIO {scenario}"),
        ]);
        accepted["agent"]["id"].as_str().unwrap().to_owned()
    }

    /// The one conversation this fixture hosts, once the provider confirmed
    /// its thread and the holder took the durability claim for it.
    fn durable_chat(&self, window: &mut Window) -> Session {
        let chat = until(Duration::from_secs(30), "a confirmed conversation", || {
            let chats = window.chats();
            assert!(chats.len() <= 1, "one conversation was started: {chats:?}");
            chats
                .into_iter()
                .next()
                .filter(|chat| chat.launch.session_id.is_some())
        });
        assert_eq!(
            chat.native_continuity,
            NativeContinuity::Durable {
                provider_version: self.version.clone()
            },
            "Codex {} is not on the allowlist, so its holder ends with the daemon and there is \
             nothing to qualify. Add it to VERIFIED_CODEX_VERSIONS locally and run this again.",
            self.version
        );
        chat
    }

    /// The provider's processes: everything running its app-server on a
    /// socket inside this fixture.
    fn servers(&self) -> BTreeSet<Identity> {
        let root = self.root.path().to_str().unwrap();
        processes()
            .into_iter()
            .filter(|(_, command)| command.contains("app-server") && command.contains(root))
            .map(|(identity, _)| identity)
            .collect()
    }

    /// Kills the daemon the way a crash or an out-of-memory kill does, and
    /// starts its replacement once the holder has had time to notice.
    fn replace_daemon(&mut self) {
        self.daemon.kill().unwrap();
        self.daemon.wait().unwrap();
        thread::sleep(Duration::from_secs(2));
        self.daemon = Self::daemon(self.root.path());
        self.await_daemon();
    }

    fn marker(&self) -> PathBuf {
        self.root.path().join("project").join(APPROVAL_MARKER)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let panicking = thread::panicking();
        if panicking {
            self.root.disable_cleanup(true);
            hosted_agent_cleanup::diagnostics(self.root.path());
        }
        // Recovery tests keep their deliberate daemon SIGKILL boundary above.
        // Only after final assertions, retire the exact accepted private agents
        // through the matched public CLI; never signal a process by a path match.
        let result = self
            .agents
            .close(self.root.path())
            .and_then(|()| hosted_agent_cleanup::stop(self.root.path()));
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.daemon.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let daemon_stopped = self.daemon.try_wait().ok().flatten().is_some();
        if !daemon_stopped {
            let _ = self.daemon.kill();
        }
        let _ = self.daemon.wait();
        if let Err(error) = result.and_then(|()| {
            if daemon_stopped {
                Ok(())
            } else {
                Err("Private daemon did not exit after public cleanup".into())
            }
        }) {
            self.root.disable_cleanup(true);
            hosted_agent_cleanup::diagnostics(self.root.path());
            eprintln!(
                "Private cleanup evidence retained at {}: {error}",
                self.root.path().display()
            );
            if !panicking {
                panic!("private fixture cleanup failed: {error}");
            }
        }
    }
}

/// Every process this user can read, with its start time, and its command
/// line followed by its working directory.
fn processes() -> Vec<(Identity, String)> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let command = fs::read(entry.path().join("cmdline")).ok()?;
            let directory = fs::read_link(entry.path().join("cwd")).unwrap_or_default();
            let command = format!(
                "{}@{}",
                String::from_utf8_lossy(&command).replace('\0', " "),
                directory.display()
            );
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            // The command name may itself contain spaces and parentheses, so
            // fields are counted from its closing one: start time is the 22nd.
            let start = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(19)?
                .parse()
                .ok()?;
            Some(((pid, start), command))
        })
        .collect()
}

fn until<T>(timeout: Duration, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// What every boundary has to leave true once its turn has finished: the
/// provider process that started the turn finished it, in one conversation,
/// from one prompt, with exactly the model requests the turn itself needs.
fn assert_one_uninterrupted_turn(
    fixture: &Fixture,
    scenario: &str,
    servers: &BTreeSet<Identity>,
    requests: usize,
) {
    // Anything sent twice would arrive within moments of the reattachment.
    thread::sleep(Duration::from_secs(2));
    let seen = fixture.model.requests();
    assert_eq!(seen.len(), requests, "the model was asked again: {seen:#?}");
    for request in &seen {
        assert_eq!(
            request.prompts,
            [format!("SCENARIO {scenario}")],
            "the prompt was sent again: {seen:#?}"
        );
        assert_eq!(
            request.conversation, seen[0].conversation,
            "the conversation changed identity: {seen:#?}"
        );
    }
    assert_eq!(
        &fixture.servers(),
        servers,
        "the provider server was replaced"
    );
}

/// Ends the conversation the way its owner does, and with it the server.
fn close(fixture: &Fixture, window: &mut Window, chat: &Session) {
    window.close(chat.id);
    until(
        Duration::from_secs(30),
        "the provider server to end",
        || fixture.servers().is_empty().then_some(()),
    );
}

/// Brings a turn to `boundary`, replaces the daemon under it, and returns the
/// window and conversation the replacement daemon reattached.
fn survive(
    fixture: &mut Fixture,
    scenario: &str,
    approval: &str,
    boundary: impl FnOnce(&Fixture, &str),
) -> (Window, Session, BTreeSet<Identity>) {
    let agent = fixture.start(scenario, approval);
    let before = fixture.durable_chat(&mut fixture.window());
    boundary(fixture, &agent);
    let servers = fixture.servers();
    assert!(!servers.is_empty(), "the provider server is not running");
    let asked = fixture.model.requests().len();

    fixture.replace_daemon();

    assert_eq!(
        fixture.servers(),
        servers,
        "the provider server did not survive its daemon"
    );
    let mut window = fixture.window();
    let after = fixture.durable_chat(&mut window);
    assert_eq!(after.id, before.id);
    assert_eq!(
        after.launch.session_id, before.launch.session_id,
        "the replacement daemon is on another thread"
    );
    assert_eq!(
        fixture.model.requests().len(),
        asked,
        "reattaching sent the model something"
    );
    (window, after, servers)
}

/// A running command keeps running, and the replacement daemon watches the
/// same turn finish.
fn running_command_survives(scenario: &str, marker: &str, requests: usize) {
    let mut fixture = Fixture::new();
    let (mut window, chat, servers) = survive(&mut fixture, scenario, "never", |fixture, _| {
        // Every request but the last has been answered once the command the
        // boundary names is running, or being polled.
        until(Duration::from_secs(30), "the command to start", || {
            (fixture.model.requests().len() == requests - 1).then_some(())
        });
        thread::sleep(Duration::from_secs(2));
    });
    let attached = window.attach(chat.clone());
    assert!(
        attached.busy,
        "the replacement daemon does not see the running turn: {attached:?}"
    );
    until(
        Duration::from_secs(COMMAND_SECONDS + 30),
        "the command's output to reach the model",
        || {
            fixture
                .model
                .requests()
                .iter()
                .any(|request| {
                    request
                        .output
                        .as_deref()
                        .is_some_and(|output| output.contains(marker))
                })
                .then_some(())
        },
    );
    until(Duration::from_secs(30), "the turn to finish", || {
        (!window.read(chat.id).busy).then_some(())
    });
    assert_one_uninterrupted_turn(&fixture, scenario, &servers, requests);
    close(&fixture, &mut window, &chat);
}

#[test]
#[ignore = "needs a real Codex executable; see the module documentation"]
fn a_running_command_survives_a_killed_daemon() {
    running_command_survives("long", LONG_MARKER, 2);
}

#[test]
#[ignore = "needs a real Codex executable; see the module documentation"]
fn a_yielded_command_poll_survives_a_killed_daemon() {
    running_command_survives("yield", YIELD_MARKER, 3);
}

#[test]
#[ignore = "needs a real Codex executable; see the module documentation"]
fn a_pending_approval_survives_a_killed_daemon_unanswered() {
    let mut fixture = Fixture::new();
    let (mut window, chat, servers) =
        survive(&mut fixture, "approval", "untrusted", |fixture, agent| {
            until(Duration::from_secs(30), "the approval request", || {
                let requests = fixture.cli(&["agent", "requests", agent]);
                (!requests["requests"].as_array().unwrap().is_empty()).then_some(())
            });
            assert!(!fixture.marker().exists());
        });
    assert!(
        !fixture.marker().exists(),
        "the command ran without anyone approving it"
    );
    let attached = window.attach(chat.clone());
    let [request] = attached.requests.as_slice() else {
        panic!(
            "the replacement daemon does not hold exactly the pending approval: {:?}",
            attached.requests
        );
    };
    assert!(request.description.contains(APPROVAL_MARKER), "{request:?}");
    // Reattaching must present the approval, never decide it.
    thread::sleep(Duration::from_secs(3));
    assert!(
        !fixture.marker().exists(),
        "reattaching answered the approval"
    );
    assert_eq!(fixture.model.requests().len(), 1);

    let allow = request
        .actions
        .iter()
        .find(|action| action.label == "Allow once")
        .unwrap_or_else(|| panic!("no single approval among {:?}", request.actions));
    window.request(ClientRequest::ChatCommand {
        chat_id: chat.id,
        command: ChatCommand::Answer {
            request_id: request.id.clone(),
            action: allow.id.clone(),
            answers: Vec::new(),
        },
    });
    until(
        Duration::from_secs(30),
        "the approved command to run",
        || fixture.marker().exists().then_some(()),
    );
    until(Duration::from_secs(30), "the turn to finish", || {
        let snapshot = window.read(chat.id);
        (!snapshot.busy && snapshot.requests.is_empty()).then_some(())
    });
    assert_one_uninterrupted_turn(&fixture, "approval", &servers, 2);
    close(&fixture, &mut window, &chat);
}
