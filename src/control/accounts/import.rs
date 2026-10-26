//! Explicit staged-cache metadata requests through one selected User daemon.
//! No credential file is opened locally, no provider is launched by this client,
//! and uncertain publication never causes a reconnect, new UUID or resend.
use super::super::*;
use crate::{
    account::import::{
        CodingProfile, CopyConsent, ImportInspection, ImportOperation, ImportPlan,
        ImportPublication, ImportReply, ImportStage, ImportStatus,
    },
    device_service::{Operation, Reply},
    model::HostSessionProof,
};
use std::{collections::HashSet, fs::Metadata, os::unix::fs::MetadataExt, path::Component};
use uuid::Uuid;

const REQUEST_BYTES: u64 = 64 * 1024;
// The bounded retained-operation metadata contract, including registered rows.
const MAX_LIST_RECORDS: usize = 16;

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub command: ImportCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ImportCommand {
    /// List retained import metadata without checking, retrying or advancing any stage.
    List,
    /// Inspect one explicitly staged auth.json on the selected device; no import is prepared.
    Inspect {
        /// Absolute staged-file path on that device, or quoted '~/path'.
        #[arg(value_parser = parse_path)]
        path: PathBuf,
    },
    /// Prepare the exact consented metadata plan; does not verify or register an account.
    Prepare {
        /// Private local JSON ImportPlan (metadata only); original operation UUID is required.
        #[arg(long)]
        request_file: PathBuf,
    },
    /// Read the original operation's metadata without retrying any stage.
    Status {
        /// Original nonnil import operation UUID, also its managed account key.
        #[arg(value_parser = parse_uuid)]
        operation: Uuid,
    },
    /// Explicitly verify the original pending home and expected identity on the current daemon.
    Verify {
        /// Original nonnil import operation UUID; never substitutes a new plan or account.
        #[arg(value_parser = parse_uuid)]
        operation: Uuid,
    },
}

pub(super) fn require_user() -> Result<(), AppError> {
    user_authority(&Authority::from_environment())
}

fn user_authority(authority: &Authority) -> Result<(), AppError> {
    if authority.token().is_some() {
        return Err(AppError::new(
            "permission_denied",
            "Account import requires your own terminal; hosted authority is retained and cannot import credentials",
        ));
    }
    Ok(())
}

fn invalid(message: &'static str) -> AppError {
    AppError::new("account_import", message)
}

fn parse_uuid(value: &str) -> Result<Uuid, String> {
    value
        .parse::<Uuid>()
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(|| "Use the original nonnil import UUID".into())
}

fn parse_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(char::is_control)
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || !(path.is_absolute()
            || value
                .strip_prefix("~/")
                .is_some_and(|rest| !Path::new(rest).is_absolute()))
    {
        return Err("Use an absolute staged-file path on the selected device or a quoted '~/path' without parent traversal or controls".into());
    }
    Ok(path.into())
}

enum Prepared {
    List,
    Inspect(PathBuf),
    Prepare(ImportPlan),
    Status(Uuid),
    Verify(Uuid),
}

impl Prepared {
    fn new(args: Args) -> Result<Self, AppError> {
        Ok(match args.command {
            ImportCommand::List => Self::List,
            ImportCommand::Inspect { path } => Self::Inspect(
                parse_path(
                    path.to_str()
                        .ok_or_else(|| invalid("The staged-file path must be valid device text"))?,
                )
                .map_err(|_| invalid("The staged-file path is invalid"))?,
            ),
            ImportCommand::Prepare { request_file } => Self::Prepare(load_plan(&request_file)?),
            ImportCommand::Status { operation } | ImportCommand::Verify { operation }
                if operation.is_nil() =>
            {
                return Err(invalid("Use the original nonnil import UUID"));
            }
            ImportCommand::Status { operation } => Self::Status(operation),
            ImportCommand::Verify { operation } => Self::Verify(operation),
        })
    }
}

pub(super) fn execute(args: Args) -> Result<CommandOutput, AppError> {
    let authority = Authority::from_environment();
    user_authority(&authority)?;
    let prepared = Prepared::new(args)?;
    let socket = control_daemon_socket()?;
    let mut client = ControlClient::connect(&socket).map_err(|_| {
        invalid("The selected daemon could not be reached; no import request was sent")
    })?;
    execute_with(&mut client, authority, prepared)
}

fn execute_with(
    client: &mut ControlClient,
    authority: Authority,
    prepared: Prepared,
) -> Result<CommandOutput, AppError> {
    user_authority(&authority)?;
    client.authenticate(authority)
        .map_err(|_| invalid("The selected daemon did not authenticate this User request; no import stage was requested"))?;
    let proof = client.proof().clone();
    if !proof.is_complete() {
        return Err(invalid(
            "The selected daemon did not provide a complete destination proof",
        ));
    }
    let (command, operation, request) = match &prepared {
        Prepared::List => ("account.import.list", None, ImportOperation::List),
        Prepared::Inspect(path) => (
            "account.import.inspect",
            None,
            ImportOperation::Inspect { path: path.clone() },
        ),
        Prepared::Prepare(plan) => {
            if plan.destination != proof {
                return Err(invalid(
                    "The selected daemon differs from the original plan. Prepare was not sent; inspect the original UUID on the selected device rather than replacing or replaying it",
                ));
            }
            (
                "account.import.prepare",
                Some(plan.operation),
                ImportOperation::Prepare { plan: plan.clone() },
            )
        }
        Prepared::Status(operation) => (
            "account.import.status",
            Some(*operation),
            ImportOperation::Status {
                operation: *operation,
            },
        ),
        Prepared::Verify(operation) => (
            "account.import.verify",
            Some(*operation),
            ImportOperation::Verify {
                operation: *operation,
                destination: proof.clone(),
            },
        ),
    };
    // Once a mutating stage is sent, a refused, lost or incompatible response
    // cannot establish a rollback. Preserve the UUID and inspect explicitly.
    let reply = client.device_operation(Operation::AccountImport(request));
    let Ok(Reply::AccountImport(reply)) = reply else {
        return match prepared {
            Prepared::Prepare(_) | Prepared::Verify(_) => {
                Ok(uncertain(command, operation.unwrap()))
            }
            _ => Err(invalid(
                "The import metadata response could not be confirmed; no request was retried",
            )),
        };
    };
    match (prepared, *reply) {
        (Prepared::List, ImportReply::Listed(statuses)) => {
            let mut seen = HashSet::with_capacity(statuses.len().min(MAX_LIST_RECORDS));
            if statuses.len() > MAX_LIST_RECORDS
                || statuses.iter().any(|status| {
                    !valid_status(status, &proof, status.plan.operation)
                        || !seen.insert(status.plan.operation)
                })
            {
                return Err(invalid(
                    "The retained import list is inconsistent or exceeds its bound; no stage was advanced",
                ));
            }
            list_output(statuses)
        }
        (Prepared::Inspect(path), ImportReply::Inspected(inspection)) => {
            if inspection.destination != proof
                || inspection.observation.validate().is_err()
                || (path.is_absolute() && inspection.observation.path() != path.as_path())
            {
                return Err(invalid(
                    "The file inspection did not match the selected destination and path",
                ));
            }
            inspection_output(inspection)
        }
        (Prepared::Prepare(plan), ImportReply::Status(status)) => {
            if !valid_status(&status, &proof, plan.operation) || status.plan != plan {
                return Ok(uncertain(command, plan.operation));
            }
            status_output(command, status)
        }
        (Prepared::Verify(operation), ImportReply::Status(status)) => {
            if !valid_status(&status, &proof, operation) {
                return Ok(uncertain(command, operation));
            }
            status_output(command, status)
        }
        (Prepared::Status(operation), ImportReply::Status(status)) => {
            if !valid_status(&status, &proof, operation) {
                return Err(invalid(
                    "The status did not match the original operation and current destination; nothing was retried",
                ));
            }
            status_output(command, status)
        }
        (Prepared::Prepare(plan), _) => Ok(uncertain(command, plan.operation)),
        (Prepared::Verify(operation), _) => Ok(uncertain(command, operation)),
        _ => Err(invalid(
            "The daemon returned an incompatible import metadata response",
        )),
    }
}

fn valid_status(status: &ImportStatus, proof: &HostSessionProof, operation: Uuid) -> bool {
    status.observed_destination == *proof
        && status.plan.operation == operation
        && status.plan.validate().is_ok()
        && status.receipt.as_ref().is_none_or(|receipt| {
            receipt.validate().is_ok()
                && receipt.operation() == operation
                && receipt.size() == status.plan.observation.size()
        })
        && status.registered.as_ref().is_none_or(|id| {
            status.stage == ImportStage::Registered && id.as_str() == format!("codex/{operation}")
        })
        && status.coding_profile.as_ref().is_none_or(|report| {
            let CodingProfile::WorkspaceWrite { network_access } = status.plan.profile else {
                return false;
            };
            status.stage == ImportStage::Registered
                && status.registered.as_ref() == Some(&report.account)
                && report.host == *proof
                && report.network_access == network_access
                && status.receipt.as_ref().is_some_and(|receipt| {
                    report.home.device == receipt.home().device()
                        && report.home.inode == receipt.home().inode()
                })
        })
        && match status.stage {
            ImportStage::Reserved => status.receipt.is_none(),
            _ => status.receipt.is_some(),
        }
}

fn stage_label(stage: ImportStage) -> &'static str {
    match stage {
        ImportStage::Reserved => "reserved",
        ImportStage::Prepared => "prepared",
        ImportStage::Published => "published",
        ImportStage::Registered => "registered",
    }
}

fn list_output(statuses: Vec<ImportStatus>) -> Result<CommandOutput, AppError> {
    let mut lines = vec!["Import\tStage\tExpected identity\tUncertain".to_owned()];
    for status in &statuses {
        lines.push(format!(
            "{}\t{}\t{}\t{}",
            status.plan.operation,
            stage_label(status.stage),
            status.plan.expected_email,
            status.uncertain
        ));
    }
    if statuses.is_empty() {
        lines.push("No retained import records were reported".into());
    }
    lines.push("Metadata only; authentication and task readiness are not measured. Use the original UUID for explicit Status or Verify; no stage was advanced or retried.".into());
    Ok(CommandOutput {
        command: "account.import.list",
        human: lines.join("\n"),
        data: json!({"imports":statuses,"authentication":"not_checked_by_list","execution_readiness":"not_measured"}),
    })
}

fn inspection_output(inspection: ImportInspection) -> Result<CommandOutput, AppError> {
    // Deliberately incomplete. The user supplies a durable UUID, expected email
    // and both acknowledgements; preview never generates intent or consent.
    let preview = json!({
        "operation": null,
        "destination": inspection.destination,
        "observation": inspection.observation,
        "expected_email": null,
        "consent": CopyConsent { staged_file_only: false, source_session_limit_acknowledged: false },
        "profile": CodingProfile::Unconfigured,
    });
    Ok(CommandOutput {
        command: "account.import.inspect",
        human: format!(
            "Staged file: {}\nBytes: {}\nDestination: {}\nNo cache was imported. Create a private metadata request with an original UUID, expected email and explicit copy acknowledgements before Prepare. Authentication and task execution have not been checked.",
            inspection.observation.path().display(),
            inspection.observation.size(),
            inspection.destination.host_incarnation.as_str()
        ),
        data: json!({"inspection": inspection, "plan_preview": preview, "preparation":"not_requested", "authentication":"not_checked", "execution_readiness":"not_measured"}),
    })
}

fn status_output(command: &'static str, status: ImportStatus) -> Result<CommandOutput, AppError> {
    let registered = status.registered.is_some() && status.stage == ImportStage::Registered;
    let stage = stage_label(status.stage);
    let publication = match status.publication {
        ImportPublication::Missing => "missing",
        ImportPublication::Present => "present",
        ImportPublication::Different => "different",
        ImportPublication::Unknown => "unknown",
    };
    let (profile_detail, profile_outcome) = match status.coding_profile.as_ref() {
        None => ("not observed in this response".to_owned(), None),
        Some(report) => match report.outcome {
            crate::account::coding_config::Outcome::Created => (
                format!("created workspace-write/on-request defaults; network {}", if report.network_access { "enabled" } else { "disabled" }),
                Some("succeeded"),
            ),
            crate::account::coding_config::Outcome::NotCreated(crate::account::coding_config::Refusal::ExistingConfiguration) => (
                "an existing configuration entry was preserved; its requested settings and original creation are unverified. Use an explicit coding-access check".into(), Some("existing_configuration"),
            ),
            crate::account::coding_config::Outcome::NotCreated(reason) => (
                format!("not created: {}", reason.description()), Some("failed"),
            ),
            crate::account::coding_config::Outcome::CreationUncertain(_) => (
                "publication uncertain; inspect the original account configuration before another explicit setup request".into(), Some("pending"),
            ),
        },
    };
    let outcome = if !registered || status.uncertain {
        "pending"
    } else if matches!(status.plan.profile, CodingProfile::WorkspaceWrite { .. }) {
        match profile_outcome {
            Some("existing_configuration") | None => "pending",
            Some(outcome) => outcome,
        }
    } else {
        "succeeded"
    };
    Ok(CommandOutput {
        command,
        human: format!(
            "Import: {}\nStage: {stage}\nOriginal cache entry: {publication}\nRegistration: {}\nUncertainty retained: {}\nCoding profile: {profile_detail}\nAuthentication is not established by status metadata; task readiness is not measured. Check the original UUID explicitly after an uncertain result; nothing was retried.",
            status.plan.operation,
            if registered { "registered" } else { "pending" },
            status.uncertain
        ),
        data: json!({"operation":status.plan.operation, "outcome":outcome, "coding_profile_outcome":profile_outcome, "status":status, "registration":if registered {"registered"} else {"pending"}, "authentication":"not_checked_by_status", "execution_readiness":"not_measured"}),
    })
}

fn uncertain(command: &'static str, operation: Uuid) -> CommandOutput {
    CommandOutput {
        command,
        human: format!(
            "Import {operation}: result uncertain. Inspect this original UUID with account import status on the same device; do not replace it or automatically retry. No registration, authentication or task readiness is claimed."
        ),
        data: json!({"operation":operation,"outcome":"pending","uncertain":true,"registration":"unknown","authentication":"not_confirmed","execution_readiness":"not_measured"}),
    }
}

fn safe_plan_file(metadata: &Metadata) -> bool {
    metadata.is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
        && metadata.nlink() == 1
}

fn load_plan(path: &Path) -> Result<ImportPlan, AppError> {
    load_plan_with(path, || {})
}

fn load_plan_with(path: &Path, after_open: impl FnOnce()) -> Result<ImportPlan, AppError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| invalid("The metadata request directory is unavailable"))?
            .join(path)
    };
    let relative = absolute
        .strip_prefix("/")
        .map_err(|_| invalid("Use a local metadata request file"))?;
    let root = crate::fs_safety::Root::open(Path::new("/"))
        .map_err(|_| invalid("The metadata request could not be opened"))?;
    let (parent, name) = crate::fs_safety::open_parent_beneath(&root, relative).map_err(|_| {
        invalid("The metadata request path must not contain symbolic links or parent traversal")
    })?;
    let parent_identity = parent
        .metadata()
        .map_err(|_| invalid("The metadata request parent could not be inspected"))?;
    let current_parent = || {
        let (current, current_name) = crate::fs_safety::open_parent_beneath(&root, relative)
            .map_err(|_| invalid("The metadata request parent changed"))?;
        let actual = current
            .metadata()
            .map_err(|_| invalid("The metadata request parent could not be inspected"))?;
        if current_name != name
            || (actual.dev(), actual.ino()) != (parent_identity.dev(), parent_identity.ino())
        {
            return Err(invalid("The metadata request parent changed"));
        }
        Ok(current)
    };
    current_parent()?;
    let mut file = crate::fs_safety::open_beneath(&parent, Path::new(&name))
        .map_err(|_| invalid("The metadata request could not be opened"))?;
    let before = file
        .metadata()
        .map_err(|_| invalid("The metadata request could not be inspected"))?;
    if !safe_plan_file(&before) || before.len() > REQUEST_BYTES {
        return Err(invalid(
            "Use an owned private singly linked regular metadata request of at most 64 KiB",
        ));
    }
    after_open();
    let bytes = crate::fs_safety::read_bounded(&mut file, REQUEST_BYTES)
        .map_err(|_| invalid("The metadata request could not be read"))?;
    let after = file
        .metadata()
        .map_err(|_| invalid("The metadata request could not be inspected"))?;
    // A retained old parent cannot authenticate the current selected path.
    let named_parent = current_parent()?;
    let named = crate::fs_safety::metadata_beneath(&named_parent, Path::new(&name))
        .map_err(|_| invalid("The metadata request changed"))?;
    let stamp = |m: &Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    if bytes.len() as u64 > REQUEST_BYTES
        || !safe_plan_file(&after)
        || !safe_plan_file(&named)
        || stamp(&before) != stamp(&after)
        || stamp(&before) != stamp(&named)
        || bytes.len() as u64 != before.len()
    {
        return Err(invalid(
            "The metadata request changed or exceeded its bound",
        ));
    }
    let plan: ImportPlan = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("The file is not a supported import metadata plan; credential files are not request files"))?;
    let stored: Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("The import plan is invalid"))?;
    if serde_json::to_value(&plan).map_err(|_| invalid("The import plan is invalid"))? != stored
        || plan.validate().is_err()
    {
        return Err(invalid(
            "The import metadata plan has unknown fields or lacks valid identity, destination and consent",
        ));
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        account::import::{PhysicalIdentity, PublicationReceipt, SourceObservation},
        model::{BootId, HostIncarnation, HostSessionId},
        terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
            ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame,
        },
    };
    use clap::Parser;
    use std::{
        fs,
        net::Shutdown,
        os::unix::{
            fs::{PermissionsExt, symlink},
            net::UnixStream,
        },
        thread,
    };

    const SECRET: &str = "synthetic-provider-secret-never-emit";
    fn proof() -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("import-cli-fixture"),
            boot_id: BootId::new("fixture-boot"),
            host_incarnation: HostIncarnation::new("fixture-incarnation"),
        }
    }
    fn plan() -> ImportPlan {
        ImportPlan {
            operation: Uuid::from_u128(1),
            destination: proof(),
            observation: SourceObservation {
                path: "/private/staged/auth.json".into(),
                parent: PhysicalIdentity {
                    device: 1,
                    inode: 2,
                },
                file: PhysicalIdentity {
                    device: 1,
                    inode: 3,
                },
                size: 128,
                mtime_seconds: 1,
                mtime_nanoseconds: 2,
                ctime_seconds: 3,
                ctime_nanoseconds: 4,
            },
            expected_email: "fixture@example.test".into(),
            consent: CopyConsent {
                staged_file_only: true,
                source_session_limit_acknowledged: true,
            },
            profile: CodingProfile::Unconfigured,
        }
    }
    fn status(plan: ImportPlan, registered: bool) -> ImportStatus {
        ImportStatus {
            observed_destination: proof(),
            receipt: Some(PublicationReceipt {
                operation: plan.operation,
                home: PhysicalIdentity {
                    device: 2,
                    inode: 10,
                },
                file: PhysicalIdentity {
                    device: 2,
                    inode: 11,
                },
                size: plan.observation.size(),
            }),
            registered: registered.then(|| format!("codex/{}", plan.operation).parse().unwrap()),
            plan,
            stage: if registered {
                ImportStage::Registered
            } else {
                ImportStage::Prepared
            },
            uncertain: false,
            coding_profile: None,
            publication: if registered {
                ImportPublication::Present
            } else {
                ImportPublication::Missing
            },
        }
    }

    #[test]
    fn staged_import_grammar_exposes_only_explicit_metadata_stages_and_keeps_device_paths() {
        assert!(Cli::try_parse_from(["cagents", "account", "import", "list"]).is_ok());
        assert!(Cli::try_parse_from(["cagents", "account", "import", "list", "extra"]).is_err());
        let cli = Cli::try_parse_from([
            "cagents",
            "--device",
            "destination",
            "account",
            "import",
            "inspect",
            "~/staged cache/auth.json",
            "--json",
        ])
        .unwrap();
        assert!(cli.json);
        assert_eq!(cli.device.as_deref(), Some("destination"));
        let TopCommand::Account {
            command:
                AccountCommand::Import(Args {
                    command: ImportCommand::Inspect { path },
                }),
        } = cli.command
        else {
            panic!("wrong command")
        };
        assert_eq!(path, Path::new("~/staged cache/auth.json"));
        assert!(
            Cli::try_parse_from([
                "cagents",
                "account",
                "import",
                "prepare",
                "--request-file",
                "private plan.json"
            ])
            .is_ok()
        );
        for verb in ["status", "verify"] {
            assert!(
                Cli::try_parse_from([
                    "cagents",
                    "account",
                    "import",
                    verb,
                    &plan().operation.to_string()
                ])
                .is_ok()
            );
            for id in ["not-a-uuid", "00000000-0000-0000-0000-000000000000"] {
                assert!(Cli::try_parse_from(["cagents", "account", "import", verb, id]).is_err());
            }
        }
        for path in [
            "relative/auth.json",
            "~other/auth.json",
            "~//other/auth.json",
            "/staged/../auth.json",
            "/staged/\nauth.json",
            "",
        ] {
            assert!(
                Cli::try_parse_from(["cagents", "account", "import", "inspect", path]).is_err(),
                "{path:?}"
            );
        }
        for flag in [
            "--auth-json",
            "--token",
            "--api-key",
            "--run",
            "--default",
            "--sign-in",
        ] {
            assert!(
                Cli::try_parse_from([
                    "cagents",
                    "account",
                    "import",
                    "prepare",
                    "--request-file",
                    "plan.json",
                    flag,
                    "arbitrary"
                ])
                .is_err()
            );
        }
        assert!(Cli::try_parse_from(["cagents", "account", "import", "prepare"]).is_err());
        for token in ["", SECRET] {
            let error = user_authority(&Authority::session(token.into()))
                .err()
                .unwrap();
            assert_eq!(error.kind, "permission_denied");
            assert!(!error.message.contains(SECRET));
        }
        assert!(user_authority(&Authority::default()).is_ok());
    }

    #[test]
    fn private_plan_reader_rejects_credentials_unknown_shapes_links_and_bounds_without_echoing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plan.json");
        let write = |bytes: &[u8]| {
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        };
        let original = plan();
        let bytes = serde_json::to_vec(&original).unwrap();
        write(&bytes);
        assert_eq!(load_plan(&path).unwrap(), original);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_plan(&path).is_err());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("alias");
        symlink(&path, &link).unwrap();
        assert!(load_plan(&link).is_err());
        fs::remove_file(&link).unwrap();
        fs::hard_link(&path, &link).unwrap();
        assert!(load_plan(&path).is_err());
        fs::remove_file(&link).unwrap();
        let auth =
            format!("{{\"auth_mode\":\"chatgpt\",\"tokens\":{{\"refresh_token\":\"{SECRET}\"}}}}");
        write(auth.as_bytes());
        let error = load_plan(&path).err().unwrap();
        assert!(!error.message.contains(SECRET));
        let mut unknown = serde_json::to_value(&original).unwrap();
        unknown["destination"]["secret"] = SECRET.into();
        write(&serde_json::to_vec(&unknown).unwrap());
        assert!(!load_plan(&path).err().unwrap().message.contains(SECRET));
        let mut invalid = original.clone();
        invalid.consent.staged_file_only = false;
        write(&serde_json::to_vec(&invalid).unwrap());
        assert!(load_plan(&path).is_err());
        write(&bytes);
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(REQUEST_BYTES + 1).unwrap();
        assert!(load_plan(&path).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), REQUEST_BYTES + 1);
        assert!(load_plan(directory.path()).is_err());
        for replace in [false, true] {
            let selected_parent = directory.path().join(if replace {
                "selected-replaced"
            } else {
                "selected-renamed"
            });
            fs::create_dir(&selected_parent).unwrap();
            let selected = selected_parent.join("plan.json");
            fs::write(&selected, &bytes).unwrap();
            fs::set_permissions(&selected, fs::Permissions::from_mode(0o600)).unwrap();
            let retained = selected_parent.with_extension("retained");
            assert!(
                load_plan_with(&selected, || {
                    fs::rename(&selected_parent, &retained).unwrap();
                    if replace {
                        fs::create_dir(&selected_parent).unwrap();
                        fs::write(&selected, &bytes).unwrap();
                        fs::set_permissions(&selected, fs::Permissions::from_mode(0o600)).unwrap();
                    }
                })
                .is_err(),
                "an old retained parent must not authenticate the current path"
            );
            assert_eq!(fs::read(retained.join("plan.json")).unwrap(), bytes);
            assert_eq!(load_plan(&retained.join("plan.json")).unwrap(), original);
            if replace {
                assert_eq!(fs::read(&selected).unwrap(), bytes);
            }
        }
        let fifo = directory.path().join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(load_plan(&fifo).is_err());
    }

    #[derive(Debug, Clone, Copy)]
    enum Case {
        List,
        EmptyList,
        DuplicateList,
        OverfullList,
        ForeignList,
        InvalidList,
        Inspect,
        Prepare,
        Status,
        Verify,
        Lost,
        WrongObserved,
        WrongEnvelope,
        WrongPlan,
        WrongKind,
        AuthRefused,
        OldPlan,
        ProfileCreated,
        StatusProfile,
        ProfileRefused,
        ProfileExisting,
        ProfileUncertain,
        ProfileWrongHost,
        ProfileWrongHome,
        ProfileWrongAccount,
        ProfileWrongNetwork,
        UnexpectedProfile,
        WrongRegisteredStage,
    }

    fn exercise(case: Case) -> Result<CommandOutput, AppError> {
        let mut original = plan();
        if matches!(
            case,
            Case::ProfileCreated
                | Case::StatusProfile
                | Case::ProfileRefused
                | Case::ProfileExisting
                | Case::ProfileUncertain
                | Case::ProfileWrongHost
                | Case::ProfileWrongHome
                | Case::ProfileWrongAccount
                | Case::ProfileWrongNetwork
        ) {
            original.profile = CodingProfile::WorkspaceWrite {
                network_access: true,
            };
        }
        let is_verify = matches!(
            case,
            Case::Verify
                | Case::ProfileCreated
                | Case::ProfileRefused
                | Case::ProfileExisting
                | Case::ProfileUncertain
                | Case::ProfileWrongHost
                | Case::ProfileWrongHome
                | Case::ProfileWrongAccount
                | Case::ProfileWrongNetwork
                | Case::UnexpectedProfile
                | Case::WrongRegisteredStage
        );
        let is_list = matches!(
            case,
            Case::List
                | Case::EmptyList
                | Case::DuplicateList
                | Case::OverfullList
                | Case::ForeignList
                | Case::InvalidList
        );
        let prepared = match case {
            _ if is_list => Prepared::List,
            Case::Inspect => Prepared::Inspect(original.observation.path().into()),
            Case::Status | Case::StatusProfile => Prepared::Status(original.operation),
            _ if is_verify => Prepared::Verify(original.operation),
            Case::OldPlan => {
                let mut plan = original.clone();
                plan.destination.host_incarnation = HostIncarnation::new("old-daemon");
                Prepared::Prepare(plan)
            }
            _ => Prepared::Prepare(original.clone()),
        };
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let worker = thread::spawn(move || {
            let host = proof();
            for step in 0..3 {
                if step == 2 && matches!(case, Case::OldPlan) {
                    break;
                }
                let envelope: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                if step > 0 {
                    assert_eq!(envelope.proof.as_ref(), Some(&host));
                }
                let response = match (step, envelope.request.clone()) {
                    (0, ClientRequest::Connect { .. }) => ServerResponse::Connected {
                        proof: host.clone(),
                        resumed: vec![],
                    },
                    (1, ClientRequest::Authenticate { authority }) => {
                        assert!(authority.token().is_none());
                        ServerResponse::Accepted
                    }
                    (
                        2,
                        ClientRequest::DeviceOperation {
                            operation: Operation::AccountImport(request),
                        },
                    ) => {
                        match case {
                            _ if is_list => assert_eq!(request, ImportOperation::List),
                            Case::Inspect => assert_eq!(
                                request,
                                ImportOperation::Inspect {
                                    path: original.observation.path().into()
                                }
                            ),
                            Case::Status | Case::StatusProfile => assert_eq!(
                                request,
                                ImportOperation::Status {
                                    operation: original.operation
                                }
                            ),
                            _ if is_verify => assert_eq!(
                                request,
                                ImportOperation::Verify {
                                    operation: original.operation,
                                    destination: host.clone()
                                }
                            ),
                            _ => assert_eq!(
                                request,
                                ImportOperation::Prepare {
                                    plan: original.clone()
                                }
                            ),
                        }
                        if matches!(case, Case::Lost) {
                            peer.shutdown(Shutdown::Write).unwrap();
                            break;
                        }
                        let reply = if is_list {
                            let mut second = original.clone();
                            second.operation = Uuid::from_u128(2);
                            second.destination.host_incarnation =
                                HostIncarnation::new("recorded-old-daemon");
                            let mut rows =
                                vec![status(original.clone(), false), status(second, false)];
                            match case {
                                Case::EmptyList => rows.clear(),
                                Case::DuplicateList => rows[1] = rows[0].clone(),
                                Case::OverfullList => {
                                    rows = (1..=MAX_LIST_RECORDS + 1)
                                        .map(|id| {
                                            let mut plan = original.clone();
                                            plan.operation = Uuid::from_u128(id as u128);
                                            status(plan, false)
                                        })
                                        .collect()
                                }
                                Case::ForeignList => {
                                    rows[1].observed_destination.host_incarnation =
                                        HostIncarnation::new("foreign-list-daemon")
                                }
                                Case::InvalidList => rows[1].plan.operation = Uuid::nil(),
                                _ => {}
                            }
                            ImportReply::Listed(rows)
                        } else if matches!(case, Case::Inspect) {
                            ImportReply::Inspected(ImportInspection {
                                destination: host.clone(),
                                observation: original.observation.clone(),
                            })
                        } else {
                            let mut status = status(
                                original.clone(),
                                is_verify || matches!(case, Case::StatusProfile),
                            );
                            if is_verify
                                && !matches!(case, Case::Verify | Case::WrongRegisteredStage)
                            {
                                let mut report = crate::account::coding_config::CreateReport {
                                    account: status.registered.clone().unwrap(), host:host.clone(),
                                    home: crate::account::readiness::PhysicalIdentity { device:2, inode:10 },
                                    network_access:true,
                                    outcome:match case {
                                        Case::ProfileRefused => crate::account::coding_config::Outcome::NotCreated(crate::account::coding_config::Refusal::RegistryBusy),
                                        Case::ProfileExisting => crate::account::coding_config::Outcome::NotCreated(crate::account::coding_config::Refusal::ExistingConfiguration),
                                        Case::ProfileUncertain => crate::account::coding_config::Outcome::CreationUncertain(crate::account::coding_config::Uncertainty::DirectorySyncFailed),
                                        _ => crate::account::coding_config::Outcome::Created,
                                    },
                                };
                                match case {
                                    Case::ProfileWrongHost => {
                                        report.host.host_incarnation =
                                            HostIncarnation::new("foreign-profile-host")
                                    }
                                    Case::ProfileWrongHome => report.home.inode += 1,
                                    Case::ProfileWrongAccount => {
                                        report.account = "codex/system".parse().unwrap()
                                    }
                                    Case::ProfileWrongNetwork => report.network_access = false,
                                    _ => {}
                                }
                                status.coding_profile = Some(report);
                            }
                            if matches!(case, Case::WrongRegisteredStage) {
                                status.stage = ImportStage::Prepared;
                            }
                            if matches!(case, Case::Status) {
                                status.plan.destination.host_incarnation =
                                    HostIncarnation::new("recorded-old-daemon");
                            }
                            if matches!(case, Case::WrongObserved) {
                                status.observed_destination.host_incarnation =
                                    HostIncarnation::new("other-daemon");
                            }
                            if matches!(case, Case::WrongPlan) {
                                status.plan.expected_email = "other@example.test".into();
                            }
                            ImportReply::Status(status)
                        };
                        ServerResponse::DeviceOperation {
                            reply: if matches!(case, Case::WrongKind) {
                                Reply::AccountRegistry(json!({"private":SECRET}))
                            } else {
                                Reply::AccountImport(Box::new(reply))
                            },
                        }
                    }
                    _ => panic!("unexpected request for {case:?} step {step}"),
                };
                let auth_refused = step == 1 && matches!(case, Case::AuthRefused);
                let message = if auth_refused {
                    ServerMessage::Error(ProtocolError::new(
                        ProtocolErrorCode::PermissionDenied,
                        SECRET,
                        false,
                    ))
                } else {
                    ServerMessage::Response { response }
                };
                let mut observed = host.clone();
                if step == 2 && matches!(case, Case::WrongEnvelope) {
                    observed.host_incarnation = HostIncarnation::new("foreign-envelope");
                }
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(envelope.request_id),
                        proof: observed,
                        message,
                    },
                )
                .unwrap();
                if auth_refused {
                    break;
                }
            }
            assert!(
                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                "no resend/reconnect/recheck/provider/default operation on this connection"
            );
        });
        let mut client = ControlClient::from_stream(stream).unwrap();
        let result = execute_with(&mut client, Authority::default(), prepared);
        drop(client);
        worker.join().unwrap();
        result
    }

    #[test]
    fn framed_import_routes_exact_original_intent_and_current_verify_proof_without_readiness_claims()
     {
        let listed = exercise(Case::List).unwrap();
        assert_eq!(listed.command, "account.import.list");
        assert_eq!(listed.data["imports"].as_array().unwrap().len(), 2);
        assert_eq!(listed.data["execution_readiness"], "not_measured");
        assert!(listed.human.contains("Expected identity"));
        assert!(listed.human.contains("fixture@example.test"));
        assert!(listed.human.contains(&plan().operation.to_string()));
        assert!(
            exercise(Case::EmptyList).unwrap().data["imports"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let inspection = exercise(Case::Inspect).unwrap();
        assert_eq!(inspection.command, "account.import.inspect");
        assert_eq!(
            inspection.data["inspection"]["destination"],
            serde_json::to_value(proof()).unwrap()
        );
        assert!(inspection.data["plan_preview"]["operation"].is_null());
        assert_eq!(
            inspection.data["plan_preview"]["consent"]["staged_file_only"],
            false
        );
        let prepared = exercise(Case::Prepare).unwrap();
        assert_eq!(prepared.data["status"]["stage"], "prepared");
        assert_eq!(prepared.data["registration"], "pending");
        assert_eq!(prepared.exit_code(), ExitCode::from(3));
        let status = exercise(Case::Status).unwrap();
        assert_eq!(
            status.data["status"]["plan"]["destination"]["host_incarnation"],
            "recorded-old-daemon"
        );
        assert_eq!(
            status.data["status"]["observed_destination"],
            serde_json::to_value(proof()).unwrap()
        );
        let verified = exercise(Case::Verify).unwrap();
        assert_eq!(verified.data["registration"], "registered");
        assert!(verified.data["status"]["coding_profile"].is_null());
        assert!(verified.human.contains("not observed in this response"));
        let profile_unobserved = exercise(Case::StatusProfile).unwrap();
        assert_eq!(profile_unobserved.data["registration"], "registered");
        assert_eq!(profile_unobserved.data["outcome"], "pending");
        assert!(profile_unobserved.data["coding_profile_outcome"].is_null());
        assert!(profile_unobserved.data["status"]["coding_profile"].is_null());
        assert!(
            profile_unobserved
                .human
                .contains("not observed in this response")
        );
        for (case, expected) in [
            (Case::ProfileCreated, "succeeded"),
            (Case::ProfileRefused, "failed"),
            (Case::ProfileUncertain, "pending"),
        ] {
            let output = exercise(case).unwrap();
            assert_eq!(output.data["registration"], "registered");
            assert_eq!(output.data["outcome"], expected);
            assert_eq!(output.data["coding_profile_outcome"], expected);
            assert_eq!(output.data["execution_readiness"], "not_measured");
            assert!(output.data["status"]["coding_profile"].is_object());
        }
        let existing = exercise(Case::ProfileExisting).unwrap();
        assert_eq!(existing.data["registration"], "registered");
        assert_eq!(existing.data["outcome"], "pending");
        assert_eq!(
            existing.data["coding_profile_outcome"],
            "existing_configuration"
        );
        assert_eq!(existing.exit_code(), ExitCode::from(3));
        assert_eq!(existing.data["execution_readiness"], "not_measured");
        assert_eq!(
            existing.data["status"]["plan"]["profile"]["network_access"],
            true
        );
        assert!(
            existing
                .human
                .contains("existing configuration entry was preserved")
        );
        assert!(
            existing
                .human
                .contains("requested settings and original creation are unverified")
        );
        assert!(!existing.human.contains("network enabled"));
        assert!(!existing.human.contains("created workspace-write"));
        for output in [&inspection, &prepared, &status, &verified] {
            assert_eq!(output.data["execution_readiness"], "not_measured");
            assert!(!output.human.contains(SECRET));
            assert!(!output.data.to_string().contains(SECRET));
        }
    }

    #[test]
    fn framed_uncertain_import_keeps_original_uuid_and_refuses_proof_drift_without_replay_or_secret_errors()
     {
        for case in [
            Case::Lost,
            Case::WrongObserved,
            Case::WrongEnvelope,
            Case::WrongPlan,
            Case::WrongKind,
            Case::ProfileWrongHost,
            Case::ProfileWrongHome,
            Case::ProfileWrongAccount,
            Case::ProfileWrongNetwork,
            Case::UnexpectedProfile,
            Case::WrongRegisteredStage,
        ] {
            let output = exercise(case).unwrap();
            assert_eq!(output.data["operation"], plan().operation.to_string());
            assert_eq!(output.data["outcome"], "pending");
            assert_eq!(output.data["uncertain"], true);
            assert_eq!(output.data["execution_readiness"], "not_measured");
            assert!(!output.human.contains(SECRET));
            assert!(!output.data.to_string().contains(SECRET));
        }
        for case in [
            Case::AuthRefused,
            Case::OldPlan,
            Case::DuplicateList,
            Case::OverfullList,
            Case::ForeignList,
            Case::InvalidList,
        ] {
            let error = exercise(case).err().unwrap();
            assert!(!error.message.contains(SECRET));
        }
    }
}
