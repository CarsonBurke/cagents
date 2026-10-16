//! Manual removal snapshots under exclusive admission; process I/O runs off HostState.
use super::*;
use crate::daemon::holder_registry::{self as holders, Kind, Process, Record};
use std::{io, path::Path};

pub(in crate::host) struct CleanupScope {
    directories: Vec<PathBuf>,
    terminal_holders: BTreeMap<TerminalId, Record>,
    native_holders: BTreeMap<TerminalId, Record>,
}

fn uncertain(detail: impl Into<String>) -> String {
    detail.into()
}

impl HostState {
    /// Snapshot under the exclusive control gate; check off HostState before
    /// acquiring the candidate directory lease and dispatching removal.
    pub(in crate::host) fn worktree_removal_scope(&self) -> Result<CleanupScope, String> {
        if self.shutting_down || self.upgrade.is_some() {
            return Err("Daemon shutdown or upgrade is in progress".into());
        }
        if !self.native_adoption_failures.is_empty() {
            return Err(uncertain("Native adoption ownership has not settled"));
        }
        // These reservations predate a launch recipe. Guessing their cwd would
        // turn an unavailable attribution into permission to delete.
        if !self.spawning.is_empty() || !self.browser_logins_spawning.is_empty() {
            return Err(uncertain(
                "A terminal launch has not established its working directory",
            ));
        }
        let mut scope = CleanupScope {
            directories: Vec::new(),
            terminal_holders: BTreeMap::new(),
            native_holders: BTreeMap::new(),
        };
        for (id, record) in &self.terminals {
            if !record.handle.is_completed() {
                scope
                    .directories
                    .push(record.session.launch.working_directory.clone());
                scope
                    .directories
                    .push(record.current_directory().to_owned());
            }
            if let Some(holder) = record.handle.holder_identity() {
                scope.terminal_holders.insert(*id, holder.clone());
            }
        }
        for (id, record) in &self.chats {
            if record.handle.owns_work() || record.returning_terminal {
                scope
                    .directories
                    .push(record.session.launch.recipe.working_directory.clone());
            }
            if let Some(holder) = record.handle.native_holder_identity() {
                scope.native_holders.insert(*id, holder.clone());
            }
        }
        for session in self.chat_spawning.values() {
            scope
                .directories
                .push(session.launch.recipe.working_directory.clone());
        }
        for (session, _) in self.execution_startups.values() {
            scope.directories.push(match session {
                crate::agent_execution::HostedSession::Chat(session) => {
                    session.launch.recipe.working_directory.clone()
                }
                crate::agent_execution::HostedSession::Terminal(session) => {
                    session.launch.working_directory.clone()
                }
            });
        }
        for id in self.chat_bindings.keys() {
            let record = self
                .terminals
                .get(id)
                .ok_or_else(|| uncertain("A chat binding has no attributable source terminal"))?;
            scope
                .directories
                .push(record.current_directory().to_owned());
            scope
                .directories
                .push(record.session.launch.working_directory.clone());
        }
        for receipt in self.chat_handoffs.values() {
            scope
                .directories
                .push(receipt.replacement().launch.working_directory.clone());
        }
        for pending in self.native_closing.values() {
            scope
                .directories
                .push(pending.working_directory().to_owned());
            scope.native_holders.insert(
                pending.holder_identity().terminal,
                pending.holder_identity().clone(),
            );
        }
        let registry = self
            .model
            .as_ref()
            .ok_or_else(|| uncertain("The session registry is unavailable"))?;
        scope.directories.extend(
            registry
                .cleanup_execution_directories()
                .map_err(uncertain)?,
        );
        for firing in registry.active_automation_firings().map_err(uncertain)? {
            // Resolve the accepted revision, not a later edited definition.
            let detail = registry
                .automation_run_detail(firing.id)
                .map_err(uncertain)?;
            scope
                .directories
                .push(detail.definition.agent.target.directory);
        }
        if let Some(store) = &self.upgrade_evidence
            && let Some(checkpoint) = store.load().map_err(uncertain)?
        {
            // Retained upgrade ownership is still authoritative after a failed
            // adoption. Never discard or rewrite its checkpoint for cleanup.
            for terminal in checkpoint.terminals {
                scope
                    .directories
                    .push(terminal.source.launch.working_directory);
                scope.directories.push(terminal.recipe.working_directory);
            }
            for terminal in checkpoint.carried {
                scope
                    .directories
                    .push(terminal.source.launch.working_directory);
            }
            for chat in checkpoint.chats {
                scope
                    .directories
                    .push(chat.source.launch.recipe.working_directory);
                scope.directories.push(chat.launch.recipe.working_directory);
            }
        }
        Ok(scope)
    }
}

impl CleanupScope {
    fn check_bindings(&self, candidate: &Path) -> Result<(), String> {
        for directory in &self.directories {
            check_directory(directory, candidate)?;
        }
        Ok(())
    }

    fn check_holder_binding(&self, record: &Record) -> Result<(), String> {
        let known = match record.kind {
            Kind::Terminal => self.terminal_holders.get(&record.terminal) == Some(record),
            Kind::Native => self.native_holders.get(&record.terminal) == Some(record),
        };
        if known {
            Ok(())
        } else {
            Err(uncertain(
                "A detached holder has no authenticated session binding",
            ))
        }
    }

    pub(in crate::host) fn check(self, candidate: &Path) -> Result<(), String> {
        let candidate = candidate.canonicalize().map_err(|error| {
            uncertain(format!("Cannot establish the worktree directory: {error}"))
        })?;
        self.check_bindings(&candidate)?;
        // Direct stdio providers and in-flight helper children have no holder.
        // Inspect their real cwd, as well as immutable session bindings above.
        check_process_family(
            &Process::read(std::process::id() as i32)
                .map_err(|error| uncertain(error.to_string()))?,
            &candidate,
        )?;
        for kind in [Kind::Terminal, Kind::Native] {
            let directory = holders::directory(kind).map_err(uncertain)?;
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(uncertain(error.to_string())),
            };
            // Recovery discovery deliberately retires records whose holder
            // disappeared. Removal uses read-only evidence instead: a dead
            // holder alone does not prove that its descendants stopped.
            for (index, entry) in entries.enumerate() {
                if index >= 8192 {
                    return Err(uncertain(
                        "Holder directory exceeds the cleanup safety bound",
                    ));
                }
                let path = entry.map_err(|error| uncertain(error.to_string()))?.path();
                if path.extension().is_none_or(|extension| extension != "json") {
                    continue;
                }
                let record = holders::evidence(&path.with_extension(""), kind)
                    .map_err(|error| {
                        uncertain(format!("Cannot attribute holder ownership: {error}"))
                    })?
                    .ok_or_else(|| uncertain("Holder ownership changed during cleanup"))?;
                self.check_holder_binding(&record)?;
                check_process_family(&record.holder, &candidate)?;
            }
        }
        Ok(())
    }
}

fn check_directory(directory: &Path, candidate: &Path) -> Result<(), String> {
    // Component-aware matching avoids confusing sibling names such as wt and
    // wt-old; canonicalization catches symlink aliases of the candidate.
    if !directory.is_absolute() {
        return Err(uncertain("A session working directory is not absolute"));
    }
    let directory = directory.canonicalize().map_err(|error| {
        uncertain(format!(
            "Cannot establish a session working directory: {error}"
        ))
    })?;
    if directory.starts_with(candidate) {
        return Err("A running session or retained operation uses this worktree".into());
    }
    Ok(())
}

/// Check all descendants, including background children that changed cwd while
/// their parent shell remained elsewhere. Two identical bounded censuses are
/// required so a changing/reparenting process tree does not look quiescent.
fn check_process_family(root: &Process, candidate: &Path) -> Result<(), String> {
    let mut previous = None;
    for _ in 0..3 {
        let family = process_family(root).map_err(|error| {
            uncertain(format!(
                "Cannot establish hosted process ownership: {error}"
            ))
        })?;
        for process in family.values() {
            let path = format!("/proc/{}/cwd", process.pid);
            let cwd = fs::read_link(path).map_err(|error| {
                uncertain(format!(
                    "Cannot establish a hosted process working directory: {error}"
                ))
            })?;
            if !process.is_current() {
                return Err(uncertain("Hosted process ownership changed during cleanup"));
            }
            check_directory(&cwd, candidate)?;
        }
        if previous.as_ref() == Some(&family) {
            return Ok(());
        }
        previous = Some(family);
    }
    Err(uncertain(
        "Hosted descendants changed during cleanup; retry after they settle",
    ))
}

fn process_family(root: &Process) -> io::Result<BTreeMap<i32, Process>> {
    let mut family = BTreeMap::new();
    let mut pending = vec![root.clone()];
    while let Some(parent) = pending.pop() {
        if !parent.is_current() {
            return Err(io::Error::other("process identity changed"));
        }
        for task in fs::read_dir(format!("/proc/{}/task", parent.pid))? {
            let path = task?.path().join("children");
            let children = match fs::read_to_string(path) {
                Ok(children) => children,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for pid in children.split_whitespace() {
                let pid = pid.parse::<i32>().map_err(io::Error::other)?;
                if family.contains_key(&pid) {
                    continue;
                }
                let stat = match fs::read(format!("/proc/{pid}/stat")) {
                    Ok(stat) => stat,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                let (_, start) = crate::model::agent_recovery::process_identity(&stat)
                    .ok_or_else(|| io::Error::other("invalid child identity"))?;
                let fields = stat
                    .rsplit(|byte| *byte == b')')
                    .next()
                    .ok_or_else(|| io::Error::other("invalid child state"))?;
                // Zombies retain a PID but no working directory or live work.
                if fields
                    .split(|byte| byte.is_ascii_whitespace())
                    .find(|field| !field.is_empty())
                    == Some(b"Z")
                {
                    continue;
                }
                let process = Process { pid, start };
                pending.push(process.clone());
                family.insert(pid, process);
                if family.len() > 4096 {
                    return Err(io::Error::other(
                        "hosted process census exceeded its safety bound",
                    ));
                }
            }
        }
        if !parent.is_current() {
            return Err(io::Error::other("process identity changed"));
        }
    }
    Ok(family)
}
