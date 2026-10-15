//! Automatic routing has an owner even when every window is unfocused or closed.
use super::state::SharedHost;
use crate::account::{Account, cache::Cache, home::Homes, probe, registry::Store};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

pub(super) const INTERVAL: u64 = 30 * 60_000;
const POLL: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Schedule {
    attempted: BTreeMap<crate::account::AccountId, u64>,
}
impl Schedule {
    fn due(&self, account: &Account, automatic: bool, hosted: bool, now: u64) -> bool {
        let jitter = account.id().as_str().bytes().fold(0u64, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(u64::from(byte))
        }) % 60_000;
        automatic
            && hosted
            && self
                .attempted
                .get(&account.id())
                .is_none_or(|last| now.saturating_sub(*last) >= INTERVAL + jitter)
    }
}

async fn cancellable_probe<F: std::future::Future>(
    signal: &Arc<(Mutex<bool>, Condvar)>,
    probe: F,
) -> Option<F::Output> {
    tokio::select! {
        result = probe => Some(result),
        _ = async {
            loop {
                if *signal.0.lock().unwrap_or_else(|p| p.into_inner()) || crate::daemon::termination_requested() { break }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        } => None,
    }
}

type HomeKey = (crate::agent::Provider, std::path::PathBuf, u64, u64);
#[derive(Clone, Default)]
struct HomeReports {
    events: VecDeque<(crate::account::LiveReport, u64)>,
    lost_login: bool,
}
#[derive(Clone, Default)]
struct Reports {
    homes: BTreeMap<HomeKey, HomeReports>,
    /// Whole-home eviction loses precise attribution. Remember at most the
    /// three providers so overflow still requires identity verification.
    lost_login_providers: std::collections::BTreeSet<crate::agent::Provider>,
}
impl Reports {
    fn is_empty(&self) -> bool { self.homes.is_empty() && self.lost_login_providers.is_empty() }
}
/// Bounded resident metadata only; provider callbacks never access disk or wait
/// for probes. Coalescing by account keeps a busy transcript from growing work.
pub(super) struct Signal {
    stopped: Arc<(Mutex<bool>, Condvar)>,
    pending: Mutex<Reports>,
    ready: AtomicBool,
}
impl Signal {
    fn new(stopped: Arc<(Mutex<bool>, Condvar)>) -> Self {
        Self {
            stopped,
            pending: Mutex::new(Reports::default()),
            ready: AtomicBool::new(false),
        }
    }
    pub(super) fn record(
        &self,
        scope: &crate::account::scope::LaunchAccountScope,
        report: crate::account::LiveReport,
        observed_at: u64,
    ) {
        if report.provider() != scope.provider {
            return;
        }
        let stopped = self.stopped.0.lock().unwrap_or_else(|p| p.into_inner());
        if *stopped {
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let key = (scope.provider, scope.canonical_home.clone(), scope.device, scope.inode);
        if pending.homes.len() >= 256 && !pending.homes.contains_key(&key) {
            if let Some(((provider, ..), dropped)) = pending.homes.pop_first()
                && (dropped.lost_login || dropped.events.iter().any(|(report, _)| matches!(report, crate::account::LiveReport::CodexLogin(_)))) {
                pending.lost_login_providers.insert(provider);
            }
        }
        let reports = pending.homes.entry(key).or_default();
        if reports.events.len() >= 32 {
            if reports.events.pop_front().is_some_and(|(report, _)| matches!(report, crate::account::LiveReport::CodexLogin(_))) {
                reports.lost_login = true;
            }
        }
        reports.events.push_back((report, observed_at));
        self.ready.store(true, Ordering::Release);
        self.stopped.1.notify_all();
        drop(stopped);
    }
    fn drain(&self) -> Reports {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        self.ready.store(false, Ordering::Release);
        std::mem::take(&mut *pending)
    }
}

/// Fold live reports into shared routing evidence even with no UI subscribers.
/// Login identity changes revoke the old allowance until a probe verifies it.
fn apply_reports(
    registry: &crate::account::registry::Registry,
    homes: Option<&Homes>,
    cache: &Cache,
    reports: Reports,
    now: u64,
) -> (
    BTreeMap<String, crate::account::cache::Entry>,
    Vec<Account>,
    Vec<Account>,
) {
    use crate::account::{LiveReport, UsageSnapshot, UsageStatus, cache::Entry, routing};
    let mut entries = cache.load(now);
    let mut exhausted = Vec::new();
    let mut changed_logins = Vec::new();
    for account in registry.accounts(homes) {
        if !registry.automatic(account.provider) {
            continue;
        }
        let Some(home) = account.resolved_home() else {
            continue;
        };
        // Filesystem identity is checked by the monitor, outside HostState.
        // A stale actor cannot attribute usage to a replacement login home.
        use std::os::unix::fs::MetadataExt;
        let Ok(canonical) = home.canonicalize() else { continue; };
        let Ok(metadata) = std::fs::metadata(&canonical) else { continue; };
        if !metadata.is_dir() { continue; }
        let home_reports = reports.homes.get(&(account.provider, canonical, metadata.dev(), metadata.ino()));
        let lost_login = reports.lost_login_providers.contains(&account.provider)
            || home_reports.is_some_and(|reports| reports.lost_login);
        if home_reports.is_none() && !lost_login { continue; }
        let Some(key) = account.cache_key() else {
            continue;
        };
        let entry = entries.entry(key.clone()).or_insert_with(|| Entry {
            identity: None,
            snapshot: UsageSnapshot::state(
                UsageStatus::Unsupported,
                "Awaiting account verification",
                0,
            ),
        });
        if lost_login {
            entry.snapshot = UsageSnapshot::state(UsageStatus::NeedsLogin,
                "Account updates exceeded the pending queue; verifying the current sign-in", now);
            let _ = cache.store(&key, entry.clone());
            changed_logins.push(account);
            continue;
        }
        let mut updated = false;
        let mut login_changed = false;
        for (report, observed_at) in home_reports.into_iter().flat_map(|reports| &reports.events) {
            if entry.snapshot.observed_at > *observed_at
                && entry.snapshot.observed_at
                    <= now.saturating_add(crate::account::schedule::CLOCK_SLACK)
            {
                continue;
            }
            if let LiveReport::CodexLogin(login) = report {
                if !login.describes(Some(&entry.snapshot), entry.identity.as_ref()) {
                    entry.snapshot = UsageSnapshot::state(
                        UsageStatus::NeedsLogin,
                        "The provider reported an account change; verifying the current sign-in",
                        *observed_at,
                    );
                    if !login_changed {
                        changed_logins.push(account.clone());
                    }
                    login_changed = true;
                    updated = true;
                }
            } else if !login_changed {
                updated |= report.clone().apply(&mut entry.snapshot, *observed_at, now);
            }
        }
        if updated {
            let _ = cache.store(&key, entry.clone());
        }
        if routing::needs_alternate_for(&account, &entry.snapshot, now) {
            exhausted.push(account);
        }
    }
    (entries, exhausted, changed_logins)
}

pub(super) struct Monitor {
    stopped: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}
impl Monitor {
    pub(super) fn start(host: &SharedHost) -> std::io::Result<Self> {
        let stopped = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = Arc::new(Signal::new(stopped.clone()));
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .account_routing = Some(signal.clone());
        let host = Arc::downgrade(host);
        let worker = std::thread::Builder::new()
            .name("account-routing".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let mut schedule = Schedule::default();
                loop {
                    let Some(host) = host.upgrade() else { break };
                    let hosted = {
                        let state = host.lock().unwrap_or_else(|p| p.into_inner());
                        state
                            .chats
                            .values()
                            .any(|record| !record.handle.is_completed())
                            || state.terminals.values().any(|record| {
                                !record.handle.is_completed()
                                    && (crate::agent::Provider::from_program(
                                        &record.session.launch.program,
                                    ).is_some()
                                        || record.session.launch.environment.contains_key(crate::account::SELECTION_MARKER)
                                        || record.session.launch.environment.contains_key(crate::account::routing::AUTOMATIC_MARKER)
                                        || record.session.launch.environment.contains_key(crate::account::SIGN_IN_MARKER))
                            })
                    };
                    drop(host);
                    let reports = signal.drain();
                    if hosted || !reports.is_empty() {
                        let _ = super::service::attempt("automatic account probing", || {
                            let Some(store) = Store::installation() else {
                                return Ok(());
                            };
                            let registry = store.load().map_err(|error| error.to_string())?;
                            let homes = Homes::installation();
                            let cache = Cache::installation();
                            if let Some(cache) = &cache {
                                let now = crate::chat::now_ms().unwrap_or_default();
                                let (mut entries, exhausted, changed_logins) =
                                    apply_reports(&registry, homes.as_ref(), cache, reports, now);
                                for account in changed_logins {
                                    match runtime.block_on(cancellable_probe(
                                        &signal.stopped,
                                        probe::run(account.clone(), Some(cache.clone()), true),
                                    )) {
                                        None => return Ok(()),
                                        Some(probe::Outcome::Answered {
                                            identity,
                                            snapshot,
                                            ..
                                        }) => {
                                            if let Some(key) = account.cache_key() {
                                                let entry = crate::account::cache::Entry {
                                                    identity,
                                                    snapshot,
                                                };
                                                let _ = cache.store(&key, entry.clone());
                                                entries.insert(key, entry);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                for account in exhausted {
                                    if !account
                                        .cache_key()
                                        .and_then(|key| entries.get(&key))
                                        .is_some_and(|entry| {
                                            crate::account::routing::needs_alternate_for(
                                                &account,
                                                &entry.snapshot,
                                                now,
                                            )
                                        })
                                    {
                                        continue;
                                    }
                                    if runtime
                                        .block_on(cancellable_probe(
                                            &signal.stopped,
                                            crate::account::routing_refresh::refresh(
                                                &registry,
                                                homes.as_ref(),
                                                &account,
                                                Some(cache.clone()),
                                                &mut entries,
                                                now,
                                            ),
                                        ))
                                        .is_none()
                                    {
                                        return Ok(());
                                    }
                                }
                            }
                            for account in registry.accounts(homes.as_ref()) {
                                let now = crate::chat::now_ms().unwrap_or_default();
                                if !schedule.due(
                                    &account,
                                    registry.automatic(account.provider),
                                    hosted,
                                    now,
                                ) {
                                    continue;
                                }
                                schedule.attempted.insert(account.id(), now);
                                // One probe at a time. Existing provider locks and
                                // cache freshness also coordinate focused windows.
                                if runtime
                                    .block_on(cancellable_probe(
                                        &signal.stopped,
                                        probe::run(account, Cache::installation(), false),
                                    ))
                                    .is_none()
                                {
                                    break;
                                }
                                if *signal.stopped.0.lock().unwrap_or_else(|p| p.into_inner()) {
                                    break;
                                }
                            }
                            Ok(())
                        });
                    }
                    let guard = signal.stopped.0.lock().unwrap_or_else(|p| p.into_inner());
                    let (guard, _) = signal
                        .stopped
                        .1
                        .wait_timeout_while(guard, POLL, |stopped| {
                            !*stopped && !signal.ready.swap(false, Ordering::AcqRel)
                        })
                        .unwrap_or_else(|p| p.into_inner());
                    if *guard {
                        break;
                    }
                }
            })?;
        Ok(Self {
            stopped,
            worker: Some(worker),
        })
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        *self.stopped.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.stopped.1.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_exhaustion_uses_the_selected_managed_home_without_window_subscribers() {
        use crate::account::{
            LiveReport, UsageSnapshot, UsageStatus,
            cache::Entry,
            registry::{Registered, Registry},
        };
        let root = tempfile::tempdir().unwrap();
        let homes = Homes::at(root.path().join("homes"));
        let provider = crate::agent::Provider::Claude;
        let key = uuid::Uuid::new_v4();
        let account = Account::managed(provider, key, homes.path(provider, key));
        let mut registry = Registry::default();
        registry
            .register(
                Registered {
                    key,
                    provider,
                    linked_home: None,
                    label: None,
                    email: Some("managed@fixture.invalid".into()),
                    organization: None,
                    added_at: 0,
                },
                &[],
            )
            .unwrap();
        let cache = Cache::at(root.path().join("cache"));
        let now = 5_000_000;
        cache
            .store(
                &account.cache_key().unwrap(),
                Entry {
                    identity: None,
                    snapshot: UsageSnapshot::state(UsageStatus::Ok, "", 1),
                },
            )
            .unwrap();
        let signal = Signal::new(Arc::new((Mutex::new(false), Condvar::new())));
        let launch = crate::chat::Launch {
            provider,
            session_id: Some("preserved-thread".into()),
            recipe: crate::model::LaunchRecipe {
                program: "/fixture/custom-wrapper".into(),
                arguments: Vec::new(),
                working_directory: root.path().into(),
                environment: BTreeMap::from([
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        account
                            .resolved_home()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    (
                        "HOME".into(),
                        root.path()
                            .join("system-home")
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    (
                        crate::account::SELECTION_MARKER.into(),
                        format!(
                            "CLAUDE_CONFIG_DIR={}",
                            account.resolved_home().unwrap().display()
                        ),
                    ),
                ]),
                unset_environment: Default::default(),
            },
        };
        std::fs::create_dir_all(account.resolved_home().unwrap()).unwrap();
        let scope = crate::account::scope::LaunchAccountScope::resolve(provider, &launch.recipe.working_directory,
            |name| launch.recipe.environment.get(name).map(std::ffi::OsString::from)).unwrap();
        signal.record(
            &scope,
            LiveReport::Claude(crate::account::claude::LiveUpdate {
                windows: Vec::new(),
                status: Some(UsageStatus::Limited),
            }),
            now,
        );
        assert!(signal.ready.load(Ordering::Acquire));
        let pending = signal.drain();
        assert!(
            pending.homes.contains_key(&(provider, scope.canonical_home.clone(), scope.device, scope.inode)),
            "a managed launch must not be reinterpreted as the system home"
        );
        let (entries, exhausted, logins) =
            apply_reports(&registry, Some(&homes), &cache, pending.clone(), now);
        assert!(logins.is_empty());
        assert_eq!(exhausted, [account.clone()]);
        assert_eq!(
            entries[&account.cache_key().unwrap()].snapshot.status,
            UsageStatus::Limited
        );
        assert_eq!(
            cache.load(now)[&account.cache_key().unwrap()]
                .snapshot
                .observed_at,
            now
        );
        registry.set_default(&account);
        let (_, exhausted, _) = apply_reports(&registry, Some(&homes), &cache, pending.clone(), now);
        assert!(
            exhausted.is_empty(),
            "pinned provider defaults do not schedule exhaustion probes"
        );
        registry.set_automatic(provider);
        cache.store(&account.cache_key().unwrap(), Entry {
            identity: None,
            snapshot: UsageSnapshot::state(UsageStatus::Ok, "replacement login", now),
        }).unwrap();
        let selected = account.resolved_home().unwrap();
        std::fs::rename(&selected, root.path().join("retired-home")).unwrap();
        std::fs::create_dir(&selected).unwrap();
        let (entries, exhausted, logins) = apply_reports(&registry, Some(&homes), &cache, pending, now);
        assert!(exhausted.is_empty());
        assert!(logins.is_empty());
        assert_eq!(entries[&account.cache_key().unwrap()].snapshot.status, UsageStatus::Ok,
            "the old actor's report must not exhaust a replacement home at the same path");
    }

    #[test]
    fn bounded_usage_overflow_cannot_discard_a_login_verification_barrier() {
        use crate::{agent::Provider, account::{LiveReport, Identity, UsageSnapshot, UsageStatus,
            cache::Entry, registry::{Registered, Registry}, scope::LaunchAccountScope}};
        for whole_home_eviction in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let homes = Homes::at(root.path().join("accounts"));
            let (key, home) = homes.create(Provider::Codex, None).unwrap();
            let account = Account::managed(Provider::Codex, key, home.clone());
            let mut registry = Registry::default();
            registry.register(Registered { key, provider: Provider::Codex, linked_home: None,
                label: None, email: Some("old@fixture.invalid".into()), organization: None, added_at: 0 }, &[]).unwrap();
            let cache = Cache::at(root.path().join("cache"));
            let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "verified old login", 1);
            snapshot.plan = Some("pro".into());
            cache.store(&account.cache_key().unwrap(), Entry {
                identity: Some(Identity { email: Some("old@fixture.invalid".into()), plan: Some("pro".into()), ..Default::default() }), snapshot,
            }).unwrap();
            let scope = LaunchAccountScope::resolve(Provider::Codex, root.path(),
                |name| (name == "CODEX_HOME").then(|| home.as_os_str().to_owned())).unwrap();
            let signal = Signal::new(Arc::new((Mutex::new(false), Condvar::new())));
            signal.record(&scope, LiveReport::CodexLogin(crate::account::codex::LoginUpdate {
                chatgpt: false, plan: None,
            }), 10);
            let limits = LiveReport::CodexLimits(crate::account::codex::LimitsUpdate {
                windows: Vec::new(), credits: None, plan: Some("pro".into()), limited: false,
            });
            if whole_home_eviction {
                for index in 0..257 {
                    let path = root.path().join(format!("zz-extra-home-{index:03}"));
                    std::fs::create_dir(&path).unwrap();
                    let scope = LaunchAccountScope::resolve(Provider::Codex, root.path(),
                        |name| (name == "CODEX_HOME").then(|| path.as_os_str().to_owned())).unwrap();
                    signal.record(&scope, limits.clone(), 11 + index);
                }
            } else {
                for index in 0..33 { signal.record(&scope, limits.clone(), 11 + index); }
            }
            let pending = signal.drain();
            assert!(pending.homes.len() <= 256);
            assert!(pending.homes.values().all(|home| home.events.len() <= 32));
            let (entries, exhausted, logins) = apply_reports(&registry, Some(&homes), &cache, pending, 1000);
            assert!(exhausted.is_empty());
            assert!(logins.contains(&account));
            assert_eq!(entries[&account.cache_key().unwrap()].snapshot.status, UsageStatus::NeedsLogin,
                "overflow must require a fresh identity probe before later limits become allowance");
        }
    }

    #[test]
    fn legacy_recipe_usage_is_attributed_only_to_the_actual_startup_home() {
        use crate::{agent::Provider, account::{LiveReport, UsageSnapshot, UsageStatus,
            cache::Entry, registry::{Registered, Registry}}, chat::{self, protocol::{Adapter, ProcessSpec, Step}}};
        struct Quiet;
        impl Adapter for Quiet {
            fn initialize(&mut self) -> Result<Step, String> { Ok(Step::default()) }
            fn command(&mut self, _: chat::Command) -> Result<Step, String> { Ok(Step::default()) }
            fn receive(&mut self, _: serde_json::Value) -> Result<Step, String> { Ok(Step::default()) }
        }
        let directory = tempfile::tempdir().unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let cache = Cache::at(directory.path().join("cache"));
        let mut registry = Registry::default();
        let mut accounts = Vec::new();
        for email in ["a@fixture.invalid", "b@fixture.invalid"] {
            let (key, home) = homes.create(Provider::Claude, None).unwrap();
            let account = Account::managed(Provider::Claude, key, home);
            registry.register(Registered { key, provider: Provider::Claude, linked_home: None,
                label: None, email: Some(email.into()), organization: None, added_at: 0 }, &[]).unwrap();
            cache.store(&account.cache_key().unwrap(), Entry { identity: None,
                snapshot: UsageSnapshot::state(UsageStatus::Ok, "verified login", 1) }).unwrap();
            accounts.push(account);
        }
        let source = chat::Session {
            id: crate::model::TerminalId::new(), proof: None, terminal: None,
            native_continuity: Default::default(), owned_codex_listener: None,
            launch: chat::Launch { provider: Provider::Claude, session_id: None,
                recipe: crate::model::LaunchRecipe { program: "/bin/cat".into(), arguments: Vec::new(),
                    working_directory: directory.path().into(), environment: BTreeMap::new(),
                    unset_environment: Default::default() } },
        };
        let mut spec = ProcessSpec::from(&source.launch.recipe);
        spec.environment.insert("HOME".into(), directory.path().display().to_string());
        spec.environment.insert("CLAUDE_CONFIG_DIR".into(), accounts[1].resolved_home().unwrap().display().to_string());
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let handle = chat::runtime::Handle::start_owned(spec, Box::new(Quiet), initial, store,
            source.clone(), Default::default(), |_| {}).unwrap();
        assert!(source.launch.recipe.environment.is_empty(), "the issued legacy descriptor stays immutable");
        let scope = handle.launch_account_scope().unwrap();
        assert_eq!(scope.canonical_home, accounts[1].resolved_home().unwrap().canonicalize().unwrap());
        let signal = Signal::new(Arc::new((Mutex::new(false), Condvar::new())));
        signal.record(scope, LiveReport::Claude(crate::account::claude::LiveUpdate {
            windows: Vec::new(), status: Some(UsageStatus::Limited),
        }), 5_000_000);
        let (entries, exhausted, _) = apply_reports(&registry, Some(&homes), &cache, signal.drain(), 5_000_000);
        assert_eq!(exhausted, [accounts[1].clone()]);
        assert_eq!(entries[&accounts[0].cache_key().unwrap()].snapshot.status, UsageStatus::Ok);
        assert_eq!(entries[&accounts[1].cache_key().unwrap()].snapshot.status, UsageStatus::Limited);
        handle.close().unwrap();
    }

    #[test]
    fn daemon_schedule_depends_on_automatic_and_hosted_work_not_window_focus() {
        let account = Account::system(crate::agent::Provider::Codex);
        let mut schedule = Schedule::default();
        assert!(!schedule.due(&account, false, true, 0));
        assert!(!schedule.due(&account, true, false, 0));
        assert!(schedule.due(&account, true, true, 0));
        schedule.attempted.insert(account.id(), 100);
        assert!(!schedule.due(&account, true, true, INTERVAL));
        assert!(schedule.due(&account, true, true, INTERVAL + 60_100));
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    #[test]
    fn stopping_drops_a_pending_probe_promptly() {
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_signal = signal.clone();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let marker = dropped.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            struct Cleanup(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::Release);
                }
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(cancellable_probe(&worker_signal, async move {
                let _cleanup = Cleanup(marker);
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            }))
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let before = std::time::Instant::now();
        *signal.0.lock().unwrap() = true;
        assert!(worker.join().unwrap().is_none());
        assert!(before.elapsed() < Duration::from_secs(1));
        assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
    }
}
