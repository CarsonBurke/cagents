//! Retained generation and setup authority for the daemon worktree service.
//! The holder owns actual execution/cleanup; a free flock never proves cleanup.
use super::*;
use std::os::unix::fs::MetadataExt;
const CREATION: &str = "cagents-setup-creation";
const EXECUTION: &str = "cagents-setup-execution";

pub(super) fn named(root: &fs_safety::Root) -> io::Result<()> {
    let held = root.metadata()?;
    let actual = fs::symlink_metadata(root.path())?;
    if !actual.is_dir() || (held.dev(), held.ino()) != (actual.dev(), actual.ino()) {
        return Err(io::Error::other(
            "Original worktree metadata directory was replaced",
        ));
    }
    Ok(())
}
fn marker(root: &fs_safety::Root) -> io::Result<OperationId> {
    let mut file = fs_safety::open_beneath(root, Path::new(CREATION)).map_err(|error| {
        if error.is_not_found() {
            io::Error::from(io::ErrorKind::NotFound)
        } else {
            io::Error::other(error)
        }
    })?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("Unsafe setup generation marker"));
    }
    let bytes = fs_safety::read_bounded(&mut file, 64)?;
    if bytes.len() != 37 || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("Invalid bounded setup generation marker"));
    }
    let text = std::str::from_utf8(&bytes[..36]).map_err(io::Error::other)?;
    let id: OperationId = text.parse().map_err(io::Error::other)?;
    if id.to_string() != text {
        return Err(io::Error::other("Noncanonical setup generation marker"));
    }
    Ok(id)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Disposition {
    creation: OperationId,
    admin: (u64, u64),
    phase: Phase,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    NeverStarted,
    Reserved {
        run: OperationId,
        terminal: crate::model::TerminalId,
        lease: (u64, u64),
    },
    /// The exact startup was permanently cancelled before any setup authority
    /// could be delivered. A late worker cannot cross that cancelled CAS.
    NotLaunched {
        run: OperationId,
        terminal: crate::model::TerminalId,
        lease: (u64, u64),
    },
    /// Only the exact original holder's proved descendant cleanup can publish
    /// this phase. No service or restart path infers it from an empty census.
    FamilyReaped {
        run: OperationId,
        terminal: crate::model::TerminalId,
        lease: (u64, u64),
    },
}
fn read_disposition(root: &fs_safety::Root, creation: OperationId) -> io::Result<Disposition> {
    named(root)?;
    if marker(root)? != creation {
        return Err(io::Error::other("Setup generation changed"));
    }
    let mut file = fs_safety::open_beneath(root, Path::new(EXECUTION)).map_err(io::Error::other)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other("Unsafe setup disposition"));
    }
    let data = fs_safety::read_bounded(&mut file, 8192)?;
    if data.len() > 8192 {
        return Err(io::Error::other("Oversized setup disposition"));
    }
    let value: Disposition = serde_json::from_slice(&data).map_err(io::Error::other)?;
    let held = root.metadata()?;
    if value.creation != creation || value.admin != (held.dev(), held.ino()) {
        return Err(io::Error::other(
            "Setup disposition belongs to another creation",
        ));
    }
    Ok(value)
}
fn publish(root: &fs_safety::Root, name: &str, bytes: &[u8], new: bool) -> io::Result<()> {
    use std::io::Write;
    named(root)?;
    let (mut file, temporary) =
        fs_safety::create_temporary_in(root, 0o600).map_err(io::Error::other)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        if new {
            fs_safety::rename_new_within(root, &temporary, OsStr::new(name))
                .map_err(io::Error::other)?;
        } else {
            fs_safety::rename_within(root, &temporary, OsStr::new(name))
                .map_err(io::Error::other)?;
        }
        sync_publication(root)
    })();
    let _ = fs_safety::unlink_within(root, &temporary);
    result
}

fn sync_publication(root: &fs_safety::Root) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_AFTER_RENAME.with(|fail| fail.replace(false)) {
        return Err(io::Error::other(
            "injected setup publication directory-sync failure after rename",
        ));
    }
    fs_safety::sync_directory(root)?;
    named(root)
}

#[cfg(test)]
thread_local! { static FAIL_AFTER_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(all(test, feature = "host"))]
pub(crate) fn fail_next_setup_publication_sync_for_test() {
    FAIL_AFTER_RENAME.with(|fail| fail.set(true));
}

/// A marker is inventory evidence only; callers still prove registration and
/// common/admin identity. Missing evidence never initializes a generation.
pub(crate) fn creation_marker(admin: &fs_safety::Root) -> io::Result<OperationId> {
    named(admin)?;
    marker(admin)
}

pub(crate) fn removal_is_quiescent(admin: &fs_safety::Root) -> io::Result<()> {
    let creation = match marker(admin) {
        Ok(creation) => creation,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Initialization publishes disposition first. Missing generation
            // evidence cannot downgrade retained setup authority to legacy.
            return match fs_safety::open_beneath(admin, Path::new(EXECUTION)) {
                Err(error) if error.is_not_found() => Ok(()),
                _ => Err(io::Error::other(
                    "Setup execution evidence exists without its original generation marker; retain this checkout for recovery",
                )),
            };
        }
        Err(error) => return Err(error),
    };
    match read_disposition(admin, creation)?.phase {
        Phase::NeverStarted | Phase::NotLaunched { .. } | Phase::FamilyReaped { .. } => Ok(()),
        Phase::Reserved { .. } => Err(io::Error::other(
            "The original setup family has no proved cleanup; keep this checkout and close or recover its exact retained setup terminal before removal",
        )),
    }
}

/// Called only while core settlement retains the original exclusive creation
/// lease and operation-id Git lock. Legacy/replaced rows never create markers
/// from a Resume, pane edit or otherwise read-only callback.
pub(super) fn record_creation(admin: &fs_safety::Root, creation: OperationId) -> io::Result<()> {
    match marker(admin) {
        Ok(existing) if existing == creation => {
            read_disposition(admin, creation)?;
            // A previous rename may be visible after its directory sync failed.
            // Replaying the same generation must finish that barrier too.
            return sync_publication(admin);
        }
        Ok(_) => {
            return Err(io::Error::other(
                "Another creation owns this administrative generation",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let metadata = admin.metadata()?;
    let disposition = Disposition {
        creation,
        admin: (metadata.dev(), metadata.ino()),
        phase: Phase::NeverStarted,
    };
    // Disposition first: a crash between these publications remains fail-closed
    // and reconciliation may finish the marker only for this exact creation.
    let existing = fs_safety::open_beneath(admin, Path::new(EXECUTION));
    match existing {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::other("Unsafe interrupted setup initialization"));
            }
            let bytes = fs_safety::read_bounded(&mut file, 8192)?;
            let prior: Disposition = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            if prior != disposition {
                return Err(io::Error::other("Setup initialization evidence changed"));
            }
        }
        Err(error) if error.is_not_found() => publish(
            admin,
            EXECUTION,
            &serde_json::to_vec(&disposition).map_err(io::Error::other)?,
            true,
        )?,
        Err(error) => return Err(io::Error::other(error)),
    }
    publish(admin, CREATION, format!("{creation}\n").as_bytes(), true)
}

/// Original directories and creation flock retained through preparation and
/// terminal publication. Actual startup additionally needs the host's SQLite
/// writer/CAS guard and transfer of this same lease to the dedicated holder.
#[cfg(feature = "host")]
pub(crate) struct SetupCustody {
    creation: OperationId,
    pub(crate) run: OperationId,
    common: fs_safety::Root,
    admin: fs_safety::Root,
    destination: fs_safety::Root,
    lease: fs::File,
    protection: crate::worktree_cleanup::Protection,
    expected: WorktreeSetupIntent,
    branch: ValidatedBranchName,
}
#[cfg(feature = "host")]
impl<E: ProcessExecutor> RepositoryService<E> {
    /// Decode inventory evidence without reloading configuration, taking an
    /// execution lease, reconciling Git or granting replay permission.
    pub(crate) fn worktree_setup_creation_from_row(
        &self,
        row: &PendingOperation,
    ) -> Result<(PathBuf, WorktreeOperationReport), StoreError> {
        let journal = super::worktree_setup_intent::settled_setup_journal(row)?;
        Ok((
            journal.common_dir.clone(),
            journal.report(row.created_at_unix_ms),
        ))
    }
    pub(crate) fn worktree_setup_custody(
        &self,
        store: &StateStore,
        creation: OperationId,
        expected: &WorktreeSetupIntent,
    ) -> Result<SetupCustody, String> {
        self.worktree_setup_custody_for_transition(store, creation, expected, false)
    }

    pub(crate) fn worktree_setup_transition_custody(
        &self,
        store: &StateStore,
        creation: OperationId,
        expected: &WorktreeSetupIntent,
    ) -> Result<SetupCustody, String> {
        self.worktree_setup_custody_for_transition(store, creation, expected, true)
    }

    fn worktree_setup_custody_for_transition(
        &self,
        store: &StateStore,
        creation: OperationId,
        expected: &WorktreeSetupIntent,
        transition: bool,
    ) -> Result<SetupCustody, String> {
        let receipt = self
            .worktree_setup_creation(store, creation)
            .map_err(|e| e.to_string())?
            .ok_or("Setup creation is missing")?;
        let request = receipt
            .request
            .as_ref()
            .ok_or("Setup has no original request")?;
        if request.setup_intent.as_ref() != Some(expected) || (expected.skip && !transition) {
            return Err("Setup was skipped or superseded".into());
        }
        let row = store
            .journaled_worktree_operation(creation)
            .map_err(|e| e.to_string())?
            .ok_or("Setup creation disappeared")?;
        let journal: WorktreeJournal =
            serde_json::from_value(row.payload).map_err(|e| e.to_string())?;
        let lease = worktree_lease(&journal.lease_path, false)
            .map_err(|e| format!("Previous setup ownership is still held or unavailable: {e}"))?;
        if journal.lease_identity != Some(lease_file_identity(&lease).map_err(|e| e.to_string())?) {
            return Err("Original creation lease changed".into());
        }
        let admin = journal
            .admin_dir
            .as_ref()
            .ok_or("Legacy setup lacks original administrative identity")?;
        let destination = fs_safety::Root::open(&request.destination).map_err(|e| e.to_string())?;
        let protection = crate::worktree_cleanup::protect_root_operation(&destination)
            .map_err(|e| e.to_string())?;
        let custody = SetupCustody {
            creation,
            run: expected.run_id.ok_or("Setup has no run identity")?,
            common: fs_safety::Root::open(&journal.common_dir).map_err(|e| e.to_string())?,
            admin: fs_safety::Root::open(admin).map_err(|e| e.to_string())?,
            destination,
            lease,
            protection,
            expected: expected.clone(),
            branch: request.branch.clone(),
        };
        custody.verify(self, store)?;
        let state = read_disposition(&custody.admin, creation).map_err(|e| e.to_string())?;
        match state.phase {
            Phase::NeverStarted => {}
            Phase::FamilyReaped { run, .. } | Phase::NotLaunched { run, .. } if transition || run != custody.run => {}
            _ => return Err("Previous setup has no proved family cleanup, or this run was already attempted; retain the checkout and review a fresh run after original cleanup".into()),
        }
        Ok(custody)
    }
}
#[cfg(feature = "host")]
impl SetupCustody {
    pub(crate) fn matches_checkout(
        &self,
        destination: &fs_safety::Root,
        admin: &fs_safety::Root,
        common: &fs_safety::Root,
    ) -> Result<(), String> {
        for (expected, actual) in [
            (&self.destination, destination),
            (&self.admin, admin),
            (&self.common, common),
        ] {
            named(expected).map_err(|error| error.to_string())?;
            named(actual).map_err(|error| error.to_string())?;
            let expected = expected.metadata().map_err(|error| error.to_string())?;
            let actual = actual.metadata().map_err(|error| error.to_string())?;
            if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
                return Err(
                    "Setup copy does not belong to the retained creation directories".into(),
                );
            }
        }
        if marker(admin).map_err(|error| error.to_string())? != self.creation {
            return Err("Setup creation marker changed".into());
        }
        Ok(())
    }
    pub(crate) fn verify<E: ProcessExecutor>(
        &self,
        service: &RepositoryService<E>,
        store: &StateStore,
    ) -> Result<(), String> {
        for root in [&self.common, &self.admin, &self.destination] {
            named(root).map_err(|e| e.to_string())?;
        }
        let current = service
            .worktree_setup_creation(store, self.creation)
            .map_err(|e| e.to_string())?
            .ok_or("Setup creation disappeared")?;
        let request = current
            .request
            .as_ref()
            .ok_or("Setup request disappeared")?;
        if request.setup_intent.as_ref() != Some(&self.expected)
            || request.destination != self.destination.path()
        {
            return Err("Setup intent was superseded before preparation/startup".into());
        }
        self.verify_checkout(service)
    }
    fn verify_checkout<E: ProcessExecutor>(
        &self,
        service: &RepositoryService<E>,
    ) -> Result<(), String> {
        for root in [&self.common, &self.admin, &self.destination] {
            named(root).map_err(|error| error.to_string())?;
        }
        let cancel = CancellationToken::new();
        if service
            .common_dir(self.destination.path(), &cancel)
            .map_err(|e| e.to_string())?
            != self.common.path()
            || service
                .git_dir(self.destination.path(), &cancel)
                .map_err(|e| e.to_string())?
                != self.admin.path()
            || marker(&self.admin).map_err(|e| e.to_string())? != self.creation
        {
            return Err("Setup checkout generation changed".into());
        }
        let records = service
            .list_worktrees(self.destination.path(), &cancel)
            .map_err(|error| error.to_string())?;
        if !records.iter().any(|record| {
            record.path == self.destination.path()
                && record.branch.as_deref() == Some(self.branch.full_ref().as_str())
                && !record.bare
        }) {
            return Err(
                "The original setup branch is no longer registered at this checkout".into(),
            );
        }
        for root in [&self.common, &self.admin, &self.destination] {
            named(root).map_err(|error| error.to_string())?;
        }
        Ok(())
    }
    pub(crate) fn copy_finished<E: ProcessExecutor>(
        &mut self,
        service: &RepositoryService<E>,
        store: &StateStore,
    ) -> Result<(), String> {
        self.verify(service, store)?;
        let mut updated = self.expected.clone();
        updated.copy_done = true;
        service
            .update_worktree_setup_intent(
                store,
                self.creation,
                Some(&self.expected),
                updated.clone(),
            )
            .map_err(|e| e.to_string())?;
        self.expected = updated;
        self.verify(service, store)
    }
    pub(crate) fn begin_reviewed_run<E: ProcessExecutor>(
        &mut self,
        service: &RepositoryService<E>,
        store: &StateStore,
        updated: WorktreeSetupIntent,
    ) -> Result<(), String> {
        self.verify(service, store)?;
        let run = updated
            .run_id
            .ok_or("Reviewed setup has no new run identity")?;
        if run == self.run {
            return Err("A reviewed retry requires a fresh setup run identity".into());
        }
        service
            .update_worktree_setup_intent(
                store,
                self.creation,
                Some(&self.expected),
                updated.clone(),
            )
            .map_err(|error| error.to_string())?;
        self.expected = updated;
        self.run = run;
        self.verify(service, store)
    }

    pub(crate) fn canonical_recipe(&self) -> Result<crate::model::LaunchRecipe, String> {
        if self.expected.skip {
            return Err("This setup run was explicitly skipped".into());
        }
        let plan = self.expected.plan.as_ref().ok_or("Setup plan is missing")?;
        let command = plan
            .commands
            .iter()
            .map(|command| format!("/bin/sh -c '{}'", command.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" && ");
        if command.is_empty() {
            return Err("Copy-only setup has no shell recipe".into());
        }
        Ok(crate::model::LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), command],
            working_directory: self.destination.path().to_owned(),
            environment: BTreeMap::from([
                ("CAGENTS_WORKTREE_SETUP".into(), "1".into()),
                ("CAGENTS_WORKTREE_SETUP_ID".into(), self.run.to_string()),
                (
                    "CAGENTS_WORKTREE_SETUP_CREATION_ID".into(),
                    self.creation.to_string(),
                ),
            ]),
            unset_environment: Default::default(),
        })
    }
    pub(crate) fn matches_recipe(&self, recipe: &crate::model::LaunchRecipe) -> bool {
        let Ok(mut expected) = self.canonical_recipe() else {
            return false;
        };
        if let Some(started) = recipe.environment.get("CAGENTS_WORKTREE_SETUP_STARTED_AT") {
            if !started
                .parse::<u64>()
                .is_ok_and(|value| value.to_string() == *started)
            {
                return false;
            }
            expected
                .environment
                .insert("CAGENTS_WORKTREE_SETUP_STARTED_AT".into(), started.clone());
        }
        *recipe == expected
    }
    /// Call while the original host SQLite writer guard is held, immediately
    /// before child creation. Reserved survives holder/daemon loss; neither a
    /// free flock nor a new UI may turn it back into NeverStarted.
    #[cfg(test)]
    pub(crate) fn reserve<E: ProcessExecutor>(
        &self,
        service: &RepositoryService<E>,
        store: &StateStore,
        terminal: crate::model::TerminalId,
        recipe: &crate::model::LaunchRecipe,
    ) -> Result<(), String> {
        self.verify(service, store)?;
        self.reserve_phase(terminal, recipe)
    }
    #[cfg(test)]
    fn reserve_phase(
        &self,
        terminal: crate::model::TerminalId,
        recipe: &crate::model::LaunchRecipe,
    ) -> Result<(), String> {
        if !self.expected.copy_done || !self.matches_recipe(recipe) {
            return Err("Setup recipe or copied intent does not match the reviewed run".into());
        }
        let mut state = read_disposition(&self.admin, self.creation).map_err(|e| e.to_string())?;
        match state.phase {
            Phase::NeverStarted => {}
            Phase::FamilyReaped { run, .. } | Phase::NotLaunched { run, .. } if run != self.run => {
            }
            _ => {
                return Err(
                    "This setup execution is already reserved or cleanup remains unproved".into(),
                );
            }
        }
        state.phase = Phase::Reserved {
            run: self.run,
            terminal,
            lease: lease_file_identity(&self.lease).map_err(|e| e.to_string())?,
        };
        publish(
            &self.admin,
            EXECUTION,
            &serde_json::to_vec(&state).map_err(|e| e.to_string())?,
            false,
        )
        .map_err(|e| e.to_string())
    }
    pub(crate) fn start<E: ProcessExecutor, T>(
        &self,
        service: &RepositoryService<E>,
        store: &StateStore,
        terminal: crate::model::TerminalId,
        recipe: &crate::model::LaunchRecipe,
        spawn: impl FnOnce(crate::terminal::holder::setup::Authority) -> Result<T, String>,
    ) -> Result<T, String> {
        self.verify(service, store)?;
        let row = store
            .journaled_worktree_operation(self.creation)
            .map_err(|error| error.to_string())?
            .ok_or("Setup creation disappeared")?;
        // Authority is prepared first; only the existing ready holder may
        // publish Reserved, after its exact Commit and before creating a PTY.
        let authority = self.holder_authority(terminal)?;
        store.with_worktree_operation_writer(&row, || {
            self.verify_checkout(service)?;
            if !self.expected.copy_done || !self.matches_recipe(recipe) {
                return Err("Setup recipe or copied intent does not match the reviewed run".into());
            }
            verify_prepared_setup_holder(&self.admin, &authority.binding)?;
            let _no_launch = NoLaunchGuard {
                authority: &authority,
            };
            spawn(authority.clone())
        })
    }
    pub(crate) fn duplicate_execution_lease(&self) -> io::Result<fs::File> {
        self.lease.try_clone()
    }
    pub(crate) fn retained_directory_files(&self) -> io::Result<Vec<std::sync::Arc<fs::File>>> {
        self.protection.retained_child_files()
    }
}

/// Validate inactive startup authority before the holder announces Ready.
#[cfg(feature = "host")]
pub(crate) fn verify_prepared_setup_holder(
    admin: &fs_safety::Root,
    binding: &crate::terminal::holder::setup::Binding,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(&binding.admin_path).map_err(|error| error.to_string())?;
    let held = admin.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || (metadata.dev(), metadata.ino()) != binding.admin
        || (held.dev(), held.ino()) != binding.admin
    {
        return Err("Original prepared setup administrative generation changed".into());
    }
    let predecessor = prepared_predecessor(binding)?;
    if read_disposition(admin, binding.creation).map_err(|error| error.to_string())? != predecessor
    {
        return Err("Original prepared setup disposition changed before holder commit".into());
    }
    Ok(())
}
#[cfg(feature = "host")]
fn prepared_predecessor(
    binding: &crate::terminal::holder::setup::Binding,
) -> Result<Disposition, String> {
    if binding.predecessor.is_empty() || binding.predecessor.len() > 8192 {
        return Err("Invalid prepared setup predecessor bound".into());
    }
    let predecessor: Disposition =
        serde_json::from_slice(&binding.predecessor).map_err(|error| error.to_string())?;
    if predecessor.creation != binding.creation
        || predecessor.admin != binding.admin
        || !match predecessor.phase {
            Phase::NeverStarted => true,
            Phase::FamilyReaped { run, .. } | Phase::NotLaunched { run, .. } => run != binding.run,
            _ => false,
        }
    {
        return Err("Setup predecessor does not prove an inactive original generation".into());
    }
    Ok(predecessor)
}
/// Called only after Commit by the already-existing sole-reaper holder.
#[cfg(feature = "host")]
pub(crate) fn reserve_setup_holder(
    admin: &fs_safety::Root,
    binding: &crate::terminal::holder::setup::Binding,
) -> Result<(), String> {
    verify_prepared_setup_holder(admin, binding)?;
    let mut state = prepared_predecessor(binding)?;
    state.phase = Phase::Reserved {
        run: binding.run,
        terminal: binding.terminal,
        lease: binding.lease,
    };
    publish(
        admin,
        EXECUTION,
        &serde_json::to_vec(&state).map_err(|error| error.to_string())?,
        false,
    )
    .map_err(|error| error.to_string())?;
    verify_reserved_setup_holder(admin, binding)
}
/// Called only by the exact execed holder after typed descriptor validation.
#[cfg(feature = "host")]
pub(crate) fn verify_reserved_setup_holder(
    admin: &fs_safety::Root,
    binding: &crate::terminal::holder::setup::Binding,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(&binding.admin_path).map_err(|error| error.to_string())?;
    let held = admin.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || (metadata.dev(), metadata.ino()) != binding.admin
        || (held.dev(), held.ino()) != binding.admin
    {
        return Err("Original setup administrative generation changed".into());
    }
    let state = read_disposition(admin, binding.creation).map_err(|error| error.to_string())?;
    if state.phase
        != (Phase::Reserved {
            run: binding.run,
            terminal: binding.terminal,
            lease: binding.lease,
        })
    {
        return Err("Setup holder does not own this exact Reserved execution".into());
    }
    Ok(())
}
#[cfg(feature = "host")]
pub(crate) fn publish_setup_family_reaped(
    admin: &fs_safety::Root,
    binding: &crate::terminal::holder::setup::Binding,
    _proof: &crate::terminal::holder::setup::ReapedFamily,
) -> Result<(), String> {
    publish_reaped_disposition(admin, binding)
}
#[cfg(feature = "host")]
fn publish_reaped_disposition(
    admin: &fs_safety::Root,
    binding: &crate::terminal::holder::setup::Binding,
) -> Result<(), String> {
    // Cleanup publishes only through the original held directory. Relocating
    // its pathname cannot strand an already reaped family or redirect proof
    // into a replacement namespace. Launch validation remains strictly named.
    let (_pin, held_admin) = held_admin_root(admin).map_err(|error| error.to_string())?;
    let admin = &held_admin;
    let held = admin.metadata().map_err(|error| error.to_string())?;
    if (held.dev(), held.ino()) != binding.admin {
        return Err("Original setup administrative identity changed".into());
    }
    let mut state = read_disposition(admin, binding.creation).map_err(|error| error.to_string())?;
    if state == prepared_predecessor(binding)? {
        // No Commit/reservation occurred: this existing holder proved ECHILD
        // and must leave the exact inactive predecessor unchanged.
        fs_safety::sync_directory(admin).map_err(|error| error.to_string())?;
        if read_disposition(admin, binding.creation).map_err(|error| error.to_string())? != state {
            return Err("Prepared setup changed during empty-family cleanup".into());
        }
        return Ok(());
    }
    let completed = Phase::FamilyReaped {
        run: binding.run,
        terminal: binding.terminal,
        lease: binding.lease,
    };
    if state.phase
        == (Phase::NotLaunched {
            run: binding.run,
            terminal: binding.terminal,
            lease: binding.lease,
        })
    {
        // The canonical startup permanently cancelled commit before a shell
        // could run. This original holder independently has ECHILD as well.
        fs_safety::sync_directory(admin).map_err(|error| error.to_string())?;
        let after = read_disposition(admin, binding.creation).map_err(|error| error.to_string())?;
        if after != state {
            return Err("Original no-launch setup disposition changed after sync".into());
        }
        return Ok(());
    }
    if state.phase == completed {
        // A prior rename may have succeeded before directory fsync failed.
        // Only this exact holder's new ECHILD proof may finish its own write.
        fs_safety::sync_directory(admin).map_err(|error| error.to_string())?;
        return verify_completed_setup_holder(admin, binding);
    }
    if state.phase
        != (Phase::Reserved {
            run: binding.run,
            terminal: binding.terminal,
            lease: binding.lease,
        })
    {
        return Err("Another setup execution owns this cleanup disposition".into());
    }
    state.phase = completed;
    publish(
        admin,
        EXECUTION,
        &serde_json::to_vec(&state).map_err(|error| error.to_string())?,
        false,
    )
    .map_err(|error| error.to_string())?;
    verify_completed_setup_holder(admin, binding)
}
#[cfg(feature = "host")]
fn held_admin_root(admin: &fs_safety::Root) -> io::Result<(fs::File, fs_safety::Root)> {
    let pin = admin.retained_directory()?;
    let root = fs_safety::Root::open(&PathBuf::from(format!(
        "/proc/self/fd/{}/.",
        pin.as_raw_fd()
    )))?;
    Ok((pin, root))
}
#[cfg(feature = "host")]
fn verify_completed_setup_holder(
    admin: &fs_safety::Root,
    binding: &crate::terminal::holder::setup::Binding,
) -> Result<(), String> {
    let state = read_disposition(admin, binding.creation).map_err(|error| error.to_string())?;
    if state.phase
        != (Phase::FamilyReaped {
            run: binding.run,
            terminal: binding.terminal,
            lease: binding.lease,
        })
    {
        return Err("Setup cleanup publication changed".into());
    }
    let held = admin.metadata().map_err(|error| error.to_string())?;
    if (held.dev(), held.ino()) != binding.admin {
        return Err("Original setup administrative identity changed after publication".into());
    }
    Ok(())
}

#[cfg(feature = "host")]
impl SetupCustody {
    pub(crate) fn holder_authority(
        &self,
        terminal: crate::model::TerminalId,
    ) -> Result<crate::terminal::holder::setup::Authority, String> {
        let protection = self
            .retained_directory_files()
            .map_err(|error| error.to_string())?;
        if protection.is_empty() || protection.len() + 2 > crate::terminal::holder::setup::MAX_FILES
        {
            return Err("Setup directory protection exceeds bounded holder authority".into());
        }
        let admin = std::sync::Arc::new(
            self.admin
                .retained_directory()
                .map_err(|error| error.to_string())?,
        );
        let lease = std::sync::Arc::new(
            self.duplicate_execution_lease()
                .map_err(|error| error.to_string())?,
        );
        let binding = crate::terminal::holder::setup::Binding {
            creation: self.creation,
            run: self.run,
            terminal,
            admin_path: self.admin.path().to_owned(),
            admin: lease_file_identity(&admin).map_err(|error| error.to_string())?,
            lease: lease_file_identity(&lease).map_err(|error| error.to_string())?,
            predecessor: serde_json::to_vec(
                &read_disposition(&self.admin, self.creation).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?,
            #[cfg(test)]
            fail_reservation_sync: false,
            protection: protection
                .iter()
                .map(|file| lease_file_identity(file).map_err(|error| error.to_string()))
                .collect::<Result<_, _>>()?,
        };
        Ok(crate::terminal::holder::setup::Authority::new(
            binding, admin, lease, protection,
        ))
    }
}

/// Scoped to the original canonical writer/start attempt, including panic
/// unwinding. A worker and pane withdrawal share the same delivery CAS; a
/// cancelled original attempt can never commit its prepared holder.
#[cfg(feature = "host")]
struct NoLaunchGuard<'a> {
    authority: &'a crate::terminal::holder::setup::Authority,
}
#[cfg(feature = "host")]
impl Drop for NoLaunchGuard<'_> {
    fn drop(&mut self) {
        let _ = self.authority.cancel_before_commit();
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    fn original() -> (
        tempfile::TempDir,
        fs_safety::Root,
        crate::terminal::holder::setup::Binding,
    ) {
        use std::os::unix::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().unwrap();
        let admin = fs_safety::Root::open(temp.path()).unwrap();
        let creation = OperationId::new();
        record_creation(&admin, creation).unwrap();
        let lease = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(temp.path().join("original.lease"))
            .unwrap();
        let binding = crate::terminal::holder::setup::Binding {
            creation,
            run: OperationId::new(),
            terminal: crate::model::TerminalId::new(),
            admin_path: temp.path().to_owned(),
            admin: lease_file_identity(&admin.retained_directory().unwrap()).unwrap(),
            lease: lease_file_identity(&lease).unwrap(),
            protection: vec![lease_file_identity(&lease).unwrap()],
            predecessor: serde_json::to_vec(&read_disposition(&admin, creation).unwrap()).unwrap(),
            fail_reservation_sync: false,
        };
        let state = Disposition {
            creation,
            admin: binding.admin,
            phase: Phase::Reserved {
                run: binding.run,
                terminal: binding.terminal,
                lease: binding.lease,
            },
        };
        publish(
            &admin,
            EXECUTION,
            &serde_json::to_vec(&state).unwrap(),
            false,
        )
        .unwrap();
        (temp, admin, binding)
    }
    #[test]
    fn exact_family_publication_can_finish_its_rename_after_directory_sync_failure() {
        let (_temp, admin, binding) = original();
        fail_next_setup_publication_sync_for_test();
        assert!(
            publish_reaped_disposition(&admin, &binding)
                .unwrap_err()
                .contains("after rename")
        );
        assert_eq!(
            read_disposition(&admin, binding.creation).unwrap().phase,
            Phase::FamilyReaped {
                run: binding.run,
                terminal: binding.terminal,
                lease: binding.lease
            }
        );
        publish_reaped_disposition(&admin, &binding).unwrap();
        let mut stale = binding.clone();
        stale.run = OperationId::new();
        assert!(
            publish_reaped_disposition(&admin, &stale).is_err(),
            "another run cannot reuse the completed write"
        );
    }
    #[test]
    fn creation_replay_finishes_the_failed_directory_barrier_without_rewriting() {
        let temp = tempfile::tempdir().unwrap();
        let admin = fs_safety::Root::open(temp.path()).unwrap();
        let creation = OperationId::new();
        let metadata = admin.metadata().unwrap();
        let disposition = Disposition {
            creation,
            admin: (metadata.dev(), metadata.ino()),
            phase: Phase::NeverStarted,
        };
        let expected_execution = serde_json::to_vec(&disposition).unwrap();
        publish(&admin, EXECUTION, &expected_execution, true).unwrap();

        // EXECUTION is already durable; fail the later CREATION rename barrier.
        fail_next_setup_publication_sync_for_test();
        assert!(record_creation(&admin, creation).is_err());
        assert_eq!(marker(&admin).unwrap(), creation);
        assert_eq!(read_disposition(&admin, creation).unwrap(), disposition);
        let expected_creation = format!("{creation}\n").into_bytes();
        let assert_unchanged = || {
            assert_eq!(
                fs::read(temp.path().join(CREATION)).unwrap(),
                expected_creation
            );
            assert_eq!(
                fs::read(temp.path().join(EXECUTION)).unwrap(),
                expected_execution
            );
        };
        assert_unchanged();

        // Visible matching bytes cannot turn a second failed barrier into success.
        fail_next_setup_publication_sync_for_test();
        assert!(record_creation(&admin, creation).is_err());
        assert_unchanged();
        record_creation(&admin, creation).unwrap();
        assert_unchanged();
        assert!(removal_is_quiescent(&admin).is_ok());
        assert!(record_creation(&admin, OperationId::new()).is_err());
        assert_unchanged();
    }

    #[test]
    fn interrupted_disposition_first_initialization_only_finishes_same_creation() {
        let temp = tempfile::tempdir().unwrap();
        let admin = fs_safety::Root::open(temp.path()).unwrap();
        let creation = OperationId::new();
        fail_next_setup_publication_sync_for_test();
        assert!(record_creation(&admin, creation).is_err());
        assert!(marker(&admin).is_err());
        assert!(removal_is_quiescent(&admin).is_err());
        assert!(record_creation(&admin, OperationId::new()).is_err());
        record_creation(&admin, creation).unwrap();
        assert_eq!(marker(&admin).unwrap(), creation);
        assert!(removal_is_quiescent(&admin).is_ok());
    }
    #[test]
    fn cleanup_publication_uses_original_admin_after_rename_without_touching_replacement() {
        let (temp, admin, binding) = original();
        let parent = tempfile::tempdir().unwrap();
        let parked = parent.path().join("original-admin");
        fs::rename(temp.path(), &parked).unwrap();
        fs::create_dir(temp.path()).unwrap();
        publish_reaped_disposition(&admin, &binding).unwrap();
        let original = fs_safety::Root::open(&parked).unwrap();
        assert_eq!(
            read_disposition(&original, binding.creation).unwrap().phase,
            Phase::FamilyReaped {
                run: binding.run,
                terminal: binding.terminal,
                lease: binding.lease
            }
        );
        assert!(
            !temp.path().join(EXECUTION).exists(),
            "replacement namespace cannot acquire original cleanup evidence"
        );
    }
}
