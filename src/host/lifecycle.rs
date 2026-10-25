//! Deadline-driven retirement. Cleanup and receipt reads run outside HostState.
use super::state::{Retirement, SharedHost};
use crate::{model::TerminalId, terminal::TerminalRetirement};
use std::{
    collections::BTreeSet,
    fs::File,
    sync::Arc,
    time::{Duration, Instant},
};

const EXITED_RETENTION: Duration = Duration::from_secs(10 * 60);
// Match the existing five-second durable-notification retry cadence. Only an
// uncertain expired pin census arms this deadline; it grants no cleanup.
const RETENTION_RETRY: Duration = Duration::from_secs(5);

pub(super) struct Lifecycle {
    retention: Duration,
    retention_error_reported: bool,
    idle: IdleTimer,
    waiting: Vec<Arc<File>>,
}

impl Lifecycle {
    pub(super) fn new() -> Self {
        Self {
            retention: duration_setting("CAGENTS_EXITED_TERMINAL_RETENTION_SECS", EXITED_RETENTION),
            retention_error_reported: false,
            idle: IdleTimer::new(
                crate::instance::IS_DEVELOPMENT,
                duration_setting(
                    "CAGENTS_DEVELOPMENT_IDLE_TIMEOUT_SECS",
                    Duration::from_secs(30 * 60),
                ),
            ),
            waiting: Vec::new(),
        }
    }

    #[cfg(test)]
    fn testing(retention: Duration) -> Self {
        Self {
            retention,
            retention_error_reported: false,
            idle: IdleTimer::new(false, Duration::ZERO),
            waiting: Vec::new(),
        }
    }

    pub(super) fn waiting_holders(&self) -> &[Arc<File>] {
        &self.waiting
    }

    pub(super) fn maintain(&mut self, host: &SharedHost, connected: bool) -> Option<Instant> {
        self.settle_retirements(host);
        let retention = self.maintain_retention(host);
        if !self.idle.enabled {
            return retention;
        }
        let (candidate, gate, registry, evidence) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            (
                state.development_idle() && !connected,
                state.control_gate.clone(),
                state.execution_registry.clone(),
                state.upgrade_evidence.clone(),
            )
        };
        let now = Instant::now();
        let idle = if candidate {
            // Empty ownership is checked under the existing admission writer;
            // no new launch may slip between that check and shutdown's latch.
            let _admission = gate.write().unwrap_or_else(|p| p.into_inner());
            let quiescent = registry.is_some_and(|registry| {
                let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
                matches!(registry.cleanup_execution_is_quiescent(), Ok(true))
                    && registry
                        .active_automation_firings()
                        .is_ok_and(|runs| runs.is_empty())
                    && matches!(registry.next_automation_deadline(), Ok(None))
            });
            let recovered = evidence.is_none_or(|store| {
                store.load().is_ok_and(|saved| {
                    saved.is_none_or(|saved| {
                        saved.terminals.is_empty()
                            && saved.carried.is_empty()
                            && saved.chats.is_empty()
                    })
                }) && store.unattached().is_ok_and(|rows| rows.is_empty())
            });
            let no_holders = quiescent && recovered && holders_empty();
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let idle = no_holders && !connected && state.development_idle();
            if idle && self.idle.expired(now) {
                tracing::info!("Stopping an unused development daemon");
                state.request_shutdown();
                crate::daemon::wake();
            }
            idle
        } else {
            false
        };
        let idle = self.idle.update(now, idle);
        match (retention, idle) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn settle_retirements(&mut self, host: &SharedHost) {
        self.waiting.clear();
        let (gate, registry, proof, observations) = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let observations = state
                .terminal_retirements
                .iter()
                .filter_map(|(id, entry)| {
                    if entry.explicit_close {
                        return None;
                    }
                    let source = state
                        .terminals
                        .get(id)
                        .filter(|record| record.handle.same_worker(&entry.handle))
                        .map(|record| record.session.clone());
                    if entry.failure.is_some() && source.is_some() {
                        return None;
                    }
                    Some((*id, entry.ticket, entry.handle.clone(), source))
                })
                .collect::<Vec<_>>();
            (
                state.control_gate.clone(),
                state.execution_registry.clone(),
                state.proof_handle(),
                observations,
            )
        };
        // In particular, receipt authorization and filesystem reads must not
        // block terminal event callbacks or keep shared admission held.
        let mut ready = Vec::new();
        for (id, ticket, handle, source) in observations {
            let Some(source) = source else {
                ready.push((id, ticket, handle, None));
                continue;
            };
            match handle.completed_retirement(&source, &proof) {
                TerminalRetirement::Pending => {}
                TerminalRetirement::WaitingForHolder(original) => self.waiting.push(original),
                TerminalRetirement::Released => ready.push((id, ticket, handle, Some(Ok(())))),
                TerminalRetirement::Retained(error) => {
                    ready.push((id, ticket, handle, Some(Err(error))))
                }
            }
        }
        if ready.is_empty() {
            return;
        }
        let admission = gate.write().unwrap_or_else(|p| p.into_inner());
        let candidates = ready
            .iter()
            .filter_map(|(id, _, _, result)| matches!(result, Some(Ok(()))).then_some(*id))
            .collect();
        // Fences normal pin publications and rechecks durable membership just
        // before removal. Unknown or newly pinned results remain listed.
        let pinned = registry
            .ok_or_else(|| "The workspace registry is unavailable".to_owned())
            .and_then(|registry| {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .pinned_terminals(&candidates)
            });
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        let mut dropped = Vec::new();
        let mut retired = Vec::new();
        for (id, ticket, handle, result) in ready {
            if !state
                .terminal_retirements
                .get(&id)
                .is_some_and(|entry| entry.ticket == ticket && entry.handle.same_worker(&handle))
            {
                continue;
            }
            if !state
                .terminals
                .get(&id)
                .is_some_and(|record| record.handle.same_worker(&handle))
            {
                dropped.push(state.terminal_retirements.remove(&id));
                continue;
            }
            let error = match result {
                Some(Err(error)) => Some(error),
                Some(Ok(())) => match &pinned {
                    Ok(pins) if !pins.contains(&id) => None,
                    Ok(_) => Some(
                        "A saved pane claimed this terminal while cleanup settled; result retained"
                            .into(),
                    ),
                    Err(error) => Some(format!(
                        "Terminal cleanup finished but pane ownership is unknown: {error}"
                    )),
                },
                None => continue,
            };
            if let Some(error) = error {
                tracing::warn!(terminal = %id, %error, "Completed terminal remains listed for explicit cleanup");
                state.terminal_retirements.get_mut(&id).unwrap().failure = Some(error);
                state.inventory_generation += 1;
            } else {
                retired.push(state.retire_terminal_if_current(id, &handle));
            }
        }
        drop(state);
        drop(admission);
        drop(retired);
        drop(dropped);
    }

    fn maintain_retention(&mut self, host: &SharedHost) -> Option<Instant> {
        let now = Instant::now();
        let (deadline, gate, registry) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.observe_completed_terminals(now);
            (
                state.terminal_retention_deadline(self.retention, &BTreeSet::new()),
                state.control_gate.clone(),
                state.execution_registry.clone(),
            )
        };
        let deadline = deadline?;
        if deadline > now {
            return Some(deadline);
        }
        let admission = gate.write().unwrap_or_else(|p| p.into_inner());
        let candidates = host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .expired_terminal_handles(now, self.retention, &BTreeSet::new())
            .into_iter()
            .map(|(id, _)| id)
            .collect::<BTreeSet<TerminalId>>();
        let pinned = registry
            .ok_or_else(|| "The workspace registry is unavailable".to_owned())
            .and_then(|registry| {
                registry
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .pinned_terminals(&candidates)
            });
        let pinned = match pinned {
            Ok(pinned) => {
                self.retention_error_reported = false;
                pinned
            }
            Err(error) => {
                if !self.retention_error_reported {
                    tracing::warn!(%error, "Completed terminal retention is deferred; pane ownership is unknown");
                    self.retention_error_reported = true;
                }
                // No holder retirement has started, so there is no pidfd edge
                // to wake a quiet listener after the pin census recovers. Keep
                // exact ownership and retry on a real future deadline instead.
                return Some(Instant::now() + RETENTION_RETRY);
            }
        };
        let handles = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let handles = state.expired_terminal_handles(now, self.retention, &pinned);
            for (id, handle) in &handles {
                state
                    .terminal_retirements
                    .insert(*id, Retirement::submitting(handle.clone()));
                state.inventory_generation += 1;
            }
            handles
        };
        drop(admission);
        // Already-reserved join service, no new cleanup thread or queue.
        for (id, handle) in handles {
            if let Err(error) = handle.begin_completed_retirement() {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(entry) = state.terminal_retirements.get_mut(&id)
                    && entry.handle.same_worker(&handle)
                {
                    entry.failure = Some(error.into());
                }
            }
        }
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminal_retention_deadline(self.retention, &pinned)
    }
}

fn duration_setting(name: &str, fallback: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds <= 365 * 24 * 60 * 60)
        .map(Duration::from_secs)
        .unwrap_or(fallback)
}

struct IdleTimer {
    enabled: bool,
    timeout: Duration,
    deadline: Option<Instant>,
}
impl IdleTimer {
    fn new(enabled: bool, timeout: Duration) -> Self {
        Self {
            enabled,
            timeout,
            deadline: None,
        }
    }
    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
    fn update(&mut self, now: Instant, idle: bool) -> Option<Instant> {
        if self.enabled && idle {
            self.deadline.get_or_insert(now + self.timeout);
        } else {
            self.deadline = None;
        }
        self.deadline
    }
}

fn holders_empty() -> bool {
    use crate::daemon::holder_registry::{self as registry, Kind};
    [Kind::Terminal, Kind::Native].into_iter().all(|kind| {
        registry::directory(kind)
            .is_ok_and(|directory| holder_directory_empty(&directory).unwrap_or(false))
    })
}

/// Absence observation must never repair permissions, parse away uncertainty,
/// remove a stale holder's still-live root evidence, or contact a process.
fn holder_directory_empty(directory: &std::path::Path) -> std::io::Result<bool> {
    use std::{
        fs, io,
        os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
    };
    crate::secure_file::reject_symlink_components(directory)?;
    let private = |metadata: &fs::Metadata| {
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == crate::daemon::current_uid()
            && metadata.mode() & 0o077 == 0
    };
    let before = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = directory
                .parent()
                .ok_or_else(|| io::Error::other("holder directory has no parent"))?;
            return if private(&fs::symlink_metadata(parent)?) {
                Ok(true)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe missing-holder directory parent",
                ))
            };
        }
        Err(error) => return Err(error),
    };
    if !private(&before) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe holder directory",
        ));
    }
    let held = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory)?;
    let opened = held.metadata()?;
    if !private(&opened) || (opened.dev(), opened.ino()) != (before.dev(), before.ino()) {
        return Err(io::Error::other(
            "holder directory changed during absence observation",
        ));
    }
    // Readdir is bound to the already-validated directory, even if its public
    // name is replaced while scanning. No record bytes or targets are opened.
    for (index, entry) in fs::read_dir(format!("/proc/self/fd/{}", held.as_raw_fd()))?.enumerate() {
        if index >= 8192 {
            return Err(io::Error::other("holder absence census limit exceeded"));
        }
        if std::path::Path::new(&entry?.file_name())
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            return Ok(false);
        }
    }
    let after = fs::symlink_metadata(directory)?;
    if !private(&after) || (after.dev(), after.ino()) != (opened.dev(), opened.ino()) {
        return Err(io::Error::other(
            "holder directory changed before absence was confirmed",
        ));
    }
    Ok(true)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{
        daemon::{
            HostLease, RuntimePaths,
            holder_registry::{Kind, Process, Record},
        },
        host::{lock::HostMutex, model::Registry, state::HostState},
        model::{
            HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe, TerminalContinuity,
            TerminalSession,
        },
        terminal::{TerminalConfig, TerminalHandle},
    };
    use std::{
        collections::BTreeMap,
        fs,
        io::Write,
        os::{
            fd::FromRawFd,
            unix::fs::{MetadataExt, PermissionsExt, symlink},
        },
        process::{Child, Command, Stdio},
        thread,
    };

    fn isolated_lifecycle_case() -> bool {
        const CHILD: &str = "CAGENTS_LIFECYCLE_REGRESSION_CHILD";
        let name = thread::current().name().unwrap().to_owned();
        if std::env::var(CHILD).as_deref() == Ok(name.as_str()) {
            return false;
        }
        let private = tempfile::Builder::new().prefix("lc-").tempdir().unwrap();
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let output = private.path().join("child.log");
        let log = File::create(&output).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture", "--test-threads=1"])
            .env(CHILD, &name)
            .env("XDG_RUNTIME_DIR", private.path())
            .env("TMPDIR", private.path())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        // HOME and opaque caller authority remain inherited; only fixture paths
        // and this exact self-child selector change.
        let deadline = Instant::now() + Duration::from_secs(40);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "lifecycle child timed out; evidence: {}",
                    private.keep().display()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        let output = fs::read_to_string(output).unwrap();
        print!("{output}");
        assert!(
            status.success(),
            "lifecycle child failed ({status}); evidence: {}",
            private.keep().display()
        );
        assert!(output.contains(&name) && output.contains("1 passed; 0 failed"));
        true
    }

    fn fixture_host() -> (SharedHost, HostLease, RuntimePaths, std::path::PathBuf) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new(uuid::Uuid::new_v4().to_string()),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new(uuid::Uuid::new_v4().to_string()),
        };
        let paths = RuntimePaths::discover().unwrap();
        let lease = HostLease::acquire(&paths, &proof).unwrap();
        let db_path = paths.directory.join("lifecycle.sqlite3");
        let mut state = HostState::new(proof);
        state.install_model(Registry::at(&db_path).unwrap());
        (Arc::new(HostMutex::new(state)), lease, paths, db_path)
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "actual lifecycle fixture did not settle"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    struct CensusChild {
        child: Child,
        identity: Process,
        _original: File,
    }
    impl CensusChild {
        fn start() -> Self {
            let mut child = Command::new("/bin/sh")
                .args(["-c", "IFS= read -r release"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            // SAFETY: this original unreaped Child is blocked on its private
            // stdin; pidfd_open takes scalar arguments and returns a fresh fd.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                panic!("pin actual census child: {error}");
            }
            // SAFETY: the successful syscall above transferred this fresh fd.
            let original = unsafe { File::from_raw_fd(fd as i32) };
            let identity = Process::read(child.id() as i32).unwrap();
            Self {
                child,
                identity,
                _original: original,
            }
        }
        fn finish(&mut self) {
            self.child
                .stdin
                .take()
                .unwrap()
                .write_all(b"release\n")
                .unwrap();
            wait_until(|| {
                self.child.try_wait().unwrap().is_some_and(|status| {
                    assert!(status.success());
                    true
                })
            });
            assert!(!self.identity.is_current());
        }
    }
    impl Drop for CensusChild {
        fn drop(&mut self) {
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    #[test]
    fn idle_census_preserves_dead_holder_live_root_evidence_and_blocks_shutdown() {
        if isolated_lifecycle_case() {
            return;
        }
        let (host, _lease, paths, _) = fixture_host();
        let proof = host.lock().unwrap().proof().clone();
        let directory = paths.directory.join("holders");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut root = CensusChild::start();
        let mut holder = CensusChild::start();
        holder.finish();
        let record = Record {
            version: 4,
            build: None,
            kind: Kind::Terminal,
            application: crate::instance::APPLICATION_ID.into(),
            boot: proof.boot_id.clone(),
            terminal: TerminalId::new(),
            holder: holder.identity.clone(),
            root: root.identity.clone(),
        };
        record.create(&directory).unwrap();
        let path = directory.join(format!("{}.json", record.terminal));
        let before = fs::read(&path).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(!record.holder.is_current() && record.root.is_current());
        assert!(!holders_empty());
        let mut lifecycle = Lifecycle::testing(Duration::ZERO);
        lifecycle.idle = IdleTimer::new(true, Duration::ZERO); // explicit development-mode fixture
        assert!(lifecycle.maintain(&host, false).is_none());
        assert!(!host.lock().unwrap().is_shutting_down());
        assert!(root.child.try_wait().unwrap().is_none());
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "absence check must not erase orphan-root evidence"
        );
        let after = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (after.dev(), after.ino(), after.mode()),
            (metadata.dev(), metadata.ino(), metadata.mode())
        );
        root.finish();
        assert!(
            !holders_empty(),
            "an unresolved record remains evidence even after its processes exit"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_file(path).unwrap(); // explicit fixture cleanup, never discovery side effect
        assert!(holders_empty());
        assert!(
            !paths.directory.join("native-holders").exists(),
            "read-only census must not create missing holder directories"
        );
        assert!(lifecycle.maintain(&host, false).is_some());
        lifecycle.maintain(&host, false);
        assert!(
            host.lock().unwrap().is_shutting_down(),
            "clean explicit development idle fixture should still stop"
        );
    }

    #[test]
    fn holder_absence_census_keeps_missing_unknown_and_unsafe_paths_distinct() {
        let private = tempfile::Builder::new().prefix("hc-").tempdir().unwrap();
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let missing = private.path().join("missing");
        assert!(holder_directory_empty(&missing).unwrap());
        assert!(!missing.exists());
        let directory = private.path().join("holders");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let unknown = directory.join("unknown.json");
        fs::write(&unknown, b"not a record; do not delete").unwrap();
        assert!(!holder_directory_empty(&directory).unwrap());
        assert_eq!(fs::read(&unknown).unwrap(), b"not a record; do not delete");
        fs::remove_file(&unknown).unwrap();
        let target = private.path().join("unread-target");
        fs::write(&target, b"preserved").unwrap();
        symlink(&target, &unknown).unwrap();
        assert!(!holder_directory_empty(&directory).unwrap());
        assert_eq!(fs::read(&target).unwrap(), b"preserved");
        fs::remove_file(&unknown).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(holder_directory_empty(&directory).is_err());
        assert_eq!(
            fs::symlink_metadata(&directory).unwrap().mode() & 0o777,
            0o755
        );
        assert!(holder_directory_empty(&directory.join("missing")).is_err());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = private.path().join("alias");
        symlink(&directory, &alias).unwrap();
        assert!(holder_directory_empty(&alias).is_err());
        assert!(holder_directory_empty(&alias.join("missing")).is_err());
        assert!(holder_directory_empty(&directory).unwrap());
    }

    #[test]
    fn real_completed_terminal_waits_for_attachment_sqlite_pin_and_query_recovery_before_retirement()
     {
        if isolated_lifecycle_case() {
            return;
        }
        let (host, _lease, paths, db_path) = fixture_host();
        let proof = host.lock().unwrap().proof().clone();
        let source = TerminalSession {
            id: TerminalId::new(),
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec![
                    "-c".into(),
                    "printf 'retained-lifecycle-result'; exit 7".into(),
                ],
                working_directory: paths.directory.clone(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            },
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        };
        let recovery = crate::terminal::holder::Recovery {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: Default::default(),
            execution_environment: None,
            execution_recipe: None,
        };
        let handle = TerminalHandle::spawn(
            source.clone(),
            TerminalConfig {
                holder_recovery: Some(recovery),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        wait_until(|| handle.is_completed());
        let original = handle.holder_identity().unwrap().clone();
        let registry = {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.reserve_spawn(source.id).unwrap();
            state
                .finish_spawn(1, source.clone(), handle.clone(), false)
                .unwrap();
            state.execution_registry.clone().unwrap()
        };
        let mut lifecycle = Lifecycle::testing(Duration::ZERO);
        lifecycle.maintain(&host, true);
        assert_eq!(
            host.lock().unwrap().list(1).len(),
            1,
            "attached final output must remain available"
        );
        assert!(host.lock().unwrap().terminal_retirements.is_empty());
        assert!(original.holder.is_current());
        registry
            .lock()
            .unwrap()
            .pin_window_terminals(&[source.id])
            .unwrap();
        host.lock().unwrap().detach(1, source.id).unwrap();
        lifecycle.maintain(&host, true);
        assert_eq!(
            host.lock().unwrap().list(1).len(),
            1,
            "saved detached pane must remain listed"
        );
        assert!(host.lock().unwrap().terminal_retirements.is_empty());
        {
            let mut registry = registry.lock().unwrap();
            let generation = registry
                .retention_pin_generation(source.id)
                .unwrap()
                .unwrap();
            registry
                .forget_retention_pin(source.id, &generation)
                .unwrap();
        }
        // An unavailable pin table must defer cleanup rather than infer no
        // saved membership. Preserve its exact published DDL: renaming back
        // leaves a quoted identifier that correctly fails schema admission.
        let fault = rusqlite::Connection::open(&db_path).unwrap();
        let published_pin_sql: String = fault
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name='terminal_retention_pins'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        fault
            .execute_batch(
                "ALTER TABLE terminal_retention_pins RENAME TO unavailable_retention_pins",
            )
            .unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .pinned_terminals(&BTreeSet::from([source.id]))
                .is_err()
        );
        lifecycle.maintain(&host, true);
        assert_eq!(host.lock().unwrap().list(1).len(), 1);
        assert!(host.lock().unwrap().terminal_retirements.is_empty());
        assert!(original.holder.is_current());
        let restored = fault.unchecked_transaction().unwrap();
        restored.execute_batch(&published_pin_sql).unwrap();
        restored
            .execute_batch(
                "INSERT INTO terminal_retention_pins(terminal,generation)
                 SELECT terminal,generation FROM unavailable_retention_pins;
                 DROP TABLE unavailable_retention_pins;",
            )
            .unwrap();
        restored.commit().unwrap();
        assert_eq!(
            fault
                .query_row::<String, _, _>(
                    "SELECT sql FROM sqlite_schema WHERE name='terminal_retention_pins'",
                    [],
                    |row| row.get(0),
                )
                .unwrap(),
            published_pin_sql,
            "fault recovery must restore the authenticated published layout"
        );
        drop(fault);
        assert!(
            registry
                .lock()
                .unwrap()
                .pinned_terminals(&BTreeSet::from([source.id]))
                .expect("restored pin census must pass current-schema admission")
                .is_empty()
        );
        lifecycle.maintain(&host, true);
        assert!(
            host.lock()
                .unwrap()
                .terminal_retirements
                .contains_key(&source.id)
        );
        assert!(
            host.lock().unwrap().attach(1, source.id).is_err(),
            "pending original retirement fences reattachment"
        );
        wait_until(|| {
            lifecycle.maintain(&host, true);
            host.lock().unwrap().list(1).is_empty()
        });
        assert!(host.lock().unwrap().terminal_retirements.is_empty());
        assert!(!original.root.is_current());
        wait_until(|| !original.holder.is_current());
        assert!(crate::terminal::holder::cleanup::confirmed(&source, &original, &proof).unwrap());

        // A second real completed PTY reaches a genuinely Released status while
        // an explicit caller still owns its ticket/response. Automatic cleanup
        // must not steal that authority merely because receipt work is ready.
        let mut explicit_source = source.clone();
        explicit_source.id = TerminalId::new();
        let explicit =
            TerminalHandle::spawn(explicit_source.clone(), TerminalConfig::default(), |_| {})
                .unwrap();
        wait_until(|| explicit.is_completed());
        let ticket = {
            let mut state = host.lock().unwrap();
            state.reserve_spawn(explicit_source.id).unwrap();
            state
                .finish_spawn(1, explicit_source.clone(), explicit.clone(), false)
                .unwrap();
            let reservation = Retirement::explicit_close(explicit.clone());
            let ticket = reservation.ticket;
            state
                .terminal_retirements
                .insert(explicit_source.id, reservation);
            ticket
        };
        explicit.begin_completed_retirement().unwrap();
        wait_until(|| {
            matches!(
                explicit.completed_retirement(&explicit_source, &proof),
                TerminalRetirement::Released
            )
        });
        lifecycle.maintain(&host, true);
        {
            let state = host.lock().unwrap();
            assert!(
                state
                    .list(1)
                    .iter()
                    .any(|descriptor| descriptor.session.id == explicit_source.id)
            );
            let reservation = state
                .terminal_retirements
                .get(&explicit_source.id)
                .expect("automatic settlement stole explicit close");
            assert_eq!(reservation.ticket, ticket);
            assert!(reservation.explicit_close && reservation.handle.same_worker(&explicit));
        }
        let retired = {
            let mut state = host.lock().unwrap();
            let reservation = state
                .terminal_retirements
                .remove(&explicit_source.id)
                .unwrap();
            assert_eq!(reservation.ticket, ticket);
            (
                reservation,
                state.retire_terminal_if_current(explicit_source.id, &explicit),
            )
        };
        drop(retired); // fixture owner settles outside HostState, after proof
        assert!(host.lock().unwrap().list(1).is_empty());
    }

    #[test]
    fn failed_pin_census_schedules_quiet_listener_retry_and_keeps_original_until_recovery() {
        if isolated_lifecycle_case() {
            return;
        }
        let (host, _lease, paths, db_path) = fixture_host();
        let proof = host.lock().unwrap().proof().clone();
        let source = TerminalSession {
            id: TerminalId::new(),
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec![
                    "-c".into(),
                    "printf 'retained-lifecycle-result'; exit 7".into(),
                ],
                working_directory: paths.directory.clone(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            },
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        };
        let recovery = crate::terminal::holder::Recovery {
            source: source.clone(),
            previous_sources: Vec::new(),
            relaunches: Vec::new(),
            options: Default::default(),
            execution_environment: None,
            execution_recipe: None,
        };
        let handle = TerminalHandle::spawn(
            source.clone(),
            TerminalConfig {
                holder_recovery: Some(recovery),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        wait_until(|| handle.is_completed());
        let original = handle.holder_identity().unwrap().clone();
        let registry = {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.reserve_spawn(source.id).unwrap();
            state
                .finish_spawn(1, source.clone(), handle.clone(), false)
                .unwrap();
            let registry = state.execution_registry.clone().unwrap();
            drop(state.unregister_client(1));
            registry
        };
        let fault = rusqlite::Connection::open(&db_path).unwrap();
        let published_version: u32 = fault
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        fault
            .pragma_update(None, "user_version", published_version + 1)
            .unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .pinned_terminals(&BTreeSet::from([source.id]))
                .is_err()
        );
        let mut lifecycle = Lifecycle::testing(Duration::ZERO);
        let before = Instant::now();
        let retry = lifecycle
            .maintain(&host, false)
            .expect("an uncertain expired census must not leave a quiet listener parked forever");
        let after = Instant::now();
        assert!(retry >= before + RETENTION_RETRY);
        assert!(retry <= after + RETENTION_RETRY);
        assert!(lifecycle.waiting_holders().is_empty());
        {
            let state = host.lock().unwrap();
            assert!(state.clients.is_empty());
            assert_eq!(state.list(1).len(), 1);
            assert!(state.terminals[&source.id].handle.same_worker(&handle));
            assert!(state.terminal_retirements.is_empty());
            assert!(!state.development_idle());
            assert!(!state.is_shutting_down());
        }
        assert!(original.holder.is_current());

        // The scheduled deadline, not another client or holder event, gives
        // the listener a next maintenance turn. Repair the real census and
        // exercise that turn directly; no sleep or fake cleanup authorization.
        fault
            .pragma_update(None, "user_version", published_version)
            .unwrap();
        drop(fault);
        assert!(
            registry
                .lock()
                .unwrap()
                .pinned_terminals(&BTreeSet::from([source.id]))
                .unwrap()
                .is_empty()
        );
        lifecycle.maintain(&host, false);
        {
            let state = host.lock().unwrap();
            let retirement = state.terminal_retirements.get(&source.id).unwrap();
            assert!(retirement.handle.same_worker(&handle));
            assert!(!retirement.explicit_close);
            assert!(retirement.failure.is_none());
        }
        wait_until(|| {
            lifecycle.maintain(&host, false);
            host.lock().unwrap().list(1).is_empty()
        });
        assert!(host.lock().unwrap().terminal_retirements.is_empty());
        assert!(!original.root.is_current());
        wait_until(|| !original.holder.is_current());
        assert!(crate::terminal::holder::cleanup::confirmed(&source, &original, &proof).unwrap());
    }

    #[test]
    fn completed_handoff_replacement_waits_for_durable_ack_and_saved_pin_release() {
        if isolated_lifecycle_case() {
            return;
        }
        let (host, _lease, paths, _) = fixture_host();
        let proof = host.lock().unwrap().proof().clone();
        let replacement = TerminalSession {
            id: TerminalId::new(),
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec![
                    "-c".into(),
                    "printf 'handoff-retention-result'; exit 0".into(),
                ],
                working_directory: paths.directory.clone(),
                environment: BTreeMap::new(),
                unset_environment: Default::default(),
            },
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: proof.host_session_id.clone(),
                boot_id: proof.boot_id.clone(),
                host_incarnation: proof.host_incarnation.clone(),
            },
        };
        let mut unrelated = replacement.clone();
        unrelated.id = TerminalId::new();
        let start = |source: &TerminalSession| {
            let handle = TerminalHandle::spawn(
                source.clone(),
                TerminalConfig {
                    holder_recovery: Some(crate::terminal::holder::Recovery {
                        source: source.clone(),
                        previous_sources: Vec::new(),
                        relaunches: Vec::new(),
                        options: Default::default(),
                        execution_environment: None,
                        execution_recipe: None,
                    }),
                    ..Default::default()
                },
                |_| {},
            )
            .unwrap();
            wait_until(|| handle.is_completed());
            handle
        };
        let retained = start(&replacement);
        let ordinary = start(&unrelated);
        let original = retained.holder_identity().unwrap().clone();
        let ordinary_original = ordinary.holder_identity().unwrap().clone();
        let chat_id = TerminalId::new();
        assert_ne!(chat_id, replacement.id);
        assert_ne!(chat_id, unrelated.id);
        let registry = {
            let mut state = host.lock().unwrap();
            state.register_client(1);
            state.reserve_spawn(replacement.id).unwrap();
            let descriptor = state
                .finish_spawn(1, replacement.clone(), retained.clone(), false)
                .unwrap();
            state.reserve_spawn(unrelated.id).unwrap();
            state
                .finish_spawn(1, unrelated.clone(), ordinary.clone(), false)
                .unwrap();
            // This is the same receipt constructor and OLD-chat-key/NEW-
            // terminal-value publication used by ChatReturnTerminal. No
            // provider or native runtime is invented: its retired source is
            // metadata, and the replacement is the real PTY above.
            state.chat_handoffs.insert(
                chat_id,
                crate::host::chat::TerminalHandoff::new(
                    crate::chat::Session {
                        id: chat_id,
                        launch: crate::chat::Launch {
                            provider: crate::agent::Provider::Claude,
                            recipe: replacement.launch.clone(),
                            session_id: Some("already-returned-handoff-fixture".into()),
                        },
                        proof: Some(proof.clone()),
                        terminal: None,
                        owned_codex_listener: None,
                        native_continuity: Default::default(),
                    },
                    descriptor.session,
                    1,
                    BTreeMap::from([(1, false)]),
                    Instant::now() + crate::host::chat::ACKNOWLEDGEMENT_GRACE,
                ),
            );
            state.execution_registry.clone().unwrap()
        };
        // Genuine disconnect removes both subscriptions. There is no durable
        // replacement pin yet: only the pending receipt prevents cleanup.
        drop(host.lock().unwrap().unregister_client(1));
        assert!(
            registry
                .lock()
                .unwrap()
                .pinned_terminals(&BTreeSet::from([replacement.id, unrelated.id]))
                .unwrap()
                .is_empty()
        );
        let mut lifecycle = Lifecycle::testing(Duration::ZERO);
        wait_until(|| {
            lifecycle.maintain(&host, false);
            let state = host.lock().unwrap();
            assert!(
                state
                    .terminals
                    .get(&replacement.id)
                    .is_some_and(|record| record.handle.same_worker(&retained))
            );
            assert!(!state.terminal_retirements.contains_key(&replacement.id));
            assert!(state.chat_handoffs.contains_key(&chat_id));
            assert!(original.holder.is_current());
            !state.terminals.contains_key(&unrelated.id)
        });
        wait_until(|| !ordinary_original.holder.is_current());
        assert!(
            crate::terminal::holder::cleanup::confirmed(&unrelated, &ordinary_original, &proof)
                .unwrap()
        );

        host.lock().unwrap().register_client(2);
        // An idempotent acknowledgement for the old source ID cannot consume
        // the replacement receipt or authorize automatic retirement.
        let (response, _, _) = crate::host::client::process_request(
            &host,
            2,
            crate::terminal_ipc::ClientRequest::ChatCommitTerminal {
                terminal_id: chat_id,
            },
        )
        .unwrap();
        assert!(matches!(
            response,
            crate::terminal_ipc::ServerResponse::Accepted
        ));
        lifecycle.maintain(&host, false);
        assert!(host.lock().unwrap().chat_handoffs.contains_key(&chat_id));
        assert!(original.holder.is_current());

        // Match the real ordering: durable saved membership commits BEFORE
        // the normal authenticated handler acknowledges the replacement.
        registry
            .lock()
            .unwrap()
            .pin_window_terminals(&[replacement.id])
            .unwrap();
        let pin = registry
            .lock()
            .unwrap()
            .retention_pin_generation(replacement.id)
            .unwrap()
            .expect("actual committed SQLite pin");
        let (response, _, _) = crate::host::client::process_request(
            &host,
            2,
            crate::terminal_ipc::ClientRequest::ChatCommitTerminal {
                terminal_id: replacement.id,
            },
        )
        .unwrap();
        assert!(matches!(
            response,
            crate::terminal_ipc::ServerResponse::Accepted
        ));
        assert!(!host.lock().unwrap().chat_handoffs.contains_key(&chat_id));
        lifecycle.maintain(&host, false);
        assert!(host.lock().unwrap().terminals.contains_key(&replacement.id));
        assert!(host.lock().unwrap().terminal_retirements.is_empty());
        assert!(
            original.holder.is_current(),
            "saved membership still owns the result"
        );
        // Closing that saved pane removes only its actual captured generation.
        // After both acknowledgement and pin release, normal retirement runs.
        registry
            .lock()
            .unwrap()
            .forget_retention_pin(replacement.id, &pin)
            .unwrap();
        wait_until(|| {
            lifecycle.maintain(&host, false);
            host.lock().unwrap().terminals.is_empty()
        });
        wait_until(|| !original.holder.is_current());
        assert!(
            crate::terminal::holder::cleanup::confirmed(&replacement, &original, &proof).unwrap()
        );
        assert!(host.lock().unwrap().terminal_retirements.is_empty());
    }
}
