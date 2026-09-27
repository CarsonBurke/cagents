use super::*;
use crate::{
    account::AccountId,
    agent::Provider,
    agent_execution::{Authority, Destination, Operation, Reply, Spec, Submit, Surface, Target},
    model::{AgentId, RunId},
};

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// The terminal presentation of an already hosted execution agent.
    #[command(hide = true)]
    Console { agent_id: AgentId },
    /// Accept one prompt durably and start a real provider session.
    Spawn {
        #[arg(long, value_parser = parse_provider)]
        provider: Provider,
        #[arg(long)]
        repository: PathBuf,
        #[arg(long)]
        directory: PathBuf,
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
        #[arg(long)]
        prompt: String,
        /// Reuse this key with the identical request to retrieve its acceptance.
        #[arg(long)]
        idempotency_key: String,
    },
    /// Accept a follow-up prompt for an owned, settled conversation.
    Send {
        agent_id: AgentId,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        idempotency_key: String,
    },
    /// Read an owned conversation and its latest accepted run.
    Status { agent_id: AgentId },
    /// List this caller's open agents without copying their result bodies.
    List,
    /// Wait for this exact accepted run to settle or request input.
    Wait {
        run_id: RunId,
        #[arg(long, default_value = "60", value_parser = parse_positive_seconds)]
        timeout: f64,
    },
    /// Stop and close an agent created by this caller.
    Close { agent_id: AgentId },
}

fn parse_provider(value: &str) -> Result<Provider, String> {
    Provider::ALL
        .into_iter()
        .find(|provider| provider.executable() == value)
        .ok_or_else(|| "Choose omp, claude, or codex".into())
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
    fn wait_agent(
        &mut self,
        authority: Authority,
        run: RunId,
        timeout: Duration,
    ) -> Result<(Reply, bool), ControlError> {
        let deadline = Instant::now() + timeout;
        let mut reply =
            self.agent_before(authority.clone(), Operation::Run(run), Some(deadline))?;
        loop {
            if matches!(&reply, Reply::Status { run, .. } if run.state.wait_ready()) {
                return Ok((reply, false));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok((reply, true));
            }
            thread::sleep(Duration::from_millis(200).min(remaining));
            if Instant::now() >= deadline {
                return Ok((reply, true));
            }
            match self.agent_before(authority.clone(), Operation::Run(run), Some(deadline)) {
                Ok(current) => reply = current,
                Err(error) if is_timeout_error(&error) => return Ok((reply, true)),
                Err(error) => return Err(error),
            }
        }
    }
}

pub(super) fn execute(command: AgentCommand) -> Result<CommandOutput, AppError> {
    if let AgentCommand::Console { agent_id } = command {
        return run_console(agent_id);
    }
    let authority = Authority::from_environment();
    let (name, operation, wait) = match command {
        AgentCommand::Console { .. } => unreachable!("handled console presentation"),
        AgentCommand::Spawn {
            provider,
            repository,
            directory,
            channel,
            account,
            surface,
            model,
            permission_mode,
            prompt,
            idempotency_key,
        } => {
            let repository =
                std::path::absolute(repository).map_err(|e| AppError::new("target", e))?;
            let directory =
                std::path::absolute(directory).map_err(|e| AppError::new("target", e))?;
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
                }),
                prompt,
                idempotency_key,
            };
            request
                .validate()
                .map_err(|e| AppError::new("agent_request", e))?;
            ("agent.spawn", Operation::Submit(request), None)
        }
        AgentCommand::Send {
            agent_id,
            prompt,
            idempotency_key,
        } => (
            "agent.send",
            Operation::Submit(Submit {
                destination: Destination::Existing(agent_id),
                prompt,
                idempotency_key,
            }),
            None,
        ),
        AgentCommand::Status { agent_id } => ("agent.status", Operation::Status(agent_id), None),
        AgentCommand::List => ("agent.list", Operation::List, None),
        AgentCommand::Wait { run_id, timeout } => (
            "agent.wait",
            Operation::Run(run_id),
            Some(Duration::from_secs_f64(timeout)),
        ),
        AgentCommand::Close { agent_id } => ("agent.close", Operation::Close(agent_id), None),
    };
    let socket = terminal::ensure_daemon_ready().map_err(|e| AppError::new("daemon_start", e))?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    let (reply, timed_out) = if let Some(timeout) = wait {
        let Operation::Run(run) = operation else {
            unreachable!("wait reads a run")
        };
        client
            .wait_agent(authority, run, timeout)
            .map_err(protocol_error)?
    } else {
        (
            client.agent(authority, operation).map_err(protocol_error)?,
            false,
        )
    };
    let human = match &reply {
        Reply::Accepted(accepted) => format!(
            "agent {} run {} {:?}",
            accepted.agent.id, accepted.run.id, accepted.run.state
        ),
        Reply::Status { agent, run } => format!(
            "agent {} run {} {:?}{}\n{}",
            agent.id,
            run.id,
            run.state,
            if timed_out { " (wait timed out)" } else { "" },
            run.result.text
        ),
        Reply::Listed(agents) => agents
            .iter()
            .map(|entry| format!("{} {} {:?}", entry.agent.id, entry.run, entry.state))
            .collect::<Vec<_>>()
            .join("\n"),
        Reply::Closed(agent) => format!("closed agent {}", agent.id),
        Reply::Console(_) => "Console updated".into(),
    };
    Ok(CommandOutput {
        command: name,
        data: reply_data(reply, timed_out),
        human,
    })
}

fn reply_data(reply: Reply, timed_out: bool) -> serde_json::Value {
    match reply {
        Reply::Accepted(accepted) => {
            json!({"agent": accepted.agent, "run": accepted.run, "created": accepted.created})
        }
        Reply::Status { agent, run } => json!({"agent": agent, "run": run, "timed_out": timed_out}),
        Reply::Listed(agents) => json!({"agents": agents}),
        Reply::Closed(agent) => json!({"agent": agent}),
        Reply::Console(view) => json!({"console": view}),
    }
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
        let (reply, timed_out) = client
            .wait_agent(Authority::default(), run_id, Duration::from_millis(100))
            .unwrap();
        assert!(timed_out);
        assert_eq!(reply, expected);
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

    #[test]
    fn cli_preserves_exact_prompt_target_and_retry_key() {
        let cli = Cli::try_parse_from([
            "cagents",
            "--json",
            "agent",
            "spawn",
            "--provider",
            "codex",
            "--repository",
            "/repo",
            "--directory",
            "/repo/worktree",
            "--prompt",
            "Investigate and report",
            "--idempotency-key",
            "task-42",
            "--account",
            "codex/system",
            "--surface",
            "terminal",
        ])
        .unwrap();
        let TopCommand::Agent {
            command:
                AgentCommand::Spawn {
                    prompt,
                    idempotency_key,
                    account,
                    surface,
                    ..
                },
        } = cli.command
        else {
            panic!("agent spawn")
        };
        assert_eq!(prompt, "Investigate and report");
        assert_eq!(idempotency_key, "task-42");
        assert_eq!(account.unwrap().as_str(), "codex/system");
        assert_eq!(surface, "terminal");
        assert!(
            Cli::try_parse_from([
                "cagents",
                "agent",
                "spawn",
                "--provider",
                "codex",
                "--repository",
                "/repo",
                "--directory",
                "/repo",
                "--prompt",
                "work"
            ])
            .is_err(),
            "retries require an explicit key"
        );
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
