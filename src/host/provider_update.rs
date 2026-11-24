//! Serializes provider installation changes with the beginnings of launches.

mod install;
mod preferences;

use std::{
    ffi::OsStr,
    future::Future,
    sync::{Arc, Condvar, Mutex, MutexGuard, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    account::{
        cache::Cache,
        home::Homes,
        probe,
        provider_install::{ProviderInstallMethod, ProviderStatus, ProviderUpdateResult},
        registry::Store,
    },
    agent::Provider,
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
};

use super::{
    control_gate::{ControlGate, Shared},
    provider_status::inspect,
    service,
    state::{HostLock, SharedHost},
};

const INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const DEFERRED_RETRY: Duration = Duration::from_secs(10 * 60);
const LAUNCH_PATIENCE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct Controller {
    providers: [Arc<ProviderUpdate>; Provider::ALL.len()],
}

#[derive(Default)]
struct ProviderUpdate {
    admission: Mutex<Admission>,
    swap_finished: Condvar,
    status: Mutex<Option<ProviderStatus>>,
}

#[derive(Default)]
struct Admission {
    launches: usize,
    checking: bool,
    applying: bool,
    apply_generation: u64,
}

#[derive(Clone, Copy)]
enum Operation {
    Launch,
    Check,
    Apply,
}

/// Owns admission without holding a mutex guard across a subprocess or I/O.
#[must_use = "provider admission ends on drop"]
pub(crate) struct Permit {
    update: Arc<ProviderUpdate>,
    operation: Operation,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.update.admission();
        match self.operation {
            Operation::Launch => state.launches -= 1,
            Operation::Check => state.checking = false,
            Operation::Apply => {
                state.applying = false;
                drop(state);
                self.update.swap_finished.notify_all();
            }
        }
    }
}

impl ProviderUpdate {
    fn admission(&self) -> MutexGuard<'_, Admission> {
        self.admission.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn try_enter(self: &Arc<Self>, operation: Operation) -> Option<Permit> {
        let mut state = self.admission();
        match operation {
            Operation::Check if !state.checking => state.checking = true,
            Operation::Apply if !state.applying && state.launches == 0 => {
                state.applying = true;
                state.apply_generation = state.apply_generation.wrapping_add(1);
            }
            _ => return None,
        }
        Some(self.permit(operation))
    }

    fn permit(self: &Arc<Self>, operation: Operation) -> Permit {
        Permit {
            update: Arc::clone(self),
            operation,
        }
    }

    fn launch(self: &Arc<Self>, patience: Duration) -> Option<Permit> {
        let mut state = self
            .swap_finished
            .wait_timeout_while(self.admission(), patience, |state| state.applying)
            .unwrap_or_else(|p| p.into_inner())
            .0;
        if state.applying {
            return None;
        }
        state.launches += 1;
        Some(self.permit(Operation::Launch))
    }

    fn idle_generation(&self) -> Option<u64> {
        let state = self.admission();
        (!state.applying).then_some(state.apply_generation)
    }

    fn status(&self) -> Option<ProviderStatus> {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn launching(&self) -> bool {
        self.admission().launches != 0
    }

    fn publish(&self, status: &ProviderStatus) {
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = Some(status.clone());
    }
}

impl Controller {
    pub(super) fn of(host: &SharedHost) -> Arc<Self> {
        Arc::clone(
            &host
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .provider_updates,
        )
    }

    fn update(&self, provider: Provider) -> &Arc<ProviderUpdate> {
        &self.providers[provider as usize]
    }

    pub(super) fn launch(&self, provider: Provider) -> Result<Permit, ProtocolError> {
        self.update(provider)
            .launch(LAUNCH_PATIENCE)
            .ok_or_else(|| {
                ProtocolError::new(
                    ProtocolErrorCode::TerminalUnavailable,
                    format!(
                        "{} is installing an update. Try again shortly.",
                        provider.label()
                    ),
                    true,
                )
            })
    }
}

fn internal(error: impl ToString) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, error.to_string(), true)
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

pub(super) fn status(host: &SharedHost) -> Result<Vec<ProviderStatus>, ProtocolError> {
    let updates = Controller::of(host);
    runtime().map_err(internal)?.block_on(async {
        let mut tasks = tokio::task::JoinSet::new();
        let search_path = probe::search_path().await;
        for provider in Provider::ALL {
            let search_path = search_path.clone();
            let updates = Arc::clone(&updates);
            tasks.spawn(async move {
                observe(updates.update(provider), provider, search_path.as_deref()).await
            });
        }
        let mut statuses = Vec::with_capacity(Provider::ALL.len());
        while let Some(status) = tasks.join_next().await {
            statuses.push(status.map_err(internal)?);
        }
        statuses.sort_by_key(|status| status.provider);
        Ok(statuses)
    })
}

async fn observe(
    slot: &Arc<ProviderUpdate>,
    provider: Provider,
    search_path: Option<&OsStr>,
) -> ProviderStatus {
    observe_installation(slot, async {
        let mut status = inspect(ProviderStatus::detect(provider, search_path), search_path).await;
        if let Some(error) = install::recovery_issue(&status, search_path) {
            status.error = Some(error);
        }
        status
    })
    .await
}

async fn observe_installation(
    slot: &ProviderUpdate,
    inspect: impl Future<Output = ProviderStatus>,
) -> ProviderStatus {
    if let Some(generation) = slot.idle_generation() {
        let detected = inspect.await;
        let history = slot.status();
        if slot.idle_generation() == Some(generation) {
            return with_history(detected, history);
        }
    }
    // A mutation publishes its baseline first. Discard observations that
    // could have seen an intermediate installation, even if the swap ended.
    slot.status().expect("provider update baseline")
}

pub(super) fn update(
    host: &SharedHost,
    provider: Option<Provider>,
) -> Result<Vec<ProviderStatus>, ProtocolError> {
    let providers = provider.as_slice();
    let providers = if providers.is_empty() {
        &Provider::ALL
    } else {
        providers
    };
    Ok(runtime().map_err(internal)?.block_on(async {
        let search_path = probe::refresh_search_path().await;
        check_providers(host, providers, search_path.as_deref(), false).await
    }))
}

async fn check_providers(
    host: &SharedHost,
    providers: &[Provider],
    search_path: Option<&OsStr>,
    automatic: bool,
) -> Vec<ProviderStatus> {
    let updates = Controller::of(host);
    let mut statuses = Vec::with_capacity(providers.len());
    for &provider in providers {
        statuses.push(
            check(
                host,
                updates.update(provider),
                provider,
                search_path,
                automatic,
            )
            .await,
        );
    }
    statuses
}

async fn check(
    host: &SharedHost,
    slot: &Arc<ProviderUpdate>,
    provider: Provider,
    search_path: Option<&OsStr>,
    automatic: bool,
) -> ProviderStatus {
    let Some(_checking) = slot.try_enter(Operation::Check) else {
        let mut status = slot
            .status()
            .unwrap_or_else(|| ProviderStatus::detect(provider, search_path));
        finish(
            &mut status,
            ProviderUpdateResult::Deferred,
            Some("A check is already running".into()),
        );
        return status;
    };
    // An interrupted swap can hide the executable entirely. Recover it before
    // probing the version or deciding whether the installation is updatable.
    let previous = slot.status();
    let mut baseline = ProviderStatus::detect(provider, search_path);
    if let Some(issue) = install::recovery_issue(&baseline, search_path) {
        baseline = with_history(baseline, previous.clone());
        baseline.error = Some(issue.clone());
        slot.publish(&baseline);
        let control = Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).control_gate);
        let failure = match admit_apply(&control, host, slot, provider) {
            Ok((_control, _applying)) => {
                install::recover(&baseline, search_path).err().map(|error| {
                    let result = if error.deferred {
                        ProviderUpdateResult::Deferred
                    } else {
                        ProviderUpdateResult::Failed
                    };
                    (result, error.reason)
                })
            }
            Err(reason) => Some((ProviderUpdateResult::Deferred, format!("{issue}. {reason}"))),
        };
        if let Some((result, error)) = failure {
            finish(&mut baseline, result, Some(error));
            slot.publish(&baseline);
            return baseline;
        }
        baseline = ProviderStatus::detect(provider, search_path);
    }
    let detected = tokio::select! {
        detected = inspect(baseline, search_path) => detected,
        () = wait_for_shutdown(host) => {
            let mut status = slot.status().unwrap_or_else(|| ProviderStatus::detect(provider, search_path));
            finish(&mut status, ProviderUpdateResult::Deferred, Some("The daemon is shutting down".into()));
            return status;
        }
    };
    let mut status = with_history(detected, previous);
    // A status request can report the previous result even while a mutation
    // is changing the installation beneath it.
    slot.publish(&status);

    let (result, error) = perform_update(host, slot, &mut status, search_path, automatic).await;
    finish(&mut status, result, error);
    slot.publish(&status);
    status
}

fn with_history(mut status: ProviderStatus, previous: Option<ProviderStatus>) -> ProviderStatus {
    if let Some(previous) = previous {
        status.last_checked_at = previous.last_checked_at;
        status.last_result = previous.last_result;
        status.error = status.error.or(previous.error);
    }
    status
}

fn finish(status: &mut ProviderStatus, result: ProviderUpdateResult, error: Option<String>) {
    status.last_checked_at = crate::chat::now_ms();
    status.last_result = Some(result);
    status.error = error;
}

async fn perform_update(
    host: &SharedHost,
    slot: &Arc<ProviderUpdate>,
    status: &mut ProviderStatus,
    search_path: Option<&OsStr>,
    automatic: bool,
) -> (ProviderUpdateResult, Option<String>) {
    use ProviderUpdateResult::{Current, Deferred, Failed, Skipped, Updated};

    if status.version.is_none()
        && matches!(
            status.install_method,
            ProviderInstallMethod::Native
                | ProviderInstallMethod::Npm { .. }
                | ProviderInstallMethod::Standalone
        )
    {
        return (Failed, status.error.clone());
    }
    if daemon_stopping(host) {
        return (Deferred, Some("The daemon is shutting down".into()));
    }
    if automatic && !automatic_updates_enabled() {
        return (Deferred, Some("Automatic updates are turned off".into()));
    }
    if slot.launching()
        || super::chat::provider_startup_pending(
            &host.lock().unwrap_or_else(|p| p.into_inner()),
            status.provider,
        )
    {
        return (Deferred, Some("A provider launch is starting".into()));
    }
    let preparation = tokio::select! {
        preparation = install::prepare(status, search_path, automatic) => preparation,
        () = wait_for_shutdown(host) => return (Deferred, Some("The daemon is shutting down".into())),
    };
    let prepared = match preparation {
        Ok(install::Preparation::Current(version)) => {
            status.version = Some(version);
            return (Current, None);
        }
        Ok(install::Preparation::Skipped(reason)) => return (Skipped, Some(reason)),
        Ok(install::Preparation::Deferred(reason)) => return (Deferred, Some(reason)),
        Ok(install::Preparation::Ready(prepared)) => prepared,
        Err(error) => return (Failed, Some(error)),
    };
    // A settings change during staging takes effect before publication.
    if automatic && !automatic_updates_enabled() {
        return (Deferred, Some("Automatic updates are turned off".into()));
    }
    match prepared.opt_out(automatic) {
        Ok(Some(reason)) => return (Skipped, Some(reason)),
        Ok(None) => {}
        Err(error) => return (Failed, Some(error)),
    }
    let (applied, prepared) = {
        let control = Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).control_gate);
        let (_control, _applying) = match admit_apply(&control, host, slot, status.provider) {
            Ok(admission) => admission,
            Err(reason) => return (Deferred, Some(reason.into())),
        };
        // Drop the transaction before the launch fence if this task unwinds.
        let mut prepared = prepared;
        let applied = prepared.apply(search_path).await;
        (applied, prepared)
    };
    let version = match applied {
        Ok(install::Applied::Updated(version)) => version,
        Ok(install::Applied::Deferred(reason)) => return (Deferred, Some(reason)),
        Err(error) => return (Failed, Some(error)),
    };
    prepared.retire_previous();
    drop(prepared);
    let new_install = ProviderStatus::detect(status.provider, search_path);
    status.version = Some(version);
    status.executable = new_install.executable;
    status.resolved_executable = new_install.resolved_executable;
    status.install_method = new_install.install_method;
    let refresh = tokio::select! {
        result = refresh_capabilities(host, status, search_path) => result,
        () = wait_for_shutdown(host) => Err("The daemon stopped before refreshing models".into()),
    };
    let error = refresh
        .err()
        .map(|error| format!("Update succeeded, but the model catalog refresh failed: {error}"));
    (Updated, error)
}

fn admit_apply<'a>(
    control: &'a ControlGate,
    host: &SharedHost,
    slot: &Arc<ProviderUpdate>,
    provider: Provider,
) -> Result<(Shared<'a>, Permit), &'static str> {
    let control = control.try_shared().ok_or("The daemon is busy")?;
    let applying = slot
        .try_enter(Operation::Apply)
        .ok_or("A provider launch is starting")?;
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.shutting_down || state.startup_recovery().is_some() {
        return Err("The daemon is shutting down or recovering sessions");
    }
    if super::chat::provider_startup_pending(&state, provider) {
        return Err("A provider launch is starting");
    }
    Ok((control, applying))
}

async fn refresh_capabilities(
    host: &SharedHost,
    status: &ProviderStatus,
    search_path: Option<&OsStr>,
) -> Result<(), String> {
    let account = Store::installation()
        .ok_or("Accounts are unavailable")?
        .load()
        .map_err(|error| error.to_string())?
        .default_account(status.provider, Homes::installation().as_ref());
    super::account_lifecycle::ensure_paths(host, account.resolved_home())
        .map_err(|error| error.message)?;
    let cache = Cache::installation().ok_or("The model catalog cache is unavailable")?;
    let program = status
        .executable
        .as_deref()
        .ok_or("The updated provider could not be located")?;
    probe::refresh_catalog(&account, program, search_path, &cache).await
}

fn daemon_stopping(host: &SharedHost) -> bool {
    crate::daemon::termination_requested()
        || host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down
}

async fn wait_for_shutdown(host: &SharedHost) {
    while !daemon_stopping(host) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) struct Scheduler {
    stop: crossbeam_channel::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Start after the daemon has restored its sessions. Recovering originals defer
/// mutation, and the later passes still run if that recovery hold is cleared.
pub(super) fn start(host: Weak<HostLock>) -> std::io::Result<Option<Scheduler>> {
    if cfg!(test) || !automatic_updates_allowed() {
        return Ok(None);
    }
    let Some(settings_path) = crate::instance::settings_path() else {
        return Ok(None);
    };
    let runtime = runtime()?;
    let (stop, stopped) = crossbeam_channel::bounded(1);
    let worker = thread::Builder::new()
        .name("cagents-provider-update".into())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            // SAFETY: both calls change only the current Linux thread's
            // priorities. Its child workers inherit these priorities.
            unsafe {
                libc::nice(10);
                libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13);
            }
            let mut preferences = preferences::SettingsWatch::new(settings_path);
            let mut pending = Vec::new();
            let mut next_check = Instant::now();
            let mut next_full_check = next_check;
            while let Some(enabled) = preferences.wait_for_check(&stopped, next_check) {
                let Some(host) = host.upgrade() else {
                    break;
                };
                if daemon_stopping(&host) {
                    break;
                }
                let now = Instant::now();
                if !enabled {
                    pending.clear();
                    next_full_check = now;
                    next_check = now + INTERVAL;
                    continue;
                }
                if now >= next_full_check {
                    pending = Provider::ALL.into();
                    next_full_check = now + INTERVAL;
                }
                let statuses = service::contain("provider update check", || {
                    runtime.block_on(async {
                        let search_path = probe::search_path().await;
                        check_providers(&host, &pending, search_path.as_deref(), true).await
                    })
                });
                pending = match statuses {
                    Ok(statuses) => statuses
                        .into_iter()
                        .filter(|status| status.last_result == Some(ProviderUpdateResult::Deferred))
                        .map(|status| status.provider)
                        .collect(),
                    Err(error) => {
                        tracing::warn!(%error, "the provider update check will be retried");
                        next_check = Instant::now() + DEFERRED_RETRY;
                        continue;
                    }
                };
                next_check = if pending.is_empty() {
                    next_full_check
                } else {
                    next_full_check.min(Instant::now() + DEFERRED_RETRY)
                };
            }
        })?;
    Ok(Some(Scheduler {
        stop,
        worker: Some(worker),
    }))
}

fn automatic_updates_allowed() -> bool {
    // These runners explicitly provide synthetic installs and must stay offline.
    if std::env::var_os("CAGENTS_EVIDENCE_FIXTURE").is_some()
        || std::env::var_os("CAGENTS_REHEARSAL_ROOT").is_some()
    {
        return false;
    }
    !crate::instance::IS_DEVELOPMENT
        || std::env::var_os("CAGENTS_DEVELOPMENT_PROVIDER_UPDATES").as_deref()
            == Some(OsStr::new("1"))
}

fn automatic_updates_enabled() -> bool {
    automatic_updates_allowed()
        && crate::instance::settings_path().is_some_and(|path| preferences::enabled(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claude_download_allows_provider_launch_and_terminal_close() {
        use std::{
            fs,
            io::ErrorKind,
            os::unix::{
                fs::{DirBuilderExt, PermissionsExt, symlink},
                net::UnixListener,
            },
            path::Path,
            process::Command,
        };

        use crate::terminal_ipc::{ClientRequest, ServerResponse, TerminalCloseResult};

        use super::super::client::{process_request, tests::retention_client_host};

        const HOME_FIXTURE: &str = "CAGENTS_TEST_CLAUDE_DOWNLOAD_HOME";
        let Some(home) = std::env::var_os(HOME_FIXTURE) else {
            // Isolate native install discovery without changing this test
            // binary's HOME or the account settings other tests rely on.
            let home = tempfile::Builder::new().prefix("cu-").tempdir().unwrap();
            let runtime = home.path().join("runtime");
            fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host::provider_update::tests::a_claude_download_allows_provider_launch_and_terminal_close", "--nocapture"])
                .env(HOME_FIXTURE, home.path())
                .env("HOME", home.path())
                .env("XDG_DATA_HOME", home.path().join(".local/share"))
                .env("XDG_STATE_HOME", home.path().join(".local/state"))
                .env("XDG_CACHE_HOME", home.path().join(".cache"))
                .env("XDG_CONFIG_HOME", home.path().join(".config"))
                .env("XDG_RUNTIME_DIR", &runtime)
                .env_remove("CLAUDE_CONFIG_DIR")
                .env_remove("DISABLE_UPDATES")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        };
        let home = Path::new(&home);
        let bin = home.join(".local/bin");
        let versions = home.join(".local/share/claude/versions");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&versions).unwrap();
        let launcher = bin.join("claude");
        let installed = versions.join("1.0.0");
        let socket = home.join("download.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let executable = |path: &Path, script: &str| {
            fs::write(path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        };
        executable(&bin.join("curl"), "#!/bin/sh\nprintf '2.0.0\\n'\n");
        executable(
            &installed,
            &format!(
                r#"#!/bin/sh
case "$1" in
    --version) printf '1.0.0 (Claude Code)\n' ;;
    --fixture-session) exit 0 ;;
    install|update)
        /usr/bin/python3 - '{}' <<'PY'
import socket
import sys
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(30)
    connection.connect(sys.argv[1])
    connection.recv(1)
PY
        exit 42
        ;;
    *) exit 2 ;;
esac
"#,
                socket.display()
            ),
        );
        symlink(&installed, &launcher).unwrap();
        let search_path =
            std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap();
        let (host, _work, terminal, _handle) = retention_client_host("claude-update");
        let worker = {
            let host = Arc::clone(&host);
            thread::spawn(move || {
                runtime().unwrap().block_on(check_providers(
                    &host,
                    &[Provider::Claude],
                    Some(&search_path),
                    false,
                ))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if worker.is_finished() {
                        panic!(
                            "Claude's download did not start: {:?}",
                            worker.join().unwrap()
                        );
                    }
                    assert!(Instant::now() < deadline, "Claude's download did not start");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("Could not accept fixture installer connection: {error}"),
            }
        };

        let mut claude = terminal.clone();
        claude.id = crate::model::TerminalId::new();
        claude.launch.program = launcher.to_str().unwrap().into();
        claude.launch.arguments = vec!["--fixture-session".into()];
        claude.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
            recipe: claude.launch.clone(),
            last_exit: None,
        };
        let launched = process_request(
            &host,
            1,
            ClientRequest::Spawn {
                session: claude,
                options: Default::default(),
            },
        );
        let closed = process_request(
            &host,
            1,
            ClientRequest::CloseTerminalChecked {
                terminal_id: terminal.id,
                allow_active: false,
            },
        );
        assert!(
            !worker.is_finished(),
            "Claude finished before both operations"
        );
        drop(connection);
        let status = worker.join().unwrap();
        assert!(matches!(
            launched.unwrap().0,
            ServerResponse::Spawned { .. }
        ));
        assert!(matches!(
            closed.unwrap().0,
            ServerResponse::TerminalCloseChecked {
                outcome: TerminalCloseResult::Closed,
                ..
            }
        ));
        assert_eq!(status[0].last_result, Some(ProviderUpdateResult::Failed));
        assert!(status[0].error.as_ref().unwrap().contains("42"));
    }

    #[test]
    fn automatic_updates_default_on_and_preserve_an_explicit_opt_out() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        assert!(crate::settings::provider_update_preference(&path).unwrap());
        crate::settings::save(
            &path,
            &crate::settings::AppSettings {
                update_providers_automatically: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!crate::settings::provider_update_preference(&path).unwrap());
        assert!(
            !crate::settings::load(&path)
                .unwrap()
                .update_providers_automatically
        );
    }

    #[tokio::test]
    async fn a_post_update_probe_replaces_the_cached_models() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("omp");
        fs::write(
            &program,
            "#!/bin/sh\n[ \"$*\" = 'models --json --no-extensions' ] || exit 2\ncat <<'END'\n{\"models\":[{\"provider\":\"fake\",\"id\":\"new\",\"name\":\"New model\",\"reasoning\":true,\"thinking\":[\"high\"]}]}\nEND\n",
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let cache = Cache::at(directory.path().join("cache"));
        cache
            .store_catalog(
                Provider::Omp,
                crate::account::cache::Catalog {
                    models: vec![crate::chat::Model {
                        id: "fake/old".into(),
                        label: "Old model".into(),
                        reasoning: Vec::new(),
                    }],
                    observed_at: 1,
                    default_model: Some("fake/old".into()),
                    reasoning_defaults: Default::default(),
                },
            )
            .unwrap();
        let account = crate::account::Account::system(Provider::Omp);
        probe::refresh_catalog(
            &account,
            &program,
            Some(OsStr::new("/usr/bin:/bin")),
            &cache,
        )
        .await
        .unwrap();
        let catalogs = cache.catalogs(crate::chat::now_ms().unwrap());
        let catalog = &catalogs[&Provider::Omp];
        assert_eq!(catalog.models[0].id, "fake/new");
        assert_eq!(catalog.models[0].reasoning, ["off", "auto", "high"]);
        assert!(catalog.default_model.is_none());
    }

    #[tokio::test]
    async fn live_status_refreshes_while_preserving_the_last_check() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("omp");
        fs::write(
            &program,
            "#!/bin/sh\n[ \"$1\" = --version ] || exit 2\necho 2.0.0\n",
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().as_os_str();
        let mut old = ProviderStatus::detect(Provider::Omp, Some(path));
        old.version = Some("1.0.0".into());
        old.last_checked_at = Some(42);
        old.last_result = Some(ProviderUpdateResult::Current);
        let updates = Controller::default();
        let slot = updates.update(Provider::Omp);
        slot.publish(&old);

        {
            let _applying = slot.try_enter(Operation::Apply).unwrap();
            assert_eq!(observe(slot, Provider::Omp, Some(path)).await, old);
        }
        let current = observe(slot, Provider::Omp, Some(path)).await;
        assert_eq!(current.version.as_deref(), Some("2.0.0"));
        assert_eq!(current.last_checked_at, Some(42));
        assert_eq!(current.last_result, Some(ProviderUpdateResult::Current));
    }

    #[tokio::test]
    async fn a_status_probe_allows_apply_and_discards_an_overlapping_observation() {
        let updates = Controller::default();
        let slot = updates.update(Provider::Omp);
        let mut baseline = ProviderStatus::detect(Provider::Omp, Some(OsStr::new("/")));
        baseline.version = Some("1.0.0".into());
        slot.publish(&baseline);
        let (started, probe_started) = tokio::sync::oneshot::channel();
        let (swapped, probe_resume) = tokio::sync::oneshot::channel();
        let observing = observe_installation(slot, async {
            started.send(()).unwrap();
            probe_resume.await.unwrap();
            let mut intermediate = baseline.clone();
            intermediate.version = None;
            intermediate
        });
        let applying = async {
            probe_started.await.unwrap();
            assert!(!slot.launching());
            let permit = slot.try_enter(Operation::Apply).unwrap();
            drop(permit);
            swapped.send(()).unwrap();
        };
        let (status, ()) = tokio::join!(observing, applying);
        assert_eq!(status, baseline);
    }

    #[tokio::test]
    async fn status_reports_an_unrepairable_stage_without_an_update_check() {
        use std::{fs, os::unix::fs::DirBuilderExt};

        let directory = tempfile::tempdir().unwrap();
        let scope = directory.path().join("lib/node_modules/@openai");
        fs::create_dir_all(&scope).unwrap();
        let stage = scope.join(".cagents-provider-update-legacy");
        fs::DirBuilder::new().mode(0o700).create(&stage).unwrap();
        fs::write(stage.join(".cagents-stage"), []).unwrap();
        fs::write(stage.join(".cagents-update-incomplete"), []).unwrap();
        fs::create_dir(stage.join("codex")).unwrap();
        // Codex's npm launcher points to the scope that contains its stage,
        // even after the underlying installation has been moved away.
        std::os::unix::fs::symlink(
            scope.join("codex/bin/codex.js"),
            directory.path().join("codex"),
        )
        .unwrap();

        let updates = Controller::default();
        let status = observe(
            updates.update(Provider::Codex),
            Provider::Codex,
            Some(directory.path().as_os_str()),
        )
        .await;
        let error = status.error.unwrap();
        assert!(error.contains(&*stage.to_string_lossy()), "{error}");
        assert!(error.contains("no rollback journal"), "{error}");
    }

    #[test]
    fn updates_stage_during_launches_and_apply_after_they_finish() {
        let updates = Controller::default();
        let codex = updates.update(Provider::Codex);
        let first = updates.launch(Provider::Codex).unwrap();
        let check = codex.try_enter(Operation::Check).unwrap();
        assert!(codex.try_enter(Operation::Check).is_none());
        assert!(codex.try_enter(Operation::Apply).is_none());
        let second = updates.launch(Provider::Codex).unwrap();
        drop(first);
        assert!(codex.try_enter(Operation::Apply).is_none());
        drop(second);

        let applied = codex.try_enter(Operation::Apply).unwrap();
        assert!(codex.launch(Duration::ZERO).is_none());
        assert!(updates.launch(Provider::Claude).is_ok());
        drop(applied);
        drop(check);
        assert!(updates.launch(Provider::Codex).is_ok());
        assert!(codex.try_enter(Operation::Check).is_some());
    }

    #[test]
    fn a_launch_waits_for_the_swap_then_acquires_its_permit() {
        let updates = Controller::default();
        let slot = updates.update(Provider::Omp);
        let applying = slot.try_enter(Operation::Apply).unwrap();
        let (started, starting) = crossbeam_channel::bounded(1);
        let (publish, launched) = crossbeam_channel::bounded(1);
        thread::scope(|scope| {
            scope.spawn(|| {
                started.send(()).unwrap();
                publish.send(updates.launch(Provider::Omp)).unwrap();
            });
            starting.recv().unwrap();
            assert!(matches!(
                launched.recv_timeout(Duration::from_millis(50)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ));
            drop(applying);
            let permit = launched
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(slot.try_enter(Operation::Apply).is_none());
            drop(permit);
            assert!(slot.try_enter(Operation::Apply).is_some());
        });
    }
}
