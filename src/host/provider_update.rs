//! Serializes provider installation changes with the beginnings of launches.

mod install;

use std::{
    ffi::OsStr,
    sync::{Arc, Mutex, Weak},
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
    provider_status::inspect,
    service,
    state::{HostLock, SharedHost},
};

const INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const DEFERRED_RETRY: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
pub(super) struct Controller {
    providers: [Arc<ProviderUpdate>; Provider::ALL.len()],
}

#[derive(Default)]
struct ProviderUpdate {
    admission: Mutex<Admission>,
    status: Mutex<Option<ProviderStatus>>,
}

#[derive(Default)]
struct Admission {
    launches: usize,
    checking: bool,
    applying: bool,
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
        let mut state = self
            .update
            .admission
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match self.operation {
            Operation::Launch => state.launches -= 1,
            Operation::Check => state.checking = false,
            Operation::Apply => state.applying = false,
        }
    }
}

impl ProviderUpdate {
    fn try_enter(self: &Arc<Self>, operation: Operation) -> Option<Permit> {
        let mut state = self.admission.lock().unwrap_or_else(|p| p.into_inner());
        match operation {
            Operation::Launch if !state.applying => state.launches += 1,
            Operation::Check if !state.checking => state.checking = true,
            Operation::Apply if !state.applying && state.launches == 0 => state.applying = true,
            _ => return None,
        }
        Some(Permit {
            update: Arc::clone(self),
            operation,
        })
    }

    fn status(&self) -> Option<ProviderStatus> {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn launching(&self) -> bool {
        self.admission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .launches
            != 0
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
            .try_enter(Operation::Launch)
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
    if let Some(_observing) = slot.try_enter(Operation::Launch) {
        with_history(inspect(provider, search_path).await, slot.status())
    } else {
        // A mutation always publishes its verified baseline first.
        slot.status().expect("provider update baseline")
    }
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
    let detected = tokio::select! {
        detected = inspect(provider, search_path) => detected,
        () = wait_for_shutdown(host) => {
            let mut status = slot.status().unwrap_or_else(|| ProviderStatus::detect(provider, search_path));
            finish(&mut status, ProviderUpdateResult::Deferred, Some("The daemon is shutting down".into()));
            return status;
        }
    };
    let mut status = with_history(detected, slot.status());
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
        return (Skipped, Some("Automatic updates are turned off".into()));
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
        preparation = install::prepare(status, search_path) => preparation,
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
        return (Skipped, Some("Automatic updates are turned off".into()));
    }
    let applied = {
        let control = Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).control_gate);
        let Some(_control) = control.try_shared() else {
            return (Deferred, Some("The daemon is busy".into()));
        };
        let Some(_applying) = slot.try_enter(Operation::Apply) else {
            return (Deferred, Some("A provider launch is starting".into()));
        };
        {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.shutting_down || state.startup_recovery().is_some() {
                return (
                    Deferred,
                    Some("The daemon is shutting down or recovering sessions".into()),
                );
            }
            if super::chat::provider_startup_pending(&state, status.provider) {
                return (Deferred, Some("A provider launch is starting".into()));
            }
        }
        prepared
            .apply(search_path, || {
                crate::daemon::termination_requested() || control.exclusive_queued()
            })
            .await
    };
    let version = match applied {
        Ok(install::Applied::Current(version)) => {
            status.version = Some(version);
            return (Current, None);
        }
        Ok(install::Applied::Updated(version)) => version,
        Ok(install::Applied::Deferred(reason)) => return (Deferred, Some(reason)),
        Err(error) => return (Failed, Some(error)),
    };
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
    if cfg!(test) {
        return Ok(None);
    }
    let runtime = runtime()?;
    let mut pending = Vec::new();
    let mut wait = Duration::ZERO;
    let mut next_full_check = Instant::now();
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
            while matches!(
                stopped.recv_timeout(wait),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ) {
                let Some(host) = host.upgrade() else {
                    break;
                };
                if daemon_stopping(&host) {
                    break;
                }
                let now = Instant::now();
                if !automatic_updates_enabled() {
                    pending.clear();
                    next_full_check = now + INTERVAL;
                    wait = INTERVAL;
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
                        wait = DEFERRED_RETRY;
                        continue;
                    }
                };
                wait = next_full_check.saturating_duration_since(Instant::now());
                if !pending.is_empty() {
                    wait = wait.min(DEFERRED_RETRY);
                }
            }
        })?;
    Ok(Some(Scheduler {
        stop,
        worker: Some(worker),
    }))
}

fn automatic_updates_enabled() -> bool {
    // These runners explicitly provide synthetic installs and must stay offline.
    if std::env::var_os("CAGENTS_EVIDENCE_FIXTURE").is_some()
        || std::env::var_os("CAGENTS_REHEARSAL_ROOT").is_some()
    {
        return false;
    }
    if crate::instance::IS_DEVELOPMENT
        && std::env::var_os("CAGENTS_DEVELOPMENT_PROVIDER_UPDATES").as_deref()
            != Some(OsStr::new("1"))
    {
        return false;
    }
    let preference = crate::settings::default_path()
        .and_then(|path| crate::settings::provider_update_preference(&path));
    preference.unwrap_or_else(|error| {
        tracing::warn!(%error, "automatic provider updates could not read settings");
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn shutting_down_reaps_the_update_command_before_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let started = directory.path().join("started");
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(r#"printf '%s' "$$" > "$1"; exec /bin/sleep 30"#)
            .arg("fixture")
            .arg(&started)
            .current_dir(directory.path());
        let result =
            probe::run_provider_command_cancellable(command, Duration::from_secs(5), || {
                std::fs::metadata(&started).is_ok_and(|file| file.len() != 0)
            })
            .await;
        assert_eq!(
            result.unwrap_err(),
            "The daemon interrupted the provider update"
        );
        let pid = std::fs::read_to_string(started).unwrap();
        assert!(!std::path::Path::new("/proc").join(pid).exists());
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
        assert!(updates.launch(Provider::Codex).err().unwrap().retryable);
        assert!(updates.launch(Provider::Claude).is_ok());
        drop(applied);
        drop(check);
        assert!(updates.launch(Provider::Codex).is_ok());
        assert!(codex.try_enter(Operation::Check).is_some());
    }
}
