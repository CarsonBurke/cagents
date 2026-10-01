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
    Interrupt { agent: String },
    /// Stop and close an agent created by this caller.
    Close { agent: String },
}

/// A prompt given as an argument, in a file, or on standard input.
#[derive(Debug, clap::Args)]
pub struct PromptArgs {
    /// The prompt; `-`, or none with input piped in, reads standard input.
    #[arg(value_name = "PROMPT")]
    text: Option<String>,
    /// Read the prompt from this file.
    #[arg(long, value_name = "FILE", conflicts_with = "text")]
    prompt_file: Option<PathBuf>,
    #[arg(long = "prompt", hide = true, conflicts_with_all = ["text", "prompt_file"])]
    flag: Option<String>,
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
    wait: bool,
    /// How long `--wait` waits.
    #[arg(long, default_value = "60", value_parser = parse_positive_seconds)]
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

fn parse_provider(value: &str) -> Result<Provider, String> {
    Provider::ALL
        .into_iter()
        .find(|provider| provider.executable() == value)
        .ok_or_else(|| "Choose omp, claude, or codex".into())
}

impl PromptArgs {
    fn read(self) -> Result<String, AppError> {
        let read_input = || {
            let mut prompt = String::new();
            io::stdin()
                .take(crate::agent_execution::MAX_PROMPT_BYTES as u64 + 1)
                .read_to_string(&mut prompt)
                .map(|_| prompt)
                .map_err(|e| AppError::new("prompt", e))
        };
        match (self.text, self.prompt_file, self.flag) {
            (Some(text), ..) if text == "-" => read_input(),
            (Some(text), ..) | (_, _, Some(text)) => Ok(text),
            (_, Some(file), _) => std::fs::read_to_string(&file)
                .map_err(|e| AppError::new("prompt", format!("{}: {e}", file.display()))),
            (None, None, None) if !io::stdin().is_terminal() => read_input(),
            (None, None, None) => Err(AppError::new(
                "prompt",
                "Give a prompt, --prompt-file FILE, or pipe it in",
            )),
        }
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

    fn agent_before(
        &mut self,
        authority: Authority,
        operation: Operation,
        deadline: Option<Instant>,
    ) -> Result<Reply, ControlError> {
        let closing = matches!(&operation, Operation::Close(_));
        let request = ClientRequest::Agent {
            authority,
            operation,
        };
        let response = if closing {
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

impl ControlClient {
    /// Polls `runs` until all of them, or with `any` the first, settle or ask
    /// for input, or `timeout` passes. Returns each run's last known status
    /// and whether the deadline came first.
    fn wait_agents(
        &mut self,
        authority: Authority,
        runs: &[RunId],
        any: bool,
        timeout: Duration,
    ) -> Result<(Vec<Reply>, bool), ControlError> {
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
        loop {
            let done = if any {
                replies.iter().any(ready)
            } else {
                replies.iter().all(ready)
            };
            if done {
                return Ok((replies, false));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok((replies, true));
            }
            thread::sleep(Duration::from_millis(200).min(remaining));
            if Instant::now() >= deadline {
                return Ok((replies, true));
            }
            for (run, reply) in runs.iter().zip(replies.iter_mut()) {
                if ready(reply) {
                    continue;
                }
                match self.agent_before(authority.clone(), Operation::Run(*run), Some(deadline)) {
                    Ok(current) => *reply = current,
                    Err(error) if is_timeout_error(&error) => return Ok((replies, true)),
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

/// What a handle names among the caller's agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handle {
    Agent(AgentId),
    Run(RunId),
}

/// Resolves a typed handle against the caller's agents and their latest
/// runs: a whole ID, or a prefix that names exactly one of them. A whole ID
/// that names none is taken as a run, which may be an older one.
fn resolve(summaries: &[Summary], handle: &str) -> Result<Handle, AppError> {
    let wanted = handle.trim().to_ascii_lowercase();
    if wanted.is_empty() {
        return Err(AppError::new("handle", "An agent or run handle is empty"));
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
                return Ok(candidate);
            }
            if text.starts_with(&wanted) && !matches.iter().any(|(known, _)| *known == candidate) {
                matches.push((candidate, text));
            }
        }
    }
    match matches.as_slice() {
        [(handle, _)] => Ok(*handle),
        [] => match wanted.parse::<RunId>() {
            Ok(run) => Ok(Handle::Run(run)),
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

/// A connected client with the caller's authority, and the caller's agents
/// once a handle needed them.
struct Session {
    client: ControlClient,
    authority: Authority,
    socket: PathBuf,
    summaries: Option<Vec<Summary>>,
}

impl Session {
    fn connect() -> Result<Self, AppError> {
        let socket =
            terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e))?;
        let client =
            ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
        Ok(Self {
            client,
            authority: Authority::from_environment(),
            socket,
            summaries: None,
        })
    }

    fn call(&mut self, operation: Operation) -> Result<Reply, AppError> {
        self.client
            .agent(self.authority.clone(), operation)
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

    /// An agent handle, or the agent of a run handle. A whole ID is taken
    /// as an agent without listing.
    fn agent(&mut self, handle: &str) -> Result<AgentId, AppError> {
        if let Ok(agent) = handle.trim().parse::<AgentId>() {
            return Ok(agent);
        }
        match resolve(self.summaries()?, handle)? {
            Handle::Agent(agent) => Ok(agent),
            Handle::Run(run) => self
                .summaries()?
                .iter()
                .find(|summary| summary.run == run)
                .map(|summary| summary.agent.id)
                .ok_or_else(|| {
                    AppError::new("handle", format!("{handle} is not one of your agents"))
                }),
        }
    }

    /// The latest run of an agent handle, or the run a run handle names.
    fn run(&mut self, handle: &str) -> Result<RunId, AppError> {
        match resolve(self.summaries()?, handle)? {
            Handle::Run(run) => Ok(run),
            Handle::Agent(agent) => self
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
    /// than start the work twice.
    fn submit(&mut self, request: Submit) -> Result<Reply, AppError> {
        match self
            .client
            .agent(self.authority.clone(), Operation::Submit(request.clone()))
        {
            Ok(reply) => Ok(reply),
            Err(
                ControlError::Frame(_) | ControlError::ConnectionClosed | ControlError::Connect(_),
            ) => {
                self.client = ControlClient::connect(&self.socket)
                    .map_err(|e| AppError::new("daemon_connection", e))?;
                self.call(Operation::Submit(request))
            }
            Err(error) => Err(protocol_error(error)),
        }
    }

    /// The open project `scope` names, and the calling session's provider,
    /// which a spawned agent defaults to.
    fn context(&mut self, scope: Scope) -> Result<(Option<PathBuf>, Option<Provider>), AppError> {
        let Reply::Directory(directory) = self.call(Operation::Directory(scope))? else {
            return Err(AppError::new("sessions", "Unexpected directory response"));
        };
        let project = match directory.scope {
            Scope::Project(project) => Some(project),
            _ => None,
        };
        Ok((project, directory.caller.and_then(|caller| caller.provider)))
    }
}

pub(super) fn execute(command: AgentCommand) -> Result<CommandOutput, AppError> {
    if let AgentCommand::Console { agent_id } = command {
        return run_console(agent_id);
    }
    let mut session = Session::connect()?;
    match command {
        AgentCommand::Console { .. } => unreachable!("handled console presentation"),
        AgentCommand::Spawn {
            prompt,
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
            let absolute =
                |path: PathBuf| std::path::absolute(path).map_err(|e| AppError::new("target", e));
            let repository = repository.map(absolute).transpose()?;
            // An agent sent to another project works at the path given for
            // it, and otherwise here.
            let directory = absolute(match (directory, &repository) {
                (Some(directory), _) => directory,
                (None, Some(repository)) => repository.clone(),
                (None, None) => std::env::current_dir().map_err(|e| AppError::new("target", e))?,
            })?;
            // The project is the open one containing the given path, which
            // may be any directory in it, such as a worktree.
            let scope = match repository {
                Some(repository) => Scope::Project(repository),
                None => Scope::Containing(directory.clone()),
            };
            let (repository, caller) = session.context(scope)?;
            let repository = repository.ok_or_else(|| {
                AppError::new(
                    "target",
                    format!(
                        "No open project contains {}; open it in cagents or pass --project",
                        directory.display()
                    ),
                )
            })?;
            let provider = provider.or(caller).ok_or_else(|| {
                AppError::new("agent_request", "Choose --provider claude, codex, or omp")
            })?;
            let request = Submit {
                destination: Destination::New(Spec {
                    provider,
                    account,
                    target: Target {
                        repository,
                        directory,
                        channel,
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
                prompt: prompt.read()?,
                idempotency_key: retry.key(),
            };
            submitted(&mut session, "agent.spawn", request, wait, show)
        }
        AgentCommand::Send {
            agent,
            prompt,
            retry,
            wait,
            show,
        } => {
            let request = Submit {
                destination: Destination::Existing(session.agent(&agent)?),
                prompt: prompt.read()?,
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
                            "{}  {:<6} {:<11} run {}  {}",
                            short(summary.agent.id),
                            summary.agent.spec.provider.executable(),
                            format!("{:?}", summary.state),
                            short(summary.run),
                            summary.agent.spec.target.directory.display()
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
            let (replies, timed_out) = session
                .client
                .wait_agents(
                    session.authority.clone(),
                    &runs,
                    any,
                    Duration::from_secs_f64(timeout),
                )
                .map_err(protocol_error)?;
            let statuses = replies
                .into_iter()
                .map(status_reply)
                .collect::<Result<Vec<_>, _>>()?;
            let human = statuses
                .iter()
                .map(|(agent, run)| {
                    status_lines(agent, run, timed_out && !run.state.wait_ready(), show)
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            let data = match statuses.as_slice() {
                [(agent, run)] => json!({"agent": agent, "run": run, "timed_out": timed_out}),
                _ => json!({
                    "results": statuses
                        .iter()
                        .map(|(agent, run)| json!({"agent": agent, "run": run}))
                        .collect::<Vec<_>>(),
                    "timed_out": timed_out,
                }),
            };
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
        AgentCommand::Interrupt { agent } => {
            let agent = session.agent(&agent)?;
            let requests = requests_reply(session.call(Operation::Control {
                agent,
                command: Control::Interrupt,
            })?)?;
            Ok(CommandOutput {
                command: "agent.interrupt",
                data: json!({"agent": agent, "requests": requests}),
                human: format!("Interrupted agent {}", short(agent)),
            })
        }
        AgentCommand::Close { agent } => {
            let agent = session.agent(&agent)?;
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
    let Reply::Accepted(accepted) = session.submit(request)? else {
        return Err(AppError::new(command, "Unexpected submit response"));
    };
    let mut human = format!(
        "{} agent {} run {}",
        if accepted.created {
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
        let (mut replies, timed_out) = session
            .client
            .wait_agents(
                session.authority.clone(),
                &[accepted.run.id],
                false,
                Duration::from_secs_f64(wait.timeout),
            )
            .map_err(protocol_error)?;
        let (agent, run) = status_reply(replies.remove(0))?;
        human = status_lines(&agent, &run, timed_out, show);
        data["agent"] = json!(agent);
        data["run"] = json!(run);
        data["timed_out"] = json!(timed_out);
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
    use super::*;

    #[test]
    fn bounded_wait_returns_last_known_run_without_polling_after_deadline() {
        use crate::agent_execution::{Agent, Origin, Run, State};
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("agent-wait"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("incarnation"),
        };
        let agent = Agent {
            id: AgentId::new(),
            origin: Origin::User,
            channel: crate::model::ChannelId::new(),
            spec: Spec {
                provider: Provider::Claude,
                account: None,
                target: Target {
                    repository: "/repo".into(),
                    directory: "/repo".into(),
                    channel: None,
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
        let run_id = run.id;
        let expected = Reply::Status { agent, run };
        let response = expected.clone();
        let server = thread::spawn(move || {
            let connect: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(connect.request_id),
                    proof: proof.clone(),
                    message: ServerMessage::Response {
                        response: ServerResponse::Connected {
                            proof: proof.clone(),
                            resumed: Vec::new(),
                        },
                    },
                },
            )
            .unwrap();
            let request: ClientEnvelope = read_frame(&mut server_stream).unwrap().unwrap();
            assert!(
                matches!(request.request, ClientRequest::Agent { operation: Operation::Run(id), .. } if id == run_id)
            );
            write_frame(
                &mut server_stream,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(request.request_id),
                    proof,
                    message: ServerMessage::Response {
                        response: ServerResponse::Agent(response),
                    },
                },
            )
            .unwrap();
            assert!(
                read_frame::<ClientEnvelope>(&mut server_stream)
                    .unwrap()
                    .is_none(),
                "no request after the polling deadline"
            );
        });
        let mut client = ControlClient::from_stream(client_stream).unwrap();
        let (replies, timed_out) = client
            .wait_agents(
                Authority::default(),
                &[run_id],
                false,
                Duration::from_millis(100),
            )
            .unwrap();
        assert!(timed_out);
        assert_eq!(replies, vec![expected]);
        assert_eq!(client.stream.read_timeout().unwrap(), Some(IO_TIMEOUT));
        drop(client);
        server.join().unwrap();
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
            ConsoleCommand::Control(crate::chat::Command::Interrupt)
        ));
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
            matches!(parse(&["interrupt", "1a2b"]), AgentCommand::Interrupt { agent } if agent == "1a2b")
        );
    }

    fn summary(agent: &str, run: &str) -> Summary {
        Summary {
            agent: Agent {
                id: agent.parse().unwrap(),
                origin: crate::agent_execution::Origin::User,
                channel: ChannelId::new(),
                spec: Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository: "/repo".into(),
                        directory: "/repo".into(),
                        channel: None,
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
        assert_eq!(resolve(&summaries, "1a2b3").unwrap(), Handle::Agent(first));
        assert_eq!(
            resolve(&summaries, " 1A2B3C ").unwrap(),
            Handle::Agent(first)
        );
        assert_eq!(
            resolve(&summaries, "9f8e").unwrap(),
            Handle::Run(summaries[0].run)
        );
        assert_eq!(
            resolve(&summaries, &first.to_string()).unwrap(),
            Handle::Agent(first)
        );
        let ambiguous = resolve(&summaries, "1a2b").unwrap_err().message;
        assert!(
            ambiguous.contains("agent 1a2b3c4d-000") && ambiguous.contains("agent 1a2b9999-000"),
            "{ambiguous}"
        );
        let older = "77777777-0000-4000-8000-000000000003";
        assert_eq!(
            resolve(&summaries, older).unwrap(),
            Handle::Run(older.parse().unwrap()),
            "a whole ID outside the list is an older run"
        );
        assert!(resolve(&summaries, "zzzz").is_err());
        assert!(resolve(&summaries, "").is_err());
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
        "cagents provider console — agent {agent}\nPrompts create durable runs. /interrupt, /answer REQUEST ACTION [ANSWERS_JSON], /quit\nThis is a console over the hosted provider, not its stock TUI.\n"
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
    if line == "/interrupt" {
        return Ok(ConsoleCommand::Control(Command::Interrupt));
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
        return Err("Commands: /interrupt, /answer REQUEST ACTION [ANSWERS_JSON], /quit".into());
    }
    Ok(ConsoleCommand::Submit {
        prompt: line.into(),
        idempotency_key: uuid::Uuid::new_v4().to_string(),
    })
}
