//! Authority retained by one accepted worktree-creation journal.
//!
//! An existing source scope does not authorize an arbitrary second checkout.
//! The destination must be created through a held parent and retained before
//! Git receives it; only its verified registration can grant destination scope.

use super::*;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryWitness {
    path: PathBuf,
    device: u64,
    inode: u64,
}

fn invalid(message: impl Into<String>) -> RepositoryError {
    ValidationError::Io(message.into()).into()
}

fn filesystem(error: impl std::fmt::Display) -> RepositoryError {
    invalid(error.to_string())
}

pub(super) fn anchor(root: &fs_safety::Root) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}/.",
        std::process::id(),
        root.descriptor()
    ))
}

/// Each new directory entry must reach disk before a journal can publish the
/// path as durable authority. Existing ancestors are retained, never traversed
/// again through their public names.
fn durable_parent(
    plan: &DestinationPlan,
    relative: &Path,
) -> Result<(fs_safety::Root, OsString), RepositoryError> {
    let parts = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => Ok(name.to_owned()),
            _ => Err(ValidationError::DestinationOutsideRoot.into()),
        })
        .collect::<Result<Vec<_>, RepositoryError>>()?;
    if parts.len() > 64 {
        return Err(invalid(
            "The managed destination exceeds 64 directory levels",
        ));
    }
    let (leaf, parents) = parts
        .split_last()
        .ok_or(ValidationError::DestinationOutsideRoot)?;
    let mut held = plan.ancestor.try_clone().map_err(filesystem)?;
    for name in parents {
        let created = match fs_safety::open_directory_beneath(&held, Path::new(name)) {
            Ok(existing) => fs_safety::DirectoryCreation::Existing(existing),
            Err(error) if error.is_not_found() => {
                // A missing managed ancestor needs the same exact-parent proof
                // before effects as the final checkout. Never bootstrap new
                // creation authority by reopening an unverified mkdir result.
                let namespace = fs_safety::DirectoryNamespace::acquire(&held).map_err(|error| {
                    invalid(format!(
                        "The managed parent cannot prove creation ownership: {error}"
                    ))
                })?;
                fs_safety::create_directory_in_namespace(&namespace, &held, Path::new(name), 0o777)
                    .map_err(filesystem)?
            }
            Err(error) => return Err(filesystem(error)),
        };
        let child = match created {
            fs_safety::DirectoryCreation::Published(publication) => {
                plan.retain_publication(publication)?
            }
            // Existing roots grant traversal, never chmod or cleanup authority.
            fs_safety::DirectoryCreation::Existing(root) => root,
            fs_safety::DirectoryCreation::Unverified { .. } => {
                return Err(invalid(
                    "The managed ancestor has no proven creation identity",
                ));
            }
        };
        // Syncing existing traversal is harmless and also covers parents that
        // were concurrently created by another cooperating writer.
        fs_safety::sync_directory(&child).map_err(filesystem)?;
        fs_safety::sync_directory(&held).map_err(filesystem)?;
        held = child;
    }
    Ok((held, leaf.clone()))
}

impl DirectoryWitness {
    fn held(root: &fs_safety::Root) -> Result<Self, RepositoryError> {
        let metadata = root.metadata().map_err(filesystem)?;
        let witness = Self {
            path: root.path().to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        witness.validate_durable_name()?;
        Ok(witness)
    }

    fn validate_durable_name(&self) -> Result<(), RepositoryError> {
        // A descriptor alias is valid for IO only, never as restart authority.
        // Canonical names also avoid retargetable symlink ancestors in receipts.
        if !self.path.is_absolute()
            || self.path.starts_with("/proc")
            || self.path.starts_with("/dev/fd")
            || fs::canonicalize(&self.path).map_err(filesystem)? != self.path
        {
            return Err(invalid(
                "Creation authority needs a stable canonical directory name",
            ));
        }
        Ok(())
    }

    fn validate(&self, root: &fs_safety::Root) -> Result<(), RepositoryError> {
        if Self::held(root)? != *self {
            return Err(invalid("The retained creation directory identity changed"));
        }
        let named = fs_safety::Root::open(&self.path).map_err(filesystem)?;
        let named = Self::held(&named)?;
        if named.device != self.device || named.inode != self.inode {
            return Err(invalid(
                "A creation directory was replaced; the replacement was not authorized",
            ));
        }
        Ok(())
    }

    fn reopen(&self) -> Result<fs_safety::Root, RepositoryError> {
        self.validate_durable_name()?;
        let root = fs_safety::Root::open(&self.path).map_err(filesystem)?;
        self.validate(&root)?;
        Ok(root)
    }
}

/// Stable names and physical identities only. Process-local descriptor anchors
/// must never enter the durable journal or become restart authority by name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreationEvidence {
    operation: OperationId,
    source: [DirectoryWitness; 3],
    ancestor: DirectoryWitness,
    parent: Option<DirectoryWitness>,
    checkout: Option<DirectoryWitness>,
    admin: Option<DirectoryWitness>,
}

impl CreationEvidence {
    pub(super) fn has_published_destination(&self) -> bool {
        self.checkout.is_some()
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_AFTER_PUBLICATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static AFTER_DIRECTORY_PUBLICATION: std::cell::Cell<Option<fn(&Path)>> = const { std::cell::Cell::new(None) };
}

struct Publication {
    known: Option<(DirectoryWitness, DirectoryWitness)>,
    result: Result<OwnedDestination, RepositoryError>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum RegistrationExpectation {
    RequestedBranch,
    Detached,
}

pub(super) struct CreationSession {
    source: WorktreeAuthority,
    source_roots: [fs_safety::Root; 3],
    plan: DestinationPlan,
    destination: Mutex<Option<Arc<OwnedDestination>>>,
    // Keep the first genuine administrative roots even on a later failure.
    created_roots: Mutex<Option<WorktreeAuthority>>,
    pub(super) pending_settlement: Mutex<Option<super::creation_retention::ActiveCreated>>,
}

impl fmt::Debug for CreationSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreationSession")
            .field("source", &self.source)
            .field("destination", &self.plan.destination)
            .finish_non_exhaustive()
    }
}

impl CreationSession {
    pub(super) fn capture(
        authority: &WorktreeAuthority,
        request: &CreateWorktreeRequest,
    ) -> Result<(Arc<Self>, CreationEvidence), RepositoryError> {
        if authority.path() != request.repository {
            return Err(
                ValidationError::InvalidWorktree(request.repository.display().to_string()).into(),
            );
        }
        let (root, private, common) = authority.retained_roots()?;
        let plan = DestinationPlan::capture(&root, &request.managed_root, &request.destination)?;
        let evidence = CreationEvidence {
            operation: request.operation_id,
            source: [
                DirectoryWitness::held(&root)?,
                DirectoryWitness::held(&private)?,
                DirectoryWitness::held(&common)?,
            ],
            ancestor: plan.witness.clone(),
            parent: None,
            checkout: None,
            admin: None,
        };
        Ok((
            Arc::new(Self {
                source: authority.clone(),
                source_roots: [root, private, common],
                plan,
                destination: Mutex::new(None),
                created_roots: Mutex::new(None),
                pending_settlement: Mutex::new(None),
            }),
            evidence,
        ))
    }

    pub(super) fn restore(
        evidence: &CreationEvidence,
        request: &CreateWorktreeRequest,
        common: &Path,
    ) -> Result<Arc<Self>, RepositoryError> {
        if evidence.operation != request.operation_id
            || evidence.source[0].path != request.repository
            || evidence.source[2].path != common
            || !destination_within_managed_root(&request.destination, &request.managed_root)
        {
            return Err(invalid(
                "Creation authority does not match its durable request",
            ));
        }
        let root = evidence.source[0].reopen()?;
        let private = evidence.source[1].reopen()?;
        let common = evidence.source[2].reopen()?;
        let source = WorktreeAuthority::from_retained_roots(
            root.try_clone().map_err(filesystem)?,
            private.try_clone().map_err(filesystem)?,
            common.try_clone().map_err(filesystem)?,
        )?;
        let ancestor = evidence.ancestor.reopen()?;
        let plan = DestinationPlan {
            managed_relative: request
                .managed_root
                .strip_prefix(&evidence.ancestor.path)
                .map_err(|_| ValidationError::DestinationOutsideRoot)?
                .to_owned(),
            destination_relative: request
                .destination
                .strip_prefix(&evidence.ancestor.path)
                .map_err(|_| ValidationError::DestinationOutsideRoot)?
                .to_owned(),
            destination: request.destination.clone(),
            ancestor,
            witness: evidence.ancestor.clone(),
            publications: Mutex::new(Vec::new()),
        };
        let destination = match (&evidence.parent, &evidence.checkout) {
            (Some(parent), Some(checkout)) => {
                if checkout.path != request.destination
                    || Some(parent.path.as_path()) != request.destination.parent()
                {
                    return Err(invalid(
                        "The saved destination authority names another directory",
                    ));
                }
                Some(Arc::new(OwnedDestination {
                    root: checkout.reopen()?,
                    witness: checkout.clone(),
                    parent: parent.reopen()?,
                    parent_witness: parent.clone(),
                    leaf: request
                        .destination
                        .file_name()
                        .ok_or(ValidationError::DestinationOutsideRoot)?
                        .to_owned(),
                }))
            }
            (None, None) => None,
            _ => {
                return Err(invalid(
                    "Creation authority has incomplete destination evidence",
                ));
            }
        };
        Ok(Arc::new(Self {
            source,
            source_roots: [root, private, common],
            plan,
            destination: Mutex::new(destination),
            created_roots: Mutex::new(None),
            pending_settlement: Mutex::new(None),
        }))
    }

    pub(super) fn validate(&self) -> Result<(), RepositoryError> {
        self.source.validate()?;
        self.plan.witness.validate(&self.plan.ancestor)?;
        if let Some(destination) = &*self.destination.lock().unwrap_or_else(|p| p.into_inner()) {
            destination.validate()?;
        }
        Ok(())
    }

    pub(super) fn has_destination(&self) -> bool {
        self.destination
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    pub(super) fn source_authority(&self) -> Result<WorktreeAuthority, RepositoryError> {
        self.validate()?;
        Ok(self.source.clone())
    }

    pub(super) fn common_operand(&self) -> Result<PathBuf, RepositoryError> {
        self.validate()?;
        Ok(anchor(&self.source_roots[2]))
    }

    pub(super) fn ensure_managed_root(&self) -> Result<(), RepositoryError> {
        self.validate()?;
        self.plan.ensure_managed_root()
    }

    pub(super) fn publish(&self, evidence: &mut CreationEvidence) -> Result<(), RepositoryError> {
        self.validate()?;
        let mut destination = self.destination.lock().unwrap_or_else(|p| p.into_inner());
        if destination.is_some() {
            return Err(invalid("The creation destination was already published"));
        }
        let publication = self.plan.publish();
        // A successful rename is a known effect even when a later sync or
        // validation fails. Never erase that identity from durable recovery.
        if let Some((parent, checkout)) = publication.known {
            evidence.parent = Some(parent);
            evidence.checkout = Some(checkout);
        }
        let owned = Arc::new(publication.result?);
        *destination = Some(owned);
        self.source.validate()
    }

    pub(super) fn bind_destination_operand(
        &self,
        mut command: CommandSpec,
        position: usize,
    ) -> Result<CommandSpec, RepositoryError> {
        self.validate()?;
        if command
            .arguments
            .get(position)
            .map(|value| Path::new(value.as_os_str()))
            != Some(self.plan.destination.as_path())
        {
            return Err(invalid(
                "The creation command lost its expected destination operand",
            ));
        }
        let destination = self
            .destination
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| invalid("Creation has not published a destination"))?;
        destination.validate()?;
        // Retain the SAME duplicate whose number enters argv. Parent-PID FD
        // paths must not escape into a child that can outlive this service.
        let (operand, directory) = authority::inherited_directory_scope(&destination.root)?;
        command.arguments[position] = operand.into_os_string();
        command.inherit_file(directory).map_err(filesystem)
    }

    pub(super) fn lease_path(&self, operation: OperationId) -> Result<PathBuf, RepositoryError> {
        self.validate()?;
        Ok(anchor(&self.source_roots[2]).join(format!("cagents-operation-{operation}.lease")))
    }

    pub(super) fn bind_created(
        self: &Arc<Self>,
        evidence: &mut CreationEvidence,
        request: &CreateWorktreeRequest,
        registered: &WorktreeRecord,
        expected_oid: &str,
        expectation: RegistrationExpectation,
    ) -> Result<Arc<VerifiedCreatedRoot>, RepositoryError> {
        self.validate()?;
        let expected_branch = match expectation {
            RegistrationExpectation::RequestedBranch => Some(request.branch.full_ref()),
            RegistrationExpectation::Detached => None,
        };
        if evidence.operation != request.operation_id
            || registered.path != request.destination
            || registered.branch != expected_branch
            || registered.head_oid.as_deref() != Some(expected_oid)
            || registered.lock.reason.as_deref()
                != Some(format!("cagents:{}", request.operation_id).as_str())
            || registered.bare
            || registered.prunable_reason.is_some()
        {
            return Err(invalid(
                "The created checkout has no matching journal-owned registration",
            ));
        }
        let owned = self
            .destination
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| invalid("No retained created directory is available"))?;
        let locator = regular_locator(&owned.root, ".git")?;
        let locator = locator
            .strip_prefix(b"gitdir: ")
            .ok_or_else(|| invalid("The created checkout has an invalid Git locator"))?;
        let admin_path = bytes_to_path(trim_one_line_ending(locator));
        let common = &self.source_roots[2];
        if admin_path.parent() != Some(common.path().join("worktrees").as_path()) {
            return Err(invalid(
                "The created checkout points outside its original common metadata",
            ));
        }
        let relative = admin_path
            .strip_prefix(common.path())
            .map_err(|_| invalid("The created administrative path changed repositories"))?;
        let admin = fs_safety::open_directory_beneath(common, relative).map_err(filesystem)?;
        let admin_witness = DirectoryWitness::held(&admin)?;
        if evidence
            .admin
            .as_ref()
            .is_some_and(|saved| saved != &admin_witness)
        {
            return Err(invalid("The created administrative directory was replaced"));
        }
        let authority = WorktreeAuthority::from_retained_roots(
            owned.root.try_clone().map_err(filesystem)?,
            admin,
            common.try_clone().map_err(filesystem)?,
        )?;
        self.created_roots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(|| authority.clone());
        let created = Arc::new(VerifiedCreatedRoot {
            source: self.clone(),
            owned,
            authority,
            admin: admin_witness.clone(),
            operation: request.operation_id,
            branch: expected_branch,
            oid: expected_oid.to_owned(),
        });
        created.validate()?;
        evidence.admin = Some(admin_witness);
        Ok(created)
    }
}

pub(super) fn regular_locator(
    root: &fs_safety::Root,
    leaf: &str,
) -> Result<Vec<u8>, RepositoryError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(anchor(root).join(leaf))
        .map_err(filesystem)?;
    if !file.metadata().map_err(filesystem)?.is_file() {
        return Err(invalid("Creation metadata is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(8193)
        .read_to_end(&mut bytes)
        .map_err(filesystem)?;
    if bytes.len() > 8192 || bytes.contains(&0) {
        return Err(invalid("Creation metadata is oversized or invalid"));
    }
    Ok(bytes)
}

/// This type can only be constructed after pre-Git publication, the matching
/// registered branch/OID, operation lock and both metadata backlinks agree.
pub(super) struct VerifiedCreatedRoot {
    source: Arc<CreationSession>,
    owned: Arc<OwnedDestination>,
    authority: WorktreeAuthority,
    admin: DirectoryWitness,
    operation: OperationId,
    branch: Option<String>,
    oid: String,
}

impl fmt::Debug for VerifiedCreatedRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedCreatedRoot")
            .field("operation", &self.operation)
            .field("destination", &self.owned.root.path())
            .field("admin", &self.admin)
            .field("registered_branch", &self.branch)
            .field("registered_oid", &self.oid)
            .finish_non_exhaustive()
    }
}

impl VerifiedCreatedRoot {
    pub(super) fn validate(&self) -> Result<(), RepositoryError> {
        self.source.validate()?;
        self.owned.validate()?;
        self.authority.validate()?;
        let (_, admin, common) = self.authority.retained_roots()?;
        self.admin.validate(&admin)?;
        let locked = regular_locator(&admin, "locked")?;
        let backlink = regular_locator(&admin, "gitdir")?;
        let commondir = regular_locator(&admin, "commondir")?;
        if trim_one_line_ending(&locked) != format!("cagents:{}", self.operation).as_bytes()
            || bytes_to_path(trim_one_line_ending(&backlink)) != self.owned.root.path().join(".git")
            || trim_one_line_ending(&commondir) != b"../.."
            || common.path() != self.source.source_roots[2].path()
        {
            return Err(invalid(
                "The created checkout lost its journal-owned metadata",
            ));
        }
        if self.branch.is_none()
            && trim_one_line_ending(&regular_locator(&admin, "HEAD")?) != self.oid.as_bytes()
        {
            return Err(invalid(
                "The provisional checkout changed its detached commit",
            ));
        }
        Ok(())
    }

    pub(super) fn provisional_metadata(&self) -> Result<(Vec<u8>, Vec<u8>), RepositoryError> {
        self.validate()?;
        let (_, admin, _) = self.authority.retained_roots()?;
        let head = regular_locator(&admin, "HEAD")?;
        let locked = regular_locator(&admin, "locked")?;
        if self.branch.is_some()
            || trim_one_line_ending(&head) != self.oid.as_bytes()
            || trim_one_line_ending(&locked) != format!("cagents:{}", self.operation).as_bytes()
        {
            return Err(invalid(
                "The provisional checkout no longer has its exact detached journal state",
            ));
        }
        Ok((head, locked))
    }

    pub(super) fn prepare_settlement(
        &self,
        generation: u64,
        record: &WorktreeRecord,
    ) -> Result<(), RepositoryError> {
        self.validate()?;
        if record.path != self.owned.root.path()
            || record.branch != self.branch
            || record.head_oid.as_deref() != Some(self.oid.as_str())
        {
            return Err(invalid(
                "The created record differs from its original verified registration",
            ));
        }
        if self.branch.is_none() {
            return Err(invalid(
                "A detached provisional checkout cannot grant a settled creation capability",
            ));
        }
        let active = super::creation_retention::ActiveCreated::new(
            self.operation,
            generation,
            self.source.source_authority()?,
            self.authority.clone(),
            record.clone(),
        );
        let mut pending = self
            .source
            .pending_settlement
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if pending.is_some() {
            return Err(invalid(
                "A creation authority transition is already pending",
            ));
        }
        *pending = Some(active);
        Ok(())
    }

    pub(super) fn authority(&self) -> Result<WorktreeAuthority, RepositoryError> {
        self.validate()?;
        Ok(self.authority.clone())
    }

    pub(super) fn roots(&self) -> Result<(fs_safety::Root, fs_safety::Root), RepositoryError> {
        self.validate()?;
        let (checkout, admin, _) = self.authority.retained_roots()?;
        Ok((admin, checkout))
    }

    pub(super) fn transaction_roots(&self) -> Result<Vec<fs_safety::Root>, RepositoryError> {
        self.validate()?;
        let (checkout, admin, _) = self.authority.retained_roots()?;
        Ok(vec![
            self.owned.parent.try_clone().map_err(filesystem)?,
            checkout,
            admin,
        ])
    }

    pub(super) fn admin_path(&self) -> &Path {
        &self.admin.path
    }
}

/// Captured before creation effects. Missing components are created only below
/// this retained ancestor, including managed directories outside the source.
pub(super) struct DestinationPlan {
    ancestor: fs_safety::Root,
    witness: DirectoryWitness,
    managed_relative: PathBuf,
    destination_relative: PathBuf,
    destination: PathBuf,
    publications: Mutex<Vec<fs_safety::DirectoryPublication>>,
}

impl DestinationPlan {
    pub(super) fn capture(
        source: &fs_safety::Root,
        managed_root: &Path,
        destination: &Path,
    ) -> Result<Self, RepositoryError> {
        if !destination_within_managed_root(destination, managed_root)
            || !managed_root.is_absolute()
            || [managed_root, destination].iter().any(|path| {
                path.components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            })
        {
            return Err(ValidationError::DestinationOutsideRoot.into());
        }
        let mut ancestor = managed_root.to_owned();
        while !ancestor.try_exists().map_err(filesystem)? {
            ancestor = ancestor
                .parent()
                .ok_or(ValidationError::InvalidDestinationRoot)?
                .to_owned();
        }
        let held = if let Ok(relative) = ancestor.strip_prefix(source.path()) {
            if relative.as_os_str().is_empty() {
                source.try_clone().map_err(filesystem)?
            } else {
                fs_safety::open_directory_beneath(source, relative).map_err(filesystem)?
            }
        } else {
            fs_safety::Root::open(&ancestor).map_err(filesystem)?
        };
        let witness = DirectoryWitness::held(&held)?;
        witness.validate(&held)?;
        Ok(Self {
            managed_relative: managed_root
                .strip_prefix(&ancestor)
                .map_err(|_| ValidationError::DestinationOutsideRoot)?
                .to_owned(),
            destination_relative: destination
                .strip_prefix(&ancestor)
                .map_err(|_| ValidationError::DestinationOutsideRoot)?
                .to_owned(),
            destination: destination.to_owned(),
            ancestor: held,
            witness,
            publications: Mutex::new(Vec::new()),
        })
    }

    /// Keep the exact producer receipt before any later fallible operation.
    /// A failed private cleanup is diagnostic uncertainty, never permission to
    /// reopen or remove a current public name. No receipt has Drop cleanup.
    fn retain_publication(
        &self,
        publication: fs_safety::DirectoryPublication,
    ) -> Result<fs_safety::Root, RepositoryError> {
        let mut retained = self
            .publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        retained.push(publication);
        let publication = retained.last().expect("just retained publication");
        if let Some(error) = publication.publication_error() {
            return Err(invalid(format!(
                "Directory publication has retained private cleanup evidence at {:?}: {error}",
                publication.pending_staging_path()
            )));
        }
        publication.directory().try_clone().map_err(filesystem)
    }

    pub(super) fn ensure_managed_root(&self) -> Result<(), RepositoryError> {
        self.witness.validate(&self.ancestor)?;
        let probe = self.managed_relative.join(".cagents-parent-probe");
        durable_parent(self, &probe)?;
        self.witness.validate(&self.ancestor)
    }

    /// Publish an empty, already-held directory without replacing any entry.
    /// Git may populate this owned directory; its argv never reopens the public
    /// destination name after an executor launch delay.
    fn publish(&self) -> Publication {
        let mut known = None;
        let result = self.publish_recording(&mut known);
        Publication { known, result }
    }

    fn publish_recording(
        &self,
        known: &mut Option<(DirectoryWitness, DirectoryWitness)>,
    ) -> Result<OwnedDestination, RepositoryError> {
        self.witness.validate(&self.ancestor)?;
        let (parent, leaf) = durable_parent(self, &self.destination_relative)?;
        let parent_witness = DirectoryWitness::held(&parent)?;
        parent_witness.validate(&parent)?;
        // Acquire publication authority from the exact actual parent before
        // any destination effect. An unsafe shared parent must not create an
        // unverified checkout that the journal could mistake for its own.
        let namespace = fs_safety::DirectoryNamespace::acquire(&parent)
            .map_err(|error| invalid(format!("The destination parent cannot prove creation ownership; no checkout was created: {error}")))?;
        let publication = match fs_safety::create_directory_in_namespace(
            &namespace,
            &parent,
            Path::new(&leaf),
            0o777,
        )
        .map_err(filesystem)?
        {
            fs_safety::DirectoryCreation::Published(publication) => publication,
            fs_safety::DirectoryCreation::Existing(_)
            | fs_safety::DirectoryCreation::Unverified { .. } => {
                return Err(invalid(
                    "The destination was not newly owned; no creation was started",
                ));
            }
        };
        // The producer captured this identity BEFORE public publication. Read
        // it nonfallibly before cleanup, fstat, fsync or named-root validation.
        let (device, inode) = publication.identity();
        let witness = DirectoryWitness {
            path: self.destination.clone(),
            device,
            inode,
        };
        *known = Some((parent_witness.clone(), witness.clone()));
        let root = self.retain_publication(publication)?;
        #[cfg(test)]
        AFTER_DIRECTORY_PUBLICATION.with(|hook| {
            if let Some(hook) = hook.take() {
                hook(&self.destination);
            }
        });
        #[cfg(test)]
        if FAIL_AFTER_PUBLICATION.with(|failure| failure.replace(false)) {
            return Err(invalid(
                "Injected failure after known destination publication",
            ));
        }
        fs_safety::sync_directory(&root).map_err(filesystem)?;
        fs_safety::sync_directory(&parent).map_err(filesystem)?;
        // The public name is compared with the original retained inode. It
        // never supplies fresh authority, even if it was replaced already.
        witness.validate(&root)?;
        Ok(OwnedDestination {
            root,
            witness,
            parent,
            parent_witness,
            leaf,
        })
    }
}

pub(super) struct OwnedDestination {
    root: fs_safety::Root,
    witness: DirectoryWitness,
    parent: fs_safety::Root,
    parent_witness: DirectoryWitness,
    leaf: OsString,
}

impl OwnedDestination {
    pub(super) fn validate(&self) -> Result<(), RepositoryError> {
        self.parent_witness.validate(&self.parent)?;
        self.witness.validate(&self.root)
    }

    pub(super) fn operand(&self) -> Result<PathBuf, RepositoryError> {
        self.validate()?;
        Ok(anchor(&self.root))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn durable_evidence_rejects_process_descriptor_aliases() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(fixture.path()).unwrap();
        let witness = DirectoryWitness::held(&root).unwrap();
        assert!(witness.reopen().is_ok());
        let alias = DirectoryWitness {
            path: anchor(&root),
            ..witness
        };
        assert!(alias.reopen().is_err());
        let alias_root = fs_safety::Root::open(&alias.path).unwrap();
        assert!(DirectoryWitness::held(&alias_root).is_err());
    }

    #[test]
    fn destination_publication_never_replaces_an_existing_entry() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        fs::create_dir(&source_path).unwrap();
        let source = fs_safety::Root::open(&source_path).unwrap();
        let managed = source_path.join(".worktrees");
        let destination = managed.join("feature/topic");
        let plan = DestinationPlan::capture(&source, &managed, &destination).unwrap();
        assert!(
            !managed.exists(),
            "capturing creation authority is read-only"
        );
        plan.ensure_managed_root().unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("keep"), "foreign work\n").unwrap();
        assert!(plan.publish().result.is_err());
        assert_eq!(
            fs::read(destination.join("keep")).unwrap(),
            b"foreign work\n"
        );
        assert_eq!(
            fs::read_dir(destination.parent().unwrap()).unwrap().count(),
            1,
            "failed no-replace publication removes only its own empty staging directory"
        );
    }

    #[test]
    fn held_destination_operand_cannot_write_a_replaced_source_or_managed_parent() {
        for external in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let source_path = fixture.path().join("source");
            fs::create_dir(&source_path).unwrap();
            let source = fs_safety::Root::open(&source_path).unwrap();
            let managed = if external {
                fixture.path().join("external")
            } else {
                source_path.join(".worktrees")
            };
            let destination = managed.join("topic");
            let plan = DestinationPlan::capture(&source, &managed, &destination).unwrap();
            plan.ensure_managed_root().unwrap();
            let owned = plan.publish().result.unwrap();
            let operand = owned.operand().unwrap();
            let replaced = if external { &managed } else { &source_path };
            let parked = fixture.path().join("parked");
            fs::rename(replaced, &parked).unwrap();
            fs::create_dir_all(&destination).unwrap();
            fs::write(destination.join("keep"), "replacement\n").unwrap();
            // Model an inner executor that delays AFTER the caller validated
            // its command. The already-issued operand still addresses the
            // original held inode, while future authority reads refuse.
            fs::write(operand.join("created"), "owned output\n").unwrap();
            assert!(owned.validate().is_err());
            assert!(!destination.join("created").exists());
            assert_eq!(
                fs::read(destination.join("keep")).unwrap(),
                b"replacement\n"
            );
            let original_destination = if external {
                parked.join("topic")
            } else {
                parked.join(".worktrees/topic")
            };
            assert_eq!(
                fs::read(original_destination.join("created")).unwrap(),
                b"owned output\n"
            );
        }
    }

    #[test]
    fn created_destination_replacement_refuses_new_authority_without_touching_it() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        fs::create_dir(&source_path).unwrap();
        let source = fs_safety::Root::open(&source_path).unwrap();
        let managed = source_path.join(".worktrees");
        let destination = managed.join("topic");
        let plan = DestinationPlan::capture(&source, &managed, &destination).unwrap();
        plan.ensure_managed_root().unwrap();
        let owned = plan.publish().result.unwrap();
        let parked = managed.join("original-topic");
        fs::rename(&destination, &parked).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), "replacement\n").unwrap();
        assert!(owned.operand().is_err());
        assert_eq!(
            fs::read(destination.join("keep")).unwrap(),
            b"replacement\n"
        );
        assert!(fs::read_dir(&parked).unwrap().next().is_none());
    }

    #[test]
    fn typed_publication_records_original_identity_before_replacement_refusal() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        fs::create_dir(&source_path).unwrap();
        let source = fs_safety::Root::open(&source_path).unwrap();
        let managed = source_path.join(".worktrees");
        let destination = managed.join("topic");
        let plan = DestinationPlan::capture(&source, &managed, &destination).unwrap();
        plan.ensure_managed_root().unwrap();
        AFTER_DIRECTORY_PUBLICATION.with(|hook| {
            hook.set(Some(|path| {
                fs::rename(path, path.with_extension("original")).unwrap();
                fs::create_dir(path).unwrap();
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o711)).unwrap();
                fs::write(path.join("foreign"), b"replacement data").unwrap();
            }))
        });
        let publication = plan.publish();
        assert!(publication.result.is_err());
        let (_, witness) = publication
            .known
            .expect("known publication must retain its original inode");
        let original = fs::metadata(destination.with_extension("original")).unwrap();
        assert_eq!(
            (witness.device, witness.inode),
            (original.dev(), original.ino())
        );
        assert_eq!(
            fs::read(destination.join("foreign")).unwrap(),
            b"replacement data"
        );
        assert_eq!(fs::metadata(&destination).unwrap().mode() & 0o777, 0o711);
        assert!(
            fs::read_dir(destination.with_extension("original"))
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            plan.publications
                .lock()
                .unwrap()
                .iter()
                .any(|receipt| receipt.identity() == (witness.device, witness.inode))
        );
    }

    #[test]
    fn unsafe_destination_parent_refuses_before_checkout_creation() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        fs::create_dir(&source_path).unwrap();
        let source = fs_safety::Root::open(&source_path).unwrap();
        let managed = source_path.join(".worktrees");
        fs::create_dir(&managed).unwrap();
        fs::set_permissions(&managed, fs::Permissions::from_mode(0o777)).unwrap();
        let destination = managed.join("topic");
        let plan = DestinationPlan::capture(&source, &managed, &destination).unwrap();
        plan.ensure_managed_root().unwrap();
        let publication = plan.publish();
        assert!(publication.result.is_err());
        assert!(publication.known.is_none());
        assert!(!destination.exists());
        assert_eq!(fs::metadata(&managed).unwrap().mode() & 0o777, 0o777);
        assert!(fs::read_dir(&managed).unwrap().next().is_none());
    }

    #[test]
    fn unsafe_missing_managed_parent_refuses_before_mkdir() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        fs::create_dir(&source_path).unwrap();
        fs::set_permissions(&source_path, fs::Permissions::from_mode(0o777)).unwrap();
        let source = fs_safety::Root::open(&source_path).unwrap();
        let managed = source_path.join(".worktrees");
        let plan = DestinationPlan::capture(&source, &managed, &managed.join("topic")).unwrap();
        assert!(plan.ensure_managed_root().is_err());
        assert!(!managed.exists());
        assert_eq!(fs::metadata(&source_path).unwrap().mode() & 0o777, 0o777);
        assert!(fs::read_dir(source_path).unwrap().next().is_none());
    }
}
