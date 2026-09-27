//! Accounts: the status bar's per-provider chips and the popover each one
//! opens, the Accounts pane's table, adding and removing accounts, and the
//! account a new conversation starts on.
//!
//! Figures come from [`crate::account::probe`], which runs each provider's
//! own CLI from an async task and does its file work on the blocking pool;
//! nothing here touches a process or a file on the UI thread. This module
//! only decides when to ask, through [`crate::account::schedule`], and draws
//! whatever came back. Omp's accounts, which Omp picks between itself, are
//! read from the usage record it keeps ([`crate::account::omp`]) on the same
//! pool.
//!
//! # Adding an account
//!
//! An account is added by giving the provider's CLI a home of its own and
//! letting the CLI sign in there, in a terminal the reader can see - the
//! provider's own login, its own browser hand-off, its own device code.
//! cagents never sees what the login produces. When the terminal ends, the
//! home is asked who it is signed in as; a home that is signed in to an
//! account cagents already has is signed out and deleted again, one that is
//! signed in to nothing is deleted, and anything else becomes an account in
//! the registry ([`crate::account::registry`]).

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::account::{
    self, Account, AccountId, Identity, LiveReport, UsageSnapshot, UsageSource, UsageStatus,
    UsageWindow,
    cache::{Cache, Catalog, Entry},
    home::Homes,
    omp::{self, History},
    probe::{self, Outcome},
    registry::{self, Registry, Store},
    schedule::{self, Millis, Schedule, Trigger},
};
use crate::agent::Provider;

mod table;

pub(super) use table::Sort;

/// How often a visible, focused window re-reads the clock and asks the
/// schedule whether an account is due.
const TICK: Duration = Duration::from_secs(30);
/// How often a sign-in in progress is checked for its terminal having
/// ended. The check reads only this window's own state; the CLI is asked
/// who it signed in as once, when the terminal ends.
const LOGIN_TICK: Duration = Duration::from_secs(1);
/// How long live figures wait before they are written to the cache, so a
/// conversation reporting on every response rewrites the file at most this
/// often.
const LIVE_SAVE_DELAY: Duration = Duration::from_secs(5);
/// How often the daemon is asked about sign-ins no card on screen watches.
/// Each question lists every hosted terminal, so it is asked less often
/// than a card on screen is looked at.
const HOST_CHECK: Millis = 3_000;
/// How long after Omp's usage record was read a tick reads it again. Less
/// than a tick, so a focused window reads it on every one: a local read
/// costs nobody's patience, and Omp records figures as it works.
const OMP_REREAD: Millis = 20_000;
const STATUS_BAR_HEIGHT: f32 = 24.0;
const CHIP_HEIGHT: f32 = 20.0;
const CHIP_ICON_SIZE: u16 = 12;
const POPOVER_WIDTH: f32 = 320.0;
/// Below this window width a chip keeps its glyph and ring and drops its
/// words, so the bar never pushes against the window edge.
const COMPACT_WIDTH: f32 = design::OVERLAY_DOCK_BREAKPOINT;

pub(super) struct State {
    accounts: Vec<AccountState>,
    registry: Registry,
    store: Option<Store>,
    homes: Option<Homes>,
    /// The clock as of the last event that read it. Views format against
    /// this rather than reading the clock themselves, so a frame is a pure
    /// function of the state and a test can state the time.
    now_ms: Millis,
    popover: Option<AccountId>,
    cache: Option<Cache>,
    logins: Vec<Login>,
    /// Set before asynchronous CLI discovery/home creation, not after it.
    browser_preparing: bool,
    /// Whether the daemon is being asked which sign-ins have ended.
    asking_host: bool,
    /// Not before this is the daemon asked again.
    next_host_check: Millis,
    table: table::Table,
    /// Accounts being signed out and deleted.
    removing: BTreeSet<AccountId>,
    /// Accounts whose figures a running conversation reported and the cache
    /// has not been given yet.
    live_unsaved: BTreeSet<AccountId>,
    /// The models each provider last listed, which a new conversation offers
    /// before any provider's CLI is started for it.
    catalogs: BTreeMap<Provider, Catalog>,
    /// Providers whose running conversation listed models the cache has not
    /// been given yet.
    catalogs_unsaved: BTreeSet<Provider>,
    /// Whether a write of [`State::live_unsaved`] and
    /// [`State::catalogs_unsaved`] is already due.
    saving_live: bool,
    omp: OmpRecord,
}

/// Omp's usage record, which lists the accounts Omp keeps and is read
/// rather than probed.
#[derive(Debug, Default)]
struct OmpRecord {
    /// Where the record is, `None` where there is nowhere to look.
    database: Option<PathBuf>,
    /// The accounts it last listed, the one Omp recorded most recently
    /// first, which is the one the status bar leads with.
    accounts: Vec<Account>,
    reading: bool,
    /// When the record was last read.
    read_at: Option<Millis>,
    /// Why the last read found nothing it could use, while it could not.
    problem: Option<String>,
}

impl OmpRecord {
    /// Whether `trigger` at `now` should read the record again. The reader
    /// asking always does; otherwise only a focused window reads, and not
    /// again straight after a read.
    fn due(&self, trigger: Trigger, now: Millis, focused: bool) -> bool {
        self.database.is_some()
            && !self.reading
            && (trigger == Trigger::Manual
                || (focused
                    && self
                        .read_at
                        .is_none_or(|read_at| now.saturating_sub(read_at) >= OMP_REREAD)))
    }
}

impl State {
    /// The state of a real window, sharing figures with every other window
    /// of this installation through the cache in its state directory, and
    /// accounts through the registry in its configuration directory.
    pub(super) fn installation() -> Self {
        let mut state = Self::new(
            Cache::installation(),
            Store::installation(),
            Homes::installation(),
        );
        state.omp.database = omp::database(&|name| std::env::var(name).ok());
        state
    }

    fn new(cache: Option<Cache>, store: Option<Store>, homes: Option<Homes>) -> Self {
        let registry = Registry::default();
        let mut state = Self {
            accounts: Vec::new(),
            registry,
            store,
            homes,
            now_ms: now_ms(),
            popover: None,
            cache,
            logins: Vec::new(),
            browser_preparing: false,
            asking_host: false,
            next_host_check: 0,
            table: table::Table::default(),
            removing: BTreeSet::new(),
            live_unsaved: BTreeSet::new(),
            catalogs: BTreeMap::new(),
            catalogs_unsaved: BTreeSet::new(),
            saving_live: false,
            omp: OmpRecord::default(),
        };
        state.sync_accounts();
        state
    }

    fn account(&self, id: &AccountId) -> Option<&AccountState> {
        self.accounts.iter().find(|account| &account.id == id)
    }

    fn account_mut(&mut self, id: &AccountId) -> Option<&mut AccountState> {
        self.accounts.iter_mut().find(|account| &account.id == id)
    }

    /// Makes the account list match the registry and Omp's record: accounts
    /// they no longer name go, new ones join, and every account that stays
    /// keeps what is known about it.
    fn sync_accounts(&mut self) {
        let mut wanted = self.registry.accounts(self.homes.as_ref());
        wanted.extend(self.omp.accounts.iter().cloned());
        let mut previous: BTreeMap<AccountId, AccountState> = std::mem::take(&mut self.accounts)
            .into_iter()
            .map(|state| (state.id.clone(), state))
            .collect();
        self.accounts = wanted
            .into_iter()
            .map(|account| {
                match previous.remove(&account.id()) {
                    Some(state) if state.account == account => state,
                    // A linked home's recorded path can change in another
                    // window or an edited registry. Its old usage and login
                    // observation then belong to the old path.
                    _ => {
                        let mut state = AccountState::new(account);
                        // Whether its CLI is still installed is the probe's to
                        // find out.
                        state.present = !state.account.is_system();
                        state
                    }
                }
            })
            .collect();
        if self
            .popover
            .as_ref()
            .is_some_and(|id| self.account(id).is_none())
        {
            self.popover = None;
        }
        self.table
            .forget_missing(|id| self.accounts.iter().any(|state| &state.id == id));
    }

    fn set_registry(&mut self, registry: Registry) {
        if registry.revision() < self.registry.revision() {
            return;
        }
        if self.registry != registry {
            self.registry = registry;
            self.sync_accounts();
        }
    }

    /// What the reader calls `account`: the name they gave it, else the
    /// address it is signed in as, else what kind of account it is. One of
    /// Omp's accounts is named with the service it signs in to, since one
    /// address can have a login with several.
    fn display_name(&self, account: &AccountState) -> String {
        if let Some(service) = account.account.brokered_service() {
            let service = omp::service_label(service);
            return match account.email(&self.registry) {
                Some(email) => format!("{service} · {email}"),
                None => format!("{service} account"),
            };
        }
        if let Some(label) = self.registry.label(&account.account) {
            return label.to_owned();
        }
        let linked_hint = account.account.is_linked().then(|| {
            account
                .account
                .resolved_home()
                .as_deref()
                .and_then(|home| {
                    let named = if home.file_name().is_some_and(|name| name == "home") {
                        home.parent().unwrap_or(home)
                    } else {
                        home
                    };
                    named.file_name()
                })
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "existing".into())
        });
        if let Some(email) = account.email(&self.registry) {
            if let Some(hint) = linked_hint {
                return format!("{hint} · {email}");
            }
            return email.to_owned();
        }
        if let Some(hint) = linked_hint {
            return format!("{} · {hint}", account.name());
        }
        if account.account.is_system() {
            "System default".into()
        } else {
            format!("{} account", account.name())
        }
    }

    /// The accounts of `provider`, in list order.
    fn of(&self, provider: Provider) -> impl Iterator<Item = &AccountState> {
        self.accounts
            .iter()
            .filter(move |account| account.account.provider == provider)
    }

    /// Whether any account of `provider` other than `except` has a probe in
    /// flight, which the next one waits for: several CLIs of one provider
    /// starting at once would only compete for the same machine.
    fn provider_busy(&self, provider: Provider, except: &AccountId, now: Millis) -> bool {
        self.of(provider)
            .any(|account| &account.id != except && account.schedule.in_flight(now))
    }

    /// Takes what a load found for each account: whether it has a CLI to ask,
    /// and what the cache had for it.
    fn restore(&mut self, restored: Vec<Restored>) {
        for restored in restored {
            let Some(state) = self.account_mut(&restored.id) else {
                continue;
            };
            // A probe that answered first knows better than the load.
            if state.snapshot.is_some() {
                continue;
            }
            state.present = restored.present;
            if let Some(entry) = restored.entry.filter(|_| restored.present) {
                let mut snapshot = entry.snapshot;
                snapshot.source = UsageSource::Cache;
                state.schedule.observe(snapshot.observed_at);
                state.identity = entry.identity;
                state.snapshot = Some(snapshot);
            }
            if !restored.present && self.popover.as_ref() == Some(&restored.id) {
                self.popover = None;
            }
        }
    }

    /// Takes what a read of Omp's usage record found at `now`. A record
    /// that is gone takes its accounts with it; one that could not be read
    /// leaves them as they were, saying why they are not newer.
    fn take_history(&mut self, history: History, now: Millis) {
        self.omp.reading = false;
        self.omp.read_at = Some(now);
        self.omp.problem = None;
        let brokered = match history {
            History::Missing => Vec::new(),
            History::Unreadable(message) => {
                self.omp.problem = Some(message.clone());
                for state in &mut self.accounts {
                    if state.account.is_brokered() {
                        state.schedule.fail(now);
                        state.failure = Some(message.clone());
                    }
                }
                return;
            }
            History::Read(brokered) => brokered,
        };
        self.omp.accounts = brokered
            .iter()
            .map(|brokered| brokered.account.clone())
            .collect();
        self.sync_accounts();
        for brokered in brokered {
            let Some(state) = self.account_mut(&brokered.account.id()) else {
                continue;
            };
            state.schedule.succeed(brokered.snapshot.observed_at);
            state.identity = Some(brokered.identity);
            state.snapshot = Some(brokered.snapshot);
            state.failure = None;
        }
    }

    /// The models `provider` last listed, in a conversation or to a probe.
    pub(super) fn catalog(&self, provider: Provider) -> Option<&Catalog> {
        self.catalogs.get(&provider)
    }

    /// Whether a new conversation can be started on `provider`: it has
    /// listed its models before, or one of its accounts has a CLI to run.
    pub(super) fn offers(&self, provider: Provider) -> bool {
        self.catalog(provider)
            .is_some_and(|catalog| !catalog.models.is_empty())
            || self.of(provider).any(|account| account.present)
    }

    /// Takes what `provider` listed, unless what is known is newer: another
    /// window's list read from the cache can be older than this window's.
    /// Whether it was taken.
    fn take_catalog(&mut self, provider: Provider, catalog: Catalog) -> bool {
        if self
            .catalogs
            .get(&provider)
            .is_some_and(|known| known.observed_at > catalog.observed_at)
        {
            return false;
        }
        self.catalogs.insert(provider, catalog);
        true
    }

    /// Why Omp's usage record could not be read, when it could not.
    pub(super) fn omp_problem(&self) -> Option<&str> {
        self.omp.problem.as_deref()
    }

    /// What the accounts running conversations reported on since the last
    /// write leave for the cache.
    fn take_live_entries(&mut self) -> Vec<(Account, Entry)> {
        std::mem::take(&mut self.live_unsaved)
            .iter()
            .filter_map(|id| {
                let state = self.account(id)?;
                Some((
                    state.account.clone(),
                    Entry {
                        identity: state.identity.clone(),
                        snapshot: state.snapshot.clone()?,
                    },
                ))
            })
            .collect()
    }

    /// What running conversations listed since the last write leave for the
    /// cache.
    fn take_live_catalogs(&mut self) -> Vec<(Provider, Catalog)> {
        std::mem::take(&mut self.catalogs_unsaved)
            .into_iter()
            .filter_map(|provider| Some((provider, self.catalogs.get(&provider)?.clone())))
            .collect()
    }

    /// The identities of the system accounts, as last reported, for telling
    /// a new login apart from them.
    fn system_identities(&self) -> Vec<(Provider, Identity)> {
        self.accounts
            .iter()
            .filter(|account| account.account.is_system())
            .filter_map(|account| Some((account.account.provider, account.identity.clone()?)))
            .collect()
    }
}

/// A window with no cache and no registry: nothing it learns outlives it or
/// reaches another window, which is what a test fixture wants.
impl Default for State {
    fn default() -> Self {
        Self::new(None, None, None)
    }
}

struct AccountState {
    account: Account,
    id: AccountId,
    /// Whether the account has anything to report: its CLI is installed and
    /// its home exists. An absent system account gets no chip, because most
    /// people use one of the two CLIs rather than both.
    present: bool,
    identity: Option<Identity>,
    snapshot: Option<UsageSnapshot>,
    schedule: Schedule,
    /// Why the latest probe produced nothing, while the figures on screen
    /// are older than that attempt.
    failure: Option<String>,
    /// A probe that came due while another account of the same provider was
    /// being probed, and starts once that one answers.
    queued: Option<Trigger>,
}

impl AccountState {
    fn new(account: Account) -> Self {
        Self {
            id: account.id(),
            account,
            present: false,
            identity: None,
            snapshot: None,
            schedule: Schedule::default(),
            failure: None,
            queued: None,
        }
    }

    fn name(&self) -> &'static str {
        self.account.provider.label()
    }

    fn plan(&self) -> Option<String> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.plan.as_deref())
            .or_else(|| self.identity.as_ref()?.plan.as_deref())
            .map(account::plan_label)
    }

    /// The address the account is signed in as: as its CLI last reported, or
    /// as the registry remembers from when it was added.
    fn email<'a>(&'a self, registry: &'a Registry) -> Option<&'a str> {
        self.identity
            .as_ref()
            .and_then(|identity| identity.email.as_deref())
            .or_else(|| {
                registry
                    .find(self.account.registry_key()?)?
                    .email
                    .as_deref()
            })
    }

    /// The window a summary of this account leads with, while its figures
    /// are figures rather than a state.
    fn headline(&self) -> Option<&UsageWindow> {
        let snapshot = self.snapshot.as_ref()?;
        matches!(
            snapshot.status,
            UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited
        )
        .then(|| headline_window(snapshot))
        .flatten()
    }
}

/// A sign-in running in a terminal, for an account that is not yet in the
/// registry. Its terminal is found by the home it signs in to, which survives
/// the terminal being relaunched or restored under another id.
#[derive(Debug, Clone)]
struct Login {
    provider: Provider,
    key: uuid::Uuid,
    home: PathBuf,
    phase: LoginPhase,
    /// How many times the home has been asked who it is signed in as.
    attempts: u32,
    /// Not before this is the home asked again.
    next_check: Millis,
    /// When the sign-in began, as the registry's pending entry records it.
    started_at: Millis,
    /// Picked up from an earlier run, with no terminal in this window: its
    /// home is only deleted once it is old enough that no window can still
    /// be signing in to it.
    recovered: bool,
}

/// How many times a sign-in's home is asked who it is signed in as before
/// the question is left for the next start.
const LOGIN_ATTEMPTS: u32 = 3;
/// How long after a failed question the home is asked again, per attempt.
const LOGIN_RETRY: Millis = 5_000;
/// How old a pending sign-in with no terminal in this window must be before
/// this window asks its home who it is signed in as.
const RECOVER_AFTER: Millis = 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginPhase {
    /// The terminal is running the provider's login.
    Waiting,
    /// The terminal ended and the home is being asked who it signed in as.
    Verifying,
    /// The answer is in, and the home is being registered or deleted.
    Settling,
    /// The callback could not be acquired; retain the row until dismissed.
    CallbackBlocked,
}

impl Login {
    fn account(&self) -> Account {
        Account::managed(self.provider, self.key, self.home.clone())
    }
}

/// How a sign-in ended.
#[derive(Debug, Clone)]
pub enum Settled {
    /// The account joined the registry, now as given.
    Registered {
        registry: Registry,
        identity: Option<Identity>,
        snapshot: Option<UsageSnapshot>,
    },
    /// The login duplicated the account named, and was signed out again.
    Duplicate {
        existing: AccountId,
        registry: Option<Registry>,
    },
    /// Nothing was signed in, and the home is gone.
    Abandoned(Option<Registry>),
}

/// What the startup load found.
#[derive(Debug, Clone)]
pub struct Loaded {
    registry: Result<Registry, String>,
    restored: Vec<Restored>,
    catalogs: BTreeMap<Provider, Catalog>,
    /// Whether presence was judged on the interface's own `PATH` because the
    /// shell had not yet said what a terminal would search.
    provisional: bool,
}

/// What the startup load found for one account.
#[derive(Debug, Clone)]
pub struct Restored {
    id: AccountId,
    present: bool,
    entry: Option<Entry>,
}

#[derive(Debug, Clone)]
pub enum Action {
    Tick,
    Loaded(Loaded),
    /// Presence judged again on the shell's `PATH`, once it is known.
    Located(Vec<Restored>),
    Probed {
        id: AccountId,
        outcome: Outcome,
    },
    /// Omp's usage record was read.
    OmpRead(History),
    Refresh(AccountId),
    /// The debounce on a manual refresh lifted, so its control can offer
    /// another.
    ManualReady,
    /// Live figures are due to be written to the cache.
    SaveLive,
    /// Opens the popover for an account, or closes it with `None` or by
    /// naming the account whose popover is already open.
    Popover(Option<AccountId>),
    /// Shows an account and, when Cagents owns the choice, makes it default.
    ShowAccount(AccountId),
    OpenPane,
    SignIn(AccountId),
    /// The CLI and browser callback were checked off the interface thread.
    LoginReady {
        id: AccountId,
        provider: Provider,
        program: Result<PathBuf, String>,
    },
    /// The registry was read again, as another window may have changed it.
    RegistryLoaded(Result<Registry, String>),
    /// A change this window made to the registry was written, or was not.
    Saved(Result<Registry, String>),
    /// Starts adding an account; `device` asks Codex for a device code
    /// rather than a browser hand-off.
    Add {
        provider: Provider,
        device: bool,
    },
    LinkStart(Provider),
    LinkInput(String),
    LinkCommit,
    LinkCancel,
    LinkReady {
        key: uuid::Uuid,
        result: Result<Registry, String>,
    },
    /// The new account's home and CLI are ready, or could not be.
    HomeReady {
        provider: Provider,
        device: bool,
        result: Result<(uuid::Uuid, PathBuf, PathBuf), String>,
    },
    LoginTick,
    LoginCallbackBlocked(uuid::Uuid),
    /// The daemon's answer for sign-ins whose terminals this window is not
    /// watching: the ones whose terminals have ended.
    LoginsEnded(Result<Vec<(uuid::Uuid, TerminalId)>, String>),
    /// A sign-in card off the strip on screen had its terminal closed
    /// through the daemon, or could not.
    OffstripClosed {
        card: CardId,
        result: Result<(), String>,
    },
    LoginVerified {
        key: uuid::Uuid,
        outcome: Outcome,
    },
    LoginSettled {
        key: uuid::Uuid,
        result: Result<Settled, String>,
    },
    CancelLogin(uuid::Uuid),
    SetDefault(AccountId),
    SetProjectDefault(crate::model::ProjectId, AccountId),
    ClearProjectDefault(crate::model::ProjectId, Provider),
    Rename(AccountId),
    RenameInput(String),
    RenameCommit,
    RenameCancel,
    /// Asks to remove an account; [`Action::RemoveConfirm`] does it.
    Remove(AccountId),
    RemoveConfirm(AccountId),
    RemoveCancel,
    /// A removal finished: the registry as it now stands, where it was
    /// changed, and what went wrong, if anything.
    Removed {
        id: AccountId,
        registry: Option<Registry>,
        error: Option<String>,
    },
    /// Opens a row's menu, or closes it with `None`.
    Menu(Option<AccountId>),
    AddMenu(bool),
    OwnCodexConfig(bool),
    Sort(Sort),
}

impl Action {
    /// Whether this is the reader acting on the account surfaces, which a
    /// dialog in front of them must stop. Closing a popup never is.
    pub(super) fn blocked_while_modal(&self) -> bool {
        matches!(
            self,
            Self::Refresh(_)
                | Self::Popover(Some(_))
                | Self::ShowAccount(_)
                | Self::OpenPane
                | Self::SignIn(_)
                | Self::Add { .. }
                | Self::LinkStart(_)
                | Self::LinkCommit
                | Self::CancelLogin(_)
                | Self::SetDefault(_)
                | Self::SetProjectDefault(_, _)
                | Self::ClearProjectDefault(_, _)
                | Self::Rename(_)
                | Self::RenameCommit
                | Self::Remove(_)
                | Self::RemoveConfirm(_)
                | Self::Menu(Some(_))
                | Self::AddMenu(true)
                | Self::OwnCodexConfig(_)
                | Self::Sort(_)
        )
    }
}

fn now_ms() -> Millis {
    crate::chat::now_ms().unwrap_or_default()
}

/// Writes figures and models running conversations reported to the cache
/// other windows read. Blocking: finding an account's cache key resolves its
/// home.
fn store_live(cache: &Cache, entries: Vec<(Account, Entry)>, catalogs: Vec<(Provider, Catalog)>) {
    for (account, entry) in entries {
        let Some(key) = account.cache_key() else {
            continue;
        };
        if let Err(error) = cache.store(&key, entry) {
            tracing::debug!(%error, "live account usage could not be cached");
        }
    }
    for (provider, catalog) in catalogs {
        if let Err(error) = cache.store_catalog(provider, catalog) {
            tracing::debug!(%error, ?provider, "listed models could not be cached");
        }
    }
}

fn restore(
    accounts: &[Account],
    cache: Option<&Cache>,
    now: Millis,
    search_path: Option<&std::ffi::OsStr>,
) -> Vec<Restored> {
    let mut entries = cache.map(|cache| cache.load(now)).unwrap_or_default();
    accounts
        .iter()
        .map(|account| Restored {
            id: account.id(),
            present: account.resolved_home().is_some_and(|home| home.is_dir())
                && probe::find_executable(account.provider.executable(), search_path).is_some(),
            entry: account.cache_key().and_then(|key| entries.remove(&key)),
        })
        .collect()
}

/// Whether figures observed at `observed_at` still describe the account at
/// `now`: they are recent, and no window has reset since they were read.
fn is_stale(snapshot: &UsageSnapshot, now: Millis) -> bool {
    if now.saturating_sub(snapshot.observed_at) > schedule::DISPLAY_STALE_AFTER {
        return true;
    }
    reset_since(snapshot, now)
}

/// Whether a window reset between the observation and `now`, which makes
/// its figure wrong however recent the observation is.
fn reset_since(snapshot: &UsageSnapshot, now: Millis) -> bool {
    snapshot.windows.iter().any(|window| {
        window
            .resets_at
            .and_then(|seconds| u64::try_from(seconds).ok())
            .map(|seconds| seconds.saturating_mul(1_000))
            .is_some_and(|reset| reset > snapshot.observed_at && reset <= now)
    })
}

/// The whole percentage left in a window, rounded toward caution: a window
/// 42.4% used has 57% left, not 58%.
fn percent_left(used: f32) -> u32 {
    // Float noise must not turn an exact 43% into 44% used.
    let used = (used * 100.0 - 0.001).ceil().clamp(0.0, 100.0);
    (100.0 - used) as u32
}

fn unix_seconds(now: Millis) -> i64 {
    i64::try_from(now / 1_000).unwrap_or(i64::MAX)
}

/// When a window resets, as the popover states it.
fn reset_line(window: &UsageWindow, now: Millis) -> Option<String> {
    let resets_at = window.resets_at?;
    let now_seconds = unix_seconds(now);
    let moment = account::format_local_moment(resets_at, now_seconds);
    Some(if resets_at <= now_seconds {
        format!("Reset at {moment}; refresh for current figures")
    } else {
        format!(
            "Resets in {} · {moment}",
            account::format_remaining(resets_at - now_seconds)
        )
    })
}

/// How long until a window resets, as a table cell states it.
fn reset_countdown(window: &UsageWindow, now: Millis) -> Option<String> {
    let resets_at = window.resets_at?;
    let now_seconds = unix_seconds(now);
    Some(if resets_at <= now_seconds {
        "reset".into()
    } else {
        account::format_remaining(resets_at - now_seconds)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChipTone {
    Current,
    /// Figures the reader should not plan around: old, or from before a
    /// reset.
    Stale,
    /// Something the reader has to do.
    Attention,
}

/// What an account's chip says.
#[derive(Debug, Clone, PartialEq)]
struct Chip {
    text: String,
    /// The used fraction of the binding window, drawn as the ring.
    gauge: Option<f32>,
    tone: ChipTone,
    /// Kept when the bar is too narrow for the rest of the text.
    essential: bool,
    tooltip: String,
}

/// The window a chip leads with: the one binding the account, unless that is
/// one model's allowance - running out of it leaves every other model
/// working - in which case the tightest account-wide window.
fn headline_window(snapshot: &UsageSnapshot) -> Option<&UsageWindow> {
    let binding = snapshot.binding_window()?;
    if binding.kind != account::WindowKind::Scoped {
        return Some(binding);
    }
    Some(
        account::tightest(
            snapshot
                .windows
                .iter()
                .filter(|window| window.kind != account::WindowKind::Scoped),
        )
        .unwrap_or(binding),
    )
}

fn chip(account: &AccountState, name: &str, now: Millis) -> Chip {
    let Some(snapshot) = &account.snapshot else {
        return match &account.failure {
            Some(failure) => Chip {
                text: "Unavailable".into(),
                gauge: None,
                tone: ChipTone::Stale,
                essential: false,
                tooltip: format!("{name} usage could not be read: {failure}"),
            },
            None => Chip {
                text: "Checking…".into(),
                gauge: None,
                tone: ChipTone::Stale,
                essential: false,
                tooltip: format!("Checking {name} usage…"),
            },
        };
    };
    match snapshot.status {
        UsageStatus::NeedsLogin => Chip {
            text: "Sign in".into(),
            gauge: None,
            tone: ChipTone::Attention,
            essential: true,
            tooltip: format!("{name} is signed out. Sign in to see its usage."),
        },
        UsageStatus::Unavailable => Chip {
            text: "Unavailable".into(),
            gauge: None,
            tone: ChipTone::Stale,
            essential: false,
            tooltip: snapshot
                .detail
                .clone()
                .unwrap_or_else(|| format!("{name} reports no usage")),
        },
        UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited => {
            let Some(window) = headline_window(snapshot) else {
                return Chip {
                    text: "No limits".into(),
                    gauge: None,
                    tone: ChipTone::Current,
                    essential: false,
                    tooltip: format!("{name} reports no usage limits for this account"),
                };
            };
            let stale = is_stale(snapshot, now);
            let left = percent_left(window.used);
            let now_seconds = unix_seconds(now);
            let remaining = window
                .resets_at
                .filter(|reset| *reset > now_seconds)
                .map(|reset| account::format_remaining(reset - now_seconds));
            let as_of = stale.then(|| {
                format!(
                    "as of {}",
                    account::format_local_moment(unix_seconds(snapshot.observed_at), now_seconds)
                )
            });
            let mut text = format!("{left}% left");
            if let Some(when) = as_of.as_ref().or(remaining.as_ref()) {
                text.push_str(" · ");
                text.push_str(when);
            }
            let mut tooltip = format!("{name} · {}: {left}% left", window.label);
            if snapshot.status == UsageStatus::Limited {
                tooltip.push_str(", limited");
            }
            if let Some(remaining) = &remaining {
                tooltip.push_str(&format!(", resets in {remaining}"));
            }
            if let Some(as_of) = &as_of {
                tooltip.push_str(&format!(" ({as_of})"));
            }
            // A model's own allowance binding is worth knowing, but it is not
            // what the account as a whole has left.
            if let Some(scoped) = snapshot
                .binding_window()
                .filter(|binding| binding.key != window.key)
            {
                tooltip.push_str(&format!(
                    "\n{}: {}% left",
                    scoped.label,
                    percent_left(scoped.used)
                ));
            }
            Chip {
                text,
                gauge: Some(window.used),
                tone: if stale {
                    ChipTone::Stale
                } else {
                    ChipTone::Current
                },
                essential: false,
                tooltip,
            }
        }
    }
}

/// How a provider's CLI signs in: through the browser by default, or, for
/// Codex, with a device code a reader can finish on another machine.
fn login_arguments(provider: Provider, device: bool) -> Option<&'static [&'static str]> {
    Some(match (provider, device) {
        (Provider::Claude, _) => &["auth", "login", "--claudeai"],
        (Provider::Codex, false) => &["login"],
        (Provider::Codex, true) => &["login", "--device-auth"],
        (Provider::Omp, _) => return None,
    })
}

/// The terminal that runs `account`'s login command: the CLI at `program`,
/// the same executable the probes run, started in the reader's home - not in
/// a project, whose own CLI settings have no business in a login.
///
/// `auth` and `login` are account management, which the launch recognizer
/// does not take for an agent conversation.
fn login_launch(
    account: &Account,
    program: &Path,
    home: PathBuf,
    device: bool,
) -> Option<LaunchRecipe> {
    let command = login_arguments(account.provider, device)?;
    let mut environment = BTreeMap::new();
    if let (Some(variable), Some(account_home)) = (
        account::home_variable(account.provider),
        account.resolved_home(),
    ) {
        environment.insert(variable.to_owned(), account_home.display().to_string());
    }
    // The CLI was found on the search path the reader's shell sets up, and
    // runs with it, as the probes run it.
    if let Some(path) = probe::shell_search_path().and_then(|path| path.to_str()) {
        environment.insert("PATH".into(), path.to_owned());
    }
    Some(LaunchRecipe {
        unset_environment: account
            .credential_overrides()
            .iter()
            .map(|name| (*name).into())
            .collect(),
        program: program.to_str()?.to_owned(),
        arguments: command
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        working_directory: home,
        environment,
    })
}

/// Whether `recipe` runs a provider's sign-in, as [`login_launch`] builds
/// it or as a reader types it.
pub(super) fn is_sign_in(recipe: &LaunchRecipe) -> bool {
    let program = Path::new(&recipe.program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let arguments: Vec<&str> = recipe.arguments.iter().map(String::as_str).collect();
    matches!(
        (program, arguments.as_slice()),
        ("claude", ["auth", "login"])
            | ("claude", ["auth", "login", "--claudeai"])
            | ("codex", ["login"])
            | ("codex", ["login", "--device-auth"])
    )
}

/// The account a conversation's launch runs on, by the home its recipe
/// points the provider's CLI at.
fn account_of_recipe<'a>(
    accounts: &'a [AccountState],
    provider: Provider,
    recipe: &LaunchRecipe,
) -> Option<&'a AccountState> {
    let variable = account::home_variable(provider)?;
    let chosen = recipe
        .environment
        .get(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let system = accounts
        .iter()
        .find(|state| state.account.provider == provider && state.account.is_system());
    let Some(chosen) = chosen else {
        return system;
    };
    accounts
        .iter()
        .filter(|state| state.account.provider == provider)
        .find(|state| state.account.resolved_home().as_deref() == Some(chosen.as_path()))
        .or_else(|| {
            // A home the recipe inherited from cagents' own environment is
            // the system account, however the reader spelled it.
            let system_home = system?.account.resolved_home()?;
            (system_home == chosen).then_some(system?)
        })
}

/// One account a conversation's composer offers.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChatAccount {
    pub id: AccountId,
    pub label: String,
    /// The plan and what is left, where known: `Max · 62% left`.
    pub detail: String,
    pub selected: bool,
    /// The used fraction of the account's headline window, for its ring.
    pub used: Option<f32>,
}

/// One account the settings pane offers as a provider's default.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DefaultChoice {
    pub id: AccountId,
    pub label: String,
    pub selected: bool,
}

/// The account picker a conversation's composer shows.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ChatAccounts {
    /// What the picker's button says: the chosen account's name.
    pub current: String,
    pub choices: Vec<ChatAccount>,
}

impl Cagents {
    /// Whether anything shows account usage, which is the only reason to
    /// ask for it.
    fn accounts_surface_visible(&self) -> bool {
        self.settings.show_status_bar || self.utility_card(&CardKind::Accounts).is_some()
    }

    /// Ticks while anything shows account usage. Unfocused, a tick only
    /// moves the clock the figures are read against - "resets in", "updated
    /// ... ago" - since [`Schedule::should_probe`] probes a focused window
    /// alone. A sign-in in progress ticks faster, to notice its terminal
    /// ending.
    pub(super) fn account_subscription(&self) -> Subscription<Message> {
        let usage = if self.accounts_surface_visible() {
            cosmic::iced::time::every(TICK).map(|_| Message::AccountUi(Action::Tick))
        } else {
            Subscription::none()
        };
        let logins = if self
            .account_ui
            .logins
            .iter()
            .any(|login| login.phase == LoginPhase::Waiting)
        {
            cosmic::iced::time::every(LOGIN_TICK).map(|_| Message::AccountUi(Action::LoginTick))
        } else {
            Subscription::none()
        };
        Subscription::batch([usage, logins])
    }

    /// Reads the registry - finishing, under its lock, any removal an earlier
    /// run was interrupted in - what earlier probes, from this window or
    /// another, left in the cache, and which accounts have a CLI to ask.
    ///
    /// It does not wait on the reader's shell for where a terminal would look
    /// for the CLIs: a slow startup file would hold back every figure the
    /// cache already has. Until the shell answers, the interface's own
    /// `PATH` stands in, and [`Self::locate_accounts`] looks again after.
    ///
    /// Omp's usage record is read beside it, on its own: a read that waits
    /// on Omp writing holds back no other account.
    pub(super) fn load_accounts(&mut self) -> Task<Message> {
        let store = self.account_ui.store.clone();
        let homes = self.account_ui.homes.clone();
        let cache = self.account_ui.cache.clone();
        let load = cosmic::task::future(async move {
            let provisional = !probe::shell_asked();
            let search_path = probe::current_search_path();
            let loaded = tokio::task::spawn_blocking(move || {
                let registry = match (&store, &homes) {
                    (Some(store), Some(homes)) => store
                        .load_and_sweep(homes)
                        .map_err(|error| error.to_string()),
                    (Some(store), None) => store.load().map_err(|error| error.to_string()),
                    _ => Ok(Registry::default()),
                };
                let accounts = registry
                    .as_ref()
                    .map(|registry| registry.accounts(homes.as_ref()))
                    .unwrap_or_else(|_| Account::system_accounts().into());
                let now = now_ms();
                let restored = restore(&accounts, cache.as_ref(), now, search_path.as_deref());
                let catalogs = cache
                    .as_ref()
                    .map(|cache| cache.catalogs(now))
                    .unwrap_or_default();
                // Not while the registry cannot be read, or its accounts' homes
                // cannot be found, which would take every added account for
                // one since removed.
                if let (Some(cache), Ok(_), Some(_)) = (&cache, &registry, &homes) {
                    let keep = accounts.iter().filter_map(Account::cache_key).collect();
                    if let Err(error) = cache.prune(&keep, now) {
                        tracing::debug!(%error, "the account usage cache could not be pruned");
                    }
                }
                Loaded {
                    registry,
                    restored,
                    catalogs,
                    provisional,
                }
            })
            .await
            .unwrap_or_else(|_| Loaded {
                registry: Err("The accounts could not be read".into()),
                restored: Vec::new(),
                catalogs: BTreeMap::new(),
                provisional,
            });
            Message::AccountUi(Action::Loaded(loaded))
        });
        match self.start_omp_read() {
            Some(read) => Task::batch([load, read]),
            None => load,
        }
    }

    /// Judges again, on the shell's `PATH`, which accounts have a CLI to ask,
    /// restoring what the cache has for any found only there.
    fn locate_accounts(&self) -> Task<Message> {
        let accounts: Vec<Account> = self
            .account_ui
            .accounts
            .iter()
            // Omp's accounts have no home or CLI of their own to find.
            .filter(|state| !state.account.is_brokered())
            .map(|state| state.account.clone())
            .collect();
        let cache = self.account_ui.cache.clone();
        cosmic::task::future(async move {
            let search_path = probe::search_path().await;
            let restored = tokio::task::spawn_blocking(move || {
                restore(&accounts, cache.as_ref(), now_ms(), search_path.as_deref())
            })
            .await
            .unwrap_or_default();
            Message::AccountUi(Action::Located(restored))
        })
    }

    /// Reads the registry again, for what another window changed.
    pub(super) fn reload_registry(&self) -> Task<Message> {
        let Some(store) = self.account_ui.store.clone() else {
            return Task::none();
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || store.load())
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            Message::AccountUi(Action::RegistryLoaded(result))
        })
    }

    /// Applies `change` to the registry on the blocking pool and reports the
    /// result as [`Action::Saved`].
    fn save_registry(
        &mut self,
        change: impl FnOnce(&mut Registry) + Send + 'static,
    ) -> Task<Message> {
        let Some(store) = self.account_ui.store.clone() else {
            // A window without a configuration directory keeps its choices
            // for as long as it runs.
            let mut registry = self.account_ui.registry.clone();
            change(&mut registry);
            self.account_ui.set_registry(registry);
            return Task::none();
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || store.update(change))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| {
                    result
                        .map(|(registry, ())| registry)
                        .map_err(|error| error.to_string())
                });
            Message::AccountUi(Action::Saved(result))
        })
    }

    /// The status bar was shown or hidden, or the Accounts pane opened.
    pub(super) fn accounts_surface_changed(&mut self) -> Task<Message> {
        if !self.settings.show_status_bar {
            self.account_ui.popover = None;
        }
        self.probe_accounts(Trigger::Focus)
    }

    /// Called with the main window's focus changes, after `refresh_ui` has
    /// recorded them; `regained` is whether the window was unfocused before.
    pub(super) fn accounts_focus_changed(
        &mut self,
        focused: bool,
        regained: bool,
    ) -> Task<Message> {
        if !focused {
            // An anchored popup outliving the reader's attention would still
            // be open, over stale figures, when they come back.
            self.account_ui.popover = None;
            self.account_ui.table.close_popups();
        }
        if !regained {
            return Task::none();
        }
        Task::batch([self.reload_registry(), self.probe_accounts(Trigger::Focus)])
    }

    /// The account popups' share of a key event, like the chrome's popups:
    /// a dialog opening closes them, and Escape closes them before anything
    /// behind.
    pub(super) fn account_key_event(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        if self.account_ui.popover.is_none() && !self.account_ui.table.has_popup() {
            return None;
        }
        if self.modal_is_open() {
            self.account_ui.popover = None;
            self.account_ui.table.close_popups();
            return None;
        }
        is_escape_press(event).then(|| {
            if self.account_ui.popover.take().is_none() {
                self.account_ui.table.escape();
            }
            Task::none()
        })
    }

    fn probe_accounts(&mut self, trigger: Trigger) -> Task<Message> {
        let now = now_ms();
        self.account_ui.now_ms = now;
        if !self.accounts_surface_visible() {
            return Task::none();
        }
        let ids: Vec<AccountId> = self
            .account_ui
            .accounts
            .iter()
            .filter(|account| !account.account.is_brokered())
            .map(|account| account.id.clone())
            .collect();
        let omp = self.read_omp(trigger, now);
        Task::batch(
            ids.iter()
                .filter_map(|id| self.probe_account(id, trigger, now))
                .chain(omp),
        )
    }

    fn probe_account(
        &mut self,
        id: &AccountId,
        trigger: Trigger,
        now: Millis,
    ) -> Option<Task<Message>> {
        let focused = self.refresh_ui.focused;
        let cache = self.account_ui.cache.clone();
        let account = &self.account_ui.account(id)?.account;
        // Omp's accounts are asked about together, by reading its record.
        if account.is_brokered() {
            return self.read_omp(trigger, now);
        }
        let provider = account.provider;
        let busy = self.account_ui.provider_busy(provider, id, now);
        let state = self.account_ui.account_mut(id)?;
        // Figures from before a reset are wrong however recent they are, so
        // they are refreshed as eagerly as on the reader's return.
        let trigger = match trigger {
            Trigger::Tick
                if state
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| reset_since(snapshot, now)) =>
            {
                Trigger::Focus
            }
            trigger => trigger,
        };
        if !state.schedule.should_probe(trigger, now, focused) {
            return None;
        }
        if busy {
            state.queued = Some(match (state.queued, trigger) {
                (Some(Trigger::Manual), _) | (_, Trigger::Manual) => Trigger::Manual,
                (Some(Trigger::Focus), _) | (_, Trigger::Focus) => Trigger::Focus,
                _ => Trigger::Tick,
            });
            return None;
        }
        state.queued = None;
        // The cache is only as fresh as the last probe, which is exactly
        // what a reader asking again, or signing in, is trying to get past.
        let force = trigger == Trigger::Manual || state.schedule.awaiting_login(now);
        state.schedule.start(trigger, now);
        let account = state.account.clone();
        let id = state.id.clone();
        let probe = cosmic::task::future(async move {
            let outcome = probe::run(account, cache, force).await;
            Message::AccountUi(Action::Probed { id, outcome })
        });
        // The refresh control comes back the moment its debounce lifts,
        // rather than on whichever tick next reads the clock.
        let Some(ready_at) = state
            .schedule
            .manual_ready_at()
            .filter(|_| trigger == Trigger::Manual)
        else {
            return Some(probe);
        };
        let wait = Duration::from_millis(ready_at.saturating_sub(now));
        Some(Task::batch([
            probe,
            cosmic::task::future(async move {
                tokio::time::sleep(wait).await;
                Message::AccountUi(Action::ManualReady)
            }),
        ]))
    }

    /// Reads Omp's usage record on the blocking pool, if `trigger` makes it
    /// due. A manual refresh of one of its accounts is a refresh of all of
    /// them, since they are one read: each shows it under way, and each is
    /// debounced. The reads a tick makes are too quick and too frequent to
    /// show.
    fn read_omp(&mut self, trigger: Trigger, now: Millis) -> Option<Task<Message>> {
        let focused = self.refresh_ui.focused;
        let omp = &mut self.account_ui.omp;
        if !omp.due(trigger, now, focused) {
            return None;
        }
        let brokered = || {
            self.account_ui
                .accounts
                .iter()
                .filter(|state| state.account.is_brokered())
        };
        if trigger == Trigger::Manual && !brokered().all(|state| state.schedule.manual_ready(now)) {
            return None;
        }
        let read = self.start_omp_read()?;
        let mut ready_at = None;
        if trigger == Trigger::Manual {
            for state in &mut self.account_ui.accounts {
                if state.account.is_brokered() {
                    state.schedule.start(trigger, now);
                    ready_at = ready_at.or(state.schedule.manual_ready_at());
                }
            }
        }
        let Some(ready_at) = ready_at else {
            return Some(read);
        };
        let wait = Duration::from_millis(ready_at.saturating_sub(now));
        Some(Task::batch([
            read,
            cosmic::task::future(async move {
                tokio::time::sleep(wait).await;
                Message::AccountUi(Action::ManualReady)
            }),
        ]))
    }

    /// Reads Omp's usage record on the blocking pool, unless there is none
    /// or a read is already under way.
    fn start_omp_read(&mut self) -> Option<Task<Message>> {
        let omp = &mut self.account_ui.omp;
        if omp.reading {
            return None;
        }
        let database = omp.database.clone()?;
        omp.reading = true;
        Some(cosmic::task::future(async move {
            let history = tokio::task::spawn_blocking(move || omp::read(&database, now_ms()))
                .await
                .unwrap_or_else(|_| {
                    History::Unreadable("Omp's usage record could not be read".into())
                });
            Message::AccountUi(Action::OmpRead(history))
        }))
    }

    /// What a running conversation's CLI reported about the account the
    /// conversation runs on, which its launch names by the home it points
    /// the CLI at.
    ///
    /// Figures refine the reading the account already has and count as a
    /// probe's would, so a busy conversation keeps its account from being
    /// probed at all, and they reach other windows through the cache. An
    /// account with no reading yet - never probed, as while no surface shows
    /// accounts - waits for its probe, and its figures go unrecorded. A
    /// changed login is not a reading of the account; it makes the account
    /// due for a probe.
    pub(super) fn account_reported(
        &mut self,
        provider: Provider,
        recipe: &LaunchRecipe,
        reports: Vec<(LiveReport, Millis)>,
    ) -> Task<Message> {
        let Some(id) = self.chat_account(provider, recipe) else {
            return Task::none();
        };
        let now = now_ms();
        self.account_ui.now_ms = now;
        let Some(state) = self.account_ui.account_mut(&id) else {
            return Task::none();
        };
        let mut observed = false;
        let mut login_changed = false;
        for (report, observed_at) in reports {
            if report.provider() != provider {
                continue;
            }
            if let LiveReport::CodexLogin(login) = &report {
                login_changed |= !login.describes(state.snapshot.as_ref(), state.identity.as_ref());
            } else if let Some(snapshot) = state.snapshot.as_mut()
                && report.apply(snapshot, observed_at, now)
            {
                state.schedule.observe(observed_at);
                observed = true;
            }
        }
        if observed {
            // A conversation reporting figures is running on a signed-in
            // account whose CLI is installed.
            state.present = true;
            state.failure = None;
            state.schedule.login_settled();
        }
        if login_changed {
            state.schedule.invalidate();
        }
        let save = if observed {
            self.save_live(id.clone())
        } else {
            Task::none()
        };
        let probe = if login_changed && self.accounts_surface_visible() {
            self.probe_account(&id, Trigger::Focus, now)
                .unwrap_or_else(Task::none)
        } else {
            Task::none()
        };
        Task::batch([save, probe])
    }

    /// Takes the models a running conversation of `provider` listed, and
    /// queues them for the cache like its figures.
    pub(super) fn observe_listed_models(
        &mut self,
        provider: Provider,
        models: Vec<crate::chat::Model>,
    ) -> Task<Message> {
        let catalog = Catalog {
            models,
            observed_at: now_ms(),
        };
        if catalog.models.is_empty() || !self.account_ui.take_catalog(provider, catalog) {
            return Task::none();
        }
        self.refresh_chat_drafts();
        if self.account_ui.cache.is_none() {
            return Task::none();
        }
        self.account_ui.catalogs_unsaved.insert(provider);
        self.schedule_live_save()
    }

    /// Queues `id`'s figures for the cache, written a little later together
    /// with whatever else arrives meanwhile.
    fn save_live(&mut self, id: AccountId) -> Task<Message> {
        if self.account_ui.cache.is_none() {
            return Task::none();
        }
        self.account_ui.live_unsaved.insert(id);
        self.schedule_live_save()
    }

    fn schedule_live_save(&mut self) -> Task<Message> {
        if std::mem::replace(&mut self.account_ui.saving_live, true) {
            return Task::none();
        }
        cosmic::task::future(async {
            tokio::time::sleep(LIVE_SAVE_DELAY).await;
            Message::AccountUi(Action::SaveLive)
        })
    }

    /// Starts the probe that was waiting for another account of `provider`.
    fn probe_queued(&mut self, provider: Provider) -> Task<Message> {
        let now = now_ms();
        let Some((id, trigger)) = self
            .account_ui
            .of(provider)
            .find_map(|account| Some((account.id.clone(), account.queued?)))
        else {
            return Task::none();
        };
        if let Some(state) = self.account_ui.account_mut(&id) {
            state.queued = None;
        }
        self.probe_account(&id, trigger, now)
            .unwrap_or_else(Task::none)
    }

    pub(super) fn update_account_ui(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Tick => self.probe_accounts(Trigger::Tick),
            Action::Loaded(loaded) => {
                let mut migration = Task::none();
                match loaded.registry {
                    Ok(mut registry) => {
                        let projects = self
                            .sidebar_model
                            .projects
                            .iter()
                            .map(|project| (project.id, project.repository.clone()))
                            .collect::<Vec<_>>();
                        if registry.migrate_project_ids(&projects) {
                            migration = self.save_registry(move |registry| {
                                registry.migrate_project_ids(&projects);
                            });
                        }
                        self.account_ui.set_registry(registry);
                        self.recover_logins();
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(format!(
                            "Your added accounts could not be read: {error}"
                        )));
                    }
                }
                self.account_ui.restore(loaded.restored);
                for (provider, catalog) in loaded.catalogs {
                    self.account_ui.take_catalog(provider, catalog);
                }
                self.refresh_chat_drafts();
                let locate = if loaded.provisional {
                    self.locate_accounts()
                } else {
                    Task::none()
                };
                Task::batch([self.probe_accounts(Trigger::Focus), locate, migration])
            }
            Action::Located(restored) => {
                self.account_ui.restore(restored);
                Task::none()
            }
            Action::Probed { id, outcome } => self.account_probed(id, outcome),
            Action::OmpRead(history) => {
                let now = now_ms();
                self.account_ui.now_ms = now;
                self.account_ui.take_history(history, now);
                Task::none()
            }
            Action::Refresh(id) => {
                let now = now_ms();
                self.account_ui.now_ms = now;
                self.probe_account(&id, Trigger::Manual, now)
                    .unwrap_or_else(Task::none)
            }
            Action::ManualReady => {
                self.account_ui.now_ms = now_ms();
                Task::none()
            }
            Action::SaveLive => {
                self.account_ui.saving_live = false;
                let entries = self.account_ui.take_live_entries();
                let catalogs = self.account_ui.take_live_catalogs();
                let Some(cache) = self
                    .account_ui
                    .cache
                    .clone()
                    .filter(|_| !entries.is_empty() || !catalogs.is_empty())
                else {
                    return Task::none();
                };
                cosmic::iced::Task::future(async move {
                    let _ =
                        tokio::task::spawn_blocking(move || store_live(&cache, entries, catalogs))
                            .await;
                })
                .discard()
            }
            Action::Popover(id) => {
                let id = id.filter(|id| self.account_ui.popover.as_ref() != Some(id));
                self.account_ui.popover = id.clone();
                let now = now_ms();
                self.account_ui.now_ms = now;
                // Opening the popover is the reader looking at the figures.
                id.and_then(|id| self.probe_account(&id, Trigger::Focus, now))
                    .unwrap_or_else(Task::none)
            }
            Action::ShowAccount(id) => {
                let Some(account) = self
                    .account_ui
                    .account(&id)
                    .map(|state| state.account.clone())
                else {
                    return Task::none();
                };
                self.account_ui.popover = Some(id.clone());
                let now = now_ms();
                self.account_ui.now_ms = now;
                let probe = self
                    .probe_account(&id, Trigger::Focus, now)
                    .unwrap_or_else(Task::none);
                let save = if account.is_brokered() {
                    // Omp chooses its own account for new conversations.
                    Task::none()
                } else {
                    self.save_registry(move |registry| registry.set_default(&account))
                };
                Task::batch([probe, save])
            }
            Action::OpenPane => {
                self.account_ui.popover = None;
                self.dismiss_chat_picker();
                let open = self.open_utility_card(CardKind::Accounts);
                Task::batch([open, self.accounts_surface_changed()])
            }
            Action::SignIn(id) => self.sign_in(id),
            Action::LoginReady {
                id,
                provider,
                program,
            } => self.open_login(&id, provider, program),
            Action::RegistryLoaded(result) => {
                if let Ok(mut registry) = result {
                    let projects = self
                        .sidebar_model
                        .projects
                        .iter()
                        .map(|project| (project.id, project.repository.clone()))
                        .collect::<Vec<_>>();
                    let migrate = registry.migrate_project_ids(&projects);
                    let before = self.account_ui.accounts.len();
                    self.account_ui.set_registry(registry);
                    let probe = if self.account_ui.accounts.len() != before {
                        self.probe_accounts(Trigger::Focus)
                    } else {
                        Task::none()
                    };
                    let save = if migrate {
                        self.save_registry(move |registry| {
                            registry.migrate_project_ids(&projects);
                        })
                    } else {
                        Task::none()
                    };
                    return Task::batch([probe, save]);
                }
                Task::none()
            }
            Action::Saved(result) => {
                match result {
                    Ok(registry) => self.account_ui.set_registry(registry),
                    Err(error) => {
                        self.status = Some(TransientNotice::error(format!(
                            "The change to your accounts could not be saved: {error}"
                        )));
                    }
                }
                Task::none()
            }
            Action::Add { provider, device } => self.add_account(provider, device),
            Action::LinkStart(provider) => {
                self.account_ui.table.add_menu = false;
                self.account_ui.table.link = Some((provider, String::new()));
                widget::text_input::focus(table::link_id())
            }
            Action::LinkInput(path) => {
                if !self.account_ui.table.linking {
                    if let Some((_, draft)) = &mut self.account_ui.table.link {
                        *draft = path;
                    }
                }
                Task::none()
            }
            Action::LinkCommit => self.link_account_home(),
            Action::LinkCancel => {
                if !self.account_ui.table.linking {
                    self.account_ui.table.link = None;
                }
                Task::none()
            }
            Action::LinkReady { key, result } => {
                self.account_ui.table.linking = false;
                match result {
                    Ok(registry) => {
                        self.account_ui.set_registry(registry);
                        self.account_ui.table.link = None;
                        self.status =
                            Some(TransientNotice::success("Existing account home added."));
                        let id = self
                            .account_ui
                            .accounts
                            .iter()
                            .find(|state| state.account.registry_key() == Some(key))
                            .map(|state| state.id.clone());
                        id.and_then(|id| self.probe_account(&id, Trigger::Manual, now_ms()))
                            .unwrap_or_else(Task::none)
                    }
                    Err(error) => {
                        self.status = Some(TransientNotice::error(format!(
                            "Could not use that account home: {error}"
                        )));
                        Task::none()
                    }
                }
            }
            Action::HomeReady {
                provider,
                device,
                result,
            } => self.open_account_login(provider, device, result),
            Action::LoginTick => self.check_logins(),
            Action::LoginCallbackBlocked(key) => {
                if let Some(login) = self
                    .account_ui
                    .logins
                    .iter_mut()
                    .find(|login| login.key == key)
                    && login.phase == LoginPhase::Verifying
                {
                    login.phase = LoginPhase::CallbackBlocked;
                }
                Task::none()
            }
            Action::LoginsEnded(result) => {
                self.account_ui.asking_host = false;
                match result {
                    Ok(mut ended) => {
                        // A sign-in settled, being asked, or restarted in
                        // another terminal while the daemon was answering has
                        // moved on from the terminal the answer is about.
                        ended.retain(|(key, terminal)| {
                            self.account_ui.logins.iter().any(|login| {
                                login.key == *key
                                    && login.phase == LoginPhase::Waiting
                                    && self
                                        .sign_in_card(login.provider, &login.home)
                                        .and_then(|card| sign_in_terminal(self.card_state(card)))
                                        .is_none_or(|current| current == *terminal)
                            })
                        });
                        Task::batch(
                            ended
                                .into_iter()
                                .map(|(key, _)| self.verify_login(key))
                                .collect::<Vec<_>>(),
                        )
                    }
                    // The next tick asks again.
                    Err(error) => {
                        tracing::debug!(%error, "the daemon could not say which sign-ins ended");
                        Task::none()
                    }
                }
            }
            Action::OffstripClosed { card, result } => match result {
                // With its card gone, the sign-in counts as ended.
                Ok(()) => Task::batch([self.retire_offstrip_card(card), self.check_logins()]),
                Err(error) => {
                    self.status = Some(TransientNotice::error(format!(
                        "The sign-in could not be stopped: {error}"
                    )));
                    Task::none()
                }
            },
            Action::LoginVerified { key, outcome } => self.login_verified(key, outcome),
            Action::LoginSettled { key, result } => self.login_settled(key, result),
            Action::CancelLogin(key) => {
                let Some(login) = self.account_ui.logins.iter().find(|login| login.key == key)
                else {
                    return Task::none();
                };
                if login.phase == LoginPhase::CallbackBlocked {
                    return self.discard_home(key, login.provider);
                }
                match self.sign_in_card(login.provider, &login.home) {
                    // Closing the terminal ends the login; the next check
                    // finds it ended and deletes the home. Cancelling is
                    // itself the answer to whether to stop it.
                    Some(card) if self.workspace.order().any(|id| id == card) => {
                        self.close_card_unasked(card)
                    }
                    // Its strip is not on screen, so there is no card close
                    // to run: the daemon stops the login and the card is
                    // retired, which the next check takes as the end.
                    Some(card) => self.close_offstrip_sign_in(card),
                    None => self.check_logins(),
                }
            }
            Action::SetDefault(id) => {
                self.account_ui.table.close_popups();
                let Some(account) = self
                    .account_ui
                    .account(&id)
                    .map(|state| state.account.clone())
                else {
                    return Task::none();
                };
                self.save_registry(move |registry| registry.set_default(&account))
            }
            Action::SetProjectDefault(project, id) => {
                self.account_ui.table.close_popups();
                let Some(repository) = self
                    .sidebar_model
                    .project(project)
                    .map(|row| row.repository.clone())
                else {
                    return Task::none();
                };
                let Some(account) = self
                    .account_ui
                    .account(&id)
                    .map(|state| state.account.clone())
                else {
                    return Task::none();
                };
                self.save_registry(move |registry| {
                    registry.set_project_default(&repository, &account)
                })
            }
            Action::ClearProjectDefault(project, provider) => {
                self.account_ui.table.close_popups();
                let Some(repository) = self
                    .sidebar_model
                    .project(project)
                    .map(|row| row.repository.clone())
                else {
                    return Task::none();
                };
                self.save_registry(move |registry| {
                    registry.clear_project_default(&repository, provider)
                })
            }
            Action::Rename(id) => {
                self.account_ui.table.menu = None;
                let Some(state) = self
                    .account_ui
                    .account(&id)
                    // Omp names its own accounts.
                    .filter(|state| !state.account.is_brokered())
                else {
                    return Task::none();
                };
                let current = self
                    .account_ui
                    .registry
                    .label(&state.account)
                    .map(str::to_owned)
                    .unwrap_or_default();
                self.account_ui.table.rename = Some((id, current));
                iced::widget::operation::focus(table::rename_id()).map(cosmic::Action::App)
            }
            Action::RenameInput(text) => {
                if let Some((_, draft)) = self.account_ui.table.rename.as_mut() {
                    *draft = text.chars().take(account::LABEL_LIMIT * 2).collect();
                }
                Task::none()
            }
            Action::RenameCommit => {
                let Some((id, draft)) = self.account_ui.table.rename.take() else {
                    return Task::none();
                };
                let Some(account) = self
                    .account_ui
                    .account(&id)
                    .map(|state| state.account.clone())
                else {
                    return Task::none();
                };
                self.save_registry(move |registry| registry.set_label(&account, Some(draft)))
            }
            Action::RenameCancel => {
                self.account_ui.table.rename = None;
                Task::none()
            }
            Action::Remove(id) => {
                if self
                    .account_ui
                    .account(&id)
                    .is_some_and(|state| state.account.registry_key().is_some())
                {
                    self.account_ui.table.confirm_in_use = self.cards_on_account(&id);
                    self.account_ui.table.menu = Some(id.clone());
                    self.account_ui.table.confirm_remove = Some(id);
                }
                Task::none()
            }
            Action::RemoveCancel => {
                self.account_ui.table.confirm_remove = None;
                Task::none()
            }
            Action::RemoveConfirm(id) => self.remove_account(id),
            Action::Removed {
                id,
                registry,
                error,
            } => {
                self.account_ui.removing.remove(&id);
                if let Some(registry) = registry {
                    self.account_ui.set_registry(registry);
                }
                if let Some(error) = error {
                    self.status = Some(TransientNotice::error(format!(
                        "The account could not be removed completely: {error}"
                    )));
                }
                Task::none()
            }
            Action::Menu(id) => {
                let id = id.filter(|id| self.account_ui.table.menu.as_ref() != Some(id));
                self.account_ui.table.menu = id;
                self.account_ui.table.confirm_remove = None;
                self.account_ui.table.add_menu = false;
                Task::none()
            }
            Action::OwnCodexConfig(own) => {
                self.account_ui.table.own_codex_config = own;
                Task::none()
            }
            Action::AddMenu(open) => {
                self.account_ui.table.add_menu = open;
                if open {
                    self.account_ui.table.menu = None;
                }
                Task::none()
            }
            Action::Sort(sort) => {
                self.account_ui.table.sort = sort;
                Task::none()
            }
        }
    }

    fn account_probed(&mut self, id: AccountId, mut outcome: Outcome) -> Task<Message> {
        let now = now_ms();
        self.account_ui.now_ms = now;
        // The probe cached what its CLI listed itself.
        if let Outcome::Answered { catalog, .. } = &mut outcome
            && let Some(catalog) = catalog.take()
            && let Some(state) = self.account_ui.account(&id)
        {
            let provider = state.account.provider;
            if self.account_ui.take_catalog(provider, catalog) {
                self.refresh_chat_drafts();
            }
        }
        let Some(state) = self.account_ui.account_mut(&id) else {
            return Task::none();
        };
        let provider = state.account.provider;
        let mut learned = None;
        match outcome {
            // Another window's cached answer can be older than what this
            // window already shows, and then says nothing new.
            Outcome::Answered { snapshot, .. }
                if snapshot.source == UsageSource::Cache
                    && state
                        .snapshot
                        .as_ref()
                        .is_some_and(|current| snapshot.observed_at < current.observed_at) =>
            {
                state.schedule.abandon();
            }
            // A probe's own answer is always the newest.
            Outcome::Answered {
                identity, snapshot, ..
            } => {
                state.schedule.succeed(snapshot.observed_at);
                if snapshot.status != UsageStatus::NeedsLogin {
                    state.schedule.login_settled();
                }
                state.present =
                    snapshot.status != UsageStatus::Unavailable || !state.account.is_system();
                // A signed-out answer, or an identity lookup that failed this
                // time, names nobody; the account last seen is still the one
                // it is, or the one to sign back in to.
                if identity.is_some() || snapshot.status == UsageStatus::Unavailable {
                    state.identity = identity;
                }
                if let (Some(key), Some(identity)) =
                    (state.account.registry_key(), state.identity.as_ref())
                {
                    learned = Some((key, identity.clone()));
                }
                state.snapshot = Some(snapshot);
                state.failure = None;
            }
            Outcome::Failed(message) => {
                state.schedule.fail(now);
                state.failure = Some(message);
            }
            Outcome::Busy => state.schedule.abandon(),
        }
        let present = state.present;
        if !present && self.account_ui.popover.as_ref() == Some(&id) {
            self.account_ui.popover = None;
        }
        // A managed account's address is remembered, so the next sign-in can
        // be told apart from it before this window has probed it.
        let remember = learned
            .filter(|(key, identity)| {
                self.account_ui.registry.find(*key).is_some_and(|managed| {
                    identity.email.is_some() && managed.email != identity.email
                        || identity.organization.is_some()
                            && managed.organization != identity.organization
                })
            })
            .map(|(key, identity)| {
                self.save_registry(move |registry| {
                    registry.observe_identity(key, &identity);
                })
            })
            .unwrap_or_else(Task::none);
        Task::batch([remember, self.probe_queued(provider)])
    }

    /// A synchronous fence covering discovery and terminals awaiting spawn.
    fn codex_browser_busy(&self) -> bool {
        self.account_ui.browser_preparing || self.durable.cards.iter().any(|card| {
            card.existence.is_open()
                && !matches!(self.cards.get(&card.id), Some(CardRuntime::Failed(_)))
                && matches!(&card.kind, CardKind::Terminal(session)
                    if account::login::is_codex_browser(&session.launch) && !terminal_ended(session))
        })
    }

    /// Finds the CLI to sign in with off the interface thread, as probes do.
    fn sign_in(&mut self, id: AccountId) -> Task<Message> {
        let Some(account) = self
            .account_ui
            .account(&id)
            .map(|state| state.account.clone())
        else {
            return Task::none();
        };
        if account.provider == Provider::Codex {
            if self.codex_browser_busy() {
                self.status = Some(TransientNotice::warning(account::login::BUSY));
                return Task::none();
            }
            self.account_ui.browser_preparing = true;
        }
        self.account_ui.popover = None;
        self.account_ui.table.close_popups();
        let provider = account.provider;
        cosmic::task::future(async move {
            let search_path = probe::search_path().await;
            let program = tokio::task::spawn_blocking(move || {
                if account.provider == Provider::Codex {
                    browser_login_available()?;
                }
                probe::find_executable(account.provider.executable(), search_path.as_deref())
                    .ok_or_else(|| {
                        format!("{} is not installed", probe::cli_name(account.provider))
                    })
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::AccountUi(Action::LoginReady {
                id,
                provider,
                program,
            })
        })
    }

    fn open_login(
        &mut self,
        id: &AccountId,
        provider: Provider,
        program: Result<PathBuf, String>,
    ) -> Task<Message> {
        if provider == Provider::Codex {
            self.account_ui.browser_preparing = false;
        }
        let program = match program {
            Ok(program) => Some(program),
            Err(error) => {
                self.status = Some(TransientNotice::warning(error));
                return Task::none();
            }
        };
        let Some(state) = self.account_ui.account(id) else {
            return Task::none();
        };
        let launch = program
            .zip(std::env::var_os("HOME"))
            .and_then(|(program, home)| {
                login_launch(&state.account, &program, PathBuf::from(home), false)
            });
        let Some(launch) = launch else {
            self.status = Some(TransientNotice::error(format!(
                "{} could not be started to sign in.",
                state.name()
            )));
            return Task::none();
        };
        let title = format!("Sign in to {}", state.name());
        let now = now_ms();
        if let Some(state) = self.account_ui.account_mut(id) {
            state.schedule.await_login(now);
        }
        self.open_login_terminal(launch, title).0
    }

    /// Opens a terminal running a provider's login, which keeps no
    /// transcript (see [`is_sign_in`]), and names it for what it is doing.
    fn open_login_terminal(
        &mut self,
        launch: LaunchRecipe,
        title: String,
    ) -> (Task<Message>, TerminalId) {
        // The login runs from the home directory, which no worktree owns, so
        // the next scope rebuild would file the card under that directory and
        // take it out of the strip it was opened in. It is filed with the
        // workspace the reader is looking at instead, as an adopted terminal
        // would be.
        let filing = self
            .worktree_scope_for(&launch.working_directory)
            .is_none()
            .then(|| self.active_workspace.clone())
            .flatten();
        let session = fresh_terminal_session(launch);
        let terminal = session.id;
        let task = self.insert_terminal_card(session, None);
        let card = self
            .durable
            .cards
            .iter()
            .find(
                |card| matches!(&card.kind, CardKind::Terminal(session) if session.id == terminal),
            )
            .map(|card| card.id);
        if let Some(card) = card {
            if let Some(state) = self.card_state_mut(card) {
                state.title = title;
            }
            if let Some(scope) = filing {
                Arc::make_mut(&mut self.durable)
                    .card_scopes
                    .insert(card, scope);
            }
        }
        (task, terminal)
    }

    /// The open card running a sign-in to `home`, if any.
    fn sign_in_card(&self, provider: Provider, home: &Path) -> Option<CardId> {
        let variable = account::home_variable(provider)?;
        self.durable
            .cards
            .iter()
            .find(|card| {
                card.existence.is_open()
                    && matches!(&card.kind, CardKind::Terminal(session)
                        if is_sign_in(&session.launch)
                            && session
                                .launch
                                .environment
                                .get(variable)
                                .is_some_and(|value| Path::new(value) == home))
            })
            .map(|card| card.id)
    }

    /// Takes up the sign-ins an earlier run left pending: one whose terminal
    /// this window restored carries on as if nothing happened, and one old
    /// enough that no window can still be running it is asked who it signed
    /// in as, which registers or deletes it.
    fn recover_logins(&mut self) {
        let Some(homes) = self.account_ui.homes.clone() else {
            return;
        };
        let now = now_ms();
        let pending: Vec<registry::Pending> = self.account_ui.registry.pending().to_vec();
        for pending in pending {
            if self
                .account_ui
                .logins
                .iter()
                .any(|login| login.key == pending.key)
            {
                continue;
            }
            let home = homes.path(pending.provider, pending.key);
            let restored = self.sign_in_card(pending.provider, &home).is_some();
            if !restored && now.saturating_sub(pending.started_at) < RECOVER_AFTER {
                continue;
            }
            self.account_ui.logins.push(Login {
                provider: pending.provider,
                key: pending.key,
                home,
                phase: LoginPhase::Waiting,
                attempts: 0,
                next_check: 0,
                started_at: pending.started_at,
                recovered: !restored,
            });
        }
    }

    /// Links an existing provider home without copying, signing out, or
    /// taking ownership of any files in it. Validation and the registry write
    /// run off the interface thread.
    fn link_account_home(&mut self) -> Task<Message> {
        let Some((provider, raw_path)) = self.account_ui.table.link.clone() else {
            return Task::none();
        };
        if self.account_ui.table.linking {
            return Task::none();
        }
        self.account_ui.table.linking = true;
        let key = uuid::Uuid::new_v4();
        let store = self.account_ui.store.clone();
        let homes = self.account_ui.homes.clone();
        let fallback = self.account_ui.registry.clone();
        let system_home = Account::system(provider).resolved_home();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let raw_path = raw_path.trim();
                let _lifecycle = homes
                    .as_ref()
                    .map(Homes::lifecycle_lock)
                    .transpose()
                    .map_err(|error| error.to_string())?;
                let chosen = if let Some(relative) = raw_path.strip_prefix("~/") {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .ok_or("HOME is unavailable")?
                        .join(relative)
                } else {
                    PathBuf::from(raw_path)
                };
                if !chosen.is_absolute() {
                    return Err("Enter an absolute path or one starting with ~/".into());
                }
                let path = std::fs::canonicalize(&chosen)
                    .map_err(|error| format!("{}: {error}", chosen.display()))?;
                crate::secure_file::validate_owned_directory(&path)
                    .map_err(|error| error.to_string())?;
                let change = |registry: &mut Registry| {
                    registry.link(
                        provider,
                        key,
                        path,
                        system_home.as_deref(),
                        homes.as_ref(),
                        now_ms(),
                    )
                };
                match store {
                    Some(store) => store
                        .update(change)
                        .map_err(|error| error.to_string())
                        .and_then(|(registry, result)| result.map(|()| registry)),
                    None => {
                        let mut registry = fallback;
                        change(&mut registry)?;
                        Ok(registry)
                    }
                }
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::AccountUi(Action::LinkReady { key, result })
        })
    }

    /// Creates the new account's home off the interface thread, with the
    /// reader's shared entries linked in, and finds its CLI.
    fn add_account(&mut self, provider: Provider, device: bool) -> Task<Message> {
        let codex_config = if self.account_ui.table.own_codex_config {
            account::home::CodexConfig::Copy
        } else {
            account::home::CodexConfig::Shared
        };
        self.account_ui.table.close_popups();
        self.account_ui.popover = None;
        if login_arguments(provider, device).is_none() {
            return Task::none();
        }
        let Some(homes) = self.account_ui.homes.clone() else {
            self.status = Some(TransientNotice::error(
                "Accounts cannot be added: this system offers no data directory.",
            ));
            return Task::none();
        };
        if provider == Provider::Codex && !device {
            if self.codex_browser_busy() {
                self.status = Some(TransientNotice::warning(account::login::BUSY));
                return Task::none();
            }
            self.account_ui.browser_preparing = true;
        }
        let store = self.account_ui.store.clone();
        cosmic::task::future(async move {
            let search_path = probe::search_path().await;
            let result = tokio::task::spawn_blocking(move || {
                if provider == Provider::Codex && !device {
                    browser_login_available()?;
                }
                let program = probe::find_executable(provider.executable(), search_path.as_deref())
                    .ok_or_else(|| format!("{} is not installed", probe::cli_name(provider)))?;
                let system = Account::system(provider).resolved_home();
                let create = || homes.create_with_config(provider, system.as_deref(), codex_config);
                let (key, home) = match &store {
                    Some(store) => store.create_pending(&homes, provider, now_ms(), create),
                    None => create(),
                }
                .map_err(|error| format!("Its home could not be created and recorded: {error}"))?;
                Ok((key, home, program))
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::AccountUi(Action::HomeReady {
                provider,
                device,
                result,
            })
        })
    }

    fn open_account_login(
        &mut self,
        provider: Provider,
        device: bool,
        result: Result<(uuid::Uuid, PathBuf, PathBuf), String>,
    ) -> Task<Message> {
        if provider == Provider::Codex && !device {
            self.account_ui.browser_preparing = false;
        }
        let (key, home, program) = match result {
            Ok(ready) => ready,
            Err(error) => {
                self.status = Some(TransientNotice::error(format!(
                    "A {} account could not be added: {error}",
                    provider.label()
                )));
                return Task::none();
            }
        };
        let account = Account::managed(provider, key, home.clone());
        let launch = std::env::var_os("HOME").and_then(|reader_home| {
            login_launch(&account, &program, PathBuf::from(reader_home), device)
        });
        let Some(launch) = launch else {
            self.status = Some(TransientNotice::error(format!(
                "{} could not be started to sign in.",
                probe::cli_name(provider)
            )));
            return self.discard_home(key, provider);
        };
        let (task, _) =
            self.open_login_terminal(launch, format!("Add a {} account", provider.label()));
        let now = now_ms();
        self.account_ui.logins.push(Login {
            provider,
            key,
            home,
            phase: LoginPhase::Waiting,
            attempts: 0,
            next_check: 0,
            started_at: now,
            recovered: false,
        });
        task
    }

    /// Deletes a home that never became an account, and its pending entry.
    fn discard_home(&self, key: uuid::Uuid, provider: Provider) -> Task<Message> {
        let homes = self.account_ui.homes.clone();
        let store = self.account_ui.store.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || -> Result<Settled, String> {
                abandon_home(store.as_ref(), homes.as_ref(), provider, key).map(Settled::Abandoned)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::AccountUi(Action::LoginSettled { key, result })
        })
    }

    /// Asks the home of every sign-in whose terminal has ended - or whose
    /// card was closed - who it is signed in as.
    fn check_logins(&mut self) -> Task<Message> {
        let now = now_ms();
        let mut ended = Vec::new();
        let mut unwatched = Vec::new();
        for login in self
            .account_ui
            .logins
            .iter()
            .filter(|login| login.phase == LoginPhase::Waiting && login.next_check <= now)
        {
            let Some(card) = self.sign_in_card(login.provider, &login.home) else {
                ended.push(login.key);
                continue;
            };
            let on_screen = self.workspace.order().any(|id| id == card);
            match self.cards.get(&card) {
                Some(CardRuntime::Failed(_)) => ended.push(login.key),
                // A card on the strip on screen follows its terminal.
                Some(CardRuntime::Terminal(_) | CardRuntime::Completed) if on_screen => {
                    if ended_terminal(self.card_state(card)).is_some() {
                        ended.push(login.key);
                    }
                }
                // A card off the strip hibernates, and one not hydrated yet
                // has not started watching: either keeps whatever it last saw.
                // What it saw end has ended; otherwise the daemon is asked -
                // once it has taken the terminal on, since until then it has
                // nothing to say about it.
                _ => {
                    if ended_terminal(self.card_state(card)).is_some() {
                        ended.push(login.key);
                    } else if let Some(CardState {
                        kind: CardKind::Terminal(session),
                        ..
                    }) = self.card_state(card)
                        && let Some(proof) = session.continuity.live_proof()
                    {
                        unwatched.push((login.key, session.id, proof));
                    }
                }
            }
        }
        let ask = if unwatched.is_empty()
            || self.account_ui.asking_host
            || self.account_ui.next_host_check > now
        {
            Task::none()
        } else {
            self.account_ui.asking_host = true;
            self.account_ui.next_host_check = now + HOST_CHECK;
            cosmic::task::future(async move {
                let result = tokio::task::spawn_blocking(move || ended_on_host(&unwatched))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
                Message::AccountUi(Action::LoginsEnded(result))
            })
        };
        let verify = ended
            .into_iter()
            .map(|key| self.verify_login(key))
            .collect::<Vec<_>>();
        Task::batch(verify.into_iter().chain([ask]))
    }

    /// Closes a sign-in card that is not on the strip on screen, which has
    /// no card close to run: the daemon closes its terminal - a no-op for
    /// one that has ended - and the card is then retired.
    fn close_offstrip_sign_in(&mut self, card: CardId) -> Task<Message> {
        let hosted = match self.card_state(card) {
            Some(CardState {
                kind: CardKind::Terminal(session),
                ..
            }) => session
                .continuity
                .live_proof()
                .map(|proof| (session.id, proof)),
            _ => None,
        };
        let Some((terminal, proof)) = hosted else {
            // Ended already, or never taken on by the daemon: nothing runs.
            return Task::batch([self.retire_offstrip_card(card), self.check_logins()]);
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || stop_hosted(terminal, &proof))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::AccountUi(Action::OffstripClosed { card, result })
        })
    }

    /// Closes the card of a sign-in that has settled: its terminal has said
    /// all it had to.
    fn close_settled_sign_in(&mut self, provider: Provider, home: &Path) -> Task<Message> {
        match self.sign_in_card(provider, home) {
            Some(card) if !self.workspace.order().any(|id| id == card) => {
                self.close_offstrip_sign_in(card)
            }
            Some(card)
                if ended_terminal(self.card_state(card)).is_some()
                    || matches!(self.cards.get(&card), Some(CardRuntime::Failed(_))) =>
            {
                cosmic::task::message(Message::CloseCard(card))
            }
            _ => Task::none(),
        }
    }

    fn verify_login(&mut self, key: uuid::Uuid) -> Task<Message> {
        let browser_terminal = self.account_ui.logins.iter()
            .find(|login| login.key == key)
            .and_then(|login| self.sign_in_card(login.provider, &login.home))
            .and_then(|card| match self.card_state(card).map(|state| &state.kind) {
                Some(CardKind::Terminal(session)) if account::login::is_codex_browser(&session.launch) => {
                    let blocked = matches!(self.cards.get(&card), Some(CardRuntime::Failed(error)) if account::login::callback_failed(error));
                    Some((session.id, blocked))
                }
                _ => None,
            });
        let Some(login) = self
            .account_ui
            .logins
            .iter_mut()
            .find(|login| login.key == key)
        else {
            return Task::none();
        };
        login.phase = LoginPhase::Verifying;
        if browser_terminal.is_some_and(|(_, blocked)| blocked) {
            login.phase = LoginPhase::CallbackBlocked;
            return Task::none();
        }
        login.attempts += 1;
        let account = login.account();
        cosmic::task::future(async move {
            if let Some((terminal, _)) = browser_terminal {
                let blocked =
                    tokio::task::spawn_blocking(move || callback_failed_on_host(terminal))
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                match blocked {
                    Ok(true) => return Message::AccountUi(Action::LoginCallbackBlocked(key)),
                    Ok(false) => {}
                    Err(error) => {
                        return Message::AccountUi(Action::LoginVerified {
                            key,
                            outcome: Outcome::Failed(error),
                        });
                    }
                }
            }
            // No cache: the answer is about a home no other window knows.
            let outcome = probe::run(account, None, true).await;
            Message::AccountUi(Action::LoginVerified { key, outcome })
        })
    }

    fn login_verified(&mut self, key: uuid::Uuid, outcome: Outcome) -> Task<Message> {
        let now = now_ms();
        let Some(index) = self
            .account_ui
            .logins
            .iter()
            .position(|login| login.key == key)
        else {
            return Task::none();
        };
        let login = &mut self.account_ui.logins[index];
        let (identity, snapshot) = match outcome {
            Outcome::Answered {
                identity, snapshot, ..
            } => (identity, snapshot),
            // A CLI that cannot say who it signed in as right now is asked
            // again a little later. A login that cannot be confirmed at all
            // is neither added nor thrown away: it stays pending, and the
            // next start asks again.
            Outcome::Failed(_) | Outcome::Busy => {
                if login.attempts < LOGIN_ATTEMPTS {
                    login.phase = LoginPhase::Waiting;
                    login.next_check = now + LOGIN_RETRY * Millis::from(login.attempts);
                    return Task::none();
                }
                let login = self.account_ui.logins.remove(index);
                if !login.recovered {
                    self.status = Some(TransientNotice::warning(format!(
                        "{} could not say which account signed in. cagents will check again the next time it starts.",
                        probe::cli_name(login.provider)
                    )));
                }
                return Task::none();
            }
        };
        let signed_in = identity.is_some()
            || matches!(
                snapshot.status,
                UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited
            );
        let provider = login.provider;
        let account = login.account();
        if !signed_in {
            // A sign-in picked up from an earlier run may still be under way
            // in another window until it is old enough not to be.
            if login.recovered
                && now.saturating_sub(login.started_at)
                    < crate::account::home::ORPHAN_AGE.as_millis() as Millis
            {
                self.account_ui.logins.remove(index);
                return Task::none();
            }
            login.phase = LoginPhase::Settling;
            return self.discard_home(key, provider);
        }
        login.phase = LoginPhase::Settling;
        let managed = registry::Registered {
            key,
            provider,
            linked_home: None,
            label: None,
            email: identity
                .as_ref()
                .and_then(|identity| identity.email.clone()),
            organization: identity
                .as_ref()
                .and_then(|identity| identity.organization.clone()),
            added_at: now,
        };
        let system = self.account_ui.system_identities();
        let store = self.account_ui.store.clone();
        let homes = self.account_ui.homes.clone();
        let fallback = self.account_ui.registry.clone();
        cosmic::task::future(async move {
            let registration_homes = homes.clone();
            let registered = match store.clone() {
                Some(store) => tokio::task::spawn_blocking(move || match registration_homes {
                    Some(homes) => store.register_pending(&homes, managed, &system),
                    None => store.update(|registry| registry.register(managed, &system)),
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string())),
                None => {
                    let mut registry = fallback;
                    let result = registry.register(managed, &system);
                    Ok((registry, result))
                }
            };
            let result = match registered {
                Ok((registry, Ok(()))) => Ok(Settled::Registered {
                    registry,
                    identity,
                    snapshot: Some(snapshot),
                }),
                Ok((_, Err(existing))) => {
                    // The same login twice would be one allowance counted
                    // twice. The new home is signed out - its login is its
                    // own, so the existing one is unaffected - and deleted.
                    let _ = probe::logout(account).await;
                    tokio::task::spawn_blocking(move || -> Result<Settled, String> {
                        Ok(Settled::Duplicate {
                            existing,
                            registry: abandon_home(store.as_ref(), homes.as_ref(), provider, key)?,
                        })
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result)
                }
                Err(error) => Err(error),
            };
            Message::AccountUi(Action::LoginSettled { key, result })
        })
    }

    fn login_settled(&mut self, key: uuid::Uuid, result: Result<Settled, String>) -> Task<Message> {
        let Some(index) = self
            .account_ui
            .logins
            .iter()
            .position(|login| login.key == key)
        else {
            return Task::none();
        };
        let login = self.account_ui.logins.remove(index);
        match result {
            Ok(Settled::Registered {
                registry,
                identity,
                snapshot,
            }) => {
                self.account_ui.set_registry(registry);
                let id = login.account().id();
                let now = now_ms();
                let name = identity
                    .as_ref()
                    .and_then(|identity| identity.email.clone())
                    .unwrap_or_else(|| format!("A {} account", login.provider.label()));
                if let Some(state) = self.account_ui.account_mut(&id) {
                    state.present = true;
                    if let Some(snapshot) = snapshot {
                        state.schedule.start(Trigger::Manual, now);
                        state.schedule.succeed(snapshot.observed_at);
                        state.snapshot = Some(snapshot);
                    }
                    state.identity = identity;
                }
                self.status = Some(TransientNotice::success(format!("{name} was added.")));
                self.close_settled_sign_in(login.provider, &login.home)
            }
            Ok(Settled::Duplicate { existing, registry }) => {
                if let Some(registry) = registry {
                    self.account_ui.set_registry(registry);
                }
                let name = self
                    .account_ui
                    .account(&existing)
                    .map(|state| self.account_ui.display_name(state))
                    .unwrap_or_else(|| "an account you already have".into());
                self.status = Some(TransientNotice::warning(format!(
                    "That login is already added as {name}."
                )));
                self.close_settled_sign_in(login.provider, &login.home)
            }
            Ok(Settled::Abandoned(registry)) => {
                if let Some(registry) = registry {
                    self.account_ui.set_registry(registry);
                }
                self.close_settled_sign_in(login.provider, &login.home)
            }
            Err(error) => {
                self.status = Some(TransientNotice::error(format!(
                    "The new {} account could not be saved: {error}",
                    login.provider.label()
                )));
                Task::none()
            }
        }
    }

    /// Forgets a linked home without touching it. A home cagents created is
    /// signed out and deleted after it is removed from the registry.
    fn remove_account(&mut self, id: AccountId) -> Task<Message> {
        self.account_ui.table.close_popups();
        let Some(account) = self
            .account_ui
            .account(&id)
            .map(|state| state.account.clone())
        else {
            return Task::none();
        };
        let Some(key) = account.registry_key() else {
            return Task::none();
        };
        let linked = account.is_linked();
        if !self.account_ui.removing.insert(id.clone()) {
            return Task::none();
        }
        let store = self.account_ui.store.clone();
        let homes = self.account_ui.homes.clone();
        let fallback = self.account_ui.registry.clone();
        let provider = account.provider;
        cosmic::task::future(async move {
            if !linked {
                let _ = probe::logout(account).await;
            }
            let (registry, error) = tokio::task::spawn_blocking(move || {
                if !linked && let (Some(store), Some(homes)) = (&store, &homes) {
                    return match store.remove_home(homes, provider, key, true) {
                        Ok((registry, error)) => (Some(registry), error),
                        Err(error) => (None, Some(error.to_string())),
                    };
                }
                let registry = match store {
                    Some(store) => match store.update(|registry| {
                        registry.unregister(key);
                    }) {
                        Ok((registry, ())) => registry,
                        Err(error) => return (None, Some(error.to_string())),
                    },
                    None => {
                        let mut registry = fallback;
                        registry.unregister(key);
                        registry
                    }
                };
                // Once forgotten, the home is no account's: a deletion that
                // fails here leaves files, never a broken account.
                let error = (!linked).then_some(homes).flatten().and_then(|homes| {
                    homes
                        .remove(provider, key)
                        .err()
                        .map(|error| format!("its files could not be deleted: {error}"))
                });
                (Some(registry), error)
            })
            .await
            .unwrap_or_else(|error| (None, Some(error.to_string())));
            Message::AccountUi(Action::Removed {
                id,
                registry,
                error,
            })
        })
    }

    /// The account `provider` starts new work on outside a project.
    pub(super) fn default_account(&self, provider: Provider) -> Account {
        self.account_ui
            .registry
            .default_account(provider, self.account_ui.homes.as_ref())
    }

    /// The longest known worktree or repository root containing `directory`
    /// determines its project. This is a short in-memory scan only when
    /// starting work or drawing the Accounts pane; no filesystem query is needed.
    fn project_for_directory(&self, directory: &Path) -> Option<(ProjectId, &str, &Path)> {
        self.sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                std::iter::once(project.repository.as_path())
                    .chain(
                        project.repository.parent().filter(|parent| {
                            sidebar::is_primary_worktree(&project.repository, parent)
                        }),
                    )
                    .chain(
                        project
                            .worktrees
                            .iter()
                            .map(|worktree| worktree.path.as_path()),
                    )
                    .filter_map(move |root| {
                        directory
                            .starts_with(root)
                            .then_some((root.components().count(), project))
                    })
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, project)| {
                (
                    project.id,
                    project.name.as_str(),
                    project.repository.as_path(),
                )
            })
    }

    /// A channel owns its project even when its displayed workspace path is
    /// unavailable or has changed. The Accounts pane uses the selected scope,
    /// then falls back to the worktree path when no channel is selected.
    pub(super) fn accounts_project(&self) -> Option<(ProjectId, &str, &Path)> {
        if let Some(channel) = self.durable.active_channel
            && let Some(project_id) = self
                .durable
                .channels
                .iter()
                .find(|entry| entry.id == channel)
                .map(|entry| entry.project_id)
            && let Some(project) = self.sidebar_model.project(project_id)
        {
            return Some((
                project.id,
                project.name.as_str(),
                project.repository.as_path(),
            ));
        }
        self.active_workspace
            .as_deref()
            .and_then(|directory| self.project_for_directory(directory))
    }

    pub(super) fn default_account_for_directory(
        &self,
        provider: Provider,
        directory: &Path,
    ) -> Account {
        let Some((_, _, repository)) = self.project_for_directory(directory) else {
            return self.default_account(provider);
        };
        self.account_ui.registry.project_default_account(
            repository,
            provider,
            self.account_ui.homes.as_ref(),
        )
    }

    /// The account named `id`, if it is still known.
    pub(super) fn known_account(&self, id: &AccountId) -> Option<Account> {
        self.account_ui
            .account(id)
            .map(|state| state.account.clone())
    }

    /// The account a conversation launched as `recipe` runs on, if it is
    /// one cagents knows.
    pub(super) fn chat_account(
        &self,
        provider: Provider,
        recipe: &LaunchRecipe,
    ) -> Option<AccountId> {
        account_of_recipe(&self.account_ui.accounts, provider, recipe).map(|state| state.id.clone())
    }

    /// The accounts a conversation launched as `recipe` could run on, for
    /// its composer's picker, or `None` where there is nothing to choose
    /// between and nothing worth naming: one account, and it is the system
    /// default.
    pub(super) fn chat_accounts(
        &self,
        provider: Provider,
        recipe: &LaunchRecipe,
    ) -> Option<ChatAccounts> {
        // Only a provider with accounts cagents can tell apart has a choice.
        if !Account::PROVIDERS.contains(&provider) {
            return None;
        }
        let state = &self.account_ui;
        let current = account_of_recipe(&state.accounts, provider, recipe);
        let choices: Vec<ChatAccount> = state
            .of(provider)
            .filter(|account| {
                account.present || Some(account.id.as_str()) == current.map(|c| c.id.as_str())
            })
            .map(|account| {
                let mut detail = Vec::new();
                if let Some(plan) = account.plan() {
                    detail.push(plan);
                }
                let headline = account.headline();
                match (&account.snapshot, headline) {
                    (_, Some(window)) => {
                        detail.push(format!("{}% left", percent_left(window.used)))
                    }
                    (Some(snapshot), None) if snapshot.status == UsageStatus::NeedsLogin => {
                        detail.push("Signed out".into())
                    }
                    _ => {}
                }
                if state.registry.is_default(&account.account) {
                    detail.push("Default".into());
                }
                ChatAccount {
                    id: account.id.clone(),
                    label: state.display_name(account),
                    detail: detail.join(" · "),
                    selected: current.is_some_and(|current| current.id == account.id),
                    used: headline.map(|window| window.used),
                }
            })
            .collect();
        let current_label = match current {
            Some(current) => state.display_name(current),
            None => "Other home".into(),
        };
        if choices.is_empty()
            || (choices.len() < 2 && current.is_some_and(|current| current.account.is_system()))
        {
            return None;
        }
        Some(ChatAccounts {
            current: current_label,
            choices,
        })
    }

    /// How many open conversations and terminals run on account `id`.
    fn cards_on_account(&self, id: &AccountId) -> usize {
        let Some(state) = self.account_ui.account(id) else {
            return 0;
        };
        let provider = state.account.provider;
        self.durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .filter_map(|card| match &card.kind {
                CardKind::Chat(session) if session.launch.provider == provider => {
                    Some(&session.launch.recipe)
                }
                CardKind::Terminal(session) => Some(&session.launch),
                _ => None,
            })
            .filter(|recipe| {
                recipe
                    .environment
                    .get(account::home_variable(provider).unwrap_or_default())
                    .is_some()
                    && account_of_recipe(&self.account_ui.accounts, provider, recipe)
                        .is_some_and(|account| &account.id == id)
            })
            .count()
    }

    /// The per-provider default choices the settings pane offers.
    pub(super) fn default_account_choices(&self) -> Vec<(Provider, Vec<DefaultChoice>)> {
        let state = &self.account_ui;
        Account::PROVIDERS
            .into_iter()
            .map(|provider| {
                (
                    provider,
                    state
                        .of(provider)
                        // The default is always listed, so the choice shows
                        // what new conversations actually start on.
                        .filter(|account| {
                            account.present
                                || !account.account.is_system()
                                || state.registry.is_default(&account.account)
                        })
                        .map(|account| DefaultChoice {
                            id: account.id.clone(),
                            label: state.display_name(account),
                            selected: state.registry.is_default(&account.account),
                        })
                        .collect(),
                )
            })
            .collect()
    }

    pub(super) fn status_bar(&self) -> Option<Element<'_, Message>> {
        if !self.settings.show_status_bar {
            return None;
        }
        let now = self.account_ui.now_ms;
        let compact = self.window_width < COMPACT_WIDTH;
        let chips: Vec<Element<'_, Message>> = Account::REPORTED
            .into_iter()
            .filter_map(|provider| self.provider_chip(provider, now, compact))
            .collect();
        // A bar with nothing to report is only a line across the window.
        if chips.is_empty() {
            return None;
        }
        let bar = widget::container(
            widget::row::with_children(vec![
                widget::row::with_children(chips)
                    .spacing(design::GRID_UNIT)
                    .align_y(Alignment::Center)
                    .into(),
                iced::widget::Space::new().width(Length::Fill).into(),
            ])
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .height(STATUS_BAR_HEIGHT - design::BORDER_WIDTH)
        .padding(Padding::from([0.0, design::GRID_UNIT * 2.0]))
        .align_y(Alignment::Center);
        Some(
            widget::column::with_children(vec![design::horizontal_rule(), bar.into()])
                .width(Length::Fill)
                .into(),
        )
    }

    /// A provider's chip: its default account's figures - or, while that
    /// account has nothing to report, the first account that does - and how
    /// many more accounts the popover lists.
    fn provider_chip(
        &self,
        provider: Provider,
        now: Millis,
        compact: bool,
    ) -> Option<Element<'_, Message>> {
        let state = &self.account_ui;
        let present: Vec<&AccountState> = state
            .of(provider)
            .filter(|account| account.present)
            .collect();
        let lead = present
            .iter()
            .find(|account| state.registry.is_default(&account.account))
            .or_else(|| present.first())
            .copied()?;
        let others = present.len() - 1;
        let name = if others > 0 {
            format!("{} ({})", lead.name(), state.display_name(lead))
        } else {
            lead.name().to_owned()
        };
        let mut summary = chip(lead, &name, now);
        if others > 0 {
            summary.tooltip.push_str(&format!(
                "\n{others} more {} account{}",
                lead.name(),
                if others == 1 { "" } else { "s" }
            ));
        }
        let popover_account = state
            .popover
            .as_ref()
            .and_then(|id| state.account(id))
            .filter(|account| account.account.provider == provider);
        let open = popover_account.is_some() && !self.modal_is_open();
        let mut items: Vec<Element<'_, Message>> = vec![
            widget::icon(crate::ui_controls::provider_icon(provider))
                .size(CHIP_ICON_SIZE)
                .into(),
        ];
        if let Some(used) = summary.gauge {
            items.push(crate::ui_controls::budget_radial(used));
        }
        if !compact || summary.essential {
            let text =
                design::caption(summary.text).wrapping(cosmic::iced::widget::text::Wrapping::None);
            items.push(
                match summary.tone {
                    ChipTone::Current => text.class(cosmic::theme::Text::Default),
                    ChipTone::Stale => text,
                    ChipTone::Attention => {
                        text.class(cosmic::theme::Text::Custom(design::warning_text))
                    }
                }
                .into(),
            );
        }
        if others > 0 && !compact {
            items.push(
                design::caption(format!("+{others}"))
                    .class(cosmic::theme::Text::Custom(design::muted_text))
                    .into(),
            );
        }
        let button = widget::button::custom(
            widget::row::with_children(items)
                .spacing(design::GRID_UNIT * 1.5)
                .align_y(Alignment::Center),
        )
        .name(summary.tooltip.clone())
        .selected(open)
        .class(crate::ui_controls::control_style(open, None))
        .height(CHIP_HEIGHT)
        .padding(Padding::from([0.0, design::GRID_UNIT * 1.5]))
        .on_press(Message::AccountUi(Action::Popover(Some(
            popover_account.map_or_else(|| lead.id.clone(), |account| account.id.clone()),
        ))));
        let anchor =
            crate::interaction::tooltip(crate::interaction::hover(button), summary.tooltip);
        Some(crate::ui_controls::popover(
            anchor,
            open.then(|| {
                let shown = popover_account.unwrap_or(lead);
                let mut sections = vec![self.account_details(shown, now, Placement::Popover)];
                if others > 0 {
                    sections.push(design::horizontal_rule());
                    sections.push(self.account_switcher(provider, &shown.id, now));
                }
                widget::container(
                    widget::column::with_children(sections).spacing(design::GRID_UNIT * 2.0),
                )
                .width(POPOVER_WIDTH)
                .padding(design::GRID_UNIT)
                .into()
            }),
            widget::popover::Position::Top,
            Message::AccountUi(Action::Popover(None)),
        ))
    }

    /// The popover's list of every account of a provider, each with what it
    /// has left, to select a Cagents-managed default or inspect an Omp login.
    fn account_switcher<'a>(
        &'a self,
        provider: Provider,
        shown: &AccountId,
        now: Millis,
    ) -> Element<'a, Message> {
        let state = &self.account_ui;
        let rows: Vec<Element<'a, Message>> = state
            .of(provider)
            .filter(|account| account.present)
            .map(|account| {
                let selected = &account.id == shown;
                let mut items: Vec<Element<'a, Message>> = Vec::new();
                match account.headline() {
                    Some(window) => {
                        items.push(crate::ui_controls::budget_radial(window.used));
                    }
                    None => items.push(
                        iced::widget::Space::new()
                            .width(design::RADIAL_GAUGE_SIZE)
                            .height(design::RADIAL_GAUGE_SIZE)
                            .into(),
                    ),
                }
                items.push(
                    design::body(state.display_name(account))
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                            cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                        ))
                        .width(Length::Fill)
                        .into(),
                );
                items.push(
                    design::caption(chip(account, account.name(), now).text)
                        .wrapping(cosmic::iced::widget::text::Wrapping::None)
                        .into(),
                );
                widget::button::custom(
                    widget::row::with_children(items)
                        .spacing(design::GRID_UNIT * 2.0)
                        .align_y(Alignment::Center),
                )
                .name(if account.account.is_brokered() {
                    format!("View {} usage", state.display_name(account))
                } else {
                    format!("Use {} by default", state.display_name(account))
                })
                .selected(selected)
                .class(crate::ui_controls::control_style(selected, None))
                .padding([design::GRID_UNIT, design::GRID_UNIT * 2.0])
                .width(Length::Fill)
                .on_press(Message::AccountUi(Action::ShowAccount(account.id.clone())))
                .into()
            })
            .collect();
        widget::column::with_children(rows)
            .spacing(2)
            .width(Length::Fill)
            .into()
    }

    /// Everything known about one account: the popover's content.
    fn account_details<'a>(
        &'a self,
        account: &'a AccountState,
        now: Millis,
        placement: Placement,
    ) -> Element<'a, Message> {
        let spacing = design::GRID_UNIT * 2.0;
        let mut rows: Vec<Element<'a, Message>> =
            vec![self.account_header(account, now, placement)];
        if let Some(line) = account.identity.as_ref().and_then(identity_line) {
            rows.push(
                design::caption(line)
                    .wrapping(cosmic::iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            );
        }
        match &account.snapshot {
            None if account.failure.is_none() => {
                rows.push(design::caption("Checking usage…").into());
            }
            None => {}
            Some(snapshot) => match snapshot.status {
                UsageStatus::NeedsLogin => {
                    rows.push(
                        design::body("Signed out")
                            .class(cosmic::theme::Text::Custom(design::warning_text))
                            .into(),
                    );
                    rows.push(
                        design::caption(format!(
                            "{} no longer has a working login. Sign in again to see this account's usage.",
                            account.name()
                        ))
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                        .into(),
                    );
                    rows.push(
                        widget::button::suggested("Sign in")
                            .on_press(Message::AccountUi(Action::SignIn(account.id.clone())))
                            .into(),
                    );
                }
                UsageStatus::Unavailable => {
                    rows.push(
                        design::caption(
                            snapshot
                                .detail
                                .clone()
                                .unwrap_or_else(|| "No usage is reported for this account".into()),
                        )
                        .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                        .into(),
                    );
                }
                UsageStatus::Ok | UsageStatus::Warning | UsageStatus::Limited => {
                    let stale = is_stale(snapshot, now);
                    rows.extend(
                        snapshot
                            .windows
                            .iter()
                            .map(|window| usage_window_row(window, now, stale)),
                    );
                    if snapshot.windows.is_empty() {
                        rows.push(
                            design::caption("No usage limits are reported for this account.")
                                .into(),
                        );
                    }
                    if let Some(credits) = &snapshot.credits {
                        rows.push(
                            widget::row::with_children(vec![
                                design::caption("Extra usage").width(Length::Fill).into(),
                                design::caption(credits.summary())
                                    .class(cosmic::theme::Text::Default)
                                    .into(),
                            ])
                            .into(),
                        );
                    }
                }
            },
        }
        let mut footer: Vec<Element<'a, Message>> = Vec::new();
        if let Some(snapshot) = &account.snapshot {
            let ago = account::format_ago(unix_seconds(now) - unix_seconds(snapshot.observed_at));
            let source = match snapshot.source {
                UsageSource::Probe => "",
                UsageSource::Cache => " · saved",
                UsageSource::Live => " · from a running conversation",
                UsageSource::Recorded => " · as Omp recorded it",
            };
            let updated = format!("Updated {ago}{source}");
            footer.push(design::caption(updated).into());
        }
        if let Some(failure) = &account.failure {
            footer.push(
                design::caption(format!("Last check failed: {failure}"))
                    .class(cosmic::theme::Text::Custom(design::warning_text))
                    .wrapping(cosmic::iced::widget::text::Wrapping::Word)
                    .into(),
            );
        }
        if !footer.is_empty() {
            rows.push(design::horizontal_rule());
            rows.push(widget::column::with_children(footer).spacing(2).into());
        }
        widget::column::with_children(rows)
            .spacing(spacing)
            .width(Length::Fill)
            .into()
    }

    fn account_header<'a>(
        &'a self,
        account: &'a AccountState,
        now: Millis,
        placement: Placement,
    ) -> Element<'a, Message> {
        let state = &self.account_ui;
        let mut title: Vec<Element<'a, Message>> = vec![design::heading(account.name()).into()];
        let several = state
            .of(account.account.provider)
            .filter(|other| other.present)
            .count()
            > 1;
        if several || !account.account.is_system() {
            title.push(
                design::caption(state.display_name(account))
                    .wrapping(cosmic::iced::widget::text::Wrapping::None)
                    .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                        cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
                    ))
                    .into(),
            );
        }
        let mut items: Vec<Element<'a, Message>> = vec![
            widget::icon(crate::ui_controls::provider_icon(account.account.provider))
                .size(design::ROW_ICON_SIZE)
                .into(),
            widget::column::with_children(title)
                .spacing(0)
                .width(Length::Shrink)
                .into(),
        ];
        if let Some(plan) = account.plan() {
            items.push(design::badge(
                plan,
                design::Tone::Themed(design::accent_color),
            ));
        }
        items.push(iced::widget::Space::new().width(Length::Fill).into());
        if account.present || account.snapshot.is_some() {
            items.push(refresh_control(account, now).into());
        }
        if placement == Placement::Popover {
            items.push(
                crate::ui_controls::compact_icon_control(
                    crate::ui_controls::symbolic_icon("view-pop-out-symbolic"),
                    "Open in the Accounts pane",
                    Some(Message::AccountUi(Action::OpenPane)),
                )
                .into(),
            );
        }
        widget::row::with_children(items)
            .spacing(design::GRID_UNIT * 2.0)
            .align_y(Alignment::Center)
            .into()
    }

    /// The Accounts pane: every account, as a table grouped by provider.
    pub(super) fn accounts_pane(&self) -> Element<'_, Message> {
        table::view(self)
    }
}

fn browser_login_available() -> Result<(), String> {
    let path = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    check_browser_login_host(&path)?;
    account::login::check_callback_port()
}

fn check_browser_login_host(path: &Path) -> Result<(), String> {
    // The daemon may not have started yet. Its atomic spawn reservation is
    // authoritative; this earlier check avoids creating a needless home.
    let mut client = match crate::control::ControlClient::connect(path) {
        Ok(client) => client,
        Err(crate::control::ControlError::Connect(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(format!("Cannot check existing sign-ins: {error}")),
    };
    if client
        .list()
        .map_err(|error| error.to_string())?
        .iter()
        .any(|descriptor| {
            account::login::is_codex_browser(&descriptor.session.launch)
                && !terminal_ended(&descriptor.session)
        })
    {
        return Err(account::login::BUSY.into());
    }
    Ok(())
}

fn callback_failed_on_host(terminal: TerminalId) -> Result<bool, String> {
    let path = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client =
        crate::control::ControlClient::connect(&path).map_err(|error| error.to_string())?;
    let hosted = client.list().map_err(|error| error.to_string())?;
    // A cancelled and already removed terminal has no failure left to read.
    if !hosted
        .iter()
        .any(|descriptor| descriptor.session.id == terminal)
    {
        return Ok(false);
    }
    client
        .snapshot(terminal)
        .map(|snapshot| account::login::callback_failed(&snapshot.text))
        .map_err(|error| error.to_string())
}

/// The terminal a sign-in card runs.
fn sign_in_terminal(state: Option<&CardState>) -> Option<TerminalId> {
    match &state?.kind {
        CardKind::Terminal(session) => Some(session.id),
        _ => None,
    }
}

/// Which of the sign-ins `watched`, each named with its terminal and the
/// daemon session that terminal was hosted in, have ended by the daemon's
/// account. A terminal the daemon hosts says so itself. One it does not
/// has ended only if the daemon is still that session: a daemon started
/// since may not have restored it yet.
fn ended_on_host(
    watched: &[(uuid::Uuid, TerminalId, HostSessionProof)],
) -> Result<Vec<(uuid::Uuid, TerminalId)>, String> {
    let path = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client =
        crate::control::ControlClient::connect(&path).map_err(|error| error.to_string())?;
    let hosted = client.list().map_err(|error| error.to_string())?;
    let session = client.proof().clone();
    Ok(watched
        .iter()
        .filter(|(_, terminal, proof)| {
            match hosted
                .iter()
                .find(|descriptor| descriptor.session.id == *terminal)
            {
                Some(descriptor) => terminal_ended(&descriptor.session),
                None => session == *proof,
            }
        })
        .map(|(key, terminal, _)| (*key, *terminal))
        .collect())
}

/// Closes the hosted `terminal` for good. A daemon that restored it under
/// a session of its own is asked again with that session, so a close is
/// only reported done once the terminal has ended or is gone.
fn stop_hosted(terminal: TerminalId, proof: &HostSessionProof) -> Result<(), String> {
    use super::terminal_close::{CloseOutcome, close_hosted_terminal};
    let outcome = match close_hosted_terminal(terminal, proof, true) {
        CloseOutcome::Stale => {
            let path =
                crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
            let mut client =
                crate::control::ControlClient::connect(&path).map_err(|error| error.to_string())?;
            let hosted = client.list().map_err(|error| error.to_string())?;
            match hosted
                .iter()
                .find(|descriptor| descriptor.session.id == terminal)
            {
                None => return Ok(()),
                Some(descriptor) if terminal_ended(&descriptor.session) => return Ok(()),
                Some(descriptor) => match descriptor.session.continuity.live_proof() {
                    Some(current) if current != *proof => {
                        close_hosted_terminal(terminal, &current, true)
                    }
                    _ => CloseOutcome::Stale,
                },
            }
        }
        outcome => outcome,
    };
    match outcome {
        CloseOutcome::Closed => Ok(()),
        CloseOutcome::Active | CloseOutcome::Stale => {
            Err("its terminal is still running and could not be closed".to_owned())
        }
        CloseOutcome::Unreachable(error) => Err(error),
    }
}

/// Forgets the pending sign-in `key`, returning the registry as it now
/// stands where there is one on disk.
fn abandon_home(
    store: Option<&Store>,
    homes: Option<&Homes>,
    provider: Provider,
    key: uuid::Uuid,
) -> Result<Option<Registry>, String> {
    if let (Some(store), Some(homes)) = (store, homes) {
        let (registry, error) = store
            .remove_home(homes, provider, key, false)
            .map_err(|error| error.to_string())?;
        if let Some(error) = error {
            return Err(error);
        }
        return Ok(Some(registry));
    }
    if let Some(homes) = homes {
        homes
            .remove(provider, key)
            .map_err(|error| error.to_string())?;
    }
    abandon(store, key)
}

fn abandon(store: Option<&Store>, key: uuid::Uuid) -> Result<Option<Registry>, String> {
    store
        .map(|store| {
            store
                .update(|registry| registry.abandon(key))
                .map(|(registry, ())| registry)
                .map_err(|error| error.to_string())
        })
        .transpose()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    Popover,
}

/// The control that asks an account's CLI again, which says why it cannot
/// while it cannot.
fn refresh_control<'a>(
    account: &AccountState,
    now: Millis,
) -> crate::ui_controls::IconControl<'a, Message> {
    let refreshing = account.schedule.in_flight(now) || account.queued.is_some();
    let ready = !refreshing && account.schedule.manual_ready(now);
    let label = if refreshing {
        "Checking…"
    } else if ready {
        "Check now"
    } else {
        "Checked moments ago"
    };
    crate::ui_controls::compact_icon_control(
        crate::ui_controls::symbolic_icon("view-refresh-symbolic"),
        label,
        ready.then(|| Message::AccountUi(Action::Refresh(account.id.clone()))),
    )
}

fn identity_line(identity: &Identity) -> Option<String> {
    let parts: Vec<&str> = [identity.email.as_deref(), identity.organization.as_deref()]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn usage_window_row<'a>(window: &'a UsageWindow, now: Millis, stale: bool) -> Element<'a, Message> {
    let tone = if stale {
        design::Tone::Themed(design::muted_color)
    } else {
        design::Tone::Themed(design::budget_tone(window.used))
    };
    let mut rows: Vec<Element<'a, Message>> = vec![
        widget::row::with_children(vec![
            design::body(window.label.as_str())
                .width(Length::Fill)
                .into(),
            design::caption(format!("{}% left", percent_left(window.used)))
                .class(cosmic::theme::Text::Default)
                .into(),
        ])
        .align_y(Alignment::Center)
        .into(),
        crate::ui_controls::budget_bar(window.used, tone),
    ];
    if let Some(line) = reset_line(window, now) {
        rows.push(design::caption(line).into());
    }
    widget::column::with_children(rows)
        .spacing(design::GRID_UNIT)
        .into()
}

#[cfg(test)]
impl State {
    /// Registers a managed account under a fixture root, as a finished
    /// sign-in would, and returns its id.
    pub(super) fn add_managed_fixture(&mut self, provider: Provider, key: u128) -> AccountId {
        let homes = self
            .homes
            .get_or_insert_with(|| Homes::at("/cagents-fixture/accounts"))
            .clone();
        let key = uuid::Uuid::from_u128(key);
        let mut registry = self.registry.clone();
        registry
            .register(
                registry::Registered {
                    key,
                    provider,
                    linked_home: None,
                    label: None,
                    email: Some(format!("{key}@example.com")),
                    organization: None,
                    added_at: 0,
                },
                &[],
            )
            .expect("a fixture account is new");
        self.set_registry(registry);
        let id = Account::managed(provider, key, homes.path(provider, key)).id();
        if let Some(state) = self.account_mut(&id) {
            state.present = true;
        }
        id
    }

    pub(super) fn snapshot_for_test(&self, id: &AccountId) -> Option<&UsageSnapshot> {
        self.account(id)?.snapshot.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::WindowKind;

    const NOW: Millis = 1_790_000_000_000;

    #[test]
    fn browser_login_preflight_allows_normal_startup_after_a_stale_socket() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.sock");
        assert!(check_browser_login_host(&path).is_ok());
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(listener);
        assert!(path.exists());
        assert!(check_browser_login_host(&path).is_ok());
    }

    fn state_with(snapshot: Option<UsageSnapshot>) -> AccountState {
        let mut state = AccountState::new(Account::system(Provider::Claude));
        state.present = true;
        state.snapshot = snapshot;
        state
    }

    fn window(used: f32, resets_at: Option<i64>) -> UsageWindow {
        UsageWindow {
            key: "session".into(),
            kind: WindowKind::Session,
            label: "Session".into(),
            used,
            resets_at,
            span_secs: Some(5 * 3_600),
            binding: false,
        }
    }

    fn snapshot(windows: Vec<UsageWindow>, observed_at: Millis) -> UsageSnapshot {
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", observed_at);
        snapshot.detail = None;
        snapshot.windows = windows;
        snapshot.refresh_status(false, false);
        snapshot
    }

    #[test]
    fn percent_left_rounds_toward_caution_without_float_noise() {
        assert_eq!(percent_left(0.43), 57);
        assert_eq!(percent_left(0.424), 57);
        assert_eq!(percent_left(0.0), 100);
        assert_eq!(percent_left(1.0), 0);
        assert_eq!(percent_left(1.3), 0);
        assert_eq!(percent_left(0.001), 99);
    }

    #[test]
    fn a_current_chip_states_what_is_left_and_when_it_resets() {
        let reset = unix_seconds(NOW) + 2 * 3_600 + 14 * 60;
        let chip = chip(
            &state_with(Some(snapshot(vec![window(0.38, Some(reset))], NOW))),
            "Claude",
            NOW,
        );
        assert_eq!(chip.text, "62% left · 2h 14m");
        assert_eq!(chip.gauge, Some(0.38));
        assert_eq!(chip.tone, ChipTone::Current);
        assert!(chip.tooltip.contains("Session: 62% left"));
        assert_eq!(
            reset_countdown(&window(0.38, Some(reset)), NOW).as_deref(),
            Some("2h 14m")
        );
    }

    /// Claude marks one model's exhausted weekly allowance as the binding
    /// limit while the account keeps working on every other model.
    #[test]
    fn a_chip_leads_with_the_account_not_one_models_allowance() {
        let mut scoped = window(1.0, None);
        scoped.key = "weekly_scoped:fable".into();
        scoped.kind = WindowKind::Scoped;
        scoped.label = "Weekly · Fable".into();
        scoped.binding = true;
        let mut weekly = window(0.95, None);
        weekly.key = "weekly_all".into();
        weekly.kind = WindowKind::Weekly;
        weekly.label = "Weekly".into();
        let state = state_with(Some(snapshot(
            vec![window(0.43, None), weekly, scoped],
            NOW,
        )));
        let chip = chip(&state, "Claude", NOW);
        assert_eq!(chip.text, "5% left");
        assert!(
            chip.tooltip.contains("Weekly · Fable: 0% left"),
            "{}",
            chip.tooltip
        );
    }

    #[test]
    fn old_figures_and_figures_from_before_a_reset_are_shown_as_of_when_they_were_read() {
        let observed = NOW - schedule::DISPLAY_STALE_AFTER - 1;
        let old = chip(
            &state_with(Some(snapshot(vec![window(0.5, None)], observed))),
            "Claude",
            NOW,
        );
        assert_eq!(old.tone, ChipTone::Stale);
        assert!(old.text.starts_with("50% left · as of "), "{}", old.text);

        let reset = unix_seconds(NOW) - 60;
        let before_reset = snapshot(vec![window(0.9, Some(reset))], NOW - 5 * 60_000);
        assert!(reset_since(&before_reset, NOW));
        assert_eq!(
            chip(&state_with(Some(before_reset)), "Claude", NOW).tone,
            ChipTone::Stale
        );
    }

    #[test]
    fn a_signed_out_account_asks_for_a_sign_in_and_an_unanswered_one_says_so() {
        let signed_out = UsageSnapshot::state(UsageStatus::NeedsLogin, "signed out", NOW);
        let chip_for_login = chip(&state_with(Some(signed_out)), "Claude", NOW);
        assert_eq!(chip_for_login.text, "Sign in");
        assert_eq!(chip_for_login.tone, ChipTone::Attention);
        assert!(chip_for_login.essential);

        assert_eq!(chip(&state_with(None), "Claude", NOW).text, "Checking…");
        let mut failed = state_with(None);
        failed.failure = Some("Claude Code did not answer".into());
        assert_eq!(chip(&failed, "Claude", NOW).text, "Unavailable");
    }

    #[test]
    fn a_login_terminal_runs_the_probed_cli_at_home_and_is_not_an_agent() {
        let launch = login_launch(
            &Account::system(Provider::Claude),
            Path::new("/opt/bin/claude"),
            PathBuf::from("/home/reader"),
            false,
        )
        .expect("Claude has a login command");
        assert_eq!(launch.program, "/opt/bin/claude");
        assert_eq!(launch.arguments, ["auth", "login", "--claudeai"]);
        assert_eq!(launch.working_directory, Path::new("/home/reader"));
        assert_eq!(
            launch
                .environment
                .get("CLAUDE_CONFIG_DIR")
                .map(String::as_str),
            Account::system(Provider::Claude)
                .resolved_home()
                .as_deref()
                .and_then(Path::to_str)
        );
        assert_eq!(Provider::recognize_launch(&launch), None);
        assert!(is_sign_in(&launch));

        let managed = Account::managed(
            Provider::Codex,
            uuid::Uuid::from_u128(1),
            PathBuf::from("/accounts/work"),
        );
        for (device, arguments) in [
            (false, &["login"][..]),
            (true, &["login", "--device-auth"][..]),
        ] {
            let launch = login_launch(
                &managed,
                Path::new("/usr/bin/codex"),
                PathBuf::from("/h"),
                device,
            )
            .expect("Codex logs in");
            assert_eq!(launch.arguments, arguments);
            assert_eq!(Provider::recognize_launch(&launch), None);
            assert_eq!(
                launch.environment.get("CODEX_HOME").map(String::as_str),
                Some("/accounts/work")
            );
            assert!(is_sign_in(&launch));
        }
    }

    #[test]
    fn only_a_providers_sign_in_is_kept_off_the_disk() {
        let recipe = |program: &str, arguments: &[&str]| LaunchRecipe {
            unset_environment: Default::default(),
            program: program.to_owned(),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            working_directory: PathBuf::from("/h"),
            environment: BTreeMap::new(),
        };
        assert!(is_sign_in(&recipe("/opt/bin/claude", &["auth", "login"])));
        assert!(is_sign_in(&recipe(
            "claude",
            &["auth", "login", "--claudeai"]
        )));
        assert!(is_sign_in(&recipe("codex", &["login"])));
        assert!(is_sign_in(&recipe("codex", &["login", "--device-auth"])));
        assert!(!is_sign_in(&recipe("claude", &[])));
        assert!(!is_sign_in(&recipe("claude", &["login"])));
        assert!(!is_sign_in(&recipe(
            "claude",
            &["auth", "login", "--console"]
        )));
        assert!(!is_sign_in(&recipe("codex", &["login", "--api-key"])));
        assert!(!is_sign_in(&recipe("/bin/fish", &["login"])));
    }

    #[test]
    fn a_conversation_runs_on_the_account_its_recipe_points_at() {
        let mut state = State::default();
        let managed = state.add_managed_fixture(Provider::Claude, 5);
        let home = state
            .account(&managed)
            .unwrap()
            .account
            .resolved_home()
            .unwrap();
        let mut recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "claude".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/w"),
            environment: BTreeMap::new(),
        };
        let found = |recipe: &LaunchRecipe| {
            account_of_recipe(&state.accounts, Provider::Claude, recipe)
                .map(|state| state.id.clone())
        };
        assert_eq!(found(&recipe), Some(Account::system(Provider::Claude).id()));
        recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), home.display().to_string());
        assert_eq!(found(&recipe), Some(managed));
        recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "/somewhere/else".into());
        assert_eq!(found(&recipe), None);
    }
}

#[cfg(test)]
mod app_tests {
    #[test]
    fn browser_add_is_fenced_before_async_home_creation_and_errors_release_it() {
        let mut app = context_inspector_test_app(false);
        app.account_ui.homes = Some(Homes::at("/cagents-fixture/accounts"));
        let _first = app.add_account(Provider::Codex, false);
        assert!(app.account_ui.browser_preparing);
        let _second = app.add_account(Provider::Codex, false);
        assert!(app.account_ui.logins.is_empty());
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.message == account::login::BUSY)
        );
        // A device-code request is independent and cannot clear the browser fence.
        let _ = app.open_account_login(Provider::Codex, true, Err("fixture".into()));
        assert!(app.account_ui.browser_preparing);
        let _ = app.open_account_login(Provider::Codex, false, Err("fixture".into()));
        assert!(!app.account_ui.browser_preparing);
    }

    #[test]
    fn callback_spawn_failure_retains_pending_row_and_device_choice() {
        let mut app = context_inspector_test_app(false);
        let key = uuid::Uuid::from_u128(511);
        let home = PathBuf::from("/cagents-fixture/accounts/codex").join(key.to_string());
        let _ = app.open_account_login(
            Provider::Codex,
            false,
            Ok((key, home, "/tools/codex".into())),
        );
        let card = app
            .sign_in_card(Provider::Codex, &app.account_ui.logins[0].home)
            .unwrap();
        assert!(app.codex_browser_busy());
        app.cards
            .insert(card, CardRuntime::Failed(account::login::PORT_BUSY.into()));
        let _ = app.check_logins();
        assert_eq!(app.account_ui.logins[0].phase, LoginPhase::CallbackBlocked);
        assert!(!app.codex_browser_busy());
        let _ = app.check_logins();
        assert_eq!(
            app.account_ui.logins.len(),
            1,
            "failure is not silently discarded as signed out"
        );
        let launch = login_launch(
            &app.account_ui.logins[0].account(),
            Path::new("/tools/codex"),
            "/fixture".into(),
            true,
        )
        .unwrap();
        assert_eq!(launch.arguments, ["login", "--device-auth"]);
    }

    #[test]
    fn callback_bind_race_answer_retains_row_until_dismissed() {
        let mut app = context_inspector_test_app(false);
        let key = pending_login(&mut app, 512);
        app.account_ui.logins[0].phase = LoginPhase::Verifying;
        let _ = app.update(Message::AccountUi(Action::LoginCallbackBlocked(key)));
        assert_eq!(app.account_ui.logins[0].phase, LoginPhase::CallbackBlocked);
        let _ = app.update(Message::AccountUi(Action::LoginTick));
        assert_eq!(app.account_ui.logins[0].phase, LoginPhase::CallbackBlocked);
    }

    use super::*;
    use crate::account::WindowKind;
    use crate::app::tests::context_inspector_test_app;

    fn signed_out(app: &mut Cagents) -> AccountId {
        let state = &mut app.account_ui.accounts[0];
        state.present = true;
        state.snapshot = Some(UsageSnapshot::state(
            UsageStatus::NeedsLogin,
            "signed out",
            NOW,
        ));
        state.id.clone()
    }

    const NOW: Millis = 1_790_000_000_000;

    fn escape() -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }
    }

    /// An app with one worktree, `/repo`, on screen.
    fn app_in_repo() -> Cagents {
        let mut app = context_inspector_test_app(false);
        app.sidebar_model.projects = vec![SidebarProject::new(
            crate::model::ProjectId::new(),
            crate::repository::RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![SidebarWorktree::new(
                crate::model::WorktreeId::new(),
                "/repo".into(),
                None,
                false,
                false,
                None,
                None,
                None,
            )],
            Vec::new(),
        )];
        app.known_worktree_roots = [PathBuf::from("/repo")].into_iter().collect();
        let _ = app.select_worktree("/repo".into(), false);
        app
    }

    /// Opens a Claude sign-in terminal and returns its card and terminal.
    fn open_sign_in(app: &mut Cagents) -> (CardId, TerminalId) {
        let launch = login_launch(
            &Account::system(Provider::Claude),
            Path::new("/opt/bin/claude"),
            PathBuf::from("/home/reader"),
            false,
        )
        .expect("Claude has a login command");
        let (_, terminal) = app.open_login_terminal(launch, "Sign in".into());
        let card = app
            .durable
            .cards
            .iter()
            .find(
                |card| matches!(&card.kind, CardKind::Terminal(session) if session.id == terminal),
            )
            .map(|card| card.id)
            .expect("the login opens a card");
        (card, terminal)
    }

    #[test]
    fn a_sign_in_stays_in_the_strip_it_was_opened_from() {
        let mut app = app_in_repo();
        let (card, _) = open_sign_in(&mut app);
        assert!(app.workspace.order().any(|id| id == card));

        // Rebuilding the scope re-derives every card's worktree; the login's
        // home directory belongs to none, so only its filing keeps it here.
        let _ = app.select_worktree("/repo".into(), false);
        assert!(
            app.workspace.order().any(|id| id == card),
            "the sign-in left the strip it was started from"
        );
    }

    #[test]
    fn a_sign_in_off_the_strip_is_settled_on_the_daemons_word() {
        let mut app = app_in_repo();
        let key = uuid::Uuid::from_u128(21);
        let home = PathBuf::from("/cagents-fixture/accounts/claude").join(key.to_string());
        let launch = login_launch(
            &Account::managed(Provider::Claude, key, home.clone()),
            Path::new("/opt/bin/claude"),
            PathBuf::from("/home/reader"),
            false,
        )
        .expect("Claude has a login command");
        let (_, terminal) = app.open_login_terminal(launch, "Sign in".into());
        let card = app
            .durable
            .cards
            .iter()
            .find(
                |card| matches!(&card.kind, CardKind::Terminal(session) if session.id == terminal),
            )
            .map(|card| card.id)
            .expect("the login opens a card");
        app.account_ui.logins.push(Login {
            provider: Provider::Claude,
            key,
            home,
            phase: LoginPhase::Waiting,
            attempts: 0,
            next_check: 0,
            started_at: now_ms(),
            recovered: false,
        });
        let phase = |app: &Cagents| app.account_ui.logins.first().map(|login| login.phase);

        // Until the daemon has taken the terminal on it has nothing to say,
        // and a card that has not started has not ended either.
        let _ = app.update(Message::AccountUi(Action::LoginTick));
        assert_eq!(phase(&app), Some(LoginPhase::Waiting));
        assert!(!app.account_ui.asking_host);

        if let Some(CardState {
            kind: CardKind::Terminal(session),
            ..
        }) = app.card_state_mut(card)
        {
            session.continuity = TerminalContinuity::DetachedSameBoot {
                host_session_id: crate::model::HostSessionId::new("host"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("incarnation"),
            };
        }
        let _ = app.select_worktree("/elsewhere".into(), false);
        assert!(!app.workspace.order().any(|id| id == card));

        // The hidden card goes on showing a running terminal whatever
        // happens to it, so the daemon is asked instead.
        let _ = app.update(Message::AccountUi(Action::LoginTick));
        assert!(app.account_ui.asking_host);
        assert_eq!(phase(&app), Some(LoginPhase::Waiting));
        // An answer about a terminal the sign-in no longer runs is not one
        // about this sign-in.
        let _ = app.update(Message::AccountUi(Action::LoginsEnded(Ok(vec![(
            key,
            TerminalId::new(),
        )]))));
        assert!(!app.account_ui.asking_host);
        assert_eq!(phase(&app), Some(LoginPhase::Waiting));
        let _ = app.update(Message::AccountUi(Action::LoginsEnded(Ok(vec![(
            key, terminal,
        )]))));
        assert_eq!(phase(&app), Some(LoginPhase::Verifying));

        // Settling closes the terminal through the daemon, which no strip
        // on screen can do, and only a close the daemon confirmed retires
        // the card.
        let _ = app.update(Message::AccountUi(Action::LoginSettled {
            key,
            result: Ok(Settled::Abandoned(None)),
        }));
        assert!(app.account_ui.logins.is_empty());
        assert!(
            app.card_state(card)
                .is_some_and(|state| state.existence.is_open())
        );
        let _ = app.update(Message::AccountUi(Action::OffstripClosed {
            card,
            result: Err("unreachable".into()),
        }));
        assert!(
            app.card_state(card)
                .is_some_and(|state| state.existence.is_open())
        );
        let _ = app.update(Message::AccountUi(Action::OffstripClosed {
            card,
            result: Ok(()),
        }));
        assert!(
            app.card_state(card)
                .is_none_or(|state| !state.existence.is_open()),
            "the settled sign-in's card was left open"
        );
        assert!(!app.durable.card_scopes.contains_key(&card));
    }

    #[test]
    fn cancelling_a_sign_in_off_the_strip_that_never_started_retires_it() {
        let mut app = app_in_repo();
        let key = uuid::Uuid::from_u128(22);
        let home = PathBuf::from("/cagents-fixture/accounts/claude").join(key.to_string());
        let launch = login_launch(
            &Account::managed(Provider::Claude, key, home.clone()),
            Path::new("/opt/bin/claude"),
            PathBuf::from("/home/reader"),
            false,
        )
        .expect("Claude has a login command");
        let (_, terminal) = app.open_login_terminal(launch, "Sign in".into());
        let card = app
            .durable
            .cards
            .iter()
            .find(
                |card| matches!(&card.kind, CardKind::Terminal(session) if session.id == terminal),
            )
            .map(|card| card.id)
            .expect("the login opens a card");
        app.account_ui.logins.push(Login {
            provider: Provider::Claude,
            key,
            home,
            phase: LoginPhase::Waiting,
            attempts: 0,
            next_check: 0,
            started_at: now_ms(),
            recovered: false,
        });
        let _ = app.select_worktree("/elsewhere".into(), false);
        assert!(!app.workspace.order().any(|id| id == card));

        // No daemon ever hosted it, so nothing needs stopping: the card is
        // retired at once and the sign-in, left without one, is settled.
        let _ = app.update(Message::AccountUi(Action::CancelLogin(key)));
        assert!(
            app.card_state(card)
                .is_none_or(|state| !state.existence.is_open())
        );
        assert_eq!(
            app.account_ui.logins.first().map(|login| login.phase),
            Some(LoginPhase::Verifying)
        );
    }

    #[test]
    fn the_status_bar_follows_its_setting_and_hiding_it_closes_the_popover() {
        let mut app = context_inspector_test_app(false);
        let id = signed_out(&mut app);
        assert!(app.status_bar().is_some());
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id.clone()))));
        assert_eq!(app.account_ui.popover.as_ref(), Some(&id));
        let _ = app.update(Message::SettingsChanged {
            field: SettingsField::ShowStatusBar,
            value: "false".into(),
        });
        assert!(!app.settings.show_status_bar);
        assert!(app.status_bar().is_none());
        assert_eq!(app.account_ui.popover, None);
    }

    #[test]
    fn a_chip_toggles_its_popover_and_escape_closes_it_first() {
        let mut app = context_inspector_test_app(false);
        let id = signed_out(&mut app);
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id.clone()))));
        assert_eq!(app.account_ui.popover.as_ref(), Some(&id));
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id.clone()))));
        assert_eq!(app.account_ui.popover, None);
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id))));
        // Captured by a focused terminal or not, Escape reaches the popover.
        let _ = app.update(Message::KeyEvent {
            event: escape(),
            captured: true,
        });
        assert_eq!(app.account_ui.popover, None);
        assert!(app.account_key_event(&escape()).is_none());
    }

    #[test]
    fn the_popover_selects_another_account_as_the_provider_default() {
        let mut app = context_inspector_test_app(false);
        let system = signed_out(&mut app);
        let managed = app.account_ui.add_managed_fixture(Provider::Claude, 3);
        assert_eq!(app.default_account(Provider::Claude).id(), system);
        let _ = app.update(Message::AccountUi(Action::Popover(Some(system))));
        let _ = app.update(Message::AccountUi(Action::ShowAccount(managed.clone())));
        assert_eq!(app.account_ui.popover.as_ref(), Some(&managed));
        assert_eq!(app.default_account(Provider::Claude).id(), managed);
        assert!(app.status_bar().is_some());
    }

    #[test]
    fn the_pop_out_opens_one_accounts_pane_and_closes_the_popover() {
        let mut app = context_inspector_test_app(false);
        let id = signed_out(&mut app);
        let _ = app.update(Message::AccountUi(Action::Popover(Some(id))));
        let _ = app.update(Message::AccountUi(Action::OpenPane));
        let pane = app
            .utility_card(&CardKind::Accounts)
            .expect("the pop-out opens the Accounts pane");
        assert_eq!(app.account_ui.popover, None);
        assert!(matches!(app.cards.get(&pane), Some(CardRuntime::Accounts)));
        let _ = app.update(Message::AccountUi(Action::OpenPane));
        assert_eq!(app.utility_card(&CardKind::Accounts), Some(pane));
        assert_eq!(
            app.durable
                .cards
                .iter()
                .filter(|card| card.existence.is_open() && card.kind == CardKind::Accounts)
                .count(),
            1
        );
        // The pane renders at any width without panicking.
        let _ = app.accounts_pane();
    }

    #[test]
    fn a_probe_answer_updates_the_account_and_a_failure_keeps_its_figures() {
        let mut app = context_inspector_test_app(false);
        let id = app.account_ui.accounts[0].id.clone();
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", NOW);
        snapshot.detail = None;
        snapshot.windows = vec![UsageWindow {
            key: "session".into(),
            kind: account::WindowKind::Session,
            label: "Session".into(),
            used: 0.4,
            resets_at: None,
            span_secs: None,
            binding: false,
        }];
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: id.clone(),
            outcome: Outcome::Answered {
                catalog: None,
                identity: None,
                snapshot: snapshot.clone(),
            },
        }));
        let state = app.account_ui.account(&id).unwrap();
        assert!(state.present);
        assert_eq!(state.snapshot.as_ref(), Some(&snapshot));
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: id.clone(),
            outcome: Outcome::Failed("timed out".into()),
        }));
        let state = app.account_ui.account(&id).unwrap();
        assert_eq!(state.snapshot.as_ref(), Some(&snapshot));
        assert_eq!(state.failure.as_deref(), Some("timed out"));
        assert_eq!(state.schedule.failures(), 1);
        // Another window's older answer, read back from the cache, changes
        // nothing - not the figures, and not who the account belongs to.
        let mut older = UsageSnapshot::state(UsageStatus::NeedsLogin, "signed out", NOW - 1);
        older.source = UsageSource::Cache;
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: id.clone(),
            outcome: Outcome::Answered {
                catalog: None,
                identity: Some(account::Identity {
                    email: Some("someone@example.com".into()),
                    organization: None,
                    plan: None,
                }),
                snapshot: older,
            },
        }));
        let state = app.account_ui.account(&id).unwrap();
        assert_eq!(state.snapshot.as_ref(), Some(&snapshot));
        assert_eq!(state.identity, None);
        assert!(!state.schedule.in_flight(NOW));
    }

    fn claude_report(used: f64) -> LiveReport {
        LiveReport::Claude(
            account::claude::parse_rate_limit_event(&serde_json::json!({
                "status": "allowed",
                "unifiedWindows": {"five_hour": {"utilization": used, "resetsAt": 1_790_119_800}},
            }))
            .unwrap(),
        )
    }

    /// A conversation launched on `account`, as its recipe records it.
    fn recipe_on(app: &Cagents, account: &AccountId) -> LaunchRecipe {
        let state = app.account_ui.account(account).unwrap();
        let mut recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: state.account.provider.executable().into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/w"),
            environment: BTreeMap::new(),
        };
        if let Some((variable, home)) = state.account.probe_environment() {
            recipe
                .environment
                .insert(variable.into(), home.display().to_string());
        }
        recipe
    }

    /// The models each provider lists come from the cache at startup, from
    /// probes and from running conversations; the newest list wins, and one a
    /// conversation listed reaches other windows through the cache.
    #[test]
    fn listed_models_are_kept_newest_first_and_shared() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let mut app = context_inspector_test_app(false);
        app.account_ui.cache = Some(cache.clone());
        let managed = app.account_ui.add_managed_fixture(Provider::Codex, 3);
        let model = |id: &str| crate::chat::Model {
            id: id.into(),
            label: id.into(),
            reasoning: Vec::new(),
        };
        let catalog = |observed_at, id: &str| Catalog {
            models: vec![model(id)],
            observed_at,
        };
        let now = now_ms();
        let _ = app.update(Message::AccountUi(Action::Loaded(Loaded {
            registry: Ok(app.account_ui.registry.clone()),
            restored: Vec::new(),
            catalogs: BTreeMap::from([(Provider::Codex, catalog(now - 10_000, "cached"))]),
            provisional: false,
        })));
        let listed = |app: &Cagents, provider| {
            app.account_ui
                .catalog(provider)
                .map(|catalog| catalog.models[0].id.clone())
        };
        assert_eq!(listed(&app, Provider::Codex).as_deref(), Some("cached"));

        // A probe's list replaces an older one, and is not written again:
        // the probe cached it.
        let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "ok", now - 5_000);
        snapshot.source = UsageSource::Probe;
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: managed.clone(),
            outcome: Outcome::Answered {
                identity: None,
                snapshot: snapshot.clone(),
                catalog: Some(catalog(now - 5_000, "probed")),
            },
        }));
        assert_eq!(listed(&app, Provider::Codex).as_deref(), Some("probed"));
        assert!(app.account_ui.catalogs_unsaved.is_empty());
        // An older list, from another window's cache, does not rewind it.
        assert!(
            !app.account_ui
                .take_catalog(Provider::Codex, catalog(1, "older"))
        );

        // A conversation's list is newer still, and is queued for the cache.
        let _ = app.observe_listed_models(Provider::Omp, Vec::new());
        assert!(app.account_ui.catalog(Provider::Omp).is_none(), "no list");
        let _ = app.observe_listed_models(Provider::Codex, vec![model("live")]);
        assert_eq!(listed(&app, Provider::Codex).as_deref(), Some("live"));
        assert!(app.account_ui.saving_live);
        let catalogs = app.account_ui.take_live_catalogs();
        assert!(app.account_ui.catalogs_unsaved.is_empty());
        store_live(&cache, Vec::new(), catalogs);
        assert_eq!(
            cache.catalogs(now_ms())[&Provider::Codex].models,
            [model("live")]
        );
    }

    /// Figures a running conversation reports refine the reading of the
    /// account its launch points at, count as a probe - a focused window's
    /// tick asks every other account that is due, but not that one - and
    /// reach other windows through the cache.
    #[test]
    fn a_conversations_live_figures_refresh_its_account_and_reach_the_cache() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path());
        let mut app = context_inspector_test_app(false);
        app.account_ui.cache = Some(cache.clone());
        app.refresh_ui.focused = true;
        let managed = app.account_ui.add_managed_fixture(Provider::Claude, 7);
        let system = Account::system(Provider::Claude).id();
        let recipe = recipe_on(&app, &managed);
        let now = now_ms();

        // Streamed windows alone are not a reading: the account without one
        // waits for its probe, and nothing is queued for the cache.
        let _ = app.account_reported(Provider::Claude, &recipe, vec![(claude_report(0.2), now)]);
        let state = app.account_ui.account(&managed).unwrap();
        assert!(state.snapshot.is_none());
        assert!(app.account_ui.live_unsaved.is_empty());

        // A probe long enough ago that the account is due again.
        let mut probed =
            UsageSnapshot::state(UsageStatus::Ok, "", now - 2 * schedule::REFRESH_INTERVAL);
        probed.detail = None;
        probed.plan = Some("max".into());
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: managed.clone(),
            outcome: Outcome::Answered {
                catalog: None,
                identity: None,
                snapshot: probed,
            },
        }));
        let _ = app.account_reported(Provider::Claude, &recipe, vec![(claude_report(0.43), now)]);
        let state = app.account_ui.account(&managed).unwrap();
        let snapshot = state.snapshot.clone().unwrap();
        assert_eq!(snapshot.source, UsageSource::Live);
        assert_eq!(snapshot.observed_at, now);
        assert!((snapshot.windows[0].used - 0.43).abs() < 1e-6);
        assert_eq!(
            snapshot.plan.as_deref(),
            Some("max"),
            "the probe's plan is kept"
        );
        assert!(app.account_ui.account(&system).unwrap().snapshot.is_none());
        let _ = app.update(Message::AccountUi(Action::Tick));
        let in_flight = |app: &Cagents, id: &AccountId| {
            app.account_ui
                .account(id)
                .unwrap()
                .schedule
                .in_flight(now_ms())
        };
        assert!(in_flight(&app, &system), "a due account is probed");
        assert!(!in_flight(&app, &managed), "a reported one is not");

        // A report older than what the account shows does not rewind it.
        let _ = app.account_reported(
            Provider::Claude,
            &recipe,
            vec![(claude_report(0.9), now - 1_000)],
        );
        let state = app.account_ui.account(&managed).unwrap();
        assert!((state.snapshot.as_ref().unwrap().windows[0].used - 0.43).abs() < 1e-6);

        // The write is coalesced: queued once, written when it comes due.
        let _ = app.account_reported(
            Provider::Claude,
            &recipe,
            vec![(claude_report(0.5), now + 1)],
        );
        assert!(app.account_ui.saving_live);
        assert_eq!(app.account_ui.live_unsaved.len(), 1);
        let entries = app.account_ui.take_live_entries();
        assert!(app.account_ui.live_unsaved.is_empty());
        store_live(&cache, entries, Vec::new());
        let key = app
            .account_ui
            .account(&managed)
            .unwrap()
            .account
            .cache_key()
            .unwrap();
        let cached = cache.load(now_ms()).remove(&key).unwrap();
        assert!((cached.snapshot.windows[0].used - 0.5).abs() < 1e-6);
        assert_eq!(cached.snapshot.observed_at, now + 1);
    }

    /// The startup load does not wait on the shell: an account whose CLI only
    /// the shell's `PATH` finds is taken as absent at first, and gets its
    /// cached figures once the shell answers - unless a probe answered in
    /// between, which knows better.
    #[test]
    fn an_account_found_on_the_shells_path_gets_its_cached_figures_late() {
        let mut app = context_inspector_test_app(false);
        let claude = Account::system(Provider::Claude).id();
        let codex = Account::system(Provider::Codex).id();
        let now = now_ms();
        let cached = |used| Entry {
            identity: None,
            snapshot: UsageSnapshot {
                windows: vec![UsageWindow {
                    key: "five_hour".into(),
                    kind: WindowKind::Session,
                    label: "Session".into(),
                    used,
                    resets_at: None,
                    span_secs: None,
                    binding: false,
                }],
                credits: None,
                status: UsageStatus::Ok,
                observed_at: now,
                source: UsageSource::Probe,
                plan: None,
                detail: None,
            },
        };
        let judged = |present| {
            vec![
                Restored {
                    id: claude.clone(),
                    present,
                    entry: Some(cached(0.3)),
                },
                Restored {
                    id: codex.clone(),
                    present,
                    entry: Some(cached(0.3)),
                },
            ]
        };
        let _ = app.update(Message::AccountUi(Action::Loaded(Loaded {
            registry: Ok(Registry::default()),
            restored: judged(false),
            catalogs: BTreeMap::new(),
            provisional: true,
        })));
        for id in [&claude, &codex] {
            let state = app.account_ui.account(id).unwrap();
            assert!(!state.present);
            assert!(state.snapshot.is_none());
        }

        let mut probed = cached(0.8).snapshot;
        probed.observed_at = now + 1;
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: codex.clone(),
            outcome: Outcome::Answered {
                catalog: None,
                identity: None,
                snapshot: probed.clone(),
            },
        }));
        let _ = app.update(Message::AccountUi(Action::Located(judged(true))));
        let state = app.account_ui.account(&claude).unwrap();
        assert!(state.present);
        let restored = state.snapshot.as_ref().unwrap();
        assert_eq!(restored.source, UsageSource::Cache);
        assert!((restored.windows[0].used - 0.3).abs() < 1e-6);
        assert_eq!(
            app.account_ui.account(&codex).unwrap().snapshot.as_ref(),
            Some(&probed)
        );
    }

    /// A CLI the interface's own `PATH` found but the shell's does not is
    /// not there to ask after all, and a popover open on it closes.
    #[test]
    fn an_account_the_shells_path_rules_out_loses_its_popover() {
        let mut app = context_inspector_test_app(false);
        let claude = Account::system(Provider::Claude).id();
        let judged = |present| {
            vec![Restored {
                id: claude.clone(),
                present,
                entry: None,
            }]
        };
        let _ = app.update(Message::AccountUi(Action::Loaded(Loaded {
            registry: Ok(Registry::default()),
            restored: judged(true),
            catalogs: BTreeMap::new(),
            provisional: true,
        })));
        let _ = app.update(Message::AccountUi(Action::Popover(Some(claude.clone()))));
        assert_eq!(app.account_ui.popover.as_ref(), Some(&claude));
        let _ = app.update(Message::AccountUi(Action::Located(judged(false))));
        assert!(!app.account_ui.account(&claude).unwrap().present);
        assert_eq!(app.account_ui.popover, None);
    }

    /// A conversation says its account's login changed: the figures shown
    /// no longer describe it, so the account is asked again at once.
    #[test]
    fn a_changed_login_reported_live_is_checked_at_once() {
        let mut app = context_inspector_test_app(false);
        app.refresh_ui.focused = true;
        let id = Account::system(Provider::Codex).id();
        let now = now_ms();
        let _ = app.update(Message::AccountUi(Action::Probed {
            id: id.clone(),
            outcome: Outcome::Answered {
                catalog: None,
                identity: None,
                snapshot: UsageSnapshot::state(UsageStatus::NeedsLogin, "signed out", now),
            },
        }));
        let recipe = recipe_on(&app, &id);
        let login = |chatgpt| {
            LiveReport::CodexLogin(account::codex::LoginUpdate {
                chatgpt,
                plan: None,
            })
        };
        // Still signed out: nothing to ask.
        let _ = app.account_reported(Provider::Codex, &recipe, vec![(login(false), now)]);
        assert!(!app.account_ui.account(&id).unwrap().schedule.in_flight(now));
        let _ = app.account_reported(Provider::Codex, &recipe, vec![(login(true), now)]);
        let state = app.account_ui.account(&id).unwrap();
        assert!(state.schedule.in_flight(now_ms()));
        assert_eq!(
            state.snapshot.as_ref().unwrap().status,
            UsageStatus::NeedsLogin,
            "the login report is not itself a reading"
        );
    }

    /// A manual refresh knows the moment its debounce lifts, and the wake
    /// scheduled for that moment re-reads the clock, which is what re-enables
    /// the refresh control without waiting for the next tick.
    #[test]
    fn a_manual_refreshs_debounce_ends_in_a_wake_that_rereads_the_clock() {
        let mut app = context_inspector_test_app(false);
        let id = app.account_ui.accounts[0].id.clone();
        let _ = app.update(Message::AccountUi(Action::Refresh(id.clone())));
        let state = app.account_ui.account(&id).unwrap();
        let ready_at = state.schedule.manual_ready_at().unwrap();
        assert!(!state.schedule.manual_ready(app.account_ui.now_ms));
        app.account_ui.now_ms = 0;
        let _ = app.update(Message::AccountUi(Action::ManualReady));
        assert!(app.account_ui.now_ms >= ready_at - schedule::MANUAL_DEBOUNCE);
    }

    #[test]
    fn the_default_account_is_chosen_renamed_and_offered_to_new_conversations() {
        let mut app = context_inspector_test_app(false);
        let system = Account::system(Provider::Codex);
        let managed = app.account_ui.add_managed_fixture(Provider::Codex, 8);
        assert_eq!(app.default_account(Provider::Codex), system);
        let _ = app.update(Message::AccountUi(Action::SetDefault(managed.clone())));
        assert_eq!(app.default_account(Provider::Codex).id(), managed);
        let choices = app.default_account_choices();
        let codex = &choices
            .iter()
            .find(|(p, _)| *p == Provider::Codex)
            .unwrap()
            .1;
        assert!(
            codex
                .iter()
                .any(|choice| choice.id == managed && choice.selected)
        );

        let _ = app.update(Message::AccountUi(Action::Rename(managed.clone())));
        let _ = app.update(Message::AccountUi(Action::RenameInput("  Work\n".into())));
        let _ = app.update(Message::AccountUi(Action::RenameCommit));
        let state = app.account_ui.account(&managed).unwrap();
        assert_eq!(app.account_ui.display_name(state), "Work");

        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "codex".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/w"),
            environment: BTreeMap::new(),
        };
        let accounts = app
            .chat_accounts(Provider::Codex, &recipe)
            .expect("two accounts are a choice");
        assert_eq!(accounts.choices.len(), 2);
        assert!(accounts.choices.iter().any(|choice| choice.label == "Work"));
        assert!(
            app.chat_accounts(
                Provider::Claude,
                &LaunchRecipe {
                    program: "claude".into(),
                    ..recipe
                }
            )
            .is_none(),
            "one system account is nothing to choose"
        );
    }

    #[test]
    fn a_new_shell_pins_the_selected_codex_home_and_keeps_it_after_default_changes() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("codex-home");
        std::fs::create_dir(&home).unwrap();
        let mut app = context_inspector_test_app(false);
        let key = uuid::Uuid::new_v4();
        let mut registry = app.account_ui.registry.clone();
        registry
            .link(
                Provider::Codex,
                key,
                home.clone(),
                None,
                app.account_ui.homes.as_ref(),
                1,
            )
            .unwrap();
        let linked = Account::linked(Provider::Codex, key, home.clone());
        let login = login_launch(
            &linked,
            Path::new("/usr/bin/codex"),
            directory.path().into(),
            false,
        )
        .unwrap();
        assert!(login.unset_environment.contains("CODEX_API_KEY"));
        let system_login = login_launch(
            &Account::system(Provider::Codex),
            Path::new("/usr/bin/codex"),
            directory.path().into(),
            false,
        )
        .unwrap();
        assert!(system_login.unset_environment.is_empty());
        registry.set_default(&linked);
        app.account_ui.set_registry(registry);
        let _ = app.add_terminal(directory.path().to_path_buf());
        let launch = app
            .durable
            .cards
            .iter()
            .find_map(|card| match &card.kind {
                CardKind::Terminal(session) => Some(session.launch.clone()),
                _ => None,
            })
            .expect("a new terminal was recorded");
        assert_eq!(
            launch.environment.get("CODEX_HOME"),
            Some(&home.display().to_string())
        );
        assert!(launch.unset_environment.contains("CODEX_API_KEY"));
        assert!(launch.unset_environment.contains("OPENAI_API_KEY"));
        assert!(!launch.unset_environment.contains("ANTHROPIC_API_KEY"));
        let mut registry = app.account_ui.registry.clone();
        registry.set_default(&Account::system(Provider::Codex));
        app.account_ui.set_registry(registry);
        assert_eq!(
            launch.environment.get("CODEX_HOME"),
            Some(&home.display().to_string())
        );
    }

    #[test]
    fn project_choice_routes_new_shells_and_chats_without_changing_other_projects() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("redact-history");
        std::fs::create_dir(&home).unwrap();
        let mut app = app_in_repo();
        let project = app.sidebar_model.projects[0].id;
        let key = uuid::Uuid::new_v4();
        let linked = Account::linked(Provider::Codex, key, home.clone());
        let mut registry = app.account_ui.registry.clone();
        registry
            .link(
                Provider::Codex,
                key,
                home.clone(),
                None,
                app.account_ui.homes.as_ref(),
                1,
            )
            .unwrap();
        registry.set_project_default(&app.sidebar_model.projects[0].repository, &linked);
        app.account_ui.set_registry(registry);
        assert_eq!(
            app.default_account_for_directory(Provider::Codex, Path::new("/repo/subdir")),
            linked
        );
        assert_eq!(
            app.default_account_for_directory(Provider::Codex, Path::new("/other")),
            Account::system(Provider::Codex)
        );

        let _ = app.add_terminal(PathBuf::from("/repo"));
        let shell = app
            .durable
            .cards
            .iter()
            .find_map(|card| match &card.kind {
                CardKind::Terminal(session) => Some(&session.launch),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            shell.environment.get("CODEX_HOME"),
            Some(&home.display().to_string())
        );

        let _ = app.new_native_chat(Provider::Codex, Some(PathBuf::from("/repo")));
        let chat = app
            .durable
            .cards
            .iter()
            .find_map(|card| match &card.kind {
                CardKind::Chat(session) => Some(&session.launch.recipe),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            chat.environment.get("CODEX_HOME"),
            Some(&home.display().to_string())
        );
        app.sidebar_model.projects[0].repository = PathBuf::from("/repo/.git");
        let mut registry = app.account_ui.registry.clone();
        registry.set_project_default(Path::new("/repo/.git"), &linked);
        app.account_ui.set_registry(registry);
        app.sidebar_model.projects[0].worktrees.clear();
        assert_eq!(
            app.default_account_for_directory(Provider::Codex, Path::new("/repo")),
            linked,
            "a channel at the repository root keeps its project choice when no worktree is listed"
        );
    }

    #[test]
    fn accounts_project_uses_the_selected_channel_when_its_workspace_path_is_stale() {
        let mut app = app_in_repo();
        let project = app.sidebar_model.projects[0].id;
        let channel = crate::model::ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: channel,
                project_id: project,
                title: "discussion".into(),
                ordinal: 1,
                shared: true,
                cards: Vec::new(),
                focused: None,
            });
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        app.active_workspace = Some(PathBuf::from("/unrelated"));

        assert_eq!(
            app.accounts_project(),
            Some((project, "repo", Path::new("/repo")))
        );

        Arc::make_mut(&mut app.durable).active_channel = None;
        assert_eq!(app.accounts_project(), None);
    }

    #[test]
    fn first_account_load_migrates_a_saved_project_choice_before_new_work_starts() {
        let mut app = app_in_repo();
        let project = app.sidebar_model.projects[0].id;
        let old = format!("version = 3\n[project_defaults.\"{project}\"]\ncodex = \"system\"\n");
        let registry: Registry = toml::from_str(&old).unwrap();
        let _ = app.update(Message::AccountUi(Action::Loaded(Loaded {
            registry: Ok(registry),
            restored: Vec::new(),
            catalogs: BTreeMap::new(),
            provisional: false,
        })));

        assert_eq!(
            app.account_ui
                .registry
                .project_choice(Path::new("/repo"), Provider::Codex),
            Some("system")
        );
    }

    #[test]
    fn delayed_account_save_cannot_restore_an_older_choice_in_the_window() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("config"));
        let project = crate::model::ProjectId::new();
        let first = store
            .update(|registry| {
                registry.set_project_default(Path::new("/repo"), &Account::system(Provider::Codex));
            })
            .unwrap()
            .0;
        let second = store
            .update(|registry| {
                registry
                    .set_project_default(Path::new("/repo"), &Account::system(Provider::Claude));
            })
            .unwrap()
            .0;
        assert!(second.revision() > first.revision());
        let mut state = State::default();
        state.set_registry(second.clone());
        state.set_registry(first);
        assert_eq!(state.registry, second);
    }

    #[test]
    fn linked_homes_with_the_same_login_have_distinct_picker_names() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = context_inspector_test_app(false);
        let mut registry = app.account_ui.registry.clone();
        let mut ids = Vec::new();
        for name in ["codex-personal-1", "codex-personal-2"] {
            let home = directory.path().join(name);
            std::fs::create_dir(&home).unwrap();
            let key = uuid::Uuid::new_v4();
            registry
                .link(
                    Provider::Codex,
                    key,
                    home,
                    None,
                    app.account_ui.homes.as_ref(),
                    1,
                )
                .unwrap();
            registry.observe_identity(
                key,
                &Identity {
                    email: Some("same@example.com".into()),
                    ..Identity::default()
                },
            );
            ids.push(Account::linked(Provider::Codex, key, PathBuf::new()).id());
        }
        app.account_ui.set_registry(registry);
        let names: Vec<String> = ids
            .iter()
            .map(|id| {
                app.account_ui
                    .display_name(app.account_ui.account(id).unwrap())
            })
            .collect();
        assert_ne!(names[0], names[1]);
        assert!(names[0].starts_with("codex-personal-1 · "));
        assert!(names[1].starts_with("codex-personal-2 · "));
        assert!(names.iter().all(|name| name.ends_with("same@example.com")));
    }

    #[test]
    fn only_a_managed_account_can_be_removed_and_only_after_confirming() {
        let mut app = context_inspector_test_app(false);
        let system = signed_out(&mut app);
        let _ = app.update(Message::AccountUi(Action::Remove(system)));
        assert_eq!(app.account_ui.table.confirm_remove, None);
        let managed = app.account_ui.add_managed_fixture(Provider::Claude, 4);
        let _ = app.update(Message::AccountUi(Action::Remove(managed.clone())));
        assert_eq!(app.account_ui.table.confirm_remove.as_ref(), Some(&managed));
        let _ = app.update(Message::AccountUi(Action::RemoveConfirm(managed.clone())));
        assert!(app.account_ui.removing.contains(&managed));
        let mut remaining = app.account_ui.registry.clone();
        remaining.unregister(uuid::Uuid::from_u128(4));
        // Its files could not all be deleted, but it is forgotten: the
        // table follows the registry, and the reader hears about the rest.
        let _ = app.update(Message::AccountUi(Action::Removed {
            id: managed.clone(),
            registry: Some(remaining),
            error: Some("its files could not be deleted: busy".into()),
        }));
        assert!(app.account_ui.account(&managed).is_none());
        assert!(app.account_ui.removing.is_empty());
        assert!(app.notices.entries.iter().any(|entry| {
            entry
                .notice
                .message
                .contains("could not be removed completely")
        }));
    }

    #[test]
    fn removing_an_account_counts_the_cards_open_on_it() {
        let mut app = context_inspector_test_app(false);
        let managed = app.account_ui.add_managed_fixture(Provider::Claude, 5);
        let home = app
            .account_ui
            .account(&managed)
            .and_then(|state| state.account.resolved_home())
            .expect("a managed account has a home");
        let launch = LaunchRecipe {
            unset_environment: Default::default(),
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/"),
            environment: BTreeMap::from([(
                account::home_variable(Provider::Claude).unwrap().to_owned(),
                home.display().to_string(),
            )]),
        };
        let _ = app.insert_terminal_card(fresh_terminal_session(launch), None);
        let _ = app.update(Message::AccountUi(Action::Remove(managed.clone())));
        assert_eq!(app.account_ui.table.confirm_in_use, 1);
    }

    #[test]
    fn only_a_provider_with_accounts_offers_a_choice_of_them() {
        let mut app = context_inspector_test_app(false);
        app.account_ui.add_managed_fixture(Provider::Claude, 6);
        let recipe = LaunchRecipe {
            unset_environment: Default::default(),
            program: "omp".into(),
            arguments: Vec::new(),
            working_directory: PathBuf::from("/w"),
            environment: BTreeMap::new(),
        };
        assert!(app.chat_accounts(Provider::Omp, &recipe).is_none());
        assert!(app.chat_accounts(Provider::Claude, &recipe).is_some());
    }

    fn pending_login(app: &mut Cagents, key: u128) -> uuid::Uuid {
        let key = uuid::Uuid::from_u128(key);
        app.account_ui.logins.push(Login {
            provider: Provider::Claude,
            key,
            home: PathBuf::from("/cagents-fixture/accounts/claude").join(key.to_string()),
            phase: LoginPhase::Waiting,
            attempts: 0,
            next_check: 0,
            started_at: now_ms(),
            recovered: false,
        });
        key
    }

    #[test]
    fn a_sign_in_that_cannot_be_confirmed_is_retried_then_left_pending() {
        let mut app = context_inspector_test_app(false);
        let key = pending_login(&mut app, 14);
        for attempt in 1..=LOGIN_ATTEMPTS {
            app.account_ui.logins[0].next_check = 0;
            let _ = app.update(Message::AccountUi(Action::LoginTick));
            assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Verifying);
            assert_eq!(app.account_ui.logins[0].attempts, attempt);
            let _ = app.update(Message::AccountUi(Action::LoginVerified {
                key,
                outcome: Outcome::Failed("not now".into()),
            }));
            if attempt < LOGIN_ATTEMPTS {
                let login = &app.account_ui.logins[0];
                assert_eq!(login.phase, LoginPhase::Waiting);
                assert!(login.next_check > now_ms(), "it waits before asking again");
            }
        }
        // Never discarded: its home may hold a login that simply could not
        // be confirmed, and the next start asks again.
        assert!(app.account_ui.logins.is_empty());
        assert!(
            app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.message.contains("check again"))
        );
    }

    #[test]
    fn sign_ins_left_pending_are_taken_up_only_when_no_window_can_still_run_them() {
        let mut app = context_inspector_test_app(false);
        app.account_ui.homes = Some(Homes::at("/cagents-fixture/accounts"));
        let now = now_ms();
        let (young, old) = (uuid::Uuid::from_u128(21), uuid::Uuid::from_u128(22));
        let mut registry = app.account_ui.registry.clone();
        registry.begin(Provider::Codex, young, now - 60_000);
        registry.begin(Provider::Claude, old, now - 2 * RECOVER_AFTER);
        app.account_ui.set_registry(registry);
        app.recover_logins();
        let keys: Vec<_> = app
            .account_ui
            .logins
            .iter()
            .map(|login| login.key)
            .collect();
        assert_eq!(keys, [old], "a recent sign-in may be another window's");
        assert!(app.account_ui.logins[0].recovered);

        // Signed out, but younger than a sign-in can last: left alone.
        let _ = app.update(Message::AccountUi(Action::LoginTick));
        let _ = app.update(Message::AccountUi(Action::LoginVerified {
            key: old,
            outcome: Outcome::Answered {
                catalog: None,
                identity: None,
                snapshot: UsageSnapshot::state(UsageStatus::NeedsLogin, "signed out", now),
            },
        }));
        assert!(app.account_ui.logins.is_empty());
        assert_eq!(app.account_ui.registry.pending().len(), 2);
    }

    #[test]
    fn a_sign_in_whose_terminal_is_gone_is_verified_and_a_failed_one_forgotten() {
        let mut app = context_inspector_test_app(false);
        let key = pending_login(&mut app, 11);
        // No card shows the login's terminal: it was closed.
        let _ = app.update(Message::AccountUi(Action::LoginTick));
        assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Verifying);
        let _ = app.update(Message::AccountUi(Action::LoginVerified {
            key,
            outcome: Outcome::Answered {
                catalog: None,
                identity: None,
                snapshot: UsageSnapshot::state(UsageStatus::NeedsLogin, "signed out", NOW),
            },
        }));
        assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Settling);
        let _ = app.update(Message::AccountUi(Action::LoginSettled {
            key,
            result: Ok(Settled::Abandoned(None)),
        }));
        assert!(app.account_ui.logins.is_empty());
        assert!(app.account_ui.registry.registered().is_empty());
    }

    #[test]
    fn a_verified_sign_in_joins_the_accounts_with_its_figures() {
        let mut app = context_inspector_test_app(false);
        let key = pending_login(&mut app, 12);
        let mut registry = app.account_ui.registry.clone();
        registry
            .register(
                registry::Registered {
                    key,
                    provider: Provider::Claude,
                    linked_home: None,
                    label: None,
                    email: Some("new@example.com".into()),
                    organization: None,
                    added_at: NOW,
                },
                &[],
            )
            .unwrap();
        app.account_ui.homes = Some(Homes::at("/cagents-fixture/accounts"));
        let identity = Identity {
            email: Some("new@example.com".into()),
            organization: None,
            plan: Some("max".into()),
        };
        let _ = app.update(Message::AccountUi(Action::LoginSettled {
            key,
            result: Ok(Settled::Registered {
                registry,
                identity: Some(identity.clone()),
                snapshot: Some(UsageSnapshot::state(UsageStatus::Ok, "", NOW)),
            }),
        }));
        assert!(app.account_ui.logins.is_empty());
        let state = app
            .account_ui
            .accounts
            .iter()
            .find(|state| state.account.registry_key() == Some(key))
            .expect("the account is listed");
        assert!(state.present);
        assert_eq!(state.identity.as_ref(), Some(&identity));
        assert_eq!(app.account_ui.display_name(state), "new@example.com");
    }

    #[test]
    fn a_duplicate_sign_in_is_reported_and_not_listed() {
        let mut app = context_inspector_test_app(false);
        let system = signed_out(&mut app);
        let key = pending_login(&mut app, 13);
        let _ = app.update(Message::AccountUi(Action::LoginSettled {
            key,
            result: Ok(Settled::Duplicate {
                existing: system,
                registry: None,
            }),
        }));
        assert!(app.account_ui.logins.is_empty());
        assert!(app.account_ui.registry.registered().is_empty());
        assert!(
            app.notices
                .entries
                .iter()
                .any(|entry| entry.notice.message.contains("already added")),
            "the reader is told which account it duplicates"
        );
    }

    /// What Omp would have recorded for one login's five-hour window.
    fn recorded(service: &str, email: &str, used: f64, recorded_at: Millis) -> omp::Row {
        omp::Row {
            recorded_at: recorded_at as i64,
            service: service.into(),
            account_key: format!("oauth|email:{email}"),
            email: Some(email.into()),
            limit_id: "primary".into(),
            label: Some("5 Hour".into()),
            window_label: Some("5 Hours".into()),
            used_fraction: Some(used),
            status: Some("ok".into()),
            resets_at: None,
        }
    }

    fn brokered(app: &Cagents) -> Vec<&AccountState> {
        app.account_ui
            .accounts
            .iter()
            .filter(|state| state.account.is_brokered())
            .collect()
    }

    #[test]
    fn omps_accounts_come_and_go_with_its_record_and_are_only_read() {
        let mut app = context_inspector_test_app(false);
        let now = now_ms();
        let both = omp::accounts(
            [
                recorded("openai-codex", "b@example.com", 0.5, now - 2_000),
                recorded("anthropic", "a@example.com", 0.25, now - 1_000),
            ],
            now,
        );
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Read(
            both.clone(),
        ))));
        let listed = brokered(&app);
        assert_eq!(listed.len(), 2);
        let (anthropic, codex) = (listed[0].id.clone(), listed[1].id.clone());
        // The login Omp used last leads, as the status bar's chip.
        assert_eq!(
            app.account_ui.display_name(listed[0]),
            "Anthropic · a@example.com"
        );
        assert!(listed.iter().all(|state| state.present));
        let snapshot = listed[0].snapshot.as_ref().unwrap();
        assert_eq!(snapshot.source, UsageSource::Recorded);
        assert_eq!(snapshot.windows[0].used, 0.25);
        assert!(app.status_bar().is_some());
        assert!(app.provider_chip(Provider::Omp, now, false).is_some());
        // Omp names and forgets its own accounts.
        let _ = app.update(Message::AccountUi(Action::Remove(anthropic.clone())));
        assert_eq!(app.account_ui.table.confirm_remove, None);
        let _ = app.update(Message::AccountUi(Action::Rename(anthropic.clone())));
        assert_eq!(app.account_ui.table.rename, None);
        // A record that cannot be read keeps what was read before it.
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Unreadable(
            "Omp's usage record is damaged".into(),
        ))));
        let state = app.account_ui.account(&anthropic).unwrap();
        assert!(state.snapshot.is_some());
        assert_eq!(
            state.failure.as_deref(),
            Some("Omp's usage record is damaged")
        );
        // A login the record no longer lists goes, and a read clears the
        // failure of those that stay.
        let _ = app.update(Message::AccountUi(Action::Popover(Some(codex.clone()))));
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Read(
            both.into_iter()
                .filter(|brokered| brokered.account.id() == anthropic)
                .collect(),
        ))));
        assert!(app.account_ui.account(&codex).is_none());
        assert_eq!(app.account_ui.popover, None);
        assert_eq!(app.account_ui.account(&anthropic).unwrap().failure, None);
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Missing)));
        assert!(brokered(&app).is_empty());
        assert!(app.account_ui.omp.accounts.is_empty());
    }

    #[test]
    fn omps_record_is_read_while_someone_looks_but_not_straight_after_a_read() {
        let now = 1_000_000;
        let mut record = OmpRecord::default();
        assert!(!record.due(Trigger::Manual, now, true), "nowhere to read");
        record.database = Some(PathBuf::from("/nonexistent/agent.db"));
        assert!(record.due(Trigger::Tick, now, true));
        assert!(!record.due(Trigger::Tick, now, false));
        assert!(record.due(Trigger::Manual, now, false));
        record.read_at = Some(now);
        assert!(!record.due(Trigger::Focus, now + OMP_REREAD - 1, true));
        assert!(record.due(Trigger::Tick, now + OMP_REREAD, true));
        assert!(record.due(Trigger::Manual, now + 1, true));
        record.reading = true;
        assert!(!record.due(Trigger::Manual, now + OMP_REREAD, true));
    }

    #[test]
    fn refreshing_one_of_omps_accounts_reads_the_record_for_all_of_them() {
        let mut app = context_inspector_test_app(false);
        let now = now_ms();
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Read(
            omp::accounts(
                [
                    recorded("anthropic", "a@example.com", 0.25, now - 1_000),
                    recorded("xai-oauth", "c@example.com", 0.1, now - 3_000),
                ],
                now,
            ),
        ))));
        let id = brokered(&app)[0].id.clone();
        // A record that is no longer there is found out by the read.
        app.account_ui.omp.database = Some(PathBuf::from("/nonexistent/agent.db"));
        let _ = app.update(Message::AccountUi(Action::Refresh(id.clone())));
        assert!(app.account_ui.omp.reading);
        let later = now_ms();
        assert!(
            brokered(&app)
                .iter()
                .all(|state| state.schedule.in_flight(later) && !state.schedule.manual_ready(later))
        );
        // A second request while the first is being read is the same read.
        let _ = app.update(Message::AccountUi(Action::Refresh(id)));
        assert!(app.account_ui.omp.reading);
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Missing)));
        assert!(!app.account_ui.omp.reading);
        assert!(brokered(&app).is_empty());
    }

    #[test]
    fn a_record_omp_keeps_in_an_unknown_form_is_reported_before_any_account_is_read() {
        let mut app = context_inspector_test_app(false);
        let problem = "Omp's usage record is in a form cagents does not know";
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Unreadable(
            problem.into(),
        ))));
        assert!(brokered(&app).is_empty());
        assert_eq!(app.account_ui.omp_problem(), Some(problem));
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Missing)));
        assert_eq!(app.account_ui.omp_problem(), None);
    }

    /// A tick reads the record every few seconds, and a check that lands in
    /// milliseconds is not worth showing that often.
    #[test]
    fn a_tick_reads_omps_record_without_showing_a_check_under_way() {
        let mut app = context_inspector_test_app(false);
        let now = now_ms();
        let _ = app.update(Message::AccountUi(Action::OmpRead(History::Read(
            omp::accounts(
                [recorded("anthropic", "a@example.com", 0.25, now - 1_000)],
                now,
            ),
        ))));
        app.refresh_ui.focused = true;
        app.account_ui.omp.database = Some(PathBuf::from("/nonexistent/agent.db"));
        app.account_ui.omp.read_at = None;
        assert!(app.read_omp(Trigger::Tick, now).is_some());
        assert!(app.account_ui.omp.reading);
        let state = brokered(&app)[0];
        assert!(!state.schedule.in_flight(now) && state.schedule.manual_ready(now));
    }

    #[test]
    fn omps_record_is_read_at_start_apart_from_the_accounts_and_before_focus() {
        let mut app = context_inspector_test_app(false);
        app.refresh_ui.focused = false;
        app.account_ui.omp.database = Some(PathBuf::from("/nonexistent/agent.db"));
        let _ = app.load_accounts();
        assert!(app.account_ui.omp.reading);
        let _ = app.load_accounts();
        assert!(
            app.read_omp(Trigger::Manual, now_ms()).is_none(),
            "a read under way is not started again"
        );
    }
}
