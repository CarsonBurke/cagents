//! Separate fresh physical consent. Internal only while transport allocation
//! is held. Metadata replies/load never enter this module as spawn authority.
use super::{
    account_lifecycle::{PreparedAdmission, PreparedCreationReservation},
    account_preparation::{OriginalCaller, metadata_stores, seed_selection},
    state::{ClientId, SharedHost},
};
use crate::{
    account::{
        fresh_code::{CodeOwner, ConditionalPlan, FreshCodeStart},
        home::{Homes, NewHomeCreationReceipt},
        preparation::{
            AccountPreparationRequest, Preparation, PreparationRequest, PreparationState,
        },
        registry::{CodeClaim, LaunchAccountLease, Store},
    },
    agent::Provider,
    model::LaunchRecipe,
    terminal_ipc::{ClientRequest, ProtocolError, ProtocolErrorCode},
};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

/// Minted only from the actual fresh receipt. It deliberately cannot be
/// cloned/deserialized from durable home metadata or reconstructed by path.
pub(super) struct CreatedRoot {
    provider: Provider,
    key: Uuid,
    path: PathBuf,
    parent_path: PathBuf,
    directory: File,
    parent: File,
}

impl CreatedRoot {
    pub(super) fn capture(receipt: &NewHomeCreationReceipt) -> io::Result<Self> {
        receipt.revalidate()?;
        let root = Self {
            provider: receipt.provider(),
            key: receipt.key(),
            path: receipt.home().to_owned(),
            parent_path: receipt
                .home()
                .parent()
                .ok_or_else(|| io::Error::other("The fresh root has no parent"))?
                .to_owned(),
            directory: receipt.directory().try_clone()?,
            parent: receipt.parent().try_clone()?,
        };
        root.revalidate()?;
        Ok(root)
    }

    pub(super) fn provider(&self) -> Provider {
        self.provider
    }
    pub(super) fn key(&self) -> Uuid {
        self.key
    }
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn matches(&self, metadata: &fs::Metadata) -> bool {
        self.directory.metadata().is_ok_and(|held| {
            held.is_dir()
                && metadata.is_dir()
                && held.dev() == metadata.dev()
                && held.ino() == metadata.ino()
        })
    }
    pub(super) fn revalidate(&self) -> io::Result<()> {
        crate::secure_file::reject_symlink_components(&self.path)?;
        for (path, held) in [
            (&self.parent_path, &self.parent),
            (&self.path, &self.directory),
        ] {
            let held = held.metadata()?;
            let named = fs::symlink_metadata(path)?;
            if !held.is_dir()
                || !named.is_dir()
                || held.dev() != named.dev()
                || held.ino() != named.ino()
                || held.uid() != unsafe { libc::geteuid() }
                || named.uid() != held.uid()
                || held.mode() & 0o077 != 0
                || named.mode() & 0o077 != 0
            {
                return Err(io::Error::other(
                    "The actual created root changed; code login remains retained",
                ));
            }
        }
        Ok(())
    }
}

pub(super) enum PreparedStart {
    Ready(Ready),
    /// Historic metadata, including Created, cannot recreate the actual pin.
    Retained {
        preparation: Preparation,
        owner: Option<CodeOwner>,
    },
    Unknown(String),
}

/// A new ownership barrier plus the actual live enrollment. This is consumed
/// into bounded constructor custody, never serialized or recovered by a read.
pub(super) struct Ready {
    owner: CodeOwner,
    root: Arc<CreatedRoot>,
    reservation: Arc<PreparedCreationReservation>,
    caller: Arc<OriginalCaller>,
    wire_request: ClientRequest,
    host: Weak<super::state::HostLock>,
    store: Store,
    homes: Homes,
}

pub(super) fn conditional_plan(
    host: &SharedHost,
    client: ClientId,
    request: &ConditionalPlan,
) -> Result<io::Result<Preparation>, ProtocolError> {
    request.validate().map_err(refused)?;
    let wire = ClientRequest::DeviceOperation {
        operation: crate::device_service::Operation::AccountPreparation {
            request: AccountPreparationRequest::Preparation(PreparationRequest::Plan {
                request_id: request.request_id,
                payload: request.payload.clone(),
            }),
        },
    };
    let caller = OriginalCaller::capture(host, client)?;
    let gate = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let _reader = gate.read().unwrap_or_else(|p| p.into_inner());
    caller.revalidate(host, &wire)?;
    super::account_lifecycle::ensure_idle(host)?;
    let (Some(store), Some(_)) = metadata_stores() else {
        return Err(refused("The original fresh-code storage is unavailable"));
    };
    Ok(store.plan_fresh_code(request))
}

/// This boundary performs no terminal construction. The reviewed retained-
/// startup owner must consume Ready before the first worker/holder is created.
pub(super) fn prepare_start(
    host: &SharedHost,
    client: ClientId,
    request: &FreshCodeStart,
) -> Result<PreparedStart, ProtocolError> {
    request.validate().map_err(refused)?;
    let wire_request = create_request(request);
    let caller = Arc::new(OriginalCaller::capture(host, client)?);
    let gate = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let (store, homes, observed) = {
        let _reader = gate.read().unwrap_or_else(|p| p.into_inner());
        caller.revalidate(host, &wire_request)?;
        super::account_lifecycle::ensure_idle(host)?;
        let (Some(store), Some(homes)) = metadata_stores() else {
            return Err(refused("The private fresh-code storage is unavailable"));
        };
        let observed = match store.try_reconcile_preparation(&request.binding) {
            Ok(record) => record,
            Err(error) => return Ok(PreparedStart::Unknown(error.to_string())),
        };
        (store, homes, observed)
    };
    if observed.state != PreparationState::Planned {
        return Ok(match store.observe_fresh_code(request) {
            Ok(owner) => PreparedStart::Retained {
                preparation: observed,
                owner,
            },
            Err(error) => PreparedStart::Unknown(error.to_string()),
        });
    }
    let admission = PreparedCreationReservation::reserve(
        host,
        store.clone(),
        homes.clone(),
        &request.binding,
        || seed_selection(request.binding.payload.provider),
        || caller.revalidate(host, &wire_request),
    );
    let reservation =
        match admission {
            Ok(PreparedAdmission::Reserved(reservation)) => Arc::new(reservation),
            Ok(PreparedAdmission::ReplayNeeded) => return Ok(PreparedStart::Unknown(
                "The preparation changed before physical admission; reconcile its exact binding"
                    .into(),
            )),
            Err(error) => return Ok(PreparedStart::Unknown(error.message)),
        };
    let (created, root) = match reservation.create_for_code() {
        Ok(created) => created,
        Err(error) => return Ok(PreparedStart::Unknown(error.to_string())),
    };
    let Some(root) = root.filter(|_| created.state == PreparationState::Created) else {
        return Ok(PreparedStart::Retained {
            preparation: created,
            owner: None,
        });
    };
    if let Err(error) = caller
        .revalidate(host, &wire_request)
        .map_err(|error| error.message)
        .and_then(|()| reservation.verify_code_root(&root))
    {
        return Ok(PreparedStart::Unknown(error));
    }
    let owner = match store.claim_fresh_code(request, caller.proof()) {
        Ok(CodeClaim::Fresh(owner)) => owner,
        Ok(CodeClaim::Retained(owner)) => {
            return Ok(PreparedStart::Retained {
                preparation: created,
                owner: Some(owner),
            });
        }
        Err(error) => return Ok(PreparedStart::Unknown(error.to_string())),
    };
    Ok(PreparedStart::Ready(Ready {
        owner,
        root,
        reservation,
        caller,
        wire_request,
        host: Arc::downgrade(host),
        store,
        homes,
    }))
}

fn create_request(request: &FreshCodeStart) -> ClientRequest {
    ClientRequest::DeviceOperation {
        operation: crate::device_service::Operation::AccountPreparation {
            request: AccountPreparationRequest::Preparation(PreparationRequest::Create {
                binding: request.binding.clone(),
            }),
        },
    }
}

fn refused(message: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::InvalidRequest,
        message.to_string(),
        false,
    )
}

/// Additive internal constructor capability. It is never a TerminalConfig,
/// TerminalSession or public wire field. Arc cloning does not permit a second
/// entry: the one-shot flag is claimed under the final account lifecycle lease.
pub(crate) struct SpawnAdmission {
    ready: Ready,
    recipe: LaunchRecipe,
    claimed: AtomicBool,
}

impl std::fmt::Debug for SpawnAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FreshCodeSpawnAdmission")
            .field("attempt", &self.ready.owner.attempt)
            .field("claimed", &self.claimed.load(Ordering::Acquire))
            .finish()
    }
}

impl Ready {
    /// Discovery is host-owned and runs after durable ownership. Failure keeps
    /// that owner retained; callers cannot replace its identity or recipe.
    pub(super) fn admission(
        self,
        program: PathBuf,
        reader_home: PathBuf,
        search_path: Option<String>,
    ) -> io::Result<Arc<SpawnAdmission>> {
        if !program.is_absolute() || !reader_home.is_absolute() {
            return Err(io::Error::other(
                "The host code-login program and HOME must be absolute",
            ));
        }
        self.root.revalidate()?;
        let account = crate::account::Account::managed(
            Provider::Codex,
            self.owner.binding.key,
            self.root.path.clone(),
        );
        let mut environment =
            BTreeMap::from([("CODEX_HOME".into(), self.root.path.display().to_string())]);
        if let Some(path) = search_path {
            environment.insert("PATH".into(), path);
        }
        let recipe = LaunchRecipe {
            program: program
                .to_str()
                .ok_or_else(|| io::Error::other("The host provider path is not UTF-8"))?
                .into(),
            arguments: vec!["login".into(), "--device-auth".into()],
            working_directory: reader_home,
            environment,
            unset_environment: account
                .credential_overrides()
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        };
        Ok(Arc::new(SpawnAdmission {
            ready: self,
            recipe,
            claimed: AtomicBool::new(false),
        }))
    }
}

impl SpawnAdmission {
    pub(crate) fn recipe(&self) -> &LaunchRecipe {
        &self.recipe
    }
    pub(crate) fn owner(&self) -> &CodeOwner {
        &self.ready.owner
    }
    pub(crate) fn authorize(&self, recipe: &LaunchRecipe) -> io::Result<LaunchAccountLease> {
        if recipe != &self.recipe {
            return Err(io::Error::other(
                "The fresh code constructor recipe changed",
            ));
        }
        self.ready
            .store
            .authorize_fresh_code_spawn(&self.ready.homes, &self.ready.owner, || {
                let host = self.ready.host.upgrade().ok_or_else(|| {
                    io::Error::other("The original code-login host is unavailable")
                })?;
                self.ready
                    .caller
                    .revalidate(&host, &self.ready.wire_request)
                    .map_err(|error| io::Error::other(error.message))?;
                self.ready
                    .reservation
                    .verify_code_root(&self.ready.root)
                    .map_err(io::Error::other)?;
                // This claims the capability; it is NOT evidence that Spawn was
                // sent. The retained constructor separately owns actual entry.
                self.claimed
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| {
                        io::Error::other(
                            "The original code-login constructor capability was already claimed",
                        )
                    })?;
                Ok(())
            })
    }
}

#[cfg(test)]
mod tests;
