use super::*;
use crate::{
    account::AccountId,
    agent::Provider,
    agent_execution::{
        Agent, Authority, Control, Destination, Operation, Reply, Run, Spec, Submit, Summary,
        Surface, Target, directory::Scope,
    },
    model::{AgentId, RunId},
};

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// The terminal presentation of an already hosted execution agent.
    #[command(hide = true)]
    Console { agent_id: AgentId },
    /// Start a provider session on one prompt, accepted durably.
    ///
    /// The provider defaults to the calling session's, the directory to the
    /// current one, and the project to the open one containing it.
    Spawn {
        #[command(flatten)]
        prompt: PromptArgs,
        /// Unique lowercase handle among your open agents, such as reviewer.
        #[arg(long)]
        name: Option<String>,
        /// claude, codex, or omp. Defaults to the calling session's provider.
        #[arg(long, value_parser = parse_provider)]
        provider: Option<Provider>,
        /// The open project to work in, by any directory inside it.
        /// Defaults to the one containing the agent's directory.
        #[arg(long, visible_alias = "project")]
        repository: Option<PathBuf>,
        /// The agent's working directory. Defaults to the --project path when
        /// given, and otherwise the current directory.
        #[arg(long)]
        directory: Option<PathBuf>,
        /// Override the calling session's exact shared view with this channel.
        /// The default reuses its same-project worktree or channel view;
        /// missing or ambiguous membership creates a fresh channel with a warning.
        #[arg(long)]
        channel: Option<ChannelId>,
        #[arg(long)]
        account: Option<AccountId>,
        #[arg(long, default_value = "chat", value_parser = ["chat", "terminal"])]
        surface: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        permission_mode: Option<String>,
        /// The provider's effort name, such as low or high; omit for its default.
        #[arg(long)]
        reasoning_effort: Option<String>,
        #[command(flatten)]
        retry: RetryArgs,
        #[command(flatten)]
        wait: WaitArgs,
        #[command(flatten)]
        show: ShowArgs,
    },
    /// Send a follow-up prompt to an owned, settled agent.
    Send {
        /// The agent, by ID or a unique prefix of it.
        agent: String,
        #[command(flatten)]
        prompt: PromptArgs,
        #[command(flatten)]
        retry: RetryArgs,
        #[command(flatten)]
        wait: WaitArgs,
        #[command(flatten)]
        show: ShowArgs,
    },
    /// Read an owned agent and its latest run.
    Status {
        agent: String,
        #[command(flatten)]
        show: ShowArgs,
    },
    /// List this caller's open agents without their results.
    List,
    /// Wait for runs to settle or ask for input.
    ///
    /// Each handle is an agent, standing for its latest run, or a run, by ID
    /// or a unique prefix of it.
    Wait {
        #[arg(required = true, value_name = "AGENT_OR_RUN")]
        handles: Vec<String>,
        /// Return as soon as any of them is ready, rather than all.
        #[arg(long)]
        any: bool,
        #[arg(long, default_value = "60", value_parser = parse_positive_seconds)]
        timeout: f64,
        #[command(flatten)]
        show: ShowArgs,
    },
    /// List the questions and approvals an owned agent is waiting on.
    Requests { agent: String },
    /// Answer one of an owned agent's requests with one of its actions.
    Answer {
        agent: String,
        /// The request ID `requests` lists.
        request: String,
        /// One of the request's actions, such as allow or submit.
        action: String,
        /// A field's value; repeat for several fields or values.
        #[arg(long = "field", value_name = "FIELD=VALUE")]
        fields: Vec<String>,
    },
    /// Interrupt an owned agent's active run.
    Interrupt {
        agent: String,
        /// Permit the interrupt to stop the provider's running background subagents.
        #[arg(long)]
        including_subagents: bool,
    },
    /// Stop and close an agent, including its background subagents.
    Close {
        #[arg(
            required_unless_present = "interrupted",
            conflicts_with = "interrupted"
        )]
        agent: Option<String>,
        /// Close only interrupted agents owned by this caller's lineage.
        #[arg(long)]
        interrupted: bool,
        /// Include other callers; available only to the user, never session agents.
        #[arg(long, requires = "interrupted")]
        all: bool,
    },
}

/// A prompt given as an argument, in a file, or on standard input.
#[derive(Debug, clap::Args)]
pub struct PromptArgs {
    /// The prompt, which may begin with `-`; a lone `-` reads it from
    /// standard input.
    #[arg(value_name = "PROMPT", allow_hyphen_values = true)]
    pub(super) text: Option<String>,
    /// Read the prompt from this file.
    #[arg(long, value_name = "FILE", conflicts_with = "text")]
    pub(super) prompt_file: Option<PathBuf>,
    #[arg(long = "prompt", hide = true, conflicts_with_all = ["text", "prompt_file"])]
    pub(super) flag: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct RetryArgs {
    /// Retrying with the same key and request returns the original
    /// acceptance instead of starting the work again. Generated when omitted.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct WaitArgs {
    /// Wait for the run to settle or ask for input before returning.
    #[arg(long)]
    pub(super) wait: bool,
    /// How long `--wait` waits.
    #[arg(long, default_value = "60", value_parser = parse_positive_seconds, requires = "wait")]
    timeout: f64,
}

#[derive(Debug, Clone, Copy, clap::Args)]
pub struct ShowArgs {
    /// Print a result whole rather than its last lines.
    #[arg(long, conflicts_with = "tail")]
    full: bool,
    /// How many of a result's last lines to print.
    #[arg(long, default_value_t = 20)]
    tail: usize,
}

pub(super) fn parse_provider(value: &str) -> Result<Provider, String> {
    Provider::ALL
        .into_iter()
        .find(|provider| provider.executable() == value)
        .ok_or_else(|| "Choose omp, claude, or codex".into())
}

impl PromptArgs {
    fn read(self) -> Result<String, AppError> {
        if let Some(text) = self.flag {
            check_text_length(
                text.len(),
                "prompt",
                crate::agent_execution::MAX_PROMPT_BYTES,
            )?;
            return Ok(text);
        }
        read_text(
            self.text,
            self.prompt_file,
            "prompt",
            "Give a prompt, --prompt-file FILE, or - to read standard input",
            crate::agent_execution::MAX_PROMPT_BYTES,
        )
    }
}

/// Text given as an argument, in a file, or, for a lone `-`, on standard input.
pub(super) fn read_text(
    text: Option<String>,
    file: Option<PathBuf>,
    code: &'static str,
    missing: &'static str,
    limit: usize,
) -> Result<String, AppError> {
    match (text, file) {
        (Some(text), _) if text == "-" => read_bounded_text(io::stdin().lock(), code, limit),
        (Some(text), _) => {
            check_text_length(text.len(), code, limit)?;
            Ok(text)
        }
        (None, Some(path)) => {
            let file = std::fs::File::open(&path)
                .map_err(|error| AppError::new(code, format!("{}: {error}", path.display())))?;
            read_bounded_text(file, code, limit).map_err(|error| {
                AppError::new(code, format!("{}: {}", path.display(), error.message))
            })
        }
        // Standard input is read only when asked for: a caller whose input
        // is a pipe that never closes would otherwise wait forever on
        // forgotten text.
        (None, None) => Err(AppError::new(code, missing)),
    }
}

fn read_bounded_text(
    reader: impl Read,
    code: &'static str,
    limit: usize,
) -> Result<String, AppError> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::new(code, error))?;
    check_text_length(bytes.len(), code, limit)?;
    String::from_utf8(bytes).map_err(|error| AppError::new(code, error))
}

fn check_text_length(length: usize, code: &'static str, limit: usize) -> Result<(), AppError> {
    if length > limit {
        Err(AppError::new(
            code,
            format!("The {code} exceeds {limit} bytes"),
        ))
    } else {
        Ok(())
    }
}

impl RetryArgs {
    fn key(self) -> String {
        self.idempotency_key
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
    }
}

impl ShowArgs {
    /// The end of a result, as much as was asked for, with a line saying how
    /// much was left out.
    fn result(self, text: &str) -> String {
        let text = text.trim_end();
        let lines: Vec<&str> = text.lines().collect();
        if self.full || lines.len() <= self.tail {
            return text.to_owned();
        }
        let hidden = lines.len() - self.tail;
        format!(
            "… {hidden} earlier line{} (--full for all)\n{}",
            if hidden == 1 { "" } else { "s" },
            lines[hidden..].join("\n")
        )
    }
}

/// The directory a project named without a path works in: the checkout
/// around a Git repository's metadata, and a folder project itself.
fn checkout(project: &Path) -> PathBuf {
    match project.parent() {
        Some(parent) if project.file_name() == Some(std::ffi::OsStr::new(".git")) => {
            parent.to_owned()
        }
        _ => project.to_owned(),
    }
}

/// The part of an ID people type and read: unique among one caller's agents
/// in practice, and accepted back as a handle.
fn short(id: impl std::fmt::Display) -> String {
    id.to_string().chars().take(8).collect()
}

impl ControlClient {
    pub fn agent(
        &mut self,
        authority: Authority,
        operation: Operation,
    ) -> Result<Reply, ControlError> {
        self.agent_before(authority, operation, None)
    }

    pub(super) fn agent_before(
        &mut self,
        authority: Authority,
        operation: Operation,
        deadline: Option<Instant>,
    ) -> Result<Reply, ControlError> {
        let Operation::Inbox(query) = operation else {
            return self.agent_request_before(authority, operation, deadline);
        };
        let reply =
            self.agent_request_before(authority.clone(), Operation::PeekInbox(query), deadline)?;
        let Reply::Inbox(mut messages) = reply else {
            return Ok(reply);
        };
        use crate::agent_execution::mailbox::{MAX_PENDING, Receipt, State};
        let ids: Vec<_> = messages
            .iter()
            .filter(|message| message.state == State::Pending)
            .map(|message| message.id)
            .collect();
        for chunk in ids.chunks(MAX_PENDING) {
            let receipt = self.agent_request_before(
                authority.clone(),
                Operation::AcknowledgeInbox(Receipt {
                    ids: chunk.to_vec(),
                }),
                deadline,
            );
            match receipt {
                Ok(Reply::InboxAcknowledged) => {
                    for message in &mut messages {
                        if chunk.contains(&message.id) {
                            message.state = State::Read;
                        }
                    }
                }
                result => {
                    // The bodies have arrived. A failed receipt must allow
                    // redelivery rather than discard them. Retire this socket
                    // so a late receipt cannot become a later request's reply.
                    match result {
                        Err(error) => {
                            tracing::debug!(%error, "mailbox receipt not confirmed; messages may be redelivered")
                        }
                        Ok(_) => tracing::debug!(
                            "unexpected mailbox receipt response; messages may be redelivered"
                        ),
                    }
                    let _ = self.stream.shutdown(std::net::Shutdown::Both);
                    break;
                }
            }
        }
        Ok(Reply::Inbox(messages))
    }

    fn agent_request_before(
        &mut self,
        authority: Authority,
        operation: Operation,
        deadline: Option<Instant>,
    ) -> Result<Reply, ControlError> {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(FrameError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "agent request deadline elapsed",
            ))
            .into());
        }
        let closing = matches!(&operation, Operation::Close(_));
        let request = ClientRequest::Agent {
            authority,
            operation,
        };
        let interrupted_cleanup = matches!(&request, ClientRequest::Agent { operation: Operation::CloseInterrupted { .. }, .. });
        let response = if interrupted_cleanup {
            let budget = RETAINED_CHAT_CLOSE_TIMEOUT
                .saturating_add(CHAT_CLOSE_TIMEOUT)
                .saturating_add(Duration::from_secs(35))
                .saturating_mul(crate::agent_execution::INTERRUPTED_CLEANUP_BATCH as u32);
            let maximum = Instant::now() + budget;
            self.request_with_deadline(request, deadline.map_or(maximum, |requested| requested.min(maximum)))?
        } else if closing {
            self.request_with_deadline(
                request,
                Instant::now() + CHAT_CLOSE_TIMEOUT + Duration::from_secs(35),
            )?
        } else if let Some(deadline) = deadline {
            self.request_with_deadline(request, deadline)?
        } else {
            self.request_strict(request)?
        };
        match response {
            ServerResponse::Agent(reply) => Ok(reply),
            response => Err(unexpected_response("agent execution", &response)),
        }
    }
}

/// What a wait for runs saw before it returned.
#[derive(Debug, PartialEq)]
struct Waited {
    /// Each run's last known status.
    replies: Vec<Reply>,
    /// The deadline came first.
    timed_out: bool,
    /// Messages for the caller that ended the wait early.
    messages: Vec<crate::agent_execution::mailbox::Message>,
}

impl Waited {
    /// Why the wait returned, for a caller reading JSON: `runs`, `messages`
    /// or `timeout`. Only `runs` means the runs are ready.
    fn ended_by(&self) -> &'static str {
        if !self.messages.is_empty() {
            "messages"
        } else if self.timed_out {
            "timeout"
        } else {
            "runs"
        }
    }
}

impl ControlClient {
    /// Polls `runs` until all of them, or with `any` the first, settle or ask
    /// for input, or `timeout` passes. With `messages`, a message left for the
    /// caller also ends the wait, and is read: a session waiting here is busy,
    /// so no message would otherwise reach it until its runs finish.
    fn wait_agents(
        &mut self,
        authority: Authority,
        runs: &[RunId],
        any: bool,
        mut messages: bool,
        timeout: Duration,
    ) -> Result<Waited, ControlError> {
        let deadline = Instant::now() + timeout;
        let mut replies = Vec::with_capacity(runs.len());
        for run in runs {
            replies.push(self.agent_before(
                authority.clone(),
                Operation::Run(*run),
                Some(deadline),
            )?);
        }
        let ready =
            |reply: &Reply| matches!(reply, Reply::Status { run, .. } if run.state.wait_ready());
        let waited = |replies, timed_out, messages| {
            Ok(Waited {
                replies,
                timed_out,
                messages,
            })
        };
        loop {
            let done = if any {
                replies.iter().any(ready)
            } else {
                replies.iter().all(ready)
            };
            if done {
                return waited(replies, false, Vec::new());
            }
            if messages {
                let inbox = Operation::Inbox(crate::agent_execution::mailbox::Inbox {
                    all: false,
                    replies_to: None,
                });
                match self.agent_before(authority.clone(), inbox, Some(deadline)) {
                    Ok(Reply::Inbox(new)) if !new.is_empty() => {
                        return waited(replies, false, new);
                    }
                    Ok(_) => {}
                    Err(error) if is_timeout_error(&error) => {
                        return waited(replies, true, Vec::new());
                    }
                    // Messages are a courtesy here; the runs are the wait.
                    Err(error) => {
                        tracing::debug!(%error, "cannot read messages while waiting");
                        messages = false;
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return waited(replies, true, Vec::new());
            }
            thread::sleep(Duration::from_millis(200).min(remaining));
            if Instant::now() >= deadline {
                return waited(replies, true, Vec::new());
            }
            // A waiting run may be answered and resume, so only a finished
            // one is known for good.
            for (run, reply) in runs.iter().zip(replies.iter_mut()) {
                if matches!(reply, Reply::Status { run, .. } if run.state.is_terminal()) {
                    continue;
                }
                match self.agent_before(authority.clone(), Operation::Run(*run), Some(deadline)) {
                    Ok(current) => *reply = current,
                    Err(error) if is_timeout_error(&error) => {
                        return waited(replies, true, Vec::new());
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

fn spawn_placement(
    explicit: Option<ChannelId>,
    caller: Option<&crate::agent_execution::directory::Entry>,
    repository: &std::path::Path,
) -> (
    Option<crate::agent_execution::Placement>,
    Option<&'static str>,
) {
    use crate::agent_execution::Placement;
    if let Some(channel) = explicit {
        return (Some(Placement::Channel(channel)), None);
    }
    let Some(caller) = caller else {
        return (None, None);
    };
    let Some(scope) = &caller.view_scope else {
        return (
            None,
            Some(
                "The caller has no unambiguous shared view; the agent was placed in a new channel.",
            ),
        );
    };
    let owner = match scope {
        Placement::Channel(_) => caller.channel_repository.as_deref(),
        Placement::Worktree { repository, .. } => Some(repository.as_path()),
    };
    if owner == Some(repository) {
        (Some(scope.clone()), None)
    } else {
        (
            None,
            Some(
                "The caller's shared view belongs to another project; the agent was placed in a new channel for the target project.",
            ),
        )
    }
}

fn with_placement_warning(mut output: CommandOutput, warning: Option<&str>) -> CommandOutput {
    if let Some(warning) = warning {
        // Keep the receipt first and the warning outside the agent's result.
        output.human = match output.human.split_once('\n') {
            Some((head, tail)) => format!("{head}\nWarning: {warning}\n{tail}"),
            None => format!("{}\nWarning: {warning}", output.human),
        };
        output.data["warnings"] = json!([warning]);
    }
    output
}

/// What a handle names among the caller's agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handle {
    Agent(AgentId),
    Run(RunId),
}

/// Resolves a typed handle against the caller's agents and their latest
/// runs: an exact name, a whole ID, or a prefix that names exactly one of
/// them. `None` for
/// a whole ID that names none of them, which the caller may still know as an
/// older run or a closed agent.
fn resolve(summaries: &[Summary], handle: &str) -> Result<Option<Handle>, AppError> {
    let wanted = handle.trim().to_ascii_lowercase();
    if wanted.is_empty() {
        return Err(AppError::new("handle", "An agent or run handle is empty"));
    }
    if let Some(summary) = summaries
        .iter()
        .find(|summary| summary.agent.name.as_deref() == Some(&wanted))
    {
        return Ok(Some(Handle::Agent(summary.agent.id)));
    }
    let mut matches: Vec<(Handle, String)> = Vec::new();
    for summary in summaries {
        for (candidate, text) in [
            (
                Handle::Agent(summary.agent.id),
                summary.agent.id.to_string(),
            ),
            (Handle::Run(summary.run), summary.run.to_string()),
        ] {
            if text == wanted {
                return Ok(Some(candidate));
            }
            if text.starts_with(&wanted) && !matches.iter().any(|(known, _)| *known == candidate) {
                matches.push((candidate, text));
            }
        }
    }
    match matches.as_slice() {
        [(handle, _)] => Ok(Some(*handle)),
        [] => match uuid::Uuid::parse_str(&wanted) {
            Ok(_) => Ok(None),
            Err(_) => Err(AppError::new(
                "handle",
                format!("No agent or run of yours starts with {handle}; see agent list"),
            )),
        },
        candidates => Err(AppError::new(
            "handle",
            format!(
                "{handle} matches {}; type more of the ID",
                candidates
                    .iter()
                    .map(|(handle, text)| match handle {
                        Handle::Agent(_) => format!("agent {}", &text[..12]),
                        Handle::Run(_) => format!("run {}", &text[..12]),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// The caller's authority and, once a command first needs the daemon, a
/// connection to it and the caller's agents.
struct Session {
    authority: Authority,
    connection: Option<(ControlClient, PathBuf)>,
    summaries: Option<Vec<Summary>>,
}

impl Session {
    fn new() -> Self {
        Self {
            authority: Authority::from_environment(),
            connection: None,
            summaries: None,
        }
    }

    /// Connects on first use, starting the daemon if it is not running.
    fn client(&mut self) -> Result<&mut ControlClient, AppError> {
        if self.connection.is_none() {
            let socket =
                terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e))?;
            let client = ControlClient::connect(&socket)
                .map_err(|e| AppError::new("daemon_connection", e))?;
            self.connection = Some((client, socket));
        }
        Ok(&mut self.connection.as_mut().expect("connected above").0)
    }

    fn call(&mut self, operation: Operation) -> Result<Reply, AppError> {
        let authority = self.authority.clone();
        self.client()?
            .agent(authority, operation)
            .map_err(protocol_error)
    }

    fn close_interrupted(&mut self, all: bool) -> Result<CommandOutput, AppError> {
        let mut closed = Vec::new();
        let mut failed = Vec::new();
        let mut after = None;
        loop {
            let reply = match self.call(Operation::CloseInterrupted { all, after }) {
                Ok(reply) => reply,
                Err(mut error) => {
                    let mut details = match error.details.take() {
                        Some(Value::Object(details)) => details,
                        _ => serde_json::Map::new(),
                    };
                    details.insert("closed".into(), json!(closed));
                    details.insert("failed".into(), json!(failed));
                    details.insert("attempts_complete".into(), json!(false));
                    error.details = Some(Value::Object(details));
                    error.message = format!("Closed {} interrupted agents; remaining cleanup was not confirmed: {}", closed.len(), error.message);
                    return Err(error);
                }
            };
            let Reply::InterruptedClosed(result) = reply else {
                return Err(AppError { kind: "agent.close", message: "Unexpected interrupted close response".into(), details: Some(json!({"closed":closed,"failed":failed,"attempts_complete":false})) });
            };
            closed.extend(result.closed);
            failed.extend(result.failed);
            let Some(next) = result.next else { break; };
            if after.is_some_and(|previous| next <= previous) {
                return Err(AppError { kind: "protocol", message: "Interrupted cleanup did not advance its cursor".into(), details: Some(json!({"closed":closed,"failed":failed,"attempts_complete":false})) });
            }
            after = Some(next);
        }
        let mut human = format!("Closed {} interrupted agents", closed.len());
        for failure in &failed { human.push_str(&format!("\n{}: {}", failure.id, failure.message)); }
        let data = json!({"closed":closed,"failed":failed,"attempts_complete":true});
        if !failed.is_empty() { return Err(AppError { kind: "agent.close", message: human, details: Some(data) }); }
        Ok(CommandOutput { command: "agent.close", human, data })
    }

    /// Waits for `runs`; an agent's wait also ends when a message arrives
    /// for it. The user's shell has `cagents inbox` and is never busy.
    fn wait(&mut self, runs: &[RunId], any: bool, timeout: f64) -> Result<Waited, AppError> {
        let authority = self.authority.clone();
        let messages = authority.token().is_some();
        self.client()?
            .wait_agents(
                authority,
                runs,
                any,
                messages,
                Duration::from_secs_f64(timeout),
            )
            .map_err(protocol_error)
    }

    fn summaries(&mut self) -> Result<&[Summary], AppError> {
        if self.summaries.is_none() {
            let Reply::Listed(summaries) = self.call(Operation::List)? else {
                return Err(AppError::new("agent.list", "Unexpected list response"));
            };
            self.summaries = Some(summaries);
        }
        Ok(self.summaries.as_deref().unwrap_or_default())
    }

    /// An agent handle, or the agent of a run handle. A whole ID outside the
    /// caller's open agents is taken as an agent, for the daemon to judge.
    fn agent(&mut self, handle: &str) -> Result<AgentId, AppError> {
        match resolve(self.summaries()?, handle)? {
            None => handle
                .trim()
                .parse()
                .map_err(|e| AppError::new("handle", format!("{handle}: {e}"))),
            Some(Handle::Agent(agent)) => Ok(agent),
            Some(Handle::Run(run)) => self
                .summaries()?
                .iter()
                .find(|summary| summary.run == run)
                .map(|summary| summary.agent.id)
                .ok_or_else(|| {
                    AppError::new("handle", format!("{handle} is not one of your agents"))
                }),
        }
    }

    /// The latest run of an agent handle, or the run a run handle names. A
    /// whole ID outside the caller's open agents is taken as an older run.
    fn run(&mut self, handle: &str) -> Result<RunId, AppError> {
        match resolve(self.summaries()?, handle)? {
            None => handle
                .trim()
                .parse()
                .map_err(|e| AppError::new("handle", format!("{handle}: {e}"))),
            Some(Handle::Run(run)) => Ok(run),
            Some(Handle::Agent(agent)) => self
                .summaries()?
                .iter()
                .find(|summary| summary.agent.id == agent)
                .map(|summary| summary.run)
                .ok_or_else(|| {
                    AppError::new("handle", format!("{handle} is not one of your agents"))
                }),
        }
    }

    /// Submits once, and once more with the same key if the daemon's answer
    /// was lost: the key makes the retry return the first acceptance rather
    /// than start the work twice. Says whether it retried.
    fn submit(&mut self, request: Submit) -> Result<(Reply, bool), AppError> {
        let authority = self.authority.clone();
        match self
            .client()?
            .agent(authority, Operation::Submit(request.clone()))
        {
            Ok(reply) => Ok((reply, false)),
            Err(
                ControlError::Frame(_) | ControlError::ConnectionClosed | ControlError::Connect(_),
            ) => {
                // A new connection, to the daemon that is running now.
                self.connection = None;
                Ok((self.call(Operation::Submit(request))?, true))
            }
            Err(error) => Err(protocol_error(error)),
        }
    }

    /// The open project `scope` names and the calling session,
    /// which a spawned agent defaults to, plus its channel membership.
    fn context(
        &mut self,
        scope: Scope,
    ) -> Result<
        (
            Option<PathBuf>,
            Option<crate::agent_execution::directory::Entry>,
        ),
        AppError,
    > {
        let Reply::Directory(directory) = self.call(Operation::Directory(scope))? else {
            return Err(AppError::new("sessions", "Unexpected directory response"));
        };
        let project = match directory.scope {
            Scope::Project(project) => Some(project),
            _ => None,
        };
        Ok((project, directory.caller))
    }
}

pub(super) fn execute(command: AgentCommand) -> Result<CommandOutput, AppError> {
    if let AgentCommand::Console { agent_id } = command {
        return run_console(agent_id);
    }
    let mut session = Session::new();
    match command {
        AgentCommand::Console { .. } => unreachable!("handled console presentation"),
        AgentCommand::Spawn {
            prompt,
            name,
            provider,
            repository,
            directory,
            channel,
            account,
            surface,
            model,
            permission_mode,
            reasoning_effort,
            retry,
            wait,
            show,
        } => {
            let prompt = prompt.read()?;
            let absolute =
                |path: PathBuf| std::path::absolute(path).map_err(|e| AppError::new("target", e));
            let given = if crate::device::is_remote() {
                directory
                    .map(|directory| {
                        if directory.is_absolute() {
                            Ok(directory)
                        } else {
                            Err(AppError::new(
                                "target",
                                "remote working directory must be absolute",
                            ))
                        }
                    })
                    .transpose()?
            } else {
                directory.map(absolute).transpose()?
            };
            let here = match &given {
                Some(directory) => directory.clone(),
                None => {
                    if crate::device::is_remote() {
                        crate::device::home()
                            .cloned()
                            .ok_or_else(|| AppError::new("target", "remote HOME is unavailable"))?
                    } else {
                        std::env::current_dir().map_err(|e| AppError::new("target", e))?
                    }
                }
            };
            let (containing, caller) = session.context(Scope::Containing(here.clone()))?;
            // Keep explicit launch paths byte-for-byte for durable retry, but
            // compare channel ownership using the daemon's resolved project.
            let mut channel_repository = containing.clone();
            let (repository, directory) = match repository {
                None => {
                    let repository = containing.ok_or_else(|| {
                        AppError::new(
                            "target",
                            format!(
                                "No open project contains {}; open it in cagents or pass --project",
                                here.display()
                            ),
                        )
                    })?;
                    (repository, here)
                }
                Some(project) => {
                    let project = super::project_argument(project)?;
                    let named =
                        session
                            .context(Scope::Project(project.clone()))
                            .and_then(|(named, _)| {
                                named.ok_or_else(|| {
                                    AppError::new(
                                        "target",
                                        format!("No open project is {}", project.display()),
                                    )
                                })
                            });
                    channel_repository = named.as_ref().ok().cloned();
                    match given {
                        // A target given in full is sent as given, so retrying
                        // it stays exact even after its directory has moved;
                        // only a directory known to be elsewhere is refused.
                        Some(_) => {
                            if let (Ok(named), Some(containing)) = (&named, &containing)
                                && named != containing
                            {
                                return Err(AppError::new(
                                    "target",
                                    format!(
                                        "{} is not in the project {}",
                                        here.display(),
                                        named.display()
                                    ),
                                ));
                            }
                            let repository = if project.is_absolute() {
                                project
                            } else {
                                named?
                            };
                            (repository, here)
                        }
                        // An agent sent to the project this one is working in
                        // works here, as in a worktree; one sent elsewhere works
                        // at the path given for it.
                        None => {
                            let named = named?;
                            if containing.as_ref() == Some(&named) {
                                (named, here)
                            } else {
                                let directory = if project.is_absolute() {
                                    project
                                } else {
                                    checkout(&named)
                                };
                                (named, directory)
                            }
                        }
                    }
                }
            };
            let (placement, placement_warning) = spawn_placement(
                channel,
                caller.as_ref(),
                channel_repository.as_deref().unwrap_or(&repository),
            );
            let provider = provider
                .or(caller.and_then(|caller| caller.provider))
                .ok_or_else(|| {
                    AppError::new("agent_request", "Choose --provider claude, codex, or omp")
                })?;
            let request = Submit {
                name,
                destination: Destination::New(Spec {
                    provider,
                    account,
                    target: Target {
                        repository,
                        directory,
                        placement,
                    },
                    surface: if surface == "terminal" {
                        Surface::Terminal
                    } else {
                        Surface::Chat
                    },
                    model,
                    permission_mode,
                    reasoning_effort,
                }),
                prompt,
                idempotency_key: retry.key(),
            };
            let output = submitted(&mut session, "agent.spawn", request, wait, show)?;
            Ok(with_placement_warning(output, placement_warning))
        }
        AgentCommand::Send {
            agent,
            prompt,
            retry,
            wait,
            show,
        } => {
            let prompt = prompt.read()?;
            let request = Submit {
                name: None,
                destination: Destination::Existing(session.agent(&agent)?),
                prompt,
                idempotency_key: retry.key(),
            };
            submitted(&mut session, "agent.send", request, wait, show)
        }
        AgentCommand::Status { agent, show } => {
            let agent = session.agent(&agent)?;
            let reply = session.call(Operation::Status(agent))?;
            let (agent, run) = status_reply(reply)?;
            let human = status_lines(&agent, &run, false, show);
            Ok(CommandOutput {
                command: "agent.status",
                data: json!({"agent": agent, "run": run, "timed_out": false}),
                human,
            })
        }
        AgentCommand::List => {
            let summaries = session.summaries()?.to_vec();
            let human = if summaries.is_empty() {
                "No open agents".to_owned()
            } else {
                summaries
                    .iter()
                    .map(|summary| {
                        format!(
                            "{}  {:<6} {:<11} run {}  {}{}",
                            short(summary.agent.id),
                            summary.agent.spec.provider.executable(),
                            format!("{:?}", summary.state),
                            short(summary.run),
                            summary.agent.spec.target.directory.display(),
                            summary
                                .agent
                                .name
                                .as_ref()
                                .map(|name| format!("  {name}"))
                                .unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(CommandOutput {
                command: "agent.list",
                data: json!({"agents": summaries}),
                human,
            })
        }
        AgentCommand::Wait {
            handles,
            any,
            timeout,
            show,
        } => {
            let runs = handles
                .iter()
                .map(|handle| session.run(handle))
                .collect::<Result<Vec<_>, _>>()?;
            let waited = session.wait(&runs, any, timeout)?;
            let ended_by = waited.ended_by();
            let Waited {
                replies,
                timed_out,
                messages,
            } = waited;
            let statuses = replies
                .into_iter()
                .map(status_reply)
                .collect::<Result<Vec<_>, _>>()?;
            let mut human = statuses
                .iter()
                .map(|(agent, run)| {
                    status_lines(agent, run, timed_out && !run.state.wait_ready(), show)
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            human.push_str(&arrived_lines(&messages));
            let mut data = match statuses.as_slice() {
                [(agent, run)] => json!({
                    "agent": agent,
                    "run": run,
                    "timed_out": timed_out,
                    "ended_by": ended_by,
                }),
                _ => json!({
                    "results": statuses
                        .iter()
                        .map(|(agent, run)| json!({"agent": agent, "run": run}))
                        .collect::<Vec<_>>(),
                    "timed_out": timed_out,
                    "ended_by": ended_by,
                }),
            };
            if !messages.is_empty() {
                data["messages"] = json!(messages);
            }
            Ok(CommandOutput {
                command: "agent.wait",
                data,
                human,
            })
        }
        AgentCommand::Requests { agent } => {
            let agent = session.agent(&agent)?;
            let requests = requests_reply(session.call(Operation::Requests(agent))?)?;
            Ok(CommandOutput {
                command: "agent.requests",
                human: requests_lines(agent, &requests),
                data: json!({"agent": agent, "requests": requests}),
            })
        }
        AgentCommand::Answer {
            agent,
            request,
            action,
            fields,
        } => {
            let agent = session.agent(&agent)?;
            let answers = answers(&fields)?;
            let requests = requests_reply(session.call(Operation::Control {
                agent,
                command: Control::Answer {
                    request_id: request.clone(),
                    action: action.clone(),
                    answers,
                },
            })?)?;
            let mut human = format!("Answered {request} with {action}");
            if !requests.is_empty() {
                human.push_str("\n\n");
                human.push_str(&requests_lines(agent, &requests));
            }
            Ok(CommandOutput {
                command: "agent.answer",
                data: json!({"agent": agent, "requests": requests}),
                human,
            })
        }
        AgentCommand::Interrupt {
            agent,
            including_subagents,
        } => {
            let agent = session.agent(&agent)?;
            let requests = requests_reply(session.call(Operation::Control {
                agent,
                command: Control::Interrupt {
                    including_subagents,
                },
            })?)?;
            Ok(CommandOutput {
                command: "agent.interrupt",
                data: json!({"agent": agent, "requests": requests, "including_subagents": including_subagents}),
                human: format!(
                    "Interrupted agent {}{}",
                    short(agent),
                    if including_subagents {
                        " including its background subagents"
                    } else {
                        ""
                    }
                ),
            })
        }
        AgentCommand::Close {
            agent,
            interrupted,
            all,
        } => {
            if interrupted {
                return session.close_interrupted(all);
            }
            let agent = session.agent(agent.as_deref().ok_or_else(|| {
                AppError::new("agent.close", "Specify an agent or --interrupted")
            })?)?;
            let Reply::Closed(agent) = session.call(Operation::Close(agent))? else {
                return Err(AppError::new("agent.close", "Unexpected close response"));
            };
            Ok(CommandOutput {
                command: "agent.close",
                human: format!("Closed agent {}", short(agent.id)),
                data: json!({"agent": agent}),
            })
        }
    }
}

/// Reports an acceptance, after waiting on its run when asked to.
fn submitted(
    session: &mut Session,
    command: &'static str,
    request: Submit,
    wait: WaitArgs,
    show: ShowArgs,
) -> Result<CommandOutput, AppError> {
    request
        .validate()
        .map_err(|e| AppError::new("agent_request", e))?;
    let key = request.idempotency_key.clone();
    // Whether a failed submission was accepted is unknown, so the key that
    // makes retrying it safe goes with the error.
    let (reply, retried) = session.submit(request).map_err(|mut error| {
        error.message = format!(
            "{}; retry with --idempotency-key {key} to avoid starting the work twice",
            error.message
        );
        let mut details = match error.details.take() {
            Some(Value::Object(details)) => details,
            _ => serde_json::Map::new(),
        };
        details.insert("idempotency_key".into(), json!(key));
        error.details = Some(Value::Object(details));
        error
    })?;
    let Reply::Accepted(accepted) = reply else {
        return Err(AppError::new(command, "Unexpected submit response"));
    };
    let mut human = format!(
        "{} agent {} run {}",
        // The retry finds the work this invocation's lost attempt accepted.
        if accepted.created || retried {
            "Accepted"
        } else {
            "Already accepted"
        },
        short(accepted.agent.id),
        short(accepted.run.id)
    );
    let mut data = json!({
        "agent": accepted.agent,
        "run": accepted.run,
        "created": accepted.created,
        "idempotency_key": key,
    });
    if wait.wait {
        let waited = session.wait(&[accepted.run.id], false, wait.timeout)?;
        data["ended_by"] = json!(waited.ended_by());
        let Waited {
            mut replies,
            timed_out,
            messages,
        } = waited;
        let (agent, run) = status_reply(replies.remove(0))?;
        human = status_lines(&agent, &run, timed_out, show);
        human.push_str(&arrived_lines(&messages));
        data["agent"] = json!(agent);
        data["run"] = json!(run);
        data["timed_out"] = json!(timed_out);
        if !messages.is_empty() {
            data["messages"] = json!(messages);
        }
    }
    Ok(CommandOutput {
        command,
        data,
        human,
    })
}

fn status_reply(reply: Reply) -> Result<(Agent, Run), AppError> {
    match reply {
        Reply::Status { agent, run } => Ok((agent, run)),
        _ => Err(AppError::new("agent.status", "Unexpected status response")),
    }
}

fn requests_reply(reply: Reply) -> Result<Vec<crate::chat::Request>, AppError> {
    match reply {
        Reply::Requests(requests) => Ok(requests),
        _ => Err(AppError::new(
            "agent.requests",
            "Unexpected requests response",
        )),
    }
}

/// The heading of the messages that ended a wait early.
pub(super) const ARRIVED: &str = "Messages arrived while waiting:";

/// The messages that ended a wait early, and how to go on.
fn arrived_lines(messages: &[crate::agent_execution::mailbox::Message]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let mut lines = vec![format!("\n\n{ARRIVED}")];
    lines.extend(messages.iter().map(super::messages::message_lines));
    lines.push(
        "Answer one with: cagents reply <id> \"…\"; then wait again with cagents agent wait".into(),
    );
    lines.join("\n\n")
}

/// An agent's latest run in a few lines: who and where, how the run stands,
/// and the end of its result.
fn status_lines(agent: &Agent, run: &Run, timed_out: bool, show: ShowArgs) -> String {
    let mut lines = format!(
        "agent {} ({}) run {} {:?}{}",
        short(agent.id),
        agent.spec.provider.executable(),
        short(run.id),
        run.state,
        if timed_out { " (wait timed out)" } else { "" }
    );
    if run.state == crate::agent_execution::State::Waiting {
        lines.push_str(&format!(
            "\nWaiting for input: cagents agent requests {}",
            short(agent.id)
        ));
    }
    if let Some(diagnostic) = &run.result.diagnostic {
        lines.push('\n');
        lines.push_str(diagnostic);
    }
    if !run.result.text.trim().is_empty() {
        lines.push_str("\n\n");
        lines.push_str(&show.result(&run.result.text));
        if run.result.truncated {
            lines.push_str("\n(result truncated by the daemon)");
        }
    }
    lines
}

/// The run in output [`submitted`] printed: the acceptance line, or the
/// [`status_lines`] that replace it when the command waited.
#[cfg(feature = "ui")]
pub(super) fn read_run(output: &str) -> Option<crate::chat::coordination::Run> {
    let head = output.lines().next()?;
    if let Some(accepted) = head
        .strip_prefix("Accepted agent ")
        .or_else(|| head.strip_prefix("Already accepted agent "))
    {
        let (agent, _) = accepted.split_once(" run ")?;
        return Some(crate::chat::coordination::Run {
            agent: agent.into(),
            state: None,
            timed_out: false,
            result: String::new(),
            ..Default::default()
        });
    }
    let (agent, rest_of_head) = head.strip_prefix("agent ")?.split_once(' ')?;
    let (provider, after_run) = rest_of_head.split_once(" run ")?;
    let provider = provider.strip_prefix('(')?.strip_suffix(')')?;
    let (_, state) = after_run.split_once(' ')?;
    let (state, timed_out) = match state.strip_suffix(" (wait timed out)") {
        Some(state) => (state, true),
        None => (state, false),
    };
    use crate::agent_execution::State;
    let state = [
        State::Accepted,
        State::Running,
        State::Waiting,
        State::Succeeded,
        State::Failed,
        State::Cancelled,
        State::Interrupted,
    ]
    .into_iter()
    .find(|candidate| format!("{candidate:?}") == state)?;
    // The result follows the first blank line, up to any messages that
    // arrived during the wait.
    let status = output
        .split_once(ARRIVED)
        .map_or(output, |(status, _)| status);
    let result = status.split_once("\n\n").map_or("", |(_, result)| result);
    Some(crate::chat::coordination::Run {
        agent: agent.into(),
        provider: provider.into(),
        state: Some(state),
        timed_out,
        result: result.trim().into(),
        ..Default::default()
    })
}

fn requests_lines(agent: AgentId, requests: &[crate::chat::Request]) -> String {
    if requests.is_empty() {
        return format!("Agent {} is not waiting on anything", short(agent));
    }
    let mut lines = Vec::new();
    for request in requests {
        lines.push(format!("request {}: {}", request.id, request.title));
        if !request.description.trim().is_empty() {
            lines.push(format!("  {}", request.description.trim()));
        }
        let actions: Vec<_> = request.actions.iter().map(|a| a.id.as_str()).collect();
        lines.push(format!("  actions: {}", actions.join(", ")));
        for field in &request.fields {
            let mut line = format!("  --field {}=…  {}", field.id, field.label);
            if !field.choices.is_empty() {
                let choices: Vec<_> = field.choices.iter().map(|c| c.id.as_str()).collect();
                line.push_str(&format!(" (one of {}", choices.join(", ")));
                line.push_str(if field.multiple { "; repeatable)" } else { ")" });
            }
            lines.push(line);
        }
    }
    lines.push(format!(
        "Answer with: cagents agent answer {} REQUEST ACTION [--field FIELD=VALUE]",
        short(agent)
    ));
    lines.join("\n")
}

/// `--field` values grouped by field, each in the order given.
fn answers(fields: &[String]) -> Result<Vec<crate::chat::Answer>, AppError> {
    let mut answers: Vec<crate::chat::Answer> = Vec::new();
    for field in fields {
        let (id, value) = field
            .split_once('=')
            .ok_or_else(|| AppError::new("agent.answer", format!("{field} is not FIELD=VALUE")))?;
        match answers.iter_mut().find(|answer| answer.field == id) {
            Some(answer) => answer.values.push(value.into()),
            None => answers.push(crate::chat::Answer {
                field: id.into(),
                values: vec![value.into()],
            }),
        }
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    #[test]
    fn interrupted_cleanup_cli_continues_bounded_pages_and_preserves_confirmed_progress() {
        for lose_final_reply in [false, true] {
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            let closed = AgentId::new();
            let server = thread::spawn(move || {
                answer(&mut server_stream, |_| true, connected());
                answer(&mut server_stream, |request| matches!(request, ClientRequest::Agent { operation: Operation::CloseInterrupted { all: false, after: None }, .. }),
                    ServerResponse::Agent(Reply::InterruptedClosed(crate::agent_execution::CloseInterrupted { closed: vec![closed], failed: vec![], next: Some(closed) })));
                let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert!(matches!(request.request, ClientRequest::Agent { operation: Operation::CloseInterrupted { all: false, after: Some(cursor) }, .. } if cursor == closed));
                if !lose_final_reply {
                    write_frame(&mut server_stream, &ServerEnvelope { version: PROTOCOL_VERSION, request_id: Some(request.request_id), proof: proof(), message: ServerMessage::Response { response: ServerResponse::Agent(Reply::InterruptedClosed(crate::agent_execution::CloseInterrupted { closed: vec![], failed: vec![], next: None })) } }).unwrap();
                }
            });
            let client = ControlClient::from_stream(client_stream).unwrap();
            let mut session = Session { authority: Authority::default(), connection: Some((client, PathBuf::from("/fixture"))), summaries: None };
            let result = session.close_interrupted(false);
            if lose_final_reply {
                let error = result.err().unwrap();
                let details = error.details.unwrap();
                assert_eq!(details["closed"][0], closed.to_string());
                assert_eq!(details["attempts_complete"], false);
            } else {
                let output = result.unwrap();
                assert_eq!(output.data["closed"][0], closed.to_string());
                assert_eq!(output.data["attempts_complete"], true);
            }
            drop(session);
            server.join().unwrap();
        }
    }

    #[test]
    fn interrupted_cleanup_cli_sends_exact_scope_and_reports_partial_failures() {
        for all in [false, true] {
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            let Reply::Status { agent, .. } = running().1 else {
                unreachable!()
            };
            let closed_id = agent.id;
            let failed_id = AgentId::new();
            let server = thread::spawn(move || {
                answer(&mut server_stream, |_| true, connected());
                answer(
                    &mut server_stream,
                    |request| matches!(request, ClientRequest::Agent { operation: Operation::CloseInterrupted { all: actual, after: None }, .. } if *actual == all),
                    ServerResponse::Agent(Reply::InterruptedClosed(
                        crate::agent_execution::CloseInterrupted {
                            closed: vec![agent.id],
                            next: None,
                            failed: if all {
                                vec![crate::agent_execution::CloseFailure {
                                    id: failed_id,
                                    message: "Exact provider teardown is unconfirmed".into(),
                                }]
                            } else {
                                vec![]
                            },
                        },
                    )),
                );
            });
            let client = ControlClient::from_stream(client_stream).unwrap();
            let mut session = Session {
                authority: Authority::default(),
                connection: Some((client, PathBuf::from("/fixture"))),
                summaries: None,
            };
            if all {
                let error = session.close_interrupted(all).err().unwrap();
                assert!(error.message.contains(&failed_id.to_string()));
                assert_eq!(
                    error.details.as_ref().unwrap()["closed"][0],
                    closed_id.to_string()
                );
                assert_eq!(
                    error.details.as_ref().unwrap()["failed"][0]["id"],
                    failed_id.to_string()
                );
            } else {
                let result = session.close_interrupted(all).unwrap();
                assert_eq!(result.data["closed"][0], closed_id.to_string());
                assert!(result.data["failed"].as_array().unwrap().is_empty());
            }
            drop(session);
            server.join().unwrap();
        }
    }

    #[test]
    fn interrupted_close_is_explicit_and_defaults_to_caller_scope() {
        assert!(matches!(
            parse(&["close", "--interrupted"]),
            AgentCommand::Close {
                agent: None,
                interrupted: true,
                all: false
            }
        ));
        assert!(matches!(
            parse(&["close", "--interrupted", "--all"]),
            AgentCommand::Close {
                agent: None,
                interrupted: true,
                all: true
            }
        ));
        assert!(matches!(
            parse(&["close", "1234"]),
            AgentCommand::Close {
                agent: Some(_),
                interrupted: false,
                all: false
            }
        ));
        for args in [
            vec!["cagents", "agent", "close"],
            vec!["cagents", "agent", "close", "1234", "--interrupted"],
            vec!["cagents", "agent", "close", "1234", "--all"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
    use super::*;

    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("agent-wait"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("incarnation"),
        }
    }

    fn running() -> (RunId, Reply) {
        use crate::agent_execution::{Agent, Origin, Run, State};
        let agent = Agent {
            name: None,
            id: AgentId::new(),
            origin: Origin::User,
            placement: crate::agent_execution::Placement::Channel(crate::model::ChannelId::new()),
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: "/repo".into(),
                    directory: "/repo".into(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            session: None,
            console: None,
            created_ms: 0,
            closed: false,
        };
        let run = Run {
            id: RunId::new(),
            agent: agent.id,
            state: State::Running,
            revision: 1,
            accepted_ms: 0,
            started_ms: Some(1),
            finished_ms: None,
            provider_key: Some("exact-turn".into()),
            result: Default::default(),
        };
        (run.id, Reply::Status { agent, run })
    }

    /// Answers the next request, which `expected` must accept.
    fn answer(
        stream: &mut UnixStream,
        expected: impl FnOnce(&ClientRequest) -> bool,
        response: ServerResponse,
    ) {
        let request: ClientEnvelope = read_frame(stream).unwrap().unwrap();
        assert!(expected(&request.request), "{:?}", request.request);
        write_frame(
            stream,
            &ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: Some(request.request_id),
                proof: proof(),
                message: ServerMessage::Response { response },
            },
        )
        .unwrap();
    }

    fn connected() -> ServerResponse {
        ServerResponse::Connected {
            proof: proof(),
            resumed: Vec::new(),
        }
    }

    fn inbox_message(
        state: crate::agent_execution::mailbox::State,
    ) -> crate::agent_execution::mailbox::Message {
        use crate::agent_execution::mailbox::{Message, Party};
        Message {
            id: crate::model::MessageId::new(),
            from: Party::user(),
            to: Party {
                lineage: None,
                session: Some(crate::model::TerminalId::new()),
                title: None,
            },
            reply_to: None,
            body: "Received body and recipient session must survive receipt handling".into(),
            wait_until_ms: None,
            state,
            created_ms: 1,
        }
    }

    #[test]
    fn inbox_receipts_chunk_pending_ids_and_preserve_historical_state_and_session() {
        use crate::agent_execution::mailbox::{Inbox, MAX_PENDING, State};
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let mut messages: Vec<_> = (0..MAX_PENDING + 6)
            .map(|_| inbox_message(State::Pending))
            .collect();
        messages.insert(3, inbox_message(State::Read));
        messages.insert(9, inbox_message(State::Delivered));
        let ids: Vec<_> = messages
            .iter()
            .filter(|message| message.state == State::Pending)
            .map(|message| message.id)
            .collect();
        let mut expected = messages.clone();
        for message in &mut expected {
            if message.state == State::Pending {
                message.state = State::Read;
            }
        }
        let server = thread::spawn(move || {
            answer(&mut server_stream, |_| true, connected());
            answer(
                &mut server_stream,
                |request| {
                    matches!(
                        request,
                        ClientRequest::Agent {
                            operation: Operation::PeekInbox(Inbox {
                                all: true,
                                replies_to: None
                            }),
                            ..
                        }
                    )
                },
                ServerResponse::Agent(Reply::Inbox(messages)),
            );
            for chunk in ids.chunks(MAX_PENDING) {
                answer(
                    &mut server_stream,
                    |request| matches!(request, ClientRequest::Agent { operation: Operation::AcknowledgeInbox(receipt), .. } if receipt.ids == chunk),
                    ServerResponse::Agent(Reply::InboxAcknowledged),
                );
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none(),
                "historical messages must not generate receipts"
            );
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let reply = client
            .agent(
                Authority::default(),
                Operation::Inbox(Inbox {
                    all: true,
                    replies_to: None,
                }),
            )
            .unwrap();
        assert_eq!(reply, Reply::Inbox(expected));
        let Reply::Inbox(messages) = reply else {
            panic!("expected received inbox messages");
        };
        assert!(messages.iter().all(|message| {
            !super::super::messages::message_lines(message).contains("Receipt not confirmed")
        }));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn lost_or_stalled_inbox_receipts_return_received_bodies_and_retire_the_socket() {
        use crate::agent_execution::mailbox::{Inbox, State};
        use std::sync::mpsc;
        for stalled in [false, true] {
            let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
            let message = inbox_message(State::Pending);
            let received = message.clone();
            let (release_tx, release_rx) = mpsc::channel();
            let server = thread::spawn(move || {
                answer(&mut server_stream, |_| true, connected());
                answer(
                    &mut server_stream,
                    |request| {
                        matches!(
                            request,
                            ClientRequest::Agent {
                                operation: Operation::PeekInbox(_),
                                ..
                            }
                        )
                    },
                    ServerResponse::Agent(Reply::Inbox(vec![message.clone()])),
                );
                let receipt: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
                assert!(
                    matches!(receipt.request, ClientRequest::Agent { operation: Operation::AcknowledgeInbox(ref receipt), .. } if receipt.ids == vec![message.id])
                );
                if stalled {
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    assert!(
                        write_frame(
                            &mut server_stream,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(receipt.request_id),
                                proof: proof(),
                                message: ServerMessage::Response {
                                    response: ServerResponse::Agent(Reply::InboxAcknowledged)
                                },
                            }
                        )
                        .is_err(),
                        "a late receipt must not reach a reusable socket"
                    );
                }
            });
            let mut client = ControlClient::from_stream(client_stream).unwrap();
            let reply = client
                .agent_before(
                    Authority::default(),
                    Operation::Inbox(Inbox {
                        all: false,
                        replies_to: None,
                    }),
                    Some(Instant::now() + Duration::from_millis(250)),
                )
                .unwrap();
            assert_eq!(reply, Reply::Inbox(vec![received]));
            let Reply::Inbox(messages) = reply else {
                panic!("expected received inbox messages");
            };
            assert!(
                super::super::messages::message_lines(&messages[0])
                    .contains("Receipt not confirmed; this message may appear again.")
            );
            assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
            assert!(
                client
                    .agent(Authority::default(), Operation::Run(RunId::new()))
                    .is_err(),
                "the retired socket must reject later requests"
            );
            if stalled {
                release_tx.send(()).unwrap();
            }
            drop(client);
            server.join().unwrap();
        }
    }

    #[test]
    fn bounded_wait_returns_last_known_run_without_polling_after_deadline() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let (run_id, expected) = running();
        let response = expected.clone();
        let server = thread::spawn(move || {
            answer(&mut server_stream, |_| true, connected());
            answer(
                &mut server_stream,
                |request| matches!(request, ClientRequest::Agent { operation: Operation::Run(id), .. } if *id == run_id),
                ServerResponse::Agent(response),
            );
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none(),
                "no request after the polling deadline"
            );
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let waited = client
            .wait_agents(
                Authority::default(),
                &[run_id],
                false,
                false,
                Duration::from_millis(100),
            )
            .unwrap();
        assert!(waited.timed_out);
        assert_eq!(waited.ended_by(), "timeout");
        assert_eq!(waited.replies, vec![expected]);
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn a_message_for_a_waiting_agent_ends_its_wait() {
        use crate::agent_execution::mailbox::{Inbox, Message, Party, State};
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let (run_id, status) = running();
        let message = Message {
            id: crate::model::MessageId::new(),
            from: Party::user(),
            to: Party::user(),
            reply_to: None,
            body: "Use the staging database".into(),
            wait_until_ms: None,
            state: State::Read,
            created_ms: 0,
        };
        let delivered = vec![message.clone()];
        let inbox = |request: &ClientRequest| {
            matches!(
                request,
                ClientRequest::Agent {
                    operation: Operation::PeekInbox(Inbox {
                        all: false,
                        replies_to: None
                    }),
                    ..
                }
            )
        };
        let server = thread::spawn(move || {
            answer(&mut server_stream, |_| true, connected());
            answer(
                &mut server_stream,
                |request| {
                    matches!(
                        request,
                        ClientRequest::Agent {
                            operation: Operation::Run(_),
                            ..
                        }
                    )
                },
                ServerResponse::Agent(status),
            );
            answer(
                &mut server_stream,
                inbox,
                ServerResponse::Agent(Reply::Inbox(Vec::new())),
            );
            answer(
                &mut server_stream,
                |request| {
                    matches!(
                        request,
                        ClientRequest::Agent {
                            operation: Operation::Run(_),
                            ..
                        }
                    )
                },
                ServerResponse::Agent(running().1),
            );
            answer(
                &mut server_stream,
                inbox,
                ServerResponse::Agent(Reply::Inbox(delivered)),
            );
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let waited = client
            .wait_agents(
                Authority::default(),
                &[run_id],
                false,
                true,
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(!waited.timed_out);
        assert_eq!(waited.ended_by(), "messages");
        assert_eq!(waited.messages, vec![message]);
        server.join().unwrap();
        let printed = arrived_lines(&waited.messages);
        assert!(
            printed.contains("Messages arrived while waiting"),
            "{printed}"
        );
        assert!(printed.contains("  Use the staging database"), "{printed}");
    }

    #[test]
    fn console_answers_require_explicit_request_action_and_structured_fields() {
        use crate::agent_execution::ConsoleCommand;
        let ConsoleCommand::Control(crate::chat::Command::Answer {
            request_id,
            action,
            answers,
        }) = console_input(
            r#"/answer request-1 submit [{"field":"choice","values":["review first"]}]"#,
        )
        .unwrap()
        else {
            panic!()
        };
        assert_eq!(request_id, "request-1");
        assert_eq!(action, "submit");
        assert_eq!(
            answers,
            vec![crate::chat::Answer {
                field: "choice".into(),
                values: vec!["review first".into()]
            }]
        );
        assert!(console_input("/answer request-1").is_err());
        assert!(console_input("/answer request-1 allow not-json").is_err());
        assert!(console_input("/allow-everything").is_err());
        assert!(matches!(
            console_input("/interrupt").unwrap(),
            ConsoleCommand::Interrupt {
                including_subagents: false
            }
        ));
        assert!(matches!(
            console_input("/interrupt --including-subagents").unwrap(),
            ConsoleCommand::Interrupt {
                including_subagents: true
            }
        ));
        assert!(console_input("/interrupt --unknown").is_err());
        assert!(
            matches!(console_input("ordinary follow-up").unwrap(), ConsoleCommand::Submit { prompt, .. } if prompt == "ordinary follow-up")
        );
    }

    fn parse(arguments: &[&str]) -> AgentCommand {
        let cli = Cli::try_parse_from(["cagents", "agent"].iter().chain(arguments)).unwrap();
        let TopCommand::Agent { command } = cli.command else {
            panic!("agent command")
        };
        command
    }

    #[test]
    fn text_sources_enforce_byte_limits_without_truncation() {
        let text = "éé";
        assert_eq!(
            read_bounded_text(text.as_bytes(), "prompt", 4).unwrap(),
            text
        );
        let error = read_bounded_text(text.as_bytes(), "prompt", 3).unwrap_err();
        assert!(error.message.contains("exceeds 3 bytes"));
        assert!(read_bounded_text(&b"\xff"[..], "message", 8).is_err());
        assert!(read_text(Some("12345".into()), None, "message", "", 4).is_err());
        let prompt = PromptArgs {
            text: None,
            prompt_file: None,
            flag: Some("x".repeat(crate::agent_execution::MAX_PROMPT_BYTES + 1)),
        };
        assert!(prompt.read().unwrap_err().message.contains("exceeds"));
        let legacy_literal = PromptArgs {
            text: None,
            prompt_file: None,
            flag: Some("-".into()),
        };
        assert_eq!(legacy_literal.read().unwrap(), "-");
    }

    #[test]
    fn oversized_reader_consumes_only_the_limit_and_one_sentinel_byte() {
        let mut reader = io::Cursor::new(vec![b'x'; 4096]);
        assert!(read_bounded_text(&mut reader, "message", 16).is_err());
        assert_eq!(reader.position(), 17);
    }

    #[test]
    fn prompt_and_message_files_share_bounded_reads_and_keep_path_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.txt");
        std::fs::write(&path, "éé").unwrap();
        assert_eq!(
            read_text(None, Some(path.clone()), "prompt", "", 4).unwrap(),
            "éé"
        );
        let error = read_text(None, Some(path.clone()), "message", "", 3).unwrap_err();
        assert!(error.message.contains("exceeds 3 bytes"));
        assert!(error.message.contains("input.txt"));
        std::fs::remove_file(&path).unwrap();
        assert!(
            read_text(None, Some(path), "prompt", "", 4)
                .unwrap_err()
                .message
                .contains("input.txt")
        );
    }

    #[test]
    fn spawn_preserves_exact_prompt_target_and_retry_key() {
        let AgentCommand::Spawn {
            prompt,
            provider,
            repository,
            directory,
            retry,
            account,
            surface,
            wait,
            ..
        } = parse(&[
            "spawn",
            "--provider",
            "codex",
            "--project",
            "/repo",
            "--directory",
            "/repo/worktree",
            "--idempotency-key",
            "task-42",
            "--account",
            "codex/system",
            "--surface",
            "terminal",
            "--wait",
            "Investigate and report",
        ])
        else {
            panic!("agent spawn")
        };
        assert_eq!(prompt.read().unwrap(), "Investigate and report");
        assert_eq!(provider, Some(Provider::Codex));
        assert_eq!(repository.unwrap(), Path::new("/repo"));
        assert_eq!(directory.unwrap(), Path::new("/repo/worktree"));
        assert_eq!(retry.key(), "task-42");
        assert_eq!(account.unwrap().as_str(), "codex/system");
        assert_eq!(surface, "terminal");
        assert!(wait.wait);
    }

    #[test]
    fn spawn_needs_only_a_prompt_and_keys_each_attempt() {
        let AgentCommand::Spawn {
            prompt,
            provider,
            repository,
            directory,
            retry,
            wait,
            ..
        } = parse(&["spawn", "Review the diff"])
        else {
            panic!("agent spawn")
        };
        assert_eq!(prompt.read().unwrap(), "Review the diff");
        assert!(provider.is_none() && repository.is_none() && directory.is_none());
        assert!(!wait.wait);
        let key = retry.key();
        assert!(uuid::Uuid::parse_str(&key).is_ok(), "{key}");
        let AgentCommand::Spawn { prompt, .. } = parse(&["spawn", "--prompt", "legacy flag"])
        else {
            panic!("agent spawn")
        };
        assert_eq!(prompt.read().unwrap(), "legacy flag");
        assert!(
            Cli::try_parse_from(["cagents", "agent", "spawn", "--provider", "gpt", "work"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["cagents", "agent", "spawn", "a", "--prompt", "b"]).is_err(),
            "one prompt source"
        );
        let AgentCommand::Spawn { prompt, wait, .. } =
            parse(&["spawn", "--wait", "- fix the parser\n- add tests"])
        else {
            panic!("agent spawn")
        };
        assert_eq!(prompt.read().unwrap(), "- fix the parser\n- add tests");
        assert!(wait.wait);
        let AgentCommand::Spawn { prompt, wait, .. } = parse(&["spawn", "--flag-like", "--wait"])
        else {
            panic!("agent spawn")
        };
        assert_eq!(prompt.read().unwrap(), "--flag-like");
        assert!(wait.wait);
        assert!(
            Cli::try_parse_from(["cagents", "agent", "spawn", "--timeout", "5", "work"]).is_err(),
            "a timeout means nothing without --wait"
        );
        let AgentCommand::Spawn { prompt, .. } = parse(&["spawn", "--provider", "codex"]) else {
            panic!("agent spawn")
        };
        assert!(
            prompt.read().is_err(),
            "standard input is read only when asked for"
        );
    }

    #[test]
    fn follow_ups_and_controls_take_handles_positionally() {
        let AgentCommand::Send { agent, prompt, .. } = parse(&["send", "1a2b", "Now add tests"])
        else {
            panic!("agent send")
        };
        assert_eq!(agent, "1a2b");
        assert_eq!(prompt.read().unwrap(), "Now add tests");
        let AgentCommand::Wait {
            handles,
            any,
            timeout,
            show,
        } = parse(&["wait", "1a2b", "9f8e", "--any", "--timeout", "5", "--full"])
        else {
            panic!("agent wait")
        };
        assert_eq!(handles, ["1a2b", "9f8e"]);
        assert!(any && show.full);
        assert_eq!(timeout, 5.0);
        assert!(Cli::try_parse_from(["cagents", "agent", "wait"]).is_err());
        assert!(
            Cli::try_parse_from([
                "cagents", "agent", "status", "1a2b", "--full", "--tail", "3"
            ])
            .is_err()
        );
        let AgentCommand::Answer {
            agent,
            request,
            action,
            fields,
        } = parse(&[
            "answer",
            "1a2b",
            "request-1",
            "submit",
            "--field",
            "scope=api",
            "--field",
            "scope=ui",
        ])
        else {
            panic!("agent answer")
        };
        assert_eq!((agent.as_str(), request.as_str()), ("1a2b", "request-1"));
        assert_eq!(action, "submit");
        assert_eq!(
            answers(&fields).unwrap(),
            vec![crate::chat::Answer {
                field: "scope".into(),
                values: vec!["api".into(), "ui".into()],
            }]
        );
        assert!(answers(&["scope".into()]).is_err());
        assert!(
            matches!(parse(&["interrupt", "1a2b"]), AgentCommand::Interrupt { agent, including_subagents: false } if agent == "1a2b")
        );
    }

    #[test]
    fn interrupt_requires_explicit_background_permission() {
        assert!(
            matches!(parse(&["interrupt", "1a2b", "--including-subagents"]),
            AgentCommand::Interrupt { agent, including_subagents: true } if agent == "1a2b")
        );
    }

    fn summary(agent: &str, run: &str) -> Summary {
        Summary {
            agent: Agent {
                name: None,
                id: agent.parse().unwrap(),
                origin: crate::agent_execution::Origin::User,
                placement: crate::agent_execution::Placement::Channel(ChannelId::new()),
                spec: Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository: "/repo".into(),
                        directory: "/repo".into(),
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                },
                session: None,
                console: None,
                created_ms: 0,
                closed: false,
            },
            run: run.parse().unwrap(),
            state: crate::agent_execution::State::Running,
        }
    }

    #[test]
    fn exact_names_win_over_prefixes_and_spawn_accepts_a_name() {
        let mut summaries = [
            summary(
                "abcdef00-0000-4000-8000-000000000001",
                "12345678-0000-4000-8000-000000000001",
            ),
            summary(
                "98765432-0000-4000-8000-000000000002",
                "abcdef11-0000-4000-8000-000000000002",
            ),
        ];
        summaries[1].agent.name = Some("abcdef".into());
        assert_eq!(
            resolve(&summaries, "ABCDEF").unwrap(),
            Some(Handle::Agent(summaries[1].agent.id))
        );
        assert!(
            resolve(&summaries, "abcde").is_err(),
            "names are exact, ID prefixes remain ambiguous"
        );
        assert_eq!(
            resolve(&summaries, &summaries[0].agent.id.to_string()).unwrap(),
            Some(Handle::Agent(summaries[0].agent.id))
        );
        assert!(
            matches!(parse(&["spawn", "--name", "reviewer", "Review this"]), AgentCommand::Spawn { name: Some(name), .. } if name == "reviewer")
        );
    }

    #[test]
    fn unknown_placement_is_visible_in_human_and_json_without_polluting_run_output() {
        let output = with_placement_warning(
            CommandOutput {
                command: "agent.spawn",
                data: json!({}),
                human: "agent 1234 (claude) run 5678 Succeeded\n\nActual result".into(),
            },
            Some("No unambiguous shared view"),
        );
        assert_eq!(
            output.data["warnings"],
            json!(["No unambiguous shared view"])
        );
        assert!(
            output
                .human
                .contains("\nWarning: No unambiguous shared view\n")
        );
        assert_eq!(output.human.split_once("\n\n").unwrap().1, "Actual result");
        #[cfg(feature = "ui")]
        assert_eq!(read_run(&output.human).unwrap().result, "Actual result");
    }

    #[test]
    fn spawn_placement_uses_exact_caller_view_and_explicit_channel_overrides() {
        use crate::agent_execution::{
            Placement,
            directory::{Entry, Relation, Surface},
        };
        let channel = ChannelId::new();
        let explicit = ChannelId::new();
        let mut caller = Entry {
            id: crate::model::TerminalId::new(),
            surface: Surface::Chat,
            provider: Some(Provider::Claude),
            title: None,
            channel: Some(channel),
            view_scope: Some(Placement::Channel(channel)),
            channel_repository: Some("/repo".into()),
            state: crate::agent::ActivityState::Idle,
            directory: Some("/repo".into()),
            worktree: Some("/repo".into()),
            project: Some("/repo".into()),
            project_name: None,
            delegation: None,
            relation: Relation::Myself,
            lineage: crate::agent_execution::Lineage(crate::model::TerminalId::new()),
        };
        let repository = std::path::Path::new("/repo");
        assert_eq!(
            spawn_placement(None, Some(&caller), repository),
            (Some(Placement::Channel(channel)), None)
        );
        assert_eq!(
            spawn_placement(Some(explicit), Some(&caller), repository),
            (Some(Placement::Channel(explicit)), None)
        );
        caller.project = Some("/other-cwd-project".into());
        assert_eq!(
            spawn_placement(None, Some(&caller), repository).0,
            Some(Placement::Channel(channel)),
            "view ownership is independent of cwd"
        );
        let strip = Placement::Worktree {
            repository: repository.into(),
            path: "/repo/linked-view".into(),
        };
        caller.channel = None;
        caller.channel_repository = None;
        caller.view_scope = Some(strip.clone());
        assert_eq!(
            spawn_placement(None, Some(&caller), repository),
            (Some(strip), None)
        );
        assert_eq!(
            spawn_placement(Some(explicit), Some(&caller), repository).0,
            Some(Placement::Channel(explicit))
        );
        let (scope, warning) = spawn_placement(None, Some(&caller), std::path::Path::new("/other"));
        assert!(scope.is_none());
        assert!(warning.unwrap().contains("another project"));
        caller.view_scope = None;
        let (scope, warning) = spawn_placement(None, Some(&caller), repository);
        assert!(scope.is_none());
        assert!(warning.unwrap().contains("no unambiguous shared view"));
        assert_eq!(
            spawn_placement(None, None, repository),
            (None, None),
            "external callers retain fresh-channel behavior"
        );
        let mut legacy = serde_json::to_value(&caller).unwrap();
        legacy.as_object_mut().unwrap().remove("view_scope");
        legacy.as_object_mut().unwrap().remove("channel");
        legacy.as_object_mut().unwrap().remove("channel_repository");
        let legacy: Entry = serde_json::from_value(legacy).unwrap();
        assert!(legacy.view_scope.is_none());
        assert!(legacy.channel.is_none());
        assert!(legacy.channel_repository.is_none());
    }

    #[test]
    fn handles_resolve_by_unique_prefix_and_name_their_rivals() {
        let summaries = [
            summary(
                "1a2b3c4d-0000-4000-8000-000000000001",
                "9f8e7d6c-0000-4000-8000-000000000001",
            ),
            summary(
                "1a2b9999-0000-4000-8000-000000000002",
                "5555aaaa-0000-4000-8000-000000000002",
            ),
        ];
        let first = summaries[0].agent.id;
        let resolved = |handle: &str| resolve(&summaries, handle).unwrap();
        assert_eq!(resolved("1a2b3"), Some(Handle::Agent(first)));
        assert_eq!(resolved(" 1A2B3C "), Some(Handle::Agent(first)));
        assert_eq!(resolved("9f8e"), Some(Handle::Run(summaries[0].run)));
        assert_eq!(resolved(&first.to_string()), Some(Handle::Agent(first)));
        assert_eq!(
            resolved(&summaries[1].run.to_string()),
            Some(Handle::Run(summaries[1].run)),
            "a whole run ID is a run, though it parses as an agent ID too"
        );
        let ambiguous = resolve(&summaries, "1a2b").unwrap_err().message;
        assert!(
            ambiguous.contains("agent 1a2b3c4d-000") && ambiguous.contains("agent 1a2b9999-000"),
            "{ambiguous}"
        );
        assert_eq!(
            resolved("77777777-0000-4000-8000-000000000003"),
            None,
            "a whole ID outside the list is left to the command"
        );
        assert!(resolve(&summaries, "zzzz").is_err());
        assert!(resolve(&summaries, "").is_err());
    }

    #[test]
    fn projects_are_named_or_located() {
        let name = super::super::project_argument("no-such-project-here".into()).unwrap();
        assert_eq!(name, Path::new("no-such-project-here"), "a word is a name");
        let nested = super::super::project_argument("no/such/place".into()).unwrap();
        assert!(nested.is_absolute(), "a path is a path, existing or not");
        let here = super::super::project_argument("src".into()).unwrap();
        assert!(
            here.is_absolute() && here.ends_with("src"),
            "an existing word is a path"
        );
        assert_eq!(checkout(Path::new("/repo/.git")), Path::new("/repo"));
        assert_eq!(checkout(Path::new("/notes")), Path::new("/notes"));
    }

    #[test]
    fn results_show_their_end_unless_asked_for_all() {
        let text = (1..=30)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let tail = ShowArgs {
            full: false,
            tail: 2,
        };
        assert_eq!(
            tail.result(&text),
            "… 28 earlier lines (--full for all)\n29\n30"
        );
        assert_eq!(tail.result("one\ntwo\n"), "one\ntwo");
        let full = ShowArgs {
            full: true,
            tail: 2,
        };
        assert_eq!(full.result(&text), text);
    }
}

fn run_console(agent: AgentId) -> Result<CommandOutput, AppError> {
    use crate::agent_execution::ConsoleCommand;
    use std::io::{BufRead, Write};
    let authority = Authority::from_environment();
    if authority.token().is_none() {
        return Err(AppError::new(
            "agent_console",
            "Open consoles through agent spawn --surface terminal",
        ));
    }
    let socket = terminal::ipc::daemon_socket_path().map_err(|e| AppError::new("daemon", e))?;
    let mut client = ControlClient::connect(&socket).map_err(protocol_error)?;
    let (sender, input) = std::sync::mpsc::sync_channel(8);
    thread::Builder::new()
        .name("console-input".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut reader = stdin.lock();
            loop {
                let mut line = String::new();
                let result = reader
                    .by_ref()
                    .take((crate::agent_execution::MAX_PROMPT_BYTES + 1) as u64)
                    .read_line(&mut line);
                match result {
                    Ok(0) | Err(_) => {
                        let _ = sender.send(None);
                        break;
                    }
                    Ok(_) if line.len() > crate::agent_execution::MAX_PROMPT_BYTES => {
                        let _ = sender.send(Some(Err("Input exceeds the prompt limit".to_owned())));
                        break;
                    }
                    Ok(_) => {
                        if sender
                            .send(Some(Ok(line.trim_end_matches(['\r', '\n']).to_owned())))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .map_err(|e| AppError::new("console_input", e))?;
    println!(
        "cagents provider console — agent {agent}\nPrompts create durable runs. /interrupt [--including-subagents], /answer REQUEST ACTION [ANSWERS_JSON], /quit\nThis is a console over the hosted provider, not its stock TUI.\n"
    );
    let startup = Instant::now() + Duration::from_secs(10);
    let mut generation = None;
    let mut previous = String::new();
    let mut last_run = None;
    let mut last_requests = Vec::new();
    loop {
        match client.agent(
            authority.clone(),
            Operation::Console {
                agent,
                command: ConsoleCommand::Read {
                    after_generation: generation,
                    after_run: last_run,
                },
            },
        ) {
            Ok(Reply::Console(Some(view))) => {
                generation = Some(view.generation);
                let text: String = view
                    .text
                    .chars()
                    .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
                    .collect();
                if let Some(delta) = text.strip_prefix(&previous) {
                    print!("{delta}");
                } else if text != previous {
                    print!(
                        "\n--- Current transcript{} ---\n{text}",
                        if view.truncated { " (bounded)" } else { "" }
                    );
                }
                previous = text;
                if last_run != Some((view.run.id, view.run.revision)) {
                    println!(
                        "\nRun {}: {:?}; usage: {}",
                        view.run.id,
                        view.run.state,
                        serde_json::to_string(&view.run.result.usage).unwrap_or_default()
                    );
                    if let Some(diagnostic) = &view.run.result.diagnostic {
                        println!("{diagnostic}");
                    }
                    last_run = Some((view.run.id, view.run.revision));
                }
                if view.requests != last_requests {
                    for request in &view.requests {
                        println!(
                            "\nInput requested:\n{}\nUse /answer {} ACTION [ANSWERS_JSON] with an action and field IDs shown above.",
                            serde_json::to_string_pretty(request).unwrap_or_default(),
                            request.id
                        );
                    }
                    last_requests = view.requests;
                }
                let _ = io::stdout().flush();
            }
            Ok(Reply::Console(None)) => {}
            Ok(_) => {
                return Err(AppError::new(
                    "agent_console",
                    "Unexpected console response",
                ));
            }
            Err(ControlError::Remote { message, .. })
                if generation.is_none()
                    && Instant::now() < startup
                    && (message.contains("not attached yet")
                        || message.contains("no longer live")
                        || message.contains("paired provider console is unavailable")) => {}
            Err(error) => return Err(protocol_error(error)),
        }
        match input.recv_timeout(Duration::from_millis(250)) {
            Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Ok(Some(Err(error))) => return Err(AppError::new("console_input", error)),
            Ok(Some(Ok(line))) => {
                if line == "/quit" {
                    break;
                }
                if line.trim().is_empty() {
                    continue;
                }
                let command = match console_input(&line) {
                    Ok(command) => command,
                    Err(error) => {
                        eprintln!("{error}");
                        continue;
                    }
                };
                match client.agent(authority.clone(), Operation::Console { agent, command }) {
                    Ok(Reply::Accepted(accepted)) => println!("Accepted run {}", accepted.run.id),
                    Ok(_) => {}
                    Err(error) => eprintln!("{error}"),
                }
                generation = None;
            }
        }
    }
    Ok(CommandOutput {
        command: "agent.console",
        data: json!({ "agent": agent, "detached": true }),
        human: "Console detached; the provider remains hosted until explicitly closed.".into(),
    })
}

fn console_input(line: &str) -> Result<crate::agent_execution::ConsoleCommand, String> {
    use crate::{agent_execution::ConsoleCommand, chat::Command};
    if line == "/interrupt" || line == "/interrupt --including-subagents" {
        return Ok(ConsoleCommand::Interrupt {
            including_subagents: line != "/interrupt",
        });
    }
    if let Some(answer) = line.strip_prefix("/answer ") {
        let mut words = answer.splitn(3, ' ');
        let request_id = words
            .next()
            .filter(|value| !value.is_empty())
            .ok_or("Missing request ID")?
            .into();
        let action = words
            .next()
            .filter(|value| !value.is_empty())
            .ok_or("Missing explicit action")?
            .into();
        let answers = match words.next() {
            Some(json) => {
                serde_json::from_str(json).map_err(|e| format!("Invalid answer fields: {e}"))?
            }
            None => Vec::new(),
        };
        return Ok(ConsoleCommand::Control(Command::Answer {
            request_id,
            action,
            answers,
        }));
    }
    if line.starts_with('/') {
        return Err("Commands: /interrupt [--including-subagents], /answer REQUEST ACTION [ANSWERS_JSON], /quit".into());
    }
    Ok(ConsoleCommand::Submit {
        prompt: line.into(),
        idempotency_key: uuid::Uuid::new_v4().to_string(),
    })
}
