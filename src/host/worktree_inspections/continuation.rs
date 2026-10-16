//! Transfer the existing inspection; never capture a replacement on submission.
use super::*;
use crate::{
    repository::{
        CommandSpec, ProcessExecutor, ProcessFailure, ProcessFailureKind, ProcessOutput,
        SystemProcessExecutor,
    },
    worktree_control::{BranchDeleteFailure, BranchDeleteFailureKind, Operation},
};
use std::{ffi::OsString, os::fd::AsRawFd};

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Phase {
    Inspected,
    Stopped,
    Removed,
}

fn connected(inspection: &Inspection, host: &SharedHost) -> bool {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    state
        .clients
        .get(&inspection.client)
        .is_some_and(|current| {
            inspection
                .connection
                .upgrade()
                .is_some_and(|original| Arc::ptr_eq(current, &original))
        })
}
fn retain(
    host: &SharedHost,
    inspection: Arc<Inspection>,
    phase: Phase,
    token: Option<uuid::Uuid>,
) -> Result<uuid::Uuid, String> {
    if Instant::now() >= inspection.expires || !connected(&inspection, host) {
        return Err(REFRESH.into());
    }
    inspection
        .phase
        .store(phase as u8, std::sync::atomic::Ordering::Release);
    let token = token.unwrap_or_else(uuid::Uuid::new_v4);
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    state
        .stop_inspections
        .retain(|_, entry| now < entry.expires && entry.connection.upgrade().is_some());
    if state.stop_inspections.len() >= TOTAL
        || state
            .stop_inspections
            .values()
            .filter(|entry| entry.client == inspection.client)
            .count()
            >= PER_CLIENT
    {
        return Err(REFRESH.into());
    }
    // Recheck exact connection in the same state lock used to publish the phase.
    if !state
        .clients
        .get(&inspection.client)
        .is_some_and(|current| {
            inspection
                .connection
                .upgrade()
                .is_some_and(|original| Arc::ptr_eq(current, &original))
        })
    {
        return Err(REFRESH.into());
    }
    state.stop_inspections.insert(token, inspection);
    Ok(token)
}
pub(in crate::host) fn stopped(
    host: &SharedHost,
    inspection: Arc<Inspection>,
) -> Result<uuid::Uuid, String> {
    retain(host, inspection, Phase::Stopped, None)
}
fn take_phase(
    host: &SharedHost,
    client: ClientId,
    token: uuid::Uuid,
    repository: &Path,
    phase: Phase,
) -> Result<Arc<Inspection>, String> {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let entry = state.stop_inspections.get(&token).ok_or(REFRESH)?;
    let current = state.clients.get(&client).ok_or(REFRESH)?;
    if entry.client != client
        || entry.repository_request != repository
        || entry.phase.load(std::sync::atomic::Ordering::Acquire) != phase as u8
        || Instant::now() >= entry.expires
        || !entry
            .connection
            .upgrade()
            .is_some_and(|original| Arc::ptr_eq(current, &original))
    {
        return Err(REFRESH.into());
    }
    state
        .stop_inspections
        .remove(&token)
        .ok_or_else(|| REFRESH.into())
}
impl Inspection {
    fn validate_branch(&self) -> Result<(), String> {
        if Instant::now() >= self.expires || self.connection.upgrade().is_none() {
            return Err(REFRESH.into());
        }
        self.physical.common.validate()?;
        // Git removed the checkout and its private admin directory. They remain
        // held, but their old names are no longer requirements for branch work.
        if self.physical.repository.path != self.physical.checkout.path {
            self.physical.repository.validate()?;
            if let Some(admin) = &self.physical.repository_admin {
                admin.validate()?;
            }
            if self
                .repository_request
                .canonicalize()
                .map_err(|_| REFRESH)?
                != self.physical.repository.path
            {
                return Err(REFRESH.into());
            }
            let common = RepositoryService::system("git")
                .common_dir(&self.physical.repository.path, &CancellationToken::new())
                .map_err(|error| error.to_string())?;
            let admin = RepositoryService::system("git")
                .git_dir(&self.physical.repository.path, &CancellationToken::new())
                .map_err(|error| error.to_string())?;
            if common != self.physical.common.path || admin != self.physical.repository_admin_path()
            {
                return Err(REFRESH.into());
            }
        } else {
            // The removed source's private HEAD is gone. Its original common
            // ref authority survives. Refuse a new checkout appearing under
            // that name, while permitting the intentionally vanished names.
            match std::fs::metadata(&self.repository_request) {
                Ok(named) => {
                    let held = self
                        .physical
                        .repository
                        .root
                        .metadata()
                        .map_err(|error| error.to_string())?;
                    if (named.dev(), named.ino()) != (held.dev(), held.ino()) {
                        return Err(REFRESH.into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(REFRESH.into()),
            }
        }
        if self.physical.repository.path != self.physical.checkout.path {
            self.physical.repository.validate()?;
            if let Some(admin) = &self.physical.repository_admin {
                admin.validate()?;
            }
            if self
                .repository_request
                .canonicalize()
                .map_err(|_| REFRESH)?
                != self.physical.repository.path
            {
                return Err(REFRESH.into());
            }
        }
        self.physical.common.validate()?;
        if Instant::now() >= self.expires || self.connection.upgrade().is_none() {
            return Err(REFRESH.into());
        }
        Ok(())
    }
}

/// The original roots are duplicated only for child inheritance. No path is
/// reopened to supply command authority. All Git commands, including the
/// prepared ref-transaction hook, share these exact owned descriptor numbers.
/// This binds inspection through dispatch and coordinated product admission.
/// Native Git may subsequently canonicalize a worktree operand and delete by
/// its registered path. External namespace replacement inside that native
/// interval is not confined by these descriptors or the product's locks.
#[derive(Clone)]
struct ScopedGit {
    inspection: Arc<Inspection>,
    host: SharedHost,
    branch_only: bool,
    common_only: bool,
    repository: Arc<std::fs::File>,
    repository_admin: Arc<std::fs::File>,
    checkout: Arc<std::fs::File>,
    admin: Arc<std::fs::File>,
    common: Arc<std::fs::File>,
}
impl ScopedGit {
    fn new(
        host: &SharedHost,
        inspection: &Arc<Inspection>,
        branch_only: bool,
    ) -> Result<Self, String> {
        if !cfg!(target_os = "linux") {
            return Err(
                "Inspected worktree deletion requires Linux directory-handle support".into(),
            );
        }
        let copy = |pin: &Pin| {
            pin.root
                .retained_directory()
                .map(Arc::new)
                .map_err(|error| error.to_string())
        };
        let common_only =
            branch_only && inspection.physical.repository.path == inspection.physical.checkout.path;
        let repository = if common_only {
            &inspection.physical.common
        } else {
            &inspection.physical.repository
        };
        let repository_admin = if common_only {
            &inspection.physical.common
        } else {
            inspection
                .physical
                .repository_admin
                .as_ref()
                .unwrap_or_else(|| {
                    if inspection.physical.repository.path == inspection.physical.checkout.path {
                        &inspection.physical.admin
                    } else {
                        &inspection.physical.common
                    }
                })
        };
        Ok(Self {
            inspection: Arc::clone(inspection),
            host: Arc::clone(host),
            branch_only,
            common_only,
            repository: copy(repository)?,
            repository_admin: copy(repository_admin)?,
            checkout: copy(&inspection.physical.checkout)?,
            admin: copy(&inspection.physical.admin)?,
            common: copy(&inspection.physical.common)?,
        })
    }
    fn child(file: &std::fs::File) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}/.", file.as_raw_fd()))
    }
}
impl ProcessExecutor for ScopedGit {
    fn execute(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessFailure> {
        let refuse = |message: String| ProcessFailure {
            kind: ProcessFailureKind::Io,
            program: command.program.clone(),
            exit_code: None,
            signal: None,
            stdout: vec![],
            stderr: vec![],
            message,
            started: false,
        };
        if self.branch_only {
            self.inspection.validate_branch()
        } else {
            self.inspection
                .validate_requested_names()
                .and_then(|()| self.inspection.physical.validate())
                .and_then(|()| {
                    if Instant::now() >= self.inspection.expires
                        || self.inspection.connection.upgrade().is_none()
                    {
                        Err(REFRESH.into())
                    } else {
                        Ok(())
                    }
                })
        }
        .map_err(refuse)?;
        if !connected(&self.inspection, &self.host) {
            return Err(refuse(REFRESH.into()));
        }
        let mut scoped = command.clone();
        let mut provided_git = None;
        let mut provided_worktree = None;
        let mut checkout = false;
        let mut anchored = false;
        let mut index = 0;
        while index < scoped.arguments.len() {
            let argument = scoped.arguments[index].to_string_lossy().into_owned();
            if argument == "--" || !argument.starts_with('-') {
                break;
            }
            match argument.as_ref() {
                "-C" => {
                    let path = scoped
                        .arguments
                        .get_mut(index + 1)
                        .ok_or_else(|| refuse(REFRESH.into()))?;
                    let original = Path::new(path);
                    checkout =
                        !self.branch_only && original == self.inspection.physical.checkout.path;
                    if !checkout && original != self.inspection.physical.repository.path {
                        return Err(refuse(REFRESH.into()));
                    }
                    *path = Self::child(if checkout {
                        &self.checkout
                    } else {
                        &self.repository
                    })
                    .into_os_string();
                    anchored = true;
                    index += 2;
                }
                "-c" | "--config-env" => index += 2,
                value if value.starts_with("--git-dir=") => {
                    provided_git = Some(PathBuf::from(value.strip_prefix("--git-dir=").unwrap()));
                    scoped.arguments.remove(index);
                }
                value if value.starts_with("--work-tree=") => {
                    provided_worktree =
                        Some(PathBuf::from(value.strip_prefix("--work-tree=").unwrap()));
                    scoped.arguments.remove(index);
                }
                "--git-dir" | "--work-tree" => return Err(refuse(REFRESH.into())),
                _ => index += 1,
            }
        }
        if !anchored || scoped.current_dir.is_some() {
            return Err(refuse(
                "Git command did not retain the inspected scope".into(),
            ));
        }
        // RegisteredHeads may add its held source scopes. They are fencing
        // evidence, not replacement authority: require the original roles,
        // then issue the command using our original inherited handles.
        let same = |path: &Path, file: &std::fs::File| -> Result<bool, ProcessFailure> {
            let named = std::fs::metadata(path).map_err(|error| refuse(error.to_string()))?;
            let held = file.metadata().map_err(|error| refuse(error.to_string()))?;
            Ok((named.dev(), named.ino()) == (held.dev(), held.ino()))
        };
        let supplied_git_matches = match provided_git.as_deref() {
            None => true,
            Some(path) => same(
                path,
                if checkout {
                    &self.admin
                } else {
                    &self.repository_admin
                },
            )?,
        };
        let supplied_worktree_matches = self.common_only
            || match provided_worktree.as_deref() {
                None => true,
                Some(path) => same(
                    path,
                    if checkout {
                        &self.checkout
                    } else {
                        &self.repository
                    },
                )?,
            };
        if !supplied_git_matches || !supplied_worktree_matches {
            return Err(refuse(REFRESH.into()));
        }
        let git = Self::child(if checkout {
            &self.admin
        } else {
            &self.repository_admin
        });
        scoped
            .arguments
            .insert(0, OsString::from(format!("--git-dir={}", git.display())));
        if !self.common_only {
            scoped.arguments.insert(
                1,
                OsString::from(format!(
                    "--work-tree={}",
                    Self::child(if checkout {
                        &self.checkout
                    } else {
                        &self.repository
                    })
                    .display()
                )),
            );
        }
        scoped.environment.insert(
            "GIT_COMMON_DIR".into(),
            Self::child(&self.common).into_os_string(),
        );
        // The removal operand itself must also follow the retained checkout.
        if scoped
            .arguments
            .windows(2)
            .any(|args| args[0] == "worktree" && args[1] == "remove")
        {
            let operand = scoped
                .arguments
                .last_mut()
                .ok_or_else(|| refuse(REFRESH.into()))?;
            if Path::new(operand) != self.inspection.physical.checkout.path {
                return Err(refuse(REFRESH.into()));
            }
            *operand = Self::child(&self.checkout).into_os_string();
        }
        for file in [
            &self.repository,
            &self.repository_admin,
            &self.checkout,
            &self.admin,
            &self.common,
        ] {
            scoped = scoped
                .inherit_file(file.clone())
                .map_err(|error| refuse(error.to_string()))?;
        }
        let remaining = self
            .inspection
            .expires
            .checked_duration_since(Instant::now())
            .ok_or_else(|| refuse(REFRESH.into()))?;
        scoped.timeout = Some(scoped.timeout.unwrap_or(remaining).min(remaining));
        SystemProcessExecutor.execute(&scoped, cancellation)
    }
}

// Caller holds exclusive control admission throughout the fresh census and
// mutation. No HostLock spans any filesystem/Git work.
pub(in crate::host) fn remove(
    host: &SharedHost,
    client: ClientId,
    mut operation: Operation,
) -> Result<Reply, String> {
    let Operation::Remove {
        ref repository,
        ref target,
        delete_branch,
        ref expected_fingerprint,
        ref resolved_path,
        continuation,
        ..
    } = operation
    else {
        return Err(REFRESH.into());
    };
    let token = continuation.ok_or(REFRESH)?;
    let inspection = take_phase(host, client, token, repository, Phase::Stopped)?;
    if delete_branch
        || expected_fingerprint.as_deref() != Some(&inspection.fingerprint)
        || (resolved_path
            .as_deref()
            .unwrap_or_else(|| Path::new(target))
            != inspection.path_request
            && resolved_path
                .as_deref()
                .unwrap_or_else(|| Path::new(target))
                != inspection.physical.checkout.path)
    {
        return Err(REFRESH.into());
    }
    inspection.validate()?;
    let mut scope = Some(
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .worktree_removal_scope()
            .map_err(|error| error.to_string())?,
    );
    // Resolve request aliases only against the original inspection. The
    // service receives captured operands, never a newly selected target.
    if let Operation::Remove {
        repository,
        target,
        resolved_path,
        ..
    } = &mut operation
    {
        *repository = inspection.physical.repository.path.clone();
        *target = inspection
            .physical
            .checkout
            .path
            .to_str()
            .ok_or("Worktree path is not representable by this protocol")?
            .to_owned();
        *resolved_path = Some(inspection.physical.checkout.path.clone());
    }
    let executor = ScopedGit::new(host, &inspection, false)?;
    let before_effect = |path: &Path| {
        if path != inspection.physical.checkout.path {
            return Err(REFRESH.into());
        }
        inspection.validate()?;
        if !connected(&inspection, host) {
            return Err(REFRESH.into());
        }
        if !super::super::worktree_sessions::busy_is_empty(host, path)? {
            return Err("New sessions appeared after stopping; review their work before removing this worktree".into());
        }
        Ok(())
    };
    let mut execution = crate::worktree_control::service::execute_inspected_remove(
        operation,
        executor,
        |path| {
            if let Some(scope) = scope.take() {
                scope.check(path).map_err(|error| error.to_string())?;
            }
            Ok(host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .worktree_live_sessions(path))
        },
        before_effect,
        || {
            inspection
                .phase
                .store(Phase::Removed as u8, std::sync::atomic::Ordering::Release)
        },
    )?;
    #[cfg(test)]
    let inspection = {
        let mut inspection = inspection;
        after_remove_for_test(
            Arc::get_mut(&mut inspection)
                .expect("completed executor released its inspection aliases"),
        );
        inspection
    };
    if let Reply::Removed {
        continuation,
        warnings,
        ..
    } = &mut execution.reply
    {
        if inspection.branch.is_some() {
            match retain(host, inspection, Phase::Removed, None) {
                Ok(token) => *continuation = Some(token),
                Err(error) => warnings.push(format!("Worktree was removed; branch was retained because its original inspection could not continue: {error}")),
            }
        }
    }
    Ok(execution.reply)
}
pub(in crate::host) fn delete_branch(
    host: &SharedHost,
    client: ClientId,
    operation: Operation,
) -> Reply {
    let failure = |detail: String| Reply::BranchDeleteRefused {
        failure: BranchDeleteFailure {
            kind: BranchDeleteFailureKind::AuthorityChanged,
            detail,
        },
        continuation: None,
    };
    let Operation::DeleteBranch {
        repository,
        branch,
        force,
        expected_oid,
        continuation,
    } = operation
    else {
        return failure(REFRESH.into());
    };
    let Some(token) = continuation else {
        return failure(REFRESH.into());
    };
    let inspection = match take_phase(host, client, token, &repository, Phase::Removed) {
        Ok(entry) => entry,
        Err(error) => return failure(error),
    };
    if inspection
        .branch
        .as_ref()
        .map(|(name, oid)| (name.as_str(), oid.as_str()))
        != expected_oid.as_deref().map(|oid| (branch.as_str(), oid))
    {
        return failure(REFRESH.into());
    }
    if let Err(error) = inspection.validate_branch() {
        return failure(error);
    }
    let executor = match ScopedGit::new(host, &inspection, true) {
        Ok(executor) => executor,
        Err(error) => return failure(error),
    };
    let result = crate::worktree_branch::delete_branch_checked_with(
        &inspection.physical.repository.path,
        &branch,
        force,
        expected_oid.as_deref(),
        &CancellationToken::new(),
        &executor,
    );
    drop(executor);
    match result {
        Ok(()) => Reply::BranchDeleted { branch },
        Err(error) => {
            let continuation = if error.kind == BranchDeleteFailureKind::Unmerged && !force {
                match inspection
                    .validate_branch()
                    .and_then(|()| retain(host, inspection, Phase::Removed, Some(token)))
                {
                    Ok(token) => Some(token),
                    Err(error) => return failure(error),
                }
            } else {
                None
            };
            Reply::BranchDeleteRefused {
                failure: error,
                continuation,
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static AFTER_REMOVE: std::cell::RefCell<Option<Box<dyn FnOnce(&mut Inspection)>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn after_remove_for_test(inspection: &mut Inspection) {
    let hook = AFTER_REMOVE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(inspection);
    }
}
