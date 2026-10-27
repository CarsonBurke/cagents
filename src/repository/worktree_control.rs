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
        fs_safety::sync_directory(root)?;
        named(root)
    })();
    let _ = fs_safety::unlink_within(root, &temporary);
    result
}

/// Called only while core settlement retains the original exclusive creation
/// lease and operation-id Git lock. Legacy/replaced rows never create markers
/// from a Resume, pane edit or otherwise read-only callback.
pub(super) fn record_creation(admin: &fs_safety::Root, creation: OperationId) -> io::Result<()> {
    match marker(admin) {
        Ok(existing) if existing == creation => {
            read_disposition(admin, creation)?;
            return Ok(());
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
pub(crate) struct SetupCustody {
    creation: OperationId,
    pub(crate) run: OperationId,
    common: fs_safety::Root,
    admin: fs_safety::Root,
    destination: fs_safety::Root,
    lease: fs::File,
    expected: WorktreeSetupIntent,
}
impl<E: ProcessExecutor> RepositoryService<E> {
    pub(crate) fn worktree_setup_custody(
        &self,
        store: &StateStore,
        creation: OperationId,
        expected: &WorktreeSetupIntent,
    ) -> Result<SetupCustody, String> {
        let receipt = self
            .worktree_setup_creation(store, creation)
            .map_err(|e| e.to_string())?
            .ok_or("Setup creation is missing")?;
        let request = receipt
            .request
            .as_ref()
            .ok_or("Setup has no original request")?;
        if request.setup_intent.as_ref() != Some(expected) || expected.skip {
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
        let custody = SetupCustody {
            creation,
            run: expected.run_id.ok_or("Setup has no run identity")?,
            common: fs_safety::Root::open(&journal.common_dir).map_err(|e| e.to_string())?,
            admin: fs_safety::Root::open(admin).map_err(|e| e.to_string())?,
            destination: fs_safety::Root::open(&request.destination).map_err(|e| e.to_string())?,
            lease,
            expected: expected.clone(),
        };
        custody.verify(self, store)?;
        let state = read_disposition(&custody.admin, creation).map_err(|e| e.to_string())?;
        match state.phase {
            Phase::NeverStarted => {}
            Phase::FamilyReaped { run, .. } if run != custody.run => {}
            _ => return Err("Previous setup has no proved family cleanup, or this run was already attempted; retain the checkout and review a fresh run after original cleanup".into()),
        }
        Ok(custody)
    }
}
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
                && record.branch.as_deref() == Some(request.branch.full_ref().as_str())
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
    pub(crate) fn canonical_recipe(&self) -> Result<crate::model::LaunchRecipe, String> {
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
    pub(crate) fn reserve<E: ProcessExecutor>(
        &self,
        service: &RepositoryService<E>,
        store: &StateStore,
        terminal: crate::model::TerminalId,
        recipe: &crate::model::LaunchRecipe,
    ) -> Result<(), String> {
        self.verify(service, store)?;
        if !self.expected.copy_done || !self.matches_recipe(recipe) {
            return Err("Setup recipe or copied intent does not match the reviewed run".into());
        }
        let mut state = read_disposition(&self.admin, self.creation).map_err(|e| e.to_string())?;
        match state.phase {
            Phase::NeverStarted => {}
            Phase::FamilyReaped { run, .. } if run != self.run => {}
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
    pub(crate) fn duplicate_execution_lease(&self) -> io::Result<fs::File> {
        self.lease.try_clone()
    }
}
