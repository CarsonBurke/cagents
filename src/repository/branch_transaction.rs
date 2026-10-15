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
        let old = journal.existing_branch_oid.as_deref()
            .ok_or_else(|| invalid("No existing branch consent was recorded"))?;
        let new = journal.base_oid.as_deref()
            .ok_or_else(|| invalid("No resolved checkout target was recorded"))?;
        if !is_hex_oid(old) || !is_hex_oid(new) { return Err(invalid("Invalid branch transaction identity")); }
        let repository = &journal.request.repository;
        let branch = &journal.request.branch;
        let full_ref = branch.full_ref();
        let census = || self.list_worktrees(repository, cancellation);
        if census()?.iter().any(|record| record.branch.as_deref() == Some(full_ref.as_str())) {
            return Err(invalid("The existing branch became checked out elsewhere; it was retained"));
        }
        // Reuse the exact actual-admin association and policy proof used by
        // landed branch deletion. The service already pins source command scope;
        // the registry never substitutes or unwraps that executor.
        let mut registry = registered_heads::RegisteredHeads::capture(self, repository, cancellation)
            .map_err(invalid)?;
        let (provisional_head, provisional_lock) = created.provisional_metadata()?;
        registry.acquire().map_err(invalid)?;
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
        registry.write_proof(&directory).map_err(invalid)?;
        let hook = directory.join("reference-transaction");
        let proof = directory.join("prepared");
        let write = |path: &Path, bytes: &[u8], mode| -> Result<(), RepositoryError> {
            let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(mode)
                .open(path).map_err(|error| invalid(error.to_string()))?;
            file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|error| invalid(error.to_string()))
        };
        write(&hook, PROBE, 0o700)?;
        write(&directory.join("config-expected"), &config.stdout, 0o600)?;
        write(&directory.join("created-HEAD"), &provisional_head, 0o600)?;
        write(&directory.join("created-locked"), &provisional_lock, 0o600)?;
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
                .env("CAGENTS_BRANCH_CREATED_ADMIN", &paths[5])
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
        registry.check().map_err(invalid)?;
        created.provisional_metadata()?;
        let command = environment(self.git_mutation(repository, [OsString::from("-c"), hooks,
            "branch".into(), "--force".into(), "--".into(), branch.as_str().into(), new.into()]))?;
        self.run_checked(&command, cancellation)?;
        if fs::read(&proof).map_err(|error| invalid(error.to_string()))? != b"prepared" {
            return Err(invalid("Git returned without the expected branch transaction proof; inspect the recorded operation"));
        }
        // The caller must still attach the child and independently reconcile
        // exact final branch/OID/registration after inherited lease retirement.
        session.validate()?;
        registry.check().map_err(invalid)?;
        created.provisional_metadata()?;
        Ok(())
    }
}
