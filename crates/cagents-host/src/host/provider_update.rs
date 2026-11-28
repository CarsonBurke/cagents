//! Updates only installs enrolled into this instance's versioned storage.

mod install;
mod policy;
mod release;

use std::{
    ffi::OsStr,
    io,
    path::Path,
    sync::{Arc, Weak},
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    account::{
        cache::Cache,
        home::Homes,
        probe,
        provider_install::{ProviderStatus, ProviderUpdateResult, read_version},
        registry::Store,
    },
    agent::Provider,
    terminal_ipc::{ClientRequest, ProtocolError, ProtocolErrorCode},
};

use super::state::{HostLock, SharedHost};
use install::Install;
use policy::Policy;
use release::{Source, parse_version};

const INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Manage,
    Update,
    Scheduled,
}

impl Operation {
    fn schedules(self) -> bool {
        matches!(self, Self::Scheduled)
    }
}

type Report = (ProviderUpdateResult, Option<String>);

fn runtime() -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

pub(super) fn request(
    host: &SharedHost,
    provider: Provider,
    operation: Operation,
) -> Result<Vec<ProviderStatus>, ProtocolError> {
    let runtime = runtime().map_err(|error| {
        ProtocolError::new(ProtocolErrorCode::Internal, error.to_string(), false)
    })?;
    runtime.block_on(async {
        tokio::select! {
            status = run(host, provider, operation) => Ok(vec![status]),
            () = wait_for_close(host) => Err(ProtocolError::new(
                ProtocolErrorCode::TerminalUnavailable,
                "The daemon stopped the provider update check",
                true,
            )),
        }
    })
}

async fn wait_for_close(host: &SharedHost) {
    loop {
        if crate::daemon::termination_requested()
            || super::client::ensure_control_open(host).is_err()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn run(host: &SharedHost, provider: Provider, operation: Operation) -> ProviderStatus {
    let path = probe::refresh_search_path().await;
    let path = path.as_deref();
    let mut status = super::provider_status::inspect(provider, path).await;
    let Some(install) = Install::discover().filter(|_| provider == Provider::Claude) else {
        return finish(
            status,
            (
                ProviderUpdateResult::Skipped,
                Some("Use this installation's own update method".into()),
            ),
        );
    };
    if !matches!(operation, Operation::Manage) && !install.selected(status.executable.as_deref()) {
        return finish(
            status,
            (
                ProviderUpdateResult::Skipped,
                Some(
                    "The daemon's PATH does not select its managed Claude Code installation".into(),
                ),
            ),
        );
    }
    let _lock = match install.lock() {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return finish(
                status,
                (
                    ProviderUpdateResult::Deferred,
                    Some("A Claude Code update check is already running".into()),
                ),
            );
        }
        Err(error) => return finish(status, failure(error)),
    };
    let result = update(host, &install, path, operation, &mut status)
        .await
        .unwrap_or_else(failure);
    status = finish(status, result);
    if let Err(error) = install.save(&status) {
        tracing::warn!(%error, "could not record last Claude Code update check");
    }
    status
}

async fn update(
    host: &SharedHost,
    install: &Install,
    path: Option<&OsStr>,
    operation: Operation,
    status: &mut ProviderStatus,
) -> Result<Report, String> {
    use ProviderUpdateResult::{Current, Deferred, Skipped, Updated};

    let manage = matches!(operation, Operation::Manage);
    let prior = install.active().map_err(detail)?;
    let present = match install.validate_launcher() {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound && manage => false,
        Err(error) => return Err(detail(error)),
    };
    if let Some(binary) = prior.as_ref().filter(|_| present) {
        *status = install.status(binary.clone());
        let reported = read_version(Provider::Claude, binary, path).await?;
        verify_path_version(binary, &reported)?;
        if manage {
            let detail = ensure_catalog(host, install, path, &reported).await;
            status.version = Some(reported);
            return Ok((Current, detail));
        }
        status.version = Some(reported);
    } else if !manage {
        return Err("The managed Claude Code launcher has no verified selection".into());
    }

    let policy = Policy::read()?;
    let channel = match policy.channel(operation.schedules()) {
        Ok(channel) => channel,
        Err(message) => return Ok((Skipped, Some(message.into()))),
    };
    let stage = install.stage().map_err(detail)?;
    let source = Source::discover(path)?;
    let version = source.version(channel, stage.path()).await?;
    if let Some(installed) = &status.version
        && present
    {
        match version.cmp(&parse_version(installed)?) {
            std::cmp::Ordering::Less => {
                return Ok((
                    Skipped,
                    Some(format!(
                        "The Claude Code {channel} channel selected older release {version}"
                    )),
                ));
            }
            std::cmp::Ordering::Equal => {
                let detail = ensure_catalog(host, install, path, installed).await;
                return Ok((Current, detail));
            }
            std::cmp::Ordering::Greater => {}
        }
    }
    if !policy.allows(&version) {
        return Ok((
            Skipped,
            Some(format!("Claude Code version settings exclude {version}")),
        ));
    }

    let staged = source.stage(&version, stage.path()).await?;
    let reported = read_version(Provider::Claude, &staged, path).await?;
    if reported != version.to_string() {
        return Err(format!("Claude Code {version} reported version {reported}"));
    }
    let binary = install
        .materialize(stage.path(), &staged, &version)
        .map_err(detail)?;

    // Policy and launch selection may change while downloading. The only
    // operation under the daemon's admission fence is publishing the link.
    let refreshed = probe::refresh_search_path().await;
    let path = refreshed.as_deref();
    if Policy::read()? != policy || (operation.schedules() && !automatic_enabled()) {
        return Ok((
            Deferred,
            Some("Claude Code update settings changed during the check".into()),
        ));
    }
    if !manage && !install.selected(probe::find_executable("claude", path).as_deref()) {
        return Ok((
            Deferred,
            Some("The daemon's Claude Code launcher changed during the check".into()),
        ));
    }
    activate(host, install, stage.path(), &binary, operation)?;

    *status = install.status(binary);
    let detail = ensure_catalog(host, install, path, &reported).await;
    status.version = Some(reported);
    Ok((Updated, detail))
}

fn verify_path_version(binary: &Path, reported: &str) -> Result<(), String> {
    let expected = binary
        .parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str);
    if expected != Some(reported) {
        return Err(format!(
            "The Claude Code binary reported {reported} instead of {}",
            expected.unwrap_or("its installed version")
        ));
    }
    Ok(())
}

fn activate(
    host: &SharedHost,
    install: &Install,
    stage: &Path,
    binary: &Path,
    operation: Operation,
) -> Result<(), String> {
    let gate = Arc::clone(&host.lock().unwrap_or_else(|p| p.into_inner()).control_gate);
    let _shared = gate.shared();
    super::client::ensure_control_open(host).map_err(|error| error.message)?;
    let request = match operation {
        Operation::Manage => ClientRequest::ProviderManage {
            provider: Provider::Claude,
        },
        Operation::Update | Operation::Scheduled => ClientRequest::ProviderUpdate {
            provider: Provider::Claude,
        },
    };
    super::recovery_admission::admit(host, &request).map_err(|error| error.message)?;
    install.activate(stage, binary).map_err(detail)
}

async fn refresh_catalog(
    host: &SharedHost,
    program: &Path,
    path: Option<&OsStr>,
) -> Result<(), String> {
    let account = Store::installation()
        .ok_or("Accounts are unavailable")?
        .load()
        .map_err(detail)?
        .default_account(Provider::Claude, Homes::installation().as_ref());
    super::account_lifecycle::ensure_paths(host, account.resolved_home())
        .map_err(|error| error.message)?;
    let cache = Cache::installation().ok_or("The model catalog cache is unavailable")?;
    probe::refresh_claude_catalog(&account, program, path, &cache).await
}

async fn ensure_catalog(
    host: &SharedHost,
    install: &Install,
    path: Option<&OsStr>,
    version: &str,
) -> Option<String> {
    let launcher = install.launcher();
    if !install.selected(probe::find_executable("claude", path).as_deref()) {
        return Some(format!(
            "Put {} first on your PATH to use the managed Claude Code",
            launcher.parent().unwrap().display()
        ));
    }
    if install.catalog_is_current(version) {
        return None;
    }
    let result = async {
        refresh_catalog(host, &launcher, path).await?;
        install.record_catalog_version(version).map_err(detail)
    }
    .await;
    result
        .err()
        .map(|error| format!("Could not refresh the Claude Code model catalog: {error}"))
}

fn finish(mut status: ProviderStatus, (result, message): Report) -> ProviderStatus {
    status.last_checked_at = crate::chat::now_ms();
    status.last_result = Some(result);
    status.error = message;
    status
}

fn failure(error: impl std::fmt::Display) -> Report {
    (ProviderUpdateResult::Failed, Some(detail(error)))
}

fn detail(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub(super) async fn inspect_managed(
    detected: &ProviderStatus,
    path: Option<&OsStr>,
) -> Option<ProviderStatus> {
    let install = Install::discover().filter(|install| {
        detected.provider == Provider::Claude && install.selected(detected.executable.as_deref())
    })?;
    let binary = (|| {
        install.validate_launcher().map_err(detail)?;
        install
            .active()
            .map_err(detail)?
            .ok_or_else(|| "The managed Claude Code launcher has no selected version".to_owned())
    })();
    let mut status = match binary {
        Ok(binary) => {
            let mut status = install.status(binary.clone());
            match read_version(Provider::Claude, &binary, path).await {
                Ok(version) => {
                    status.error = verify_path_version(&binary, &version).err();
                    status.version = Some(version);
                }
                Err(error) => status.error = Some(error),
            }
            status
        }
        Err(error) => {
            let mut status = detected.clone();
            status.install_method =
                crate::account::provider_install::ProviderInstallMethod::CagentsManaged;
            status.resolved_executable = None;
            status.error = Some(error);
            status
        }
    };
    if let Err(error) = install.history(&mut status) {
        tracing::warn!(%error, "could not read the last Claude Code update check");
    }
    Some(status)
}

fn automatic_enabled() -> bool {
    crate::instance::settings_path()
        .and_then(|path| crate::settings::load(path).ok())
        .is_some_and(|settings| settings.update_providers_automatically)
}

pub(super) struct Scheduler {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Scheduler {
    pub(super) fn start(host: Weak<HostLock>) -> io::Result<Self> {
        let runtime = runtime()?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let worker = thread::Builder::new()
            .name("cagents-provider-updates".into())
            .spawn(move || {
                runtime.block_on(async {
                    tokio::select! {
                        _ = stopped => {}
                        () = periodically(&host) => {}
                    }
                });
            })?;
        Ok(Self {
            stop: Some(stop),
            worker: Some(worker),
        })
    }
}

async fn periodically(host: &Weak<HostLock>) {
    loop {
        let Some(shared) = host.upgrade() else { return };
        if Install::discover().is_some_and(|install| install.launcher().is_file())
            && automatic_enabled()
        {
            tokio::select! {
                _ = run(&shared, Provider::Claude, Operation::Scheduled) => {}
                () = wait_for_close(&shared) => return,
            }
        }
        drop(shared);
        tokio::time::sleep(INTERVAL).await;
    }
}
