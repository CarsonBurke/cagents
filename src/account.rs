//! Provider accounts and the usage limits each one is working against.
//!
//! An account is a provider plus the home directory its CLI keeps its login
//! in. Ordinary account metadata comes from that CLI: its identity
//! from `claude auth status` or Codex's `account/read`, and its limits from
//! Claude's `get_usage` control request or Codex's `account/rateLimits/read`.
//! Omp keeps several logins of its own and picks between them itself; its
//! accounts, and their limits, come from the usage record it keeps.
//! The reset transport is a narrow exception for Claude's private reset API:
//! it reads the stored subscription bearer in memory and never copies or
//! refreshes it. Login remains owned by the provider CLI. The account model
//! contains only non-secret metadata.
//!
//! The model is shared by every lane. The parsers are pure functions over the
//! JSON a CLI printed, so they are fixture-tested here. A running native chat
//! forwards the account frames its CLI already streams as a [`LiveReport`],
//! which spares a probe while the conversation is busy. Probing a CLI is the
//! interface's job and lives in [`probe`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::Provider;
use crate::chat::local_time;

#[cfg(any(feature = "ui", feature = "host"))]
pub mod cache;
pub mod claude;
pub mod codex;
#[cfg(feature = "host")]
pub mod handoff;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod home;
pub mod info;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod login;
pub mod omp;
pub mod preferences;
pub mod resets;
pub use resets::{ResetKind, ResetScope, UsageReset};
#[cfg(any(feature = "ui", feature = "host"))]
pub mod probe;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod registry;
pub mod remote;
pub mod routing;
pub mod schedule;
#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) mod scope;

/// A used fraction past which an account is reported as approaching its
/// limit. It is the same point past which the shell draws a budget in its
/// warning tone, so a status and a ring cannot disagree.
pub const WARNING_FRACTION: f32 = 0.75;

/// The most of a window a provider is believed about. A window can run past
/// its limit - Claude reports overage above one - but a figure beyond this
/// is noise, and clamping it keeps it finite once narrowed to `f32`.
const MAX_FRACTION: f64 = 10.0;

/// A used fraction read from a provider, which the parsers have already
/// checked is finite, clamped to what a window can plausibly report.
fn fraction(value: f64) -> f32 {
    value.clamp(0.0, MAX_FRACTION) as f32
}

/// Where an account's CLI keeps its login.
///
/// The system default is whatever the CLI would use on its own - the
/// `CLAUDE_CONFIG_DIR` or `CODEX_HOME` cagents itself was started with, and
/// otherwise the CLI's documented default under the user's home - except a
/// home cagents selected for the launch cagents was started from, which is
/// that account's and never the system's (see [`inherited_selections`]). It
/// is never moved or rewritten. A managed home is a directory cagents created for a
/// further login of the same provider; the CLI is pointed at it through the
/// same variable, so two accounts never share a credential store.
///
/// A managed home is named by the key cagents gave it, and its path is kept
/// beside the key only so nothing has to recompute it: the key alone decides
/// the account's identity, so the same account keeps its name however the
/// data directory is spelled.
///
/// A brokered account is one a CLI keeps among several of its own and picks
/// between itself, as Omp does: cagents has no home to point it at, only the
/// record the CLI keeps of each account's usage. It is named by the service
/// the login is for and the key the CLI files it under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AccountHome {
    System,
    /// A selected daemon's system login, resolved on that host.
    SystemAt {
        path: PathBuf,
    },
    Managed {
        key: uuid::Uuid,
        path: PathBuf,
    },
    /// A provider home the reader linked from elsewhere. cagents never owns
    /// or removes its files.
    Linked {
        key: uuid::Uuid,
        path: PathBuf,
    },
    Brokered {
        service: String,
        key: String,
    },
}

/// One provider login, identified by the home its CLI keeps it in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Account {
    pub provider: Provider,
    pub home: AccountHome,
}

/// The stable name of an account inside one cagents installation.
///
/// Derived from the provider and the home rather than from anything the
/// provider reports, because an account has to be nameable before its first
/// successful probe - that is exactly when it needs a "Sign in" chip.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(String);

impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for AccountId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((provider, key)) = value.split_once('/') else {
            return Err("Use a provider/account ID, such as claude/system".into());
        };
        let brokered = provider == "omp"
            && key.split_once('/').is_some_and(|(service, digest)| {
                !service.is_empty()
                    && service.as_bytes()[0].is_ascii_alphanumeric()
                    && service.len() <= 128
                    && service.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
                    && digest.len() == 16
                    && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        if !Provider::ALL
            .iter()
            .any(|candidate| candidate.executable() == provider)
            || !(key == "system" || uuid::Uuid::parse_str(key).is_ok() || brokered)
        {
            return Err("Use a provider followed by /system or a registered account UUID".into());
        }
        Ok(Self(value.into()))
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A short, stable stand-in for a brokered account's key in its id and lock
/// name. The key is the broker's own filing name and often spells out the
/// login's address, which an id - read in logs and tests - has no need of.
fn brokered_digest(service: &str, key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::new()
        .chain_update(service.as_bytes())
        .chain_update([0])
        .chain_update(key.as_bytes())
        .finalize();
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Validate an existing-home operand without interpreting it on this device.
/// CLI callers retain the original spelling; native fields may trim it first.
pub(crate) fn validate_linked_home_operand(value: &str) -> Result<(), String> {
    // The destination's established linker trims text-field input. Refuse an
    // ambiguous CLI operand rather than silently selecting a different home.
    if value.trim() != value {
        return Err("Use an account home without leading or trailing whitespace".into());
    }
    if value
        .strip_prefix("~/")
        .is_some_and(|relative| Path::new(relative).is_absolute())
    {
        return Err("Use one slash after ~ so the account home stays relative to the selected device's home".into());
    }
    if value.contains('\0') || !(Path::new(value).is_absolute() || value.starts_with("~/")) {
        return Err(
            "Use an absolute account home on the selected device, or one starting with ~/".into(),
        );
    }
    Ok(())
}

/// Expand only on the device that owns the account registry. An absolute
/// remainder after ~/ would replace HOME during Path::join, so reject it first.
#[cfg(any(feature = "ui", feature = "host", test))]
pub(crate) fn expand_linked_home(
    value: &str,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, String> {
    validate_linked_home_operand(value)?;
    let path = if let Some(relative) = value.strip_prefix("~/") {
        PathBuf::from(home.ok_or("HOME is unavailable")?).join(relative)
    } else {
        PathBuf::from(value)
    };
    if !path.is_absolute() {
        return Err("Use an absolute account home on the selected device".into());
    }
    Ok(path)
}

/// The environment variable a provider's CLI reads its home from.
pub fn home_variable(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::Claude => Some("CLAUDE_CONFIG_DIR"),
        Provider::Codex => Some("CODEX_HOME"),
        Provider::Omp => None,
    }
}

/// Documented process credentials and provider selectors that outrank a CLI's
/// stored login. Keep this list explicit: cloud credentials may still be needed
/// by the agent's tools, and account-local configuration remains user-owned.
/// See docs/user-guide.md, "Account credential isolation", for provider sources.
pub(crate) fn credential_overrides(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
            "CLAUDE_CODE_OAUTH_SCOPES",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_MANTLE",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
            "ANTHROPIC_PROFILE",
            "ANTHROPIC_FEDERATION_RULE_ID",
            "ANTHROPIC_ORGANIZATION_ID",
        ],
        Provider::Codex => &[
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
            "CODEX_ACCESS_TOKEN",
            "OPENAI_FEDERATION_RULE_ID",
            "OPENAI_IDENTITY_TOKEN_FILE",
            "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
        ],
        Provider::Omp => &[],
    }
}

/// Recognize only the canonical home namespace owned by this installation.
/// Merely pointing a system account at an arbitrary directory is not isolation.
pub fn managed_home_identity(root: &Path, path: &Path) -> Option<(Provider, uuid::Uuid)> {
    let mut components = path.strip_prefix(root).ok()?.components();
    let provider = components.next()?.as_os_str().to_str()?;
    let provider = Account::PROVIDERS
        .into_iter()
        .find(|candidate| candidate.executable() == provider)?;
    let key = uuid::Uuid::parse_str(components.next()?.as_os_str().to_str()?).ok()?;
    (components.next().is_none()
        && root
            .join(provider.executable())
            .join(key.hyphenated().to_string())
            == path)
        .then_some((provider, key))
}

/// Whether `path` has the shape of a home a cagents installation manages:
/// `<data dir>/accounts/<provider>/<uuid>`. Every installation - a development
/// instance included - uses that shape, so a path that has it is an account
/// cagents selected, never the provider's own default.
pub fn is_account_namespace(provider: Provider, path: &Path) -> bool {
    let mut components = path.components().rev();
    let mut name = || {
        components
            .next()
            .and_then(|component| component.as_os_str().to_str())
    };
    name().is_some_and(|key| uuid::Uuid::parse_str(key).is_ok())
        && name() == Some(provider.executable())
        && name() == Some("accounts")
}

/// Carried by every terminal and conversation cagents starts on a managed or
/// linked account: the provider homes it selected, as `VARIABLE=path` lines.
/// A home the reader's own environment chose - the system account's - is
/// never listed, so it survives into a cagents process started from there.
pub const SELECTION_MARKER: &str = "CAGENTS_ACCOUNT_HOMES";

/// Carried by every terminal and conversation cagents starts on the system
/// Claude login with a `CLAUDE_CONFIG_DIR` the reader started cagents with -
/// the one the daemon that starts it was started with - as a
/// `CLAUDE_CONFIG_DIR=path` line. It tells a cagents process started
/// there that the home is the reader's own choice, which a launch by earlier
/// cagents, exporting Claude's default for every system launch, could not
/// (see [`inherited_selections`]).
pub const SYSTEM_HOME_MARKER: &str = "CAGENTS_SYSTEM_HOMES";

/// The value of [`SYSTEM_HOME_MARKER`] for a process about to start with the
/// provider homes `home_of` names and the [`SELECTION_MARKER`] `selection`,
/// from a process whose own environment is `own`: the Claude home it exports
/// when that is the one this process was started with and no account
/// selected it. A conversation saved by earlier cagents, resuming with the
/// default it exported, is not listed.
pub fn system_home_marker(
    home_of: impl Fn(&str) -> Option<String>,
    selection: Option<&str>,
    own: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let variable = home_variable(Provider::Claude)?;
    let home = home_of(variable).filter(|home| !home.is_empty())?;
    let line = format!("{variable}={home}");
    let selected = selection.is_some_and(|selection| selection.lines().any(|l| l == line));
    (!selected && own(variable).is_some_and(|own| own == home)).then_some(line)
}

/// The provider home variables to drop from an inherited environment, read
/// through `environment`: those that only record an account cagents selected
/// for a launch, which a cagents process started under that launch must not
/// take for the provider's own default. A variable is dropped while it still
/// holds the home [`SELECTION_MARKER`] lists for it, or when it names a
/// directory with the shape of a managed home.
///
/// The system account is whatever home the provider's CLI resolves outside
/// cagents. cagents points every terminal it starts, and every conversation,
/// at the home of the account chosen for it. A cagents window, daemon or
/// command started from inside one - by an agent running `cagents`, by the
/// reader, or by a daemon such a command started - inherited that choice and
/// took it for the system account: the system and the chosen account then
/// named one home and reported one login, system work ran on the chosen
/// account, and signing in to the system account signed the chosen
/// account's home in to the system login.
///
/// In a process cagents started, `CLAUDE_CONFIG_DIR` naming Claude Code's
/// default `~/.claude` is dropped too, unless [`SYSTEM_HOME_MARKER`] lists
/// it. Earlier cagents exported it for every launch on the system login, and
/// it selects no other home: it only moves the login's record of who it is
/// from `~/.claude.json` into that directory. Kept, a window, daemon or
/// command started from such a launch would read and write a second record
/// of who the system login is, unlike the reader's own `claude` and every
/// other cagents process. Outside a cagents launch, or listed by one, the
/// variable is the reader's own setting and kept.
pub fn inherited_selections(environment: &dyn Fn(&str) -> Option<String>) -> Vec<&'static str> {
    let marker = environment(SELECTION_MARKER).unwrap_or_default();
    Account::PROVIDERS
        .into_iter()
        .filter_map(|provider| {
            let variable = home_variable(provider)?;
            let value = environment(variable)?;
            let selected = marker.lines().any(|line| {
                line.split_once('=')
                    .is_some_and(|(name, home)| name == variable && home == value)
            });
            (selected
                || is_account_namespace(provider, Path::new(&value))
                || legacy_claude_default(provider, variable, &value, environment))
            .then_some(variable)
        })
        .collect()
}

/// Whether `value` of `variable` is Claude Code's default directory as an
/// earlier cagents launch exported it: `environment`'s `HOME` joined with
/// `.claude`, compared by path components, in a process cagents started,
/// and not listed by [`SYSTEM_HOME_MARKER`]. Only a lexical comparison, as
/// the account views read this on every render.
fn legacy_claude_default(
    provider: Provider,
    variable: &str,
    value: &str,
    environment: &dyn Fn(&str) -> Option<String>,
) -> bool {
    let launched = LAUNCH_MARKERS
        .iter()
        .any(|marker| environment(marker).is_some_and(|value| !value.is_empty()));
    let declared = environment(SYSTEM_HOME_MARKER).is_some_and(|listed| {
        listed
            .lines()
            .any(|line| line.split_once('=') == Some((variable, value)))
    });
    provider == Provider::Claude
        && launched
        && !declared
        && environment("HOME")
            .filter(|user| !user.is_empty())
            .is_some_and(|user| Path::new(value) == Path::new(&user).join(".claude"))
}

/// Set in the environment of every terminal cagents starts
/// (`CAGENTS_TERMINAL_ID`) and of every conversation agent it hosts
/// ([`crate::agent_execution::AUTHORITY_ENV`]).
const LAUNCH_MARKERS: [&str; 2] = ["CAGENTS_TERMINAL_ID", crate::agent_execution::AUTHORITY_ENV];

/// Forgets the provider homes this process inherited from an account
/// cagents selected (see [`inherited_selections`]), so that neither it nor
/// any process it starts takes that account for the system one.
///
/// # Safety
///
/// Must be called before the process starts a second thread: it changes the
/// process environment, which another thread may be reading.
pub unsafe fn detach_inherited_selections() {
    let removed = inherited_selections(&|name| {
        std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
    });
    for variable in removed.into_iter().chain([SELECTION_MARKER]) {
        // SAFETY: the caller guarantees the process is still single-threaded.
        unsafe { std::env::remove_var(variable) };
    }
}

/// Effective spawn policy also protects recipes saved before explicit removals
/// existed. Do not rewrite a live descriptor: its identity is daemon evidence.
#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn launch_credential_overrides(
    recipe: &crate::model::LaunchRecipe,
) -> std::collections::BTreeSet<String> {
    let mut names = recipe.unset_environment.clone();
    if let Some(dirs) = crate::instance::project_dirs() {
        let root = dirs.data_dir().join("accounts");
        for provider in Account::PROVIDERS {
            if recipe
                .environment
                .get(home_variable(provider).unwrap())
                .is_some_and(|home| {
                    managed_home_identity(&root, Path::new(home))
                        .is_some_and(|(owner, _)| owner == provider)
                })
            {
                names.extend(
                    credential_overrides(provider)
                        .iter()
                        .map(|name| (*name).to_owned()),
                );
            }
        }
    }
    names
}

impl Account {
    /// Selecting a home selects its login, for both owned and linked homes.
    /// The system account deliberately retains the caller's credential policy.
    pub fn credential_overrides(&self) -> &'static [&'static str] {
        match self.home {
            AccountHome::Managed { .. } | AccountHome::Linked { .. } => {
                credential_overrides(self.provider)
            }
            AccountHome::System | AccountHome::SystemAt { .. } | AccountHome::Brokered { .. } => {
                &[]
            }
        }
    }

    /// The account a provider's CLI uses when nothing chooses another.
    pub fn system(provider: Provider) -> Self {
        Self {
            provider,
            home: AccountHome::System,
        }
    }

    /// A further login cagents keeps in a home of its own.
    pub fn managed(provider: Provider, key: uuid::Uuid, path: PathBuf) -> Self {
        Self {
            provider,
            home: AccountHome::Managed { key, path },
        }
    }

    pub fn linked(provider: Provider, key: uuid::Uuid, path: PathBuf) -> Self {
        Self {
            provider,
            home: AccountHome::Linked { key, path },
        }
    }

    /// An account a CLI brokers among its own, known only from its record of
    /// the account's usage.
    pub fn brokered(provider: Provider, service: String, key: String) -> Self {
        Self {
            provider,
            home: AccountHome::Brokered { service, key },
        }
    }

    /// The system accounts, which every installation reports on whether or
    /// not it added any account of its own. Omp brokers its own accounts,
    /// which are found in its usage record rather than listed here.
    pub fn system_accounts() -> [Self; 2] {
        [
            Self::system(Provider::Claude),
            Self::system(Provider::Codex),
        ]
    }

    /// The providers cagents can add accounts for, in the order they are
    /// listed.
    pub const PROVIDERS: [Provider; 2] = [Provider::Claude, Provider::Codex];

    /// The providers whose usage is reported, in the order they are listed.
    pub const REPORTED: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Omp];

    pub fn id(&self) -> AccountId {
        let provider = self.provider.executable();
        AccountId(match &self.home {
            AccountHome::System | AccountHome::SystemAt { .. } => format!("{provider}/system"),
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => {
                format!("{provider}/{key}")
            }
            AccountHome::Brokered { service, key } => {
                format!("{provider}/{service}/{}", brokered_digest(service, key))
            }
        })
    }

    /// The key of an account registered in cagents, `None` for the system or
    /// a provider-brokered account.
    pub fn registry_key(&self) -> Option<uuid::Uuid> {
        match &self.home {
            AccountHome::System | AccountHome::SystemAt { .. } | AccountHome::Brokered { .. } => {
                None
            }
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => Some(*key),
        }
    }

    pub fn is_linked(&self) -> bool {
        matches!(self.home, AccountHome::Linked { .. })
    }

    pub fn is_system(&self) -> bool {
        matches!(
            self.home,
            AccountHome::System | AccountHome::SystemAt { .. }
        )
    }

    /// Whether a CLI keeps this account among its own and picks between
    /// them itself, so cagents only reads what the CLI records of it.
    pub fn is_brokered(&self) -> bool {
        matches!(self.home, AccountHome::Brokered { .. })
    }

    /// The service a brokered account's login is for, `None` for any other.
    pub fn brokered_service(&self) -> Option<&str> {
        match &self.home {
            AccountHome::Brokered { service, .. } => Some(service),
            AccountHome::System
            | AccountHome::SystemAt { .. }
            | AccountHome::Managed { .. }
            | AccountHome::Linked { .. } => None,
        }
    }

    /// The name of this account's probe lock, which keeps two windows from
    /// probing the same account at once while leaving different accounts of
    /// one provider free to be probed side by side.
    pub fn lock_name(&self) -> String {
        match &self.home {
            AccountHome::System | AccountHome::SystemAt { .. } => {
                self.provider.executable().to_owned()
            }
            AccountHome::Managed { key, .. } | AccountHome::Linked { key, .. } => {
                format!("{}-{}", self.provider.executable(), key.simple())
            }
            AccountHome::Brokered { service, key } => format!(
                "{}-{}",
                self.provider.executable(),
                brokered_digest(service, key)
            ),
        }
    }

    /// The directory the CLI keeps this account's login in, resolved against
    /// `environment` the way the CLI itself resolves it. A brokered account
    /// has none of its own.
    pub fn home_directory(&self, environment: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
        match &self.home {
            AccountHome::Brokered { .. } => None,
            AccountHome::SystemAt { path }
            | AccountHome::Managed { path, .. }
            | AccountHome::Linked { path, .. } => Some(path.clone()),
            AccountHome::System => {
                let variable = home_variable(self.provider)?;
                if let Some(explicit) = environment(variable).filter(|value| !value.is_empty())
                    && !inherited_selections(environment).contains(&variable)
                {
                    return Some(PathBuf::from(explicit));
                }
                let home = environment("HOME").filter(|value| !value.is_empty())?;
                let directory = match self.provider {
                    Provider::Claude => ".claude",
                    Provider::Codex => ".codex",
                    Provider::Omp => return None,
                };
                Some(Path::new(&home).join(directory))
            }
        }
    }

    /// The home in the running process's own environment.
    pub fn resolved_home(&self) -> Option<PathBuf> {
        self.home_directory(&|name| std::env::var(name).ok())
    }

    /// The home a conversation, terminal or sign-in on this account exports
    /// through its provider's home variable, or `None` when it exports none
    /// and the CLI resolves its own default.
    ///
    /// A managed, linked or selected device's home is always exported, as is
    /// the system account's when cagents itself was started with one, unless
    /// it was inherited from a cagents launch (see [`inherited_selections`]);
    /// every cagents process started alike resolves the same. The system Claude login
    /// otherwise exports nothing, so every launch reads
    /// the same records as the reader's own `claude`: Claude Code keeps who a
    /// login belongs to in its global configuration, which any explicit
    /// `CLAUDE_CONFIG_DIR` moves from `~/.claude.json` into that directory -
    /// even when the variable names the default `~/.claude`. Exporting the
    /// default would give one login two records of who it is, one that only
    /// cagents' launches keep.
    pub fn launch_home(&self) -> Option<PathBuf> {
        self.launch_home_in(&|name| std::env::var(name).ok())
    }

    /// [`Self::launch_home`] against `environment`.
    pub fn launch_home_in(&self, environment: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
        let home = self.home_directory(environment)?;
        if !matches!(self.home, AccountHome::System) || self.provider != Provider::Claude {
            return Some(home);
        }
        let variable = home_variable(self.provider)?;
        let explicit = environment(variable).is_some_and(|value| !value.is_empty())
            && !inherited_selections(environment).contains(&variable);
        explicit.then_some(home)
    }

    /// Binds `recipe`'s provider home to this account, replacing whatever
    /// binding it had: exports [`Self::launch_home`], or, where that is
    /// `None` for the system Claude login, unsets the variable, so the CLI
    /// resolves its default as the reader's own `claude` and this account's
    /// probe do, whatever the process that spawns it inherited. A daemon
    /// started with its own `CLAUDE_CONFIG_DIR` would otherwise hand it on.
    ///
    /// Leaves `recipe` unchanged when the home is not a path it can record.
    pub fn bind_home(&self, recipe: &mut crate::model::LaunchRecipe) -> Result<(), String> {
        self.bind_home_in(recipe, &|name| std::env::var(name).ok())
    }

    /// [`Self::bind_home`] against `environment`.
    pub fn bind_home_in(
        &self,
        recipe: &mut crate::model::LaunchRecipe,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(), String> {
        let Some(variable) = home_variable(self.provider) else {
            return Ok(());
        };
        let home = self
            .launch_home_in(environment)
            .map(|home| {
                home.into_os_string()
                    .into_string()
                    .map_err(|_| "The account's home is not a path a launch can record".to_owned())
            })
            .transpose()?;
        recipe.environment.remove(variable);
        recipe.unset_environment.remove(variable);
        match home {
            Some(home) => {
                recipe.environment.insert(variable.to_owned(), home);
            }
            None if self.provider == Provider::Claude
                && matches!(self.home, AccountHome::System) =>
            {
                recipe.unset_environment.insert(variable.to_owned());
            }
            None => {}
        }
        Ok(())
    }

    /// The environment a probe of this account sets on the CLI.
    ///
    /// The system account sets nothing, so the CLI resolves its default
    /// exactly as it would in the user's own shell, and as the system
    /// account's launches have it resolve (see [`Self::launch_home`]).
    pub fn probe_environment(&self) -> Option<(&'static str, PathBuf)> {
        match &self.home {
            AccountHome::System | AccountHome::SystemAt { .. } | AccountHome::Brokered { .. } => {
                None
            }
            AccountHome::Managed { path, .. } | AccountHome::Linked { path, .. } => {
                Some((home_variable(self.provider)?, path.clone()))
            }
        }
    }

    /// The key the cross-window cache files this account under: the provider
    /// and the canonical home, so two windows started with differently
    /// spelled paths to the same directory share one entry.
    ///
    /// A brokered account has no home, and needs no cache: the record it is
    /// read from is one every window reads alike.
    pub fn cache_key(&self) -> Option<String> {
        let home = self.resolved_home()?;
        let canonical = std::fs::canonicalize(&home).unwrap_or(home);
        Some(format!(
            "{}:{}",
            self.provider.executable(),
            canonical.display()
        ))
    }
}

/// Who an account is signed in as. Only what the CLI reports as display
/// metadata: never a token, a key, or anything that authorizes a request.
///
/// Records - the shared cache, a device's probe reply - keep every field.
/// The daemon's binary wire keeps the fields it has always carried, in their
/// order, so that adding [`Self::disputed_by`] did not change its protocol:
/// what the wire's readers need of a dispute, they are told as a conflict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub email: Option<String>,
    pub organization: Option<String>,
    /// The reported plan name, including a reported tier where available.
    /// Older providers may supply only an identifier; see [`plan_label`].
    pub plan: Option<String>,
    /// The login another record of the same credentials names, when the
    /// records disagree.
    ///
    /// Claude Code keeps who a login is in its global configuration file,
    /// apart from its credentials, and `claude auth status` reports that
    /// record without asking the service. One set of credentials can therefore be
    /// described by two records naming two logins (see
    /// [`Account::launch_home`]); nothing a probe can read without a token
    /// says which is right, so the probe reports both rather than vouch for
    /// either. Only the system account's default home can have a second
    /// record.
    pub disputed_by: Option<String>,
}

/// [`Identity`] as a record keeps it.
#[derive(Deserialize)]
#[serde(rename = "Identity")]
struct IdentityRecord {
    email: Option<String>,
    organization: Option<String>,
    plan: Option<String>,
    #[serde(default)]
    disputed_by: Option<String>,
}

/// [`IdentityRecord`], written.
#[derive(Serialize)]
#[serde(rename = "Identity")]
struct IdentityRecordRef<'a> {
    email: &'a Option<String>,
    organization: &'a Option<String>,
    plan: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disputed_by: Option<&'a String>,
}

/// [`Identity`] as the binary wire carries it.
#[derive(Deserialize)]
#[serde(rename = "Identity")]
struct IdentityWire {
    email: Option<String>,
    organization: Option<String>,
    plan: Option<String>,
}

/// [`IdentityWire`], written.
#[derive(Serialize)]
#[serde(rename = "Identity")]
struct IdentityWireRef<'a> {
    email: &'a Option<String>,
    organization: &'a Option<String>,
    plan: &'a Option<String>,
}

impl Serialize for Identity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (email, organization, plan) = (&self.email, &self.organization, &self.plan);
        if serializer.is_human_readable() {
            IdentityRecordRef {
                email,
                organization,
                plan,
                disputed_by: self.disputed_by.as_ref(),
            }
            .serialize(serializer)
        } else {
            IdentityWireRef {
                email,
                organization,
                plan,
            }
            .serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let record = IdentityRecord::deserialize(deserializer)?;
            Ok(Self {
                email: record.email,
                organization: record.organization,
                plan: record.plan,
                disputed_by: record.disputed_by,
            })
        } else {
            let wire = IdentityWire::deserialize(deserializer)?;
            Ok(Self {
                email: wire.email,
                organization: wire.organization,
                plan: wire.plan,
                disputed_by: None,
            })
        }
    }
}

impl Identity {
    /// Whether `self` and `other` are the same provider login seen through
    /// two homes, which is what makes a second home for it redundant.
    ///
    /// Only an email both sides reported can say so: two logins with no
    /// email are not known to be anyone in particular. Organizations are
    /// compared where both sides name one, because one person can belong to
    /// a personal and a team organization under the same address, and those
    /// are separate allowances.
    ///
    /// Nor can a login whose records disagree about who it is (see
    /// [`Self::disputed_by`]): it claims a login, which
    /// [`Self::claims_same_login`] compares, but is not known to be it.
    pub fn same_login(&self, other: &Identity) -> bool {
        self.disputed_by.is_none() && other.disputed_by.is_none() && self.claims_same_login(other)
    }

    /// Whether `self` and `other` name the same login, as [`Self::same_login`]
    /// compares them, whether or not either is disputed.
    pub fn claims_same_login(&self, other: &Identity) -> bool {
        let (Some(mine), Some(theirs)) = (self.email.as_deref(), other.email.as_deref()) else {
            return false;
        };
        if !mine.trim().eq_ignore_ascii_case(theirs.trim()) {
            return false;
        }
        match (self.organization.as_deref(), other.organization.as_deref()) {
            (Some(mine), Some(theirs)) => mine.trim() == theirs.trim(),
            _ => true,
        }
    }
}

/// The longest name a reader can give an account.
pub const LABEL_LIMIT: usize = 64;

/// A name the reader typed for an account, as it is stored: trimmed, without
/// control characters, and at most [`LABEL_LIMIT`] characters. `None` for a
/// name with nothing left, which clears the name back to the account's email.
pub fn clean_label(label: &str) -> Option<String> {
    let cleaned: String = label
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(LABEL_LIMIT)
        .collect();
    let cleaned = cleaned.trim().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// What a usage window limits, which decides how it is labelled and grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowKind {
    /// A short rolling window, Claude's five hours or Codex's primary limit.
    Session,
    /// A weekly allowance shared by everything the account does.
    Weekly,
    /// A weekly allowance for one model or surface.
    Scoped,
    /// A limit the provider reported under a name cagents does not know. It
    /// is still shown, under the provider's own words, rather than dropped.
    Other,
}

/// One rate-limit window an account is subject to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    /// Stable identity across observations, so a live update can replace the
    /// window a probe reported rather than adding a second copy of it.
    pub key: String,
    pub kind: WindowKind,
    pub label: String,
    /// The fraction of the window used. May exceed one: a provider can let
    /// lower-priority work run past a window's cap.
    pub used: f32,
    /// Unix seconds at which the window resets, where the provider said.
    pub resets_at: Option<i64>,
    /// How long the window is, where known.
    pub span_secs: Option<u64>,
    /// The provider named this window as the one currently limiting the
    /// account.
    pub binding: bool,
}

impl UsageWindow {
    /// The fraction still available, never negative.
    pub fn left(&self) -> f32 {
        (1.0 - self.used).clamp(0.0, 1.0)
    }
}

/// An amount of money in a currency's minor units, `exponent` places below
/// the major unit. Parsers accept at most six places, which covers every
/// currency and keeps the scale well inside an `i64`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub minor: i64,
    pub exponent: u32,
    pub currency: String,
}

impl std::fmt::Display for Money {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let exponent = self.exponent.min(6);
        let scale = 10_i64.pow(exponent);
        let sign = if self.minor < 0 { "-" } else { "" };
        let magnitude = self.minor.unsigned_abs();
        let whole = magnitude / scale as u64;
        let fraction = magnitude % scale as u64;
        let amount = if exponent == 0 {
            format!("{whole}")
        } else {
            format!("{whole}.{fraction:0width$}", width = exponent as usize)
        };
        match self.currency.as_str() {
            "USD" => write!(formatter, "{sign}${amount}"),
            "EUR" => write!(formatter, "{sign}€{amount}"),
            "GBP" => write!(formatter, "{sign}£{amount}"),
            other => write!(formatter, "{sign}{amount} {other}"),
        }
    }
}

/// Paid usage beyond the plan's included limits: Claude's extra usage or
/// Codex's credits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credits {
    pub enabled: bool,
    pub unlimited: bool,
    pub used: Option<Money>,
    pub limit: Option<Money>,
    /// A balance the provider reports as preformatted text.
    pub balance: Option<String>,
    /// The fraction of a spending limit used, where there is one.
    pub used_fraction: Option<f32>,
}

impl Credits {
    /// One line describing the allowance, for the popover and the table.
    pub fn summary(&self) -> String {
        if self.unlimited {
            return "Unlimited".into();
        }
        if !self.enabled {
            return "Off".into();
        }
        match (&self.used, &self.limit, &self.balance) {
            (Some(used), Some(limit), _) => format!("{used} of {limit} used"),
            (Some(used), None, _) => format!("{used} used"),
            (None, _, Some(balance)) => format!("{balance} left"),
            (None, _, None) => match self.used_fraction {
                Some(used) => format!("{:.0}% used", used * 100.0),
                None => "On".into(),
            },
        }
    }
}

/// How an account stands against its limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageStatus {
    Ok,
    /// A window is past [`WARNING_FRACTION`], or the provider said so.
    Warning,
    /// A window the whole account depends on is exhausted.
    Limited,
    /// The CLI is installed and has a home, but its login no longer works.
    NeedsLogin,
    /// There is nothing to report: the CLI is missing, the account has never
    /// been set up, or the provider does not report limits for it.
    Unavailable,
}

/// Where a snapshot came from, which is how old its figures can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageSource {
    /// Reported by a conversation that is running on the account right now.
    Live,
    /// Asked of the CLI by cagents.
    Probe,
    /// Read back from the cache another window, or an earlier run, wrote.
    Cache,
    /// Read from the record a CLI that brokers its own accounts keeps of what
    /// it observed, as of when it observed it.
    Recorded,
}

/// Everything known about an account's limits at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    #[serde(default)]
    pub resets: Vec<UsageReset>,
    pub windows: Vec<UsageWindow>,
    pub credits: Option<Credits>,
    pub status: UsageStatus,
    /// Latest explicit backend permission for ordinary Codex usage. Sparse
    /// window notifications do not replace this account-wide evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinary_usage_allowed: Option<bool>,
    /// Unix milliseconds at which these figures were observed.
    pub observed_at: u64,
    pub source: UsageSource,
    /// The plan the usage response named, where it named one.
    pub plan: Option<String>,
    /// Why a snapshot has no figures, in the provider's or cagents' words.
    pub detail: Option<String>,
}

/// How far apart, in seconds, two reports of one weekly allowance may place
/// its reset: providers round it, and report it at different moments.
pub const RESET_TOLERANCE: u64 = 3_600;

impl UsageSnapshot {
    /// A snapshot that reports a state rather than figures.
    pub fn state(status: UsageStatus, detail: impl Into<String>, observed_at: u64) -> Self {
        Self {
            windows: Vec::new(),
            resets: Vec::new(),
            credits: None,
            status,
            ordinary_usage_allowed: None,
            observed_at,
            source: UsageSource::Probe,
            plan: None,
            detail: Some(detail.into()),
        }
    }

    /// The window that decides when this account next runs out.
    ///
    /// The provider's own choice where it made one - Claude marks the limit
    /// currently binding the account. Otherwise the fullest window, and of
    /// equally full windows the one that resets soonest, because that is the
    /// figure the reader has to plan around.
    pub fn binding_window(&self) -> Option<&UsageWindow> {
        self.windows
            .iter()
            .find(|window| window.binding)
            .or_else(|| tightest(&self.windows))
    }

    /// Whether `self` and `other` are the figures of two separate
    /// allowances, as of `now` in Unix seconds: a weekly window both report
    /// under one key still runs in both and resets more than
    /// [`RESET_TOLERANCE`] apart. One login seen through two homes draws on
    /// one allowance, whose every window resets once.
    pub fn separate_allowance(&self, other: &UsageSnapshot, now: i64) -> bool {
        let running = |window: &UsageWindow| {
            window.kind == WindowKind::Weekly
                && window.resets_at.is_some_and(|resets_at| resets_at > now)
        };
        self.windows
            .iter()
            .filter(|window| running(window))
            .any(|mine| {
                other
                    .windows
                    .iter()
                    .filter(|window| running(window) && window.key == mine.key)
                    .any(|theirs| {
                        mine.resets_at
                            .zip(theirs.resets_at)
                            .is_some_and(|(mine, theirs)| mine.abs_diff(theirs) > RESET_TOLERANCE)
                    })
            })
    }

    /// Recomputes the status from the windows after they changed, keeping a
    /// state the windows cannot express.
    pub fn refresh_status(&mut self, provider_limited: bool, provider_warning: bool) {
        if matches!(
            self.status,
            UsageStatus::NeedsLogin | UsageStatus::Unavailable
        ) && self.windows.is_empty()
        {
            return;
        }
        self.status = status_for(&self.windows, provider_limited, provider_warning);
    }
}

/// What a running conversation's CLI reported about the account it runs on,
/// from the frames it streams anyway: Claude's `rate_limit_event`, Codex's
/// `account/rateLimits/updated` and `account/updated`.
///
/// It names no account. The conversation's environment decides that, and
/// only the interface knows which account an environment belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LiveReport {
    Claude(claude::LiveUpdate),
    CodexLimits(codex::LimitsUpdate),
    CodexLogin(codex::LoginUpdate),
}

impl LiveReport {
    pub fn provider(&self) -> Provider {
        match self {
            Self::Claude(_) => Provider::Claude,
            Self::CodexLimits(_) | Self::CodexLogin(_) => Provider::Codex,
        }
    }

    /// Folds the figures this report carries into `current`, the reading the
    /// account already has from a probe or the cache, observed at
    /// `observed_at`, and says whether it took them.
    ///
    /// A report only refines a reading: it carries the windows the CLI
    /// happened to stream, not the plan, the credits or every model's window,
    /// so an account without one waits for its probe. A report older than
    /// `current` is dropped rather than rewinding a newer reading, unless
    /// `current` is stamped further ahead of `now` than a clock can drift - a
    /// stamp from before the clock was set back. A login report carries no
    /// figures and is never taken.
    pub fn apply(self, current: &mut UsageSnapshot, observed_at: u64, now: u64) -> bool {
        if current.observed_at > observed_at
            && current.observed_at <= now.saturating_add(schedule::CLOCK_SLACK)
        {
            return false;
        }
        match self {
            Self::Claude(update) => {
                claude::merge_live(current, update, observed_at);
                current.observed_at = observed_at;
            }
            Self::CodexLimits(update) => {
                if codex::merge_update(current, update, observed_at) {
                    current.observed_at = observed_at;
                }
            }
            Self::CodexLogin(_) => return false,
        }
        true
    }
}

/// The fullest of `windows`, and of equally full windows the one that resets
/// soonest.
pub fn tightest<'a>(windows: impl IntoIterator<Item = &'a UsageWindow>) -> Option<&'a UsageWindow> {
    windows.into_iter().max_by(|left, right| {
        left.used
            .partial_cmp(&right.used)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                // Sooner resets rank higher; an unknown reset ranks last.
                let left = left.resets_at.unwrap_or(i64::MAX);
                let right = right.resets_at.unwrap_or(i64::MAX);
                right.cmp(&left)
            })
    })
}

/// The status a set of windows describes.
///
/// Only a window the whole account depends on makes it Limited: one model's
/// weekly allowance running out leaves every other model usable, which is a
/// warning about that model rather than an account that cannot work.
pub fn status_for(
    windows: &[UsageWindow],
    provider_limited: bool,
    provider_warning: bool,
) -> UsageStatus {
    let exhausted = windows
        .iter()
        .any(|window| window.kind != WindowKind::Scoped && window.used >= 1.0);
    if exhausted || provider_limited {
        return UsageStatus::Limited;
    }
    if provider_warning || windows.iter().any(|window| window.used > WARNING_FRACTION) {
        return UsageStatus::Warning;
    }
    UsageStatus::Ok
}

/// A plan identifier as a person would write it.
pub fn plan_label(plan: &str) -> String {
    match plan {
        "prolite" => "Pro Lite".into(),
        "edu_plus" => "Edu Plus".into(),
        "edu_pro" => "Edu Pro".into(),
        // Underscores identify older machine names. Otherwise preserve the
        // provider's punctuation and capitalization within its display name.
        other => humanize_words(other, &['_', ' ']),
    }
}

/// `weekly_scoped` as `Weekly scoped`: a provider's identifier made legible
/// for the one place it has to be shown verbatim.
pub fn humanize(identifier: &str) -> String {
    humanize_words(identifier, &['_', '-', ' '])
}

fn humanize_words(identifier: &str, separators: &[char]) -> String {
    let mut words = identifier.split(separators).filter(|word| !word.is_empty());
    let mut label = String::with_capacity(identifier.len());
    if let Some(first) = words.next() {
        let mut characters = first.chars();
        if let Some(initial) = characters.next() {
            label.extend(initial.to_uppercase());
            label.push_str(characters.as_str());
        }
    }
    for word in words {
        label.push(' ');
        label.push_str(word);
    }
    label
}

/// A window's length as a label: `5h`, `Weekly`, `30d`.
pub fn span_label(span_secs: u64) -> String {
    const HOUR: u64 = 3_600;
    const DAY: u64 = 24 * HOUR;
    match span_secs {
        seconds if seconds == 7 * DAY => "Weekly".into(),
        seconds if seconds == DAY => "Daily".into(),
        seconds if seconds % DAY == 0 => format!("{}d", seconds / DAY),
        seconds if seconds % HOUR == 0 => format!("{}h", seconds / HOUR),
        seconds => format!("{}m", seconds.div_ceil(60)),
    }
}

/// Unix seconds for an RFC 3339 timestamp such as
/// `2026-09-22T23:30:00.267281+00:00`, or `None` for anything else.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = text.get(range)?;
        slice
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| slice.parse().ok())?
    };
    if bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // A leap second is folded into the next second rather than refused.
    if second > 60 {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let end = fraction
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(fraction.len());
        if end == 0 {
            return None;
        }
        rest = &fraction[end..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        zone if zone.len() == 6 && matches!(zone.as_bytes()[0], b'+' | b'-') => {
            if zone.as_bytes()[3] != b':' {
                return None;
            }
            let digits = |text: &str| {
                text.bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| text.parse::<i64>().ok())
                    .flatten()
            };
            let hours = digits(&zone[1..3]).filter(|hours| *hours <= 23)?;
            let minutes = digits(&zone[4..6]).filter(|minutes| *minutes <= 59)?;
            let magnitude = hours * 3_600 + minutes * 60;
            if zone.starts_with('-') {
                -magnitude
            } else {
                magnitude
            }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// Days since the Unix epoch for a proleptic Gregorian date (Howard
/// Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// A duration until something happens, at the two most significant units:
/// `3d 4h`, `2h 14m`, `14m`, `<1m`.
pub fn format_remaining(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "<1m".into()
    }
}

/// How long ago something was observed: `just now`, `2m ago`, `3h ago`.
pub fn format_ago(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        "just now".into()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

/// A moment as the reader would name it from `now`: `18:00` today,
/// `Wed 18:00` within the week, `Sep 30` beyond it.
pub fn format_local_moment(unix_seconds: i64, now_seconds: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (Some(moment), Some(now)) = (local_time(unix_seconds), local_time(now_seconds)) else {
        return String::new();
    };
    let clock = moment.clock();
    if moment.year == now.year && moment.day_of_year == now.day_of_year {
        clock
    } else if (unix_seconds - now_seconds).abs() < 6 * 86_400 {
        format!("{} {clock}", WEEKDAYS[moment.weekday])
    } else {
        format!("{} {}", MONTHS[moment.month], moment.day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_home_expansion_keeps_destination_home_and_refuses_absolute_remainders() {
        use std::ffi::OsStr;
        let destination = OsStr::new("/selected-device/home");
        // The defect selected /other/account instead of the chosen device HOME.
        assert_eq!(
            Path::new(destination).join("/other/account"),
            Path::new("/other/account")
        );
        for value in [
            "~//other/account",
            "~///other/account",
            "relative/account",
            "",
            "~/bad\0home",
        ] {
            assert!(
                expand_linked_home(value, Some(destination)).is_err(),
                "{value:?}"
            );
        }
        assert_eq!(
            expand_linked_home("~/account homes/work", Some(destination)).unwrap(),
            Path::new("/selected-device/home/account homes/work")
        );
        assert_eq!(
            expand_linked_home("/explicit/other home", None).unwrap(),
            Path::new("/explicit/other home")
        );
        assert_eq!(
            expand_linked_home("~/", Some(destination)).unwrap(),
            Path::new("/selected-device/home")
        );
        assert!(expand_linked_home("~/account", None).is_err());
        assert!(expand_linked_home("~/account", Some(OsStr::new("relative/home"))).is_err());
        // Neither UI normalization nor CLI validation may rebind the operand.
        assert_eq!(
            expand_linked_home("  ~/account homes/work  ".trim(), Some(destination)).unwrap(),
            Path::new("/selected-device/home/account homes/work")
        );
        assert!(validate_linked_home_operand("  ~/account homes/work  ").is_err());
        assert!(validate_linked_home_operand("~/account homes/work").is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let home = OsStr::from_bytes(b"/selected-device/\xff");
            assert_eq!(
                expand_linked_home("~/account", Some(home)).unwrap(),
                Path::new(home).join("account")
            );
        }
    }

    #[test]
    fn managed_identity_requires_exact_provider_and_canonical_uuid_path() {
        let root = Path::new("/installation/accounts");
        let key = uuid::Uuid::parse_str("12345678-1234-4234-8234-123456789abc").unwrap();
        let valid = root.join("claude").join(key.hyphenated().to_string());
        assert_eq!(
            managed_home_identity(root, &valid),
            Some((Provider::Claude, key))
        );
        for path in [
            PathBuf::from("/elsewhere/claude/12345678-1234-4234-8234-123456789abc"),
            root.join("omp").join(key.hyphenated().to_string()),
            root.join("claude").join(key.simple().to_string()),
            root.join("claude/not-an-account"),
            valid.join("nested"),
        ] {
            assert_eq!(managed_home_identity(root, &path), None);
        }
    }

    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn legacy_isolation_does_not_treat_an_external_system_home_as_managed() {
        let mut recipe = crate::model::LaunchRecipe::shell("/fixture".into()).unwrap();
        recipe.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            "/external/claude/12345678-1234-4234-8234-123456789abc".into(),
        );
        assert!(launch_credential_overrides(&recipe).is_empty());
        let home = crate::instance::project_dirs()
            .unwrap()
            .data_dir()
            .join("accounts/claude/12345678-1234-4234-8234-123456789abc");
        recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), home.to_string_lossy().into());
        assert!(launch_credential_overrides(&recipe).contains("ANTHROPIC_API_KEY"));
        assert!(!launch_credential_overrides(&recipe).contains("CODEX_API_KEY"));
    }

    fn window(key: &str, kind: WindowKind, used: f32, resets_at: Option<i64>) -> UsageWindow {
        UsageWindow {
            key: key.into(),
            kind,
            label: key.into(),
            used,
            resets_at,
            span_secs: None,
            binding: false,
        }
    }

    fn snapshot(windows: Vec<UsageWindow>) -> UsageSnapshot {
        UsageSnapshot {
            resets: Vec::new(),
            windows,
            credits: None,
            status: UsageStatus::Ok,
            ordinary_usage_allowed: None,
            observed_at: 0,
            source: UsageSource::Probe,
            plan: None,
            detail: None,
        }
    }

    #[test]
    fn rfc3339_timestamps_resolve_to_unix_seconds() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-09-22T23:30:00.267281+00:00"),
            Some(1_790_119_800)
        );
        // The offset is subtracted: 01:30 at +02:00 is 23:30 UTC the day before.
        assert_eq!(
            parse_rfc3339("2026-09-23T01:30:00+02:00"),
            Some(1_790_119_800)
        );
        assert_eq!(
            parse_rfc3339("2000-02-29T12:00:00-05:30"),
            Some(951_845_400)
        );
        for invalid in [
            "",
            "2026-09-22",
            "2026-13-01T00:00:00Z",
            "2026-09-22T24:00:00Z",
            "2026-09-22T23:30:00",
            "2026-09-22T23:30:00++1:00",
            "2026-09-22T23:30:00+24:00",
            "2026-09-22T23:30:00+01:60",
            "2026-09-22T23:30:00.+00:00",
            "2026-09-22T23:30:00+0000",
            "not a timestamp at all",
        ] {
            assert_eq!(parse_rfc3339(invalid), None, "{invalid:?} parsed");
        }
    }

    /// The provider's own choice of binding window wins even over a fuller
    /// one, because it knows which limit actually stops the next request.
    #[test]
    fn the_binding_window_is_the_providers_choice_when_it_makes_one() {
        let mut marked = window("scoped", WindowKind::Scoped, 0.4, Some(200));
        marked.binding = true;
        let usage = snapshot(vec![
            window("session", WindowKind::Session, 0.9, Some(100)),
            marked,
        ]);
        assert_eq!(usage.binding_window().unwrap().key, "scoped");
    }

    #[test]
    fn without_a_marked_window_the_fullest_then_soonest_reset_binds() {
        let usage = snapshot(vec![
            window("weekly", WindowKind::Weekly, 0.5, Some(1_000)),
            window("session", WindowKind::Session, 0.8, Some(5_000)),
            window("other", WindowKind::Other, 0.8, Some(2_000)),
            window("unknown", WindowKind::Other, 0.8, None),
        ]);
        assert_eq!(usage.binding_window().unwrap().key, "other");
        assert!(snapshot(Vec::new()).binding_window().is_none());
    }

    #[test]
    fn only_an_account_wide_window_makes_the_account_limited() {
        let scoped = window("fable", WindowKind::Scoped, 1.0, None);
        assert_eq!(
            status_for(&[scoped.clone()], false, false),
            UsageStatus::Warning
        );
        let weekly = window("weekly", WindowKind::Weekly, 1.02, None);
        assert_eq!(
            status_for(&[scoped, weekly], false, false),
            UsageStatus::Limited
        );
        let calm = window("session", WindowKind::Session, 0.3, None);
        assert_eq!(status_for(&[calm.clone()], false, false), UsageStatus::Ok);
        assert_eq!(
            status_for(&[calm.clone()], false, true),
            UsageStatus::Warning
        );
        assert_eq!(status_for(&[calm], true, false), UsageStatus::Limited);
    }

    #[test]
    fn system_homes_resolve_like_the_clis_resolve_them() {
        let environment = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        let claude = Account::system(Provider::Claude);
        assert_eq!(
            claude.home_directory(&environment(&[("HOME", "/home/a")])),
            Some(PathBuf::from("/home/a/.claude"))
        );
        assert_eq!(
            claude.home_directory(&environment(&[
                ("HOME", "/home/a"),
                ("CLAUDE_CONFIG_DIR", "/elsewhere")
            ])),
            Some(PathBuf::from("/elsewhere"))
        );
        // An empty variable is unset as far as the CLI is concerned.
        let codex = Account::system(Provider::Codex);
        assert_eq!(
            codex.home_directory(&environment(&[("HOME", "/home/a"), ("CODEX_HOME", "")])),
            Some(PathBuf::from("/home/a/.codex"))
        );
        assert_eq!(codex.home_directory(&environment(&[])), None);
        assert_eq!(claude.probe_environment(), None);
        // The system Claude login exports no home of its own, so the CLI
        // reads `~/.claude.json` as in the reader's shell: an explicit
        // `CLAUDE_CONFIG_DIR=~/.claude` would have it read and write
        // `~/.claude/.claude.json` instead, a second record of who it is.
        let user = environment(&[("HOME", "/home/a")]);
        assert_eq!(claude.launch_home_in(&user), None);
        // A directory the reader started cagents with is theirs, and kept.
        let chosen = environment(&[
            ("HOME", "/home/a"),
            ("CLAUDE_CONFIG_DIR", "/home/a/claude-config"),
        ]);
        assert_eq!(
            claude.launch_home_in(&chosen),
            Some(PathBuf::from("/home/a/claude-config"))
        );
        // In a cagents terminal, the default spelled out is no choice of
        // another home, only the second record earlier cagents exported.
        for spelled in ["/home/a/.claude", "/home/a/.claude/", "/home/a//.claude"] {
            let pinned = move |name: &str| match name {
                "HOME" => Some("/home/a".to_owned()),
                "CLAUDE_CONFIG_DIR" => Some(spelled.to_owned()),
                "CAGENTS_TERMINAL_ID" => Some("terminal".to_owned()),
                _ => None,
            };
            assert_eq!(inherited_selections(&pinned), ["CLAUDE_CONFIG_DIR"]);
            assert_eq!(claude.launch_home_in(&pinned), None);
            assert_eq!(
                claude.home_directory(&pinned),
                Some(PathBuf::from("/home/a/.claude"))
            );
        }
        // Outside one it is the reader's own setting, and Codex's default
        // is never dropped.
        let own = environment(&[
            ("HOME", "/home/a"),
            ("CLAUDE_CONFIG_DIR", "/home/a/.claude"),
        ]);
        assert!(inherited_selections(&own).is_empty());
        assert_eq!(
            claude.launch_home_in(&own),
            Some(PathBuf::from("/home/a/.claude"))
        );
        let codex_default = environment(&[
            ("HOME", "/home/a"),
            ("CODEX_HOME", "/home/a/.claude"),
            ("CAGENTS_TERMINAL_ID", "terminal"),
        ]);
        assert!(inherited_selections(&codex_default).is_empty());
        // A conversation agent's process is a cagents launch as well; an
        // empty marker is none.
        for (marker, value, dropped) in [
            ("CAGENTS_AGENT_AUTHORITY", "token", true),
            ("CAGENTS_TERMINAL_ID", "", false),
        ] {
            let launched = move |name: &str| match name {
                "HOME" => Some("/home/a".to_owned()),
                "CLAUDE_CONFIG_DIR" => Some("/home/a/.claude".to_owned()),
                name if name == marker => Some(value.to_owned()),
                _ => None,
            };
            assert_eq!(!inherited_selections(&launched).is_empty(), dropped);
        }
        // A launch by this cagents of a reader who chose the default lists
        // it, so a cagents process started there keeps it too.
        let home_of =
            |name: &str| (name == "CLAUDE_CONFIG_DIR").then(|| "/home/a/.claude".to_owned());
        let declared = system_home_marker(home_of, None, home_of).unwrap();
        assert_eq!(declared, "CLAUDE_CONFIG_DIR=/home/a/.claude");
        let kept = |name: &str| match name {
            SYSTEM_HOME_MARKER => Some(declared.clone()),
            "HOME" => Some("/home/a".to_owned()),
            "CLAUDE_CONFIG_DIR" => Some("/home/a/.claude".to_owned()),
            "CAGENTS_TERMINAL_ID" => Some("terminal".to_owned()),
            _ => None,
        };
        assert!(inherited_selections(&kept).is_empty());
        assert_eq!(
            claude.launch_home_in(&kept),
            Some(PathBuf::from("/home/a/.claude"))
        );
        // A selected home is never listed as the system's, nor one the
        // spawning process was not started with - a conversation earlier
        // cagents saved with the default - and an unset one lists nothing.
        assert_eq!(
            system_home_marker(home_of, Some("CLAUDE_CONFIG_DIR=/home/a/.claude"), home_of),
            None
        );
        assert_eq!(system_home_marker(home_of, None, |_| None), None);
        assert_eq!(system_home_marker(|_| None, None, home_of), None);
        // One inherited from an account cagents selected is not.
        let selected = environment(&[
            ("HOME", "/home/a"),
            ("CLAUDE_CONFIG_DIR", "/home/a/.claude"),
            (SELECTION_MARKER, "CLAUDE_CONFIG_DIR=/home/a/.claude"),
        ]);
        assert_eq!(claude.launch_home_in(&selected), None);
        // Codex keeps its login in its home either way.
        assert_eq!(
            codex.launch_home_in(&user),
            Some(PathBuf::from("/home/a/.codex"))
        );
        // Every other home is the account's own and always exported.
        let linked = Account::linked(
            Provider::Claude,
            uuid::Uuid::from_u128(2),
            "/home/a/.claude-work".into(),
        );
        assert_eq!(
            linked.launch_home_in(&user),
            Some(PathBuf::from("/home/a/.claude-work"))
        );
        let key = uuid::Uuid::from_u128(1);
        let managed = Account::managed(Provider::Codex, key, "/data/accounts/codex/1".into());
        assert_eq!(
            managed.probe_environment(),
            Some(("CODEX_HOME", PathBuf::from("/data/accounts/codex/1")))
        );
        assert_ne!(managed.id(), codex.id());
        assert_eq!(codex.id().as_str(), "codex/system");
        assert_eq!(
            managed.id().as_str(),
            "codex/00000000-0000-0000-0000-000000000001"
        );
        // The same key under a differently spelled data directory is still
        // the same account.
        assert_eq!(
            Account::managed(Provider::Codex, key, "/elsewhere".into()).id(),
            managed.id()
        );
        assert_ne!(managed.lock_name(), codex.lock_name());
        assert!(!managed.is_system() && codex.is_system());
    }

    /// A launch on the system Claude login unsets the home variable rather
    /// than leaving it out, so a daemon started with its own
    /// `CLAUDE_CONFIG_DIR` cannot hand it on and split the login's record
    /// from the one its probe reads. Every other binding replaces the last.
    #[test]
    fn a_launch_binds_the_home_its_account_resolves_whatever_spawns_it() {
        let environment = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        let user = environment(&[("HOME", "/home/a")]);
        let chosen = environment(&[("HOME", "/home/a"), ("CLAUDE_CONFIG_DIR", "/chosen")]);
        let recipe = || crate::model::LaunchRecipe {
            program: "claude".into(),
            arguments: Vec::new(),
            working_directory: "/".into(),
            environment: [("CLAUDE_CONFIG_DIR".into(), "/home/a/.claude".into())].into(),
            unset_environment: ["ANTHROPIC_API_KEY".into()].into(),
        };
        let bound = |account: &Account, environment: &dyn Fn(&str) -> Option<String>| {
            let mut recipe = recipe();
            account.bind_home_in(&mut recipe, environment).unwrap();
            (recipe.environment, recipe.unset_environment)
        };
        let system = Account::system(Provider::Claude);
        assert_eq!(
            bound(&system, &user),
            (
                Default::default(),
                ["ANTHROPIC_API_KEY".into(), "CLAUDE_CONFIG_DIR".into()].into()
            )
        );
        assert_eq!(
            bound(&system, &chosen),
            (
                [("CLAUDE_CONFIG_DIR".into(), "/chosen".into())].into(),
                ["ANTHROPIC_API_KEY".into()].into()
            )
        );
        // Moving a system launch onto another account exports its home
        // instead of leaving the default unset.
        let linked = Account::linked(
            Provider::Claude,
            uuid::Uuid::from_u128(2),
            "/home/a/.claude-work".into(),
        );
        let mut moved = recipe();
        system.bind_home_in(&mut moved, &user).unwrap();
        linked.bind_home_in(&mut moved, &user).unwrap();
        assert_eq!(
            (moved.environment, moved.unset_environment),
            (
                [("CLAUDE_CONFIG_DIR".into(), "/home/a/.claude-work".into())].into(),
                ["ANTHROPIC_API_KEY".into()].into()
            )
        );
        // Codex keeps its login in its home, which it always exports.
        let (environment, unset) = bound(&Account::system(Provider::Codex), &user);
        assert_eq!(
            environment.get("CODEX_HOME").map(String::as_str),
            Some("/home/a/.codex")
        );
        assert!(!unset.contains("CODEX_HOME"));
    }

    /// Regression for #520: a cagents process started from inside a
    /// conversation or terminal on a chosen account inherited that account's
    /// home and took it for the system account.
    #[test]
    fn the_system_account_is_never_a_home_cagents_selected() {
        let environment = |pairs: Vec<(&'static str, String)>| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
            }
        };
        let key = uuid::Uuid::new_v4();
        let chosen_codex = format!("/data/cagents/accounts/codex/{key}");
        let chosen_claude = format!("/data-dev/cagents-dev/accounts/claude/{key}/");
        assert!(is_account_namespace(
            Provider::Codex,
            Path::new(&chosen_codex)
        ));
        assert!(is_account_namespace(
            Provider::Claude,
            Path::new(&chosen_claude)
        ));
        // Only that exact shape, for that provider.
        for (provider, path) in [
            (Provider::Claude, chosen_codex.clone()),
            (Provider::Codex, format!("/data/accounts/codex/{key}/sub")),
            (Provider::Codex, format!("/data/homes/codex/{key}")),
            (Provider::Codex, "/data/accounts/codex/not-a-key".into()),
            (Provider::Codex, "/home/a/.codex".into()),
        ] {
            assert!(!is_account_namespace(provider, Path::new(&path)), "{path}");
        }

        // A daemon started by `cagents` in an agent's shell on a chosen
        // Codex account, with the reader's own Claude directory.
        let inherited = environment(vec![
            ("HOME", "/home/a".into()),
            ("CODEX_HOME", chosen_codex.clone()),
            ("CLAUDE_CONFIG_DIR", "/home/a/work-claude".into()),
        ]);
        assert_eq!(inherited_selections(&inherited), ["CODEX_HOME"]);
        let codex = Account::system(Provider::Codex);
        let claude = Account::system(Provider::Claude);
        assert_eq!(
            codex.home_directory(&inherited),
            Some(PathBuf::from("/home/a/.codex"))
        );
        // A directory the reader chose themselves is still their default.
        assert_eq!(
            claude.home_directory(&inherited),
            Some(PathBuf::from("/home/a/work-claude"))
        );

        // A terminal or conversation on a linked account lists the homes
        // cagents selected for it. The reader's own directory, pinned for
        // the system account, is not listed and stays their default; a home
        // the reader exported after cagents selected one is theirs too.
        let launched = |codex_home: &str| {
            environment(vec![
                ("HOME", "/home/a".into()),
                (SELECTION_MARKER, "CODEX_HOME=/home/a/linked-codex".into()),
                ("CODEX_HOME", codex_home.into()),
                ("CLAUDE_CONFIG_DIR", "/home/a/own-claude".into()),
            ])
        };
        assert_eq!(
            inherited_selections(&launched("/home/a/linked-codex")),
            ["CODEX_HOME"]
        );
        assert_eq!(
            codex.home_directory(&launched("/home/a/linked-codex")),
            Some(PathBuf::from("/home/a/.codex"))
        );
        assert_eq!(
            claude.home_directory(&launched("/home/a/linked-codex")),
            Some(PathBuf::from("/home/a/own-claude"))
        );
        assert!(inherited_selections(&launched("/home/a/exported")).is_empty());
        // Nothing inherited, nothing dropped.
        assert!(inherited_selections(&environment(vec![("HOME", "/home/a".into())])).is_empty());
    }

    /// The binaries forget an inherited selection before anything reads the
    /// environment, so every daemon, window and terminal they start inherits
    /// the reader's own default instead.
    #[test]
    fn a_cagents_process_forgets_an_inherited_account_selection() {
        const CHILD: &str = "CAGENTS_TEST_DETACH_SELECTIONS";
        if std::env::var_os(CHILD).is_none() {
            let key = uuid::Uuid::new_v4();
            // Preserve the caller's scope; override only these fixture inputs.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "account::tests::a_cagents_process_forgets_an_inherited_account_selection",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .env("HOME", "/home/a")
                .env("CODEX_HOME", format!("/data/accounts/codex/{key}"))
                .env("CLAUDE_CONFIG_DIR", "/home/a/own-claude")
                .env(SELECTION_MARKER, "CLAUDE_CONFIG_DIR=/home/a/other")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        // SAFETY: the harness runs this child with a single test thread, and
        // nothing else in it reads the environment concurrently.
        unsafe { detach_inherited_selections() };
        assert_eq!(std::env::var_os("CODEX_HOME"), None);
        assert_eq!(std::env::var_os(SELECTION_MARKER), None);
        assert_eq!(
            std::env::var("CLAUDE_CONFIG_DIR").as_deref(),
            Ok("/home/a/own-claude")
        );
        assert_eq!(
            Account::system(Provider::Codex).resolved_home(),
            Some(PathBuf::from("/home/a/.codex"))
        );
    }

    #[test]
    fn a_login_is_the_same_only_when_both_sides_name_one_address() {
        let identity = |email: Option<&str>, organization: Option<&str>| Identity {
            email: email.map(str::to_owned),
            organization: organization.map(str::to_owned),
            plan: None,
            disputed_by: None,
        };
        let work = identity(Some("Me@Example.com"), Some("Acme"));
        assert!(work.same_login(&identity(Some("me@example.com "), Some("Acme"))));
        assert!(work.same_login(&identity(Some("me@example.com"), None)));
        assert!(!work.same_login(&identity(Some("me@example.com"), Some("Personal"))));
        assert!(!work.same_login(&identity(Some("you@example.com"), Some("Acme"))));
        assert!(!identity(None, None).same_login(&identity(None, None)));
        // A login whose records disagree claims an address, but is not known
        // to be it.
        let disputed = Identity {
            disputed_by: Some("other@example.com".into()),
            ..identity(Some("me@example.com"), Some("Acme"))
        };
        assert!(disputed.claims_same_login(&work) && work.claims_same_login(&disputed));
        assert!(!disputed.same_login(&work) && !work.same_login(&disputed));
    }

    /// A dispute is kept wherever an identity is recorded, and the daemon's
    /// binary wire carries an identity exactly as before it existed.
    #[test]
    fn a_dispute_is_recorded_but_leaves_the_binary_wire_as_it_was() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Before {
            email: Option<String>,
            organization: Option<String>,
            plan: Option<String>,
        }
        let disputed = Identity {
            email: Some("me@example.com".into()),
            organization: None,
            plan: Some("max".into()),
            disputed_by: Some("other@example.com".into()),
        };
        let before = Before {
            email: disputed.email.clone(),
            organization: None,
            plan: disputed.plan.clone(),
        };
        let wire = postcard::to_allocvec(&disputed).unwrap();
        assert_eq!(wire, postcard::to_allocvec(&before).unwrap());
        assert_eq!(
            postcard::from_bytes::<Identity>(&wire).unwrap(),
            Identity {
                disputed_by: None,
                ..disputed.clone()
            }
        );
        let record = serde_json::to_value(&disputed).unwrap();
        assert_eq!(record["disputed_by"], "other@example.com");
        assert_eq!(
            serde_json::from_value::<Identity>(record).unwrap(),
            disputed
        );
        // A record written before disputes existed, or of an undisputed
        // login, reads as undisputed, and is written as it was.
        let undisputed = serde_json::to_value(&before).unwrap();
        let identity = serde_json::from_value::<Identity>(undisputed.clone()).unwrap();
        assert_eq!(identity.disputed_by, None);
        assert_eq!(serde_json::to_value(&identity).unwrap(), undisputed);
        assert_eq!(
            serde_json::from_value::<Identity>(serde_json::json!({})).unwrap(),
            Identity::default()
        );
    }

    #[test]
    fn one_allowance_resets_once_and_only_running_windows_are_compared() {
        let window = |key: &str, kind: WindowKind, resets_at: Option<i64>| UsageWindow {
            key: key.into(),
            kind,
            label: String::new(),
            used: 0.5,
            resets_at,
            span_secs: None,
            binding: false,
        };
        let snapshot = |windows: Vec<UsageWindow>| {
            let mut snapshot = UsageSnapshot::state(UsageStatus::Ok, "", 0);
            snapshot.windows = windows;
            snapshot
        };
        let now = 1_000_000;
        let weekly = |resets_at: i64| window("weekly_all", WindowKind::Weekly, Some(resets_at));
        let week = snapshot(vec![
            window("session", WindowKind::Session, Some(now + 60)),
            weekly(now + 86_400),
        ]);
        // Reported a moment apart, or rounded: one allowance.
        assert!(!week.separate_allowance(&snapshot(vec![weekly(now + 86_400 + 1_800)]), now));
        assert!(week.separate_allowance(&snapshot(vec![weekly(now + 3 * 86_400)]), now));
        // The session window says nothing about the weekly allowance.
        let session = window("session", WindowKind::Session, Some(now + 9_000));
        assert!(!week.separate_allowance(&snapshot(vec![session]), now));
        // A window that has already reset is a figure from before it.
        assert!(!week.separate_allowance(&snapshot(vec![weekly(now - 3 * 86_400)]), now));
        // Another weekly window of the same login - one model's, listed
        // first by one report - resets on its own schedule, and is compared
        // only with itself.
        let model = |resets_at: i64| window("weekly_opus", WindowKind::Weekly, Some(resets_at));
        let listed = snapshot(vec![model(now + 5 * 86_400), weekly(now + 86_400)]);
        assert!(!week.separate_allowance(&listed, now));
        assert!(!listed.separate_allowance(&week, now));
        assert!(listed.separate_allowance(&snapshot(vec![model(now + 86_400)]), now));
    }

    #[test]
    fn a_typed_label_is_trimmed_bounded_and_single_line() {
        assert_eq!(clean_label("  Work  "), Some("Work".into()));
        assert_eq!(clean_label("a\nb\tc"), Some("a b c".into()));
        assert_eq!(clean_label(" \n "), None);
        assert_eq!(
            clean_label(&"x".repeat(200)).map(|label| label.chars().count()),
            Some(LABEL_LIMIT)
        );
    }

    #[test]
    fn durations_read_at_their_two_most_significant_units() {
        assert_eq!(format_remaining(-5), "<1m");
        assert_eq!(format_remaining(59), "<1m");
        assert_eq!(format_remaining(14 * 60 + 59), "14m");
        assert_eq!(format_remaining(2 * 3_600 + 14 * 60), "2h 14m");
        assert_eq!(format_remaining(2 * 3_600 + 4 * 60), "2h 04m");
        assert_eq!(format_remaining(3 * 86_400 + 4 * 3_600 + 59), "3d 4h");
        assert_eq!(format_remaining(2 * 86_400), "2d");
        assert_eq!(format_ago(10), "just now");
        assert_eq!(format_ago(125), "2m ago");
        assert_eq!(format_ago(3 * 3_600), "3h ago");
        assert_eq!(format_ago(2 * 86_400 + 5), "2d ago");
    }

    #[test]
    fn labels_are_legible_without_a_table_of_every_identifier() {
        assert_eq!(humanize("weekly_scoped"), "Weekly scoped");
        assert_eq!(humanize("max"), "Max");
        assert_eq!(humanize(""), "");
        assert_eq!(plan_label("prolite"), "Pro Lite");
        assert_eq!(plan_label("team"), "Team");
        assert_eq!(plan_label("Max 20x"), "Max 20x");
        assert_eq!(plan_label("Ultra"), "Ultra");
        assert_eq!(plan_label("Fixture-Horizon 37x"), "Fixture-Horizon 37x");
        assert_eq!(span_label(300 * 60), "5h");
        assert_eq!(span_label(10_080 * 60), "Weekly");
        assert_eq!(span_label(30 * 86_400), "30d");
        assert_eq!(span_label(90), "2m");
    }

    #[test]
    fn money_is_written_in_its_currency() {
        let money = |minor, exponent, currency: &str| Money {
            minor,
            exponent,
            currency: currency.into(),
        };
        assert_eq!(money(1_234, 2, "USD").to_string(), "$12.34");
        assert_eq!(money(5, 2, "USD").to_string(), "$0.05");
        assert_eq!(money(-250, 2, "EUR").to_string(), "-€2.50");
        assert_eq!(money(7, 0, "JPY").to_string(), "7 JPY");
        let credits = Credits {
            enabled: true,
            unlimited: false,
            used: Some(money(1_000, 2, "USD")),
            limit: Some(money(5_000, 2, "USD")),
            balance: None,
            used_fraction: Some(0.2),
        };
        assert_eq!(credits.summary(), "$10.00 of $50.00 used");
        let off = Credits {
            enabled: false,
            ..credits.clone()
        };
        assert_eq!(off.summary(), "Off");
    }

    #[test]
    fn a_moment_is_named_relative_to_today() {
        let now = 1_790_119_800;
        assert_eq!(format_local_moment(now, now).len(), 5);
        let later_this_week = format_local_moment(now + 3 * 86_400, now);
        assert!(
            later_this_week.len() == 9,
            "{later_this_week:?} should be a weekday and a clock"
        );
        let next_month = format_local_moment(now + 30 * 86_400, now);
        assert!(next_month.starts_with("Oct"), "{next_month:?}");
    }

    /// A report older than the reading is dropped, however slightly - unless
    /// the reading is stamped past any drift of the present, which only a
    /// clock set back leaves behind.
    #[test]
    fn a_live_report_never_rewinds_a_newer_reading() {
        const NOW: u64 = 1_790_000_000_000;
        let report = || {
            LiveReport::Claude(claude::LiveUpdate {
                windows: vec![window("five_hour", WindowKind::Session, 0.4, None)],
                status: None,
            })
        };
        let reading = |observed_at| UsageSnapshot {
            resets: Vec::new(),
            windows: vec![window("five_hour", WindowKind::Session, 0.1, None)],
            credits: None,
            status: UsageStatus::Ok,
            ordinary_usage_allowed: None,
            observed_at,
            source: UsageSource::Probe,
            plan: Some("max".into()),
            detail: None,
        };

        let mut newer = reading(NOW);
        assert!(!report().apply(&mut newer, NOW - 1, NOW));
        assert_eq!(newer, reading(NOW));
        // However much older: the drift allowed is the reading's from the
        // present, not the report's from the reading.
        assert!(!report().apply(&mut newer, NOW - 10 * schedule::CLOCK_SLACK, NOW));
        assert_eq!(newer, reading(NOW));

        let mut older = reading(NOW - 1);
        assert!(report().apply(&mut older, NOW, NOW));
        assert_eq!(older.observed_at, NOW);
        assert_eq!(older.source, UsageSource::Live);
        assert_eq!(older.plan.as_deref(), Some("max"));
        assert!((older.windows[0].used - 0.4).abs() < 1e-6);

        // Stamped an hour ahead of a present that has not reached it.
        let mut ahead = reading(NOW + 3_600_000);
        assert!(report().apply(&mut ahead, NOW, NOW));
        assert_eq!(ahead.observed_at, NOW);

        let login = LiveReport::CodexLogin(codex::LoginUpdate {
            chatgpt: true,
            plan: None,
        });
        let mut untouched = reading(NOW - 1);
        assert!(!login.apply(&mut untouched, NOW, NOW));
        assert_eq!(untouched, reading(NOW - 1));
    }
}

/// Provider credentials and service endpoints stay inside the explicit
/// Claude reset transport exception. Ordinary account metadata and all
/// other provider operations continue to use the provider's own client.
#[cfg(test)]
mod credential_guard {
    //! A tripwire, not a proof: a determined change can spell a needle so it
    //! does not match. What it does catch is the ordinary way the rule gets
    //! broken - code that names a provider's credential store, token fields,
    //! or service hosts - so that doing so takes a deliberate edit here.

    /// Each needle is assembled at run time, so this file does not match
    /// itself.
    fn forbidden() -> Vec<String> {
        [
            // Credential stores and the fields inside them.
            ["claude", "AiOauth"],
            ["oauth", "Account"],
            [".credentials", ".json"],
            ["auth", ".json"],
            ["access", "Token"],
            ["refresh", "Token"],
            ["access", "_token"],
            ["refresh", "_token"],
            ["id", "_token"],
            ["OAUTH", "_TOKEN"],
            // Claude Code's entry in the system keyring.
            ["Claude Code", "-credentials"],
            // Omp's logins, kept in the same database as its usage record.
            ["auth_", "credential"],
            // Provider services, which only the providers' own clients call.
            ["anthropic", ".com"],
            ["claude.ai", "/api"],
            ["openai", ".com"],
            ["chatgpt", ".com"],
            ["backend-api", "/wham"],
        ]
        .iter()
        .map(|parts| parts.concat())
        .collect()
    }

    /// Everything that becomes or drives a cagents binary. Recorded fixtures
    /// are data a test reads, not code, and quote the services they came
    /// from.
    fn sources(path: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                sources(&entry.unwrap().path(), files);
            }
        } else if path
            .extension()
            .is_none_or(|extension| ["rs", "sh", "toml"].iter().any(|code| extension == *code))
        {
            files.push(path.to_path_buf());
        }
    }

    // Only this complete boolean request expression is permitted. Reading
    // that field or supplying credential values remains forbidden.
    fn without_native_refresh_request(file: &std::path::Path, text: String) -> String {
        if file.ends_with("src/chat/protocol/codex.rs") {
            let request = ["json!({\"refresh", "Token\":true})"].concat();
            text.replace(&request, "json!({\"provider-refresh-option\":true})")
        } else {
            text
        }
    }

    #[test]
    fn native_refresh_exception_allows_only_the_boolean_request_expression() {
        let key = ["refresh", "Token"].concat();
        let adapter = std::path::Path::new("src/chat/protocol/codex.rs");
        let request = format!("json!({{\"{key}\":true}})");
        assert!(!without_native_refresh_request(adapter, request.clone()).contains(&key));
        for source in [
            format!("result[\"{key}\"].as_str()"),
            format!("json!({{\"{key}\":\"credential-value\"}})"),
            format!("json!({{\"{key}\":true_credentials}})"),
        ] {
            assert!(without_native_refresh_request(adapter, source).contains(&key));
        }
        assert!(
            without_native_refresh_request(std::path::Path::new("src/account.rs"), request)
                .contains(&key)
        );
    }

    #[test]
    fn no_source_reads_provider_credentials_or_calls_provider_services() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for entry in [
            "src",
            "tests",
            "crates",
            "examples",
            "scripts",
            "build.rs",
            "install.sh",
            "Cargo.toml",
        ] {
            let path = root.join(entry);
            if path.exists() {
                sources(&path, &mut files);
            }
        }
        assert!(
            files.len() > 50,
            "the guard found too few sources to mean anything"
        );
        let forbidden = forbidden();
        let mut violations = Vec::new();
        for file in files {
            // Explicitly authorized private Claude reset adapter. Keep all
            // credential discovery and provider HTTP in this one boundary.
            if file.ends_with("src/account/probe/reset_transport.rs") {
                continue;
            }
            // This documented Codex setting selects storage; naming it does
            // not access Omp's similarly named credential table.
            let mut text = std::fs::read_to_string(&file)
                .unwrap_or_default()
                .replace("cli_auth_credentials_store", "");
            // Exact quoted environment names are needed to remove inherited
            // credentials. This does not allow credential file fields, values,
            // endpoints, or arbitrary strings containing those names.
            for provider in crate::account::Account::PROVIDERS {
                for name in super::credential_overrides(provider) {
                    text = text.replace(&format!("\"{name}\""), "\"credential-variable-name\"");
                }
            }
            // The Accounts install action opens this fixed documentation page
            // in a browser. It does not read credentials or call a service API.
            if file.ends_with("src/app/account_ui.rs") {
                let documentation = ["https://developers.open", "ai.com/codex/cli/"].concat();
                text = text.replace(&format!("\"{documentation}\""), "\"install-documentation\"");
            }
            text = without_native_refresh_request(&file, text);
            for needle in &forbidden {
                if text.contains(needle.as_str()) {
                    violations.push(format!("{} names {needle}", file.display()));
                }
            }
        }
        assert!(violations.is_empty(), "{violations:#?}");
    }
}
