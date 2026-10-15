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
//! CAGENTS_QUALIFY_CODEX=/path/to/native/codex cargo test --locked \
//!     --no-default-features --features host \
//!     --test codex_continuity_qualification -- --ignored --test-threads=1
//! ```
//!
//! and keep the allowlist entry only if all three pass. The path must name the
//! native executable, not a Node launcher: the fixture's `PATH` holds nothing
//! but that one program. Codex itself may still reach the network for its own
//! housekeeping; the model endpoint never leaves this process.

use cagents::{
    chat::{Command as ChatCommand, NativeContinuity, Session, Snapshot},
    model::{HostSessionProof, TerminalId},
    terminal_ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope, ServerMessage,
        ServerResponse, read_frame, write_frame,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        fs::{MetadataExt, PermissionsExt, symlink},
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
struct Model {
    port: u16,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Model {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let seen = seen.clone();
                thread::spawn(move || serve(stream, &seen));
            }
        });
        Self { port, requests }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn serve(stream: TcpStream, seen: &Mutex<Vec<ModelRequest>>) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0;
    let mut chunked = false;
    let mut conversation = None;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap();
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap(),
            "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
            "content-encoding" => panic!("the fixture cannot read a {value} request body"),
            "session_id" | "conversation_id" => conversation = Some(value.to_owned()),
            _ => {}
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).unwrap();
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).unwrap();
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
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
        seen.push(ModelRequest {
            prompts: prompts.clone(),
            output: output.clone(),
            conversation: conversation
                .or_else(|| body["prompt_cache_key"].as_str().map(str::to_owned)),
        });
        seen.len()
    };
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
        Some(output) if scenario.contains("SCENARIO orphan-reaping")
            && !output.contains("ORPHAN_LOOP_FINISHED") => {
            let session = output.split("session ID ").nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|id| id.parse::<u64>().ok())
                .expect("the ongoing orphan loop reports its tool session");
            tool_call(&format!("orphan-poll-{number}"), "write_stdin",
                json!({"session_id":session,"chars":"","yield_time_ms":1000}))
        }
        Some(_) => json!({
            "type":"message","role":"assistant","id":format!("message-{number}"),
            "content":[{"type":"output_text","text":"SCENARIO_COMPLETE"}]
        }),
        None if scenario.contains("SCENARIO orphan-reaping") => shell(
            "orphan-tools", "i=0; while [ \"$i\" -lt 3600 ] && [ ! -e stop-orphan-loop ]; do sh -c 'sleep 1 & echo $! >> orphan-pids'; sleep 0.2; i=$((i+1)); done; echo ORPHAN_LOOP_FINISHED".into(), 1000,
        ),
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
        self.request_result(request).unwrap_or_else(|error| panic!("the daemon refused a request: {error:?}"))
    }

    fn request_result(&mut self, request: ClientRequest) -> Result<ServerResponse, ProtocolError> {
        let request_id = self.next;
        self.next += 1;
        exchange_result(
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
    exchange_result(stream, request_id, proof, request)
        .unwrap_or_else(|error| panic!("the daemon refused a request: {error:?}"))
}

fn exchange_result(
    stream: &mut UnixStream,
    request_id: u64,
    proof: Option<HostSessionProof>,
    request: ClientRequest,
) -> Result<ServerResponse, ProtocolError> {
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
            ServerMessage::Response { response } => return Ok(response),
            ServerMessage::Error(error) => return Err(error),
            _ => {}
        }
    }
}

// Every filesystem path and every process belongs to this fixture: the daemon
// has its own runtime, data and account directories, so it can be killed
// without touching an installed one.
struct Fixture {
    cli_program: PathBuf,
    root: tempfile::TempDir,
    daemon: Child,
    model: Model,
    version: String,
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
        Self::with_products(Path::new(env!("CARGO_BIN_EXE_cagents")), Path::new(env!("CARGO_BIN_EXE_cagentsd")))
    }

    fn with_products(cli_program: &Path, daemon_program: &Path) -> Self {
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

        // Keep native binaries/transcripts off a potentially tmpfs-backed /tmp.
        let evidence = std::env::var_os("CAGENTS_QUALIFICATION_EVIDENCE").map(PathBuf::from);
        let directory = evidence.as_deref().and_then(Path::parent)
            .filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("/var/tmp"));
        fs::create_dir_all(directory).unwrap();
        let directory = fs::canonicalize(directory).unwrap();
        let root = tempfile::Builder::new().prefix("codex-continuity-").tempdir_in(&directory).unwrap();
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
        let model = Model::start();
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
        let daemon = Self::daemon_at(root.path(), daemon_program);
        let fixture = Self {
            cli_program: cli_program.to_owned(),
            root,
            daemon,
            model,
            version,
        };
        fixture.await_daemon();
        fixture
    }

    fn daemon(root: &Path) -> Child {
        Self::daemon_at(root, Path::new(env!("CARGO_BIN_EXE_cagentsd")))
    }

    fn daemon_at(root: &Path, program: &Path) -> Child {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("daemon.log"))
            .unwrap();
        Self::command(root, program)
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
        let mut child = Self::command(self.root.path(), &self.cli_program)
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
        child.wait_with_output().unwrap()
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
        self.cli_program = PathBuf::from(env!("CARGO_BIN_EXE_cagents"));
        self.await_daemon();
    }

    fn marker(&self) -> PathBuf {
        self.root.path().join("project").join(APPROVAL_MARKER)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if thread::panicking() {
            self.root.disable_cleanup(true);
            eprintln!("Failed Codex qualification state retained at {}", self.root.path().display());
        }
        let preflight = self.output(&["daemon", "preflight"]);
        if let Ok(value) = serde_json::from_slice::<Value>(&preflight.stdout)
            && let Some(capability) = value["data"].get("capability")
        {
            let _ = self.output(&["daemon", "stop", "--confirm", &capability.to_string()]);
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        // A failed assertion can leave the provider and its holder running:
        // a durable holder is meant to outlive its daemon. Cleanup requires
        // this private cwd and the same pinned process incarnation.
        let root = self.root.path().to_path_buf();
        for (identity, _) in processes() {
            if let Err(error) = stop_fixture_process(identity, &root) {
                self.root.disable_cleanup(true);
                eprintln!("Could not clean exact fixture process {identity:?}: {error}; state retained");
            }
        }
        if thread::panicking() {
            // Both daemons append to these: what they printed, and their log.
            for log in [
                PathBuf::from("daemon.log"),
                Path::new("data")
                    .join(cagents::instance::STORAGE_NAME)
                    .join("cagentsd.log"),
            ] {
                if let Ok(text) = fs::read_to_string(self.root.path().join(&log)) {
                    eprintln!("--- fixture {} ---\n{text}", log.display());
                }
            }
        }
    }
}

/// Only private fixture processes qualify for teardown. Pin the process before
/// a second identity/ownership check; a numeric PID from the census alone is
/// never signal authority.
fn stop_fixture_process(identity: Identity, root: &Path) -> std::io::Result<()> {
    // /proc cwd links are absolute and resolve symlinks. An aliased evidence
    // directory must still identify the same private root; failure preserves
    // its evidence and never broadens signal authority.
    let root = fs::canonicalize(root)?;
    if identity.0 == std::process::id() || !fixture_process_matches(identity, &root) {
        return Ok(());
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, identity.0, 0) as i32 };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error) };
    }
    let pidfd = unsafe { fs::File::from_raw_fd(fd) };
    if !fixture_process_matches(identity, &root) { return Ok(()); }
    let result = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd.as_raw_fd(), libc::SIGKILL,
        std::ptr::null::<libc::siginfo_t>(), 0) };
    if result == 0 { return Ok(()); }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(error) }
}

fn fixture_process_matches((pid, start): Identity, root: &Path) -> bool {
    let process = PathBuf::from(format!("/proc/{pid}"));
    let start_matches = || fs::read_to_string(process.join("stat")).ok().and_then(|stat| {
        stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse::<u64>().ok()
    }) == Some(start);
    if !start_matches() || !fs::metadata(&process).is_ok_and(|metadata| metadata.uid() == unsafe { libc::geteuid() }) {
        return false;
    }
    // Merely naming a fixture path in argv (for example, a log reader) does
    // not make an unrelated process ours. All fixture launches and tools use
    // this private cwd; anything else is left untouched.
    let private = fs::read_link(process.join("cwd")).is_ok_and(|cwd| cwd.starts_with(root));
    private && start_matches()
}

#[test]
fn fixture_cleanup_requires_private_cwd_and_the_same_process_incarnation() {
    struct Probe(Child);
    impl Drop for Probe {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }
    let private = tempfile::Builder::new().prefix("codex-cleanup-private-").tempdir_in("/var/tmp").unwrap();
    let outside = tempfile::Builder::new().prefix("codex-cleanup-outside-").tempdir_in("/var/tmp").unwrap();
    let spawn = |cwd: &Path| Probe(Command::new("/bin/sleep").arg("600").env_clear().current_dir(cwd)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let mut owned = spawn(private.path());
    let mut unrelated = spawn(outside.path());
    let identities = processes();
    let identity = |pid| identities.iter().find(|((candidate, _), _)| *candidate == pid).unwrap().0;
    let owned_identity = identity(owned.0.id());
    stop_fixture_process((owned_identity.0, owned_identity.1 + 1), private.path()).unwrap();
    assert!(owned.0.try_wait().unwrap().is_none(), "a stale start time must not authorize a signal");
    stop_fixture_process(identity(unrelated.0.id()), private.path()).unwrap();
    assert!(unrelated.0.try_wait().unwrap().is_none(), "an exact pidfd alone must not admit an outside process");
    let alias = outside.path().join("private-alias");
    symlink(private.path(), &alias).unwrap();
    stop_fixture_process(owned_identity, &alias).unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(owned.0.wait().unwrap().signal(), Some(libc::SIGKILL));
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

/// Runs the real provider's execution tool against a local model fixture. The
/// parent shells exit before their background children, exercising subreaper
/// adoption continuously rather than just checking direct child cleanup.
#[test]
#[ignore = "ten-minute native Codex orphan qualification; no remote model calls"]
fn ten_minutes_of_background_tools_leave_no_holder_zombies() {
    let fixture = Fixture::new();
    fixture.start("orphan-reaping", "never");
    let mut window = fixture.window();
    let chat = fixture.durable_chat(&mut window);
    window.attach(chat.clone());
    let directory = fixture.root.path().join("run").join(cagents::instance::RUNTIME_SUBDIRECTORY).join("native-holders");
    let record: Value = until(Duration::from_secs(10), "the native holder registry", || {
        fs::read_dir(&directory).ok()?.flatten().filter(|entry| entry.path().extension().is_some_and(|extension| extension == "json"))
            .find_map(|entry| serde_json::from_slice(&fs::read(entry.path()).ok()?).ok())
    });
    let holder = record["holder"]["pid"].as_u64().unwrap();
    let holder_start = record["holder"]["start"].as_u64().unwrap();
    // Every exit has a finite SIGCHLD delivery/reap window. Record raw Z
    // observations honestly, and separately detect accumulation persisting
    // across a 250ms grace period; a product polling timer is not involved.
    let mut witnessed_children = BTreeSet::new();
    let mut census = || {
        let identity = || {
            let stat = fs::read_to_string(format!("/proc/{holder}/stat")).expect("fixture holder vanished");
            let fields = stat.rsplit_once(')').unwrap().1.split_whitespace().collect::<Vec<_>>();
            assert!(!matches!(fields.first(), Some(&"Z") | Some(&"X") | Some(&"x")),
                "the original holder must remain live; reparented children cannot prove reaping");
            fields[19].parse::<u64>().unwrap()
        };
        assert_eq!(identity(), holder_start, "the fixture holder incarnation changed");
        let mut children = BTreeSet::new();
        for task in fs::read_dir(format!("/proc/{holder}/task")).unwrap().flatten() {
            if let Ok(pids) = fs::read_to_string(task.path().join("children")) {
                children.extend(pids.split_whitespace().map(str::to_owned));
            }
        }
        let mut zombies = BTreeSet::new();
        for pid in children {
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                let fields = stat.rsplit_once(')').unwrap().1.split_whitespace().collect::<Vec<_>>();
                if fields.get(1).and_then(|parent| parent.parse::<u64>().ok()) == Some(holder) {
                    let identity = (pid, fields[19].parse::<u64>().unwrap());
                    witnessed_children.insert(identity.clone());
                    if fields.first() == Some(&"Z") { zombies.insert(identity); }
                }
            }
        }
        assert_eq!(identity(), holder_start);
        zombies
    };
    let started = Instant::now();
    let mut samples = 0u64;
    let mut observed = BTreeSet::new();
    let mut pending = BTreeMap::new();
    let mut persistent = BTreeSet::new();
    while started.elapsed() < Duration::from_secs(10 * 60) {
        let zombies = census();
        pending.retain(|identity, _| zombies.contains(identity));
        for identity in zombies {
            observed.insert(identity.clone());
            let since = pending.entry(identity.clone()).or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(250) { persistent.insert(identity); }
        }
        samples += 1;
        thread::sleep(Duration::from_millis(100));
    }
    // End only this fixture's tool loop, keeping its actual provider/holder
    // alive so the final zero census cannot be caused by owner destruction.
    fs::write(fixture.root.path().join("project/stop-orphan-loop"), b"settle").unwrap();
    until(Duration::from_secs(30), "the fixture orphan loop to settle", || {
        let snapshot = window.read(chat.id);
        assert!(!snapshot.exited, "the provider exited instead of completing its fixture loop");
        (snapshot.ready && !snapshot.busy && snapshot.requests.is_empty()).then_some(())
    });
    let final_provider = window.read(chat.id);
    assert!(final_provider.ready && !final_provider.exited,
        "the original provider must remain alive for the final holder census");
    let settle_deadline = Instant::now() + Duration::from_secs(2);
    let final_zombies = loop {
        let zombies = census();
        if zombies.is_empty() || Instant::now() >= settle_deadline { break zombies; }
        thread::sleep(Duration::from_millis(10));
    };
    drop(census);
    let tool_pids = fs::read_to_string(fixture.root.path().join("project/orphan-pids")).unwrap();
    let tools = tool_pids.lines().count();
    let tool_pids = tool_pids.lines().map(str::to_owned).collect::<BTreeSet<_>>();
    let witnessed_adoptions = witnessed_children.iter().filter(|(pid, _)| tool_pids.contains(pid)).count();
    let report = json!({"provider":fixture.version, "seconds":started.elapsed().as_secs_f64(),
        "holder":holder, "holder_start":holder_start, "provider_exited":final_provider.exited,
        "provider_ready":final_provider.ready, "samples":samples, "background_children":tools,
        "witnessed_background_adoptions":witnessed_adoptions,
        "raw_zombie_observations":observed, "persistent_zombies_250ms":persistent, "final_zombies":final_zombies});
    eprintln!("{report}");
    if let Some(path) = std::env::var_os("CAGENTS_QUALIFICATION_EVIDENCE") {
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    assert!(tools >= 1000, "the provider did not repeatedly background tools: {report}");
    assert!(witnessed_adoptions >= 100, "the fixture did not witness repeated background-child adoption by this holder: {report}");
    assert!(persistent.is_empty(), "holder children remained unreaped across the measured grace period: {report}");
    assert!(final_zombies.is_empty(), "the same live holder retained zombies after its tool loop settled: {report}");
    let diagnostics = fixture.cli(&["diagnostics"]);
    let reports = diagnostics["holders"].as_array().expect("CLI holder diagnostics are absent");
    assert!(reports.iter().any(|report| report["pid"] == holder && report["zombie_children"] == 0));
    close(&fixture, &mut window, &chat);
}

/// Local idle is not an atomic backend quiescence barrier. A refused update
/// must leave the genuinely older owner, its conversation and history usable.
#[test]
#[ignore = "needs matched old binaries and native Codex; no remote model calls"]
fn an_observed_idle_old_helper_is_preserved_when_safe_replacement_cannot_be_proved() {
    let old_cli = std::env::var_os("CAGENTS_QUALIFY_OLD_CLI").expect("set CAGENTS_QUALIFY_OLD_CLI");
    let old_daemon = std::env::var_os("CAGENTS_QUALIFY_OLD_DAEMON").expect("set CAGENTS_QUALIFY_OLD_DAEMON");
    let mut fixture = Fixture::with_products(Path::new(&old_cli), Path::new(&old_daemon));
    fixture.start("long", "never");
    until(Duration::from_secs(30), "the older provider turn", || (!fixture.model.requests().is_empty()).then_some(()));
    let old_servers = fixture.servers();
    assert_eq!(old_servers.len(), 1);
    fixture.replace_daemon();
    let mut window = fixture.window();
    let chat = fixture.durable_chat(&mut window);
    let attached = window.attach(chat.clone());
    assert!(attached.native_helper.as_ref().is_some_and(|helper| helper.outdated));
    until(Duration::from_secs(COMMAND_SECONDS + 30), "the old turn to finish", || {
        let snapshot = window.read(chat.id);
        (!snapshot.busy && snapshot.ready && !snapshot.exited && snapshot.requests.is_empty()).then_some(snapshot)
    });
    let requests = fixture.model.requests().len();
    let idle = until(Duration::from_secs(20), "the exact idle-generation safety refusal", || {
        let idle = window.read(chat.id);
        let error = window.request_result(ClientRequest::ChatRestartHelper { source: chat.clone(), generation: idle.generation })
            .expect_err("an observed idle snapshot must not authorize automatic provider replacement");
        assert_eq!(error.code, ProtocolErrorCode::TerminalUnavailable);
        if error.message.contains("cannot prove safe automatic replacement") { Some(idle) }
        else {
            assert!(error.message.contains("Provider state changed before conditional idle close"), "{error:?}");
            None
        }
    });
    let after = window.read(chat.id);
    assert!(after.ready && !after.exited);
    assert_eq!(after.session_id, idle.session_id);
    assert!(after.items.len() >= idle.items.len());
    assert_eq!(fixture.servers(), old_servers, "a refused replacement must keep the same provider incarnation");
    assert_eq!(fixture.model.requests().len(), requests, "refusal must not replay a prompt");
    assert!(window.chats().contains(&chat), "the issued session must remain published after refusal");
    close(&fixture, &mut window, &chat);
}

/// Uses genuine binaries of the previous build, then replaces only its private
/// fixture daemon. The old durable holder remains alive until the idle restart
/// request proves cleanup and starts the same external thread in a new helper.
#[test]
#[ignore = "unmet acceptance: needs a verified provider conditional-close barrier, matched old binaries and native Codex"]
fn an_old_helper_is_flagged_and_rolls_over_without_changing_its_conversation() {
    let old_cli = std::env::var_os("CAGENTS_QUALIFY_OLD_CLI").expect("set CAGENTS_QUALIFY_OLD_CLI to the matched older CLI");
    let old_daemon = std::env::var_os("CAGENTS_QUALIFY_OLD_DAEMON").expect("set CAGENTS_QUALIFY_OLD_DAEMON to the matched older daemon");
    let mut fixture = Fixture::with_products(Path::new(&old_cli), Path::new(&old_daemon));
    fixture.start("long", "never");
    until(Duration::from_secs(30), "the older provider turn", || (!fixture.model.requests().is_empty()).then_some(()));
    thread::sleep(Duration::from_secs(2));
    let old_servers = fixture.servers();
    assert_eq!(old_servers.len(), 1);
    fixture.replace_daemon();
    let mut window = fixture.window();
    let old_chat = fixture.durable_chat(&mut window);
    let attached = window.attach(old_chat.clone());
    assert!(attached.native_helper.as_ref().is_some_and(|helper| helper.outdated),
        "the genuinely older helper was not identified: {:?}", attached.native_helper);
    assert_eq!(fixture.servers(), old_servers, "adoption replaced the old provider before permission");
    let idle = until(Duration::from_secs(COMMAND_SECONDS + 30), "the exact old turn to finish", || {
        let snapshot = window.read(old_chat.id);
        (!snapshot.busy && snapshot.ready && snapshot.requests.is_empty()).then_some(snapshot)
    });
    let prepared = match window.request(ClientRequest::ChatRestartHelper { source: old_chat.clone(), generation: idle.generation }) {
        ServerResponse::ChatResumePrepared { session } => session,
        response => panic!("helper restart was not prepared: {response:?}"),
    };
    assert_eq!(prepared.launch.session_id, old_chat.launch.session_id);
    let (replacement, snapshot) = match window.request(ClientRequest::ChatStart { session: prepared,
        source: None, terminate_source: false, execution_environment: Default::default() }) {
        ServerResponse::ChatAttached { session, snapshot } => (session, snapshot),
        response => panic!("replacement helper did not start: {response:?}"),
    };
    assert_eq!(snapshot.session_id, old_chat.launch.session_id);
    assert!(snapshot.native_helper.as_ref().is_some_and(|helper| !helper.outdated));
    assert!(cagents::chat::is_helper_recovery(&old_chat, &replacement));
    assert!(fixture.servers().is_disjoint(&old_servers), "the outdated provider was retained");
    window.request(ClientRequest::ChatVisibility { chat_id: replacement.id, visible: true });
    let restored = window.read(replacement.id);
    assert!(!restored.items.is_empty(), "the same conversation resumed without its transcript");
    eprintln!("helper rollover retained thread {:?}: {} -> {}", replacement.launch.session_id, old_chat.id, replacement.id);
    close(&fixture, &mut window, &replacement);
}
