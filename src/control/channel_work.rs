//! Explicit channel work intent and exact-request recovery after an uncertain reply.
use super::*;
use crate::{
    channel_work::{Change, Home, Mutation, Page, Role, Snapshot, Target},
    channel_work_api::{Context, Failure, Request, Response},
    github_identity::PullRequestKey,
};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt};
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_BYTES: u64 = 64 * 1024;

#[derive(Debug, Subcommand)]
pub enum WorkCommand {
    /// Show the authenticated session's actual channel, or an explicit user selection.
    Context {
        /// Channel UUID; omit to use your authenticated session's channel.
        #[arg(long)]
        channel: Option<ChannelId>,
    },
    /// Show declared work, related reviews and recent history in a channel.
    Show {
        /// Channel UUID; omit to use your authenticated session's channel.
        #[arg(long)]
        channel: Option<ChannelId>,
        /// Continue history with the previous response's next_history cursor.
        #[arg(long)]
        before: Option<u64>,
        /// Continue current entries with the previous response's next_current cursor.
        #[arg(long)]
        current_before: Option<u64>,
        /// Maximum entries per current or history page (1-100).
        #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
    /// Declare a PR relationship; this does not claim verified filesystem writes.
    Track {
        /// HTTPS pull request URL: https://HOST/OWNER/REPO/pull/NUMBER.
        #[arg(value_parser = parse_pull_request)]
        url: Target,
        /// Relationship: contribute, steward, review, or reference.
        #[arg(value_parser = parse_role)]
        role: Role,
        /// Channel UUID; omit to use your authenticated session's channel.
        #[arg(long)]
        channel: Option<ChannelId>,
        /// Save the exact safe request before sending it. The path must not exist.
        #[arg(long)]
        save_request: Option<PathBuf>,
    },
    /// End your participation, or all participation when invoked explicitly by the user.
    End {
        /// HTTPS pull request URL: https://HOST/OWNER/REPO/pull/NUMBER.
        #[arg(value_parser = parse_pull_request)]
        url: Target,
        /// Channel UUID; omit to use your authenticated session's channel.
        #[arg(long)]
        channel: Option<ChannelId>,
        /// Save the exact safe request before sending it. The path must not exist.
        #[arg(long)]
        save_request: Option<PathBuf>,
    },
    /// Mark a relationship as not belonging to this channel (explicit user only).
    Dismiss {
        /// HTTPS pull request URL: https://HOST/OWNER/REPO/pull/NUMBER.
        #[arg(value_parser = parse_pull_request)]
        url: Target,
        /// Channel UUID; omit to use your authenticated session's channel.
        #[arg(long)]
        channel: Option<ChannelId>,
        /// Save the exact safe request before sending it. The path must not exist.
        #[arg(long)]
        save_request: Option<PathBuf>,
    },
    /// Inspect an original receipt. Missing means unknown, not proof of no effect.
    Result {
        /// Registry UUID captured for the original operation.
        #[arg(long, value_parser = parse_uuid)]
        registry: Uuid,
        /// Channel UUID captured for the original operation.
        #[arg(long)]
        channel: ChannelId,
        /// Confirmed channel creation ordinal captured for the original operation.
        #[arg(long, value_parser = parse_ordinal)]
        ordinal: u32,
        /// Original mutation UUID to inspect.
        #[arg(long, value_parser = parse_uuid)]
        operation: Uuid,
    },
    /// Explicitly retry an exact saved mutation, preserving its UUID, CAS and caller.
    Apply {
        /// Path to the exact request file saved with --save-request.
        request: PathBuf,
    },
}

fn parse_uuid(value: &str) -> Result<Uuid, String> {
    let id = value
        .parse::<Uuid>()
        .map_err(|_| "Expected a UUID".to_owned())?;
    if id.is_nil() {
        return Err("The UUID must not be nil".into());
    }
    Ok(id)
}

fn parse_ordinal(value: &str) -> Result<u32, String> {
    let ordinal = value
        .parse::<u32>()
        .map_err(|_| "Expected a channel creation ordinal".to_owned())?;
    if ordinal == crate::workspace_model::PENDING_ORDINAL {
        return Err("The channel must have a confirmed creation ordinal".into());
    }
    Ok(ordinal)
}

fn parse_role(value: &str) -> Result<Role, String> {
    match value {
        "contribute" => Ok(Role::Contribute),
        "steward" => Ok(Role::Steward),
        "review" => Ok(Role::Review),
        "reference" => Ok(Role::Reference),
        _ => Err("Use contribute, steward, review, or reference".into()),
    }
}

#[cfg(test)]
use crate::github_identity::pull_request_path_segment as path_segment;

fn parse_pull_request(value: &str) -> Result<Target, String> {
    Target::PullRequest(PullRequestKey::from_url(value)?)
        .canonicalized()
        .map_err(|_| "The PR identity is invalid".into())
}

enum Prepared {
    Context(Option<ChannelId>),
    Show(Option<ChannelId>, Page),
    Change {
        channel: Option<ChannelId>,
        change: Change,
        save: Option<PathBuf>,
    },
    Receipt(Home, Uuid),
    Apply(Request),
}

impl Prepared {
    fn new(command: WorkCommand) -> Result<Self, AppError> {
        Ok(match command {
            WorkCommand::Context { channel } => Self::Context(channel),
            WorkCommand::Show {
                channel,
                before,
                current_before,
                limit,
            } => {
                let page = Page {
                    current_before,
                    before,
                    limit,
                };
                page.validate().map_err(invalid)?;
                Self::Show(channel, page)
            }
            WorkCommand::Track {
                url,
                role,
                channel,
                save_request,
            } => Self::Change {
                channel,
                change: Change::Claim {
                    target: url.canonicalized().map_err(invalid)?,
                    role,
                },
                save: save_request,
            },
            WorkCommand::End {
                url,
                channel,
                save_request,
            } => Self::Change {
                channel,
                change: Change::End {
                    target: url.canonicalized().map_err(invalid)?,
                },
                save: save_request,
            },
            WorkCommand::Dismiss {
                url,
                channel,
                save_request,
            } => Self::Change {
                channel,
                change: Change::Suppress {
                    target: url.canonicalized().map_err(invalid)?,
                },
                save: save_request,
            },
            WorkCommand::Result {
                registry,
                channel,
                ordinal,
                operation,
            } => {
                let home = Home {
                    registry,
                    channel,
                    creation_ordinal: ordinal,
                };
                home.validate().map_err(invalid)?;
                if operation.is_nil() {
                    return Err(invalid("The operation UUID must not be nil"));
                }
                Self::Receipt(home, operation)
            }
            WorkCommand::Apply { request } => Self::Apply(load_request(&request)?),
        })
    }
}

fn invalid(reason: impl std::fmt::Display) -> AppError {
    AppError::new("channel_work_input", reason)
}
fn contract() -> AppError {
    AppError::new(
        "channel_work_protocol",
        "The daemon returned inconsistent channel work",
    )
}

pub(super) fn execute(command: WorkCommand) -> Result<CommandOutput, AppError> {
    execute_with_context(command, &ServiceRequestContext::capture())
}

pub(super) fn execute_with_context(
    command: WorkCommand,
    context: &ServiceRequestContext,
) -> Result<CommandOutput, AppError> {
    context.admit_remote()?;
    // Saved input, IDs and page bounds are validated before starting a daemon.
    let prepared = Prepared::new(command)?;
    let mut client = context.connect()?;
    run(prepared, &mut |request| {
        client.channel_work(context.authority.clone(), request, REQUEST_TIMEOUT)
    })
}

fn context<F>(channel: Option<ChannelId>, send: &mut F) -> Result<Context, AppError>
where
    F: FnMut(Request) -> Result<Response, ControlError>,
{
    match send(Request::Context { channel }).map_err(protocol_error)? {
        Response::Context(context) => {
            context.home.validate().map_err(|_| contract())?;
            if channel.is_some_and(|id| id != context.home.channel) {
                return Err(contract());
            }
            if context.caller.as_ref().is_some_and(|c| {
                c.session.as_uuid().is_nil()
                    || c.placement_epoch == 0
                    || c.placement_epoch > i64::MAX as u64
                    || c.card.as_uuid().is_nil()
                    || c.run.is_some_and(|id| id.as_uuid().is_nil())
            }) {
                return Err(contract());
            }
            Ok(context)
        }
        Response::Failed(Failure::Refused(reason)) => {
            Err(AppError::new("channel_work_refused", reason))
        }
        _ => Err(contract()),
    }
}

fn read<F>(home: Home, page: Page, send: &mut F) -> Result<Snapshot, AppError>
where
    F: FnMut(Request) -> Result<Response, ControlError>,
{
    match send(Request::Read { home, page }).map_err(protocol_error)? {
        Response::Snapshot(snapshot) if snapshot.home == home => {
            snapshot.validate().map_err(|_| contract())?;
            if snapshot.entries.len() > usize::from(page.limit)
                || snapshot.history.len() > usize::from(page.limit)
                || snapshot
                    .next_current
                    .zip(page.current_before)
                    .is_some_and(|(next, before)| next >= before)
                || snapshot
                    .next_history
                    .zip(page.before)
                    .is_some_and(|(next, before)| next >= before)
            {
                return Err(contract());
            }
            Ok(snapshot)
        }
        Response::Failed(Failure::Refused(reason)) => {
            Err(AppError::new("channel_work_refused", reason))
        }
        _ => Err(contract()),
    }
}

fn run<F>(prepared: Prepared, send: &mut F) -> Result<CommandOutput, AppError>
where
    F: FnMut(Request) -> Result<Response, ControlError>,
{
    match prepared {
        Prepared::Context(channel) => {
            let value = context(channel, send)?;
            Ok(CommandOutput {
                command: "channel work context",
                human: format!(
                    "{}\nRegistry {} · channel {} · incarnation {}",
                    format::plain(&value.title),
                    value.home.registry,
                    value.home.channel,
                    value.home.creation_ordinal
                ),
                data: json!({"context":value}),
            })
        }
        Prepared::Show(channel, page) => {
            let selected = context(channel, send)?;
            let snapshot = read(selected.home, page, send)?;
            let mut lines = vec![format!(
                "{} · {} declared ongoing work shown on this page",
                format::plain(&selected.title),
                snapshot
                    .entries
                    .iter()
                    .filter(|e| e.is_ongoing_work())
                    .count()
            )];
            for entry in snapshot.entries.iter().chain(&snapshot.history) {
                let target = match &entry.target {
                    Target::PullRequest(key) => {
                        format!("{} #{}", key.repository.gh_selector(), key.number)
                    }
                    Target::Local(id) => format!("Local context {id}"),
                };
                lines.push(format!(
                    "{}\t{}\t{}",
                    format::plain(&target),
                    match entry.bucket() {
                        crate::channel_work::Bucket::Ongoing => "Ongoing relation",
                        crate::channel_work::Bucket::NeedsConfirmation => "Needs confirmation",
                        crate::channel_work::Bucket::History => "History",
                    },
                    entry.role().map_or("No current declaration", Role::label)
                ));
            }
            if let Some(cursor) = snapshot.next_current {
                lines.push(format!("More current relations: --current-before {cursor}"));
            }
            if let Some(cursor) = snapshot.next_history {
                lines.push(format!("More history: --before {cursor}"));
            }
            Ok(CommandOutput {
                command: "channel work show",
                data: json!({"snapshot":snapshot}),
                human: lines.join("\n"),
            })
        }
        Prepared::Change {
            channel,
            change,
            save,
        } => {
            let selected = context(channel, send)?;
            let revision = target_revision(selected.home, change.target(), send)?;
            let request = Request::Mutate {
                mutation: Mutation {
                    home: selected.home,
                    expected_revision: revision.unwrap_or(0),
                    operation: Uuid::new_v4(),
                    change,
                },
                caller: selected.caller,
            };
            if let Some(path) = save {
                save_request(&path, &request)?;
            }
            submit(request, send)
        }
        Prepared::Apply(request) => submit(request, send),
        Prepared::Receipt(home, operation) => {
            match send(Request::Receipt { home, operation }).map_err(protocol_error)? {
                Response::Receipt(Some(receipt))
                    if receipt.home == home && receipt.operation == operation =>
                {
                    Ok(CommandOutput {
                        command: "channel work result",
                        data: json!({"outcome":"succeeded","receipt":receipt}),
                        human: format!(
                            "Original operation {operation} is recorded at entry revision {}. Inspect current work separately; this receipt does not resume it.",
                            receipt.revision
                        ),
                    })
                }
                Response::Receipt(None) => Ok(CommandOutput {
                    command: "channel work result",
                    data: json!({"outcome":"pending","home":home,"operation":operation,"receipt":null}),
                    human: format!(
                        "Operation {operation} is still unknown. A missing receipt does not prove no effect. Inspect again, or explicitly apply the same saved request; do not generate a replacement UUID."
                    ),
                }),
                Response::Failed(Failure::Refused(reason)) => {
                    Err(AppError::new("channel_work_refused", reason))
                }
                _ => Err(contract()),
            }
        }
    }
}

fn target_revision<F>(home: Home, target: &Target, send: &mut F) -> Result<Option<u64>, AppError>
where
    F: FnMut(Request) -> Result<Response, ControlError>,
{
    match send(Request::ReadTarget {
        home,
        target: target.clone(),
    })
    .map_err(protocol_error)?
    {
        Response::Target(snapshot) if snapshot.home == home && &snapshot.target == target => {
            snapshot.validate().map_err(|_| contract())?;
            if snapshot.lifecycle != crate::channel_work::HomeLifecycle::Active {
                return Err(AppError::new(
                    "channel_work_inactive",
                    "This channel work home is archived or deleted; no mutation was sent",
                ));
            }
            Ok(snapshot.entry.map(|entry| entry.revision))
        }
        Response::Failed(Failure::Refused(reason)) => {
            Err(AppError::new("channel_work_refused", reason))
        }
        _ => Err(contract()),
    }
}

fn submit<F>(request: Request, send: &mut F) -> Result<CommandOutput, AppError>
where
    F: FnMut(Request) -> Result<Response, ControlError>,
{
    validate_saved(&request)?;
    // No authority token, recipe, environment, or arbitrary context is in this DTO.
    eprintln!(
        "Captured channel work request: {}",
        serde_json::to_string(&request).map_err(invalid)?
    );
    let Request::Mutate { mutation, .. } = &request else {
        return Err(invalid("Expected a mutation request"));
    };
    let result = send(request.clone());
    match result {
        Ok(Response::Applied(reply)) if valid_reply(mutation, &reply) => Ok(CommandOutput {
            command: "channel work apply",
            data: json!({"outcome":"succeeded","request":request,"reply":reply}),
            human: format!(
                "{} recorded for operation {} at entry revision {}. This is intent, not proof of filesystem writes.",
                match mutation.change {
                    Change::Claim { role, .. } => role.label(),
                    Change::End { .. } => "End participation",
                    Change::Suppress { .. } => "Not this channel",
                },
                mutation.operation,
                reply.receipt.revision
            ),
        }),
        Ok(Response::Failed(Failure::Refused(reason))) => Ok(CommandOutput {
            command: "channel work apply",
            data: json!({"outcome":"failed","request":request,"reason":reason}),
            human: format!(
                "Refused: {reason}. This attempt made no change; an earlier attempt with this UUID may already be recorded."
            ),
        }),
        _ => Ok(uncertain(&request)),
    }
}

fn valid_reply(mutation: &Mutation, reply: &crate::channel_work::Reply) -> bool {
    reply.receipt.home == mutation.home
        && reply.receipt.operation == mutation.operation
        && mutation
            .expected_revision
            .checked_add(u64::from(reply.receipt.changed))
            == Some(reply.receipt.revision)
        && reply.snapshot.home == mutation.home
        && reply.receipt.revision <= reply.snapshot.revision
        && reply.snapshot.validate().is_ok()
}

fn uncertain(request: &Request) -> CommandOutput {
    let Request::Mutate { mutation, .. } = request else {
        unreachable!()
    };
    let result = format!(
        "cagents channel work result --registry {} --channel {} --ordinal {} --operation {}",
        mutation.home.registry,
        mutation.home.channel,
        mutation.home.creation_ordinal,
        mutation.operation
    );
    CommandOutput {
        command: "channel work apply",
        data: json!({"outcome":"pending","home":mutation.home,"operation":mutation.operation,"request":request,"inspect":result}),
        human: format!(
            "Operation {} has an unknown result; the request may have committed.\n{result}\nMissing does not mean no effect. Explicitly apply the exact saved request to retry the same UUID, never a replacement operation.",
            mutation.operation
        ),
    }
}

fn validate_saved(request: &Request) -> Result<(), AppError> {
    let Request::Mutate { mutation, caller } = request else {
        return Err(invalid("Only an exact saved mutation can be applied"));
    };
    if mutation.canonicalized().map_err(invalid)? != *mutation {
        return Err(invalid(
            "The saved request must contain its original canonical identity",
        ));
    }
    if caller.as_ref().is_some_and(|c| {
        c.session.as_uuid().is_nil()
            || c.placement_epoch == 0
            || c.placement_epoch > i64::MAX as u64
            || c.card.as_uuid().is_nil()
            || c.run.is_some_and(|id| id.as_uuid().is_nil())
    }) {
        return Err(invalid("The saved caller comparison values are invalid"));
    }
    Ok(())
}

fn save_request(path: &Path, request: &Request) -> Result<(), AppError> {
    validate_saved(request)?;
    let bytes = serde_json::to_vec_pretty(request).map_err(invalid)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| invalid("The request file could not be created; use a new task-owned path"))?;
    file.write_all(&bytes)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|_| invalid("The request could not be saved completely; no mutation was sent"))
}

fn load_request(path: &Path) -> Result<Request, AppError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| invalid("The saved request could not be opened"))?;
    if !file
        .metadata()
        .map_err(|_| invalid("The saved request could not be inspected"))?
        .is_file()
    {
        return Err(invalid(
            "The saved request must be a regular file, not a pipe or directory",
        ));
    }
    let mut bytes = Vec::new();
    file.take(REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("The saved request could not be read"))?;
    if bytes.len() as u64 > REQUEST_BYTES {
        return Err(invalid("The saved request exceeds 64 KiB"));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("The saved request is not valid request JSON"))?;
    if value
        .get("Mutate")
        .and_then(|request| request.get("caller"))
        .and_then(Value::as_object)
        .is_some_and(|caller| {
            caller.contains_key("view_revision") && !caller.contains_key("placement_epoch")
        })
    {
        return Err(invalid(
            "This saved actor request uses an older placement format. Its file has been kept unchanged; inspect the original operation with channel work result before creating new intent",
        ));
    }
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("The saved request has an unsupported shape"))?;
    if serde_json::to_value(&request).map_err(invalid)? != value {
        return Err(invalid("The saved request has unknown or missing fields"));
    }
    validate_saved(&request)?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_work::{
        Entry, HomeLifecycle, HomeState, Origin, Receipt, TargetSnapshot, reduce_entry,
    };
    use crate::channel_work_api::Caller;
    use clap::Parser;
    use std::os::unix::fs::PermissionsExt;

    #[derive(Parser)]
    struct Grammar {
        #[command(subcommand)]
        command: WorkCommand,
    }
    fn home() -> Home {
        Home {
            registry: Uuid::from_u128(1),
            channel: ChannelId::from_uuid(Uuid::from_u128(2)),
            creation_ordinal: 3,
        }
    }
    fn context_value() -> Context {
        Context {
            home: home(),
            title: "Work".into(),
            caller: Some(Caller {
                session: TerminalId::from_uuid(Uuid::from_u128(4)),
                card: crate::model::CardId::from_uuid(Uuid::from_u128(5)),
                placement_epoch: 6,
                run: None,
            }),
        }
    }
    fn snapshot_value() -> Snapshot {
        Snapshot {
            home: home(),
            lifecycle: crate::channel_work::HomeLifecycle::Active,
            revision: 0,
            entries: vec![],
            history: vec![],
            next_current: None,
            next_history: None,
        }
    }
    fn target_value(target: Target, entry: Option<Entry>) -> TargetSnapshot {
        TargetSnapshot {
            home: home(),
            lifecycle: HomeLifecycle::Active,
            revision: 100,
            target,
            entry,
        }
    }
    fn request() -> Request {
        Request::Mutate {
            mutation: Mutation {
                home: home(),
                expected_revision: 0,
                operation: Uuid::from_u128(7),
                change: Change::Claim {
                    target: parse_pull_request("https://github.com/owner/repo/pull/1").unwrap(),
                    role: Role::Steward,
                },
            },
            caller: context_value().caller,
        }
    }

    #[test]
    fn command_grammar_requires_explicit_recovery_home_and_parses_full_pr_identity() {
        assert!(
            Grammar::try_parse_from(["work", "track", "https://github.com/a/b/pull/1"]).is_err()
        );
        assert!(
            Grammar::try_parse_from(["work", "track", "https://github.com/a/b/pull/1", "review"])
                .is_ok()
        );
        assert!(Grammar::try_parse_from(["work", "context"]).is_ok());
        assert!(
            Grammar::try_parse_from(["work", "result", "--channel", &home().channel.to_string()])
                .is_err()
        );
        assert!(Grammar::try_parse_from(["work", "show", "--limit", "101"]).is_err());
        let show =
            Grammar::try_parse_from(["work", "show", "--before", "20", "--current-before", "30"])
                .unwrap();
        let Prepared::Show(_, page) = Prepared::new(show.command).unwrap() else {
            panic!("show")
        };
        assert_eq!(page.before, Some(20));
        assert_eq!(page.current_before, Some(30));
        for url in [
            "http://github.com/a/b/pull/1",
            "https://token@github.com/a/b/pull/1",
            "https://github.com/a/b/pull/0",
            "https://github.com/a/b/pull/1?x=1",
            "https://github.com/a/b/pull/1#x",
            "https://github.com/a%2Fb/c/pull/1",
        ] {
            assert!(parse_pull_request(url).is_err(), "{url}");
        }
        let target =
            parse_pull_request("https://enterprise.example:8443/Team%3Fname/Repo%23name/pull/12")
                .unwrap();
        assert_eq!(
            target.pull_request_url().unwrap().unwrap().as_str(),
            "https://enterprise.example:8443/team%3Fname/repo%23name/pull/12"
        );
        assert_ne!(
            target,
            parse_pull_request("https://enterprise.example:8443/fork/Repo%23name/pull/12").unwrap()
        );
        assert_eq!(path_segment("a+b").unwrap(), "a+b");
        assert!(
            Grammar::try_parse_from(["work", "track", "https://github.com/a/b/pull/1", "owner"])
                .is_err()
        );
    }

    #[test]
    fn lost_reply_preserves_single_captured_request_and_writes_safe_exclusive_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("request.json");
        let mut sent = Vec::new();
        let output = run(
            Prepared::Change {
                channel: None,
                change: Change::Claim {
                    target: parse_pull_request("https://github.com/a/b/pull/1").unwrap(),
                    role: Role::Review,
                },
                save: Some(path.clone()),
            },
            &mut |r| {
                sent.push(r.clone());
                Ok(match r {
                    Request::Context { .. } => Response::Context(context_value()),
                    Request::ReadTarget { target, .. } => {
                        Response::Target(target_value(target, None))
                    }
                    Request::Mutate { mutation, caller } => {
                        assert_eq!(
                            mutation.expected_revision, 0,
                            "only an exact absent target creates revision zero"
                        );
                        assert_eq!(caller, context_value().caller);
                        return Err(ControlError::ConnectionClosed);
                    }
                    _ => panic!("unexpected request"),
                })
            },
        )
        .unwrap();
        assert_eq!(sent.len(), 3);
        assert_eq!(output.data["outcome"], "pending");
        assert_eq!(load_request(&path).unwrap(), sent[2]);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(save_request(&path, &request()).is_err());
        assert_eq!(
            load_request(&path).unwrap(),
            sent[2],
            "existing safe request is not replaced"
        );
        assert_eq!(
            output.data["request"],
            serde_json::to_value(&sent[2]).unwrap()
        );
        assert!(output.human.contains("Missing does not mean no effect"));
    }

    #[test]
    fn explicit_apply_does_not_rebind_caller_or_generate_another_operation() {
        let original = request();
        let mut calls = Vec::new();
        let output = run(Prepared::Apply(original.clone()), &mut |r| {
            calls.push(r);
            Ok(Response::Failed(Failure::Uncertain))
        })
        .unwrap();
        assert_eq!(calls, [original.clone()]);
        assert_eq!(
            output.data["request"],
            serde_json::to_value(original).unwrap()
        );
        assert_eq!(output.data["outcome"], "pending");
        let mut count = 0;
        let output = run(Prepared::Receipt(home(), Uuid::from_u128(7)), &mut |_| {
            count += 1;
            Ok(Response::Receipt(None))
        })
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(output.data["outcome"], "pending");
        assert!(output.human.contains("does not prove no effect"));
    }

    #[test]
    fn wrong_scope_target_or_unavailable_lookup_stops_before_any_mutation() {
        let target = parse_pull_request("https://github.com/a/b/pull/1").unwrap();
        for case in 0..5 {
            let mut count = 0;
            let result = run(
                Prepared::Change {
                    channel: Some(home().channel),
                    change: Change::End {
                        target: target.clone(),
                    },
                    save: None,
                },
                &mut |request| {
                    count += 1;
                    Ok(match request {
                        Request::Context { .. } => Response::Context(context_value()),
                        Request::ReadTarget {
                            home: selected,
                            target: requested,
                        } => {
                            assert_eq!(selected, home());
                            assert_eq!(requested, target);
                            let mut value = target_value(target.clone(), None);
                            match case {
                                0 => value.home.registry = Uuid::from_u128(99),
                                1 => {
                                    value.target =
                                        parse_pull_request("https://github.com/fork/b/pull/1")
                                            .unwrap()
                                }
                                2 => return Err(ControlError::ConnectionClosed),
                                3 => {
                                    return Ok(Response::Failed(Failure::Refused(
                                        "Unavailable".into(),
                                    )));
                                }
                                4 => value.lifecycle = HomeLifecycle::Archived,
                                _ => unreachable!(),
                            }
                            Response::Target(value)
                        }
                        _ => panic!("failed lookup must not mutate: {case}"),
                    })
                },
            );
            assert!(result.is_err(), "case {case}");
            assert_eq!(count, 2, "case {case}");
        }
    }

    #[test]
    fn contradictory_applied_reply_stays_unknown_instead_of_claiming_no_effect() {
        let original = request();
        let output = run(Prepared::Apply(original), &mut |_| {
            Ok(Response::Applied(crate::channel_work::Reply {
                receipt: Receipt {
                    home: home(),
                    operation: Uuid::from_u128(999),
                    revision: 1,
                    sequence: 1,
                    changed: true,
                },
                snapshot: snapshot_value(),
            }))
        })
        .unwrap();
        assert_eq!(output.data["outcome"], "pending");
        assert!(output.human.contains("may have committed"));
    }

    #[test]
    fn matching_applied_reply_reports_original_receipt_and_current_snapshot() {
        let Request::Mutate { mutation, .. } = request() else {
            panic!("mutation fixture")
        };
        let state = HomeState {
            home: home(),
            lifecycle: HomeLifecycle::Active,
            revision: 0,
        };
        let entry = reduce_entry(&state, None, &mutation, &Origin::User)
            .unwrap()
            .entry;
        let mut snapshot = snapshot_value();
        snapshot.revision = 1;
        snapshot.entries.push(entry);
        let receipt = Receipt {
            home: home(),
            operation: mutation.operation,
            revision: 1,
            sequence: 20,
            changed: true,
        };
        let original = Request::Mutate {
            mutation,
            caller: None,
        };
        let mut count = 0;
        let output = run(Prepared::Apply(original), &mut |_| {
            count += 1;
            Ok(Response::Applied(crate::channel_work::Reply {
                receipt: receipt.clone(),
                snapshot: snapshot.clone(),
            }))
        })
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(output.data["outcome"], "succeeded");
        assert_eq!(
            output.data["reply"]["receipt"],
            serde_json::to_value(receipt).unwrap()
        );
        assert!(
            output
                .human
                .contains("intent, not proof of filesystem writes")
        );
    }

    fn historical_entry(number: u64) -> Entry {
        let state = HomeState {
            home: home(),
            lifecycle: HomeLifecycle::Active,
            revision: 100,
        };
        let target =
            parse_pull_request(&format!("https://github.com/owner/repo/pull/{number}")).unwrap();
        let request = Mutation {
            home: home(),
            expected_revision: 0,
            operation: Uuid::from_u128(100 + u128::from(number)),
            change: Change::Claim {
                target: target.clone(),
                role: Role::Contribute,
            },
        };
        let entry = reduce_entry(&state, None, &request, &Origin::User)
            .unwrap()
            .entry;
        let end = Mutation {
            expected_revision: entry.revision,
            operation: Uuid::from_u128(200 + u128::from(number)),
            change: Change::End { target },
            ..request
        };
        reduce_entry(&state, Some(&entry), &end, &Origin::User)
            .unwrap()
            .entry
    }

    #[test]
    fn exact_target_lookup_uses_its_entry_revision_and_captured_caller_without_page_scans() {
        let target = parse_pull_request("https://github.com/owner/repo/pull/1").unwrap();
        let mut sent = Vec::new();
        let output = run(
            Prepared::Change {
                channel: None,
                change: Change::End {
                    target: target.clone(),
                },
                save: None,
            },
            &mut |request| {
                sent.push(request.clone());
                Ok(match request {
                    Request::Context { .. } => Response::Context(context_value()),
                    Request::ReadTarget {
                        home: selected,
                        target: requested,
                    } => {
                        assert_eq!(selected, home());
                        assert_eq!(requested, target);
                        Response::Target(target_value(target.clone(), Some(historical_entry(1))))
                    }
                    Request::Mutate { mutation, caller } => {
                        assert_eq!(mutation.change.target(), &target);
                        assert_eq!(mutation.expected_revision, 2);
                        assert_eq!(caller, context_value().caller);
                        Response::Failed(Failure::Refused("Fixed fixture refusal".into()))
                    }
                    _ => panic!("unexpected request"),
                })
            },
        )
        .unwrap();
        assert_eq!(
            sent.len(),
            3,
            "one exact target lookup and exactly one mutation"
        );
        assert_eq!(output.data["outcome"], "failed");
        assert!(
            output
                .human
                .contains("an earlier attempt with this UUID may already be recorded")
        );
    }

    #[test]
    fn show_pages_current_and_history_independently_without_claiming_global_totals() {
        let page = Page {
            current_before: Some(30),
            before: Some(20),
            limit: 2,
        };
        let mut sent = Vec::new();
        let output = run(Prepared::Show(None, page), &mut |request| {
            sent.push(request.clone());
            Ok(match request {
                Request::Context { .. } => Response::Context(context_value()),
                Request::Read {
                    home: selected,
                    page: requested,
                } => {
                    assert_eq!(selected, home());
                    assert_eq!(requested, page);
                    let mut value = snapshot_value();
                    value.revision = 100;
                    let mut current = historical_entry(1);
                    current.user_ended = false;
                    current.claims[0].active = true;
                    value.entries.push(current);
                    value.history.push(historical_entry(2));
                    value.next_current = Some(10);
                    value.next_history = Some(5);
                    Response::Snapshot(value)
                }
                _ => panic!("show does not mutate"),
            })
        })
        .unwrap();
        assert_eq!(sent.len(), 2);
        assert!(
            output
                .human
                .contains("1 declared ongoing work shown on this page")
        );
        assert!(output.human.contains("--current-before 10"));
        assert!(output.human.contains("--before 5"));
        for invalid in 0..3 {
            let result = read(home(), page, &mut |_| {
                let mut value = snapshot_value();
                value.revision = 100;
                value.history.push(historical_entry(1));
                match invalid {
                    0 => value.next_history = Some(20),
                    1 => {
                        value.history.push(historical_entry(2));
                        value.history.push(historical_entry(3));
                    }
                    2 => {
                        let mut row = historical_entry(2);
                        row.user_ended = false;
                        row.claims[0].active = true;
                        value.entries.push(row);
                        value.next_current = Some(31);
                    }
                    _ => unreachable!(),
                }
                Ok(Response::Snapshot(value))
            });
            assert!(result.is_err(), "page contract case {invalid}");
        }
    }

    #[test]
    fn legacy_actor_placement_requests_are_kept_and_refused_before_transport() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy-actor.json");
        let mut value = serde_json::to_value(request()).unwrap();
        let caller = value["Mutate"]["caller"].as_object_mut().unwrap();
        let old_revision = caller.remove("placement_epoch").unwrap();
        caller.insert("view_revision".into(), old_revision);
        let original_bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(&path, &original_bytes).unwrap();
        let error = match Prepared::new(WorkCommand::Apply {
            request: path.clone(),
        }) {
            Err(error) => error,
            Ok(_) => panic!("an old actor request must never be reinterpreted"),
        };
        assert!(format!("{error:?}").contains("older placement format"));
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        let fresh = request();
        let fresh_path = directory.path().join("current-actor.json");
        save_request(&fresh_path, &fresh).unwrap();
        assert_eq!(load_request(&fresh_path).unwrap(), fresh);
        for epoch in [0, i64::MAX as u64 + 1] {
            let mut malformed = request();
            let Request::Mutate {
                caller: Some(caller),
                ..
            } = &mut malformed
            else {
                panic!()
            };
            caller.placement_epoch = epoch;
            assert!(validate_saved(&malformed).is_err());
            let mut calls = 0;
            let mut fresh_context = context_value();
            fresh_context.caller.as_mut().unwrap().placement_epoch = epoch;
            assert!(
                run(
                    Prepared::Change {
                        channel: None,
                        change: Change::Claim {
                            target: parse_pull_request("https://github.com/a/b/pull/1").unwrap(),
                            role: Role::Review
                        },
                        save: None,
                    },
                    &mut |request| {
                        calls += 1;
                        assert!(matches!(request, Request::Context { .. }));
                        Ok(Response::Context(fresh_context.clone()))
                    }
                )
                .is_err()
            );
            assert_eq!(
                calls, 1,
                "invalid Context epochs must not reach target lookup or mutation"
            );
        }
        let mut user_request = request();
        let Request::Mutate { caller, .. } = &mut user_request else {
            panic!()
        };
        *caller = None;
        let user_path = directory.path().join("user.json");
        save_request(&user_path, &user_request).unwrap();
        assert_eq!(
            load_request(&user_path).unwrap(),
            user_request,
            "explicit User requests have no actor stamp to migrate"
        );
    }

    #[test]
    fn saved_request_refuses_fifo_directory_and_symlink_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("request.pipe");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // This fixture owns the fresh private path; no external process or writer exists.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(load_request(&fifo).is_err());
        assert!(load_request(directory.path()).is_err());
        let regular = directory.path().join("request.json");
        save_request(&regular, &request()).unwrap();
        let before = std::fs::read(&regular).unwrap();
        let link = directory.path().join("request.link");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        assert!(load_request(&link).is_err());
        assert_eq!(std::fs::read(&regular).unwrap(), before);
        assert_eq!(
            load_request(&regular).unwrap(),
            request(),
            "ordinary saved request still works"
        );
    }

    #[test]
    fn saved_input_rejects_unknown_authority_fields_nonmutation_and_oversize_before_io() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.json");
        let mut value = serde_json::to_value(request()).unwrap();
        value["Mutate"]
            .as_object_mut()
            .unwrap()
            .insert("authority".into(), json!("forged"));
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(load_request(&path).is_err());
        std::fs::write(
            &path,
            serde_json::to_vec(&Request::Context { channel: None }).unwrap(),
        )
        .unwrap();
        assert!(load_request(&path).is_err());
        std::fs::write(&path, vec![b' '; REQUEST_BYTES as usize + 1]).unwrap();
        assert!(load_request(&path).is_err());
        let duplicated = serde_json::to_string(&request()).unwrap().replace(
            "\"operation\":",
            "\"operation\":\"00000000-0000-0000-0000-000000000099\",\"operation\":",
        );
        std::fs::write(&path, duplicated).unwrap();
        assert!(
            load_request(&path).is_err(),
            "duplicate semantic fields are not an exact saved request"
        );
    }
}
