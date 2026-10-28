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
    /// Read fresh deletion counts, ignored-file policy and a confirmation fingerprint.
    Inspect {
        path: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Check a leftover cagents creation lock, or explicitly unlock its proved owner.
    Lock {
        path: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        unlock: bool,
    },
    /// Show the clone-local policy; --set replaces it (an empty list is allowed).
    IgnoredDirectories {
        path: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long, num_args = 0..)]
        set: Option<Vec<String>>,
    },
    /// Delete an exact local branch; force requires separate commit-bound consent.
    DeleteBranch {
        branch: String,
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        force: bool,
        #[arg(long, requires = "force")]
        expected_oid: Option<String>,
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
        /// Bind commit-loss consent to fresh inspect output; file loss still needs --force.
        #[arg(long)]
        fingerprint: Option<String>,
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
    let known = operation.clone();
    let result = client.worktree_operation(operation).and_then(|reply| {
        let matching = match (&known, &reply) {
            (
                Operation::ResolveProject { .. } | Operation::ResolveRepository { .. },
                Reply::ResolvedRepository { .. },
            )
            | (Operation::Create { .. }, Reply::Created(_) | Reply::CreationFailed(_))
            | (Operation::List { .. }, Reply::Listed { .. })
            | (Operation::Remove { .. }, Reply::Removed(_) | Reply::RemovalFailed(_))
            | (Operation::SetupIntents { .. }, Reply::SetupIntents { .. })
            | (Operation::ReviewSetup { .. }, Reply::SetupReview(_)) => true,
            (Operation::Inspect { .. }, Reply::Inspected { .. })
            | (
                Operation::GetDisposableIgnoredDirectories { .. }
                | Operation::SetDisposableIgnoredDirectories { .. },
                Reply::DisposableIgnoredDirectories { .. },
            ) => true,
            (
                Operation::DeleteBranch {
                    branch: requested, ..
                },
                Reply::BranchDeleted { branch: actual }
                | Reply::BranchDeleteFailed { branch: actual, .. },
            ) => requested == actual,
            (
                Operation::StaleLock {
                    unlock: requested, ..
                },
                Reply::StaleLock { unlocked: actual },
            ) => requested == actual,
            (
                Operation::ResumeSetup {
                    skip: requested, ..
                },
                Reply::SetupPrepared { skip: actual, .. },
            ) => requested == actual,
            _ => false,
        };
        if matching {
            Ok(reply)
        } else {
            Err(ControlError::UnexpectedResponse {
                expected: "matching worktree operation result",
                actual: "different worktree result",
            })
        }
    });
    result.map_err(|error| {
        // Only the dedicated pre-effect admission code proves no worktree
        // mutation started. A generic remote error may follow trust/run/copy
        // publication, even though its response arrived successfully.
        let uncertain = known.mutates() && !matches!(&error, ControlError::Remote { code: ProtocolErrorCode::WorktreeNotStarted, .. });
        let cause = protocol_error(error);
        if !uncertain { return cause; }
        let verb = match &known {
            Operation::Create { .. } => "worktree.create",
            Operation::Remove { .. } => "worktree.remove",
            Operation::ResumeSetup { skip: true, .. } => "worktree.setup.skip",
            Operation::ResumeSetup { .. } => "worktree.setup.resume",
            Operation::StaleLock { unlock: true, .. } => "worktree.lock.unlock",
            Operation::SetDisposableIgnoredDirectories { .. } => "worktree.ignored-directories.set",
            Operation::DeleteBranch { .. } => "worktree.delete-branch",
            _ => unreachable!("mutating operation"),
        };
        let recovery = match &known {
            Operation::SetDisposableIgnoredDirectories { .. } => vec!["worktree ignored-directories", "worktree inspect"],
            Operation::StaleLock { .. } => vec!["worktree list", "worktree lock"],
            Operation::DeleteBranch { .. } => vec!["Inspect the exact local branch and any partial Git cleanup", "worktree list"],
            _ => vec!["worktree list", "worktree setup list", "worktree setup review"],
        };
        AppError {
            kind: "worktree_outcome_unknown",
            message: format!("{verb} did not return a confirmed outcome; effects may have committed: {}. Do not retry the mutation. Inspect current state for the same project first: {}", cause.message, recovery.join("; ")),
            details: Some(json!({"schema_version":1,"outcome_unknown":true,"may_have_effects":true,"retryable":false,"verb":verb,"request":known,"cause":{"kind":cause.kind,"message":cause.message,"details":cause.details},"recovery":recovery})),
        }
    })
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
        | WorktreeCommand::Remove { project, .. }
        | WorktreeCommand::Inspect { project, .. }
        | WorktreeCommand::Lock { project, .. }
        | WorktreeCommand::IgnoredDirectories { project, .. }
        | WorktreeCommand::DeleteBranch { project, .. } => project.clone(),
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
        WorktreeCommand::Inspect { path, .. } => (
            Operation::Inspect {
                repository: repository.clone(),
                path: checkout_path(path)?,
            },
            None,
        ),
        WorktreeCommand::Lock { path, unlock, .. } => (
            Operation::StaleLock {
                repository: repository.clone(),
                path: checkout_path(path)?,
                unlock,
            },
            None,
        ),
        WorktreeCommand::IgnoredDirectories { path, set, .. } => {
            let path = checkout_path(path)?;
            (
                match set {
                    Some(directories) => Operation::SetDisposableIgnoredDirectories {
                        repository: repository.clone(),
                        path,
                        directories,
                    },
                    None => Operation::GetDisposableIgnoredDirectories {
                        repository: repository.clone(),
                        path,
                    },
                },
                None,
            )
        }
        WorktreeCommand::DeleteBranch {
            branch,
            force,
            expected_oid,
            ..
        } => (
            Operation::DeleteBranch {
                repository: repository.clone(),
                branch,
                force,
                expected_oid,
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
            fingerprint,
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
                    expected_fingerprint: fingerprint,
                },
                None,
            )
        }
    };
    let reply = request(&mut client, operation)?;
    finish_reply(&mut client, repository, spawn, reply)
}

type AgentStart = (Provider, String, Option<String>, Option<String>);
fn finish_reply(
    client: &mut ControlClient,
    repository: PathBuf,
    spawn: Option<AgentStart>,
    reply: Reply,
) -> Result<CommandOutput, AppError> {
    match reply {
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
            let mut data = json!({"schema_version":1,"state":"created","committed":true,"operation_id":created.operation_id,"path":created.path,"branch":created.branch,"worktree":created.worktree,"warnings":created.warnings,"setup":created.setup,"error":created.error});
            let failure = created.error.as_ref().or_else(|| {
                created
                    .setup
                    .as_ref()
                    .and_then(|setup| setup.error.as_ref())
            });
            if let Some(error) = failure {
                return Err(AppError {
                    kind: "worktree_create",
                    message: format!(
                        "Worktree creation {} committed at {}; no create retry is needed: {error}",
                        created.operation_id,
                        created.path.display()
                    ),
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
                    let terminal = setup.terminal.as_ref().ok_or_else(|| AppError { kind: "worktree_setup", message: format!("Worktree creation {} remains committed at {}; its reviewed setup has no accepted terminal, so no agent was started. Do not retry creation", created.operation_id, created.path.display()), details: Some(data.clone()) })?;
                    wait_for_setup(client, terminal).map_err(|error| AppError {
                        kind: "worktree_setup",
                        message: format!(
                            "Worktree creation {} remains committed at {}; no agent was started: {error}. Do not retry creation",
                            created.operation_id, created.path.display()
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
                            "Worktree creation {} remains committed at {}; agent startup was not confirmed: {error}. Inspect existing agents before another submission; do not retry creation",
                            created.operation_id, created.path.display()
                        ),
                        details: Some(data.clone()),
                    })?;
                let AgentReply::Accepted(accepted) = accepted else {
                    return Err(AppError {
                        kind: "protocol",
                        message: format!(
                            "Unexpected agent result; worktree creation {} remains committed at {}. Inspect existing agents; do not retry creation",
                            created.operation_id,
                            created.path.display()
                        ),
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
            human.push_str("Use setup skip with --creation; skip needs no fingerprint or valid setup file. Use setup resume with --creation and the current valid plan's --fingerprint (and --trust only after review). ");
            if review.intent.run_id.is_some() {
                human.push_str("Supply this run as --expected-run for either action. ");
            } else {
                human.push_str("No prior run exists; omit --expected-run. ");
            }
            human.push_str("An uncertain previous execution remains blocked.");
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
                command: if skip {
                    "worktree.setup.skip"
                } else {
                    "worktree.setup.resume"
                },
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
            let data = json!({"schema_version":1,"state":"removed","committed":true,"path":removed.path,"branch_deleted":removed.branch_deleted,"rescue_branch":removed.rescue_branch,"warnings":removed.warnings,"branch_error":removed.branch_error});
            let mut human = format!("Removed {}", removed.path.display());
            if let Some(branch) = &removed.rescue_branch {
                human.push_str(&format!("\nPreserved detached history on {branch}"));
            }
            if let Some(branch) = &removed.branch_deleted {
                human.push_str(&format!("\nDeleted branch {branch}"));
            } else {
                human.push_str("\nBranch was not deleted");
            }
            for warning in &removed.warnings {
                human.push_str(&format!("\nWarning: {warning}"));
            }
            if let Some(error) = removed.branch_error {
                return Err(AppError {
                    kind: "worktree_branch",
                    message: format!(
                        "{human}\nBranch retained: {error}. The worktree removal is already committed"
                    ),
                    details: Some(data),
                });
            }
            Ok(CommandOutput {
                command: "worktree.remove",
                human,
                data,
            })
        }
        Reply::RemovalFailed(failure) => Err(AppError {
            kind: if failure.outcome_unknown {
                "worktree_remove_unknown"
            } else {
                "worktree_remove_refused"
            },
            message: format!(
                "Removal at {} {}: {}. {}{}",
                failure.path.display(),
                if failure.outcome_unknown {
                    "was not confirmed"
                } else {
                    "was refused; checkout retained"
                },
                failure.error,
                if failure.outcome_unknown {
                    "Inspect current registration and files before any retry"
                } else {
                    "Resolve the retained checkout's reported condition before another request"
                },
                failure
                    .rescue_branch
                    .as_ref()
                    .map(|branch| format!(". Detached history is preserved on {branch}"))
                    .unwrap_or_default()
            ),
            details: Some(
                json!({"schema_version":1,"path":failure.path,"rescue_branch":failure.rescue_branch,"outcome_unknown":failure.outcome_unknown,"may_have_effects":failure.outcome_unknown}),
            ),
        }),
        Reply::ResolvedRepository { .. } => {
            Err(AppError::new("protocol", "Unexpected worktree result"))
        }
        Reply::Inspected {
            snapshot,
            disposable_ignored_directories,
        } => {
            let snapshot: Value = serde_json::from_str(&snapshot)
                .map_err(|error| AppError::new("protocol", error))?;
            Ok(CommandOutput {
                command: "worktree.inspect",
                human: format!(
                    "{} modified, {} untracked, {} notable ignored file(s), {} unpushed and {} detached-only commit(s)\nConfirmation: {}",
                    snapshot["modified"],
                    snapshot["untracked"],
                    snapshot["notable_ignored"].as_array().map_or(0, Vec::len),
                    snapshot["unpushed_commits"],
                    snapshot["detached_only_commits"],
                    snapshot["fingerprint"].as_str().unwrap_or("unavailable")
                ),
                data: json!({"schema_version":1,"snapshot":snapshot,"disposable_ignored_directories":disposable_ignored_directories}),
            })
        }
        Reply::StaleLock { unlocked } => Ok(CommandOutput {
            command: "worktree.lock",
            human: if unlocked {
                "Unlocked the proved inactive cagents creation lock".into()
            } else {
                "The cagents creation lock is eligible for explicit recovery".into()
            },
            data: json!({"schema_version":1,"unlocked":unlocked}),
        }),
        Reply::DisposableIgnoredDirectories { directories } => Ok(CommandOutput {
            command: "worktree.ignored-directories",
            human: format!(
                "Disposable ignored directories: {}",
                if directories.is_empty() {
                    "none".into()
                } else {
                    directories.join(", ")
                }
            ),
            data: json!({"schema_version":1,"directories":directories}),
        }),
        Reply::BranchDeleted { branch } => Ok(CommandOutput {
            command: "worktree.delete-branch",
            human: format!("Deleted branch {branch}"),
            data: json!({"schema_version":1,"branch":branch,"deleted":true}),
        }),
        Reply::BranchDeleteFailed {
            branch,
            failure,
            outcome_unknown,
        } => Err(AppError {
            kind: if outcome_unknown {
                "worktree_branch_unknown"
            } else {
                "worktree_branch_refused"
            },
            message: format!(
                "Branch {branch}: {}{}",
                failure.detail,
                if outcome_unknown {
                    ". Inspect the exact branch and repository before any retry; deletion may have committed"
                } else {
                    ""
                }
            ),
            details: Some(
                json!({"schema_version":1,"branch":branch,"failure":failure,"outcome_unknown":outcome_unknown,"may_have_effects":outcome_unknown,"retryable":false}),
            ),
        }),
    }
}

fn wait_for_setup(client: &mut ControlClient, expected: &TerminalSession) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(120);
    wait_for_setup_until(client, expected, deadline, Instant::now)
}

fn wait_for_setup_until(
    client: &mut ControlClient,
    expected: &TerminalSession,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
) -> Result<(), String> {
    loop {
        if now() >= deadline {
            return Err("Setup is still running; inspect it before starting an agent".into());
        }
        let terminals = match client
            .request_with_deadline(ClientRequest::List, deadline)
            .map_err(|error| error.to_string())?
        {
            ServerResponse::Listed { terminals } => terminals,
            _ => return Err("Unexpected setup terminal inventory response".into()),
        };
        if now() >= deadline {
            return Err(
                "Setup did not complete within the waiting deadline; inspect its original terminal"
                    .into(),
            );
        }
        let terminal = terminals.into_iter().find(|terminal| terminal.session.id == expected.id).ok_or("The original setup terminal is unavailable; inspect its card before starting an agent")?;
        if terminal.session.launch != expected.launch {
            return Err("The original setup terminal source changed; no agent was started. Inspect its retained setup card".into());
        }
        let exit = match terminal.session.continuity {
            TerminalContinuity::Exited { last_exit } => Some(last_exit),
            TerminalContinuity::RelaunchAvailable { last_exit, .. } => last_exit,
            TerminalContinuity::Failed { typed_error, .. } => {
                return Err(format!("Setup terminal failed: {typed_error:?}"));
            }
            _ => None,
        };
        if let Some(exit) = exit {
            if now() >= deadline {
                return Err("Setup success arrived after the waiting deadline; inspect its original terminal before starting an agent".into());
            }
            return if exit.code == Some(0) && exit.signal.is_none() {
                Ok(())
            } else {
                Err(format!(
                    "Setup failed (exit {:?}, signal {:?})",
                    exit.code, exit.signal
                ))
            };
        }
        if now() >= deadline {
            return Err("Setup is still running; inspect it before starting an agent".into());
        }
        thread::sleep(Duration::from_millis(100).min(deadline.saturating_duration_since(now())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn connected_client(
        after: impl FnOnce(&mut UnixStream, &HostSessionProof) + Send + 'static,
    ) -> (ControlClient, thread::JoinHandle<()>) {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("worktree-cli-fixture"),
            boot_id: crate::model::BootId::new("fixture"),
            host_incarnation: crate::model::HostIncarnation::new("fixture"),
        };
        let server = thread::spawn(move || {
            peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let connected: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
            write_frame(
                &mut peer,
                &ServerEnvelope {
                    version: PROTOCOL_VERSION,
                    request_id: Some(connected.request_id),
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
            after(&mut peer, &proof);
        });
        (ControlClient::from_stream(stream).unwrap(), server)
    }
    #[test]
    fn lost_mutation_response_is_unknown_and_never_replayed_but_remote_refusal_stays_known() {
        for (response, wrong_reply) in [
            (None, false),
            (Some(ProtocolErrorCode::WorktreeNotStarted), false),
            (Some(ProtocolErrorCode::InvalidRequest), false),
            (None, true),
        ] {
            let (mut client, server) = connected_client(move |peer, proof| {
                let received: ClientEnvelope = read_frame(peer).unwrap().unwrap();
                assert!(
                    matches!(received.request, ClientRequest::Worktree { operation: Operation::Create { branch, .. }, .. } if branch == "topic")
                );
                if wrong_reply {
                    write_frame(
                        peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(received.request_id),
                            proof: proof.clone(),
                            message: ServerMessage::Response {
                                response: ServerResponse::Worktree(Reply::Listed {
                                    worktrees: Vec::new(),
                                }),
                            },
                        },
                    )
                    .unwrap();
                    assert!(read_frame::<ClientEnvelope>(peer).unwrap().is_none());
                } else if let Some(code) = response {
                    write_frame(
                        peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(received.request_id),
                            proof: proof.clone(),
                            message: ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                                code,
                                "authoritative remote response",
                                false,
                            )),
                        },
                    )
                    .unwrap();
                    assert!(read_frame::<ClientEnvelope>(peer).unwrap().is_none());
                }
                // Otherwise EOF immediately after accepting exactly one request.
            });
            let error = request(
                &mut client,
                Operation::Create {
                    repository: "/fixture/project.git".into(),
                    branch: "topic".into(),
                    base: Some("main".into()),
                    linear: None,
                    no_setup: true,
                },
            )
            .unwrap_err();
            if response == Some(ProtocolErrorCode::WorktreeNotStarted) {
                assert_ne!(error.kind, "worktree_outcome_unknown");
                assert_eq!(error.details.as_ref().unwrap()["retryable"], false);
                assert!(error.message.contains("authoritative remote response"));
            } else {
                assert_eq!(error.kind, "worktree_outcome_unknown");
                let data = error.details.as_ref().unwrap();
                assert_eq!(data["outcome_unknown"], true);
                assert_eq!(data["may_have_effects"], true);
                assert_eq!(data["retryable"], false);
                assert_eq!(data["request"]["Create"]["branch"], "topic");
                assert!(
                    data.get("operation_id").is_none(),
                    "missing response must not fabricate a durable ID"
                );
                assert!(error.message.contains("Do not retry"));
            }
            drop(client);
            server.join().unwrap();
        }
    }
    #[test]
    fn formatted_retained_outcomes_keep_committed_ids_rescue_refs_and_exact_verbs() {
        use crate::worktree_control::{Created, RemovalFailure, Removed};
        let (mut client, server) = connected_client(|peer, _| {
            assert!(read_frame::<ClientEnvelope>(peer).unwrap().is_none())
        });
        let repository = PathBuf::from("/fixture/project.git");
        let path = PathBuf::from("/fixture/topic");
        let creation = crate::model::OperationId::new();
        let error = finish_reply(
            &mut client,
            repository.clone(),
            None,
            Reply::Created(Created {
                operation_id: creation,
                path: path.clone(),
                branch: Some("topic".into()),
                worktree: None,
                warnings: Vec::new(),
                setup: None,
                error: Some("status unavailable".into()),
            }),
        )
        .err()
        .unwrap();
        assert!(error.message.contains(&creation.to_string()));
        assert!(error.message.contains("committed") && error.message.contains("/fixture/topic"));
        assert_eq!(error.details.as_ref().unwrap()["state"], "created");
        assert_eq!(error.details.as_ref().unwrap()["committed"], true);
        let error = finish_reply(
            &mut client,
            repository.clone(),
            Some((Provider::Claude, "fixture only".into(), None, None)),
            Reply::Created(Created {
                operation_id: creation,
                path: path.clone(),
                branch: Some("topic".into()),
                worktree: None,
                warnings: Vec::new(),
                error: None,
                setup: Some(crate::worktree_control::Setup {
                    command: "fixture setup".into(),
                    run_id: crate::model::OperationId::new(),
                    terminal: None,
                    attempted_terminal: None,
                    error: None,
                }),
            }),
        )
        .err()
        .unwrap();
        assert!(
            error.message.contains(&creation.to_string())
                && error.message.contains("/fixture/topic")
                && error.message.contains("no agent was started")
                && error.message.contains("Do not retry creation")
        );
        assert_eq!(error.details.as_ref().unwrap()["committed"], true);
        for branch_error in [None, Some("unmerged branch retained".to_owned())] {
            let outcome = finish_reply(
                &mut client,
                repository.clone(),
                None,
                Reply::Removed(Removed {
                    path: path.clone(),
                    branch_deleted: None,
                    rescue_branch: Some("rescue/topic".into()),
                    warnings: vec!["receipt retirement needs inspection".into()],
                    branch_error: branch_error.clone(),
                }),
            );
            let human = if branch_error.is_some() {
                let error = outcome.err().unwrap();
                assert_eq!(error.details.as_ref().unwrap()["committed"], true);
                error.message
            } else {
                outcome.ok().unwrap().human
            };
            assert!(
                human.contains("rescue/topic")
                    && human.contains("receipt retirement needs inspection")
                    && human.contains("not deleted")
            );
        }
        for unknown in [false, true] {
            let error = finish_reply(
                &mut client,
                repository.clone(),
                None,
                Reply::RemovalFailed(RemovalFailure {
                    path: path.clone(),
                    rescue_branch: None,
                    error: "original owner remains".into(),
                    outcome_unknown: unknown,
                }),
            )
            .err()
            .unwrap();
            assert_eq!(
                error.kind,
                if unknown {
                    "worktree_remove_unknown"
                } else {
                    "worktree_remove_refused"
                }
            );
            assert_eq!(error.details.unwrap()["outcome_unknown"], unknown);
        }
        let result = finish_reply(
            &mut client,
            repository,
            None,
            Reply::SetupPrepared {
                path,
                creation_id: creation,
                run_id: crate::model::OperationId::new(),
                skip: true,
                setup: None,
                error: None,
            },
        )
        .ok()
        .unwrap();
        assert_eq!(result.command, "worktree.setup.skip");
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn setup_wait_rejects_late_or_replaced_success_without_starting_an_agent() {
        for (expires_at, replaced) in [(2, false), (3, false), (usize::MAX, true)] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            let proof = HostSessionProof {
                host_session_id: crate::model::HostSessionId::new("worktree-wait"),
                boot_id: crate::model::BootId::new("fixture"),
                host_incarnation: crate::model::HostIncarnation::new("fixture"),
            };
            let id = TerminalId::new();
            let mut descriptor = TerminalDescriptor::relaunchable(
                id,
                LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: Vec::new(),
                    working_directory: "/".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                None,
            );
            descriptor.session.continuity = TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            };
            let expected = descriptor.session.clone();
            if replaced {
                descriptor.session.launch.program = "/different-setup-source".into();
            }
            let server = thread::spawn(move || {
                peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let connected: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(connected.request_id),
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
                let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(request.request, ClientRequest::List));
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(request.request_id),
                        proof,
                        message: ServerMessage::Response {
                            response: ServerResponse::Listed {
                                terminals: vec![descriptor],
                            },
                        },
                    },
                )
                .unwrap();
                assert!(
                    read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                    "expired waiting path must end without submitting another request"
                );
            });
            let mut client = ControlClient::from_stream(stream).unwrap();
            let before = Instant::now();
            let deadline = before + Duration::from_secs(5);
            let mut observations = 0;
            let error = wait_for_setup_until(&mut client, &expected, deadline, || {
                observations += 1;
                if observations < expires_at {
                    before
                } else {
                    deadline
                }
            })
            .unwrap_err();
            if replaced {
                assert!(error.contains("source changed"), "{error}");
                assert_eq!(observations, 2);
            } else {
                assert!(error.contains("waiting deadline"), "{error}");
                assert_eq!(
                    observations, expires_at,
                    "the same original deadline is checked after RPC and before accepting success"
                );
            }
            drop(client);
            server.join().unwrap();
        }
    }
    #[test]
    fn restored_mutations_preserve_unknown_outcomes_and_exact_branch_reply_correlation() {
        let path = PathBuf::from("/fixture/checkout");
        let operations = vec![
            Operation::SetDisposableIgnoredDirectories {
                repository: path.clone(),
                path: path.clone(),
                directories: vec!["cache".into()],
            },
            Operation::StaleLock {
                repository: path.clone(),
                path: path.clone(),
                unlock: true,
            },
            Operation::DeleteBranch {
                repository: path.clone(),
                branch: "topic".into(),
                force: true,
                expected_oid: Some("a".repeat(40)),
            },
        ];
        for operation in operations {
            assert!(operation.mutates());
            for known_no_start in [false, true] {
                let expected = operation.clone();
                let (mut client, server) = connected_client(move |peer, proof| {
                    let received: ClientEnvelope = read_frame(peer).unwrap().unwrap();
                    let ClientRequest::Worktree { operation, .. } = received.request else {
                        panic!("wrong request");
                    };
                    assert_eq!(operation, expected);
                    let message = if known_no_start {
                        ServerMessage::Error(crate::terminal_ipc::ProtocolError::new(
                            ProtocolErrorCode::WorktreeNotStarted,
                            "pre-service refusal",
                            false,
                        ))
                    } else {
                        // An unrelated branch cannot confirm this operation,
                        // even though its inner result has a valid wire shape.
                        ServerMessage::Response {
                            response: ServerResponse::Worktree(Reply::BranchDeleted {
                                branch: "another-branch".into(),
                            }),
                        }
                    };
                    write_frame(
                        peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(received.request_id),
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .unwrap();
                    assert!(
                        read_frame::<ClientEnvelope>(peer).unwrap().is_none(),
                        "no automatic mutation retry"
                    );
                });
                let error = request(&mut client, operation.clone()).unwrap_err();
                if known_no_start {
                    assert_ne!(error.kind, "worktree_outcome_unknown");
                } else {
                    assert_eq!(error.kind, "worktree_outcome_unknown");
                    let details = error.details.unwrap();
                    assert_eq!(
                        details["request"],
                        serde_json::to_value(&operation).unwrap()
                    );
                    assert_eq!(details["retryable"], false);
                    assert_eq!(details["may_have_effects"], true);
                }
                drop(client);
                server.join().unwrap();
            }
        }
        for operation in [
            Operation::Inspect {
                repository: path.clone(),
                path: path.clone(),
            },
            Operation::GetDisposableIgnoredDirectories {
                repository: path.clone(),
                path: path.clone(),
            },
            Operation::StaleLock {
                repository: path.clone(),
                path,
                unlock: false,
            },
        ] {
            assert!(!operation.mutates());
        }
    }
}
