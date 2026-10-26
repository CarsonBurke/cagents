//! User-admitted staged imports. Credential bytes never enter IPC, registry,
//! journal or provider diagnostics. Each effect retains the original operation.
use super::{account_lifecycle::Reservation, account_readiness::use_lease, state::SharedHost};
use crate::{
    account::{
        Account,
        coding_config::{CreateRequest, InspectRequest, Presence},
        home::Homes,
        import::{
            CodingProfile, ImportInspection, ImportOperation, ImportPlan, ImportPublication,
            ImportReply, ImportStage, ImportStatus,
            cache::{self, PublicationState},
            journal::{Journal, Phase, Record},
        },
        probe::{self, import_identity},
        registry::{Registered, Store},
    },
    agent::Provider,
    fs_safety::Root,
    model::HostSessionProof,
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
};
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

fn refused(message: &'static str) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
fn proof(host: &SharedHost) -> HostSessionProof {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone()
}
fn current_destination(
    host: &SharedHost,
    expected: &HostSessionProof,
) -> Result<(), ProtocolError> {
    if !expected.is_complete() || proof(host) != *expected {
        return Err(refused(
            "The destination daemon changed; inspect the original import before explicitly verifying it on the current connection",
        ));
    }
    Ok(())
}

struct Installation {
    store: Store,
    homes: Homes,
    journal: Journal,
}
impl Installation {
    fn selected(create_parent: bool) -> Result<Self, ProtocolError> {
        let dirs = crate::instance::project_dirs()
            .ok_or_else(|| refused("Account storage is unavailable"))?;
        if create_parent {
            crate::secure_file::prepare_private_directory(dirs.config_dir())
                .map_err(|_| refused("Private account storage is unavailable"))?;
        }
        Ok(Self {
            store: Store::installation()
                .ok_or_else(|| refused("Account registry is unavailable"))?,
            homes: Homes::installation().ok_or_else(|| refused("Account homes are unavailable"))?,
            journal: Journal::at(dirs.config_dir().join("account-imports")),
        })
    }

    fn original(&self, operation: Uuid) -> Result<Record, ProtocolError> {
        self.journal.status(operation)
            .map_err(|_| refused("The original import journal is unavailable or requires a newer application; it was retained"))?
            .ok_or_else(|| refused("No original import intent was observed; no import stage was repeated"))
    }
}

fn staged_path(path: PathBuf) -> Result<PathBuf, ProtocolError> {
    if let Some(relative) = path.to_str().and_then(|p| p.strip_prefix("~/")) {
        if Path::new(relative).is_absolute() {
            return Err(refused("Use one slash after ~ in a staged-file path"));
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| refused("The destination HOME is unavailable"))?;
        let result = PathBuf::from(home).join(relative);
        if !result.is_absolute() {
            return Err(refused("The destination HOME must be absolute"));
        }
        return Ok(result);
    }
    if !path.is_absolute() {
        return Err(refused(
            "Choose an absolute staged-file path on the destination device",
        ));
    }
    Ok(path)
}

fn publication(homes: &Homes, record: &Record) -> ImportPublication {
    let Some(receipt) = record.receipt() else {
        return ImportPublication::Unknown;
    };
    let path = homes.path(Provider::Codex, record.plan().operation);
    if crate::secure_file::reject_symlink_components(&path).is_err() {
        return ImportPublication::Unknown;
    }
    let Ok(root) = Root::open(&path) else {
        return ImportPublication::Unknown;
    };
    match cache::publication_state(&root, receipt) {
        Ok(PublicationState::Present) => ImportPublication::Present,
        Ok(PublicationState::Missing) => ImportPublication::Missing,
        Ok(PublicationState::Different) => ImportPublication::Different,
        Err(_) => ImportPublication::Unknown,
    }
}

fn status_at(
    host: &SharedHost,
    installation: &Installation,
    operation: Uuid,
    uncertain: bool,
) -> Result<ImportStatus, ProtocolError> {
    let record = installation.original(operation)?;
    let registered = installation
        .store
        .read_import_metadata()
        .ok()
        .and_then(|registry| {
            if registry.is_newer()
                || registry.is_archived(operation)
                || registry
                    .pending()
                    .iter()
                    .any(|pending| pending.key == operation)
            {
                return None;
            }
            let entry = registry.find(operation)?;
            if entry.provider != Provider::Codex
                || entry.linked_home.is_some()
                || !entry
                    .email
                    .as_deref()
                    .is_some_and(|email| email.eq_ignore_ascii_case(&record.plan().expected_email))
            {
                return None;
            }
            let receipt = record.receipt()?;
            let path = installation.homes.path(Provider::Codex, operation);
            crate::secure_file::reject_symlink_components(&path).ok()?;
            let root = Root::open(&path).ok()?;
            let metadata = root.metadata().ok()?;
            if metadata.dev() != receipt.home().device()
                || metadata.ino() != receipt.home().inode()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() == 0
            {
                return None;
            }
            Some(
                Account::managed(
                    Provider::Codex,
                    operation,
                    installation.homes.path(Provider::Codex, operation),
                )
                .id(),
            )
        });
    Ok(ImportStatus {
        observed_destination: proof(host),
        plan: record.plan().clone(),
        receipt: record.receipt().cloned(),
        stage: match record.phase() {
            Phase::Reserved => ImportStage::Reserved,
            Phase::Prepared => ImportStage::Prepared,
            Phase::Published => ImportStage::Published,
            Phase::Registered => ImportStage::Registered,
        },
        uncertain: uncertain || record.uncertain(),
        publication: publication(&installation.homes, &record),
        registered: if record.phase() == Phase::Registered {
            registered
        } else {
            None
        },
        coding_profile: None,
    })
}

fn uncertain_status(
    host: &SharedHost,
    installation: &Installation,
    plan: &ImportPlan,
) -> Result<ImportReply, ProtocolError> {
    // Failure to write uncertainty never becomes evidence of rollback. The
    // returned status keeps uncertainty even if the journal could not advance.
    let _ = installation.journal.mark_uncertain(plan);
    Ok(ImportReply::Status(status_at(
        host,
        installation,
        plan.operation,
        true,
    )?))
}

fn prepare(host: &SharedHost, plan: ImportPlan) -> Result<ImportReply, ProtocolError> {
    plan.validate().map_err(refused)?;
    current_destination(host, &plan.destination)?;
    let installation = Installation::selected(true)?;
    prepare_at(host, plan, &installation)
}

fn prepare_at(
    host: &SharedHost,
    plan: ImportPlan,
    installation: &Installation,
) -> Result<ImportReply, ProtocolError> {
    plan.validate().map_err(refused)?;
    current_destination(host, &plan.destination)?;
    if let Some(existing) = installation.journal.status(plan.operation).map_err(|_| {
        refused("The original import journal could not be inspected; nothing was retried")
    })? {
        if existing.plan() != &plan {
            return Err(refused(
                "The operation differs from its original import intent",
            ));
        }
        return Ok(ImportReply::Status(status_at(
            host,
            &installation,
            plan.operation,
            false,
        )?));
    }
    // Validate and snapshot the selected bytes before reserving durable intent.
    // Neither this read nor Prepare starts a provider or looks up its executable.
    let snapshot = cache::snapshot(plan.observation.path(), &plan.observation)
        .map_err(|_| refused("The staged cache is unsafe, unsupported or changed; inspect it again before creating a new intent"))?;
    let reservation = Reservation::for_import_at(
        host,
        plan.operation,
        true,
        installation.store.clone(),
        &installation.homes,
    )?;
    installation.journal.begin(&plan)
        .map_err(|_| refused("The import journal is unavailable, conflicts, or contains 16 retained operations; existing evidence was preserved"))?;
    let mut prepared = None;
    let created = installation.store.try_create_import_pending_reserved(
        &reservation,
        &installation.homes,
        plan.operation,
        crate::chat::now_ms().unwrap_or_default(),
        || {
            reservation.create_import_home(&installation.homes, |created| {
                let cache = cache::prepare(snapshot, created).map_err(|_| {
                    std::io::Error::other("The private import cache could not be prepared")
                })?;
                installation
                    .journal
                    .record_prepared(&plan, &cache.receipt())
                    .map_err(|_| {
                        std::io::Error::other(
                            "The original prepared import evidence could not be persisted",
                        )
                    })?;
                prepared = Some(cache);
                Ok(())
            })
        },
    );
    if created.is_err() {
        return uncertain_status(host, &installation, &plan);
    }
    let Some(prepared) = prepared else {
        return uncertain_status(host, &installation, &plan);
    };
    let receipt = prepared.receipt();
    // Only the real pending Store ACK above permits this exact opaque inode to
    // be linked. The short recheck owns the real Store and lifecycle leases.
    let linked = installation.store.try_with_import_pending_reserved(
        &reservation,
        &installation.homes,
        plan.operation,
        receipt.home(),
        || {
            cache::publish(prepared)
                .map_err(|_| std::io::Error::other("The original import publication is uncertain"))
        },
    );
    if linked.is_err() || installation.journal.mark_published(&plan).is_err() {
        return uncertain_status(host, &installation, &plan);
    }
    Ok(ImportReply::Status(status_at(
        host,
        &installation,
        plan.operation,
        false,
    )?))
}

/// No HostState strong reference lives inside this guard: a retained Scope can
/// keep the reservation and probe lock without creating a HostState/map cycle.
struct Guard {
    reservation: Reservation,
    _probe_lock: crate::instance::OwnedLock,
    store: Store,
    homes: Homes,
    journal: Journal,
    record: Record,
    destination: HostSessionProof,
}
struct Owner {
    host: SharedHost,
    guard: Arc<Guard>,
    _lease: Arc<use_lease::Lease>,
}
impl probe::SpawnOwner for Owner {
    fn spawn(
        &self,
        command: &mut tokio::process::Command,
    ) -> Result<std::io::Result<tokio::process::Child>, String> {
        let gate = self
            .host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .control_gate
            .clone();
        let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
        super::client::ensure_control_open(&self.host)
            .map_err(|_| "The account host is closing".to_owned())?;
        current_destination(&self.host, &self.guard.destination)
            .map_err(|_| "The destination daemon changed".to_owned())?;
        let operation = self.guard.record.plan().operation;
        let current = self
            .guard
            .journal
            .status(operation)
            .map_err(|_| "The import journal changed".to_owned())?
            .ok_or("The original import intent is unavailable")?;
        if current.plan() != self.guard.record.plan()
            || current.receipt() != self.guard.record.receipt()
            || current.phase() != Phase::Published
        {
            return Err("The original import verification context changed".into());
        }
        let receipt = current
            .receipt()
            .ok_or("The original import receipt is unavailable")?;
        let _launch = self
            .guard
            .store
            .authorize_import_command_reserved(
                &self.guard.reservation,
                &self.guard.homes,
                operation,
                receipt.home(),
                &current.plan().expected_email,
                command.as_std(),
            )
            .map_err(|_| "The original import no longer permits verification".to_owned())?;
        // The dedicated real lease above admits this one identity-only command.
        // The generic launcher correctly refuses import-owned Pending homes.
        Ok(command.spawn())
    }
    fn preserve_unconfirmed(&self, child: tokio::process::Child, group_signal_confirmed: bool) {
        self._lease
            .retain_unconfirmed(child, group_signal_confirmed);
    }
}

fn verification_owner(
    host: &SharedHost,
    installation: &Installation,
    record: Record,
    destination: HostSessionProof,
) -> Result<Arc<Owner>, ProtocolError> {
    let operation = record.plan().operation;
    let receipt = record
        .receipt()
        .ok_or_else(|| refused("The original prepared import receipt is unavailable"))?;
    let reservation = Reservation::for_import_at(
        host,
        operation,
        false,
        installation.store.clone(),
        &installation.homes,
    )?;
    reservation
        .verify_import_home(operation, receipt.home())
        .map_err(|_| refused("The original imported home changed"))?;
    let account = Account::managed(
        Provider::Codex,
        operation,
        installation.homes.path(Provider::Codex, operation),
    );
    let probe_lock = import_identity::probe_lock(&account).map_err(|_| {
        refused("An account probe is busy or unavailable; preserve this import and check again")
    })?;
    let home = Root::open(&installation.homes.path(Provider::Codex, operation))
        .map_err(|_| refused("The original imported home is unavailable"))?;
    let pin: File = home
        .retained_directory()
        .map_err(|_| refused("The original imported home cannot be pinned"))?;
    let guard = Arc::new(Guard {
        reservation,
        _probe_lock: probe_lock,
        store: installation.store.clone(),
        homes: installation.homes.clone(),
        journal: installation.journal.clone(),
        record,
        destination,
    });
    let scope =
        use_lease::Scope::capture_guarded(home.path(), &pin, home.path(), &pin, guard.clone())
            .map_err(|_| refused("The original imported home changed during verification"))?;
    let (gate, map) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.control_gate.clone(),
            state.account_readiness_uses.clone(),
        )
    };
    use_lease::cleanup_retained(&map);
    let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
    super::client::ensure_control_open(host)?;
    current_destination(host, &guard.destination)?;
    guard
        .reservation
        .verify_import_home(operation, guard.record.receipt().unwrap().home())
        .map_err(|_| refused("The original imported home changed"))?;
    super::worktree_removal::ensure_outside_directory(host, &pin)?;
    super::worktree_removal::ensure_outside(host, home.path())?;
    let lease = Arc::new(
        use_lease::Lease::enroll(&map, scope)
            .map_err(|_| refused("Too many account checks are active or awaiting cleanup"))?,
    );
    Ok(Arc::new(Owner {
        host: host.clone(),
        guard,
        _lease: lease,
    }))
}

fn verify_identity(
    host: &SharedHost,
    installation: &Installation,
    operation: Uuid,
    destination: HostSessionProof,
) -> Result<ImportStatus, ProtocolError> {
    current_destination(host, &destination)?;
    let mut record = installation.original(operation)?;
    if record.phase() == Phase::Registered {
        return status_at(host, installation, operation, false);
    }
    if record.phase() == Phase::Prepared {
        if publication(&installation.homes, &record) != ImportPublication::Present {
            return Err(refused(
                "The original cache publication is not confirmed; it was not repeated",
            ));
        }
        installation
            .journal
            .mark_published(record.plan())
            .map_err(|_| {
                refused(
                    "The original import publication evidence is uncertain; nothing was reseeded",
                )
            })?;
        record = installation.original(operation)?;
    }
    if record.phase() != Phase::Published {
        return Err(refused(
            "No original cache publication is available for verification",
        ));
    }
    let owner = verification_owner(host, installation, record.clone(), destination)?;
    let receipt = record.receipt().unwrap();
    let account = Account::managed(
        Provider::Codex,
        operation,
        installation.homes.path(Provider::Codex, operation),
    );
    // The official client may atomically rotate its own auth cache. Validate a
    // current private supported cache, but never require the initial inode after
    // publication, replace it with an old seed, or infer identity from its bytes.
    cache::inspect(
        &installation
            .homes
            .path(Provider::Codex, operation)
            .join("auth.json"),
    )
    .map_err(|_| {
        refused("The current imported cache is unsafe or unavailable; no identity was inferred")
    })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| refused("The identity verification runtime is unavailable"))?;
    let identity = runtime.block_on(import_identity::verify(account, receipt.home(), &record.plan().expected_email, owner.clone()))
        .map_err(|_| refused("The official client did not confirm the expected ChatGPT identity and cleanup; preserve the original import"))?;
    let (registry, result) = installation.store.try_register_import_pending_reserved(
        &owner.guard.reservation, &installation.homes, operation, receipt.home(), &record.plan().expected_email,
        Registered { key: operation, provider: Provider::Codex, linked_home: None, label: None, email: identity.email, organization: identity.organization, added_at: crate::chat::now_ms().unwrap_or_default() }, &[],
    ).map_err(|_| refused("The import registration result is uncertain; inspect the original operation before continuing"))?;
    if result.is_err() || registry.find(operation).is_none() {
        return Err(refused(
            "That identity already belongs to another account; the pending import was retained without selecting it",
        ));
    }
    if installation.journal.mark_registered(record.plan()).is_err() {
        return status_at(host, installation, operation, true);
    }
    status_at(host, installation, operation, false)
}

fn requested_profile(host: &SharedHost, status: ImportStatus) -> ImportStatus {
    requested_profile_with(
        status,
        |request| super::coding_config::inspect(host, request),
        |request| super::coding_config::create(host, request),
    )
}

fn requested_profile_with(
    mut status: ImportStatus,
    inspect: impl FnOnce(
        InspectRequest,
    ) -> Result<crate::account::coding_config::Observation, ProtocolError>,
    create: impl FnOnce(
        CreateRequest,
    ) -> Result<crate::account::coding_config::CreateReport, ProtocolError>,
) -> ImportStatus {
    let CodingProfile::WorkspaceWrite { network_access } = status.plan.profile else {
        return status;
    };
    let (Some(account), Some(receipt)) = (status.registered.clone(), status.receipt.as_ref())
    else {
        return status;
    };
    let request = InspectRequest {
        account,
        expected_home: crate::account::readiness::PhysicalIdentity {
            device: receipt.home().device(),
            inode: receipt.home().inode(),
        },
        expected_email: status.plan.expected_email.clone(),
    };
    if let Ok(observation) = inspect(request) {
        status.coding_profile = match observation.presence {
            Presence::Absent => create(CreateRequest {
                observation,
                network_access,
            })
            .ok(),
            Presence::Present => Some(crate::account::coding_config::CreateReport {
                account: observation.account,
                host: observation.host,
                home: observation.home,
                // This is the immutable request, not an observation of the
                // existing file's contents or effective network permission.
                network_access,
                outcome: crate::account::coding_config::Outcome::NotCreated(
                    crate::account::coding_config::Refusal::ExistingConfiguration,
                ),
            }),
            Presence::Unknown => None,
        };
    }
    status
}

fn list_at(
    host: &SharedHost,
    installation: &Installation,
) -> Result<Vec<ImportStatus>, ProtocolError> {
    let records = installation.journal.load().map_err(|_| {
        refused("The retained import journal is unavailable; no stage was repeated")
    })?;
    records
        .into_iter()
        .map(|record| status_at(host, installation, record.plan().operation, false))
        .collect()
}

pub(super) fn execute(
    host: &SharedHost,
    operation: ImportOperation,
) -> Result<ImportReply, ProtocolError> {
    match operation {
        ImportOperation::List => Ok(ImportReply::Listed(list_at(
            host,
            &Installation::selected(false)?,
        )?)),
        ImportOperation::Inspect { path } => {
            let observation = cache::inspect(&staged_path(path)?).map_err(|_| {
                refused("The selected staged cache is unsafe, unsupported or unavailable")
            })?;
            Ok(ImportReply::Inspected(ImportInspection {
                destination: proof(host),
                observation,
            }))
        }
        ImportOperation::Prepare { plan } => prepare(host, plan),
        ImportOperation::Status { operation } => Ok(ImportReply::Status(status_at(
            host,
            &Installation::selected(false)?,
            operation,
            false,
        )?)),
        ImportOperation::Verify {
            operation,
            destination,
        } => {
            let installation = Installation::selected(false)?;
            let status = verify_identity(host, &installation, operation, destination)?;
            Ok(ImportReply::Status(requested_profile(host, status)))
        }
    }
}

#[cfg(test)]
mod tests;
