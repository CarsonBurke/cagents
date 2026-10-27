//! Worktree control routes through the selected daemon's journal and fences.
use super::*;
use crate::{
    agent::Provider,
    worktree_control::{Operation, Reply},
};

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// Create a journaled worktree, then run approved setup before any agent.
    Create {
        branch: String,
        #[arg(long)]
        base: Option<String>,
        /// Absolute checkout, persisted project ID, or unambiguous open name.
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        linear: Option<String>,
        /// Skip both setup copies and commands.
        #[arg(long)]
        no_setup: bool,
        #[arg(long, value_parser = agents::parse_provider)]
        spawn_agent: Option<Provider>,
        #[arg(
            long,
            requires = "spawn_agent",
            default_value = "Inspect this worktree and await further instructions."
        )]
        prompt: String,
        #[arg(long, requires = "spawn_agent")]
        model: Option<String>,
        #[arg(long, requires = "spawn_agent")]
        reasoning_effort: Option<String>,
    },
    /// List paths, branches, state, upstream counts and live sessions.
    List {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Inspect retained setup or explicitly review a fresh run.
    Setup {
        #[command(subcommand)]
        command: SetupCommand,
    },
    /// Remove a linked checkout after checking files, commits and live owners.
    Remove {
        target: String,
        #[arg(long)]
        project: Option<PathBuf>,
        /// Confirm discarding local files/history; never bypass live ownership.
        #[arg(long)]
        force: bool,
        /// Separately request Git's safe branch -d, never force branch deletion.
        #[arg(long)]
        delete_branch: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    List {
        #[arg(long)]
        project: Option<PathBuf>,
    },
    Review {
        path: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Start a fresh reviewed run; an uncertain previous run cannot be replayed.
    Resume {
        path: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        creation: crate::model::OperationId,
        #[arg(long)]
        expected_run: Option<crate::model::OperationId>,
        #[arg(long)]
        fingerprint: Option<String>,
        #[arg(long)]
        trust: bool,
    },
    /// Explicitly skip setup after reviewing this exact retained creation.
    Skip {
        path: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        creation: crate::model::OperationId,
        #[arg(long)]
        expected_run: Option<crate::model::OperationId>,
        #[arg(long)]
        fingerprint: Option<String>,
    },
}
fn checkout_path(path: PathBuf) -> Result<PathBuf, AppError> {
    if crate::device::is_remote() {
        if !path.is_absolute() {
            return Err(AppError::new(
                "project",
                "Use an absolute path on the selected device",
            ));
        }
        Ok(path)
    } else {
        path.canonicalize()
            .map_err(|error| AppError::new("project", error))
    }
}
fn request(client: &mut ControlClient, operation: Operation) -> Result<Reply, AppError> {
    // This uses the dedicated finite deadline in the CLI activation seam.
    // A lost response never triggers a mutation retry.
    client.worktree_operation(operation).map_err(protocol_error)
}
fn resolve(client: &mut ControlClient, path: PathBuf) -> Result<PathBuf, AppError> {
    match request(client, Operation::ResolveRepository { path })? {
        Reply::ResolvedRepository { repository } => Ok(repository),
        _ => Err(AppError::new("protocol", "Unexpected repository response")),
    }
}
fn repository(
    client: &mut ControlClient,
    selector: Option<PathBuf>,
    model: &WorkspaceModel,
) -> Result<PathBuf, AppError> {
    let Some(given) = selector else {
        if crate::device::is_remote() {
            return Err(AppError::new(
                "project",
                "Choose an absolute project path or open project name on the selected device",
            ));
        }
        return resolve(
            client,
            std::env::current_dir().map_err(|error| AppError::new("project", error))?,
        );
    };
    let explicit = given.is_absolute()
        || matches!(
            given.components().next(),
            Some(std::path::Component::CurDir | std::path::Component::ParentDir)
        );
    if !explicit {
        if let Some(id) = given
            .to_str()
            .and_then(|text| text.parse::<crate::model::ProjectId>().ok())
        {
            return match request(client, Operation::ResolveProject { id })? {
                Reply::ResolvedRepository { repository } => resolve(client, repository),
                _ => Err(AppError::new(
                    "protocol",
                    "Unexpected project identity response",
                )),
            };
        }
        let named = model
            .projects
            .iter()
            .filter(|project| Path::new(&project.name) == given)
            .collect::<Vec<_>>();
        match named.as_slice() {
            [project] => {
                let named = resolve(client, project.repository.clone())?;
                if !crate::device::is_remote()
                    && given
                        .try_exists()
                        .map_err(|error| AppError::new("project", error))?
                {
                    let path = resolve(
                        client,
                        std::env::current_dir()
                            .map_err(|error| AppError::new("project", error))?
                            .join(&given),
                    )?;
                    if path != named {
                        return Err(AppError::new(
                            "project",
                            "This selector names different directory and open projects; use the exact absolute path or ./path",
                        ));
                    }
                }
                return Ok(named);
            }
            [] => {}
            _ => {
                return Err(AppError::new(
                    "project",
                    "This open project name is ambiguous; use its absolute path",
                ));
            }
        }
    }
    if crate::device::is_remote() && !given.is_absolute() {
        return Err(AppError::new(
            "project",
            "A relative local path cannot select a remote checkout; use an absolute device path",
        ));
    }
    let path = if given.is_absolute() {
        given
    } else {
        std::env::current_dir()
            .map_err(|error| AppError::new("project", error))?
            .join(given)
    };
    resolve(client, path)
}

pub(super) fn execute(command: WorktreeCommand) -> Result<CommandOutput, AppError> {
    let mut client = ControlClient::connect(&control_daemon_socket()?)
        .map_err(|error| AppError::new("daemon_connection", error))?;
    let model = client.read_workspace_model().map_err(protocol_error)?;
    let selector = match &command {
        WorktreeCommand::Create { project, .. }
        | WorktreeCommand::List { project }
        | WorktreeCommand::Remove { project, .. } => project.clone(),
        WorktreeCommand::Setup { command } => match command {
            SetupCommand::List { project }
            | SetupCommand::Review { project, .. }
            | SetupCommand::Resume { project, .. }
            | SetupCommand::Skip { project, .. } => project.clone(),
        },
    };
    let repository = repository(&mut client, selector, &model)?;
    let (operation, spawn) = match command {
        WorktreeCommand::Create {
            branch,
            base,
            linear,
            no_setup,
            spawn_agent,
            prompt,
            model,
            reasoning_effort,
            ..
        } => (
            Operation::Create {
                repository: repository.clone(),
                branch,
                base,
                linear,
                no_setup,
            },
            spawn_agent.map(|provider| (provider, prompt, model, reasoning_effort)),
        ),
        WorktreeCommand::List { .. } => (
            Operation::List {
                repository: repository.clone(),
            },
            None,
        ),
        WorktreeCommand::Setup { command } => (
            match command {
                SetupCommand::List { .. } => Operation::SetupIntents {
                    repository: repository.clone(),
                },
                SetupCommand::Review { path, .. } => Operation::ReviewSetup {
                    repository: repository.clone(),
                    path: checkout_path(path)?,
                },
                SetupCommand::Resume {
                    path,
                    creation,
                    expected_run,
                    fingerprint,
                    trust,
                    ..
                } => Operation::ResumeSetup {
                    repository: repository.clone(),
                    path: checkout_path(path)?,
                    creation_id: creation,
                    expected_run,
                    expected_fingerprint: fingerprint,
                    trust,
                    skip: false,
                },
                SetupCommand::Skip {
                    path,
                    creation,
                    expected_run,
                    fingerprint,
                    ..
                } => Operation::ResumeSetup {
                    repository: repository.clone(),
                    path: checkout_path(path)?,
                    creation_id: creation,
                    expected_run,
                    expected_fingerprint: fingerprint,
                    trust: false,
                    skip: true,
                },
            },
            None,
        ),
        WorktreeCommand::Remove {
            target,
            force,
            delete_branch,
            ..
        } => {
            let resolved_path = if crate::device::is_remote() {
                None
            } else {
                std::fs::canonicalize(&target)
                    .ok()
                    .map(|path| {
                        if path.to_str().is_none() {
                            Err(AppError::new(
                                "worktree_path",
                                "Refusing a worktree path that cannot be encoded exactly as UTF-8",
                            ))
                        } else {
                            Ok(path)
                        }
                    })
                    .transpose()?
            };
            (
                Operation::Remove {
                    repository: repository.clone(),
                    target,
                    force,
                    delete_branch,
                    resolved_path,
                },
                None,
            )
        }
    };
    match request(&mut client, operation)? {
        Reply::CreationFailed(failure) => Err(AppError {
            kind: "worktree_create",
            message: format!(
                "Creation {} needs review at {} ({}): {}. Inspect this operation and checkout before any retry",
                failure.operation_id,
                failure.path.display(),
                failure.state,
                failure.error
            ),
            details: Some(
                json!({"schema_version":1,"operation_id":failure.operation_id,"path":failure.path,"state":failure.state,"residual":failure.residual,"error":failure.error,"may_have_effects":failure.may_have_effects}),
            ),
        }),
        Reply::Created(created) => {
            let mut data = json!({"schema_version":1,"operation_id":created.operation_id,"path":created.path,"branch":created.branch,"worktree":created.worktree,"warnings":created.warnings,"setup":created.setup,"error":created.error});
            let failure = created.error.as_ref().or_else(|| {
                created
                    .setup
                    .as_ref()
                    .and_then(|setup| setup.error.as_ref())
            });
            if let Some(error) = failure {
                return Err(AppError {
                    kind: "worktree_create",
                    message: format!("Worktree retained at {}: {error}", created.path.display()),
                    details: Some(data),
                });
            }
            let mut human = format!(
                "Created {} at {}",
                created.branch.as_deref().unwrap_or("detached"),
                created.path.display()
            );
            for warning in &created.warnings {
                human.push_str(&format!("\nWarning: {warning}"));
            }
            if let Some((provider, prompt, model, reasoning_effort)) = spawn {
                if let Some(setup) = &created.setup {
                    let terminal = setup.terminal.as_ref().ok_or_else(|| AppError { kind: "worktree_setup", message: format!("Worktree retained at {}; its reviewed setup has no accepted terminal, so no agent was started", created.path.display()), details: Some(data.clone()) })?;
                    wait_for_setup(&mut client, terminal.id).map_err(|error| AppError {
                        kind: "worktree_setup",
                        message: format!(
                            "Worktree retained at {}; no agent was started: {error}",
                            created.path.display()
                        ),
                        details: Some(data.clone()),
                    })?;
                }
                use crate::agent_execution::{
                    Destination, Operation as AgentOperation, Reply as AgentReply, Spec, Submit,
                    Surface, Target,
                };
                let accepted = client
                    .agent(
                        Authority::from_environment(),
                        AgentOperation::Submit(Submit {
                            name: None,
                            destination: Destination::New(Spec {
                                provider,
                                account: None,
                                target: Target {
                                    repository: repository.clone(),
                                    directory: created.path.clone(),
                                    placement: Some(
                                        crate::workspace_model::views::Scope::Worktree {
                                            repository,
                                            path: created.path.clone(),
                                        },
                                    ),
                                },
                                surface: Surface::Chat,
                                model,
                                permission_mode: None,
                                reasoning_effort,
                            }),
                            prompt,
                            idempotency_key: uuid::Uuid::new_v4().to_string(),
                        }),
                    )
                    .map_err(|error| AppError {
                        kind: "worktree_agent",
                        message: format!(
                            "Worktree retained at {}; agent startup was not confirmed: {error}",
                            created.path.display()
                        ),
                        details: Some(data.clone()),
                    })?;
                let AgentReply::Accepted(accepted) = accepted else {
                    return Err(AppError {
                        kind: "protocol",
                        message: "Unexpected agent result; the worktree remains created".into(),
                        details: Some(data),
                    });
                };
                human.push_str(&format!("\nAgent {}", accepted.agent.id));
                data["agent"] = serde_json::to_value(accepted)
                    .map_err(|error| AppError::new("serialize", error))?;
            }
            Ok(CommandOutput {
                command: "worktree.create",
                data,
                human,
            })
        }
        Reply::SetupIntents { intents, warnings } => {
            let mut human = intents
                .iter()
                .map(|intent| {
                    format!(
                        "{}  creation={} run={} {}",
                        intent.path.display(),
                        intent.creation_id,
                        intent
                            .run_id
                            .map(|run| run.to_string())
                            .unwrap_or_else(|| "-".into()),
                        if intent.skip { "skipped" } else { "retained" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if human.is_empty() {
                human = "No retained setup intents".into();
            }
            for warning in &warnings {
                human.push_str(&format!("\nWarning: {warning}"));
            }
            Ok(CommandOutput {
                command: "worktree.setup.list",
                data: json!({"schema_version":1,"intents":intents,"warnings":warnings}),
                human,
            })
        }
        Reply::SetupReview(review) => {
            let mut human = format!(
                "{}\nCreation: {}\nRun: {}\n",
                review.intent.path.display(),
                review.intent.creation_id,
                review
                    .intent
                    .run_id
                    .map(|run| run.to_string())
                    .unwrap_or_else(|| "-".into())
            );
            if let Some(plan) = &review.current_plan {
                human.push_str(&format!(
                    "Fingerprint: {}\nTrusted: {}\n",
                    plan.fingerprint, review.trusted
                ));
                for pattern in &plan.copy {
                    human.push_str(&format!("Copy: {pattern}\n"));
                }
                for command in &plan.commands {
                    human.push_str(&format!("Command: {command}\n"));
                }
            } else {
                human.push_str("No current setup plan\n");
            }
            if let Some(error) = &review.current_error {
                human.push_str(&format!("Review unavailable: {error}\n"));
            }
            human.push_str("Use setup resume or setup skip with this creation, expected run and current fingerprint. An uncertain previous execution remains blocked.");
            Ok(CommandOutput {
                command: "worktree.setup.review",
                data: json!({"schema_version":1,"review":review}),
                human,
            })
        }
        Reply::SetupPrepared {
            path,
            creation_id,
            run_id,
            skip,
            setup,
            error,
        } => {
            let data = json!({"schema_version":1,"path":path,"creation_id":creation_id,"run_id":run_id,"skip":skip,"setup":setup,"error":error});
            if let Some(error) = error
                .as_ref()
                .or_else(|| setup.as_ref().and_then(|setup| setup.error.as_ref()))
            {
                return Err(AppError {
                    kind: "worktree_setup",
                    message: format!(
                        "Checkout retained at {}: {error}. Review setup again before retrying",
                        path.display()
                    ),
                    details: Some(data),
                });
            }
            Ok(CommandOutput {
                command: "worktree.setup.resume",
                data,
                human: format!(
                    "{} setup for {} (run {})",
                    if skip { "Skipped" } else { "Prepared" },
                    path.display(),
                    run_id
                ),
            })
        }
        Reply::Listed { worktrees } => {
            let rows = worktrees
                .iter()
                .map(|entry| {
                    [
                        entry.path.display().to_string(),
                        entry.branch.as_deref().unwrap_or("(detached)").into(),
                        match entry.dirty {
                            Some(true) => "dirty".into(),
                            Some(false) => "clean".into(),
                            None => "unavailable".into(),
                        },
                        entry.ahead.map(|n| format!("+{n}")).unwrap_or("-".into()),
                        entry.behind.map(|n| format!("-{n}")).unwrap_or("-".into()),
                        entry
                            .live_sessions
                            .as_ref()
                            .map(|sessions| sessions.len().to_string())
                            .unwrap_or("?".into()),
                    ]
                })
                .collect();
            let human = super::format::table(
                ["PATH", "BRANCH", "STATE", "AHEAD", "BEHIND", "SESSIONS"],
                rows,
                "No worktrees",
            );
            Ok(CommandOutput {
                command: "worktree.list",
                data: json!({"schema_version":1,"worktrees":worktrees}),
                human,
            })
        }
        Reply::Removed(removed) => {
            let data = json!({"schema_version":1,"path":removed.path,"branch_deleted":removed.branch_deleted,"rescue_branch":removed.rescue_branch,"warnings":removed.warnings,"branch_error":removed.branch_error});
            if let Some(error) = removed.branch_error {
                return Err(AppError {
                    kind: "worktree_branch",
                    message: error,
                    details: Some(data),
                });
            }
            Ok(CommandOutput {
                command: "worktree.remove",
                human: format!("Removed {}", removed.path.display()),
                data,
            })
        }
        Reply::RemovalFailed(failure) => Err(AppError {
            kind: "worktree_remove_unknown",
            message: format!(
                "Removal at {} was not confirmed: {}. Inspect its current registration and files before any retry",
                failure.path.display(),
                failure.error
            ),
            details: Some(
                json!({"schema_version":1,"path":failure.path,"rescue_branch":failure.rescue_branch,"outcome_unknown":failure.outcome_unknown}),
            ),
        }),
        Reply::ResolvedRepository { .. } => {
            Err(AppError::new("protocol", "Unexpected worktree result"))
        }
    }
}

fn wait_for_setup(client: &mut ControlClient, id: TerminalId) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if Instant::now() >= deadline {
            return Err("Setup is still running; inspect it before starting an agent".into());
        }
        let terminals = match client
            .request_with_deadline(ClientRequest::List, deadline)
            .map_err(|error| error.to_string())?
        {
            ServerResponse::Listed { terminals } => terminals,
            _ => return Err("Unexpected setup terminal inventory response".into()),
        };
        if Instant::now() >= deadline {
            return Err(
                "Setup did not complete within the waiting deadline; inspect its original terminal"
                    .into(),
            );
        }
        let terminal = terminals.into_iter().find(|terminal| terminal.session.id == id).ok_or("The original setup terminal is unavailable; inspect its card before starting an agent")?;
        let exit = match terminal.session.continuity {
            TerminalContinuity::Exited { last_exit } => Some(last_exit),
            TerminalContinuity::RelaunchAvailable { last_exit, .. } => last_exit,
            TerminalContinuity::Failed { typed_error, .. } => {
                return Err(format!("Setup terminal failed: {typed_error:?}"));
            }
            _ => None,
        };
        if let Some(exit) = exit {
            return if exit.code == Some(0) && exit.signal.is_none() {
                Ok(())
            } else {
                Err(format!(
                    "Setup failed (exit {:?}, signal {:?})",
                    exit.code, exit.signal
                ))
            };
        }
        if Instant::now() >= deadline {
            return Err("Setup is still running; inspect it before starting an agent".into());
        }
        thread::sleep(
            Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}
