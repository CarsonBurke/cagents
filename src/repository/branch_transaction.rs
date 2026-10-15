//! Expected-old/new branch movement through native Git's checked-out guard.
//!
//! The prepared hook binds consent while Git owns the reference lock. This is
//! not a family-exit certificate: inherited lease retirement has narrower scope.
use super::*;
use std::{io::Write, os::{fd::RawFd, unix::fs::{MetadataExt, OpenOptionsExt}}};

const PROBE: &[u8] = b"#!/bin/sh\nset -eu\n/bin/cat >/dev/null\nif [ \"$1\" = prepared ]; then printf prepared > \"$CAGENTS_BRANCH_PROOF\"; fi\n";

fn invalid(message: impl Into<String>) -> RepositoryError {
    ValidationError::Io(message.into()).into()
}

fn head_lock(root: &fs_safety::Root) -> Result<GitFileLock, RepositoryError> {
    let (file, temporary) = fs_safety::create_temporary_in(root, 0o600)
        .map_err(|error| invalid(error.to_string()))?;
    let name = OsString::from("HEAD.lock");
    if let Err(error) = fs_safety::rename_new_within(root, &temporary, &name) {
        let _ = fs_safety::unlink_within(root, &temporary);
        return Err(invalid(format!("A registered checkout is busy: {error}")));
    }
    let metadata = file.metadata().map_err(|error| invalid(error.to_string()))?;
    Ok(GitFileLock { parent: root.try_clone().map_err(|error| invalid(error.to_string()))?,
        name, version: GitFileLock::version(&metadata), file, active: true })
}

impl<E: ProcessExecutor> RepositoryService<E> {
    /// No caller gets a general unscoped executor. The source is the journal's
    /// already-held original repository and every command still passes its
    /// scoped executor, cancellation and output limits.
    pub(super) fn advance_unoccupied_creation_branch(&self, journal: &WorktreeJournal,
        cancellation: &CancellationToken, lease: RawFd) -> Result<(), RepositoryError>
    {
        let session = journal.live_creation.as_ref()
            .ok_or_else(|| invalid("Branch advancement needs the journal's live source authority"))?;
        session.validate()?;
        let authority = session.source_authority()?;
        let (source, private, common) = authority.retained_roots()?;
        let created = journal.live_created.as_ref()
            .ok_or_else(|| invalid("Branch advancement needs the verified provisional checkout"))?;
        let mut held = vec![source, private, common];
        held.extend(created.transaction_roots()?);
        let common = held[2].try_clone().map_err(|error| invalid(error.to_string()))?;
        let old = journal.existing_branch_oid.as_deref()
            .ok_or_else(|| invalid("No existing branch consent was recorded"))?;
        let new = journal.base_oid.as_deref()
            .ok_or_else(|| invalid("No resolved checkout target was recorded"))?;
        if !is_hex_oid(old) || !is_hex_oid(new) { return Err(invalid("Invalid branch transaction identity")); }
        let repository = &journal.request.repository;
        let branch = &journal.request.branch;
        let full_ref = branch.full_ref();
        // Files HEAD.lock guards do not constrain a reftable HEAD backend.
        let storage = self.run(&self.git_read(repository,
            ["config", "--local", "--get", "extensions.refStorage"]), cancellation)?;
        if !(storage.exit_code == Some(1) || (storage.success
            && trim_one_line_ending(&storage.stdout) == b"files"))
        { return Err(invalid("This branch transaction requires Git's files reference backend")); }
        let census = || self.list_worktrees(repository, cancellation);
        let records = census()?;
        if records.iter().any(|record| record.branch.as_deref() == Some(full_ref.as_str())) {
            return Err(invalid("The existing branch became checked out elsewhere; it was retained"));
        }
        // Native Git resolves registered HEADs from its common metadata, not
        // from mutable <checkout>/.git aliases. Lock those exact admin roots.
        let mut roots = vec![common.try_clone().map_err(|error| invalid(error.to_string()))?];
        let mut linked_paths = HashSet::new();
        let registry = match fs_safety::open_directory_beneath(&common, Path::new("worktrees")) {
            Ok(root) => Some(root),
            Err(error) if error.is_not_found() => None,
            Err(error) => return Err(invalid(error.to_string())),
        };
        if let Some(registry) = registry {
            for (index, entry) in registry.read_dir().map_err(|error| invalid(error.to_string()))?.enumerate() {
                if index >= 4096 { return Err(invalid("The registered checkout inventory exceeds its safe bound")); }
                let entry = entry.map_err(|error| invalid(error.to_string()))?;
                let admin = fs_safety::open_directory_beneath(&registry, Path::new(&entry.file_name()))
                    .map_err(|error| invalid(error.to_string()))?;
                let backlink = creation_authority::regular_locator(&admin, "gitdir")?;
                let marker = bytes_to_path(trim_one_line_ending(&backlink));
                let record = records.iter().find(|record| !record.bare && record.path.join(".git") == marker)
                    .ok_or_else(|| invalid("A registered admin has no matching native checkout record; repair or prune it first"))?;
                if !linked_paths.insert(record.path.clone()) {
                    return Err(invalid("Multiple registered admins claim the same checkout path"));
                }
                if !fs_safety::metadata_beneath(&admin, Path::new("HEAD"))
                    .map_err(|error| invalid(error.to_string()))?.is_file() {
                    return Err(invalid("A registered checkout has no regular files-backend HEAD"));
                }
                roots.push(admin);
            }
        }
        let primary = records.iter().filter(|record| !record.bare && !linked_paths.contains(&record.path))
            .collect::<Vec<_>>();
        if primary.len() > 1 { return Err(invalid("The primary checkout registration is ambiguous")); }
        for record in primary {
            let captured = WorktreeAuthority::capture(&record.path)?;
            let (_, admin, _) = captured.retained_roots()?;
            let actual = admin.metadata().map_err(|error| invalid(error.to_string()))?;
            let expected = common.metadata().map_err(|error| invalid(error.to_string()))?;
            if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
                return Err(invalid("The primary checkout locator no longer names its registered admin"));
            }
        }
        held.extend(roots.iter().map(|root| root.try_clone().map_err(|error| invalid(error.to_string())))
            .collect::<Result<Vec<_>, _>>()?);
        let locks = roots.iter().map(head_lock).collect::<Result<Vec<_>, _>>()?;
        for lock in &locks { lock.check().map_err(invalid)?; }
        if census()?.iter().any(|record| record.branch.as_deref() == Some(full_ref.as_str())) {
            return Err(invalid("The branch became checked out while acquiring checkout guards"));
        }
        self.run_checked(&self.git_read(repository,
            ["merge-base", "--is-ancestor", old, new]), cancellation)?;
        let previous = self.run_checked(&self.git_read(repository,
            ["rev-parse", "--path-format=absolute", "--git-path", "hooks/reference-transaction"]), cancellation)?;
        let previous = parse_path_line(&previous.stdout, "reference_hook")?;
        let config = self.run_checked(&self.git_read(repository,
            ["config", "--null", "--list", "--show-scope"]), cancellation)?;
        // Retain small private diagnostics even on uncertainty. Never remove a
        // hook's files merely because its direct parent returned or timed out.
        let directory = tempfile::Builder::new().prefix("cagents-branch-advance-")
            .tempdir_in(session.common_operand()?).map_err(|error| invalid(error.to_string()))?;
        let directory = directory.keep();
        let hook = directory.join("reference-transaction");
        let proof = directory.join("prepared");
        let write = |path: &Path, bytes: &[u8], mode| -> Result<(), RepositoryError> {
            let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(mode)
                .open(path).map_err(|error| invalid(error.to_string()))?;
            file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|error| invalid(error.to_string()))
        };
        write(&hook, PROBE, 0o700)?;
        write(&directory.join("config-expected"), &config.stdout, 0o600)?;
        let mut files = Vec::new();
        let mut paths = Vec::new();
        for root in &held {
            let (path, file) = authority::inherited_directory_scope(root)?;
            files.push(file); paths.push(path);
        }
        let mut identities = Vec::new();
        let quote = |bytes: &[u8]| -> Vec<u8> {
            let mut result = vec![b'\''];
            for byte in bytes {
                if *byte == b'\'' { result.extend_from_slice(b"'\\''"); }
                else { result.push(*byte); }
            }
            result.push(b'\''); result
        };
        for (root, path) in held.iter().zip(&paths) {
            let metadata = root.metadata().map_err(|error| invalid(error.to_string()))?;
            identities.extend_from_slice(b"[ ! -L ");
            identities.extend(quote(root.path().as_os_str().as_encoded_bytes()));
            identities.extend_from_slice(b" ]\n");
            for path in [root.path(), path.as_path()] {
                identities.extend_from_slice(b"[ \"$(/usr/bin/stat -Lc '%d:%i' -- ");
                identities.extend(quote(path.as_os_str().as_encoded_bytes()));
                identities.extend_from_slice(b")\" = ");
                identities.extend(quote(format!("{}:{}", metadata.dev(), metadata.ino()).as_bytes()));
                identities.extend_from_slice(b" ]\n");
            }
        }
        write(&directory.join("check-identities"), &identities, 0o600)?;
        let child_directory = paths[2].join(directory.file_name()
            .ok_or_else(|| invalid("The private branch operation has no directory name"))?);
        let child_proof = child_directory.join("prepared");
        let mut hooks = OsString::from("core.hooksPath="); hooks.push(&child_directory);
        let original = std::env::var_os("GIT_CONFIG_PARAMETERS").unwrap_or_default();
        let mut policy_config = original.clone();
        if !policy_config.is_empty() { policy_config.push(" "); }
        policy_config.push("'core.fsmonitor=false'");
        let environment = |mut command: CommandSpec| -> Result<CommandSpec, RepositoryError> {
            command = command.env("CAGENTS_BRANCH_PROOF", &child_proof)
                .env("CAGENTS_BRANCH_PRIVATE", &child_directory)
                .env("CAGENTS_BRANCH_REF", &full_ref).env("CAGENTS_BRANCH_OLD", old)
                .env("CAGENTS_BRANCH_NEW", new).env("CAGENTS_BRANCH_GIT", &self.git)
                .env("CAGENTS_BRANCH_PREVIOUS_HOOK", &previous)
                .env("CAGENTS_BRANCH_ORIGINAL_CONFIG", &original)
                .env("CAGENTS_BRANCH_PREVIOUS_CONFIG", &policy_config);
            for (index, path) in paths.iter().take(3).enumerate() {
                command = command.env(format!("CAGENTS_BRANCH_FD_{index}"), path);
            }
            for file in &files { command = command.inherit_file(file.clone()).map_err(|error| invalid(error.to_string()))?; }
            command.inherited_lease = Some(lease);
            Ok(command)
        };
        let probe = environment(self.git_mutation(repository, [OsString::from("-c"), hooks.clone(),
            "update-ref".into(), "--stdin".into()]).stdin(
                format!("start\nverify {full_ref} {old}\nprepare\nabort\n").into_bytes()))?;
        self.run_checked(&probe, cancellation)?;
        if fs::read(&proof).map_err(|error| invalid(error.to_string()))? != b"prepared" {
            return Err(invalid("Git did not prove a prepared reference transaction; branch retained"));
        }
        fs::remove_file(&proof).map_err(|error| invalid(error.to_string()))?;
        let mut file = fs::OpenOptions::new().write(true).truncate(true).open(&hook)
            .map_err(|error| invalid(error.to_string()))?;
        file.write_all(include_bytes!("branch_transaction.sh")).and_then(|()| file.sync_all())
            .map_err(|error| invalid(error.to_string()))?;
        drop(file);
        session.validate()?;
        for lock in &locks { lock.check().map_err(invalid)?; }
        let command = environment(self.git_mutation(repository, [OsString::from("-c"), hooks,
            "branch".into(), "--force".into(), "--".into(), branch.as_str().into(), new.into()]))?;
        self.run_checked(&command, cancellation)?;
        if fs::read(&proof).map_err(|error| invalid(error.to_string()))? != b"prepared" {
            return Err(invalid("Git returned without the expected branch transaction proof; inspect the recorded operation"));
        }
        // The caller must still attach the child and independently reconcile
        // exact final branch/OID/registration after inherited lease retirement.
        session.validate()?;
        Ok(())
    }
}
