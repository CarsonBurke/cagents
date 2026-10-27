//! Explicit recovery of a settled creation's leftover Git worktree lock.
//!
//! Only the supplied retained directories grant authority. No path discovery,
//! setup replay, process signalling, or absence-of-flock cleanup inference is
//! performed here. As with Git's own locks, the final check/unlink assumes
//! cooperating writers: Linux offers no compare-inode-and-unlink operation.
use super::*;
#[cfg(feature = "host")]
use std::{io::Seek, os::unix::fs::MetadataExt};

#[cfg(feature = "host")]
type FileVersion = (u64, u64, u64, i64, i64, i64, i64);

#[cfg(feature = "host")]
struct RetainedLeaf {
    file: fs::File,
    version: FileVersion,
    bytes: Vec<u8>,
}

#[cfg(feature = "host")]
impl RetainedLeaf {
    fn read(root: &fs_safety::Root, name: &str, limit: u64) -> Result<Self, String> {
        let mut file =
            fs_safety::open_beneath(root, Path::new(name)).map_err(|error| error.to_string())?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(format!("Unsafe worktree recovery evidence: {name}"));
        }
        let version = GitFileLock::version(&metadata);
        let bytes = fs_safety::read_bounded(&mut file, limit).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > limit {
            return Err(format!("Oversized worktree recovery evidence: {name}"));
        }
        let mut leaf = Self {
            file,
            version,
            bytes,
        };
        leaf.verify(root, name)?;
        Ok(leaf)
    }

    fn verify(&mut self, root: &fs_safety::Root, name: &str) -> Result<(), String> {
        let current = fs_safety::metadata_beneath(root, Path::new(name))
            .map_err(|error| error.to_string())?;
        let held = self.file.metadata().map_err(|error| error.to_string())?;
        if GitFileLock::version(&held) != self.version
            || GitFileLock::version(&current) != self.version
        {
            return Err(format!("Worktree recovery evidence changed: {name}"));
        }
        self.file.rewind().map_err(|error| error.to_string())?;
        let bytes = fs_safety::read_bounded(&mut self.file, self.bytes.len() as u64)
            .map_err(|error| error.to_string())?;
        if bytes != self.bytes
            || GitFileLock::version(&self.file.metadata().map_err(|error| error.to_string())?)
                != self.version
        {
            return Err(format!("Worktree recovery evidence changed: {name}"));
        }
        Ok(())
    }
}

// This is the existing on-disk setup disposition, not a new receipt or a
// replacement for the original holder. Reject unknown fields/phases rather
// than interpreting a future format as a settled execution.
#[cfg(feature = "host")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettledDisposition {
    creation: OperationId,
    admin: (u64, u64),
    phase: SettledPhase,
}

#[cfg(feature = "host")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum SettledPhase {
    NeverStarted,
    NotLaunched {
        run: OperationId,
        terminal: crate::model::TerminalId,
        lease: (u64, u64),
    },
    FamilyReaped {
        run: OperationId,
        terminal: crate::model::TerminalId,
        lease: (u64, u64),
    },
}

#[cfg(feature = "host")]
struct StaleLockGuard {
    common: fs_safety::Root,
    admin: fs_safety::Root,
    checkout: fs_safety::Root,
    _protection: crate::worktree_cleanup::Protection,
    lease: fs::File,
    operation: PendingOperation,
    journal: WorktreeJournal,
    marker: RetainedLeaf,
    disposition: RetainedLeaf,
    lock: RetainedLeaf,
}

#[cfg(feature = "host")]
impl<E: ProcessExecutor> RepositoryService<E> {
    /// Read-only eligibility, with no legacy/missing-receipt fallback. The
    /// service must already have admitted this exact repository and checkout.
    pub(crate) fn stale_worktree_lock_at(
        &self,
        store: &StateStore,
        common: &fs_safety::Root,
        admin: &fs_safety::Root,
        checkout: &fs_safety::Root,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let mut guard = self.stale_lock_guard(store, common, admin, checkout, cancellation)?;
        store.with_worktree_operation_writer(&guard.operation.clone(), || {
            guard.verify(self, cancellation)
        })
    }

    /// Explicit user-requested unlock. Retain the original creation lease,
    /// canonical journal writer and cleanup protection through the exact
    /// administrative marker removal and its directory sync.
    pub(crate) fn unlock_stale_worktree_at(
        &self,
        store: &StateStore,
        common: &fs_safety::Root,
        admin: &fs_safety::Root,
        checkout: &fs_safety::Root,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let mut guard = self.stale_lock_guard(store, common, admin, checkout, cancellation)?;
        store.with_worktree_operation_writer(&guard.operation.clone(), || {
            guard.verify(self, cancellation)?;
            // Git worktree unlock removes exactly this marker. Resolving it
            // through the retained admin avoids a mutable checkout .git route.
            fs_safety::unlink_within(&guard.admin, OsStr::new("locked"))
                .map_err(|error| format!("Git lock removal did not report success; inspect the current lock before retrying: {error}"))?;
            let settled = (|| -> Result<(), String> {
                #[cfg(test)]
                if FAIL_UNLOCK_SYNC.with(|fail| fail.replace(false)) {
                    return Err("injected directory sync failure after unlink".into());
                }
                fs_safety::sync_directory(&guard.admin).map_err(|error| error.to_string())?;
                guard.verify_roots_and_generation()?;
                match fs_safety::metadata_beneath(&guard.admin, Path::new("locked")) {
                    Err(error) if error.is_not_found() => Ok(()),
                    _ => Err("A new Git worktree lock appeared during unlock".into()),
                }
            })();
            settled.map_err(|error| format!(
                "Worktree unlock removed the original Git lock, but settlement is uncertain; inspect this checkout and do not retry automatically: {error}"
            ))
        })
    }

    fn stale_lock_guard(
        &self,
        store: &StateStore,
        common: &fs_safety::Root,
        admin: &fs_safety::Root,
        checkout: &fs_safety::Root,
        cancellation: &CancellationToken,
    ) -> Result<StaleLockGuard, String> {
        if cancellation.is_cancelled() {
            return Err("Cancelled before worktree unlock; its lock was retained".into());
        }
        check_roots(common, admin, checkout)?;
        let protection = crate::worktree_cleanup::protect_root_operation(checkout)
            .map_err(|error| error.to_string())?;
        let creation = worktree_control::creation_marker(admin).map_err(|error| {
            format!("Original setup generation is unavailable; retain its lock: {error}")
        })?;
        // Select by the current physical generation, never journal iteration
        // order or a familiar label attached to a reused checkout path.
        let operation = store
            .journaled_worktree_operation(creation)
            .map_err(|error| error.to_string())?
            .ok_or("No canonical creation receipt for the current setup generation")?;
        let journal = worktree_setup_intent::settled_setup_journal(&operation)
            .map_err(|error| error.to_string())?;
        if journal.common_dir != common.path()
            || journal.admin_dir.as_deref() != Some(admin.path())
            || journal.request.destination != checkout.path()
        {
            return Err("Creation receipt does not own these retained directories".into());
        }
        let lease_name = format!("cagents-operation-{creation}.lease");
        let lease = worktree_lease(&common.process_path().join(&lease_name), false)
            .map_err(|error| format!("Creation lease is active or unavailable: {error}"))?;
        let lease_identity = lease_file_identity(&lease).map_err(|error| error.to_string())?;
        if journal.lease_identity != Some(lease_identity) {
            return Err("Original creation lease identity changed".into());
        }
        let mut guard = StaleLockGuard {
            common: common.try_clone().map_err(|error| error.to_string())?,
            admin: admin.try_clone().map_err(|error| error.to_string())?,
            checkout: checkout.try_clone().map_err(|error| error.to_string())?,
            _protection: protection,
            lease,
            operation,
            journal,
            marker: RetainedLeaf::read(admin, "cagents-setup-creation", 64)?,
            disposition: RetainedLeaf::read(admin, "cagents-setup-execution", 8192)?,
            lock: RetainedLeaf::read(admin, "locked", 128)?,
        };
        guard.verify(self, cancellation)?;
        Ok(guard)
    }
}

#[cfg(feature = "host")]
fn check_roots(
    common: &fs_safety::Root,
    admin: &fs_safety::Root,
    checkout: &fs_safety::Root,
) -> Result<(), String> {
    for root in [common, admin, checkout] {
        worktree_control::named(root).map_err(|error| error.to_string())?;
        crate::secure_file::reject_symlink_components(root.path())
            .map_err(|error| error.to_string())?;
        if fs::canonicalize(root.path()).map_err(|error| error.to_string())? != root.path() {
            return Err("Worktree recovery requires the exact canonical directory names".into());
        }
    }
    let relative = admin
        .path()
        .strip_prefix(common.path())
        .map_err(|_| "Administrative directory is outside the retained common directory")?;
    if admin.path().parent() != Some(common.path().join("worktrees").as_path()) {
        return Err("Only a linked worktree administrative directory may be unlocked".into());
    }
    let registered_admin =
        fs_safety::open_directory_beneath(common, relative).map_err(|error| error.to_string())?;
    let held = admin.metadata().map_err(|error| error.to_string())?;
    let actual = registered_admin
        .metadata()
        .map_err(|error| error.to_string())?;
    if (held.dev(), held.ino()) != (actual.dev(), actual.ino()) {
        return Err("Administrative directory changed in the retained common namespace".into());
    }
    for (root, name, expected) in [
        (
            checkout,
            ".git",
            [
                b"gitdir: ".as_slice(),
                admin.path().as_os_str().as_encoded_bytes(),
            ]
            .concat(),
        ),
        (
            admin,
            "gitdir",
            checkout
                .path()
                .join(".git")
                .as_os_str()
                .as_encoded_bytes()
                .to_vec(),
        ),
    ] {
        if trim_one_line_ending(&RetainedLeaf::read(root, name, 8192)?.bytes) != expected {
            return Err("Original checkout and administrative backlinks no longer agree".into());
        }
    }
    let commondir = RetainedLeaf::read(admin, "commondir", 8192)?;
    let bytes = trim_one_line_ending(&commondir.bytes);
    if bytes != b"../.." && bytes != common.path().as_os_str().as_encoded_bytes() {
        return Err("Administrative directory has a different common-directory backlink".into());
    }
    Ok(())
}

#[cfg(feature = "host")]
impl StaleLockGuard {
    fn verify_roots_and_generation(&mut self) -> Result<(), String> {
        check_roots(&self.common, &self.admin, &self.checkout)?;
        let creation = self.journal.request.operation_id;
        if worktree_control::creation_marker(&self.admin).map_err(|error| error.to_string())?
            != creation
        {
            return Err("Current setup generation changed".into());
        }
        self.marker.verify(&self.admin, "cagents-setup-creation")?;
        self.disposition
            .verify(&self.admin, "cagents-setup-execution")?;
        let disposition: SettledDisposition = serde_json::from_slice(&self.disposition.bytes)
            .map_err(|error| format!("No positive settled setup disposition: {error}"))?;
        let admin = self.admin.metadata().map_err(|error| error.to_string())?;
        let lease = lease_file_identity(&self.lease).map_err(|error| error.to_string())?;
        if disposition.creation != creation
            || disposition.admin != (admin.dev(), admin.ino())
            || self.journal.lease_identity != Some(lease)
        {
            return Err(
                "Setup disposition does not belong to the original generation and lease".into(),
            );
        }
        match disposition.phase {
            SettledPhase::NeverStarted => {}
            SettledPhase::NotLaunched {
                run,
                terminal,
                lease: phase_lease,
            }
            | SettledPhase::FamilyReaped {
                run,
                terminal,
                lease: phase_lease,
            } => {
                // A reviewed retry/skip may already have assigned a fresh
                // intent run while the original holder's positive cleanup
                // remains the current disposition. That transition does not
                // execute anything. A new actual startup must publish Reserved
                // while owning this same creation lease, which we retain.
                let _ = (run, terminal);
                if phase_lease != lease {
                    return Err("Settled setup receipt belongs to another lease".into());
                }
            }
        }
        // Also retain the production phase interpretation. Missing markers
        // cannot take its legacy fallback because the strict marker/receipt
        // checks above have already succeeded.
        worktree_control::removal_is_quiescent(&self.admin).map_err(|error| error.to_string())?;
        let name = format!("cagents-operation-{creation}.lease");
        let current = fs_safety::metadata_beneath(&self.common, Path::new(&name))
            .map_err(|error| error.to_string())?;
        if !current.is_file()
            || (current.dev(), current.ino()) != lease
            || current.nlink() != 1
            || current.uid() != unsafe { libc::geteuid() }
            || current.mode() & 0o077 != 0
        {
            return Err("Current creation lease was replaced".into());
        }
        Ok(())
    }

    fn verify<E: ProcessExecutor>(
        &mut self,
        service: &RepositoryService<E>,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        if cancellation.is_cancelled() {
            return Err("Cancelled before worktree unlock; its lock was retained".into());
        }
        self.verify_roots_and_generation()?;
        let creation = self.journal.request.operation_id;
        let expected = format!("cagents:{creation}");
        if trim_one_line_ending(&self.lock.bytes) != expected.as_bytes() {
            return Err("This Git lock is foreign or belongs to another creation".into());
        }
        let records = service
            .list_worktrees(&self.common.process_path(), cancellation)
            .map_err(|error| error.to_string())?;
        let record = records
            .iter()
            .find(|record| record.path == self.checkout.path())
            .ok_or("Worktree is no longer registered")?;
        if record.bare
            || record.detached
            || record.branch.as_deref() != Some(self.journal.request.branch.full_ref().as_str())
            || !record.lock.locked
            || record.lock.reason.as_deref() != Some(expected.as_str())
        {
            return Err("Current registered worktree, branch or lock ownership changed".into());
        }
        self.verify_roots_and_generation()?;
        self.lock.verify(&self.admin, "locked")?;
        if cancellation.is_cancelled() {
            return Err("Cancelled before worktree unlock; its lock was retained".into());
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "host"))]
thread_local! { static FAIL_UNLOCK_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use std::os::unix::fs::{OpenOptionsExt, symlink};

    struct Inventory {
        checkout: PathBuf,
        admin: PathBuf,
    }
    impl ProcessExecutor for Inventory {
        fn execute(
            &self,
            command: &CommandSpec,
            _: &CancellationToken,
        ) -> Result<ProcessOutput, ProcessFailure> {
            // This fixture is offline: its sole process seam is Git's current
            // registration read. No Git mutation, setup or helper is executed.
            assert!(
                command
                    .arguments
                    .windows(2)
                    .any(|args| args[0] == "worktree" && args[1] == "list")
            );
            let mut bytes = b"worktree ".to_vec();
            bytes.extend_from_slice(self.checkout.as_os_str().as_encoded_bytes());
            bytes.extend_from_slice(
                b"\0HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0branch refs/heads/topic\0",
            );
            if let Ok(reason) = fs::read(self.admin.join("locked")) {
                bytes.extend_from_slice(b"locked ");
                bytes.extend_from_slice(trim_one_line_ending(&reason));
                bytes.push(0);
            }
            bytes.push(0);
            Ok(ProcessOutput {
                exit_code: Some(0),
                signal: None,
                success: true,
                stdout: bytes,
                stderr: Vec::new(),
            })
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        common: fs_safety::Root,
        admin: fs_safety::Root,
        checkout: fs_safety::Root,
        store: StateStore,
        service: RepositoryService<Inventory>,
        creation: OperationId,
        run: OperationId,
        lease: (u64, u64),
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().canonicalize().unwrap();
            let common = path.join("common");
            let admin = common.join("worktrees/topic");
            let managed = path.join("managed");
            let checkout = managed.join("topic");
            fs::create_dir_all(&admin).unwrap();
            fs::create_dir_all(&checkout).unwrap();
            fs::write(
                checkout.join(".git"),
                [
                    b"gitdir: ".as_slice(),
                    admin.as_os_str().as_encoded_bytes(),
                    b"\n",
                ]
                .concat(),
            )
            .unwrap();
            fs::write(
                admin.join("gitdir"),
                [checkout.join(".git").as_os_str().as_encoded_bytes(), b"\n"].concat(),
            )
            .unwrap();
            fs::write(admin.join("commondir"), b"../..\n").unwrap();
            let creation = OperationId::new();
            let run = OperationId::new();
            let lease_path = common.join(format!("cagents-operation-{creation}.lease"));
            let held = worktree_lease(&lease_path, true).unwrap();
            let lease = lease_file_identity(&held).unwrap();
            drop(held);
            let common_root = fs_safety::Root::open(&common).unwrap();
            let admin_root = fs_safety::Root::open(&admin).unwrap();
            let checkout_root = fs_safety::Root::open(&checkout).unwrap();
            worktree_control::record_creation(&admin_root, creation).unwrap();
            fs::write(admin.join("locked"), format!("cagents:{creation}\n")).unwrap();
            let request = CreateWorktreeRequest {
                repository: checkout.clone(),
                managed_root: managed,
                managed_ignore_root: None,
                destination: checkout.clone(),
                branch: ValidatedBranchName::parse("topic").unwrap(),
                base: BaseRevision::LocalBranch(ValidatedBranchName::parse("main").unwrap()),
                track: false,
                operation_id: creation,
                setup_intent: Some(WorktreeSetupIntent {
                    run_id: Some(run),
                    ..Default::default()
                }),
            };
            let record = WorktreeRecord {
                id: WorktreeId::new(),
                path: checkout.clone(),
                created_at_unix_ms: Some(1),
                head_oid: Some("a".repeat(40)),
                branch: Some("refs/heads/topic".into()),
                detached: false,
                bare: false,
                lock: WorktreeLock {
                    locked: true,
                    reason: Some(format!("cagents:{creation}")),
                },
                prunable_reason: None,
            };
            let journal = WorktreeJournal {
                version: 1,
                request,
                common_dir: common.clone(),
                generation: 7,
                state: WorktreeOperationState::Created,
                lease_path,
                lease_identity: Some(lease),
                effects_started: true,
                checkout_completed: true,
                base_oid: Some("a".repeat(40)),
                existing_branch_oid: None,
                pr_checkout: None,
                admin_dir: Some(admin.clone()),
                linear_issue: None,
                outcome: Some(WorktreeCreationOutcome::Created {
                    worktree: record,
                    warnings: Vec::new(),
                }),
                warnings: Vec::new(),
                detail: "created".into(),
            };
            let payload = serde_json::to_value(&journal).unwrap();
            let store = StateStore::open(
                path.join("state.sqlite"),
                crate::model::BootId::new("unlock-test"),
            )
            .unwrap();
            store
                .begin_operation(&PendingOperation {
                    id: creation,
                    entity_kind: "worktree_creation".into(),
                    entity_id: journal_identity(&common, &journal.request),
                    action: "create".into(),
                    base_generation: 7,
                    payload: payload.clone(),
                    status: PendingOperationStatus::Pending,
                    created_at_unix_ms: 1,
                    updated_at_unix_ms: 1,
                })
                .unwrap();
            store
                .update_operation(
                    creation,
                    &payload,
                    &payload,
                    PendingOperationStatus::Applied,
                )
                .unwrap();
            Self {
                _directory: directory,
                common: common_root,
                admin: admin_root,
                checkout: checkout_root,
                store,
                service: RepositoryService::new("git", Arc::new(Inventory { checkout, admin })),
                creation,
                run,
                lease,
            }
        }
        fn unlock(&self) -> Result<(), String> {
            self.service.unlock_stale_worktree_at(
                &self.store,
                &self.common,
                &self.admin,
                &self.checkout,
                &CancellationToken::new(),
            )
        }
        fn guard(&self) -> StaleLockGuard {
            self.service
                .stale_lock_guard(
                    &self.store,
                    &self.common,
                    &self.admin,
                    &self.checkout,
                    &CancellationToken::new(),
                )
                .unwrap()
        }
        fn phase(&self, phase: serde_json::Value) {
            let admin = self.admin.metadata().unwrap();
            fs::write(
                self.admin.path().join("cagents-setup-execution"),
                serde_json::to_vec(&serde_json::json!({
                    "creation":self.creation,"admin":[admin.dev(),admin.ino()],"phase":phase
                }))
                .unwrap(),
            )
            .unwrap();
        }
        fn retained(&self) -> Vec<u8> {
            fs::read(self.admin.path().join("locked")).unwrap()
        }
    }

    #[test]
    fn settled_never_started_unlock_removes_only_exact_git_marker() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::settled_never_started_unlock_removes_only_exact_git_marker"
        )) {
            return;
        }
        let f = Fixture::new();
        let row = f.store.journaled_worktree_operation(f.creation).unwrap();
        f.service
            .stale_worktree_lock_at(
                &f.store,
                &f.common,
                &f.admin,
                &f.checkout,
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(f.admin.path().join("locked").exists());
        f.unlock().unwrap();
        assert!(!f.admin.path().join("locked").exists());
        assert_eq!(
            f.store.journaled_worktree_operation(f.creation).unwrap(),
            row
        );
        assert_eq!(
            worktree_control::creation_marker(&f.admin).unwrap(),
            f.creation
        );
    }

    #[test]
    fn free_creation_lease_does_not_settle_reserved_setup() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::free_creation_lease_does_not_settle_reserved_setup"
        )) {
            return;
        }
        let f = Fixture::new();
        f.phase(serde_json::json!({"Reserved":{"run":f.run,"terminal":crate::model::TerminalId::new(),"lease":f.lease}}));
        let before = f.retained();
        assert!(f.unlock().is_err());
        assert_eq!(f.retained(), before);
    }

    #[test]
    fn original_settled_holder_binding_is_required() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::original_settled_holder_binding_is_required"
        )) {
            return;
        }
        for phase in ["FamilyReaped", "NotLaunched"] {
            for mismatch in ["none", "lease"] {
                let f = Fixture::new();
                let lease = if mismatch == "lease" {
                    (f.lease.0, f.lease.1 + 1)
                } else {
                    f.lease
                };
                f.phase(serde_json::json!({(phase):{"run":f.run,"terminal":crate::model::TerminalId::new(),"lease":lease}}));
                let before = f.retained();
                if mismatch == "none" {
                    f.unlock().unwrap();
                } else {
                    assert!(f.unlock().is_err());
                    assert_eq!(f.retained(), before);
                }
            }
        }
    }

    #[test]
    fn reviewed_retry_or_skip_keeps_original_positive_cleanup_valid() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::reviewed_retry_or_skip_keeps_original_positive_cleanup_valid"
        )) {
            return;
        }
        for skip in [false, true] {
            let f = Fixture::new();
            f.phase(serde_json::json!({"FamilyReaped":{"run":f.run,"terminal":crate::model::TerminalId::new(),"lease":f.lease}}));
            let row = f
                .store
                .journaled_worktree_operation(f.creation)
                .unwrap()
                .unwrap();
            let journal: WorktreeJournal = serde_json::from_value(row.payload).unwrap();
            let original = journal.request.setup_intent.unwrap();
            let mut reviewed = original.clone();
            reviewed.run_id = Some(OperationId::new());
            reviewed.skip = skip;
            // Exercise the real supported metadata transition: it does not
            // rewrite the original holder's settled disposition or run setup.
            f.service
                .update_worktree_setup_intent(&f.store, f.creation, Some(&original), reviewed)
                .unwrap();
            f.unlock().unwrap();
            assert!(!f.admin.path().join("locked").exists());
        }
    }

    #[test]
    fn missing_or_reused_generation_and_disposition_are_not_legacy() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::missing_or_reused_generation_and_disposition_are_not_legacy"
        )) {
            return;
        }
        for case in ["marker", "disposition", "new-generation", "malformed"] {
            let f = Fixture::new();
            match case {
                "marker" => fs::remove_file(f.admin.path().join("cagents-setup-creation")).unwrap(),
                "disposition" => {
                    fs::remove_file(f.admin.path().join("cagents-setup-execution")).unwrap()
                }
                "new-generation" => fs::write(
                    f.admin.path().join("cagents-setup-creation"),
                    format!("{}\n", OperationId::new()),
                )
                .unwrap(),
                _ => fs::write(f.admin.path().join("cagents-setup-execution"), b"{}").unwrap(),
            }
            let before = f.retained();
            assert!(f.unlock().is_err(), "{case}");
            assert_eq!(f.retained(), before);
        }
    }

    #[test]
    fn replaced_or_foreign_lock_leaf_is_preserved() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::replaced_or_foreign_lock_leaf_is_preserved"
        )) {
            return;
        }
        for case in ["replace", "edit", "symlink"] {
            let f = Fixture::new();
            let mut guard = f.guard();
            let lock = f.admin.path().join("locked");
            if case != "edit" {
                fs::remove_file(&lock).unwrap();
            }
            if case == "symlink" {
                let foreign = f.checkout.path().join("foreign-lock");
                fs::write(&foreign, b"foreign evidence").unwrap();
                symlink(&foreign, &lock).unwrap();
            } else {
                fs::write(&lock, b"another tool\n").unwrap();
            }
            assert!(guard.verify(&f.service, &CancellationToken::new()).is_err());
            assert_eq!(
                fs::read(&lock).unwrap(),
                if case == "symlink" {
                    b"foreign evidence".as_slice()
                } else {
                    b"another tool\n".as_slice()
                }
            );
        }
    }

    #[test]
    fn replaced_retained_directories_and_backlinks_refuse_unlock() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::replaced_retained_directories_and_backlinks_refuse_unlock"
        )) {
            return;
        }
        for case in ["common", "admin", "checkout", "backlink"] {
            let f = Fixture::new();
            let mut guard = f.guard();
            if case == "backlink" {
                fs::write(f.checkout.path().join(".git"), b"gitdir: /foreign/admin\n").unwrap();
            } else {
                let path = match case {
                    "common" => f.common.path(),
                    "admin" => f.admin.path(),
                    _ => f.checkout.path(),
                };
                let old = path.with_extension("retained");
                fs::rename(path, &old).unwrap();
                fs::create_dir(path).unwrap();
            }
            assert!(
                guard.verify(&f.service, &CancellationToken::new()).is_err(),
                "{case}"
            );
            assert_eq!(
                fs_safety::open_beneath(&guard.admin, Path::new("locked"))
                    .unwrap()
                    .metadata()
                    .unwrap()
                    .len(),
                guard.lock.bytes.len() as u64
            );
        }
    }

    #[test]
    fn canonical_journal_writer_rejects_a_superseded_guard() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::canonical_journal_writer_rejects_a_superseded_guard"
        )) {
            return;
        }
        let f = Fixture::new();
        let guard = f.guard();
        let mut updated = guard.operation.payload.clone();
        updated["detail"] = "superseded".into();
        f.store
            .update_operation(
                f.creation,
                &guard.operation.payload,
                &updated,
                PendingOperationStatus::Applied,
            )
            .unwrap();
        let mut called = false;
        assert!(
            f.store
                .with_worktree_operation_writer(&guard.operation, || {
                    called = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!called);
        assert_eq!(f.retained(), guard.lock.bytes);
    }

    #[test]
    fn live_or_replaced_lease_and_cancelled_request_leave_lock_intact() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::live_or_replaced_lease_and_cancelled_request_leave_lock_intact"
        )) {
            return;
        }
        let f = Fixture::new();
        let lease_path = f
            .common
            .path()
            .join(format!("cagents-operation-{}.lease", f.creation));
        let held = worktree_lease(&lease_path, false).unwrap();
        assert!(f.unlock().is_err());
        drop(held);
        let before = f.retained();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(
            f.service
                .unlock_stale_worktree_at(&f.store, &f.common, &f.admin, &f.checkout, &cancellation)
                .is_err()
        );
        let mut guard = f.guard();
        fs::remove_file(&lease_path).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&lease_path)
            .unwrap();
        assert!(guard.verify(&f.service, &CancellationToken::new()).is_err());
        assert_eq!(f.retained(), before);
    }
    #[test]
    fn unapplied_or_unknown_creation_cannot_unlock() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::unapplied_or_unknown_creation_cannot_unlock"
        )) {
            return;
        }
        for applied in [false, true] {
            let f = Fixture::new();
            let row = f
                .store
                .journaled_worktree_operation(f.creation)
                .unwrap()
                .unwrap();
            if applied {
                let mut unknown = row.payload.clone();
                unknown["version"] = 99.into();
                f.store
                    .update_operation(
                        f.creation,
                        &row.payload,
                        &unknown,
                        PendingOperationStatus::Applied,
                    )
                    .unwrap();
            } else {
                // Keep the canonical settled payload, but replace its row with
                // a never-applied operation. Its familiar label is no proof.
                f.store.release_worktree_creation(f.creation).unwrap();
                let mut pending = row;
                pending.status = PendingOperationStatus::Pending;
                f.store.begin_operation(&pending).unwrap();
            }
            let before = f.retained();
            assert!(f.unlock().is_err());
            assert_eq!(f.retained(), before);
        }
    }

    #[test]
    fn sync_failure_reports_removed_marker_as_uncertain_not_refusal() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::sync_failure_reports_removed_marker_as_uncertain_not_refusal"
        )) {
            return;
        }
        let f = Fixture::new();
        FAIL_UNLOCK_SYNC.with(|fail| fail.set(true));
        let error = f.unlock().unwrap_err();
        assert!(error.contains("removed the original Git lock"), "{error}");
        assert!(error.contains("do not retry automatically"), "{error}");
        assert!(!f.admin.path().join("locked").exists());
        assert_eq!(
            worktree_control::creation_marker(&f.admin).unwrap(),
            f.creation
        );
    }

    #[test]
    fn real_git_settled_creation_unlock_preserves_checkout_and_receipts() {
        if super::super::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::real_git_settled_creation_unlock_preserves_checkout_and_receipts"
        )) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = root.join("repository");
        fs::create_dir(&repository).unwrap();
        let cancellation = CancellationToken::new();
        let git = |args: &[&str]| {
            let command = CommandSpec::new("git")
                .arg("-C")
                .arg(repository.as_os_str())
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args.iter().copied())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .timeout(Duration::from_secs(10));
            SystemProcessExecutor
                .execute(&command, &cancellation)
                .unwrap()
                .require_success(&command.program)
                .unwrap()
        };
        git(&["init", "--initial-branch=main"]);
        fs::write(repository.join("tracked.txt"), b"owned fixture\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-m", "base"]);
        let service = RepositoryService::system("git");
        let store = StateStore::open(
            root.join("state.sqlite"),
            crate::model::BootId::new("real-unlock"),
        )
        .unwrap();
        let managed = root.join("managed");
        let request = CreateWorktreeRequest {
            repository: repository.clone(),
            managed_root: managed.clone(),
            managed_ignore_root: None,
            destination: managed.join("topic"),
            branch: ValidatedBranchName::parse("topic").unwrap(),
            base: BaseRevision::LocalBranch(ValidatedBranchName::parse("main").unwrap()),
            track: false,
            operation_id: OperationId::new(),
            setup_intent: None,
        };
        let report = service.create_worktree_journaled_sync(
            &store,
            &request,
            None,
            7,
            &cancellation,
            |_| {},
        );
        assert_eq!(report.state, WorktreeOperationState::Created, "{report:?}");
        let reason = format!("cagents:{}", request.operation_id);
        git(&[
            "worktree",
            "lock",
            "--reason",
            &reason,
            request.destination.to_str().unwrap(),
        ]);
        let common =
            fs_safety::Root::open(&service.common_dir(&repository, &cancellation).unwrap())
                .unwrap();
        let admin = fs_safety::Root::open(
            &service
                .git_dir(&request.destination, &cancellation)
                .unwrap(),
        )
        .unwrap();
        let checkout = fs_safety::Root::open(&request.destination).unwrap();
        let mut before = service
            .list_worktrees(&repository, &cancellation)
            .unwrap()
            .into_iter()
            .find(|record| record.path == request.destination)
            .unwrap();
        assert!(before.lock.locked);
        let journal = store
            .journaled_worktree_operation(request.operation_id)
            .unwrap();
        let disposition = fs::read(admin.path().join("cagents-setup-execution")).unwrap();
        service
            .stale_worktree_lock_at(&store, &common, &admin, &checkout, &cancellation)
            .unwrap();
        service
            .unlock_stale_worktree_at(&store, &common, &admin, &checkout, &cancellation)
            .unwrap();
        let after = service
            .list_worktrees(&repository, &cancellation)
            .unwrap()
            .into_iter()
            .find(|record| record.path == request.destination)
            .unwrap();
        assert!(!after.lock.locked);
        before.lock = WorktreeLock::default();
        assert_eq!(after, before);
        assert_eq!(
            fs::read(checkout.path().join("tracked.txt")).unwrap(),
            b"owned fixture\n"
        );
        assert_eq!(
            store
                .journaled_worktree_operation(request.operation_id)
                .unwrap(),
            journal
        );
        assert_eq!(
            fs::read(admin.path().join("cagents-setup-execution")).unwrap(),
            disposition
        );
        assert_eq!(
            worktree_control::creation_marker(&admin).unwrap(),
            request.operation_id
        );
        check_roots(&common, &admin, &checkout).unwrap();
    }
}
