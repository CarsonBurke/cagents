//! Asking a provider's own CLI how an account stands.
//!
//! Every question goes through the provider's official client, as that
//! client's own documented protocol: Claude Code's stream-json control
//! requests and `claude auth status`, Codex's app server. cagents never reads
//! a credential file and never calls a provider's service itself, so a login
//! stays the property of the client it belongs to.
//!
//! A probe runs on the async runtime, never the interface thread. It is
//! bounded in time ([`TIMEOUT`]) and in how much output it will read
//! ([`OUTPUT_LIMIT`]); the CLI's stderr is discarded; and nothing it was sent
//! or printed, nor its environment, is logged, because those carry the
//! account's identity. The child is started in its own process group and the
//! group is killed on the way out, so a probe leaves nothing running.

use std::{
    ffi::{OsStr, OsString},
    io,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

use super::{
    Account, Identity, UsageSnapshot, UsageSource, UsageStatus, cache::Cache, cache::Entry,
    schedule::STALE_AFTER,
};
use crate::agent::Provider;

/// How long a probe may take in total.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// How much of a CLI's stdout a probe reads before giving up on it.
pub const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;
/// How long the reader's shell is given to say what `PATH` it sets up.
const SHELL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a CLI is given to exit after its input closes.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How often `finish` looks for the CLI to have exited on its own.
const EXIT_POLL: Duration = Duration::from_millis(20);

/// Variables that would make a probe behave unlike the user's own shell:
/// Claude Code's markers for "running inside Claude Code", and cagents' own
/// launch handshake.
const SCRUBBED_ENVIRONMENT: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    crate::instance::UI_LAUNCH_HANDSHAKE_ENV,
];

/// What the reader's shell is asked to run: print its `PATH`, and nothing
/// else of its environment, after [`SHELL_PATH_MARKER`] and its length in
/// bytes, and before a NUL that ends it. One `printf` writes it all at once,
/// so nothing a startup file printed, or a job it left running prints, lands
/// in between.
///
/// The printing is left to `/bin/sh`, whose `printf` is the same whatever
/// the reader's shell; theirs only has to run it, and every shell reads
/// single quotes as literal. `LC_ALL=C` makes `${#PATH}` count bytes where
/// `/bin/sh` would otherwise count a UTF-8 locale's characters.
const SHELL_PATH_COMMAND: &str =
    r#"exec /bin/sh -c 'LC_ALL=C; printf "__cagents_path__%d:%s\0" "${#PATH}" "$PATH"'"#;
/// What the shell prints in front of its `PATH`.
const SHELL_PATH_MARKER: &[u8] = b"__cagents_path__";
/// The most a shell may print before its `PATH`: a startup file that prints
/// more is not waited out.
const MAX_SHELL_OUTPUT: usize = 1 << 20;

/// Variables the shell asked for its `PATH` goes without as well, so that a
/// startup file that wants a password or a key's passphrase cannot put a
/// prompt in front of the reader.
const SHELL_SCRUBBED_ENVIRONMENT: &[&str] = &[
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "SUDO_ASKPASS",
    "GIT_ASKPASS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
];

/// What a probe learned.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The account's state, which may be "signed out" or "not installed".
    Answered {
        identity: Option<Identity>,
        snapshot: UsageSnapshot,
    },
    /// The CLI gave no usable answer. The message is cagents' own summary of
    /// the failure, never the CLI's output.
    Failed(String),
    /// Another window is probing this provider right now.
    Busy,
}

/// Probes `account`, sharing the answer with other windows through `cache`.
///
/// Unless `force`d, a cached answer another window observed within
/// [`STALE_AFTER`] is returned instead of probing again.
pub async fn run(account: Account, cache: Option<Cache>, force: bool) -> Outcome {
    let now = now_ms();
    let search_path = search_path().await;
    let setup = {
        let account = account.clone();
        let cache = cache.clone();
        tokio::task::spawn_blocking(move || {
            prepare(&account, cache.as_ref(), force, now, search_path.as_deref())
        })
        .await
    };
    let (program, key, lock) = match setup {
        Ok(Prepared::Done(outcome)) => return outcome,
        Ok(Prepared::Probe { program, key, lock }) => (program, key, lock),
        Err(_) => return Outcome::Failed("The probe could not be prepared".into()),
    };
    let probed = tokio::time::timeout(TIMEOUT, probe_cli(&account, &program)).await;
    let outcome = match probed {
        Ok(Ok((identity, snapshot))) => Outcome::Answered { identity, snapshot },
        Ok(Err(message)) => Outcome::Failed(message),
        Err(_) => Outcome::Failed(format!(
            "{} did not answer within {} seconds",
            cli_name(account.provider),
            TIMEOUT.as_secs()
        )),
    };
    if let (Outcome::Answered { identity, snapshot }, Some(cache), Some(key)) =
        (&outcome, cache, key)
    {
        let entry = Entry {
            identity: identity.clone(),
            snapshot: snapshot.clone(),
        };
        // Written in the background so the answer reaches the reader at
        // once, with the probe lock held until the answer is in the cache:
        // a window that takes the lock next finds it there instead of
        // probing again. A cache that cannot be written only costs other
        // windows a probe.
        tokio::task::spawn_blocking(move || {
            let _ = cache.store(&key, entry);
            drop(lock);
        });
    }
    outcome
}

enum Prepared {
    Done(Outcome),
    Probe {
        program: PathBuf,
        key: Option<String>,
        lock: Option<std::fs::File>,
    },
}

fn prepare(
    account: &Account,
    cache: Option<&Cache>,
    force: bool,
    now: u64,
    search_path: Option<&OsStr>,
) -> Prepared {
    let unavailable = |detail: String| {
        Prepared::Done(answered(
            None,
            UsageSnapshot::state(UsageStatus::Unavailable, detail, now),
        ))
    };
    // An account whose home was never created was never set up. It is not a
    // login to prompt for - most people have one of the two CLIs, not both -
    // so it reports nothing rather than a "Sign in" chip.
    if !account.resolved_home().is_some_and(|home| home.is_dir()) {
        return unavailable(format!(
            "{} has not been set up",
            cli_name(account.provider)
        ));
    }
    let Some(program) = find_executable(account.provider.executable(), search_path) else {
        return unavailable(format!("{} is not installed", cli_name(account.provider)));
    };
    let key = account.cache_key();
    let Some(cache) = cache else {
        return Prepared::Probe {
            program,
            key,
            lock: None,
        };
    };
    let lock = match cache.try_probe_lock(&account.lock_name()) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => return Prepared::Done(Outcome::Busy),
        // Without a lock directory the probe still runs; it only loses the
        // guarantee that another window is not running the same one.
        Err(_) => None,
    };
    if !force
        && let Some(entry) = key.as_ref().and_then(|key| cache.load(now).remove(key))
        && now.saturating_sub(entry.snapshot.observed_at) < STALE_AFTER
    {
        let mut snapshot = entry.snapshot;
        snapshot.source = UsageSource::Cache;
        return Prepared::Done(answered(entry.identity, snapshot));
    }
    Prepared::Probe { program, key, lock }
}

fn answered(identity: Option<Identity>, snapshot: UsageSnapshot) -> Outcome {
    Outcome::Answered { identity, snapshot }
}

pub fn cli_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "Claude Code",
        Provider::Codex => "Codex",
        Provider::Omp => "Omp",
    }
}

fn now_ms() -> u64 {
    crate::chat::now_ms().unwrap_or_default()
}

async fn probe_cli(
    account: &Account,
    program: &Path,
) -> Result<(Option<Identity>, UsageSnapshot), String> {
    match account.provider {
        Provider::Claude => probe_claude(account, program).await,
        Provider::Codex => probe_codex(account, program).await,
        Provider::Omp => Err("Omp does not report account usage".into()),
    }
}

/// The `PATH` the reader's shell sets up, once asked: `Some(None)` when it
/// could not say.
static SHELL_PATH: tokio::sync::OnceCell<Option<OsString>> = tokio::sync::OnceCell::const_new();

/// The directories a provider's CLI is looked for in: the ones a terminal
/// would search.
///
/// A terminal runs the reader's shell interactively, and its startup files
/// are where installers put `~/.local/bin` or a Node version manager's `bin`.
/// The interface may have been started from a desktop launcher with a much
/// shorter `PATH`, and a CLI the reader runs in every terminal would then
/// read as not installed. So the shell is asked, once per process; where it
/// cannot say, this process's own `PATH` stands in.
pub async fn search_path() -> Option<OsString> {
    SHELL_PATH
        .get_or_init(|| async {
            let shell = crate::model::login_shell().ok()?;
            shell_path(Path::new(&shell), SHELL_TIMEOUT).await
        })
        .await;
    current_search_path()
}

/// What [`search_path`] would answer now, without waiting for the shell:
/// this process's own `PATH` until the shell has been asked.
pub fn current_search_path() -> Option<OsString> {
    shell_search_path()
        .map(OsStr::to_owned)
        .or_else(|| std::env::var_os("PATH"))
}

/// Whether the shell has been asked for its `PATH`, so that
/// [`current_search_path`] is [`search_path`]'s final answer.
pub fn shell_asked() -> bool {
    SHELL_PATH.initialized()
}

/// The shell's `PATH`, if [`search_path`] has already read one. A CLI found on
/// it runs with it, as it would in a terminal: a Node CLI's
/// `#!/usr/bin/env node` looks for Node there.
pub fn shell_search_path() -> Option<&'static OsStr> {
    SHELL_PATH.get()?.as_deref()
}

/// The `PATH` `shell` sets up when started interactively, as a terminal
/// starts it, or `None` if it does not say within `timeout`.
///
/// Interactive, and not a login shell, because that is how a terminal starts
/// it: the startup files that put a CLI on the `PATH` - `.bashrc`, `.zshrc`,
/// a Node version manager's hook - are the interactive ones, which a login
/// shell run without a terminal would skip.
///
/// It runs with no terminal and its output discarded but for the `PATH` it
/// prints, in its own process group, which goes with it. Nothing it starts
/// can ask the reader for anything: no terminal, no input, no display, no
/// askpass.
async fn shell_path(shell: &Path, timeout: Duration) -> Option<OsString> {
    let mut command = Command::new(shell);
    command.args(["-i", "-c", SHELL_PATH_COMMAND]);
    for variable in SCRUBBED_ENVIRONMENT
        .iter()
        .chain(SHELL_SCRUBBED_ENVIRONMENT)
    {
        command.env_remove(variable);
    }
    if let Some(home) = std::env::var_os("HOME") {
        command.current_dir(home);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut group = Group::spawn(command).await.ok()?;
    let mut stdout = group.stdout().ok()?;
    // A job a startup file left running may hold the pipe open long after
    // the value, so the read stops at the value rather than at the end of
    // the output.
    let read = tokio::time::timeout(timeout, async {
        let mut output = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(read) => output.extend_from_slice(&chunk[..read]),
            }
            if let Some(path) = path_from_output(&output) {
                return Some(path);
            }
            if output.len() > MAX_SHELL_OUTPUT {
                return None;
            }
        }
    })
    .await;
    drop(stdout);
    // A shell that has not answered in time is not given a grace to exit.
    if read.is_err() {
        group.kill();
    }
    group.finish().await;
    read.ok()?
}

/// The `PATH` in what the shell printed, after whatever a startup file
/// printed first: the first marker followed by a length, a colon, a value of
/// that many bytes and a NUL. The length is what tells the value apart from
/// startup output that happens to hold the marker or a NUL, and from a
/// `PATH` that holds the marker itself.
fn path_from_output(output: &[u8]) -> Option<OsString> {
    let mut from = 0;
    while let Some(found) = output[from..]
        .windows(SHELL_PATH_MARKER.len())
        .position(|window| window == SHELL_PATH_MARKER)
    {
        from += found + SHELL_PATH_MARKER.len();
        if let Some(value) = length_prefixed(&output[from..]) {
            return (!value.is_empty()).then(|| OsString::from_vec(value.to_vec()));
        }
    }
    None
}

/// The value at the start of `text`, written as its length in decimal, a
/// colon, the value and a NUL, if `text` holds all of it.
fn length_prefixed(text: &[u8]) -> Option<&[u8]> {
    let colon = text.iter().position(|byte| *byte == b':')?;
    let digits = &text[..colon];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let length: usize = std::str::from_utf8(digits).ok()?.parse().ok()?;
    let value = &text[colon + 1..];
    (value.get(length) == Some(&0)).then(|| &value[..length])
}

/// The first executable named `name` on `search_path`.
///
/// Only absolute entries are searched: a relative entry would resolve
/// against whatever directory the interface happens to run in.
pub fn find_executable(name: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(search_path?)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn command(account: &Account, program: &Path) -> Command {
    let mut command = Command::new(program);
    for variable in SCRUBBED_ENVIRONMENT {
        command.env_remove(variable);
    }
    if let Some((variable, home)) = account.probe_environment() {
        command.env(variable, home);
    }
    if let Some(path) = shell_search_path() {
        command.env("PATH", path);
    }
    if let Some(home) = std::env::var_os("HOME") {
        command.current_dir(home);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    command
}

/// A spawned CLI and the process group it leads, which goes with it: when
/// the probe finishes, and when it is abandoned by its timeout, so nothing
/// the CLI started outlives the probe.
struct Group {
    child: Child,
    /// The group's id - the child's pid - until the group has been killed.
    id: Option<i32>,
}

impl Group {
    /// Starts `command`. An executable that is still open for writing -
    /// just installed or updated, or inherited for an instant by a process
    /// another thread is forking - is busy rather than missing, so it is
    /// tried again a few times before the start counts as failed.
    async fn spawn(mut command: Command) -> Result<Self, String> {
        let mut delay = Duration::from_millis(10);
        let child = loop {
            match command.spawn() {
                Ok(child) => break child,
                Err(error)
                    if error.kind() == io::ErrorKind::ExecutableFileBusy
                        && delay <= Duration::from_millis(160) =>
                {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(error) => return Err(spawn_failure(&error)),
            }
        };
        let id = child.id().and_then(|id| i32::try_from(id).ok());
        Ok(Self { child, id })
    }

    fn stdout(&mut self) -> Result<tokio::io::Take<ChildStdout>, String> {
        self.child
            .stdout
            .take()
            .map(|stdout| stdout.take(OUTPUT_LIMIT))
            .ok_or_else(|| "The CLI's output could not be read".to_owned())
    }

    /// Lets the CLI exit on its own for a moment, then kills whatever is
    /// left of its group and only then reaps it. The caller has already
    /// dropped its ends of the pipes, so a CLI still writing cannot block on
    /// them.
    async fn finish(&mut self) {
        let deadline = tokio::time::Instant::now() + EXIT_GRACE;
        while !self.leader_exited() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(EXIT_POLL).await;
        }
        self.kill();
        let _ = self.child.wait().await;
    }

    /// Whether the leader has exited, without reaping it: until it is
    /// reaped its pid - the group's id - cannot be handed to anyone else.
    fn leader_exited(&self) -> bool {
        let Some(id) = self.id else {
            return true;
        };
        // SAFETY: an all-zero `siginfo_t` is a valid value for `waitid` to
        // fill in; `WNOWAIT` leaves the child waitable for `Child::wait`.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let result = libc::waitid(
                libc::P_PID,
                id as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            // An error means there is nothing left to wait for.
            result != 0 || info.si_pid() != 0
        }
    }

    fn kill(&mut self) {
        if let Some(id) = self.id.take() {
            // SAFETY: `killpg` only sends a signal. The id is a group this
            // probe created, and its leader is not reaped until after this
            // call - `finish` waits without reaping, and a dropped child is
            // reaped later by tokio - so the id cannot belong to anyone else.
            unsafe {
                libc::killpg(id, libc::SIGKILL);
            }
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        // Reached without `finish` when a probe is abandoned mid-way, by its
        // timeout most of all. `kill_on_drop` covers the child; this covers
        // anything it started.
        self.kill();
    }
}

/// A running CLI speaking JSON lines.
struct Session {
    group: Group,
    stdin: Option<ChildStdin>,
    lines: tokio::io::Lines<BufReader<tokio::io::Take<ChildStdout>>>,
}

impl Session {
    async fn spawn(mut command: Command) -> Result<Self, String> {
        command.stdin(Stdio::piped());
        let mut group = Group::spawn(command).await?;
        let stdin = group.child.stdin.take();
        let stdout = group.stdout()?;
        Ok(Self {
            group,
            stdin,
            lines: BufReader::new(stdout).lines(),
        })
    }

    async fn send(&mut self, frame: &Value) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "The CLI's input is closed".to_owned())?;
        let mut line = serde_json::to_vec(frame).map_err(|error| error.to_string())?;
        line.push(b'\n');
        stdin
            .write_all(&line)
            .await
            .map_err(|_| "The CLI stopped reading its input".to_owned())?;
        stdin
            .flush()
            .await
            .map_err(|_| "The CLI stopped reading its input".to_owned())
    }

    /// The next JSON frame the CLI printed, skipping anything else it wrote.
    async fn next(&mut self) -> Result<Value, String> {
        loop {
            match self.lines.next_line().await {
                Ok(Some(line)) => {
                    if let Ok(frame) = serde_json::from_str::<Value>(&line)
                        && frame.is_object()
                    {
                        return Ok(frame);
                    }
                }
                Ok(None) => return Err("The CLI exited before answering".into()),
                Err(_) => return Err("The CLI's output could not be read".into()),
            }
        }
    }

    /// Closes the CLI's input and output, and finishes its group.
    async fn close(self) {
        let Self {
            mut group,
            stdin,
            lines,
        } = self;
        drop(stdin);
        drop(lines);
        group.finish().await;
    }
}

fn spawn_failure(error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        "The CLI is not installed".into()
    } else {
        "The CLI could not be started".into()
    }
}

/// Runs a CLI that prints one JSON document and exits.
async fn run_json(command: Command) -> Result<Value, String> {
    let mut group = Group::spawn(command).await?;
    let mut output = Vec::new();
    // The output is dropped once read, so a CLI that writes past the limit
    // meets a closed pipe rather than blocking until the timeout.
    let read = group.stdout()?.read_to_end(&mut output).await;
    group.finish().await;
    read.map_err(|_| "The CLI's output could not be read".to_owned())?;
    serde_json::from_slice(&output).map_err(|_| "The CLI printed something unexpected".to_owned())
}

/// Signs `account` out through its own CLI - `claude auth logout`, `codex
/// logout` - before its home is deleted, so a login kept outside the home,
/// in a system keyring, goes with it.
///
/// A CLI that is missing, fails, or says there was nothing to sign out of is
/// not an error: the home is going regardless, and only the CLI knows where
/// else its login lived. Only a managed account is ever signed out here; the
/// system account's login is the reader's own.
pub async fn logout(account: Account) -> Result<(), String> {
    if account.is_system() {
        return Err("The system account is not signed out by cagents".into());
    }
    let arguments: &[&str] = match account.provider {
        Provider::Claude => &["auth", "logout"],
        Provider::Codex => &["logout"],
        Provider::Omp => return Ok(()),
    };
    let executable = account.provider.executable();
    let search_path = search_path().await;
    let Some(program) =
        tokio::task::spawn_blocking(move || find_executable(executable, search_path.as_deref()))
            .await
            .ok()
            .flatten()
    else {
        return Ok(());
    };
    let mut command = command(&account, &program);
    command.args(arguments);
    let mut group = Group::spawn(command).await?;
    let finished = tokio::time::timeout(TIMEOUT, async {
        let mut discard = Vec::new();
        let _ = group.stdout()?.read_to_end(&mut discard).await;
        Ok::<(), String>(())
    })
    .await;
    group.finish().await;
    match finished {
        Ok(_) => Ok(()),
        Err(_) => Err(format!(
            "{} did not sign out within {} seconds",
            cli_name(account.provider),
            TIMEOUT.as_secs()
        )),
    }
}

/// Claude Code: `claude auth status --json` for the identity, and the
/// stream-json `get_usage` control request for the limits, concurrently.
///
/// The usage session starts with no setting sources, no MCP servers, no
/// slash commands and no session persistence: it only ever answers two
/// control requests and never takes a turn, and that way it runs none of the
/// user's hooks and leaves nothing behind in their history.
async fn probe_claude(
    account: &Account,
    program: &Path,
) -> Result<(Option<Identity>, UsageSnapshot), String> {
    let mut status = command(account, program);
    status.args(["auth", "status", "--json"]);
    let mut usage = command(account, program);
    usage.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--disable-slash-commands",
    ]);
    let (status, usage) = tokio::join!(run_json(status), claude_usage(usage));
    let observed_at = now_ms();
    let identity = match status.as_ref().map(super::claude::parse_auth_status) {
        Ok(Err(state)) => {
            return Ok((
                None,
                UsageSnapshot::state(state, "Signed out of Claude Code", observed_at),
            ));
        }
        Ok(Ok(identity)) => Some(identity),
        Err(_) => None,
    };
    let snapshot = match usage {
        Ok(Ok(response)) => super::claude::parse_usage(&response, observed_at),
        Ok(Err(message)) => match super::claude::error_status(&message) {
            Some(UsageStatus::NeedsLogin) => UsageSnapshot::state(
                UsageStatus::NeedsLogin,
                "Signed out of Claude Code",
                observed_at,
            ),
            Some(state) => UsageSnapshot::state(
                state,
                "This version of Claude Code does not report usage",
                observed_at,
            ),
            // Most likely the service, not the account: retried with
            // backoff while the figures already on screen stay.
            None => return Err("Claude Code could not read usage just now".into()),
        },
        // The session never answered: a failure to back off from, even when
        // the identity came back, since the limits are what is shown.
        Err(message) => return Err(message),
    };
    Ok((identity, snapshot))
}

/// The `get_usage` control response's inner `response`, or the error message
/// Claude answered the request with.
async fn claude_usage(command: Command) -> Result<Result<Value, String>, String> {
    let mut session = Session::spawn(command).await?;
    let result = claude_exchange(&mut session).await;
    session.close().await;
    result
}

async fn claude_exchange(session: &mut Session) -> Result<Result<Value, String>, String> {
    const INITIALIZE: &str = "cagents-usage-initialize";
    const USAGE: &str = "cagents-usage";
    session
        .send(&json!({"type": "control_request", "request_id": INITIALIZE,
            "request": {"subtype": "initialize"}}))
        .await?;
    if let Err(message) = claude_response(session, INITIALIZE).await? {
        return Ok(Err(message));
    }
    session
        .send(&json!({"type": "control_request", "request_id": USAGE,
            "request": {"subtype": "get_usage", "skip_behaviors": true}}))
        .await?;
    claude_response(session, USAGE).await
}

async fn claude_response(
    session: &mut Session,
    request_id: &str,
) -> Result<Result<Value, String>, String> {
    loop {
        let frame = session.next().await?;
        if let Some(answer) = match_control_response(&frame, request_id) {
            return Ok(answer);
        }
    }
}

/// The answer `frame` carries for `request_id`, if it is that control
/// response: the success payload, or the error message.
pub fn match_control_response(frame: &Value, request_id: &str) -> Option<Result<Value, String>> {
    if frame["type"] != "control_response" {
        return None;
    }
    let response = &frame["response"];
    if response["request_id"].as_str() != Some(request_id) {
        return None;
    }
    Some(match response["subtype"].as_str() {
        Some("success") => Ok(response["response"].clone()),
        _ => Err(response["error"]
            .as_str()
            .unwrap_or("Claude Code refused the request")
            .to_owned()),
    })
}

/// Codex: `codex app-server` over stdio, asked for the account and its
/// limits and then closed.
async fn probe_codex(
    account: &Account,
    program: &Path,
) -> Result<(Option<Identity>, UsageSnapshot), String> {
    let mut command = command(account, program);
    command.arg("app-server");
    let mut session = Session::spawn(command).await?;
    let result = codex_exchange(&mut session).await;
    session.close().await;
    let (account_read, limits_read) = result?;
    let observed_at = now_ms();
    let identity = match account_read {
        Ok(result) => match super::codex::parse_account(&result) {
            Ok(identity) => Some(identity),
            Err(state) => {
                let detail = match state {
                    UsageStatus::NeedsLogin => "Signed out of Codex",
                    _ => "Codex is not signed in to a ChatGPT plan",
                };
                return Ok((None, UsageSnapshot::state(state, detail, observed_at)));
            }
        },
        Err(_) => None,
    };
    let snapshot = match limits_read {
        Ok(result) => super::codex::parse_rate_limits(&result, observed_at),
        Err(error) => match super::codex::error_status(&error) {
            Some(UsageStatus::NeedsLogin) => UsageSnapshot::state(
                UsageStatus::NeedsLogin,
                "Codex needs you to sign in again",
                observed_at,
            ),
            Some(state) => UsageSnapshot::state(
                state,
                "This version of Codex does not report usage",
                observed_at,
            ),
            None => return Err("Codex could not read usage just now".into()),
        },
    };
    Ok((identity, snapshot))
}

type RpcResult = Result<Value, Value>;

async fn codex_exchange(session: &mut Session) -> Result<(RpcResult, RpcResult), String> {
    session
        .send(&json!({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "cagents", "title": "cagents", "version": env!("CARGO_PKG_VERSION")},
        }}))
        .await?;
    codex_response(session, 1)
        .await?
        .map_err(|_| "Codex refused to start".to_owned())?;
    session.send(&json!({"method": "initialized"})).await?;
    session
        .send(&json!({"id": 2, "method": "account/read", "params": {}}))
        .await?;
    session
        .send(&json!({"id": 3, "method": "account/rateLimits/read"}))
        .await?;
    let (mut account, mut limits) = (None, None);
    while account.is_none() || limits.is_none() {
        let frame = session.next().await?;
        match frame["id"].as_i64() {
            Some(2) if frame.get("method").is_none() => account = Some(rpc_result(frame)),
            Some(3) if frame.get("method").is_none() => limits = Some(rpc_result(frame)),
            _ => {}
        }
    }
    Ok((account.unwrap(), limits.unwrap()))
}

async fn codex_response(session: &mut Session, id: i64) -> Result<RpcResult, String> {
    loop {
        let frame = session.next().await?;
        if frame["id"].as_i64() == Some(id) && frame.get("method").is_none() {
            return Ok(rpc_result(frame));
        }
    }
}

fn rpc_result(mut frame: Value) -> RpcResult {
    match frame.get_mut("error").map(Value::take) {
        Some(error) if !error.is_null() => Err(error),
        _ => Ok(frame
            .get_mut("result")
            .map(Value::take)
            .unwrap_or(Value::Null)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_matching_control_response_answers() {
        let success = json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "cagents-usage",
            "response": {"subscription_type": "max"}}});
        assert_eq!(
            match_control_response(&success, "cagents-usage"),
            Some(Ok(json!({"subscription_type": "max"})))
        );
        assert_eq!(match_control_response(&success, "other"), None);
        assert_eq!(
            match_control_response(
                &json!({"type": "system", "subtype": "init"}),
                "cagents-usage"
            ),
            None
        );
        let error = json!({"type": "control_response", "response": {
            "subtype": "error", "request_id": "cagents-usage", "error": "Not logged in"}});
        assert_eq!(
            match_control_response(&error, "cagents-usage"),
            Some(Err("Not logged in".into()))
        );
    }

    #[test]
    fn rpc_frames_split_into_results_and_errors() {
        assert_eq!(
            rpc_result(json!({"id": 2, "result": {"a": 1}})),
            Ok(json!({"a": 1}))
        );
        assert_eq!(
            rpc_result(json!({"id": 3, "error": {"code": -1}})),
            Err(json!({"code": -1}))
        );
        assert_eq!(
            rpc_result(json!({"id": 3, "error": null, "result": 5})),
            Ok(json!(5))
        );
    }

    /// A fake CLI stands in for the provider's: the probe has to speak the
    /// protocol, read only the answer it asked for, and leave nothing running.
    #[tokio::test]
    async fn a_scripted_claude_answers_through_the_stream_protocol() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("claude");
        std::fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" = auth ]; then
  echo '{"loggedIn": true, "email": "user@example.com", "subscriptionType": "max"}'
  exit 0
fi
read init
echo 'not json'
echo '{"type":"control_response","response":{"subtype":"success","request_id":"cagents-usage-initialize","response":{}}}'
read usage
echo '{"type":"control_response","response":{"subtype":"success","request_id":"someone-else","response":{}}}'
echo '{"type":"control_response","response":{"subtype":"success","request_id":"cagents-usage","response":{"subscription_type":"max","rate_limits":{"limits":[{"kind":"session","group":"session","percent":25,"resets_at":null,"is_active":true}]}}}}'
sleep 30
"#,
        )
        .unwrap();
        make_executable(&script);
        let account = Account::system(Provider::Claude);
        let (identity, snapshot) = probe_claude(&account, &script).await.unwrap();
        assert_eq!(identity.unwrap().email.as_deref(), Some("user@example.com"));
        assert_eq!(snapshot.windows.len(), 1);
        assert!(snapshot.windows[0].binding);
        assert_eq!(snapshot.status, UsageStatus::Ok);
    }

    #[tokio::test]
    async fn a_signed_out_claude_asks_for_a_login() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\nif [ \"$1\" = auth ]; then echo '{\"loggedIn\": false}'; exit 1; fi\nexit 1\n",
        )
        .unwrap();
        make_executable(&script);
        let (identity, snapshot) = probe_claude(&Account::system(Provider::Claude), &script)
            .await
            .unwrap();
        assert!(identity.is_none());
        assert_eq!(snapshot.status, UsageStatus::NeedsLogin);
    }

    #[tokio::test]
    async fn a_stale_codex_login_asks_for_a_login() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("codex");
        let error = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/accounts/codex-rate-limits-401.json"),
        )
        .unwrap();
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
read initialize
echo '{{"method":"remoteControl/status/changed","params":{{}}}}'
echo '{{"id":1,"result":{{"codexHome":"/tmp"}}}}'
read initialized
read account
read limits
echo '{{"id":2,"result":{{"account":{{"type":"chatgpt","email":"user@example.com","planType":"plus"}},"requiresOpenaiAuth":true}}}}'
printf '%s\n' '{}'
cat >/dev/null
"#,
                error.trim()
            ),
        )
        .unwrap();
        make_executable(&script);
        let (identity, snapshot) = probe_codex(&Account::system(Provider::Codex), &script)
            .await
            .unwrap();
        assert_eq!(identity.unwrap().plan.as_deref(), Some("plus"));
        assert_eq!(snapshot.status, UsageStatus::NeedsLogin);
        assert!(snapshot.windows.is_empty());
    }

    #[tokio::test]
    async fn a_cli_that_exits_without_answering_is_a_failure() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("codex");
        std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
        make_executable(&script);
        assert!(
            probe_codex(&Account::system(Provider::Codex), &script)
                .await
                .is_err()
        );
    }

    /// Signing a managed account out runs its CLI's own logout in its home,
    /// and never touches the system account.
    #[tokio::test]
    async fn logout_runs_the_cli_in_the_managed_home_only() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let record = directory.path().join("record");
        let script = bin.join("codex");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$CODEX_HOME $*\" > '{}'\n",
                record.display()
            ),
        )
        .unwrap();
        make_executable(&script);
        let home = directory.path().join("home");
        let account = Account::managed(Provider::Codex, uuid::Uuid::new_v4(), home.clone());
        let mut command = command(&account, &script);
        command.arg("logout");
        let mut group = Group::spawn(command).await.unwrap();
        group.finish().await;
        assert_eq!(
            std::fs::read_to_string(&record).unwrap().trim(),
            format!("{} logout", home.display())
        );
        assert!(logout(Account::system(Provider::Codex)).await.is_err());
    }

    /// A shell's startup files decide where a terminal finds the CLI, and
    /// may greet the reader first or leave a job running that holds the
    /// output open; a shell that never answers leaves the search to this
    /// process's own `PATH`, without holding anything up.
    #[tokio::test]
    async fn the_cli_is_looked_for_where_the_readers_shell_would_look() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("shell bin");
        std::fs::create_dir_all(&bin).unwrap();
        let cli = bin.join("codex");
        std::fs::write(&cli, "#!/bin/sh\n").unwrap();
        make_executable(&cli);
        let shell = directory.path().join("shell");
        let asked = directory.path().join("asked");
        std::fs::write(
            &shell,
            format!(
                "#!/bin/sh\n\
                 [ \"$1 $2\" = '-i -c' ] || exit 9\n\
                 [ -z \"${{DISPLAY-}}${{WAYLAND_DISPLAY-}}${{SSH_ASKPASS-}}\" ] || exit 8\n\
                 printf '%s' \"$3\" > '{}'\n\
                 printf 'Welcome back\\0__cagents_path__'\n\
                 sleep 30 &\n\
                 PATH='relative:{}' eval \"$3\"\n",
                asked.display(),
                bin.display()
            ),
        )
        .unwrap();
        make_executable(&shell);
        let path = shell_path(&shell, Duration::from_secs(5)).await.unwrap();
        assert_eq!(std::fs::read_to_string(&asked).unwrap(), SHELL_PATH_COMMAND);
        assert_eq!(path, OsString::from(format!("relative:{}", bin.display())));
        assert_eq!(find_executable("codex", Some(&path)), Some(cli));
        assert_eq!(find_executable("claude", Some(&path)), None);

        let silent = directory.path().join("silent");
        std::fs::write(&silent, "#!/bin/sh\nexec sleep 30\n").unwrap();
        make_executable(&silent);
        let timeout = Duration::from_millis(200);
        let started = std::time::Instant::now();
        assert_eq!(shell_path(&silent, timeout).await, None);
        assert!(
            started.elapsed() < timeout + EXIT_GRACE,
            "a shell out of time is not given a grace to exit"
        );
    }

    #[test]
    fn the_path_is_read_past_a_greeting() {
        assert_eq!(
            path_from_output(b"Hello\nthere, no newline__cagents_path__5:/a:/b\0"),
            Some(OsString::from("/a:/b"))
        );
        assert_eq!(
            path_from_output(b"a NUL\0and __cagents_path__ named__cagents_path__2:/c\0late"),
            Some(OsString::from("/c")),
            "a greeting's NUL or marker is not the value"
        );
        assert_eq!(
            path_from_output(b"__cagents_path__22:/x__cagents_path__2:/y\0"),
            Some(OsString::from("/x__cagents_path__2:/y")),
            "a PATH that holds the marker is read whole"
        );
        assert_eq!(path_from_output(b"__cagents_path__0:\0"), None);
        assert_eq!(path_from_output(b"/a:/b\0"), None, "no marker, no value");
        assert_eq!(
            path_from_output(b"__cagents_path__4:/d\0"),
            None,
            "a value of the wrong length is not taken"
        );
        assert_eq!(
            path_from_output(b"__cagents_path__4:/d/e"),
            None,
            "a value cut off is not taken"
        );
        assert_eq!(path_from_output(b"__cagents_path__+2:/f\0"), None);
        assert_eq!(path_from_output(b""), None);
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
